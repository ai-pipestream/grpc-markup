// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the WebVTT fold, over a real socket.

mod common;

use common::{
    client, cues, document, info, meta_scalar, options, parse, parse_ok, spanned, status, styles,
    warning,
};
use grpc_markup::document::v1 as doc;
use grpc_markup::document_fold::integrity_errors;
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
    assert_eq!(meta_scalar(&info.metadata["kind"]), "captions");
    assert_eq!(meta_scalar(&info.metadata["language"]), "en");
}

#[tokio::test]
async fn cues_carry_timings_in_milliseconds_ids_and_speakers() {
    let client = client().await;
    let events = parse_ok(&client, VTT, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(
        cues.len(),
        3,
        "the block without timings is skipped; a cue whose end precedes its start is kept"
    );

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

    assert_eq!(cues[2].cue_id.as_deref(), Some("backwards"));
    assert_eq!(cues[2].start_ms, 10_000);
    assert_eq!(
        cues[2].end_ms, 10_001,
        "end <= start becomes start + 1 ms, the shortest WebVTT timestamp span"
    );
    assert_eq!(cues[2].text, "never delivered");

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
    assert_eq!(
        total, 2,
        "the backwards cue was extended, and the block without timings was skipped"
    );
    assert_eq!(status(&events).counts.as_ref().unwrap().cues, 3);
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

/// A caption file whose cue payloads carry the whole cue-span vocabulary
/// over multi-byte text, so a range that is right in bytes and wrong in code
/// points fails rather than passing quietly.
const VTT_SPANS: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nhéllo wörld <b>λόγος</b> <c.loud.narrow>SHOUT</c> <lang fr>bonjour</lang>\n\n00:00:05.000 --> 00:00:08.000\nnäver<00:00:06.000> gönna<00:00:07.000> give\n";

#[tokio::test]
async fn cue_spans_name_the_runs_the_tags_covered() {
    let client = client().await;
    let events = parse_ok(&client, VTT_SPANS, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    let cue = cues[0];
    assert_eq!(cue.text, "héllo wörld λόγος SHOUT bonjour");
    assert_eq!(cue.spans.len(), 3);

    assert_eq!(spanned(&cue.text, &cue.spans[0]), "λόγος");
    assert_eq!(styles(&cue.spans[0]), [pb::SpanStyle::Bold]);
    assert_eq!(
        cue.spans[0].range.unwrap().start,
        12,
        "the offsets count code points, not the bytes the accents cost"
    );

    assert_eq!(spanned(&cue.text, &cue.spans[1]), "SHOUT");
    assert_eq!(cue.spans[1].style_name.as_deref(), Some("loud.narrow"));

    assert_eq!(spanned(&cue.text, &cue.spans[2]), "bonjour");
    assert_eq!(cue.spans[2].language.as_deref(), Some("fr"));
}

#[tokio::test]
async fn karaoke_timestamps_become_timed_segments_over_the_cue_text() {
    let client = client().await;
    let events = parse_ok(&client, VTT_SPANS, options(pb::MarkupFormat::Vtt)).await;
    let cue = cues(&events)[1];
    assert_eq!(cue.text, "näver gönna give");
    let covered: Vec<(String, u64, u64)> = cue
        .timings
        .iter()
        .map(|timing| {
            let text: String = cue
                .text
                .chars()
                .skip(timing.range.unwrap().start as usize)
                .take((timing.range.unwrap().end - timing.range.unwrap().start) as usize)
                .collect();
            (text, timing.start_ms, timing.end_ms)
        })
        .collect();
    assert_eq!(
        covered,
        [
            ("näver".to_owned(), 5_000, 6_000),
            (" gönna".to_owned(), 6_000, 7_000),
            (" give".to_owned(), 7_000, 8_000),
        ],
        "the segments tile the whole text, starting at the cue's own start"
    );
    assert!(
        cues(&events)[0].timings.is_empty(),
        "a cue with no timestamp tag repeats nothing"
    );
}

#[tokio::test]
async fn clearing_the_span_option_keeps_the_timings_and_drops_the_runs() {
    let client = client().await;
    let flat = pb::ParseOptions {
        format: pb::MarkupFormat::Vtt as i32,
        emit_inline_spans: Some(false),
        ..pb::ParseOptions::default()
    };
    let events = parse_ok(&client, VTT_SPANS, flat).await;
    let cues = cues(&events);
    assert!(
        cues[0].spans.is_empty(),
        "the runs are what the option gates"
    );
    assert_eq!(
        cues[1].timings.len(),
        3,
        "a karaoke segment is a coordinate the source stated, not a rendering signal"
    );
}

#[tokio::test]
async fn a_karaoke_cue_folds_its_segments_as_timed_provenance() {
    let client = client().await;
    let with_document = pb::ParseOptions {
        format: pb::MarkupFormat::Vtt as i32,
        emit_document: true,
        ..pb::ParseOptions::default()
    };
    let events = parse_ok(&client, VTT_SPANS, with_document).await;
    let document = document(&events).expect("the caller asked for a Document");

    let Some(doc::base_text_item::Item::Text(styled)) = document.texts[0].item.as_ref() else {
        panic!("cues fold as TextItem");
    };
    let base = styled.base.as_ref().unwrap();
    assert!(
        base.prov.is_empty(),
        "a cue that times nothing states no box"
    );
    assert_eq!(
        base.spans.len(),
        3,
        "the cue's runs reach the Document plane"
    );
    assert_eq!(base.spans[1].style_name.as_deref(), Some("loud.narrow"));
    assert_eq!(base.spans[2].language.as_deref(), Some("fr"));
    let formatting = base.spans[0].formatting.as_ref().expect("bold is a style");
    assert!(formatting.bold);

    let Some(doc::base_text_item::Item::Text(karaoke)) = document.texts[1].item.as_ref() else {
        panic!("cues fold as TextItem");
    };
    let base = karaoke.base.as_ref().unwrap();
    assert_eq!(
        base.prov.len(),
        3,
        "one provenance item per karaoke segment"
    );
    let first = &base.prov[0];
    let time = first.time.as_ref().expect("a timed range");
    assert!((time.start_ms - 5_000.0).abs() < f64::EPSILON);
    assert!((time.end_ms - 6_000.0).abs() < f64::EPSILON);
    let charspan = first.charspan.expect("the code points it covers");
    assert_eq!((charspan.start, charspan.end), (0, 5));
    assert!(
        first.bbox.is_none() && first.page_no == 0,
        "a caption track has no page and no box, and none is invented"
    );
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

#[tokio::test]
async fn a_ruby_reading_reaches_both_planes_as_the_annotation_it_is() {
    let client = client().await;
    let ruby = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\n\
                <ruby>東京<rt>とうきょう</rt></ruby> へ ようこそ\n";
    let with_document = pb::ParseOptions {
        format: pb::MarkupFormat::Vtt as i32,
        emit_document: true,
        ..pb::ParseOptions::default()
    };
    let events = parse_ok(&client, ruby, with_document).await;

    let cue = cues(&events)[0];
    assert_eq!(
        cue.text, "東京 へ ようこそ",
        "the reading never enters the text it is printed over"
    );
    assert_eq!(spanned(&cue.text, &cue.spans[0]), "東京");
    assert_eq!(cue.spans[0].annotation.as_deref(), Some("とうきょう"));

    let document = document(&events).expect("the caller asked for a Document");
    let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
        panic!("cues fold as TextItem");
    };
    let base = item.base.as_ref().unwrap();
    assert_eq!(
        base.spans[0].annotation.as_deref(),
        Some("とうきょう"),
        "the run-level annotation the schema grew for exactly this"
    );
    assert!(
        base.spans[0].formatting.is_none(),
        "a ruby is a reading, not a rendering signal"
    );
    assert_eq!(integrity_errors(document), Vec::<String>::new());
}

#[tokio::test]
async fn a_leading_bom_still_satisfies_the_signature() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "\u{feff}WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n",
        options(pb::MarkupFormat::Vtt),
    )
    .await;
    let cues = cues(&events);
    assert_eq!(
        cues.len(),
        1,
        "the grammar allows one optional U+FEFF before WEBVTT"
    );
    assert_eq!(cues[0].text, "hi");
}

/// Named, decimal and hexadecimal references, plus one that does not resolve.
/// A reference for a line terminator drops that cue and leaves the others.
#[tokio::test]
async fn html_character_references_decode_in_cue_text_and_voice_names() {
    let client = client().await;
    let document = "\
WEBVTT

00:01.000 --> 00:03.000
It&#39;s raining.

00:03.000 --> 00:05.000
Caf&eacute; or caf&#xE9;?

00:05.000 --> 00:07.000
<v Ren&eacute;>Tom &amp; Jerry</v>

00:07.000 --> 00:09.000
R&foo;d &

00:09.000 --> 00:11.000
a&#10;b

00:11.000 --> 00:13.000
after
";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(
        cues.len(),
        5,
        "the line-terminator cue is the only one dropped"
    );
    assert_eq!(cues[0].text, "It's raining.");
    assert_eq!(cues[1].text, "Caf\u{e9} or caf\u{e9}?");
    assert_eq!(cues[2].speaker, "Ren\u{e9}");
    assert_eq!(cues[2].text, "Tom & Jerry");
    assert_eq!(cues[3].text, "R&foo;d &");
    assert_eq!(cues[4].text, "after");
    let malformed =
        warning(&events, pb::WarningCode::MalformedStructure).expect("the line terminator warns");
    assert!(malformed.message.contains("line terminator"));
    assert_eq!(malformed.count, 1);
}

