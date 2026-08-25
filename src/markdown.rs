// SPDX-License-Identifier: Apache-2.0

//! The Markdown fold: CommonMark plus pipe tables, footnotes, task lists,
//! strikethrough, math and YAML front matter, mapped onto the shared block
//! vocabulary with pulldown-cmark.
//!
//! Shape of the mapping, in stream order:
//!
//! - YAML front matter between leading `---` fences becomes
//!   `MarkupInfo.metadata` as the tree the source declared: scalars are
//!   verbatim text, sequences are `MetaList`s, nested mappings are
//!   `MetaMap`s, and the top-level keys are the mapping's own. Its `title:`
//!   key becomes `MarkupInfo.title` when it is a non-empty scalar. A block
//!   that fails to parse as YAML gets one `WARNING_CODE_METADATA` and its
//!   raw text is not emitted as content.
//! - The first H1 is `BLOCK_LABEL_TITLE`; every other heading is
//!   `BLOCK_LABEL_SECTION_HEADER` with its Markdown level.
//! - Paragraphs flatten their inline markup to text with whitespace
//!   collapsed; a paragraph that is entirely one link carries `hyperlink`.
//! - List items carry `enumerated`, the marker as written in the source, and
//!   the nesting depth. A task-list item records its checkbox in `role`.
//! - Fenced and indented code keeps its text verbatim; the info string's
//!   first word is the `language`.
//! - Blockquote paragraphs are `BLOCK_LABEL_QUOTE`, one block per paragraph,
//!   nesting flattened.
//! - Pipe tables stream as `TableStart` / `TableRow` / `TableEnd` with the
//!   header row flagged.
//! - Images become `ImageBlock` events with `src`, `alt` and `title` as
//!   written; nothing is fetched.
//! - Footnote definitions emit `BLOCK_LABEL_FOOTNOTE` blocks whose `role` is
//!   the footnote label.
//! - Display math is `BLOCK_LABEL_FORMULA`, verbatim.
//! - Raw HTML — block or inline — is flattened to its text with
//!   `WARNING_CODE_EMBEDDED_HTML_FLATTENED`; this collector never parses
//!   HTML (that is the HTML fold's job, in a later build).
//!
//! `source_line` on every block comes from pulldown-cmark's offset iterator,
//! mapped to 1-based lines of the original text, front matter included.

use std::collections::HashMap;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use yaml_rust2::{Yaml, YamlLoader};

use crate::parse::{Block, Emitter, ParseError};
use crate::proto::v1 as pb;

/// Map one Markdown document. Empty input is a valid empty parse.
///
/// # Errors
///
/// Only [`ParseError::ConsumerGone`]: Markdown's own error handling never
/// rejects input, it re-reads it as literal text, and this fold follows it.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let (metadata, title, body_start) = front_matter(text, emitter);
    emitter.info(title, metadata)?;

    let body = &text[body_start..];
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_MATH);

    let mut walk = Walk {
        emitter,
        body,
        lines: LineIndex::new(text),
        base: body_start,
        title_taken: false,
        captures: Vec::new(),
        lists: Vec::new(),
        quote_depth: 0,
        footnotes: Vec::new(),
        table: None,
    };
    for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
        walk.event(event, range.start)?;
    }
    Ok(())
}

// ------------------------------------------------------------- front matter

