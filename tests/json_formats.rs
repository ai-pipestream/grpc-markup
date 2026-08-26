// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the two JSON folds — BoxNote and Docling JSON —
//! over a real socket.

mod common;

use common::{
    BOXNOTE, DOCLING, blocks_labelled, client, document, images, info, meta_scalar, options, parse,
    parse_ok, spanned, status, styles, table_rows, text_blocks, texts, warning,
};
use grpc_markup::document::v1 as doc;
use grpc_markup::proto::v1 as pb;

// -------------------------------------------------------------------- boxnote

#[tokio::test]
async fn a_boxnote_maps_headings_lists_and_paragraphs() {
    let client = client().await;
    let events = parse_ok(&client, BOXNOTE, options(pb::MarkupFormat::Boxnote)).await;

    assert_eq!(info(&events).title.as_deref(), Some("Meeting Notes"));
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Title)),
        ["Meeting Notes"],
        "the first level-1 heading is the title"
    );
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Decisions"]);
    assert_eq!(headers[0].level, Some(2));

    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    let agreed = paragraphs
        .iter()
        .find(|p| p.text == "Agreed to ship")
        .unwrap_or_else(|| panic!("{:?}", texts(&paragraphs)));
    assert_eq!(
        styles(&agreed.spans[0]),
        vec![pb::SpanStyle::Bold],
        "the styled run joins the flattened text and says it was styled"
    );
    assert_eq!(spanned(&agreed.text, &agreed.spans[0]), "ship");

    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(
        texts(&items),
        ["first", "second", "nested", "done thing"],
        "item text precedes the list nested under it"
    );
    assert_eq!(items[0].list_depth, Some(1));
    assert!(items[0].marker.is_none(), "BoxNote numbering is implicit");
    assert_eq!(items[2].list_depth, Some(2));
    assert!(items[2].enumerated);
    assert_eq!(items[3].role, "task-item-checked");
}

#[tokio::test]
async fn a_boxnote_maps_code_quotes_tables_images_and_links() {
    let client = client().await;
    let events = parse_ok(&client, BOXNOTE, options(pb::MarkupFormat::Boxnote)).await;

    let code = blocks_labelled(&events, pb::BlockLabel::Code);
    assert_eq!(texts(&code), ["SELECT 1;"]);
    assert_eq!(code[0].language.as_deref(), Some("sql"));

    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Quote)),
        ["a quote"]
    );

    let rows = table_rows(&events);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].is_header);
    assert_eq!(rows[0].cells[1].column_span, 2);
    assert_eq!(rows[0].cells[1].column_index, 1);
    assert!(!rows[1].is_header);
    assert_eq!(rows[1].cells[2].column_index, 2);
    let end = events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableEnd(end)) => Some(end),
            _ => None,
        })
        .expect("the table closes");
    assert_eq!(end.column_count, 3, "the colspan widens the table");

    let pictures = images(&events);
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].src, "https://app.box.com/s/abc");
    assert_eq!(pictures[0].alt, "diagram");
    assert_eq!(pictures[0].title, "arch.png");

    let link = blocks_labelled(&events, pb::BlockLabel::Paragraph)
        .into_iter()
        .find(|p| p.text == "See docs")
        .expect("the link paragraph");
    assert_eq!(link.hyperlink.as_deref(), Some("https://example.com/docs"));
}

#[tokio::test]
async fn an_unknown_boxnote_node_warns_and_the_parse_continues() {
    let client = client().await;
    let events = parse_ok(&client, BOXNOTE, options(pb::MarkupFormat::Boxnote)).await;
    let unmapped =
        warning(&events, pb::WarningCode::UnmappedConstruct).expect("the mystery node is reported");
    assert!(
        unmapped.message.contains("mystery_widget"),
        "{}",
        unmapped.message
    );
}

