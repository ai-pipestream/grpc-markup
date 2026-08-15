// SPDX-License-Identifier: Apache-2.0

//! The Document projection over the wire: placement of the `document`
//! event, per-format fold content, and the merge-safety contract
//! (`integrity_errors` empty) for every fragment a caller can receive.

mod common;

use common::{BOXNOTE, DOCLING, client, document, kind, parse_ok};
use grpc_markup::document::v1 as doc;
use grpc_markup::document_fold::integrity_errors;
use grpc_markup::proto::v1 as pb;

/// Options asking for the Document projection of one format.
fn with_document(format: pb::MarkupFormat) -> pb::ParseOptions {
    pb::ParseOptions {
        format: format as i32,
        emit_document: true,
        ..pb::ParseOptions::default()
    }
}

/// The Markdown fixture the fold tests read; small enough that every
/// assertion can name its item.
const MARKDOWN: &str = "\
# The Title

Intro paragraph.

## Section

- one
- two
  1. deep

```rust
fn x() {}
```

| H |
| - |
| c |

![alt text](img.png)
";

#[tokio::test]
async fn without_the_option_no_document_event_is_sent() {
    let client = client().await;
    let events = parse_ok(
        &client,
        MARKDOWN,
        pb::ParseOptions {
            format: pb::MarkupFormat::Markdown as i32,
            ..pb::ParseOptions::default()
        },
    )
    .await;
    assert!(document(&events).is_none(), "the fold is opt-in");
}

#[tokio::test]
async fn the_document_event_arrives_exactly_once_immediately_before_status() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, with_document(pb::MarkupFormat::Markdown)).await;
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "document").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(kinds[kinds.len() - 2], "document", "{kinds:?}");
    assert_eq!(kinds[kinds.len() - 1], "status", "{kinds:?}");
}

