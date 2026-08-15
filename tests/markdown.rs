// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the Markdown fold, over a real socket.

mod common;

use common::{
    blocks_labelled, client, images, indexes, info, options, parse_ok, status, table_rows,
    text_blocks, texts, warning,
};
use grpc_markup::proto::v1 as pb;

/// A document exercising front matter, headings, inline markup, quotes,
/// nested lists, fenced code, a pipe table, an image, footnotes, display
/// math and a whole-paragraph link.
const MARKDOWN: &str = r#"---
title: Streaming Markup
year: 1843
draft: false
authors:
  - Ada Lovelace
  - Charles Babbage
publisher:
  name: Analytical Press
  city: London
---
# Streaming Markup

An opening paragraph with *emphasis* and `inline code`.

## Blocks

> A quoted sentence.
> Split across source lines.

- first bullet
- second bullet
  1. nested one
  2. nested two

```rust
fn main() {
    println!("hi");
}
```

| Name | Role |
| ---- | ---- |
| Ada | Analyst |
| Charles | Engine |

![The engine](figures/engine.png "Plate 1")

A paragraph mentioning[^n] a footnote.

[^n]: The footnote body.

$$E = mc^2$$

[Docs](https://example.com)
"#;

/// 1-based line of the first occurrence of `needle` in `haystack`.
fn line_of(haystack: &str, needle: &str) -> u32 {
    u32::try_from(
        haystack
            .lines()
            .position(|line| line.contains(needle))
            .expect("fixture line")
            + 1,
    )
    .unwrap()
}

#[tokio::test]
async fn front_matter_becomes_metadata_and_title() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Markdown as i32);
    assert_eq!(info.evidence, pb::FormatEvidence::Requested as i32);
    assert_eq!(info.title.as_deref(), Some("Streaming Markup"));

    let metadata = &info.metadata;
    assert_eq!(metadata["title"], "Streaming Markup");
    assert_eq!(metadata["year"], "1843", "scalars land verbatim");
    assert_eq!(metadata["draft"], "false");
    assert_eq!(
        metadata["publisher.name"], "Analytical Press",
        "nested maps flatten to dotted keys"
    );
    assert_eq!(metadata["publisher.city"], "London");
    assert_eq!(
        metadata["authors"], r#"["Ada Lovelace","Charles Babbage"]"#,
        "sequences are JSON-encoded"
    );
}

#[tokio::test]
async fn the_first_h1_is_the_title_and_deeper_headings_carry_levels() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;

    let titles = blocks_labelled(&events, pb::BlockLabel::Title);
    assert_eq!(texts(&titles), ["Streaming Markup"]);
    assert!(titles[0].level.is_none(), "a title has no level");
    assert_eq!(
        titles[0].source_line,
        Some(line_of(MARKDOWN, "# Streaming Markup")),
        "source lines count from the top of the file, front matter included"
    );

    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Blocks"]);
    assert_eq!(headers[0].level, Some(2));
}

#[tokio::test]
async fn a_second_h1_is_a_section_header_not_a_second_title() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "# One\n\nbody\n\n# Two\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Title)),
        ["One"]
    );
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Two"]);
    assert_eq!(headers[0].level, Some(1));
}

#[tokio::test]
async fn paragraphs_flatten_inline_markup_and_collapse_whitespace() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    assert_eq!(
        paragraphs[0].text,
        "An opening paragraph with emphasis and inline code."
    );
    for block in &paragraphs {
        let source = block.source.as_ref().expect("provenance is always set");
        assert_eq!(source.collector, "markup");
        assert_eq!(source.model.as_deref(), Some("markdown"));
        assert!(source.confidence.is_none());
    }
}

#[tokio::test]
async fn quote_paragraphs_carry_the_quote_label() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let quotes = blocks_labelled(&events, pb::BlockLabel::Quote);
    assert_eq!(
        texts(&quotes),
        ["A quoted sentence. Split across source lines."],
        "one block per quoted paragraph, line breaks collapsed"
    );
}

#[tokio::test]
async fn lists_carry_markers_depth_and_enumeration() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(
        texts(&items),
        ["first bullet", "second bullet", "nested one", "nested two"],
        "the parent item precedes the list nested under it"
    );
    assert_eq!(items[0].marker.as_deref(), Some("-"));
    assert_eq!(items[0].list_depth, Some(1));
    assert!(!items[0].enumerated);
    assert_eq!(items[2].marker.as_deref(), Some("1."), "markers as written");
    assert_eq!(items[3].marker.as_deref(), Some("2."));
    assert_eq!(items[2].list_depth, Some(2));
    assert!(items[2].enumerated);
}

#[tokio::test]
async fn task_list_items_record_their_checkbox_in_role() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "- [x] shipped\n- [ ] pending\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(texts(&items), ["shipped", "pending"]);
    assert_eq!(items[0].role, "task-item-checked");
    assert_eq!(items[1].role, "task-item");
}

#[tokio::test]
async fn fenced_code_keeps_its_text_verbatim_with_the_declared_language() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let code = blocks_labelled(&events, pb::BlockLabel::Code);
    assert_eq!(code.len(), 1);
    assert_eq!(code[0].language.as_deref(), Some("rust"));
    assert_eq!(
        code[0].text, "fn main() {\n    println!(\"hi\");\n}",
        "code keeps indentation and newlines"
    );
}

#[tokio::test]
async fn an_indented_code_block_has_no_language() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "prose\n\n    indented code\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    let code = blocks_labelled(&events, pb::BlockLabel::Code);
    assert_eq!(texts(&code), ["indented code"]);
    assert!(code[0].language.is_none());
}

