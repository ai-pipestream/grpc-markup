// SPDX-License-Identifier: Apache-2.0

//! The HTML fold: whole documents and body-only fragments, parsed with
//! html5ever (via `scraper`) and mapped onto the shared block vocabulary.
//! The mapping decides which elements become items and which flatten into
//! the enclosing text; rendering, form and provenance machinery are out of
//! scope by construction.
//!
//! Shape of the mapping, in DOM order:
//!
//! - `<title>` becomes `MarkupInfo.title` and `<meta>` pairs become scalar
//!   `MarkupInfo.metadata` values under `meta.*` keys, keyed by `name` or,
//!   for OpenGraph and its relatives, by `property`.
//! - `h1`..`h6` flatten their inline markup; the first `h1` is
//!   `BLOCK_LABEL_TITLE` (the same rule as the Markdown fold), every other
//!   heading is `BLOCK_LABEL_SECTION_HEADER` at its own level.
//! - Inline text between blocks buffers and flushes as one paragraph per
//!   block boundary, whitespace collapsed; a block that is entirely one
//!   `<a>` carries `hyperlink`, and every inline element leaves an
//!   `InlineSpan` over the run it covers: emphasis and its siblings as
//!   styles, every `<a href>` as a link (a `#fragment` also as a typed
//!   cross reference), an `<abbr title>`, `<time datetime>` or
//!   `<data value>` as the machine-readable value the text does not say.
//!   An `id` anywhere is an anchor, on the block or on the run.
//!   Paragraphs inside `<blockquote>` are `BLOCK_LABEL_QUOTE`, nesting
//!   flattened.
//! - `ul` / `ol` nest through `li`, `enumerated` for `ol`; the marker is
//!   written out only when the source numbered the list itself (an `ol
//!   start` attribute). `dl` maps `dt` / `dd` to list
//!   items with the proto's own `definition-term` role vocabulary.
//! - `table` streams with `thead` cells and `th` flagged as headers,
//!   `rowspan` / `colspan` honored through an occupancy grid, the
//!   `<caption>` element on `TableStart`, `<colgroup>` / `<col>` geometry
//!   as the table's column specs, and each cell's own `align` / `valign`.
//!   Nested block structure inside a cell still flattens to its text; its
//!   emphasis and its links do not.
//! - `pre` keeps its text verbatim as `BLOCK_LABEL_CODE` with a
//!   `language-*` class as the declared language.
//! - `img` becomes an `ImageBlock` (`src`, `alt`, `title` verbatim, never
//!   fetched); a `figure`'s `figcaption` follows as a `CAPTION` block.
//! - `details` descends and `summary` / `address` keep their name in
//!   `role`.
//! - `script`, `style`, `head` content and form controls never become
//!   text.
//!
//! Warnings: an element with no mapping has its text flattened into the
//! enclosing block and one `WARNING_CODE_UNMAPPED_CONSTRUCT` aggregated per
//! element name; `WARNING_CODE_MALFORMED_STRUCTURE` is raised only from
//! html5ever's own tree-builder error list — the repairs the parser
//! actually made — never guessed.

use scraper::node::Node;
use scraper::{ElementRef, Html};

use crate::parse::{Block, Emitter, Image, ParseError, scalar};
use crate::proto::v1 as pb;
use crate::spans::{Inline, Mark};

/// Elements whose entire subtree is dropped: executable, presentational or
/// interactive machinery whose text is not document content.
const SKIPPED: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "object", "embed", "svg", "canvas",
    "audio", "video", "source", "track", "input", "select", "option", "optgroup", "textarea",
    "button", "datalist", "map", "area", "base", "link", "meta", "title", "head",
];

/// Inline elements whose text flows into the enclosing block unchanged.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "del", "dfn", "em", "i", "ins", "kbd",
    "label", "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var",
    "wbr",
];

/// Containers with no item of their own: flush the inline buffer and walk
/// their children. Their structure survives through what is inside them.
const CONTAINERS: &[&str] = &[
    "html", "body", "div", "section", "article", "main", "aside", "nav", "header", "footer",
    "form", "fieldset", "center", "hgroup", "picture", "figure",
];

