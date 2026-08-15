// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the HTML fold, over a real socket.

mod common;

use common::{
    blocks_labelled, client, images, info, meta_scalar, options, parse_ok, sniff_options, status,
    table_rows, text_blocks, texts, warning,
};
use grpc_markup::proto::v1 as pb;

/// A whole document exercising head metadata, the heading ladder, inline
/// markup, quotes, nested and definition lists, a spanned table with a
/// caption, code, a figure, and a whole-paragraph link.
const HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
  <title>Streaming Markup</title>
  <meta name="author" content="Ada Lovelace">
  <meta name="description" content="A fixture">
  <meta charset="utf-8">
  <style>p { color: red }</style>
  <script>alert("never text");</script>
</head>
<body>
  <h1>Streaming <em>Markup</em></h1>
  <p>An opening paragraph with <strong>bold</strong> and <code>inline code</code>.</p>
  <h2>Blocks</h2>
  <blockquote><p>A quoted sentence.</p></blockquote>
  <ul>
    <li>first bullet</li>
    <li>second bullet
      <ol start="3">
        <li>nested three</li>
        <li>nested four</li>
      </ol>
    </li>
  </ul>
  <dl>
    <dt>term</dt>
    <dd>its description</dd>
  </dl>
  <pre class="language-rust">fn main() {
    println!("hi");
}</pre>
  <table>
    <caption>Throughput</caption>
    <thead>
      <tr><th>Name</th><th colspan="2">Role</th></tr>
    </thead>
    <tbody>
      <tr><td>Ada</td><td>Analyst</td><td>Lead</td></tr>
    </tbody>
  </table>
  <figure>
    <img src="figures/engine.png" alt="The engine" title="Plate 1">
    <figcaption>The analytical engine.</figcaption>
  </figure>
  <p><a href="https://example.com">Docs</a></p>
</body>
</html>
"#;

#[tokio::test]
async fn the_head_yields_title_and_prefixed_meta_keys() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Html as i32);
    assert_eq!(info.title.as_deref(), Some("Streaming Markup"));
    assert_eq!(meta_scalar(&info.metadata["meta.author"]), "Ada Lovelace");
    assert_eq!(meta_scalar(&info.metadata["meta.description"]), "A fixture");
    assert!(
        !info.metadata.keys().any(|k| k.contains("charset")),
        "a nameless meta declares nothing"
    );
}

#[tokio::test]
async fn the_first_h1_is_the_title_and_other_headings_carry_levels() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let titles = blocks_labelled(&events, pb::BlockLabel::Title);
    assert_eq!(
        texts(&titles),
        ["Streaming Markup"],
        "inline markup inside the heading flattens"
    );
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Blocks"]);
    assert_eq!(headers[0].level, Some(2));

    let second_h1 = parse_ok(
        &client,
        "<h1>One</h1><p>x</p><h1>Two</h1>",
        options(pb::MarkupFormat::Html),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&second_h1, pb::BlockLabel::Title)),
        ["One"]
    );
    let headers = blocks_labelled(&second_h1, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&headers), ["Two"]);
    assert_eq!(headers[0].level, Some(1));
}

#[tokio::test]
async fn paragraphs_flatten_inline_markup_and_script_style_never_become_text() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    assert_eq!(
        paragraphs[0].text,
        "An opening paragraph with bold and inline code."
    );
    for block in text_blocks(&events) {
        assert!(!block.text.contains("never text"), "script leaked");
        assert!(!block.text.contains("color: red"), "style leaked");
        assert_eq!(
            block.source.as_ref().unwrap().model.as_deref(),
            Some("html")
        );
    }
}

#[tokio::test]
async fn blockquote_paragraphs_carry_the_quote_label() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Quote)),
        ["A quoted sentence."]
    );
}

#[tokio::test]
async fn lists_nest_and_only_a_numbered_start_writes_markers() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    let list_items: Vec<&&pb::TextBlock> =
        items.iter().filter(|item| item.role.is_empty()).collect();
    assert_eq!(
        list_items
            .iter()
            .map(|i| i.text.clone())
            .collect::<Vec<_>>(),
        [
            "first bullet",
            "second bullet",
            "nested three",
            "nested four"
        ]
    );
    assert_eq!(list_items[0].list_depth, Some(1));
    assert!(!list_items[0].enumerated);
    assert!(
        list_items[0].marker.is_none(),
        "an unnumbered source writes no marker"
    );
    assert_eq!(list_items[2].list_depth, Some(2));
    assert!(list_items[2].enumerated);
    assert_eq!(
        list_items[2].marker.as_deref(),
        Some("3."),
        "an explicit ol start writes the numbering out"
    );
    assert_eq!(list_items[3].marker.as_deref(), Some("4."));
}

#[tokio::test]
async fn definition_lists_use_the_proto_role_vocabulary() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    let term = items
        .iter()
        .find(|i| i.role == "definition-term")
        .expect("the dt");
    assert_eq!(term.text, "term");
    assert_eq!(term.list_depth, Some(1));
    let description = items
        .iter()
        .find(|i| i.role == "definition-description")
        .expect("the dd");
    assert_eq!(description.text, "its description");
    assert_eq!(description.list_depth, Some(2), "nested under its term");
}