#[tokio::test]
async fn boxnote_refusals_are_invalid_argument() {
    let client = client().await;
    let cases: [(&str, &str); 4] = [
        ("", "empty input is not JSON"),
        ("{\"hello\": 1}", "no doc tree"),
        ("[1, 2]", "not an object"),
        (
            "{\"atext\": {\"text\": \"old\"}, \"pool\": {}}",
            "legacy atext note",
        ),
    ];
    for (bad, why) in cases {
        let error = parse(&client, bad, options(pb::MarkupFormat::Boxnote))
            .await
            .expect_err(why);
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{why}");
    }
    let legacy = parse(
        &client,
        "{\"atext\": {\"text\": \"old\"}, \"pool\": {}}",
        options(pb::MarkupFormat::Boxnote),
    )
    .await
    .expect_err("legacy notes are refused, not mis-parsed");
    assert!(legacy.message().contains("legacy"), "{}", legacy.message());
}

// --------------------------------------------------------------- docling json

#[tokio::test]
async fn docling_json_projects_typed_events_in_reading_order() {
    let client = client().await;
    let events = parse_ok(&client, DOCLING, options(pb::MarkupFormat::DoclingJson)).await;

    let info = info(&events);
    assert_eq!(info.title.as_deref(), Some("Quarterly Review"));
    assert_eq!(
        meta_scalar(&info.metadata["schema_name"]),
        "DoclingDocument"
    );
    assert_eq!(meta_scalar(&info.metadata["version"]), "1.3.0");

    let blocks = text_blocks(&events);
    assert_eq!(
        texts(&blocks),
        [
            "Quarterly Review",
            "Summary",
            "Revenue up",
            "Costs flat",
            "print(1)"
        ],
        "body order is reading order"
    );
    assert_eq!(blocks[0].label, pb::BlockLabel::Title as i32);
    assert_eq!(blocks[1].label, pb::BlockLabel::SectionHeader as i32);
    assert_eq!(blocks[1].level, Some(1));
    assert_eq!(blocks[2].label, pb::BlockLabel::ListItem as i32);
    assert_eq!(blocks[2].list_depth, Some(1));
    assert_eq!(blocks[2].marker.as_deref(), Some("-"));
    assert_eq!(blocks[4].label, pb::BlockLabel::Code as i32);
    assert_eq!(blocks[4].language.as_deref(), Some("python"));

    let rows = table_rows(&events);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].is_header);
    assert_eq!(rows[0].cells[0].text, "Region");
    assert_eq!(rows[1].cells[1].text, "+12%");

    let pictures = images(&events);
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].src, "img/chart.png");

    let counts = status(&events).counts.as_ref().unwrap();
    assert_eq!(counts.text_blocks, 5);
    assert_eq!(counts.tables, 1);
    assert_eq!(counts.images, 1);
}

/// A serialized document whose text items and table cells carry the runs
/// and the callout kind the schema declares, over multi-byte text: a range
/// that is right in bytes and wrong in code points fails here.
const DOCLING_SPANS: &str = r##"{
  "schema_name": "DoclingDocument",
  "version": "1.3.0",
  "name": "Runs",
  "furniture": {"self_ref": "#/furniture", "children": [], "content_layer": "furniture",
                "name": "_root_", "label": "unspecified"},
  "body": {"self_ref": "#/body",
           "children": [{"$ref": "#/texts/0"}, {"$ref": "#/tables/0"}],
           "content_layer": "body", "name": "_root_", "label": "unspecified"},
  "groups": [],
  "texts": [
    {"self_ref": "#/texts/0", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "text", "prov": [],
     "orig": "héllo wörld λόγος see §2",
     "text": "héllo wörld λόγος see §2",
     "admonition_kind": "warning",
     "spans": [
       {"range": {"start": 12, "end": 17}, "formatting": {"bold": true, "monospace": true}},
       {"range": {"start": 18, "end": 24}, "hyperlink": "https://example.com/a",
        "link_title": "Appendix", "language": "fr", "style_name": "Emphasis",
        "reference": "sec-two", "reference_kind": "section",
        "target": {"$ref": "#/texts/0", "range": {"start": 0, "end": 5}}}
     ]}
  ],
  "tables": [
    {"self_ref": "#/tables/0", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "table", "prov": [],
     "captions": [], "references": [], "footnotes": [],
     "data": {"num_rows": 1, "num_cols": 1, "table_cells": [],
       "grid": [[{"row_span": 1, "col_span": 1, "start_row_offset_idx": 0,
                  "end_row_offset_idx": 1, "start_col_offset_idx": 0, "end_col_offset_idx": 1,
                  "text": "naïve λ", "column_header": false, "row_header": false,
                  "row_section": false, "align": "center", "valign": "bottom",
                  "spans": [{"range": {"start": 0, "end": 5},
                             "formatting": {"italic": true}}]}]]}}
  ],
  "pages": {}
}"##;