/// Parse a leading YAML front matter block, when there is one.
///
/// Returns the metadata map, the title it declared, and the byte offset the
/// Markdown body starts at. A block that is fenced but not YAML warns and is
/// skipped; text without a closed leading fence is all body.
fn front_matter(
    text: &str,
    emitter: &mut Emitter<'_>,
) -> (HashMap<String, pb::MetaValue>, Option<String>, usize) {
    let Some((raw, body_start)) = front_matter_span(text) else {
        return (HashMap::new(), None, 0);
    };
    let parsed = YamlLoader::load_from_str(raw);
    let mapping = match parsed.as_deref() {
        Ok([Yaml::Hash(hash)]) => hash,
        Ok(_) | Err(_) => {
            emitter.warn(
                pb::WarningCode::Metadata,
                "front matter is not a YAML mapping and was skipped",
            );
            return (HashMap::new(), None, body_start);
        }
    };
    let metadata: HashMap<String, pb::MetaValue> = mapping
        .iter()
        .map(|(key, value)| (yaml_key(key), meta_value(value)))
        .collect();
    let title = metadata.get("title").and_then(|value| match &value.kind {
        Some(pb::meta_value::Kind::Value(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    });
    (metadata, title, body_start)
}

/// The raw text between leading `---` fences, and where the body resumes.
fn front_matter_span(text: &str) -> Option<(&str, usize)> {
    let after_open = text
        .strip_prefix("---\n")
        .map(|rest| (rest, 4))
        .or_else(|| text.strip_prefix("---\r\n").map(|rest| (rest, 5)))?;
    let (rest, open_len) = after_open;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare == "---" || bare == "..." {
            return Some((&rest[..offset], open_len + offset + line.len()));
        }
        offset += line.len();
    }
    None
}

/// One front-matter value as the tree the source declared.
///
/// A scalar is verbatim text exactly as the author wrote it — a
/// `version: 1.10` stays the string "1.10" because the wire has no number
/// arm to mangle it with — a sequence is a `MetaList`, a mapping is a
/// `MetaMap` keyed by [`yaml_key`], and a null is the empty string. Nothing
/// is ever JSON-encoded.
fn meta_value(value: &Yaml) -> pb::MetaValue {
    use pb::meta_value::Kind;
    let kind = match value {
        Yaml::String(text) => Kind::Value(text.clone()),
        Yaml::Integer(number) => Kind::Value(number.to_string()),
        Yaml::Real(raw) => Kind::Value(raw.clone()),
        Yaml::Boolean(flag) => Kind::Value(flag.to_string()),
        Yaml::Array(items) => Kind::List(pb::MetaList {
            items: items.iter().map(meta_value).collect(),
        }),
        Yaml::Hash(hash) => Kind::Map(pb::MetaMap {
            entries: hash
                .iter()
                .map(|(key, nested)| (yaml_key(key), meta_value(nested)))
                .collect(),
        }),
        // An alias never survives yaml-rust2's loader and a BadValue never
        // reaches a well-formed mapping; both degrade like a null.
        Yaml::Null | Yaml::Alias(_) | Yaml::BadValue => Kind::Value(String::new()),
    };
    pb::MetaValue { kind: Some(kind) }
}

/// A YAML mapping key as a string, however the source spelled it. A key
/// that is itself a collection — legal YAML, unheard-of front matter —
/// falls back to the loader's debug rendering rather than earning a JSON
/// encoder.
fn yaml_key(key: &Yaml) -> String {
    match key {
        Yaml::String(text) => text.clone(),
        Yaml::Integer(number) => number.to_string(),
        Yaml::Real(raw) => raw.clone(),
        Yaml::Boolean(flag) => flag.to_string(),
        Yaml::Null => "null".to_owned(),
        other => format!("{other:?}"),
    }
}

// ------------------------------------------------------------------- driver

/// 1-based source lines from byte offsets.
struct LineIndex {
    /// Byte offset each line starts at, line 1 first.
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(at, _)| at + 1),
        );
        Self { starts }
    }

    fn line_of(&self, offset: usize) -> u32 {
        u32::try_from(self.starts.partition_point(|start| *start <= offset)).unwrap_or(u32::MAX)
    }
}

/// Inline text being flattened: entity decoding is pulldown-cmark's, runs of
/// whitespace collapse to one space here.
#[derive(Default)]
struct Flat {
    buf: String,
    pending_space: bool,
}

impl Flat {
    fn push(&mut self, text: &str) {
        for c in text.chars() {
            if c.is_whitespace() {
                self.pending_space = !self.buf.is_empty();
            } else {
                if self.pending_space {
                    self.buf.push(' ');
                    self.pending_space = false;
                }
                self.buf.push(c);
            }
        }
    }