#[tokio::test]
async fn pre_keeps_text_verbatim_with_the_language_class() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let code = blocks_labelled(&events, pb::BlockLabel::Code);
    assert_eq!(code.len(), 1);
    assert_eq!(code[0].text, "fn main() {\n    println!(\"hi\");\n}");
    assert_eq!(code[0].language.as_deref(), Some("rust"));
}

#[tokio::test]
async fn tables_stream_with_caption_header_flags_and_spans() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let start = events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableStart(start)) => Some(start),
            _ => None,
        })
        .expect("one table");
    assert_eq!(start.caption.as_deref(), Some("Throughput"));

    let rows = table_rows(&events);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].is_header, "the thead row is a header row");
    assert_eq!(rows[0].cells[1].column_span, 2);
    assert!(!rows[1].is_header);
    assert_eq!(rows[1].cells[2].column_index, 2);

    let end = events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableEnd(end)) => Some(end),
            _ => None,
        })
        .expect("the table closes");
    assert_eq!(end.row_count, 2);
    assert_eq!(end.column_count, 3);
}

#[tokio::test]
async fn a_rowspan_shifts_the_next_rows_columns() {
    let client = client().await;
    let document = "<table><tr><td rowspan=\"2\">a</td><td>b</td></tr><tr><td>c</td></tr></table>";
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Html)).await;
    let rows = table_rows(&events);
    assert_eq!(rows[1].cells[0].text, "c");
    assert_eq!(
        rows[1].cells[0].column_index, 1,
        "the spanned cell above still occupies column 0"
    );
}

#[tokio::test]
async fn figures_yield_the_image_then_its_caption() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let pictures = images(&events);
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].src, "figures/engine.png");
    assert_eq!(pictures[0].alt, "The engine");
    assert_eq!(pictures[0].title, "Plate 1");
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Caption)),
        ["The analytical engine."]
    );
}

#[tokio::test]
async fn a_paragraph_that_is_entirely_one_link_carries_the_hyperlink() {
    let client = client().await;
    let events = parse_ok(&client, HTML, options(pb::MarkupFormat::Html)).await;
    let docs = blocks_labelled(&events, pb::BlockLabel::Paragraph)
        .into_iter()
        .find(|p| p.text == "Docs")
        .expect("the link paragraph");
    assert_eq!(docs.hyperlink.as_deref(), Some("https://example.com"));
}

#[tokio::test]
async fn a_body_only_fragment_parses_without_a_wrapper() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "<h2>Fragment</h2><p>Loose <b>text</b> works.</p>",
        options(pb::MarkupFormat::Html),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::SectionHeader)),
        ["Fragment"]
    );
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["Loose text works."]
    );
    assert!(
        !status(&events)
            .warnings
            .iter()
            .any(|w| w.code == pb::WarningCode::MalformedStructure as i32),
        "a fragment is a legal input, not a damaged document: {:?}",
        status(&events).warnings
    );
}

#[tokio::test]
async fn loose_text_between_blocks_flushes_as_paragraphs() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "<div>before<p>inside</p>after</div>",
        options(pb::MarkupFormat::Html),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["before", "inside", "after"]
    );
}

#[tokio::test]
async fn an_unmapped_element_flattens_with_one_aggregated_warning() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "<p>keep <blink>this</blink> and <blink>that</blink></p>",
        options(pb::MarkupFormat::Html),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["keep this and that"],
        "the text is flattened into the parent, not dropped"
    );
    let unmapped = warning(&events, pb::WarningCode::UnmappedConstruct)
        .expect("the element is named on the trailer");
    assert_eq!(unmapped.message, "blink");
    assert_eq!(unmapped.count, 2, "aggregated per element name");
}

#[tokio::test]
async fn tree_builder_repairs_surface_as_malformed_structure() {
    let client = client().await;
    // A stray </b> end tag with no open element: html5ever drops it and
    // says so.
    let events = parse_ok(
        &client,
        "<!DOCTYPE html><html><body><p>text</b></p></body></html>",
        options(pb::MarkupFormat::Html),
    )
    .await;
    assert!(
        warning(&events, pb::WarningCode::MalformedStructure).is_some(),
        "{:?}",
        status(&events).warnings
    );
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["text"],
        "the repaired tree still parses"
    );
}

#[tokio::test]
async fn sniffed_html_dispatches_with_signature_evidence() {
    let client = client().await;
    for document in [
        "<!DOCTYPE html>\n<html><body><p>x</p></body></html>",
        "<html lang=\"en\"><p>x</p></html>",
    ] {
        let events = parse_ok(&client, document, sniff_options()).await;
        let info = info(&events);
        assert_eq!(info.format, pb::MarkupFormat::Html as i32);
        assert_eq!(info.evidence, pb::FormatEvidence::Signature as i32);
    }
}

#[tokio::test]
async fn empty_html_is_a_valid_empty_parse() {
    let client = client().await;
    let events = parse_ok(&client, "", options(pb::MarkupFormat::Html)).await;
    assert_eq!(events.len(), 2, "info and status; HTML has no magic header");
    assert_eq!(status(&events).counts.as_ref().unwrap().text_blocks, 0);
}