/// The " " line YouTube writes at the start of a payload is part of the cue,
/// and a whitespace-only line still separates two cues when the next one
/// starts right after it.
#[tokio::test]
async fn whitespace_only_lines_stay_inside_a_cue() {
    let client = client().await;
    let youtube = "\
WEBVTT

00:00:00.160 --> 00:00:02.869 align:start position:0%
 
hello<00:00:00.640><c> everyone</c>

00:00:02.879 --> 00:00:05.110 align:start position:0%
hello everyone
welcome back
";
    let events = parse_ok(&client, youtube, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 2);
    assert!(status(&events).warnings.is_empty());
    assert_eq!(cues[0].text, "hello everyone");
    assert_eq!(cues[0].start_ms, 160);
    assert_eq!(cues[0].end_ms, 2_869);
    assert_eq!(cues[1].text, "hello everyone\nwelcome back");

    let separated = "\
WEBVTT

1
00:01.000 --> 00:02.000
one
 
2
00:02.000 --> 00:03.000
two
 
00:03.000 --> 00:04.000
three
";
    let events = parse_ok(&client, separated, options(pb::MarkupFormat::Vtt)).await;
    let found = common::cues(&events);
    assert_eq!(found.len(), 3);
    assert_eq!(
        found
            .iter()
            .map(|cue| cue.cue_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("1"), Some("2"), None]
    );
    assert_eq!(found[0].text, "one");
    assert_eq!(found[1].text, "two");
    assert_eq!(found[2].text, "three");

    let internal = "\
WEBVTT

00:01.000 --> 00:02.000
hello
 
world
";
    let events = parse_ok(&client, internal, options(pb::MarkupFormat::Vtt)).await;
    assert_eq!(common::cues(&events)[0].text, "hello\n \nworld");
}

