// SPDX-License-Identifier: Apache-2.0

//! The wire-level rules: request framing, the byte cap, admission control,
//! sniff resolution and its evidence, the `UNIMPLEMENTED` refusals, and
//! `GetServiceInfo`.

mod common;

use std::sync::atomic::Ordering;

use common::{
    BOXNOTE, CALL_TIMEOUT, DOCLING, LiveUpload, client, client_with, info, options, parse,
    parse_ok, sniff_options, text_blocks,
};
use grpc_markup::proto::v1 as pb;
use grpc_markup::service::MarkupGrpc;
use tonic::Code;

// ----------------------------------------------------------------- framing

#[tokio::test]
async fn a_chunk_first_stream_is_invalid_argument() {
    let mut client = client().await;
    let requests = vec![pb::ParseMarkupRequest {
        payload: Some(pb::parse_markup_request::Payload::Chunk(b"# hi".to_vec())),
    }];
    let error = drain(client.parse_markup(tokio_stream::iter(requests)).await)
        .await
        .expect_err("refused");
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(error.message().contains("options"), "{}", error.message());
}

#[tokio::test]
async fn an_empty_request_stream_is_invalid_argument() {
    let mut client = client().await;
    let requests: Vec<pb::ParseMarkupRequest> = Vec::new();
    let error = drain(client.parse_markup(tokio_stream::iter(requests)).await)
        .await
        .expect_err("refused");
    assert_eq!(error.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn options_sent_twice_is_invalid_argument() {
    let mut client = client().await;
    let requests = vec![
        pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Options(options(
                pb::MarkupFormat::Markdown,
            ))),
        },
        pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Options(options(
                pb::MarkupFormat::Markdown,
            ))),
        },
    ];
    let error = drain(client.parse_markup(tokio_stream::iter(requests)).await)
        .await
        .expect_err("refused");
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(
        error.message().contains("first request message"),
        "{}",
        error.message()
    );
}

#[tokio::test]
async fn a_frame_with_neither_field_is_invalid_argument() {
    let mut client = client().await;
    let requests = vec![
        pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Options(options(
                pb::MarkupFormat::Markdown,
            ))),
        },
        pb::ParseMarkupRequest { payload: None },
    ];
    let error = drain(client.parse_markup(tokio_stream::iter(requests)).await)
        .await
        .expect_err("refused");
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(
        error.message().contains("`options` or `chunk`"),
        "{}",
        error.message()
    );
}

#[tokio::test]
async fn an_empty_chunk_is_legal_and_ignored() {
    let mut client = client().await;
    let requests = vec![
        pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Options(options(
                pb::MarkupFormat::Markdown,
            ))),
        },
        pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Chunk(Vec::new())),
        },
        pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Chunk(b"# hi\n".to_vec())),
        },
    ];
    let events = drain(client.parse_markup(tokio_stream::iter(requests)).await)
        .await
        .expect("parses");
    assert!(events.len() >= 3, "info, the title block, status");
}