#[tokio::test]
async fn pipe_tables_stream_with_the_header_row_flagged() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;

    let starts: Vec<&pb::TableStart> = events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableStart(start)) => Some(start),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].source_line,
        Some(line_of(MARKDOWN, "| Name | Role |"))
    );

    let rows = table_rows(&events);
    assert_eq!(rows.len(), 3);
    assert!(rows[0].is_header);
    assert!(rows[0].cells.iter().all(|cell| cell.is_header));
    assert_eq!(rows[0].cells[0].text, "Name");
    assert!(!rows[1].is_header);
    assert_eq!(rows[1].cells[1].text, "Analyst");
    assert_eq!(rows[2].cells[0].column_index, 0);
    assert_eq!(rows[2].cells[1].column_index, 1);

    let ends: Vec<&pb::TableEnd> = events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableEnd(end)) => Some(end),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].table_ref, starts[0].table_ref);
    assert_eq!(ends[0].row_count, 3);
    assert_eq!(ends[0].column_count, 2);

    let counts = status(&events).counts.as_ref().unwrap();
    assert_eq!(counts.tables, 1);
    assert_eq!(counts.table_rows, 3);
}

#[tokio::test]
async fn images_carry_src_alt_and_title_as_written() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let pictures = images(&events);
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].src, "figures/engine.png");
    assert_eq!(pictures[0].alt, "The engine");
    assert_eq!(pictures[0].title, "Plate 1");
    assert_eq!(status(&events).counts.as_ref().unwrap().images, 1);
}

#[tokio::test]
async fn footnote_definitions_become_footnote_blocks_named_by_label() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let notes = blocks_labelled(&events, pb::BlockLabel::Footnote);
    assert_eq!(texts(&notes), ["The footnote body."]);
    assert_eq!(notes[0].role, "n");
    // The reference stays in the referring paragraph's text.
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    assert!(
        paragraphs
            .iter()
            .any(|p| p.text == "A paragraph mentioning[n] a footnote."),
        "{:?}",
        texts(&paragraphs)
    );
}

#[tokio::test]
async fn display_math_is_a_formula_block_kept_verbatim() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let formulas = blocks_labelled(&events, pb::BlockLabel::Formula);
    assert_eq!(texts(&formulas), ["E = mc^2"]);
}

#[tokio::test]
async fn a_paragraph_that_is_entirely_one_link_carries_the_hyperlink() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    let docs = paragraphs
        .iter()
        .find(|p| p.text == "Docs")
        .expect("the link paragraph");
    assert_eq!(docs.hyperlink.as_deref(), Some("https://example.com"));
    // A link inside a longer paragraph flattens to its text instead.
    assert!(
        paragraphs
            .iter()
            .filter(|p| p.text != "Docs")
            .all(|p| p.hyperlink.is_none())
    );
}

#[tokio::test]
async fn embedded_html_is_flattened_to_text_with_a_warning() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "Hello <b>bold</b> world.\n\n<div>\n<p>Block content</p>\n</div>\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    let blocks = text_blocks(&events);
    assert!(
        blocks.iter().any(|b| b.text == "Hello bold world."),
        "inline HTML tags vanish, their text stays: {:?}",
        texts(&blocks)
    );
    let html_block = blocks
        .iter()
        .find(|b| b.role == "html")
        .expect("the flattened HTML block");
    assert_eq!(html_block.text, "Block content");
    assert!(
        warning(&events, pb::WarningCode::EmbeddedHtmlFlattened).is_some(),
        "{:?}",
        status(&events).warnings
    );
}

#[tokio::test]
async fn broken_front_matter_warns_and_is_not_emitted_as_content() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "---\n\t: not yaml at all: [\n---\nA real paragraph.\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    assert!(info(&events).metadata.is_empty());
    let blocks = text_blocks(&events);
    assert_eq!(texts(&blocks), ["A real paragraph."]);
    assert!(
        warning(&events, pb::WarningCode::Metadata).is_some(),
        "{:?}",
        status(&events).warnings
    );
}

#[tokio::test]
async fn thematic_breaks_aggregate_into_one_warning_with_a_count() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "a\n\n---\n\nb\n\n---\n\nc\n\n---\n",
        options(pb::MarkupFormat::Markdown),
    )
    .await;
    let warning = warning(&events, pb::WarningCode::UnmappedConstruct)
        .expect("thematic breaks have no mapping");
    assert_eq!(warning.count, 3, "aggregated, not repeated");
    assert_eq!(status(&events).warnings.len(), 1);
}

#[tokio::test]
async fn empty_markdown_is_a_valid_empty_parse() {
    let client = client().await;
    let events = parse_ok(&client, "", options(pb::MarkupFormat::Markdown)).await;
    assert_eq!(events.len(), 2, "info and status, nothing else");
    let status = status(&events);
    assert_eq!(status.format, pb::MarkupFormat::Markdown as i32);
    assert_eq!(status.counts.as_ref().unwrap().text_blocks, 0);
    assert_eq!(status.bytes_consumed, 0);
}

#[tokio::test]
async fn stream_indexes_are_dense_and_ascending_across_event_kinds() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let indexes = indexes(&events);
    assert!(!indexes.is_empty());
    assert_eq!(
        indexes,
        (0..indexes.len() as u64).collect::<Vec<_>>(),
        "indexes are dense from zero across blocks, tables and images"
    );
}

#[tokio::test]
async fn the_trailer_accounts_for_bytes_and_blocks() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, options(pb::MarkupFormat::Markdown)).await;
    let status = status(&events);
    assert_eq!(status.bytes_consumed, MARKDOWN.len() as u64);
    assert_eq!(
        status.counts.as_ref().unwrap().text_blocks,
        text_blocks(&events).len() as u64
    );
    assert_eq!(status.counts.as_ref().unwrap().cues, 0);
}