/// Several ruby bases, an omitted last `</rt>`, an `<rt>` inside an `<rt>`,
/// and an `<rt>` outside a ruby. The annotation is the typed span field.
#[tokio::test]
async fn ruby_pairs_keep_each_base_and_its_reading() {
    let client = client().await;
    let document = "\
WEBVTT - Ruby annotations

00:01.000 --> 00:02.000
<ruby>漢字<rt>かんじ</rt></ruby> is kanji

00:02.000 --> 00:03.000
<ruby>日<rt>に</rt>本<rt>ほん</ruby>

00:03.000 --> 00:04.000
<ruby>日<rt>に<rt>ほん</rt></ruby>

00:04.000 --> 00:05.000
<rt>x</rt> and <i><rt>y</rt></i>

00:05.000 --> 00:06.000
<v Speaker><ruby>東<rt>ひがし</rt></ruby></v>
";
    let with_document = pb::ParseOptions {
        format: pb::MarkupFormat::Vtt as i32,
        emit_document: true,
        ..pb::ParseOptions::default()
    };
    let events = parse_ok(&client, document, with_document).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 5);
    assert_eq!(info(&events).title.as_deref(), Some("Ruby annotations"));

    assert_eq!(cues[0].text, "漢字 is kanji");
    assert_eq!(spanned(&cues[0].text, &cues[0].spans[0]), "漢字");
    assert_eq!(cues[0].spans[0].annotation.as_deref(), Some("かんじ"));

    assert_eq!(cues[1].text, "日本");
    assert_eq!(cues[1].spans.len(), 2);
    assert_eq!(spanned(&cues[1].text, &cues[1].spans[0]), "日");
    assert_eq!(cues[1].spans[0].annotation.as_deref(), Some("に"));
    assert_eq!(spanned(&cues[1].text, &cues[1].spans[1]), "本");
    assert_eq!(cues[1].spans[1].annotation.as_deref(), Some("ほん"));

    assert_eq!(cues[2].text, "日");
    assert_eq!(cues[2].spans[0].annotation.as_deref(), Some("にほん"));

    assert_eq!(cues[3].text, "x and y");
    assert_eq!(spanned(&cues[3].text, &cues[3].spans[0]), "y");
    assert_eq!(styles(&cues[3].spans[0]), [pb::SpanStyle::Italic]);
    assert!(cues[3].spans[0].annotation.is_none());

    assert_eq!(cues[4].speaker, "Speaker");
    assert_eq!(cues[4].text, "東");
    assert_eq!(cues[4].spans[0].annotation.as_deref(), Some("ひがし"));

    let folded = common::document(&events).expect("the caller asked for a Document");
    let Some(doc::base_text_item::Item::Text(item)) = folded.texts[1].item.as_ref() else {
        panic!("cues fold as TextItem");
    };
    let base = item.base.as_ref().unwrap();
    assert_eq!(base.spans.len(), 2);
    assert_eq!(base.spans[0].annotation.as_deref(), Some("に"));
    assert_eq!(base.spans[1].annotation.as_deref(), Some("ほん"));
    assert_eq!(integrity_errors(folded), Vec::<String>::new());
}

