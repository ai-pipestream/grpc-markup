// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the HTML fold, over a real socket.

mod common;

use common::{
    blocks_labelled, client, document, images, info, meta_scalar, options, parse_ok, sniff_options,
    spanned, status, styles, table_rows, table_starts, text_blocks, texts, warning,
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

// ---------------------------------------------------------- inline runs

/// A fragment whose inline elements, anchors and table geometry are the
/// point: the twenty-seven-name inline set, in-prose links, a fragment
/// link, ids, `<colgroup>` and per-cell alignment.
const INLINE: &str = r##"<body>
<h2 id="méthode">La <em>méthode</em></h2>
<p id="lede">Le <b>café</b> est <i>prêt</i> et <s>froid</s>, dit
<a href="https://ada.example" title="Her page">Ada</a> et
<a href="https://grace.example">Grace</a>, voir <a href="#méthode">plus bas</a>.</p>
<p>Water is H<sub>2</sub>O, the area is r<sup>2</sup>, and
<abbr title="HyperText Markup Language">HTML</abbr> is <mark>marked</mark>,
<ins>added</ins>, <del>removed</del> and <small>quiet</small>, at
<time datetime="1843-10-01">October</time>.</p>
<table>
  <colgroup>
    <col align="left">
    <col align="center" width="40%">
    <col align="right" span="2">
  </colgroup>
  <tr><td>a</td><td valign="top"><b>b</b> <a href="https://c.example">c</a></td><td>d</td><td>e</td></tr>
</table>
</body>"##;

#[tokio::test]
async fn the_inline_element_set_leaves_runs_instead_of_bare_text() {
    let client = client().await;
    let events = parse_ok(&client, INLINE, options(pb::MarkupFormat::Html)).await;
    let paragraph = text_blocks(&events)
        .into_iter()
        .find(|block| block.text.starts_with("Le "))
        .expect("the lede");
    let styled: Vec<(String, Vec<pb::SpanStyle>)> = paragraph
        .spans
        .iter()
        .filter(|span| !span.styles.is_empty())
        .map(|span| (spanned(&paragraph.text, span), styles(span)))
        .collect();
    assert_eq!(
        styled,
        vec![
            ("café".to_owned(), vec![pb::SpanStyle::Bold]),
            ("prêt".to_owned(), vec![pb::SpanStyle::Italic]),
            ("froid".to_owned(), vec![pb::SpanStyle::Strikethrough]),
        ],
        "the accented text does not shift the code point ranges"
    );

    let second = text_blocks(&events)
        .into_iter()
        .find(|block| block.text.starts_with("Water"))
        .expect("the second paragraph");
    let of = |wanted: pb::SpanStyle| -> Vec<String> {
        second
            .spans
            .iter()
            .filter(|span| styles(span).contains(&wanted))
            .map(|span| spanned(&second.text, span))
            .collect()
    };
    assert_eq!(of(pb::SpanStyle::Subscript), vec!["2"]);
    assert_eq!(of(pb::SpanStyle::Superscript), vec!["2"]);
    assert_eq!(of(pb::SpanStyle::Mark), vec!["marked"]);
    assert_eq!(of(pb::SpanStyle::Insertion), vec!["added"]);
    assert_eq!(of(pb::SpanStyle::Strikethrough), vec!["removed"]);
    assert_eq!(of(pb::SpanStyle::Small), vec!["quiet"]);
    let abbr = second
        .spans
        .iter()
        .find(|span| styles(span).contains(&pb::SpanStyle::Abbreviation))
        .expect("an abbreviation run");
    assert_eq!(
        abbr.raw.as_deref(),
        Some("HyperText Markup Language"),
        "the expansion the visible text does not carry"
    );
    let time = second
        .spans
        .iter()
        .find(|span| spanned(&second.text, span) == "October")
        .expect("a time run");
    assert_eq!(time.raw.as_deref(), Some("1843-10-01"));
}

#[tokio::test]
async fn in_prose_links_and_ids_survive_with_fragments_typed() {
    let client = client().await;
    let events = parse_ok(&client, INLINE, options(pb::MarkupFormat::Html)).await;
    let heading = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert_eq!(texts(&heading), ["La méthode"]);
    assert_eq!(
        heading[0].anchor_id.as_deref(),
        Some("méthode"),
        "an id is an anchor an intra-document link can resolve to"
    );

    let paragraph = text_blocks(&events)
        .into_iter()
        .find(|block| block.text.starts_with("Le "))
        .expect("the lede");
    assert_eq!(paragraph.anchor_id.as_deref(), Some("lede"));
    assert!(paragraph.hyperlink.is_none(), "three links is not one link");
    let links: Vec<(String, String)> = paragraph
        .spans
        .iter()
        .filter_map(|span| Some((spanned(&paragraph.text, span), span.hyperlink.clone()?)))
        .collect();
    assert_eq!(
        links,
        vec![
            ("Ada".to_owned(), "https://ada.example".to_owned()),
            ("Grace".to_owned(), "https://grace.example".to_owned()),
            ("plus bas".to_owned(), "#méthode".to_owned()),
        ]
    );
    let fragment = paragraph
        .spans
        .iter()
        .find(|span| span.hyperlink.as_deref() == Some("#méthode"))
        .expect("the fragment link");
    assert_eq!(fragment.reference.as_deref(), Some("méthode"));
    assert_eq!(
        fragment.reference_kind,
        pb::ReferenceKind::CrossRef as i32,
        "a same-document link is a cross reference, not only a URL"
    );
    let titled = paragraph
        .spans
        .iter()
        .find(|span| span.hyperlink.as_deref() == Some("https://ada.example"))
        .expect("the first link");
    assert_eq!(titled.link_title.as_deref(), Some("Her page"));
}

#[tokio::test]
async fn colgroup_geometry_and_cell_alignment_reach_the_wire() {
    let client = client().await;
    let events = parse_ok(&client, INLINE, options(pb::MarkupFormat::Html)).await;
    let start = table_starts(&events)[0];
    assert_eq!(
        start
            .columns
            .iter()
            .map(|column| column.align)
            .collect::<Vec<_>>(),
        vec![
            pb::Alignment::Left as i32,
            pb::Alignment::Center as i32,
            pb::Alignment::Right as i32,
            pb::Alignment::Right as i32,
        ],
        "a col span repeats its declaration across the columns it covers"
    );
    assert_eq!(start.columns[1].width.as_deref(), Some("40%"));

    let row = table_rows(&events)[0];
    assert_eq!(row.cells[1].valign, pb::Alignment::Top as i32);
    assert_eq!(row.cells[1].text, "b c");
    assert_eq!(
        styles(&row.cells[1].spans[0]),
        vec![pb::SpanStyle::Bold],
        "a cell's own emphasis is no longer flattened away"
    );
    assert_eq!(
        row.cells[1].spans[1].hyperlink.as_deref(),
        Some("https://c.example")
    );
}

#[tokio::test]
async fn open_graph_meta_tags_are_no_longer_skipped() {
    let client = client().await;
    let document = r#"<html><head>
      <meta property="og:title" content="The Engine">
      <meta name="author" content="Ada">
    </head><body><p>x</p></body></html>"#;
    let events = parse_ok(&client, document, options(pb::MarkupFormat::Html)).await;
    let metadata = &info(&events).metadata;
    assert_eq!(
        meta_scalar(&metadata["meta.og:title"]),
        "The Engine",
        "a property-named meta tag is metadata too"
    );
    assert_eq!(meta_scalar(&metadata["meta.author"]), "Ada");
}

#[tokio::test]
async fn an_image_carries_the_dimensions_the_document_asks_for() {
    let client = client().await;
    let html = r#"<body>
<img src="schéma.png" alt="schéma" width="640" height="480">
<img src="bandeau.png" alt="bandeau" width="50%">
</body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images[0].width.as_deref(), Some("640"));
    assert_eq!(images[0].height.as_deref(), Some("480"));
    assert_eq!(
        images[1].width.as_deref(),
        Some("50%"),
        "a percentage rides verbatim; nothing is converted"
    );
    assert!(images[1].height.is_none());
}

