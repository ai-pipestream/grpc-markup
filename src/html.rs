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
//!   A cell's text is flattened as always; a *rich* cell — one holding a
//!   nested table, a list, a heading, a `pre` with inline structure, or
//!   `u`/`ins`/`sub`/`sup` formatting — also carries its structure as
//!   `CellBlock`s on the cell, because no block events may stream between
//!   a `TableRow` and its `TableEnd`.
//! - `pre` keeps its text verbatim as `BLOCK_LABEL_CODE` with a
//!   `language-*` class as the declared language.
//! - `img` becomes an `ImageBlock` (`src`, `alt`, `title` verbatim, never
//!   fetched; a `srcset` with no `src` lends its first candidate); a
//!   `figure`'s `figcaption` follows as a `CAPTION` block. An `<a>` whose
//!   only content is the image lends the block its `href`, and a caption
//!   that merely holds links carries the first one's target; the anchor
//!   text stays inline either way. The other ways a
//!   page places a picture map onto the same block through
//!   [`crate::html_pictures`]: an `<svg>` yields one `ImageBlock` per
//!   `<image>` it holds (`href`, else `xlink:href`), or one carrying its
//!   own drawing as a `data:` URI when it holds none and is small enough,
//!   and nothing when it is decorative; an `<object>` or `<embed>` whose
//!   type is an image yields one from its `data` / `src`; a `<picture>`
//!   yields exactly the one its `<img>` fallback names.
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

use html5ever::tendril::TendrilSink;
use scraper::node::Node;
use scraper::{ElementRef, Html, HtmlTreeSink};

use crate::html_pictures::{embedded_picture, srcset_first_candidate, svg_pictures};
use crate::parse::{Block, Cancel, Emitter, Image, ParseError, Table, scalar};
use crate::proto::v1 as pb;
use crate::spans::{Inline, Mark};

/// Elements whose entire subtree is dropped: executable, presentational or
/// interactive machinery whose text is not document content. `svg`,
/// `object` and `embed` are not here because each can place a picture; the
/// ones that do not are dropped by their own arms.
const SKIPPED: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "canvas", "audio", "video", "source",
    "track", "input", "select", "option", "optgroup", "textarea", "button", "datalist", "map",
    "area", "base", "link", "meta", "title", "head",
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

/// How deep elements may nest before the rest of a subtree is flattened.
///
/// html5ever builds trees of any depth, and the walk below recurses once or
/// more per element, so a few kilobytes of nested `<div>`s would overflow
/// the blocking pool's 2 MiB stack and abort the process. Real pages nest a
/// few dozen levels; past this one the subtree keeps its text and loses
/// its structure.
pub const MAX_ELEMENT_DEPTH: usize = 256;

/// Map one HTML document or fragment. Empty input is a valid empty parse:
/// HTML has no mandatory header the way WebVTT does.
///
/// # Errors
///
/// Only [`ParseError::ConsumerGone`]: html5ever's tree builder repairs
/// rather than rejects, and every repair it reports becomes a warning.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let mut html = parse_document(text, emitter.cancel())?;
    let flattened = flatten_deep_subtrees(&mut html);
    emitter.warn_n(
        pb::WarningCode::UnmappedConstruct,
        &format!("elements nested deeper than {MAX_ELEMENT_DEPTH} levels flattened to their text"),
        flattened,
    );

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

/// How many bytes the tree builder is fed between cancellation checks.
/// The cost of a slice grows with the nesting it opens, so this is what
/// bounds how far past its deadline a hostile parse can run: at 64 KiB a
/// `<ul><li>` ladder overran a 200 ms deadline by a second in release and
/// by half a minute in debug.
const PARSE_SLICE: usize = 4 * 1024;

/// `Html::parse_document`, fed in slices with a cancellation check
/// between them.
///
/// html5ever's tree builder scans the stack of open elements for scope
/// checks, so deeply nested input costs time quadratic in its depth before
/// the fold sees a single node; slicing is what lets a deadline or a
/// departed client stop that work.
fn parse_document(text: &str, cancel: &Cancel) -> Result<Html, ParseError> {
    let mut parser = html5ever::driver::parse_document(
        HtmlTreeSink::new(Html::new_document()),
        Default::default(),
    );
    let mut rest = text;
    while !rest.is_empty() {
        cancel.check()?;
        let mut cut = rest.len().min(PARSE_SLICE);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (slice, tail) = rest.split_at(cut);
        parser.process(slice.into());
        rest = tail;
    }
    Ok(parser.finish())
}