#[tokio::test]
async fn the_runs_a_document_declares_survive_the_round_trip() {
    let client = client().await;
    let with_document = pb::ParseOptions {
        format: pb::MarkupFormat::DoclingJson as i32,
        emit_document: true,
        ..pb::ParseOptions::default()
    };
    let events = parse_ok(&client, DOCLING_SPANS, with_document).await;
    let document = document(&events).expect("the caller asked for a Document");

    let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
        panic!("the item is a text item");
    };
    let base = item.base.as_ref().unwrap();
    assert_eq!(
        base.admonition_kind.as_deref(),
        Some("warning"),
        "the callout kind is ingested, not defaulted away"
    );
    assert_eq!(base.spans.len(), 2, "both runs come back");

    let bold = &base.spans[0];
    assert_eq!(bold.range, Some(doc::IntSpan { start: 12, end: 17 }));
    let covered: String = base.text.chars().skip(12).take(5).collect();
    assert_eq!(covered, "λόγος", "the range indexes code points");
    let formatting = bold.formatting.as_ref().expect("the run states a style");
    assert!(formatting.bold && formatting.monospace);

    let linked = &base.spans[1];
    assert_eq!(linked.hyperlink.as_deref(), Some("https://example.com/a"));
    assert_eq!(linked.link_title.as_deref(), Some("Appendix"));
    assert_eq!(linked.language.as_deref(), Some("fr"));
    assert_eq!(linked.style_name.as_deref(), Some("Emphasis"));
    assert_eq!(linked.reference.as_deref(), Some("sec-two"));
    assert_eq!(
        linked.reference_kind,
        Some(doc::ReferenceKind::Section as i32)
    );
    let target = linked.target.as_ref().expect("the resolved pointer");
    assert_eq!(target.r#ref, "#/texts/0");
    assert_eq!(target.range, Some(doc::IntSpan { start: 0, end: 5 }));

    let cell = &document.tables[0].data.as_ref().unwrap().grid[0].cells[0];
    assert_eq!(cell.spans.len(), 1, "a cell's runs come back too");
    assert!(cell.spans[0].formatting.as_ref().unwrap().italic);
    assert_eq!(cell.align, Some(doc::Alignment::Center as i32));
    assert_eq!(cell.valign, Some(doc::VerticalAlignment::Bottom as i32));
}

#[tokio::test]
async fn the_ingested_runs_reach_the_typed_wire_too() {
    let client = client().await;
    let events = parse_ok(
        &client,
        DOCLING_SPANS,
        options(pb::MarkupFormat::DoclingJson),
    )
    .await;
    let block = text_blocks(&events)[0];
    assert_eq!(block.admonition, pb::AdmonitionKind::Warning as i32);
    assert_eq!(block.spans.len(), 2);
    assert_eq!(spanned(&block.text, &block.spans[0]), "λόγος");
    assert_eq!(
        styles(&block.spans[0]),
        [pb::SpanStyle::Bold, pb::SpanStyle::Code]
    );
    assert_eq!(spanned(&block.text, &block.spans[1]), "see §2");
    assert_eq!(
        block.spans[1].reference_kind,
        pb::ReferenceKind::CrossRef as i32,
        "the wire has no arm of its own for a section reference"
    );
    assert_eq!(block.spans[1].style_name.as_deref(), Some("Emphasis"));

    let cell = &table_rows(&events)[0].cells[0];
    assert_eq!(spanned(&cell.text, &cell.spans[0]), "naïve");
    assert_eq!(styles(&cell.spans[0]), [pb::SpanStyle::Italic]);
    assert_eq!(cell.align, pb::Alignment::Center as i32);
    assert_eq!(cell.valign, pb::Alignment::Bottom as i32);
}

