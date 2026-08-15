// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the AsciiDoc fold, over a real socket.

mod common;

use common::{
    blocks_labelled, client, images, info, options, parse_ok, sniff_options, status, table_rows,
    texts, warning,
};
use grpc_markup::proto::v1 as pb;

/// A document exercising attribute entries, the title and section ladder,
/// multiline paragraphs, an admonition, nested lists, a captioned table
/// with cell specifiers, a source listing, and an image macro.
const ASCIIDOC: &str = "\
:author: Ada Lovelace
:revnumber: 1.2

= Streaming Markup

An opening paragraph
wrapped across two source lines.

== Blocks

NOTE: Admonitions keep their text,
not their chrome.

* first bullet
** nested bullet
. first step
.. nested step
3. explicit third

.Throughput
|===
|Name 2+|Role

|Ada
|Analyst
|===

[source,rust]
----
fn main() {
    println!(\"hi\");
}
----

.The engine
image::figures/engine.png[The engine, width=200]
";

#[tokio::test]
async fn attribute_entries_become_metadata() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Asciidoc as i32);
    assert_eq!(info.title, None, "the doc title arrives as a TITLE block");
    assert_eq!(info.metadata["author"], "Ada Lovelace");
    assert_eq!(info.metadata["revnumber"], "1.2");
}

#[tokio::test]
async fn the_title_and_section_ladder_follow_doclings_levels() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Title)),
        ["Streaming Markup"]
    );
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Blocks"]);
    assert_eq!(headers[0].level, Some(1), "`==` is level 1, docling's rule");

    let second_title = parse_ok(
        &client,
        "= One\n\nbody\n\n= Two\n",
        options(pb::MarkupFormat::Asciidoc),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&second_title, pb::BlockLabel::Title)),
        ["One"]
    );
    let headers = blocks_labelled(&second_title, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Two"]);
    assert_eq!(headers[0].level, Some(1));
}

#[tokio::test]
async fn paragraphs_join_wrapped_lines_and_admonitions_carry_the_role() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    assert_eq!(
        paragraphs[0].text,
        "An opening paragraph wrapped across two source lines."
    );
    assert_eq!(paragraphs[0].source_line, Some(6));
    let admonition = paragraphs
        .iter()
        .find(|p| p.role == "admonition")
        .expect("the NOTE paragraph");
    assert_eq!(
        admonition.text,
        "Admonitions keep their text, not their chrome."
    );
}

#[tokio::test]
async fn lists_nest_by_marker_depth_with_markers_as_written() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(
        texts(&items),
        [
            "first bullet",
            "nested bullet",
            "first step",
            "nested step",
            "explicit third"
        ]
    );
    assert_eq!(items[0].marker.as_deref(), Some("*"));
    assert_eq!(items[0].list_depth, Some(1));
    assert!(!items[0].enumerated);
    assert_eq!(items[1].marker.as_deref(), Some("**"));
    assert_eq!(items[1].list_depth, Some(2));
    assert_eq!(items[2].marker.as_deref(), Some("."));
    assert!(items[2].enumerated);
    assert_eq!(items[3].list_depth, Some(2));
    assert_eq!(
        items[4].marker.as_deref(),
        Some("3."),
        "numbered as written"
    );
    assert_eq!(items[4].list_depth, Some(1));
    assert!(items[4].enumerated);
}

#[tokio::test]
async fn tables_take_their_caption_flag_the_first_row_and_strip_cell_specs() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    let start = events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableStart(start)) => Some(start),
            _ => None,
        })
        .expect("one table");
    assert_eq!(start.caption.as_deref(), Some("Throughput"));

    let rows = table_rows(&events);
    assert_eq!(rows.len(), 3, "one header row, two single-cell body rows");
    assert!(rows[0].is_header);
    assert_eq!(
        rows[0].cells[0].text, "Name",
        "the 2+ specifier is stripped"
    );
    assert_eq!(rows[0].cells[1].text, "Role");
    assert!(!rows[1].is_header);
    assert_eq!(rows[1].cells[0].text, "Ada");
    assert_eq!(rows[2].cells[0].text, "Analyst");
}

#[tokio::test]
async fn source_listings_keep_their_text_verbatim_with_the_declared_language() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    let code = blocks_labelled(&events, pb::BlockLabel::Code);
    assert_eq!(code.len(), 1);
    assert_eq!(code[0].language.as_deref(), Some("rust"));
    assert_eq!(code[0].text, "fn main() {\n    println!(\"hi\");\n}");
}

#[tokio::test]
async fn image_macros_yield_the_reference_verbatim_and_their_caption() {
    let client = client().await;
    let events = parse_ok(&client, ASCIIDOC, options(pb::MarkupFormat::Asciidoc)).await;
    let pictures = images(&events);
    assert_eq!(pictures.len(), 1);
    assert_eq!(
        pictures[0].src, "figures/engine.png",
        "the path as written, no file: mangling"
    );
    assert_eq!(pictures[0].alt, "The engine");
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Caption)),
        ["The engine"]
    );
}

#[tokio::test]
async fn an_unclosed_listing_block_warns_and_keeps_its_content() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "para\n\n----\ncode that never closes\n",
        options(pb::MarkupFormat::Asciidoc),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Code)),
        ["code that never closes"]
    );
    assert!(
        warning(&events, pb::WarningCode::MalformedStructure).is_some(),
        "{:?}",
        status(&events).warnings
    );
}

#[tokio::test]
async fn sniffed_asciidoc_dispatches_with_heuristic_evidence() {
    let client = client().await;
    let events = parse_ok(&client, "= Title\n\nSome prose.\n", sniff_options()).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Asciidoc as i32);
    assert_eq!(info.evidence, pb::FormatEvidence::Heuristic as i32);
}

#[tokio::test]
async fn empty_asciidoc_is_a_valid_empty_parse() {
    let client = client().await;
    let events = parse_ok(&client, "", options(pb::MarkupFormat::Asciidoc)).await;
    assert_eq!(events.len(), 2);
    assert_eq!(status(&events).counts.as_ref().unwrap().text_blocks, 0);
}