/// Replace the children of every element sitting [`MAX_ELEMENT_DEPTH`]
/// elements deep with one text node holding their text, so every walk
/// after this one recurses a bounded number of times. Iterative throughout;
/// returns how many subtrees were flattened.
fn flatten_deep_subtrees(html: &mut Html) -> u64 {
    let mut cut = Vec::new();
    let mut stack = vec![(html.tree.root().id(), 0usize)];
    while let Some((id, depth)) = stack.pop() {
        let Some(node) = html.tree.get(id) else {
            continue;
        };
        if depth >= MAX_ELEMENT_DEPTH && node.has_children() {
            cut.push(id);
            continue;
        }
        for child in node.children() {
            let child_depth = depth + usize::from(child.value().is_element());
            stack.push((child.id(), child_depth));
        }
    }
    for id in &cut {
        // The subtree's text, iteratively, leaving out the subtrees the
        // walk would have skipped and separating elements with a space so
        // block boundaries do not glue words together.
        let mut text = String::new();
        let mut pending: Vec<_> = html
            .tree
            .get(*id)
            .into_iter()
            .flat_map(|n| n.children().rev())
            .collect();
        while let Some(node) = pending.pop() {
            match node.value() {
                Node::Text(run) => text.push_str(run),
                Node::Element(element) if !SKIPPED.contains(&element.name()) => {
                    text.push(' ');
                    pending.extend(node.children().rev());
                }
                _ => {}
            }
        }
        let Some(mut node) = html.tree.get_mut(*id) else {
            continue;
        };
        while let Some(mut child) = node.first_child() {
            child.detach();
        }
        if !text.trim().is_empty() {
            node.append(Node::Text(scraper::node::Text { text: text.into() }));
        }
    }
    cut.len() as u64
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
            "svg" => self.pictures(svg_pictures(element)),
            "object" => self.pictures(embedded_picture(element, "data")),
            "embed" => self.pictures(embedded_picture(element, "src")),
            "details" => {
                self.flush()?;
                self.container(element)
            }
            "hr" => {
                self.flush()?;
                self.emitter.warn(pb::WarningCode::UnmappedConstruct, "hr");
                Ok(())
            }
            // A break the author wrote is a line break, not a word
            // boundary the collapsing may swallow.
            "br" => {
                if let Some(buffer) = self.buffer.as_mut() {
                    buffer.push_verbatim("\n");
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
        // An <a> whose only content is one image is a link on the picture,
        // not on prose: the href rides on the ImageBlock. Text beside the
        // image keeps the link a prose link instead.
        if name == "a"
            && let Some(href) = element
                .attr("href")
                .map(str::trim)
                .filter(|href| !href.is_empty())
            && let Some(img) = lone_image(element)
        {
            self.flush()?;
            return self.linked_image(&img, href);
        }
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
        self.emitter.table_start(Table {
            caption,
            columns: column_specs(element),
            ..Table::default()
        })?;
        let source = self.emitter.source().clone();
        for row in table_rows(element, &source) {
            self.emitter.table_row(row.is_header, row.cells)?;
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
                        // A caption that is one link carries it as the
                        // block's own; a caption that merely holds links
                        // names the first of them as the picture's target.
                        // The anchor text stays inline either way.
                        let hyperlink = hyperlink.or_else(|| first_href(&child));
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

    /// The pictures an element other than `<img>` places, if any. A source
    /// that places none leaves the inline buffer alone, exactly as the
    /// skipped subtree it used to be did.
    fn pictures(&mut self, images: impl IntoIterator<Item = Image>) -> Result<(), ParseError> {
        let mut images = images.into_iter().peekable();
        if images.peek().is_some() {
            self.flush()?;
        }
        images.try_for_each(|image| self.emitter.image(image))
    }

    fn image(&mut self, element: &ElementRef<'_>) -> Result<(), ParseError> {
        if let Some(image) = image_block(element) {
            self.emitter.image(image)?;
        }
        Ok(())
    }

    /// The one image of an `<a>` that holds nothing else: the link belongs
    /// to the picture, so the href rides on the block rather than on a
    /// text run.
    fn linked_image(&mut self, element: &ElementRef<'_>, href: &str) -> Result<(), ParseError> {
        if let Some(mut image) = image_block(element) {
            image.hyperlink = Some(href.to_owned());
            self.emitter.image(image)?;
        }
        Ok(())
    }
}

/// The image reference one `<img>` declares, or nothing when it carries no
/// reference at all.
fn image_block(element: &ElementRef<'_>) -> Option<Image> {
    let attr = |name: &str| element.attr(name).unwrap_or("").trim().to_owned();
    let mut src = attr("src");
    if src.is_empty() {
        // A responsive image with no fallback `src` still names its
        // candidates; the first one is the reference.
        src = srcset_first_candidate(&attr("srcset")).unwrap_or_default();
    }
    let alt = attr("alt");
    let title = attr("title");
    if src.is_empty() && alt.is_empty() && title.is_empty() {
        // A bare <img> with nothing on it carries no reference at all.
        return None;
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
    Some(Image {
        src,
        alt,
        title,
        source_line: None,
        width: dimension("width"),
        height: dimension("height"),
        anchor_id: anchor_of(element),
        hyperlink: None,
    })
}

/// The one image an element holds, when an image is all it holds: the
/// `<a href><img></a>` shape, where the link belongs to the picture.
/// Whitespace around the image is nothing; a second element or a word of
/// text makes the element not image-only.
fn lone_image<'a>(element: &ElementRef<'a>) -> Option<ElementRef<'a>> {
    let mut image = None;
    for node in element.children() {
        match node.value() {
            Node::Text(text) if text.trim().is_empty() => {}
            Node::Element(_) => {
                let child = ElementRef::wrap(node).expect("an element node wraps");
                if image.is_some() || child.value().name() != "img" {
                    return None;
                }
                image = Some(child);
            }
            // A word of text, a comment, anything else: not image-only.
            _ => return None,
        }
    }
    image
}

/// The destination of the first link a subtree declares, in document
/// order: the link a caption names even when the caption is more than the
/// link alone.
fn first_href(element: &ElementRef<'_>) -> Option<String> {
    for child in element.child_elements() {
        if child.value().name() == "a"
            && let Some(href) = child
                .attr("href")
                .map(str::trim)
                .filter(|href| !href.is_empty())
        {
            return Some(href.to_owned());
        }
        if let Some(href) = first_href(&child) {
            return Some(href);
        }
    }
    None
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
        let repeat = repeat.min(MAX_COLUMN_SPECS - specs.len());
        specs.extend(std::iter::repeat_n(spec, repeat));
        if specs.len() == MAX_COLUMN_SPECS {
            break;
        }
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
                    inline.push_verbatim("\n");
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

/// The largest `colspan` the HTML table model honours.
const MAX_COLSPAN: u32 = 1000;

/// The largest `rowspan` the HTML table model honours.
const MAX_ROWSPAN: u32 = 65534;

/// How many cells may span down across later rows at once before further
/// `rowspan`s are dropped. Placing a cell sweeps the spans still open, so
/// this bounds the cost of every row.
const MAX_CARRIED_SPANS: usize = 1024;

/// The most column specs one table's `<col>` declarations may produce.
const MAX_COLUMN_SPECS: usize = 16_384;

/// The rows of one `<table>`, in DOM order with `thead`/`tbody`/`tfoot`
/// unwrapped, header cells and rows flagged, and `rowspan` / `colspan`
/// applied through an occupancy grid.
///
/// `table_ref` stays empty on the result: the emitter stamps the identity
/// of a streamed table, and a table nested in a cell has no stream
/// identity of its own.
fn table_rows(element: &ElementRef<'_>, source: &pb::CollectorSource) -> Vec<pb::TableRow> {
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

    let row_count = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    // Cells from earlier rows that still cover later ones, as (first
    // column, end column, first row no longer covered), sorted by first
    // column. Tracking spans rather than every (row, column) pair they
    // cover keeps the cost proportional to the cells, not to the area a
    // hostile `rowspan` times `colspan` claims.
    let mut carried: Vec<(u32, u32, u32)> = Vec::new();
    let mut out = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let row_index = u32::try_from(row_index).unwrap_or(u32::MAX);
        carried.retain(|&(_, _, until)| until > row_index);
        let mut sweep = 0usize;
        let mut started_here = Vec::new();
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
            // Skip the columns a cell from an earlier row still covers.
            while let Some(&(start, end, _)) = carried.get(sweep) {
                if end <= column {
                    sweep += 1;
                } else if start <= column {
                    column = end;
                    sweep += 1;
                } else {
                    break;
                }
            }
            let span = |attr: &str| {
                cell.attr(attr)
                    .and_then(|value| value.trim().parse::<u32>().ok())
                    .filter(|span| *span >= 1)
                    .unwrap_or(1)
            };
            // The HTML table model's own limits: `colspan` at most 1000,
            // `rowspan` at most 65534 and never past the table's last row.
            let column_span = span("colspan").min(MAX_COLSPAN);
            let mut row_span = span("rowspan").min(MAX_ROWSPAN).min(row_count - row_index);
            if row_span > 1 {
                if carried.len() + started_here.len() < MAX_CARRIED_SPANS {
                    started_here.push((
                        column,
                        column.saturating_add(column_span),
                        row_index + row_span,
                    ));
                } else {
                    // Past this many cells spanning down at once the table
                    // is not a table anyone wrote; the span is dropped so
                    // every later row stays cheap to place.
                    row_span = 1;
                }
            }
            let is_header = cell_name == "th" || in_head;
            all_headers &= is_header;
            // The flat walk is the cell's text, unchanged; a rich cell's
            // structure rides beside it on `blocks`.
            let (text, spans) = cell_inline(&cell);
            let blocks = if is_rich_cell(&cell) {
                cell_blocks(&cell, source)
            } else {
                Vec::new()
            };
            cells.push(pb::TableCell {
                column_index: column,
                text,
                column_span,
                row_span,
                is_header,
                spans,
                align: attr_alignment(&cell, "align") as i32,
                valign: attr_alignment(&cell, "valign") as i32,
                blocks,
            });
            column = column.saturating_add(column_span);
        }
        carried.extend(started_here);
        carried.sort_unstable_by_key(|&(start, _, _)| start);
        if cells.is_empty() {
            continue;
        }
        out.push(pb::TableRow {
            table_ref: String::new(),
            row_index,
            is_header: all_headers,
            cells,
        });
    }
    out
}

/// Whether a cell is rich: it holds nested block structure (a table, a
/// list, a heading, a `pre` with inline structure inside it) or the
/// inline formatting a flat projection cannot say (`u`, `ins`, `sub`,
/// `sup`). This is docling's rule for when a cell's content becomes a
/// group of blocks rather than flat text alone.
fn is_rich_cell(cell: &ElementRef<'_>) -> bool {
    let mut stack: Vec<ElementRef<'_>> = cell.child_elements().collect();
    while let Some(element) = stack.pop() {
        let name = element.value().name();
        match name {
            "table" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "u" | "ins"
            | "sub" | "sup" => return true,
            // A `pre` is a code block, which flat text carries; one holding
            // inline elements has structure the flat text cannot say.
            "pre" if element.child_elements().next().is_some() => return true,
            _ => stack.extend(element.child_elements()),
        }
    }
    false
}

/// A rich cell's content as cell blocks: the nested tables, lists,
/// headings and structured code the flat text flattens away, with the
/// prose between them as paragraphs.
///
/// This is a second, structured walk beside the flat one, and it emits
/// nothing: no block events may stream between a `TableRow` and its
/// `TableEnd`, so the blocks ride on the cell itself.
fn cell_blocks(cell: &ElementRef<'_>, source: &pb::CollectorSource) -> Vec<pb::CellBlock> {
    let mut walk = CellBlocks::default();
    walk.children(cell, source);
    walk.flush(source);
    walk.blocks
}

/// The accumulating state of the structured cell walk: the blocks built
/// so far and the prose collected since the last block boundary.
#[derive(Default)]
struct CellBlocks {
    blocks: Vec<pb::CellBlock>,
    inline: Inline,
}

impl CellBlocks {
    /// One level of a rich cell's content: block elements become blocks,
    /// everything between them collects as prose.
    fn children(&mut self, element: &ElementRef<'_>, source: &pb::CollectorSource) {
        for node in element.children() {
            match node.value() {
                Node::Text(text) => self.inline.push(text),
                Node::Element(_) => {
                    let child = ElementRef::wrap(node).expect("an element node wraps");
                    let name = child.value().name().to_ascii_lowercase();
                    match name.as_str() {
                        _ if SKIPPED.contains(&name.as_str()) => {}
                        "table" => {
                            self.flush(source);
                            self.blocks.push(pb::CellBlock {
                                kind: Some(pb::cell_block::Kind::Table(cell_table(&child, source))),
                            });
                        }
                        "ul" => {
                            self.flush(source);
                            self.list(&child, false, 1, source);
                        }
                        "ol" => {
                            self.flush(source);
                            self.list(&child, true, 1, source);
                        }
                        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                            self.flush(source);
                            let mut inline = Inline::default();
                            cell_runs(&child, &mut inline, false);
                            let (text, spans) = inline.take();
                            if !text.is_empty() {
                                let level = u32::from(name.as_bytes()[1] - b'0');
                                let mut block = cell_text_block(
                                    pb::BlockLabel::SectionHeader,
                                    text,
                                    spans,
                                    source,
                                );
                                block.level = Some(level);
                                block.anchor_id = anchor_of(&child);
                                self.blocks.push(into_cell_text(block));
                            }
                        }
                        "pre" if child.child_elements().next().is_some() => {
                            self.flush(source);
                            self.pre(&child, source);
                        }
                        "img" => {
                            self.flush(source);
                            if let Some(image) = image_block(&child) {
                                self.blocks.push(cell_image_block(image, source));
                            }
                        }
                        // A container is a boundary, not a block of its
                        // own: its content joins the cell's sequence.
                        _ if CONTAINERS.contains(&name.as_str()) => {
                            self.flush(source);
                            self.children(&child, source);
                        }
                        _ => cell_run_element(&child, &mut self.inline, false),
                    }
                }
                _ => {}
            }
        }
    }

    /// The prose accumulated since the last block, as one paragraph block.
    fn flush(&mut self, source: &pb::CollectorSource) {
        if self.inline.is_empty() {
            return;
        }
        let (text, spans) = self.inline.take();
        if text.is_empty() {
            return;
        }
        self.blocks.push(into_cell_text(cell_text_block(
            pb::BlockLabel::Paragraph,
            text,
            spans,
            source,
        )));
    }

    /// A list inside a rich cell: one list-item block per `li`, a nested
    /// list following the item it belongs to.
    fn list(
        &mut self,
        list: &ElementRef<'_>,
        ordered: bool,
        depth: u32,
        source: &pb::CollectorSource,
    ) {
        // As on the document level: a marker exists only when the source
        // numbered the list itself.
        let start: Option<u64> = list
            .attr("start")
            .and_then(|start| start.trim().parse().ok())
            .filter(|_| ordered);
        let mut counter = start;
        for child in list.child_elements() {
            match child.value().name() {
                "li" => {
                    let marker = counter.map(|n| format!("{n}."));
                    if let Some(n) = counter.as_mut() {
                        *n += 1;
                    }
                    self.list_item(&child, ordered, depth, marker, source);
                }
                "ul" => self.list(&child, false, depth + 1, source),
                "ol" => self.list(&child, true, depth + 1, source),
                _ => {}
            }
        }
    }

    /// One `li`: its inline content is the item's text, nested lists
    /// follow as deeper items.
    fn list_item(
        &mut self,
        li: &ElementRef<'_>,
        ordered: bool,
        depth: u32,
        marker: Option<String>,
        source: &pb::CollectorSource,
    ) {
        let mut inline = Inline::default();
        let mut nested = Vec::new();
        for node in li.children() {
            match node.value() {
                Node::Text(text) => inline.push(text),
                Node::Element(_) => {
                    let child = ElementRef::wrap(node).expect("an element node wraps");
                    match child.value().name() {
                        "ul" | "ol" => nested.push(child),
                        _ => cell_run_element(&child, &mut inline, false),
                    }
                }
                _ => {}
            }
        }
        let (text, spans) = inline.take();
        if !text.is_empty() {
            let mut block = cell_text_block(pb::BlockLabel::ListItem, text, spans, source);
            block.enumerated = ordered;
            block.marker = marker;
            block.list_depth = Some(depth);
            block.anchor_id = anchor_of(li);
            self.blocks.push(into_cell_text(block));
        }
        for child in nested {
            match child.value().name() {
                "ul" => self.list(&child, false, depth + 1, source),
                "ol" => self.list(&child, true, depth + 1, source),
                _ => {}
            }
        }
    }

    /// A `pre` with inline structure: the text verbatim as a code block,
    /// every run code-styled, and the inner elements' own runs kept — the
    /// link inside stays a link rather than the whole becoming a bare
    /// backticked fragment.
    fn pre(&mut self, pre: &ElementRef<'_>, source: &pb::CollectorSource) {
        let mut inline = Inline::default();
        inline.open(Mark::style(pb::SpanStyle::Code));
        cell_runs(pre, &mut inline, true);
        inline.close();
        let (text, spans) = inline.take();
        if text.trim().is_empty() {
            return;
        }
        let mut block = cell_text_block(pb::BlockLabel::Code, text, spans, source);
        block.language = language_class(pre)
            .or_else(|| child_element(pre, "code").and_then(|code| language_class(&code)));
        block.anchor_id = anchor_of(pre);
        self.blocks.push(into_cell_text(block));
    }
}