    fn space(&mut self) {
        self.pending_space = !self.buf.is_empty();
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn take(&mut self) -> String {
        self.pending_space = false;
        std::mem::take(&mut self.buf)
    }
}

/// Whether a block is, in its entirety, one hyperlink.
#[derive(Default)]
struct LinkTrack {
    /// Destination of the first link seen.
    url: Option<String>,
    /// Links seen.
    count: usize,
    /// Depth of open links, so text inside one is not "outside".
    open: usize,
    /// Non-whitespace text seen outside any link.
    outside: bool,
}

impl LinkTrack {
    fn open(&mut self, url: &str) {
        if self.count == 0 {
            self.url = Some(url.to_owned());
        }
        self.count += 1;
        self.open += 1;
    }

    fn close(&mut self) {
        self.open = self.open.saturating_sub(1);
    }

    fn text(&mut self, text: &str) {
        if self.open == 0 && !text.trim().is_empty() {
            self.outside = true;
        }
    }

    fn whole_block_link(&self) -> Option<String> {
        if self.count == 1 && !self.outside {
            self.url.clone()
        } else {
            None
        }
    }
}

/// A block whose text is still being captured.
enum Capture {
    /// A paragraph, or the leading text of a loose list item.
    Para {
        line: u32,
        flat: Flat,
        link: LinkTrack,
    },
    /// A heading.
    Heading {
        level: u32,
        line: u32,
        flat: Flat,
        link: LinkTrack,
    },
    /// A code block; text is kept verbatim.
    Code {
        line: u32,
        language: Option<String>,
        buf: String,
    },
    /// A list item: its own inline text, in a tight or loose list.
    Item {
        line: u32,
        flat: Flat,
        link: LinkTrack,
        marker: Option<String>,
        depth: u32,
        enumerated: bool,
        role: String,
        emitted: bool,
    },
    /// An HTML block, captured raw and flattened at the end.
    HtmlBlock { line: u32, buf: String },
    /// An image; inner text is the alt text.
    Image {
        line: u32,
        src: String,
        title: String,
        flat: Flat,
    },
    /// One table cell.
    Cell { flat: Flat },
}

/// A pipe table being streamed.
struct TableState {
    in_head: bool,
    next_column: u32,
    cells: Vec<pb::TableCell>,
}

/// The event-loop state of one Markdown parse.
struct Walk<'w, 'e> {
    emitter: &'w mut Emitter<'e>,
    /// The parsed body, for reading list markers back from the source.
    body: &'w str,
    lines: LineIndex,
    /// Byte offset of the parsed body within the original text, so offsets
    /// from pulldown-cmark can be mapped to lines of the whole file.
    base: usize,
    title_taken: bool,
    captures: Vec<Capture>,
    /// Open lists, innermost last: `true` for ordered.
    lists: Vec<bool>,
    quote_depth: u32,
    /// Open footnote definitions, innermost label last.
    footnotes: Vec<String>,
    table: Option<TableState>,
}

impl Walk<'_, '_> {
    fn event(&mut self, event: Event<'_>, offset: usize) -> Result<(), ParseError> {
        let line = self.lines.line_of(self.base + offset);
        match event {
            Event::Start(tag) => self.start(tag, line, offset)?,
            Event::End(tag) => self.end(tag)?,
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                self.mark_outside_link();
                self.text(&code);
            }
            Event::InlineMath(math) => {
                self.mark_outside_link();
                self.text(&math);
            }
            Event::DisplayMath(math) => {
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Formula,
                    text: math.into_string(),
                    source_line: Some(line),
                    ..Block::default()
                })?;
            }
            Event::Html(html) => {
                if let Some(Capture::HtmlBlock { buf, .. }) = self.captures.last_mut() {
                    buf.push_str(&html);
                }
            }
            Event::InlineHtml(html) => {
                self.emitter.warn(
                    pb::WarningCode::EmbeddedHtmlFlattened,
                    "inline HTML was flattened to its text",
                );
                let stripped = strip_tags(&html);
                if !stripped.trim().is_empty() {
                    self.text(&stripped);
                }
            }
            Event::FootnoteReference(label) => self.text(&format!("[{label}]")),
            Event::SoftBreak | Event::HardBreak => {
                if let Some(flat) = self.top_flat() {
                    flat.space();
                }
            }
            Event::Rule => self
                .emitter
                .warn(pb::WarningCode::UnmappedConstruct, "thematic break"),
            Event::TaskListMarker(checked) => {
                if let Some(Capture::Item { role, .. }) = self
                    .captures
                    .iter_mut()
                    .rev()
                    .find(|capture| matches!(capture, Capture::Item { .. }))
                {
                    *role = if checked {
                        "task-item-checked".to_owned()
                    } else {
                        "task-item".to_owned()
                    };
                }
            }
        }
        Ok(())
    }

    fn start(&mut self, tag: Tag<'_>, line: u32, offset: usize) -> Result<(), ParseError> {
        match tag {
            Tag::Paragraph => {
                self.flush_started_item()?;
                self.captures.push(Capture::Para {
                    line,
                    flat: Flat::default(),
                    link: LinkTrack::default(),
                });
            }
            Tag::Heading { level, .. } => {
                self.captures.push(Capture::Heading {
                    level: level as u32,
                    line,
                    flat: Flat::default(),
                    link: LinkTrack::default(),
                });
            }
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|token| !token.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.captures.push(Capture::Code {
                    line,
                    language,
                    buf: String::new(),
                });
            }
            Tag::HtmlBlock => {
                self.captures.push(Capture::HtmlBlock {
                    line,
                    buf: String::new(),
                });
            }
            Tag::List(start) => {
                self.flush_started_item()?;
                self.lists.push(start.is_some());
            }
            Tag::Item => {
                let depth = u32::try_from(self.lists.len()).unwrap_or(u32::MAX);
                self.captures.push(Capture::Item {
                    line,
                    flat: Flat::default(),
                    link: LinkTrack::default(),
                    marker: self.marker_at(offset),
                    depth: depth.max(1),
                    enumerated: self.lists.last().copied().unwrap_or(false),
                    role: String::new(),
                    emitted: false,
                });
            }
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::FootnoteDefinition(label) => self.footnotes.push(label.into_string()),
            Tag::Table(_) => {
                self.emitter.table_start(None, Some(line))?;
                self.table = Some(TableState {
                    in_head: false,
                    next_column: 0,
                    cells: Vec::new(),
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => self.captures.push(Capture::Cell {
                flat: Flat::default(),
            }),
            Tag::Image {
                dest_url, title, ..
            } => {
                self.captures.push(Capture::Image {
                    line,
                    src: dest_url.into_string(),
                    title: title.into_string(),
                    flat: Flat::default(),
                });
            }
            Tag::Link { dest_url, .. } => {
                if let Some(link) = self.top_link() {
                    link.open(&dest_url);
                }
            }
            // Emphasis and its siblings shape rendering, not structure: the
            // text inside them flows into the enclosing block unchanged.
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough => {}
            // Not enabled in this build's option set; named so a future
            // enablement is a decision rather than an accident.
            Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
        Ok(())
    }

    fn end(&mut self, tag: TagEnd) -> Result<(), ParseError> {
        match tag {
            TagEnd::Paragraph => {
                let Some(Capture::Para { line, flat, link }) = self.captures.pop() else {
                    return Ok(());
                };
                self.finish_para(line, flat, link)?;
            }
            TagEnd::Heading(_) => {
                let Some(Capture::Heading {
                    level,
                    line,
                    mut flat,
                    link,
                }) = self.captures.pop()
                else {
                    return Ok(());
                };
                let text = flat.take();
                if text.is_empty() {
                    return Ok(());
                }
                let (label, level) = if level == 1 && !self.title_taken {
                    self.title_taken = true;
                    (pb::BlockLabel::Title, None)
                } else {
                    (pb::BlockLabel::SectionHeader, Some(level))
                };
                self.emitter.text_block(Block {
                    label,
                    text,
                    level,
                    hyperlink: link.whole_block_link(),
                    source_line: Some(line),
                    ..Block::default()
                })?;
            }
            TagEnd::CodeBlock => {
                let Some(Capture::Code {
                    line,
                    language,
                    buf,
                }) = self.captures.pop()
                else {
                    return Ok(());
                };
                let text = buf.strip_suffix('\n').unwrap_or(&buf).to_owned();
                if !text.is_empty() {
                    self.emitter.text_block(Block {
                        label: pb::BlockLabel::Code,
                        text,
                        language,
                        source_line: Some(line),
                        ..Block::default()
                    })?;
                }
            }
            TagEnd::HtmlBlock => {
                let Some(Capture::HtmlBlock { line, buf }) = self.captures.pop() else {
                    return Ok(());
                };
                self.emitter.warn(
                    pb::WarningCode::EmbeddedHtmlFlattened,
                    "an HTML block was flattened to its text",
                );
                let mut flat = Flat::default();
                flat.push(&strip_tags(&buf));
                let text = flat.take();
                if !text.is_empty() {
                    self.emitter.text_block(Block {
                        label: pb::BlockLabel::Paragraph,
                        role: "html".to_owned(),
                        text,
                        source_line: Some(line),
                        ..Block::default()
                    })?;
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                if let Some(Capture::Item { .. }) = self.captures.last() {
                    self.flush_started_item()?;
                    self.captures.pop();
                }
            }
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::FootnoteDefinition => {
                self.footnotes.pop();
            }
            TagEnd::Table => {
                if self.table.take().is_some() {
                    self.emitter.table_end()?;
                }
            }
            TagEnd::TableHead => self.finish_row(true)?,
            TagEnd::TableRow => self.finish_row(false)?,
            TagEnd::TableCell => {
                let Some(Capture::Cell { mut flat }) = self.captures.pop() else {
                    return Ok(());
                };
                if let Some(table) = self.table.as_mut() {
                    table.cells.push(pb::TableCell {
                        column_index: table.next_column,
                        text: flat.take(),
                        column_span: 1,
                        row_span: 1,
                        is_header: table.in_head,
                        ..pb::TableCell::default()
                    });
                    table.next_column += 1;
                }
            }
            TagEnd::Image => {
                let Some(Capture::Image {
                    line,
                    src,
                    title,
                    mut flat,
                }) = self.captures.pop()
                else {
                    return Ok(());
                };
                self.emitter.image(src, flat.take(), title, Some(line))?;
            }
            TagEnd::Link => {
                if let Some(link) = self.top_link() {
                    link.close();
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {}
            TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
        Ok(())
    }

    /// A finished paragraph: a list item's own text when it is the item's
    /// first, otherwise a block labelled by where it sits.
    fn finish_para(
        &mut self,
        line: u32,
        mut flat: Flat,
        link: LinkTrack,
    ) -> Result<(), ParseError> {
        if flat.is_empty() {
            return Ok(());
        }
        if let Some(Capture::Item {
            flat: item_flat,
            link: item_link,
            emitted: false,
            ..
        }) = self.captures.last_mut()
            && item_flat.is_empty()
        {
            *item_flat = flat;
            *item_link = link;
            return Ok(());
        }
        let label = if !self.footnotes.is_empty() {
            pb::BlockLabel::Footnote
        } else if self.quote_depth > 0 {
            pb::BlockLabel::Quote
        } else {
            pb::BlockLabel::Paragraph
        };
        self.emitter.text_block(Block {
            label,
            role: self.footnotes.last().cloned().unwrap_or_default(),
            text: flat.take(),
            hyperlink: link.whole_block_link(),
            source_line: Some(line),
            ..Block::default()
        })
    }

    /// Emit the pending text of the innermost open list item, so the item
    /// precedes whatever interrupted it — a nested list, a further
    /// paragraph, or its own end.
    fn flush_started_item(&mut self) -> Result<(), ParseError> {
        let Some(Capture::Item {
            line,
            flat,
            link,
            marker,
            depth,
            enumerated,
            role,
            emitted,
        }) = self.captures.last_mut()
        else {
            return Ok(());
        };
        if *emitted || flat.is_empty() {
            return Ok(());
        }
        *emitted = true;
        let block = Block {
            label: pb::BlockLabel::ListItem,
            role: std::mem::take(role),
            text: flat.take(),
            enumerated: *enumerated,
            marker: marker.clone(),
            list_depth: Some(*depth),
            hyperlink: link.whole_block_link(),
            source_line: Some(*line),
            ..Block::default()
        };
        self.emitter.text_block(block)
    }

    /// Close the table row being collected, header or body.
    fn finish_row(&mut self, is_header: bool) -> Result<(), ParseError> {
        let Some(table) = self.table.as_mut() else {
            return Ok(());
        };
        let cells = std::mem::take(&mut table.cells);
        table.next_column = 0;
        if cells.is_empty() {
            return Ok(());
        }
        self.emitter.table_row(is_header, cells)
    }

    /// Route inline text to the innermost capture; text outside any block
    /// container has nowhere to belong and is dropped, which pulldown-cmark
    /// never actually produces.
    fn text(&mut self, text: &str) {
        match self.captures.last_mut() {
            Some(
                Capture::Para { flat, link, .. }
                | Capture::Heading { flat, link, .. }
                | Capture::Item { flat, link, .. },
            ) => {
                link.text(text);
                flat.push(text);
            }
            Some(Capture::Image { flat, .. } | Capture::Cell { flat }) => flat.push(text),
            Some(Capture::Code { buf, .. } | Capture::HtmlBlock { buf, .. }) => {
                buf.push_str(text);
            }
            None => {}
        }
    }

    /// The flattener of the innermost capture, for word breaks.
    fn top_flat(&mut self) -> Option<&mut Flat> {
        self.captures
            .iter_mut()
            .rev()
            .find_map(|capture| match capture {
                Capture::Para { flat, .. }
                | Capture::Heading { flat, .. }
                | Capture::Item { flat, .. }
                | Capture::Image { flat, .. }
                | Capture::Cell { flat } => Some(flat),
                Capture::Code { .. } | Capture::HtmlBlock { .. } => None,
            })
    }

    /// The link tracker of the innermost linkable capture.
    fn top_link(&mut self) -> Option<&mut LinkTrack> {
        self.captures
            .iter_mut()
            .rev()
            .find_map(|capture| match capture {
                Capture::Para { link, .. }
                | Capture::Heading { link, .. }
                | Capture::Item { link, .. } => Some(link),
                _ => None,
            })
    }

    /// Non-link inline content inside a link-tracking capture: the block is
    /// more than one link even if the extra content is not plain text.
    fn mark_outside_link(&mut self) {
        if let Some(link) = self.top_link()
            && link.open == 0
        {
            link.outside = true;
        }
    }

    /// The list marker as the source wrote it, read straight from the bytes
    /// at the item's offset: `-`, `*`, `+`, or digits closed by `.` or `)`.
    fn marker_at(&self, offset: usize) -> Option<String> {
        let text = self.body.get(offset..)?;
        let first = text.chars().next()?;
        if matches!(first, '-' | '*' | '+') {
            return Some(first.to_string());
        }
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && matches!(text[digits..].chars().next(), Some('.' | ')')) {
            return Some(text[..=digits].to_owned());
        }
        None
    }
}

/// Raw HTML with its tags removed: everything outside `<...>` spans, with
/// comments and the tags themselves dropped. Deliberately not an HTML
/// parser — that is the HTML fold's job in a later build; this is only the
/// "keep the human-visible text" half of flattening.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(comment) = after.strip_prefix("<!--") {
            match comment.find("-->") {
                Some(end) => rest = &comment[end + 3..],
                None => return out,
            }
        } else {
            match after.find('>') {
                Some(end) => rest = &after[end + 1..],
                None => return out,
            }
        }
    }
    out.push_str(rest);
    decode_basic_entities(&out)
}

/// The five character entities pulldown-cmark leaves in raw HTML text.
fn decode_basic_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