#[tokio::test]
async fn a_br_is_a_line_break_and_not_a_swallowed_word_boundary() {
    let client = client().await;
    let html = "<body><p>Première ligne<br>seconde ligne</p>\
<table><tr><td>haut<br>bas</td></tr></table></body>";
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["Première ligne\nseconde ligne"]
    );
    assert_eq!(
        table_rows(&events)[0].cells[0].text,
        "haut\nbas",
        "a cell flattens through the same path"
    );
}

/// Project Gutenberg EPUBs space their pages with `<p><br/><br/></p>`: line
/// breaks and nothing else. Such a paragraph, or a cell like it, is no text,
/// not a block of newlines.
#[tokio::test]
async fn a_paragraph_of_line_breaks_alone_is_not_a_block() {
    let client = client().await;
    let html = "<body><p><br/>\n<br/></p><div class=\"c3\"><br/>\n<br/>\n<br/></div>\
<p>Kept</p><table><tr><td><br/></td><td>x</td></tr></table></body>";
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["Kept"]
    );
    assert_eq!(
        table_rows(&events)[0].cells[0].text,
        "",
        "a spacer cell is empty"
    );
}

/// The cover page real EPUBs ship: an XHTML 1.1 document, the XLink
/// namespace declared, the image wrapped in an inline `<svg>` inside a
/// cover `div`, and the reference spelled `xlink:href` only.
const XHTML_COVER: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:xlink="http://www.w3.org/1999/xlink">
<head><title>Cover</title></head>
<body>
  <div class="cover">
    <svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"
         version="1.1" width="100%" height="100%" viewBox="0 0 1200 1800" preserveAspectRatio="xMidYMid meet">
      <image width="1200" height="1800" xlink:href="../Images/cover.jpg"/>
    </svg>
  </div>
