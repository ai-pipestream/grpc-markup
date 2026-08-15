// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the two JSON folds — BoxNote and Docling JSON —
//! over a real socket.

mod common;

use common::{
    BOXNOTE, DOCLING, blocks_labelled, client, images, info, meta_scalar, options, parse, parse_ok,
    status, table_rows, text_blocks, texts, warning,
};
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
    assert!(
        paragraphs.iter().any(|p| p.text == "Agreed to ship"),
        "styled runs join into one flattened text: {:?}",
        texts(&paragraphs)
    );

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