#[tokio::test]
async fn an_unknown_format_number_is_invalid_argument() {
    let client = client().await;
    let error = parse(
        &client,
        "# hi\n",
        pb::ParseOptions {
            format: 99,
            ..pb::ParseOptions::default()
        },
    )
    .await
    .expect_err("99 is not a MarkupFormat");
    assert_eq!(error.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------- byte cap

#[tokio::test]
async fn the_byte_cap_trips_mid_upload_with_resource_exhausted() {
    // A 1 MiB cap, tripped by the fifth 256 KiB chunk while the upload is
    // deliberately held open: the refusal must not wait for EOF.
    let client = client_with(MarkupGrpc::new().with_default_max_document_mib(1)).await;
    let upload = LiveUpload::start(&client, options(pb::MarkupFormat::Markdown)).await;
    let chunk = vec![b'a'; 256 * 1024];
    for _ in 0..6 {
        if !upload.send(&chunk).await {
            // The server hung up on the crossing chunk; that is the point.
            break;
        }
    }
    let outcome = tokio::time::timeout(CALL_TIMEOUT, upload.call)
        .await
        .expect("the refusal must arrive while the upload is still open")
        .expect("client task");
    let error = outcome.expect_err("the cap refuses the call");
    assert_eq!(error.code(), Code::ResourceExhausted);
    assert!(error.message().contains("byte cap"), "{}", error.message());
    // The sender is still alive: the refusal really was mid-upload.
    drop(upload.requests);
}

#[tokio::test]
async fn a_request_may_lower_but_not_raise_past_the_ceiling() {
    let client = client_with(
        MarkupGrpc::new()
            .with_default_max_document_mib(8)
            .with_ceiling_max_document_mib(1),
    )
    .await;
    // Asks for 512 MiB; the ceiling clamps to 1 MiB, so 2 MiB of input is
    // refused.
    let big = "a".repeat(2 * 1024 * 1024);
    let error = parse(
        &client,
        &big,
        pb::ParseOptions {
            format: pb::MarkupFormat::Markdown as i32,
            max_document_mib: 512,
            ..pb::ParseOptions::default()
        },
    )
    .await
    .expect_err("the ceiling clamps");
    assert_eq!(error.code(), Code::ResourceExhausted);
}

// ------------------------------------------------- memory budget, timeouts

#[tokio::test]
async fn the_memory_budget_is_shared_across_parses_and_released_after() {
    // A 1 MiB budget: one upload holding 768 KiB leaves too little for a
    // second one of 512 KiB, which is refused while the first is open.
    let client = client_with(MarkupGrpc::new().with_memory_budget_mib(1)).await;
    let first = LiveUpload::start(&client, options(pb::MarkupFormat::Markdown)).await;
    for _ in 0..3 {
        assert!(first.send(&vec![b'a'; 256 * 1024]).await);
    }
    // Let the server buffer those chunks before the second call arrives.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let second = parse(
        &client,
        &"b".repeat(512 * 1024),
        options(pb::MarkupFormat::Markdown),
    )
    .await
    .expect_err("the budget is in use");
    assert_eq!(second.code(), Code::ResourceExhausted);
    assert!(
        second.message().contains("memory budget"),
        "{}",
        second.message()
    );

    // Finishing the first parse gives its bytes back.
    drop(first.requests);
    let mut stream = tokio::time::timeout(CALL_TIMEOUT, first.call)
        .await
        .expect("first call answers")
        .expect("client task")
        .expect("first parse admitted")
        .into_inner();
    while stream
        .message()
        .await
        .expect("first parse streams")
        .is_some()
    {}
    parse_ok(
        &client,
        &"c".repeat(512 * 1024),
        options(pb::MarkupFormat::Markdown),
    )
    .await;
}

#[tokio::test]
async fn a_stalled_upload_is_abandoned_and_frees_its_slot() {
    // One slot, held by a client that sends its options and then nothing.
    let client = client_with(
        MarkupGrpc::new()
            .with_max_concurrent_parses(1)
            .with_upload_idle_timeout(std::time::Duration::from_millis(300)),
    )
    .await;
    let stalled = LiveUpload::start(&client, options(pb::MarkupFormat::Markdown)).await;
    assert!(stalled.send(b"# partial").await);
    let error = tokio::time::timeout(CALL_TIMEOUT, stalled.call)
        .await
        .expect("the idle timeout fires while the stream is still open")
        .expect("client task")
        .expect_err("the stalled upload is refused");
    assert_eq!(error.code(), Code::DeadlineExceeded);
    assert!(
        error.message().contains("no request frame"),
        "{}",
        error.message()
    );
    // The slot it held is free again.
    parse_ok(&client, "# hi\n", options(pb::MarkupFormat::Markdown)).await;
    drop(stalled.requests);
}

#[tokio::test]
async fn a_slow_trickle_hits_the_total_upload_timeout() {
    let client = client_with(
        MarkupGrpc::new()
            .with_upload_idle_timeout(std::time::Duration::from_secs(5))
            .with_upload_total_timeout(std::time::Duration::from_millis(500)),
    )
    .await;
    let LiveUpload { requests, call } =
        LiveUpload::start(&client, options(pb::MarkupFormat::Markdown)).await;
    let trickle = async {
        // A byte every 100 ms never trips the idle timeout.
        while requests
            .send(pb::ParseMarkupRequest {
                payload: Some(pb::parse_markup_request::Payload::Chunk(b"a".to_vec())),
            })
            .await
            .is_ok()
        {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let call = async {
        tokio::time::timeout(CALL_TIMEOUT, call)
            .await
            .expect("the total timeout fires")
            .expect("client task")
    };
    let (_, outcome) = tokio::join!(trickle, call);
    let error = outcome.expect_err("the trickle is refused");
    assert_eq!(error.code(), Code::DeadlineExceeded);
    assert!(
        error.message().contains("upload took longer"),
        "{}",
        error.message()
    );
}

#[tokio::test]
async fn a_parse_past_the_server_budget_is_deadline_exceeded() {
    // A zero budget: the parse is over before the fold sends anything.
    let client =
        client_with(MarkupGrpc::new().with_max_parse_time(std::time::Duration::ZERO)).await;
    let error = parse(&client, "# hi\n", options(pb::MarkupFormat::Markdown))
        .await
        .expect_err("no time to parse");
    assert_eq!(error.code(), Code::DeadlineExceeded);
}

// --------------------------------------------------------------- admission

#[tokio::test]
async fn past_the_concurrency_limit_requests_are_refused_not_queued() {
    let client = client_with(MarkupGrpc::new().with_max_concurrent_parses(0)).await;
    let error = parse(&client, "# hi\n", options(pb::MarkupFormat::Markdown))
        .await
        .expect_err("no slots at all");
    assert_eq!(error.code(), Code::ResourceExhausted);
    assert!(
        error.message().contains("concurrent"),
        "{}",
        error.message()
    );
}

// ------------------------------------------------------------ unimplemented

#[tokio::test]
async fn a_failed_sniff_is_unimplemented_and_says_to_state_the_format() {
    let client = client().await;
    for document in ["plain prose with no structure at all.\n", ""] {
        let error = parse(&client, document, sniff_options())
            .await
            .expect_err("nothing to recognize");
        assert_eq!(error.code(), Code::Unimplemented, "{document:?}");
        assert!(
            error.message().contains("state `format`"),
            "{}",
            error.message()
        );
    }
}

// ------------------------------------------------------------------- sniff

#[tokio::test]
async fn signatures_resolve_the_format_with_signature_evidence() {
    let client = client().await;
    for (document, format) in [
        (
            "WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n",
            pb::MarkupFormat::Vtt,
        ),
        (BOXNOTE, pb::MarkupFormat::Boxnote),
        (DOCLING, pb::MarkupFormat::DoclingJson),
    ] {
        let events = parse_ok(&client, document, sniff_options()).await;
        let info = info(&events);
        assert_eq!(info.format, format as i32);
        assert_eq!(
            info.evidence,
            pb::FormatEvidence::Signature as i32,
            "{format:?}"
        );
    }
}

#[tokio::test]
async fn markdown_texture_resolves_with_heuristic_evidence() {
    let client = client().await;
    let events = parse_ok(&client, "# A heading\n\nprose\n", sniff_options()).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Markdown as i32);
    assert_eq!(info.evidence, pb::FormatEvidence::Heuristic as i32);
}

#[tokio::test]
async fn an_explicit_request_wins_outright_with_requested_evidence() {
    let client = client().await;
    // Bytes whose signature says VTT, parsed as Markdown because the caller
    // said so: the request wins, and the sniff never runs.
    let events = parse_ok(
        &client,
        "WEBVTT and then markdown-ish text\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Markdown as i32);
    assert_eq!(info.evidence, pb::FormatEvidence::Requested as i32);
}

// ------------------------------------------------------------ service info

#[tokio::test]
async fn get_service_info_reports_identity_limits_and_formats() {
    let mut client = client().await;
    let response = client
        .get_service_info(pb::GetServiceInfoRequest {})
        .await
        .expect("info answers")
        .into_inner();
    assert_eq!(response.service, "grpc-markup");
    assert_eq!(response.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        response.formats,
        vec![
            pb::MarkupFormat::Markdown as i32,
            pb::MarkupFormat::Vtt as i32,
            pb::MarkupFormat::Boxnote as i32,
            pb::MarkupFormat::DoclingJson as i32,
            pb::MarkupFormat::Html as i32,
            pb::MarkupFormat::Asciidoc as i32,
            pb::MarkupFormat::Latex as i32,
        ],
        "exactly the formats this build maps, unspecified excluded"
    );
    assert_eq!(response.default_max_document_mib, 256);
    assert_eq!(response.ceiling_max_document_mib, 1024);
    assert_eq!(response.max_concurrent_parses, 64);
    let ui = response.ui.expect("ui advertisement is set");
    assert_eq!(ui.title, "Markup");
    assert_eq!(ui.path, "/ui/markup");
    assert_eq!(
        ui.description,
        "Markdown, HTML, AsciiDoc, LaTeX, WebVTT, BoxNote, Docling JSON to a shared block vocabulary"
    );
}

#[tokio::test]
async fn asking_for_a_format_outside_the_advertised_list_matches_the_contract() {
    // The contract couples GetServiceInfo.formats to the UNIMPLEMENTED
    // rule; this test holds the two ends together.
    let mut client = client().await;
    let advertised = client
        .get_service_info(pb::GetServiceInfoRequest {})
        .await
        .unwrap()
        .into_inner()
        .formats;
    let plain = client.clone();
    for (format, probe) in [
        (pb::MarkupFormat::Markdown, "# probe\n"),
        (pb::MarkupFormat::Html, "<html></html>"),
        (pb::MarkupFormat::Asciidoc, "= probe\n"),
        (pb::MarkupFormat::Latex, "\\documentclass{article}\n"),
        (pb::MarkupFormat::Vtt, "WEBVTT\n"),
    ] {
        let result = parse(&plain, probe, options(format)).await;
        if advertised.contains(&(format as i32)) {
            assert!(result.is_ok(), "{format:?} is advertised, so it parses");
        } else {
            assert_eq!(
                result.expect_err("not advertised").code(),
                Code::Unimplemented,
                "{format:?}"
            );
        }
    }
}

#[tokio::test]
async fn restructuredtext_is_refused_by_name_rather_than_parsed_as_markdown() {
    let client = client().await;
    let document = "Guide\n#####\n\n.. note:: mind the gap\n";
    // Sniffed: the bytes resolve to reStructuredText, which this build has
    // no fold for, so the refusal names it instead of mapping the file as
    // Markdown.
    let sniffed = parse(&client, document, sniff_options())
        .await
        .expect_err("no rst fold in this build");
    assert_eq!(sniffed.code(), Code::Unimplemented);
    assert!(sniffed.message().contains("rst"), "{}", sniffed.message());

    // Requested: the same refusal, without the sniff running at all.
    let requested = parse(&client, "anything\n", options(pb::MarkupFormat::Rst))
        .await
        .expect_err("no rst fold in this build");
    assert_eq!(requested.code(), Code::Unimplemented);
    assert!(
        requested
            .message()
            .contains("rst fold is not in this build"),
        "{}",
        requested.message()
    );
}

// ------------------------------------------------------------ span option

#[tokio::test]
async fn inline_spans_are_on_by_default_and_the_option_turns_them_off() {
    let client = client().await;
    let document = "A paragraph with **bold** words.\n";

    let default = parse_ok(&client, document, options(pb::MarkupFormat::Markdown)).await;
    let spans = &text_blocks(&default)[0].spans;
    assert_eq!(spans.len(), 1, "an unset option still carries the runs");

    let off = parse_ok(
        &client,
        document,
        pb::ParseOptions {
            format: pb::MarkupFormat::Markdown as i32,
            emit_inline_spans: Some(false),
            ..pb::ParseOptions::default()
        },
    )
    .await;
    assert!(
        text_blocks(&off)[0].spans.is_empty(),
        "clearing the option makes the stream flat again"
    );
    assert_eq!(
        text_blocks(&off)[0].text,
        text_blocks(&default)[0].text,
        "the text is the same either way"
    );
}

// ----------------------------------------------------------------- helpers

/// Drain a call to completion: events on success, the ending status
/// otherwise, wherever it surfaced.
async fn drain(
    response: Result<tonic::Response<tonic::Streaming<pb::ParseMarkupResponse>>, tonic::Status>,
) -> Result<Vec<pb::ParseMarkupResponse>, tonic::Status> {
    let mut stream = response?.into_inner();
    let mut events = Vec::new();
    while let Some(event) = stream.message().await? {
        events.push(event);
    }
    Ok(events)
}

// ------------------------------------------------------------ small events

/// Many small blocks reach a client that reads nothing until the server has
/// written the whole stream.
///
/// Each paragraph here is one small `text_block` event. h2 charges every DATA
/// frame under 256 bytes against a per-connection budget that refills only
/// as the application reads, and closes the connection with GOAWAY
/// `too_many_data_frames` when it runs out, so a server that wrote one frame
/// per event would cut this client off a hundred or so blocks in. The server
/// parses only once the upload is complete, so the client is made to fall
/// behind by not reading until the parse has finished. Every block has to
/// arrive, in order, followed by the trailer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_small_blocks_reach_a_client_that_reads_late() {
    const PARAGRAPHS: usize = 2000;
    let service = MarkupGrpc::new();
    let metrics = service.metrics();
    let mut client = client_with(service).await;
    let document: String = (0..PARAGRAPHS).map(|n| format!("p{n}\n\n")).collect();
    let requests = common::frames(&document, options(pb::MarkupFormat::Markdown), 8 * 1024);
    let mut stream = client
        .parse_markup(tokio_stream::iter(requests))
        .await
        .expect("open the call")
        .into_inner();
    tokio::time::timeout(CALL_TIMEOUT, async {
        while metrics.parses_ok.load(Ordering::Relaxed)
            + metrics.parses_failed.load(Ordering::Relaxed)
            == 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the parse ends");

    let mut events = Vec::new();
    while let Some(event) = stream.message().await.expect("stream is healthy") {
        events.push(event);
    }
    assert_eq!(
        events.len(),
        PARAGRAPHS + 2,
        "info, one block per paragraph, status"
    );
    info(&events);
    let blocks = text_blocks(&events);
    assert_eq!(blocks.len(), PARAGRAPHS);
    for (n, block) in blocks.iter().enumerate() {
        assert_eq!(block.text, format!("p{n}"));
        assert_eq!(block.label, pb::BlockLabel::Paragraph as i32);
    }
    assert!(matches!(
        events.last().and_then(|e| e.event.as_ref()),
        Some(pb::parse_markup_response::Event::Status(_))
    ));
}