</body>
</html>
"#;

#[tokio::test]
async fn the_nested_xhtml_cover_yields_one_image_from_the_xlink_href() {
    let client = client().await;
    let events = parse_ok(&client, XHTML_COVER, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 1);
    assert_eq!(
        images[0].src, "../Images/cover.jpg",
        "the reference is the attribute verbatim, exactly as an <img src> would be"
    );
    assert_eq!(images[0].alt, "");
    assert_eq!(images[0].width.as_deref(), Some("1200"));
    assert_eq!(images[0].height.as_deref(), Some("1800"));
    assert!(text_blocks(&events).is_empty(), "a cover page has no prose");
    assert!(
        warning(&events, pb::WarningCode::UnmappedConstruct).is_none(),
        "svg is mapped, not flattened"
    );
}

#[tokio::test]
async fn an_svg_yields_one_image_per_image_element_named_by_its_text() {
    let client = client().await;
    let html = r#"<body>
<svg id="plates" viewBox="0 0 10 10">
  <title>Two plates</title>
  <desc>Side by side</desc>
  <g><image href="left.png" xlink:href="stale.png" width="5" height="10"/></g>
  <image id="right" xlink:href="right.png"><title>The right plate</title></image>
</svg>
</body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].src, "left.png", "href wins over xlink:href");
    assert_eq!(
        images[0].alt, "Two plates",
        "the svg's title names an unnamed image"
    );
    assert_eq!(images[0].title, "Side by side");
    assert_eq!(images[0].width.as_deref(), Some("5"));
    assert_eq!(images[0].anchor_id.as_deref(), Some("plates"));
    assert_eq!(images[1].src, "right.png");
    assert_eq!(
        images[1].alt, "The right plate",
        "an image's own title wins"
    );
    assert_eq!(images[1].anchor_id.as_deref(), Some("right"));
}