#[tokio::test]
async fn the_runs_and_the_callout_are_no_longer_unmapped_keys() {
    let client = client().await;
    let events = parse_ok(
        &client,
        DOCLING_SPANS,
        options(pb::MarkupFormat::DoclingJson),
    )
    .await;
    let unmapped: Vec<&str> = status(&events)
        .warnings
        .iter()
        .filter(|w| w.code == pb::WarningCode::UnmappedConstruct as i32)
        .map(|w| w.message.as_str())
        .collect();
    assert!(
        unmapped.is_empty(),
        "every key this fixture writes is now consumed: {unmapped:?}"
    );
}

#[tokio::test]
async fn keys_the_mapping_does_not_consume_warn_once_per_key_path() {
    let client = client().await;
    let events = parse_ok(&client, DOCLING, options(pb::MarkupFormat::DoclingJson)).await;
    let warnings: Vec<&pb::ParseWarning> = status(&events)
        .warnings
        .iter()
        .filter(|w| w.code == pb::WarningCode::UnmappedConstruct as i32)
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("$.pictures[].annotations")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().all(|w| w.count == 1),
        "one aggregated warning per key path: {warnings:?}"
    );
}

#[tokio::test]
async fn docling_refusals_are_invalid_argument() {
    let client = client().await;
    let cases: [(&str, &str); 4] = [
        ("", "empty input is not JSON"),
        ("not json", "not JSON at all"),
        ("{\"hello\": 1}", "no schema_name"),
        (
            "{\"schema_name\": \"SomethingElse\", \"texts\": []}",
            "a schema_name that is not docling's",
        ),
    ];
    for (bad, why) in cases {
        let error = parse(&client, bad, options(pb::MarkupFormat::DoclingJson))
            .await
            .expect_err(why);
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{why}");
        assert!(
            error.message().contains("DoclingDocument"),
            "{}",
            error.message()
        );
    }
}

/// A note whose marks are the point: several on one run, a colour and a
/// size, and a link in a paragraph that holds more than one run.
const MARKED: &str = r##"{
  "title": "Marks",
  "doc": {"type": "doc", "content": [
    {"type": "paragraph", "content": [
      {"type": "text", "text": "Le café ", "marks": [{"type": "strong"}]},
      {"type": "text", "text": "est prêt",
       "marks": [{"type": "em"},
                 {"type": "font_color", "attrs": {"color": "#336699"}}]},
      {"type": "text", "text": ", voir "},
      {"type": "text", "text": "les notes",
       "marks": [{"type": "link", "attrs": {"href": "https://box.com/notes"}}]}
    ]}
  ]}
}"##;

#[tokio::test]
async fn boxnote_marks_beyond_link_reach_the_wire() {
    let client = client().await;
    let events = parse_ok(&client, MARKED, options(pb::MarkupFormat::Boxnote)).await;
    let paragraph = text_blocks(&events)[0];
    assert_eq!(paragraph.text, "Le café est prêt, voir les notes");
    assert!(
        paragraph.hyperlink.is_none(),
        "four runs is not a whole-block link"
    );
    let seen: Vec<(String, Vec<pb::SpanStyle>)> = paragraph
        .spans
        .iter()
        .map(|span| (spanned(&paragraph.text, span), styles(span)))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("Le café".to_owned(), vec![pb::SpanStyle::Bold]),
            ("est prêt".to_owned(), vec![pb::SpanStyle::Italic]),
            ("les notes".to_owned(), Vec::new()),
        ],
        "the accented runs do not shift the ranges after them"
    );
    assert_eq!(paragraph.spans[1].color.as_deref(), Some("#336699"));
    assert_eq!(
        paragraph.spans[2].hyperlink.as_deref(),
        Some("https://box.com/notes"),
        "a second run in the block used to delete the link outright"
    );
}
