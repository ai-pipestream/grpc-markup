// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the AsciiDoc fold, over a real socket.

mod common;

use common::{
    blocks_labelled, client, document, images, info, meta_scalar, options, parse_ok, sniff_options,
    status, styles, table_rows, text_blocks, texts, warning,
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
    assert_eq!(meta_scalar(&info.metadata["author"]), "Ada Lovelace");
    assert_eq!(meta_scalar(&info.metadata["revnumber"]), "1.2");
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
async fn tables_take_their_caption_flag_the_first_row_and_honour_cell_specs() {
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
    assert_eq!(rows[0].cells[0].text, "Name");
    assert_eq!(rows[0].cells[0].column_span, 1);
    assert_eq!(rows[0].cells[1].text, "Role");
    assert_eq!(
        rows[0].cells[1].column_span, 2,
        "the 2+ specifier belongs to the cell it opens, and is now honoured"
    );
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

/// A table whose every cell specifier says something: spans, a duplication
/// factor, alignments, a header style letter and an emphasis style letter.
const SPECS: &str = "\
= Specs

|===
2+^|Spanning head

|Ada 2.2+>.^|Deep h|Head
|left 2*^|both centered |plain
|s|strong e|emphatic
|===
";

#[tokio::test]
async fn every_part_of_a_cell_specifier_lands_on_the_cell() {
    let client = client().await;
    let events = parse_ok(&client, SPECS, options(pb::MarkupFormat::Asciidoc)).await;
    let rows = table_rows(&events);

    // A row that opens with a bare specifier has no phantom empty cell.
    assert_eq!(rows[0].cells.len(), 1, "{:?}", rows[0].cells);
    assert_eq!(rows[0].cells[0].text, "Spanning head");
    assert_eq!(rows[0].cells[0].column_span, 2);
    assert_eq!(rows[0].cells[0].align, pb::Alignment::Center as i32);

    // Spans, both alignments, and a style letter that means "header".
    let deep = &rows[1].cells[1];
    assert_eq!(deep.text, "Deep");
    assert_eq!((deep.column_span, deep.row_span), (2, 2));
    assert_eq!(deep.align, pb::Alignment::Right as i32);
    assert_eq!(deep.valign, pb::Alignment::Middle as i32);
    assert_eq!(rows[1].cells[2].text, "Head");
    assert!(
        rows[1].cells[2].is_header,
        "an h style letter marks the cell a header"
    );
    assert_eq!(
        rows[1].cells[1].column_index, 1,
        "column indexes advance past the spans before them"
    );
    assert_eq!(rows[1].cells[2].column_index, 3);

    // A duplication factor applies one specifier to the next two cells.
    let second = &rows[2];
    assert_eq!(second.cells[1].align, pb::Alignment::Center as i32);
    assert_eq!(second.cells[2].align, pb::Alignment::Center as i32);
    assert_eq!(second.cells[0].align, pb::Alignment::Unspecified as i32);

    // Style letters that mean emphasis put a run over the cell text.
    let styled = &rows[3];
    assert_eq!(styled.cells[1].text, "strong");
    assert_eq!(styles(&styled.cells[1].spans[0]), vec![pb::SpanStyle::Bold]);
    assert_eq!(styled.cells[2].text, "emphatic");
    assert_eq!(
        styles(&styled.cells[2].spans[0]),
        vec![pb::SpanStyle::Italic]
    );
}

#[tokio::test]
async fn admonitions_keep_which_of_the_five_they_are() {
    let client = client().await;
    let document = "\
= Admonitions

NOTE: A note.

WARNING: A warning.

CAUTION: Take care.
";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Asciidoc)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    assert_eq!(
        paragraphs
            .iter()
            .map(|block| (block.role.as_str(), block.admonition))
            .collect::<Vec<_>>(),
        vec![
            ("admonition", pb::AdmonitionKind::Note as i32),
            ("admonition", pb::AdmonitionKind::Warning as i32),
            ("admonition", pb::AdmonitionKind::Caution as i32),
        ],
        "the marker was matched to be stripped; the kind now survives it"
    );
    assert_eq!(texts(&paragraphs), ["A note.", "A warning.", "Take care."]);
}

/// A document naming its blocks the way AsciiDoc does, over multi-byte
/// text so a range that is right in bytes and wrong in code points fails.
const ANCHORED: &str = "\
= Le café

[[préparation]]
== Préparation

Le café [[goût]] est prêt.

[[étapes]]
* moudre
";

#[tokio::test]
async fn an_anchor_line_names_the_block_below_it() {
    let client = client().await;
    let events = parse_ok(&client, ANCHORED, options(pb::MarkupFormat::Asciidoc)).await;
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Préparation"]);
    assert_eq!(headers[0].anchor_id.as_deref(), Some("préparation"));
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(items[0].anchor_id.as_deref(), Some("étapes"));
    assert!(
        !texts(&text_blocks(&events))
            .iter()
            .any(|text| text.contains("[[")),
        "an anchor line is never prose"
    );
}

#[tokio::test]
async fn an_inline_anchor_leaves_a_run_where_it_stood() {
    let client = client().await;
    let events = parse_ok(&client, ANCHORED, options(pb::MarkupFormat::Asciidoc)).await;
    let paragraph = blocks_labelled(&events, pb::BlockLabel::Paragraph)
        .into_iter()
        .find(|block| block.text.starts_with("Le café"))
        .expect("the paragraph");
    assert_eq!(paragraph.text, "Le café  est prêt.");
    assert_eq!(paragraph.spans.len(), 1);
    assert_eq!(paragraph.spans[0].anchor_id.as_deref(), Some("goût"));
    let range = paragraph.spans[0].range.expect("a range");
    assert_eq!(
        (range.start, range.end),
        (8, 8),
        "an anchor names a position, counted in code points"
    );
}

#[tokio::test]
async fn asciidoc_anchors_reach_the_document_anchor_list() {
    let client = client().await;
    let with_document = pb::ParseOptions {
        format: pb::MarkupFormat::Asciidoc as i32,
        emit_document: true,
        ..pb::ParseOptions::default()
    };
    let events = parse_ok(&client, ANCHORED, with_document).await;
    let folded = document(&events).expect("the fold was requested");
    let named: Vec<&str> = folded
        .anchors
        .iter()
        .map(|anchor| anchor.name.as_str())
        .collect();
    assert_eq!(named, ["goût", "préparation", "étapes"]);
    let inline = folded
        .anchors
        .iter()
        .find(|anchor| anchor.name == "goût")
        .expect("the inline anchor");
    let target = inline.target.as_ref().expect("a target");
    assert!(target.r#ref.starts_with("#/texts/"));
    assert_eq!(
        target.range.map(|range| (range.start, range.end)),
        Some((8, 8)),
        "the pointer keeps the position the run named"
    );
}

#[tokio::test]
async fn a_leading_bom_does_not_hide_the_document_title() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "\u{feff}= Title\n\nSome prose.\n",
        options(pb::MarkupFormat::Asciidoc),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Title)),
        ["Title"],
        "a kept BOM would leave the `=` line an ordinary paragraph"
    );
}

#[tokio::test]
async fn a_no_break_space_inside_a_cell_is_text_and_not_a_specifier_boundary() {
    // U+00A0 is whitespace to `char::is_whitespace` and two bytes long. The
    // specifier scan found the last whitespace by byte and stepped one byte
    // past it, which landed inside the character and aborted the parse.
    let document = "|===\n|Nom\u{a0}complet |R\u{f4}le\n|Ada\u{a0}Lovelace |Analyste\n|===\n";
    let client = client().await;
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Asciidoc)).await;
    let rows = table_rows(&events);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].cells[0].text, "Nom complet",
        "whitespace collapses, nothing is lost"
    );
    assert_eq!(
        rows[0].cells[0].column_span, 1,
        "no specifier was read out of the prose"
    );
    assert_eq!(rows[1].cells[0].text, "Ada Lovelace");
    assert_eq!(rows[1].cells[1].text, "Analyste");
}