#[tokio::test]
async fn decorative_svgs_yield_nothing_and_no_warning() {
    let client = client().await;
    let html = r#"<body>
<p>Before <svg aria-hidden="true"><image href="hidden.png"/></svg> after.</p>
<svg role="presentation"><image href="chrome.png"/></svg>
<svg width="16" height="16"><path d="M0 0h16v16z"/></svg>
<svg viewBox="0 0 1 1"></svg>
</body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    assert!(images(&events).is_empty(), "{:?}", images(&events));
    assert_eq!(
        texts(&text_blocks(&events)),
        ["Before after."],
        "a decorative svg does not split the paragraph around it"
    );
    assert!(warning(&events, pb::WarningCode::UnmappedConstruct).is_none());
}

#[tokio::test]
async fn an_svg_drawing_rides_as_a_data_uri_under_the_cap() {
    let client = client().await;
    let html = r#"<body><svg width="200" height="100"><title>A bar</title><rect width="200" height="100"/></svg></body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 1);
    assert!(
        images[0].src.starts_with("data:image/svg+xml;base64,"),
        "{}",
        images[0].src
    );
    assert_eq!(images[0].alt, "A bar");
    assert_eq!(images[0].width.as_deref(), Some("200"));
}

#[tokio::test]
async fn a_picture_element_yields_exactly_its_img_fallback() {
    let client = client().await;
    let html = r#"<body><picture>
<source srcset="hero.avif" type="image/avif">
<source srcset="hero.webp 1x, hero@2x.webp 2x" type="image/webp">
<img src="hero.jpg" alt="Hero">
</picture></body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 1, "never one per <source>");
    assert_eq!(images[0].src, "hero.jpg");
    assert_eq!(images[0].alt, "Hero");
}

#[tokio::test]
async fn an_img_with_only_a_srcset_takes_the_first_candidate() {
    let client = client().await;
    let html = r#"<body>
<img srcset="small.jpg 480w, large.jpg 1200w" alt="Responsive">
<img src="plain.jpg" srcset="other.jpg 2x" alt="Plain">
</body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].src, "small.jpg");
    assert_eq!(
        images[1].src, "plain.jpg",
        "a src is never displaced by a srcset"
    );
}

#[tokio::test]
async fn objects_and_embeds_of_image_type_yield_pictures() {
    let client = client().await;
    let html = r#"<body>
<object data="diagram.svg" type="image/svg+xml" width="400">The flow, as a diagram</object>
<embed src="scan.png" type="image/png" title="Page scan">
<object data="movie.swf" type="application/x-shockwave-flash">Flash fallback</object>
<embed src="clip.mp4" type="video/mp4">
</body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 2, "{images:?}");
    assert_eq!(images[0].src, "diagram.svg");
    assert_eq!(images[0].alt, "The flow, as a diagram");
    assert_eq!(images[0].width.as_deref(), Some("400"));
    assert_eq!(images[1].src, "scan.png");
    assert_eq!(images[1].title, "Page scan");
    assert!(
        text_blocks(&events).is_empty(),
        "a non-image object is still dropped whole: {:?}",
        texts(&text_blocks(&events))
    );
}

#[tokio::test]
async fn a_figure_wrapping_an_svg_image_yields_the_image_then_its_caption() {
    let client = client().await;
    let html = r#"<body><figure>
<svg viewBox="0 0 1 1"><image xlink:href="plate.jpg"/></svg>
<figcaption>Plate II.</figcaption>
</figure></body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].src, "plate.jpg");
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Caption)),
        ["Plate II."]
    );
    let caption_index = blocks_labelled(&events, pb::BlockLabel::Caption)[0].index;
    assert!(images[0].index < caption_index, "the image comes first");
}

#[tokio::test]
async fn a_leading_bom_parses_the_same_as_none() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "\u{feff}<h1>Title</h1><p>body</p>",
        options(pb::MarkupFormat::Html),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Title)),
        ["Title"]
    );
}

// --------------------------------------------------- picture link targets

/// A figure whose image is wrapped in a link and whose caption holds one:
/// the two places upstream docling sources a picture's caption link from.
const LINKED_FIGURE: &str = r#"<body>
<figure>
<a href="https://example.com/plate"><img src="plate.png" alt="The plate"></a>
<figcaption>Photo by <a href="https://ada.example">Ada</a>, with <a href="https://bb.example">Babbage</a>.</figcaption>
</figure>
</body>"#;