/// One text block of a rich cell, stamped with the parse's provenance.
/// Its `index` is unused: the cell carrying it is its position.
fn cell_text_block(
    label: pb::BlockLabel,
    text: String,
    spans: Vec<pb::InlineSpan>,
    source: &pb::CollectorSource,
) -> pb::TextBlock {
    pb::TextBlock {
        label: label as i32,
        text,
        source: Some(source.clone()),
        spans,
        ..pb::TextBlock::default()
    }
}

/// Wrap a rich cell's text block as a cell block.
fn into_cell_text(block: pb::TextBlock) -> pb::CellBlock {
    pb::CellBlock {
        kind: Some(pb::cell_block::Kind::Text(block)),
    }
}

/// One image of a rich cell, as the wire's own image block.
fn cell_image_block(image: Image, source: &pb::CollectorSource) -> pb::CellBlock {
    pb::CellBlock {
        kind: Some(pb::cell_block::Kind::Image(pb::ImageBlock {
            index: 0,
            src: image.src,
            alt: image.alt,
            title: image.title,
            source_line: image.source_line,
            source: Some(source.clone()),
            anchor_id: image.anchor_id,
            width: image.width,
            height: image.height,
            hyperlink: image.hyperlink,
        })),
    }
}

/// A `<table>` nested in a cell, in the wire's own row shape.
fn cell_table(table: &ElementRef<'_>, source: &pb::CollectorSource) -> pb::CellTable {
    let caption = child_element(table, "caption")
        .map(|caption| collapse(&text_of(&caption)))
        .filter(|caption| !caption.is_empty());
    pb::CellTable {
        rows: table_rows(table, source),
        caption,
    }
}