#[tokio::test]
async fn the_markdown_fold_is_a_merge_safe_fragment_with_docling_structure() {
    let client = client().await;
    let events = parse_ok(&client, MARKDOWN, with_document(pb::MarkupFormat::Markdown)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(integrity_errors(document), Vec::<String>::new());

    assert_eq!(document.name, "The Title", "named after its title item");

    // The heading ladder: the section header parents what follows it.
    let Some(doc::base_text_item::Item::SectionHeader(section)) = document.texts[2].item.as_ref()
    else {
        panic!(
            "texts[2] is the section header, got {:?}",
            document.texts[2]
        );
    };
    assert_eq!(section.level, 2);
    assert!(
        !section.base.as_ref().unwrap().children.is_empty(),
        "content after the heading hangs off it"
    );

    // One list group per contiguous list, nested groups for nested lists.
    assert_eq!(document.groups.len(), 2);
    assert_eq!(document.groups[0].label, doc::GroupLabel::List as i32);
    assert_eq!(
        document.groups[1].label,
        doc::GroupLabel::OrderedList as i32
    );
    assert_eq!(
        document.groups[1].parent.as_ref().unwrap().r#ref,
        "#/groups/0"
    );

    // The code item carries the enum language; the table has its grid; the
    // picture points at the source's own reference.
    let code = document
        .texts
        .iter()
        .find_map(|item| match item.item.as_ref() {
            Some(doc::base_text_item::Item::Code(code)) => Some(code),
            _ => None,
        })
        .expect("a code item");
    assert_eq!(
        code.code_language,
        Some(doc::CodeLanguageLabel::Rust as i32)
    );

    let table = &document.tables[0];
    let data = table.data.as_ref().unwrap();
    assert_eq!(data.num_rows, 2);
    assert_eq!(data.num_cols, 1);
    assert!(data.grid[0].cells[0].column_header);
    assert_eq!(data.grid[1].cells[0].text, "c");

    assert_eq!(document.pictures[0].image.as_ref().unwrap().uri, "img.png");

    // Every item is attributed to this collector and fold.
    let Some(doc::base_text_item::Item::Title(title)) = document.texts[0].item.as_ref() else {
        panic!("texts[0] is the title");
    };
    let source = &title.base.as_ref().unwrap().source[0];
    let Some(doc::source_type::Source::Collector(collector)) = source.source.as_ref() else {
        panic!("collector provenance");
    };
    assert_eq!(collector.collector, "markup");
    assert_eq!(collector.model.as_deref(), Some("markdown"));
    assert_eq!(
        collector.version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(collector.confidence.is_none());
}

#[tokio::test]
async fn the_html_fold_is_a_merge_safe_fragment_with_docling_structure() {
    let client = client().await;
    let html = "<html><head><title>Named</title><meta name=\"author\" content=\"Ada\"></head>\
                <body><h1>The Title</h1><h2>Section</h2><p>under it</p>\
                <ul><li>one</li><li>two</li></ul>\
                <pre class=\"language-rust\">fn x() {}</pre>\
                <img src=\"img.png\" alt=\"a\"></body></html>";
    let events = parse_ok(&client, html, with_document(pb::MarkupFormat::Html)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(integrity_errors(document), Vec::<String>::new());
    assert_eq!(
        document.name, "Named",
        "MarkupInfo.title names the document"
    );
    assert_eq!(document.origin.as_ref().unwrap().mimetype, "text/html");
    let meta = document.body.as_ref().unwrap().meta.as_ref().unwrap();
    assert_eq!(
        meta.custom_fields["html.meta.author"].kind,
        Some(prost_types::value::Kind::StringValue("Ada".to_owned()))
    );
    // The heading ladder and list group survive the fold.
    let Some(doc::base_text_item::Item::SectionHeader(section)) = document.texts[1].item.as_ref()
    else {
        panic!("texts[1] is the section header");
    };
    assert!(!section.base.as_ref().unwrap().children.is_empty());
    assert_eq!(document.groups.len(), 1);
    assert_eq!(document.groups[0].label, doc::GroupLabel::List as i32);
    let code = document
        .texts
        .iter()
        .find_map(|item| match item.item.as_ref() {
            Some(doc::base_text_item::Item::Code(code)) => Some(code),
            _ => None,
        })
        .expect("a code item");
    assert_eq!(
        code.code_language,
        Some(doc::CodeLanguageLabel::Rust as i32)
    );
    assert_eq!(document.pictures[0].image.as_ref().unwrap().uri, "img.png");
}

#[tokio::test]
async fn the_asciidoc_fold_is_a_merge_safe_fragment() {
    let client = client().await;
    let adoc = ":author: Ada\n\n= The Title\n\n== Section\n\nunder it\n\n* one\n** two\n\n[source,rust]\n----\nfn x() {}\n----\n\nimage::img.png[a]\n";
    let events = parse_ok(&client, adoc, with_document(pb::MarkupFormat::Asciidoc)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(integrity_errors(document), Vec::<String>::new());
    assert_eq!(document.name, "The Title", "named after its title item");
    assert_eq!(document.origin.as_ref().unwrap().mimetype, "text/asciidoc");
    let meta = document.body.as_ref().unwrap().meta.as_ref().unwrap();
    assert_eq!(
        meta.custom_fields["asciidoc.author"].kind,
        Some(prost_types::value::Kind::StringValue("Ada".to_owned()))
    );
    assert_eq!(document.groups.len(), 2, "outer list and its nested list");
    assert_eq!(
        document.groups[1].parent.as_ref().unwrap().r#ref,
        "#/groups/0"
    );
    let code = document
        .texts
        .iter()
        .find_map(|item| match item.item.as_ref() {
            Some(doc::base_text_item::Item::Code(code)) => Some(code),
            _ => None,
        })
        .expect("a code item");
    assert_eq!(
        code.code_language,
        Some(doc::CodeLanguageLabel::Rust as i32)
    );
    assert_eq!(document.pictures[0].image.as_ref().unwrap().uri, "img.png");
}

#[tokio::test]
async fn the_latex_fold_is_a_merge_safe_fragment() {
    let client = client().await;
    let latex = "\\title{The Title}\\author{Ada}\n\\begin{document}\n\\section{Section}\n\nunder it\n\n\\begin{itemize}\\item one\\item two\\end{itemize}\n\n\\begin{equation}E=mc^2\\end{equation}\n\n\\includegraphics{img.png}\n\\end{document}\n";
    let events = parse_ok(&client, latex, with_document(pb::MarkupFormat::Latex)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(integrity_errors(document), Vec::<String>::new());
    assert_eq!(document.name, "The Title");
    assert_eq!(
        document.origin.as_ref().unwrap().mimetype,
        "application/x-latex"
    );
    let Some(doc::base_text_item::Item::Title(_)) = document.texts[0].item.as_ref() else {
        panic!("texts[0] is the title");
    };
    assert_eq!(document.groups.len(), 1, "one list group");
    assert!(
        document.texts.iter().any(|item| matches!(
            item.item.as_ref(),
            Some(doc::base_text_item::Item::Formula(_))
        )),
        "the equation folds as a FormulaItem"
    );
    assert_eq!(document.pictures[0].image.as_ref().unwrap().uri, "img.png");
}

#[tokio::test]
async fn the_vtt_fold_carries_cue_timings_as_track_provenance() {
    let client = client().await;
    let vtt = "WEBVTT\n\nintro\n00:00:01.000 --> 00:00:02.500\n<v Ada>Hello\n\n00:03.000 --> 00:04.000\nSecond\n";
    let events = parse_ok(&client, vtt, with_document(pb::MarkupFormat::Vtt)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(integrity_errors(document), Vec::<String>::new());
    assert_eq!(document.texts.len(), 2);

    let Some(doc::base_text_item::Item::Text(first)) = document.texts[0].item.as_ref() else {
        panic!("cues fold as TextItem");
    };
    let base = first.base.as_ref().unwrap();
    assert_eq!(base.text, "Hello");
    let Some(doc::source_type::Source::Track(track)) = base.source[0].source.as_ref() else {
        panic!("the first source is the track");
    };
    assert!((track.start_time - 1.0).abs() < f64::EPSILON);
    assert!((track.end_time - 2.5).abs() < f64::EPSILON);
    assert_eq!(track.identifier.as_deref(), Some("intro"));
    assert_eq!(track.voice.as_deref(), Some("Ada"));
    assert!(
        matches!(
            base.source[1].source.as_ref(),
            Some(doc::source_type::Source::Collector(_))
        ),
        "the collector stamp rides alongside the track"
    );
}

#[tokio::test]
async fn the_boxnote_fold_is_a_merge_safe_fragment() {
    let client = client().await;
    let events = parse_ok(&client, BOXNOTE, with_document(pb::MarkupFormat::Boxnote)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(integrity_errors(document), Vec::<String>::new());
    assert_eq!(document.name, "Meeting Notes");
    assert_eq!(
        document.origin.as_ref().unwrap().mimetype,
        "application/vnd.box.boxnote"
    );
    assert!(
        document
            .groups
            .iter()
            .any(|group| group.label == doc::GroupLabel::OrderedList as i32),
        "the nested ordered list became a group"
    );
    assert_eq!(document.tables.len(), 1);
    assert_eq!(document.pictures.len(), 1);
}

#[tokio::test]
async fn the_docling_document_is_carried_over_losslessly_and_stamped() {
    let client = client().await;
    let events = parse_ok(
        &client,
        DOCLING,
        with_document(pb::MarkupFormat::DoclingJson),
    )
    .await;
    let document = document(&events).expect("the fold was requested");

    // The ingested arenas survive intact, wiring included.
    assert_eq!(integrity_errors(document), Vec::<String>::new());
    assert_eq!(document.name, "Quarterly Review");
    assert_eq!(document.schema_name.as_deref(), Some("DoclingDocument"));
    assert_eq!(document.version.as_deref(), Some("1.3.0"));
    let origin = document.origin.as_ref().unwrap();
    assert_eq!(origin.mimetype, "application/pdf");
    assert_eq!(origin.binary_hash, 42);
    assert_eq!(origin.filename, "q.pdf");

    // Page geometry and provenance — facts this collector could never
    // invent — pass through.
    let page = &document.pages[&1];
    assert!((page.size.as_ref().unwrap().width - 612.0).abs() < f64::EPSILON);
    let Some(doc::base_text_item::Item::Title(title)) = document.texts[0].item.as_ref() else {
        panic!("texts[0] is the title");
    };
    let base = title.base.as_ref().unwrap();
    let prov = &base.prov[0];
    assert_eq!(prov.page_no, 1);
    let bbox = prov.bbox.as_ref().unwrap();
    assert!((bbox.r - 100.0).abs() < f64::EPSILON);
    assert_eq!(bbox.coord_origin, Some(doc::CoordOrigin::Topleft as i32));
    assert_eq!(
        prov.charspan.as_ref().map(|span| (span.start, span.end)),
        Some((0, 16))
    );

    // The picture keeps its real ImageRef.
    let image = document.pictures[0].image.as_ref().unwrap();
    assert_eq!(image.uri, "img/chart.png");
    assert_eq!(image.mimetype, "image/png");
    assert_eq!(image.dpi, 72);

    // And every item gained this collector's stamp, additively.
    let Some(doc::source_type::Source::Collector(stamp)) =
        base.source.last().and_then(|s| s.source.as_ref())
    else {
        panic!("the ingested item is stamped");
    };
    assert_eq!(stamp.collector, "markup");
    assert_eq!(stamp.model.as_deref(), Some("docling-json"));
    assert!(
        document.tables[0].source.iter().any(|s| matches!(
            s.source.as_ref(),
            Some(doc::source_type::Source::Collector(c)) if c.collector == "markup"
        )),
        "tables are stamped too"
    );
}

#[tokio::test]
async fn info_metadata_lands_on_the_body_meta_under_the_format_prefix() {
    let client = client().await;
    let markdown = "---\ntitle: Named\nauthor: Ada\n---\nbody text\n";
    let events = parse_ok(&client, markdown, with_document(pb::MarkupFormat::Markdown)).await;
    let document = document(&events).expect("the fold was requested");
    assert_eq!(document.name, "Named");
    let meta = document.body.as_ref().unwrap().meta.as_ref().unwrap();
    assert_eq!(
        meta.custom_fields["markdown.author"].kind,
        Some(prost_types::value::Kind::StringValue("Ada".to_owned()))
    );
}

#[tokio::test]
async fn info_metadata_trees_land_as_the_same_tree_of_struct_values() {
    use prost_types::value::Kind;
    let client = client().await;
    let markdown = "---\ntitle: Named\ntags:\n  - streaming\n  - markup\nauthors:\n  - name: Ada\n  - name: Charles\n---\nbody text\n";
    let events = parse_ok(&client, markdown, with_document(pb::MarkupFormat::Markdown)).await;
    let document = document(&events).expect("the fold was requested");
    let meta = document.body.as_ref().unwrap().meta.as_ref().unwrap();
    let Some(Kind::ListValue(tags)) = meta.custom_fields["markdown.tags"].kind.as_ref() else {
        panic!("a list of scalars lands as a ListValue");
    };
    assert_eq!(
        tags.values[0].kind,
        Some(Kind::StringValue("streaming".to_owned()))
    );
    assert_eq!(
        tags.values[1].kind,
        Some(Kind::StringValue("markup".to_owned()))
    );
    let Some(Kind::ListValue(authors)) = meta.custom_fields["markdown.authors"].kind.as_ref()
    else {
        panic!("a list of maps lands as a ListValue");
    };
    let Some(Kind::StructValue(ada)) = authors.values[0].kind.as_ref() else {
        panic!("each map in the list lands as a Struct");
    };
    assert_eq!(
        ada.fields["name"].kind,
        Some(Kind::StringValue("Ada".to_owned())),
        "the leaf is the verbatim string, not a JSON encoding"
    );
}
