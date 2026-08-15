// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the WebVTT fold, over a real socket.

mod common;

use common::{client, cues, info, options, parse, parse_ok, status, warning};
use grpc_markup::proto::v1 as pb;

/// A caption file exercising the header title and metadata, comment and
/// style blocks, identified and bare cues, voice spans, styling tags,
/// karaoke timestamps, and two flavours of malformed cue.
const VTT: &str = "WEBVTT - Kitchen Sink\nKind: captions\nLanguage: en\n\nNOTE This comment block\nspans two lines and is skipped\n\nSTYLE\n::cue { color: lime }\n\nintro\n00:00:01.000 --> 00:00:04.000 align:start\n<v Ada>Hello <b>there</b>\n\n00:05.500 --> 00:07.250\nNever<00:00:06.000> gonna\nline two\n\nbackwards\n00:00:10.000 --> 00:00:09.000\nnever delivered\n\nthis block has no timings line\n";

#[tokio::test]
async fn the_header_yields_title_and_metadata() {
    let client = client().await;
    let events = parse_ok(&client, VTT, options(pb::MarkupFormat::Vtt)).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Vtt as i32);
    assert_eq!(info.title.as_deref(), Some("Kitchen Sink"));
    assert_eq!(info.metadata["kind"], "captions");
    assert_eq!(info.metadata["language"], "en");
}

#[tokio::test]
async fn cues_carry_timings_in_milliseconds_ids_and_speakers() {
    let client = client().await;
    let events = parse_ok(&client, VTT, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 2, "the two malformed cues are skipped");

    assert_eq!(cues[0].cue_id.as_deref(), Some("intro"));
    assert_eq!(cues[0].start_ms, 1_000);
    assert_eq!(cues[0].end_ms, 4_000);
    assert_eq!(cues[0].speaker, "Ada");
    assert_eq!(cues[0].text, "Hello there", "voice and styling tags strip");

    assert_eq!(cues[1].cue_id, None);
    assert_eq!(cues[1].start_ms, 5_500, "mm:ss.ttt without hours");
    assert_eq!(cues[1].end_ms, 7_250);
    assert_eq!(cues[1].speaker, "");
    assert_eq!(
        cues[1].text, "Never gonna\nline two",
        "karaoke timestamps vanish and payload lines join with newline"
    );

    for cue in &cues {
        let source = cue.source.as_ref().expect("provenance is always set");
        assert_eq!(source.model.as_deref(), Some("vtt"));
    }
}

#[tokio::test]
async fn malformed_cues_are_skipped_with_aggregated_warnings() {
    let client = client().await;
    let events = parse_ok(&client, VTT, options(pb::MarkupFormat::Vtt)).await;
    let malformed = warning(&events, pb::WarningCode::MalformedStructure)
        .expect("both bad cues warn under one code");
    assert!(!malformed.message.is_empty());
    let total: u64 = status(&events)
        .warnings
        .iter()
        .filter(|w| w.code == pb::WarningCode::MalformedStructure as i32)
        .map(|w| w.count)
        .sum();
    assert_eq!(total, 2, "one backwards cue, one block without timings");
    assert_eq!(status(&events).counts.as_ref().unwrap().cues, 2);
}

#[tokio::test]
async fn note_style_and_region_blocks_are_skipped() {
    let client = client().await;
    let document = "WEBVTT\n\nREGION\nid:bill width:40%\n\nNOTE hi\n\nSTYLE\n::cue { }\n\n00:00.000 --> 00:01.000\nonly cue\n";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 1);
    assert_eq!(cues[0].text, "only cue");
    assert!(
        status(&events).warnings.is_empty(),
        "skipping keyword blocks is the format's own rule, not a finding"
    );
}

#[tokio::test]
async fn a_bare_webvtt_header_with_no_cues_is_a_valid_empty_track() {
    let client = client().await;
    let events = parse_ok(&client, "WEBVTT\n", options(pb::MarkupFormat::Vtt)).await;
    assert!(cues(&events).is_empty());
    assert_eq!(info(&events).title, None);
    assert_eq!(status(&events).counts.as_ref().unwrap().cues, 0);
}

#[tokio::test]
async fn a_file_without_the_header_is_invalid_argument() {
    let client = client().await;
    for bad in [
        "",
        "not a vtt file\n",
        "WEBVTTX\n00:00.000 --> 00:01.000\nx\n",
    ] {
        let error = parse(&client, bad, options(pb::MarkupFormat::Vtt))
            .await
            .expect_err("the WEBVTT header is mandatory");
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{bad:?}");
        assert!(error.message().contains("WEBVTT"), "{}", error.message());
    }
}

#[tokio::test]
async fn crlf_line_terminators_parse_the_same() {
    let client = client().await;
    let document = "WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nwindows line\r\n";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 1);
    assert_eq!(cues[0].text, "windows line");
}