#[tokio::test]
async fn a_link_wrapping_only_an_image_lends_the_picture_its_href() {
    let client = client().await;
    let html = r#"<body>
<a href="https://example.com/plate"><img src="plate.png" alt="The plate"></a>
<a href="https://example.com/figure"><img src="fig.png" alt="The figure"> and its text</a>
</body>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(images.len(), 2);
    assert_eq!(
        images[0].hyperlink.as_deref(),
        Some("https://example.com/plate"),
        "a link whose only content is the image belongs to the picture"
    );
    assert!(
        images[1].hyperlink.is_none(),
        "text beside the image keeps the link a prose link, not the picture's"
    );
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["and its text"],
        "the text beside the image is still the prose it always was"
    );
}

#[tokio::test]
async fn a_figcaption_names_the_first_link_it_holds() {
    let client = client().await;
    let events = parse_ok(&client, LINKED_FIGURE, options(pb::MarkupFormat::Html)).await;
    let images = images(&events);
    assert_eq!(
        images[0].hyperlink.as_deref(),
        Some("https://example.com/plate"),
        "the link wrapping the figure's image"
    );
    let captions = blocks_labelled(&events, pb::BlockLabel::Caption);
    assert_eq!(captions.len(), 1);
    assert_eq!(
        captions[0].text, "Photo by Ada, with Babbage.",
        "the anchor texts stay inline in the caption"
    );
    assert_eq!(
        captions[0].hyperlink.as_deref(),
        Some("https://ada.example"),
        "the caption carries the first link it holds, though it is not one link"
    );
    let links: Vec<&str> = captions[0]
        .spans
        .iter()
        .filter_map(|span| span.hyperlink.as_deref())
        .collect();
    assert_eq!(links, ["https://ada.example", "https://bb.example"]);
}