/// A zero-length cue and a reversed cue both become a 1 ms cue.
#[tokio::test]
async fn a_cue_shorter_than_one_millisecond_is_extended() {
    let client = client().await;
    let document = "\
WEBVTT

00:00:11.000 --> 00:00:11.000
short cue

00:00:10.000 --> 00:00:09.000
reversed

00:00:12.000 --> 00:00:13.500
normal cue
";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 3, "neither collapsed cue is dropped");
    assert_eq!((cues[0].start_ms, cues[0].end_ms), (11_000, 11_001));
    assert_eq!(cues[0].text, "short cue");
    assert_eq!((cues[1].start_ms, cues[1].end_ms), (10_000, 10_001));
    assert_eq!(cues[1].text, "reversed");
    assert_eq!((cues[2].start_ms, cues[2].end_ms), (12_000, 13_500));
    let malformed =
        warning(&events, pb::WarningCode::MalformedStructure).expect("extending a cue warns");
    assert!(malformed.message.contains("1 ms"));
    assert_eq!(malformed.count, 2);
}

/// Tags that are not cue spans are removed. Their text, and the cue, stay.
#[tokio::test]
async fn unknown_tags_strip_and_keep_their_text_and_the_cue() {
    let client = client().await;
    let document = "\
WEBVTT

00:00:01.000 --> 00:00:03.000
First cue

00:00:03.000 --> 00:00:05.000
<font color=\"#ffff00\">Where are you going?</font>

00:00:05.000 --> 00:00:07.000
<v Anna><B>Home</B>, I think.</v>

00:00:07.000 --> 00:00:09.000
Last cue
";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Vtt)).await;
    let cues = cues(&events);
    assert_eq!(cues.len(), 4);
    assert!(status(&events).warnings.is_empty());
    assert_eq!(cues[0].text, "First cue");
    assert_eq!(cues[1].text, "Where are you going?");
    assert!(cues[1].spans.is_empty());
    assert_eq!(cues[2].speaker, "Anna");
    assert_eq!(cues[2].text, "Home, I think.");
    assert!(cues[2].spans.is_empty());
    assert_eq!(cues[3].text, "Last cue");
}
