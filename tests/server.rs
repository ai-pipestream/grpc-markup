// SPDX-License-Identifier: Apache-2.0

//! The wire-level rules: request framing, the byte cap, admission control,
//! sniff resolution and its evidence, the `UNIMPLEMENTED` refusals, and
//! `GetServiceInfo`.

mod common;

use common::{
    BOXNOTE, CALL_TIMEOUT, DOCLING, LiveUpload, client, client_with, info, options, parse,
    parse_ok, sniff_options,
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
        "LaTeX, WebVTT, BoxNote, Docling JSON to a shared block vocabulary"
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