/// Walk an inline subtree into a cell block's flattener: the same marks
/// the flat walk records, with `verbatim` keeping the author's spacing
/// inside `pre`.
fn cell_runs(element: &ElementRef<'_>, inline: &mut Inline, verbatim: bool) {
    for node in element.children() {
        match node.value() {
            Node::Text(text) => {
                if verbatim {
                    inline.push_verbatim(text);
                } else {
                    inline.push(text);
                }
            }
            Node::Element(_) => {
                let child = ElementRef::wrap(node).expect("an element node wraps");
                cell_run_element(&child, inline, verbatim);
            }
            _ => {}
        }
    }
}

/// One element's contribution to a cell block's runs: its own mark over
/// the run it covers, or its text flattened in with word boundaries.
fn cell_run_element(element: &ElementRef<'_>, inline: &mut Inline, verbatim: bool) {
    let name = element.value().name().to_ascii_lowercase();
    if SKIPPED.contains(&name.as_str()) {
        return;
    }
    if name == "br" {
        inline.push_verbatim("\n");
        return;
    }
    let link = (name == "a").then(|| link_mark(element)).flatten();
    let mark = link.clone().or_else(|| inline_mark(element, &name));
    match (link.is_some(), mark) {
        (true, Some(mark)) => inline.open_link_mark(mark),
        (false, Some(mark)) => inline.open(mark),
        (_, None) => {
            inline.space();
            cell_runs(element, inline, verbatim);
            inline.space();
            return;
        }
    }
    cell_runs(element, inline, verbatim);
    if link.is_some() {
        inline.close_link();
    } else {
        inline.close();
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