#[tokio::test]
async fn the_picture_and_caption_items_carry_the_links_into_the_document() {
    let client = client().await;
    let events = parse_ok(
        &client,
        LINKED_FIGURE,
        pb::ParseOptions {
            format: pb::MarkupFormat::Html as i32,
            emit_document: true,
            ..pb::ParseOptions::default()
        },
    )
    .await;
    let document = document(&events).expect("the fold was requested");
    let picture = &document.pictures[0];
    assert_eq!(
        picture.hyperlink.as_deref(),
        Some("https://example.com/plate"),
        "the PictureItem keeps the href of the link that wrapped the image"
    );
    assert_eq!(picture.captions[0].r#ref, "#/texts/0");
    let caption = &document.texts[0];
    let Some(grpc_markup::document::v1::base_text_item::Item::Text(caption)) =
        caption.item.as_ref()
    else {
        panic!("the caption is a text item");
    };
    assert_eq!(
        caption.base.as_ref().unwrap().hyperlink.as_deref(),
        Some("https://ada.example"),
        "the caption item carries the first link the figcaption held"
    );
}

// ------------------------------------------------------------- rich cells

/// The block of one cell block's text arm, for the assertions below.
fn cell_text(block: &pb::CellBlock) -> &pb::TextBlock {
    match block.kind.as_ref() {
        Some(pb::cell_block::Kind::Text(text)) => text,
        other => panic!("expected a text block, got {other:?}"),
    }
}

#[tokio::test]
async fn a_nested_table_rides_on_its_cell_as_blocks() {
    let client = client().await;
    let html = "<table><tr><td>plain</td>\
                <td><table><tr><td>in</td><td>ner</td></tr></table></td></tr></table>";
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let cells = &table_rows(&events)[0].cells;
    assert!(cells[0].blocks.is_empty(), "a plain cell carries no blocks");
    assert_eq!(
        cells[1].text, "in ner",
        "the flat walk is untouched: the nested table still flattens"
    );
    assert_eq!(cells[1].blocks.len(), 1);
    let Some(pb::cell_block::Kind::Table(nested)) = cells[1].blocks[0].kind.as_ref() else {
        panic!("the block is the nested table");
    };
    assert_eq!(nested.rows.len(), 1);
    assert_eq!(nested.rows[0].cells[0].text, "in");
    assert_eq!(nested.rows[0].cells[1].text, "ner");
    assert!(
        nested.rows[0]
            .cells
            .iter()
            .all(|cell| cell.blocks.is_empty()),
        "the nested cells are plain"
    );
}

#[tokio::test]
async fn a_list_in_a_cell_keeps_its_items_as_blocks() {
    let client = client().await;
    let html = "<table><tr><td><ul><li>one</li><li>two</li></ul></td></tr></table>";
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let cell = &table_rows(&events)[0].cells[0];
    assert_eq!(cell.text, "one two", "the flat text is the flattened list");
    let items: Vec<&pb::TextBlock> = cell.blocks.iter().map(cell_text).collect();
    assert_eq!(items.len(), 2);
    for (item, text) in items.iter().zip(["one", "two"]) {
        assert_eq!(item.label, pb::BlockLabel::ListItem as i32);
        assert_eq!(item.text, text);
        assert_eq!(item.list_depth, Some(1));
        assert!(!item.enumerated);
    }
}

#[tokio::test]
async fn a_heading_in_a_cell_stays_a_section_header_block() {
    let client = client().await;
    let html = "<table><tr><td><h3>Sub</h3>trailing</td></tr></table>";
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let cell = &table_rows(&events)[0].cells[0];
    assert_eq!(cell.text, "Sub trailing");
    assert_eq!(cell.blocks.len(), 2);
    let heading = cell_text(&cell.blocks[0]);
    assert_eq!(heading.label, pb::BlockLabel::SectionHeader as i32);
    assert_eq!(heading.text, "Sub");
    assert_eq!(heading.level, Some(3));
    let prose = cell_text(&cell.blocks[1]);
    assert_eq!(prose.label, pb::BlockLabel::Paragraph as i32);
    assert_eq!(prose.text, "trailing");
}

#[tokio::test]
async fn underline_formatting_makes_a_cell_rich_and_plain_cells_stay_flat() {
    let client = client().await;
    let html = "<table><tr><td>a <u>signed</u> note</td><td>plain</td></tr></table>";
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let cells = &table_rows(&events)[0].cells;
    assert!(
        cells[1].blocks.is_empty(),
        "uniform prose is not rich, so its cell stays block-free"
    );
    let block = cell_text(&cells[0].blocks[0]);
    assert_eq!(block.text, "a signed note");
    let underlined = block
        .spans
        .iter()
        .find(|span| styles(span).contains(&pb::SpanStyle::Underline))
        .expect("the underline run survives in the block");
    assert_eq!(spanned(&block.text, underlined), "signed");
}

#[tokio::test]
async fn a_pre_with_inline_structure_keeps_code_styled_runs() {
    let client = client().await;
    let html = r#"<table><tr><td><pre>See <a href="https://docs.example">the docs</a> first</pre></td></tr></table>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let cell = &table_rows(&events)[0].cells[0];
    let block = cell_text(&cell.blocks[0]);
    assert_eq!(block.label, pb::BlockLabel::Code as i32);
    assert_eq!(block.text, "See the docs first");
    let code = block
        .spans
        .iter()
        .find(|span| styles(span).contains(&pb::SpanStyle::Code))
        .expect("the whole block is code-styled");
    assert_eq!(spanned(&block.text, code), "See the docs first");
    let link = block
        .spans
        .iter()
        .find(|span| span.hyperlink.is_some())
        .expect("the link inside survives as a run, not a backticked fragment");
    assert_eq!(spanned(&block.text, link), "the docs");
    assert_eq!(link.hyperlink.as_deref(), Some("https://docs.example"));
}

#[tokio::test]
async fn a_spanning_rich_cell_keeps_its_blocks_on_the_anchor_cell_only() {
    let client = client().await;
    let html = r#"<table><tr><td rowspan="2"><ul><li>x</li></ul></td><td>b</td></tr><tr><td>c</td></tr></table>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let rows = table_rows(&events);
    assert_eq!(rows[0].cells[0].row_span, 2);
    assert_eq!(rows[0].cells[0].blocks.len(), 1, "the anchor cell is rich");
    assert!(rows[0].cells[1].blocks.is_empty());
    assert_eq!(rows[1].cells.len(), 1, "the covered position is not a cell");
    assert_eq!(rows[1].cells[0].column_index, 1);
    assert_eq!(rows[1].cells[0].text, "c");
    assert!(
        rows[1].cells[0].blocks.is_empty(),
        "a covered position carries no blocks"
    );
}

#[tokio::test]
async fn a_plain_table_carries_no_blocks() {
    let client = client().await;
    let html =
        r#"<table><tr><td>a</td><td><b>b</b> <a href="https://c.example">c</a></td></tr></table>"#;
    let events = parse_ok(&client, html, options(pb::MarkupFormat::Html)).await;
    let cells = &table_rows(&events)[0].cells;
    assert!(
        cells.iter().all(|cell| cell.blocks.is_empty()),
        "emphasis and links are runs, not structure: the cells stay plain"
    );
    assert_eq!(cells[1].text, "b c");
    assert_eq!(
        cells[1].spans[1].hyperlink.as_deref(),
        Some("https://c.example")
    );
}

#[tokio::test]
async fn a_rich_cell_points_at_a_group_holding_its_structure() {
    let client = client().await;
    let html = "<table><tr><td><ul><li>one</li><li>two</li></ul></td></tr></table>";
    let events = parse_ok(
        &client,
        html,
        pb::ParseOptions {
            format: pb::MarkupFormat::Html as i32,
            emit_document: true,
            ..pb::ParseOptions::default()
        },
    )
    .await;
    let document = document(&events).expect("the fold was requested");
    assert!(
        grpc_markup::document_fold::integrity_errors(document).is_empty(),
        "the fragment stays merge-safe"
    );
    let table = &document.tables[0];
    let cell = &table.data.as_ref().unwrap().grid[0].cells[0];
    let cell_ref = &cell.r#ref.as_ref().expect("the rich cell points").r#ref;
    assert_eq!(cell_ref, "#/groups/0");
    assert_eq!(
        table.data.as_ref().unwrap().table_cells[0]
            .r#ref
            .as_ref()
            .unwrap()
            .r#ref,
        "#/groups/0",
        "both shapes of the cell name the same group"
    );
    let group = &document.groups[0];
    assert_eq!(group.parent.as_ref().unwrap().r#ref, "#/tables/0");
    assert_eq!(group.label_raw.as_deref(), Some("rich_cell"));
    assert_eq!(group.children.len(), 2, "the two list items");
    for child in &group.children {
        let item = &document.texts[child
            .r#ref
            .strip_prefix("#/texts/")
            .unwrap()
            .parse::<usize>()
            .unwrap()];
        let base = match item.item.as_ref() {
            Some(grpc_markup::document::v1::base_text_item::Item::ListItem(item)) => {
                item.base.as_ref().unwrap()
            }
            other => panic!("the cell's items are list items, got {other:?}"),
        };
        assert_eq!(
            base.label,
            grpc_markup::document::v1::DocItemLabel::ListItem as i32
        );
        assert_eq!(base.parent.as_ref().unwrap().r#ref, "#/groups/0");
    }
}

#[tokio::test]
async fn a_flat_stream_request_strips_the_cell_block_runs_too() {
    let client = client().await;
    let html = "<table><tr><td>a <u>signed</u> note</td></tr></table>";
    let events = parse_ok(
        &client,
        html,
        pb::ParseOptions {
            format: pb::MarkupFormat::Html as i32,
            emit_inline_spans: Some(false),
            ..pb::ParseOptions::default()
        },
    )
    .await;
    let cell = &table_rows(&events)[0].cells[0];
    assert!(cell.spans.is_empty());
    let block = cell_text(&cell.blocks[0]);
    assert_eq!(block.text, "a signed note", "the text is still the text");
    assert!(
        block.spans.is_empty(),
        "the runs inside a cell block follow the same option"
    );
}