/// Map one HTML document or fragment. Empty input is a valid empty parse:
/// HTML has no mandatory header the way WebVTT does.
///
/// # Errors
///
/// Only [`ParseError::ConsumerGone`]: html5ever's tree builder repairs
/// rather than rejects, and every repair it reports becomes a warning.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let html = Html::parse_document(text);

    // The tree builder's own error list is the only source of
    // MALFORMED_STRUCTURE: these are repairs html5ever actually made. Two
    // reports are excluded because they mark legal inputs, not damage: the
    // missing-doctype complaint, and the bare detail-free "Unexpected
    // token" the builder emits for the same reason when a fragment starts
    // with content where a doctype could have been.
    for error in &html.errors {
        if error.to_ascii_lowercase().contains("doctype") || error.as_ref() == "Unexpected token" {
            continue;
        }
        emitter.warn(
            pb::WarningCode::MalformedStructure,
            &format!("html5ever repaired the tree: {error}"),
        );
    }

    let root = html.root_element();
    let head = child_element(&root, "head");
    let title = head
        .and_then(|head| child_element(&head, "title"))
        .map(|title| collapse(&text_of(&title)))
        .filter(|title| !title.is_empty());
    let mut metadata = std::collections::HashMap::new();
    if let Some(head) = head {
        for meta in head.child_elements() {
            if meta.value().name() != "meta" {
                continue;
            }
            // `property` is how OpenGraph, Dublin Core and Twitter cards
            // name themselves; requiring `name` skipped all of them.
            let key = meta.attr("name").or_else(|| meta.attr("property"));
            if let (Some(name), Some(content)) = (key, meta.attr("content")) {
                let name = name.trim().to_ascii_lowercase();
                if !name.is_empty() && !content.is_empty() {
                    metadata.insert(format!("meta.{name}"), scalar(content));
                }
            }
        }
    }
    emitter.info(title, metadata)?;

    let mut walk = Walk {
        emitter,
        title_taken: false,
        quote_depth: 0,
        buffer: None,
    };
    if let Some(body) = child_element(&root, "body") {
        walk.container(&body)?;
    }
    Ok(())
}

/// The first child element named `name`.
fn child_element<'a>(parent: &ElementRef<'a>, name: &str) -> Option<ElementRef<'a>> {
    parent
        .child_elements()
        .find(|child| child.value().name() == name)
}

/// All text under an element, in order, whitespace as written.
fn text_of(element: &ElementRef<'_>) -> String {
    element.text().collect()
}

/// Runs of whitespace collapsed to single spaces, ends trimmed.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The run one inline element declares, or nothing when the element is
/// only a container for its children.
///
/// This is the table the fold used to not have: every name in [`INLINE`]
/// either names a rendering signal, carries a machine-readable value the
/// text does not, or says nothing at all.
fn inline_mark(element: &ElementRef<'_>, name: &str) -> Option<Mark> {
    let attr = |key: &str| element.attr(key).map(str::trim).filter(|v| !v.is_empty());
    let style = |style: pb::SpanStyle| Some(Mark::style(style));
    let mut mark = match name {
        "b" | "strong" => style(pb::SpanStyle::Bold),
        "i" | "em" | "cite" | "dfn" | "var" => style(pb::SpanStyle::Italic),
        "u" => style(pb::SpanStyle::Underline),
        "ins" => Some(Mark::style(pb::SpanStyle::Insertion).with_style(pb::SpanStyle::Underline)),
        "s" | "del" => style(pb::SpanStyle::Strikethrough),
        "code" | "kbd" | "samp" => style(pb::SpanStyle::Code),
        "mark" => style(pb::SpanStyle::Mark),
        "small" => style(pb::SpanStyle::Small),
        "sub" => style(pb::SpanStyle::Subscript),
        "sup" => style(pb::SpanStyle::Superscript),
        "q" => style(pb::SpanStyle::Quote),
        // The expansion, the machine-readable timestamp and the machine
        // value are exactly what the visible text does not say.
        "abbr" => Some(match attr("title") {
            Some(title) => Mark::style(pb::SpanStyle::Abbreviation).with_raw(title),
            None => Mark::style(pb::SpanStyle::Abbreviation),
        }),
        "time" => attr("datetime").map(|value| Mark::default().with_raw(value)),
        "data" => attr("value").map(|value| Mark::default().with_raw(value)),
        _ => None,
    };
    // An `id` on an inline element is an anchor a fragment link resolves
    // to, whatever else the element says.
    if let Some(id) = attr("id") {
        mark.get_or_insert_default().anchor_id = Some(id.to_owned());
    }
    mark
}

/// The link one `<a>` declares: its destination, its title, and the
/// fragment it points at when the target is inside this document.
fn link_mark(element: &ElementRef<'_>) -> Option<Mark> {
    let href = element
        .attr("href")
        .map(str::trim)
        .filter(|v| !v.is_empty())?;
    let mut mark = Mark::link(href, element.attr("title"));
    if let Some(fragment) = href.strip_prefix('#').filter(|f| !f.is_empty()) {
        mark.reference = Some(fragment.to_owned());
        mark.reference_kind = pb::ReferenceKind::CrossRef;
    }
    if let Some(id) = element.attr("id").map(str::trim).filter(|v| !v.is_empty()) {
        mark.anchor_id = Some(id.to_owned());
    }
    Some(mark)
}

/// The anchor a block element declares, when it names one.
fn anchor_of(element: &ElementRef<'_>) -> Option<String> {
    element
        .attr("id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// The walk state of one document.
struct Walk<'w, 'e> {
    emitter: &'w mut Emitter<'e>,
    title_taken: bool,
    quote_depth: u32,
    /// Inline text collected since the last block boundary.
    buffer: Option<Inline>,
}

impl Walk<'_, '_> {
    /// Walk the children of a container, flushing loose inline text as
    /// paragraphs at block boundaries — docling's `_walk` in miniature.
    fn container(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        for node in element.children() {
            match node.value() {
                Node::Text(text) => {
                    self.buffer.get_or_insert_default().push(text);
                }
                Node::Element(_) => {
                    let child = ElementRef::wrap(node).expect("an element node wraps");
                    self.element(&child)?;
                }
                _ => {}
            }
        }
        self.flush()
    }

    /// Dispatch one element.
    fn element(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        let name_owned = element.value().name().to_ascii_lowercase();
        let name = name_owned.as_str();
        if SKIPPED.contains(&name) {
            return Ok(());
        }
        if INLINE.contains(&name) {
            return self.inline(element, name);
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush()?;
                self.heading(element, name)
            }
            "p" => {
                self.flush()?;
                self.paragraph(element, "")
            }
            "address" | "summary" => {
                self.flush()?;
                self.paragraph(element, name)
            }
            "blockquote" => {
                self.flush()?;
                self.quote_depth += 1;
                let result = self.container(element);
                self.quote_depth -= 1;
                result
            }
            "pre" => {
                self.flush()?;
                self.pre(element)
            }
            "ul" => {
                self.flush()?;
                self.list(element, false, 1)
            }
            "ol" => {
                self.flush()?;
                self.list(element, true, 1)
            }
            "dl" => {
                self.flush()?;
                self.definition_list(element, 1)
            }
            "table" => {
                self.flush()?;
                self.table(element)
            }
            "figure" => {
                self.flush()?;
                self.figure(element)
            }
            "img" => {
                self.flush()?;
                self.image(element)
            }
            "details" => {
                self.flush()?;
                self.container(element)
            }
            "hr" => {
                self.flush()?;
                self.emitter.warn(pb::WarningCode::UnmappedConstruct, "hr");
                Ok(())
            }
            "br" => {
                if let Some(buffer) = self.buffer.as_mut() {
                    buffer.space();
                }
                Ok(())
            }
            // Orphaned pieces of structures handled by their parents: treat
            // as containers so their text is not lost.
            "li" | "dt" | "dd" | "caption" | "figcaption" | "thead" | "tbody" | "tfoot" | "tr"
            | "td" | "th" => {
                self.flush()?;
                self.container(element)
            }
            _ if CONTAINERS.contains(&name) => {
                self.flush()?;
                self.container(element)
            }
            _ => {
                // No mapping. The text still flows into the enclosing
                // block rather than being dropped, and the trailer says
                // what was flattened.
                self.emitter
                    .warn(pb::WarningCode::UnmappedConstruct, &name_owned);
                self.inline(element, name)
            }
        }
    }

    /// An inline element: its text flows into the current buffer, and the
    /// run it covers is recorded. A nested block (invalid but common)
    /// flushes and dispatches.
    fn inline(&mut self, element: &ElementRef<'_>, name: &str) -> Result<(), ParseError> {
        let link = (name == "a").then(|| link_mark(element)).flatten();
        let mark = link.clone().or_else(|| inline_mark(element, name));
        if let Some(mark) = mark.clone() {
            let buffer = self.buffer.get_or_insert_default();
            if link.is_some() {
                buffer.open_link_mark(mark);
            } else {
                buffer.open(mark);
            }
        }
        for node in element.children() {
            match node.value() {
                Node::Text(text) => self.buffer.get_or_insert_default().push(text),
                Node::Element(_) => {
                    let child = ElementRef::wrap(node).expect("an element node wraps");
                    self.element(&child)?;
                }
                _ => {}
            }
        }
        if mark.is_some()
            && let Some(buffer) = self.buffer.as_mut()
        {
            if link.is_some() {
                buffer.close_link();
            } else {
                buffer.close();
            }
        }
        Ok(())
    }

    /// Send one block, stamping how deep inside `<blockquote>` it sat.
    ///
    /// Every block this fold sends goes through here: a quotation contains
    /// whatever the author put in it, and the depth is what lets the
    /// Document fold rebuild the nesting.
    fn emit(&mut self, mut block: Block) -> Result<(), ParseError> {
        if self.quote_depth > 0 {
            block.quote_depth = Some(self.quote_depth);
        }
        self.emitter.text_block(block)
    }

    /// Flush the inline buffer as one paragraph (or quote) block.
    fn flush(&mut self) -> Result<(), ParseError> {
        let Some(mut buffer) = self.buffer.take() else {
            return Ok(());
        };
        if buffer.is_empty() {
            return Ok(());
        }
        let label = if self.quote_depth > 0 {
            pb::BlockLabel::Quote
        } else {
            pb::BlockLabel::Paragraph
        };
        let hyperlink = buffer.hyperlink();
        let (text, spans) = buffer.take();
        self.emit(Block {
            label,
            text,
            hyperlink,
            spans,
            ..Block::default()
        })
    }

    fn heading(&mut self, element: &ElementRef<'_>, name: &str) -> Result<(), ParseError> {
        let (text, spans, hyperlink) = self.flatten(element)?;
        if text.is_empty() {
            return Ok(());
        }
        let level = u32::from(name.as_bytes()[1] - b'0');
        let (label, level) = if level == 1 && !self.title_taken {
            self.title_taken = true;
            (pb::BlockLabel::Title, None)
        } else {
            (pb::BlockLabel::SectionHeader, Some(level))
        };
        self.emit(Block {
            label,
            text,
            level,
            hyperlink,
            spans,
            anchor_id: anchor_of(element),
            ..Block::default()
        })
    }

    fn paragraph(&mut self, element: &ElementRef<'_>, role: &str) -> Result<(), ParseError> {
        let (text, spans, hyperlink) = self.flatten(element)?;
        if text.is_empty() {
            return Ok(());
        }
        let label = if self.quote_depth > 0 {
            pb::BlockLabel::Quote
        } else {
            pb::BlockLabel::Paragraph
        };
        self.emit(Block {
            label,
            role: role.to_owned(),
            text,
            hyperlink,
            spans,
            anchor_id: anchor_of(element),
            ..Block::default()
        })
    }

    /// Flatten an element's inline subtree into one text and its runs,
    /// emitting the nested blocks (images, lists, tables) it may illegally
    /// contain before the text itself.
    fn flatten(
        &mut self,
        element: &ElementRef<'_>,
    ) -> Result<(String, Vec<pb::InlineSpan>, Option<String>), ParseError> {
        let outer = self.buffer.take();
        self.buffer = Some(Inline::default());
        for node in element.children() {
            match node.value() {
                Node::Text(text) => self.buffer.get_or_insert_default().push(text),
                Node::Element(_) => {
                    let child = ElementRef::wrap(node).expect("an element node wraps");
                    self.element(&child)?;
                }
                _ => {}
            }
        }
        let mut buffer = self.buffer.take().unwrap_or_default();
        self.buffer = outer;
        let hyperlink = buffer.hyperlink();
        let (text, spans) = buffer.take();
        Ok((text, spans, hyperlink))
    }

    fn pre(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        let raw = text_of(element);
        let text = raw
            .strip_prefix('\n')
            .unwrap_or(&raw)
            .trim_end_matches('\n')
            .to_owned();
        if text.is_empty() {
            return Ok(());
        }
        let language = language_class(element)
            .or_else(|| child_element(element, "code").and_then(|code| language_class(&code)));
        self.emit(Block {
            label: pb::BlockLabel::Code,
            text,
            language,
            anchor_id: anchor_of(element),
            ..Block::default()
        })
    }

    fn list(
        &mut self,
        element: &ElementRef<'_>,
        ordered: bool,
        depth: u32,
    ) -> Result<(), ParseError> {
        // A written-out marker exists only when the source numbered the
        // list itself; implicit numbering carries no marker, as docling.
        let start: Option<u64> = element
            .attr("start")
            .and_then(|start| start.trim().parse().ok())
            .filter(|_| ordered);
        let mut counter = start;
        for child in element.child_elements() {
            match child.value().name() {
                "li" => {
                    let marker = counter.map(|n| format!("{n}."));
                    if let Some(n) = counter.as_mut() {
                        *n += 1;
                    }
                    self.list_item(&child, ordered, depth, marker)?;
                }
                // A list directly inside a list is invalid HTML browsers
                // accept; it reads as a nested list.
                "ul" => self.list(&child, false, depth + 1)?,
                "ol" => self.list(&child, true, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// One `li`: its inline text is the item, nested blocks follow it.
    fn list_item(
        &mut self,
        li: &ElementRef<'_>,
        ordered: bool,
        depth: u32,
        marker: Option<String>,
    ) -> Result<(), ParseError> {
        let outer = self.buffer.take();
        self.buffer = Some(Inline::default());
        let anchor = anchor_of(li);
        let mut emitted = false;
        for node in li.children() {
            let child = match node.value() {
                Node::Text(text) => {
                    self.buffer.get_or_insert_default().push(text);
                    continue;
                }
                Node::Element(_) => ElementRef::wrap(node).expect("an element node wraps"),
                _ => continue,
            };
            let name = child.value().name();
            let is_block = matches!(name, "ul" | "ol" | "dl" | "table" | "pre" | "blockquote")
                || (name == "p" && !self.buffer.as_ref().is_none_or(Inline::is_empty));
            if is_block {
                self.flush_item(ordered, depth, marker.clone(), anchor.clone(), &mut emitted)?;
                match name {
                    "ul" => self.list(&child, false, depth + 1)?,
                    "ol" => self.list(&child, true, depth + 1)?,
                    "dl" => self.definition_list(&child, depth + 1)?,
                    _ => self.element(&child)?,
                }
                self.buffer = Some(Inline::default());
            } else if name == "p" {
                // The leading paragraph is the item's own text.
                for grandchild in child.children() {
                    match grandchild.value() {
                        Node::Text(text) => self.buffer.get_or_insert_default().push(text),
                        Node::Element(_) => {
                            let inline =
                                ElementRef::wrap(grandchild).expect("an element node wraps");
                            self.element(&inline)?;
                        }
                        _ => {}
                    }
                }
            } else {
                self.element(&child)?;
            }
        }
        self.flush_item(ordered, depth, marker, anchor, &mut emitted)?;
        self.buffer = outer;
        Ok(())
    }

    /// Emit the pending list-item text, once.
    fn flush_item(
        &mut self,
        ordered: bool,
        depth: u32,
        marker: Option<String>,
        anchor: Option<String>,
        emitted: &mut bool,
    ) -> Result<(), ParseError> {
        let Some(mut buffer) = self.buffer.take() else {
            return Ok(());
        };
        if buffer.is_empty() {
            return Ok(());
        }
        let hyperlink = buffer.hyperlink();
        let (text, spans) = buffer.take();
        if *emitted {
            // A second run of text in the same item: a further paragraph.
            return self.emit(Block {
                label: pb::BlockLabel::Paragraph,
                text,
                hyperlink,
                spans,
                ..Block::default()
            });
        }
        *emitted = true;
        self.emit(Block {
            label: pb::BlockLabel::ListItem,
            text,
            enumerated: ordered,
            marker,
            list_depth: Some(depth),
            hyperlink,
            spans,
            anchor_id: anchor,
            ..Block::default()
        })
    }

    /// `dl`: terms and descriptions as list items with the proto's own
    /// role vocabulary, descriptions nested one deeper than their term.
    fn definition_list(&mut self, element: &ElementRef<'_>, depth: u32) -> Result<(), ParseError> {
        for child in element.child_elements() {
            let (role, item_depth) = match child.value().name() {
                "dt" => ("definition-term", depth),
                "dd" => ("definition-description", depth + 1),
                _ => continue,
            };
            let (text, spans, hyperlink) = self.flatten(&child)?;
            if text.is_empty() {
                continue;
            }
            self.emit(Block {
                label: pb::BlockLabel::ListItem,
                role: role.to_owned(),
                text,
                list_depth: Some(item_depth),
                hyperlink,
                spans,
                anchor_id: anchor_of(&child),
                ..Block::default()
            })?;
        }
        Ok(())
    }

    fn table(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        let caption = child_element(element, "caption")
            .map(|caption| collapse(&text_of(&caption)))
            .filter(|caption| !caption.is_empty());
        self.emitter
            .table_start_with_columns(caption, None, column_specs(element))?;

        // Rows in DOM order, `thead`/`tbody`/`tfoot` unwrapped.
        let mut rows: Vec<ElementRef<'_>> = Vec::new();
        for child in element.child_elements() {
            match child.value().name() {
                "tr" => rows.push(child),
                "thead" | "tbody" | "tfoot" => {
                    rows.extend(
                        child
                            .child_elements()
                            .filter(|row| row.value().name() == "tr"),
                    );
                }
                _ => {}
            }
        }

        let mut occupied: std::collections::BTreeSet<(u32, u32)> =
            std::collections::BTreeSet::new();
        for (row_index, row) in rows.iter().enumerate() {
            let row_index = u32::try_from(row_index).unwrap_or(u32::MAX);
            let in_head = row
                .parent()
                .and_then(ElementRef::wrap)
                .is_some_and(|parent| parent.value().name() == "thead");
            let mut column = 0u32;
            let mut cells = Vec::new();
            let mut all_headers = true;
            for cell in row.child_elements() {
                let cell_name = cell.value().name();
                if cell_name != "td" && cell_name != "th" {
                    continue;
                }
                while occupied.contains(&(row_index, column)) {
                    column += 1;
                }
                let span = |attr: &str| {
                    cell.attr(attr)
                        .and_then(|value| value.trim().parse::<u32>().ok())
                        .filter(|span| *span >= 1)
                        .unwrap_or(1)
                };
                let column_span = span("colspan");
                let row_span = span("rowspan");
                for r in row_index..row_index.saturating_add(row_span) {
                    for c in column..column.saturating_add(column_span) {
                        occupied.insert((r, c));
                    }
                }
                let is_header = cell_name == "th" || in_head;
                all_headers &= is_header;
                // Nested block structure inside a cell still flattens to
                // its text; its emphasis and its links no longer do.
                let (text, spans) = cell_inline(&cell);
                cells.push(pb::TableCell {
                    column_index: column,
                    text,
                    column_span,
                    row_span,
                    is_header,
                    spans,
                    align: attr_alignment(&cell, "align") as i32,
                    valign: attr_alignment(&cell, "valign") as i32,
                });
                column = column.saturating_add(column_span);
            }
            if cells.is_empty() {
                continue;
            }
            self.emitter.table_row(all_headers, cells)?;
        }
        self.emitter.table_end()
    }

    /// `figure`: the image first, its `figcaption` as a following caption
    /// block, anything else as ordinary content.
    fn figure(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        for child in element.child_elements() {
            match child.value().name() {
                "img" => self.image(&child)?,
                "figcaption" => {
                    let (text, spans, hyperlink) = self.flatten(&child)?;
                    if !text.is_empty() {
                        self.emit(Block {
                            label: pb::BlockLabel::Caption,
                            text,
                            hyperlink,
                            spans,
                            anchor_id: anchor_of(&child),
                            ..Block::default()
                        })?;
                    }
                }
                _ => self.element(&child)?,
            }
        }
        self.flush()
    }

    fn image(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        let attr = |name: &str| element.attr(name).unwrap_or("").trim().to_owned();
        let src = attr("src");
        let alt = attr("alt");
        let title = attr("title");
        if src.is_empty() && alt.is_empty() && title.is_empty() {
            // A bare <img> with nothing on it carries no reference at all.
            return Ok(());
        }
        // The attributes say how wide the document asks the image to be, in
        // whatever the author wrote: `640`, `50%`. Kept verbatim, because
        // this service never fetches the image and so never measures one.
        let dimension = |name: &str| {
            element
                .attr(name)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        self.emitter.image(Image {
            src,
            alt,
            title,
            source_line: None,
            width: dimension("width"),
            height: dimension("height"),
        })
    }
}

/// The column geometry a table's `<colgroup>` and `<col>` elements
/// declare, expanded so index N of the result is column N.
fn column_specs(table: &ElementRef<'_>) -> Vec<pb::ColumnSpec> {
    let mut specs = Vec::new();
    let columns = table
        .child_elements()
        .flat_map(|child| match child.value().name() {
            "colgroup" => child.child_elements().collect::<Vec<_>>(),
            "col" => vec![child],
            _ => Vec::new(),
        })
        .filter(|column| column.value().name() == "col");
    for column in columns {
        let spec = pb::ColumnSpec {
            align: attr_alignment(&column, "align") as i32,
            valign: attr_alignment(&column, "valign") as i32,
            width: column
                .attr("width")
                .map(str::trim)
                .filter(|width| !width.is_empty())
                .map(str::to_owned),
        };
        // `span` repeats one declaration across that many columns.
        let repeat = column
            .attr("span")
            .and_then(|span| span.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1024);
        specs.extend(std::iter::repeat_n(spec, repeat));
    }
    specs
}

/// One alignment attribute as the wire's own vocabulary.
fn attr_alignment(element: &ElementRef<'_>, name: &str) -> pb::Alignment {
    match element
        .attr(name)
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("left") => pb::Alignment::Left,
        Some("center" | "centre") => pb::Alignment::Center,
        Some("right") => pb::Alignment::Right,
        Some("justify") => pb::Alignment::Justify,
        Some("top") => pb::Alignment::Top,
        Some("middle") => pb::Alignment::Middle,
        Some("bottom") => pb::Alignment::Bottom,
        _ => pb::Alignment::Unspecified,
    }
}

/// One table cell's text and the runs inside it.
///
/// Deliberately not [`Walk::flatten`]: a cell is flattened without emitting
/// anything, because a block event between a `TableRow` and its `TableEnd`
/// would break the stream's document order.
fn cell_inline(cell: &ElementRef<'_>) -> (String, Vec<pb::InlineSpan>) {
    let mut inline = Inline::default();
    cell_children(cell, &mut inline);
    let (text, spans) = inline.take();
    (text, spans)
}

/// Walk one cell subtree into a flattener, recording the runs its inline
/// elements declare and flattening everything else.
fn cell_children(element: &ElementRef<'_>, inline: &mut Inline) {
    for node in element.children() {
        match node.value() {
            Node::Text(text) => inline.push(text),
            Node::Element(_) => {
                let child = ElementRef::wrap(node).expect("an element node wraps");
                let name = child.value().name().to_ascii_lowercase();
                if SKIPPED.contains(&name.as_str()) {
                    continue;
                }
                if name == "br" {
                    inline.space();
                    continue;
                }
                let link = (name == "a").then(|| link_mark(&child)).flatten();
                let mark = link.clone().or_else(|| inline_mark(&child, &name));
                match (link.is_some(), mark) {
                    (true, Some(mark)) => inline.open_link_mark(mark),
                    (false, Some(mark)) => inline.open(mark),
                    (_, None) => {
                        // A block inside a cell is a word boundary, not a
                        // run: its text joins the cell's.
                        inline.space();
                        cell_children(&child, inline);
                        inline.space();
                        continue;
                    }
                }
                cell_children(&child, inline);
                if link.is_some() {
                    inline.close_link();
                } else {
                    inline.close();
                }
            }
            _ => {}
        }
    }
}

/// The declared code language from a `language-*` or `lang-*` class.
fn language_class(element: &ElementRef<'_>) -> Option<String> {
    element.value().classes().find_map(|class| {
        class
            .strip_prefix("language-")
            .or_else(|| class.strip_prefix("lang-"))
            .filter(|language| !language.is_empty())
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_flattens_all_whitespace_runs() {
        assert_eq!(collapse("  a\n\t b  c  "), "a b c");
        assert_eq!(collapse(""), "");
    }

    #[test]
    fn language_classes_are_recognized_with_both_prefixes() {
        let html = Html::parse_fragment(r#"<pre class="language-rust wrap"></pre>"#);
        let pre = html
            .select(&scraper::Selector::parse("pre").unwrap())
            .next();
        assert_eq!(language_class(&pre.unwrap()).as_deref(), Some("rust"));

        let html = Html::parse_fragment(r#"<code class="lang-sql"></code>"#);
        let code = html
            .select(&scraper::Selector::parse("code").unwrap())
            .next();
        assert_eq!(language_class(&code.unwrap()).as_deref(), Some("sql"));
    }

    #[test]
    fn the_whole_block_link_rule_matches_the_markdown_fold() {
        let mut whole = Inline::default();
        whole.open_link("https://example.com", None);
        whole.push("Docs");
        whole.close_link();
        assert_eq!(whole.hyperlink().as_deref(), Some("https://example.com"));

        let mut partial = Inline::default();
        partial.push("see ");
        partial.open_link("https://example.com", None);
        partial.push("docs");
        partial.close_link();
        assert!(
            partial.hyperlink().is_none(),
            "one link plus prose is not a whole-block link"
        );
        let (_, spans) = partial.take();
        assert_eq!(
            spans.len(),
            1,
            "the URL still survives as a run, which is the point"
        );
    }

    #[test]
    fn inline_names_map_to_the_runs_they_declare() {
        let html = Html::parse_fragment(
            r#"<p><strong>a</strong><abbr title="HyperText">b</abbr><span id="x">c</span></p>"#,
        );
        let pick = |selector: &str| {
            let selector = scraper::Selector::parse(selector).unwrap();
            html.select(&selector).next().expect("present")
        };
        let strong = pick("strong");
        assert_eq!(
            inline_mark(&strong, "strong").unwrap().styles,
            vec![pb::SpanStyle::Bold]
        );
        let abbr = pick("abbr");
        let mark = inline_mark(&abbr, "abbr").unwrap();
        assert_eq!(mark.styles, vec![pb::SpanStyle::Abbreviation]);
        assert_eq!(mark.raw.as_deref(), Some("HyperText"));
        // A `span` says nothing on its own, but an id on one is an anchor.
        let span = pick("span");
        assert_eq!(
            inline_mark(&span, "span").unwrap().anchor_id.as_deref(),
            Some("x")
        );
    }
}
