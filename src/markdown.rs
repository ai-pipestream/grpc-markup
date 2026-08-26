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
//!   collapsed; a paragraph that is entirely one link carries `hyperlink`,
//!   and every formatted, linked or referring run inside it leaves an
//!   `InlineSpan` over the text it covers.
//! - List items carry `enumerated`, the marker as written in the source, and
//!   the nesting depth. A task-list item records its checkbox in
//!   `task_state`.
//! - Fenced and indented code keeps its text verbatim; the info string's
//!   first word is the `language`.
//! - Blockquote paragraphs are `BLOCK_LABEL_QUOTE`, one block per paragraph,
//!   nesting flattened.
//! - Pipe tables stream as `TableStart` / `TableRow` / `TableEnd` with the
//!   header row flagged.
//! - Images become `ImageBlock` events with `src`, `alt` and `title` as
//!   written; nothing is fetched.
//! - Footnote definitions emit `BLOCK_LABEL_FOOTNOTE` blocks whose `role`
//!   and `anchor_id` are the footnote label; a reference to one is a span
//!   over the label text carrying the key it points at.
//! - Definition lists map onto the same `definition-term` /
//!   `definition-description` role vocabulary the HTML fold uses, and a GFM
//!   alert blockquote carries its `admonition` kind.
//! - A pipe table's delimiter row becomes `TableStart.columns`, one spec
//!   per column, and a heading attribute block becomes the block's
//!   `anchor_id`.
//! - Display math is `BLOCK_LABEL_FORMULA`, verbatim.
//! - Raw HTML — block or inline — is flattened to its text with
//!   `WARNING_CODE_EMBEDDED_HTML_FLATTENED`; this collector never parses
//!   HTML (that is the HTML fold's job, in a later build).
//!
//! `source_line` on every block comes from pulldown-cmark's offset iterator,
//! mapped to 1-based lines of the original text, front matter included.

use std::collections::HashMap;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, Options, Parser, Tag, TagEnd,
};
use yaml_rust2::{Yaml, YamlLoader};

use crate::parse::{Block, Emitter, Image, ParseError, Table};
use crate::proto::v1 as pb;
use crate::spans::{Inline, Mark};

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
    // Four extensions whose events this fold now maps rather than
    // discarding: definition lists onto the same role vocabulary the HTML
    // fold uses, sub/superscript onto span styles, heading attributes onto
    // `anchor_id`, and GFM alerts onto `admonition`.
    options.insert(Options::ENABLE_DEFINITION_LIST);
    options.insert(Options::ENABLE_SUPERSCRIPT);
    options.insert(Options::ENABLE_SUBSCRIPT);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options.insert(Options::ENABLE_GFM);

    let mut walk = Walk {
        emitter,
        body,
        lines: LineIndex::new(text),
        base: body_start,
        title_taken: false,
        captures: Vec::new(),
        lists: Vec::new(),
        quote_depth: 0,
        alerts: Vec::new(),
        definitions: 0,
        footnotes: Vec::new(),
        table: None,
    };
    for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
        walk.event(event, range.start)?;
    }
    Ok(())
}

// ------------------------------------------------------------- front matter

/// Parse a leading front matter block, when there is one.
///
/// Returns the metadata map, the title it declared, and the byte offset the
/// Markdown body starts at. The fence decides the dialect: `---` is YAML
/// and `+++` is TOML, which is what the generators that write front matter
/// mean by each. A block that is fenced but does not parse as its own
/// dialect warns and is skipped; text without a closed leading fence is all
/// body.
fn front_matter(
    text: &str,
    emitter: &mut Emitter<'_>,
) -> (HashMap<String, pb::MetaValue>, Option<String>, usize) {
    let Some((raw, body_start, dialect)) = front_matter_span(text) else {
        return (HashMap::new(), None, 0);
    };
    let Some(metadata) = (match dialect {
        Dialect::Yaml => yaml_mapping(raw),
        Dialect::Toml => crate::toml::parse(raw),
    }) else {
        emitter.warn(
            pb::WarningCode::Metadata,
            &format!("front matter is not a {dialect} mapping and was skipped"),
        );
        return (HashMap::new(), None, body_start);
    };
    let title = metadata.get("title").and_then(|value| match &value.kind {
        Some(pb::meta_value::Kind::Value(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    });
    (metadata, title, body_start)
}

/// Which dialect a front-matter fence declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// The `---` fence.
    Yaml,
    /// The `+++` fence.
    Toml,
}

impl Dialect {
    /// The fence that opens and closes this dialect's block.
    const fn fence(self) -> &'static str {
        match self {
            Self::Yaml => "---",
            Self::Toml => "+++",
        }
    }
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Yaml => "YAML",
            Self::Toml => "TOML",
        })
    }
}

/// A YAML front-matter block as the metadata tree, or nothing when it is
/// not a mapping.
fn yaml_mapping(raw: &str) -> Option<HashMap<String, pb::MetaValue>> {
    match YamlLoader::load_from_str(raw).as_deref() {
        Ok([Yaml::Hash(hash)]) => Some(
            hash.iter()
                .map(|(key, value)| (yaml_key(key), meta_value(value)))
                .collect(),
        ),
        Ok(_) | Err(_) => None,
    }
}

/// The raw text between leading fences, where the body resumes, and which
/// dialect the fence declared.
fn front_matter_span(text: &str) -> Option<(&str, usize, Dialect)> {
    let dialect = [Dialect::Yaml, Dialect::Toml]
        .into_iter()
        .find(|dialect| text.starts_with(dialect.fence()))?;
    let fence = dialect.fence();
    let (rest, open_len) = text
        .strip_prefix(&format!("{fence}\n"))
        .map(|rest| (rest, fence.len() + 1))
        .or_else(|| {
            text.strip_prefix(&format!("{fence}\r\n"))
                .map(|rest| (rest, fence.len() + 2))
        })?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        // YAML's own end-of-document marker closes a YAML block too.
        if bare == fence || (dialect == Dialect::Yaml && bare == "...") {
            return Some((&rest[..offset], open_len + offset + line.len(), dialect));
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

/// A block whose text is still being captured.
enum Capture {
    /// A paragraph, or the leading text of a loose list item.
    Para { line: u32, inline: Inline },
    /// A heading, with the anchor its attribute block declares.
    Heading {
        level: u32,
        line: u32,
        anchor: Option<String>,
        inline: Inline,
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
        inline: Inline,
        marker: Option<String>,
        depth: u32,
        enumerated: bool,
        task: Option<pb::TaskState>,
        emitted: bool,
    },
    /// One term or description of a definition list.
    Definition {
        line: u32,
        role: &'static str,
        depth: u32,
        inline: Inline,
    },
    /// An HTML block, captured raw and flattened at the end.
    HtmlBlock { line: u32, buf: String },
    /// An image; inner text is the alt text.
    Image {
        line: u32,
        src: String,
        title: String,
        inline: Inline,
    },
    /// One table cell.
    Cell { inline: Inline },
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
    /// Open block quotes, innermost last, each with the GFM alert kind it
    /// declares. A paragraph takes the innermost kind it sits inside.
    alerts: Vec<Option<pb::AdmonitionKind>>,
    /// Open definition lists, for the nesting depth of their items.
    definitions: u32,
    /// Open footnote definitions, innermost label last.
    footnotes: Vec<String>,
    table: Option<TableState>,
}

impl Walk<'_, '_> {
    /// Send one block, stamping how deep inside a blockquote it sat.
    ///
    /// Every block this fold sends goes through here, because a quotation
    /// contains whatever the author put in it: a paragraph, a list, a code
    /// block. The depth is what lets the Document fold rebuild the nesting
    /// the flattening removed.
    fn emit(&mut self, mut block: Block) -> Result<(), ParseError> {
        if self.quote_depth > 0 {
            block.quote_depth = Some(self.quote_depth);
        }
        self.emitter.text_block(block)
    }

    fn event(&mut self, event: Event<'_>, offset: usize) -> Result<(), ParseError> {
        let line = self.lines.line_of(self.base + offset);
        match event {
            Event::Start(tag) => self.start(tag, line, offset)?,
            Event::End(tag) => self.end(tag)?,
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.marked(&code, Mark::style(pb::SpanStyle::Code)),
            Event::InlineMath(math) => {
                // The delimiters are gone from the text; `raw` is what a
                // consumer needs to tell `$x$` from a currency amount.
                let mark = Mark::style(pb::SpanStyle::Math).with_raw(&math);
                self.marked(&math, mark);
            }
            Event::DisplayMath(math) => {
                self.emit(Block {
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
            Event::FootnoteReference(label) => {
                // The marker is the label itself, not a bracketed string
                // nothing can parse back: the span says what it points at.
                let mark = Mark::reference(&label, pb::ReferenceKind::Footnote)
                    .with_style(pb::SpanStyle::Superscript);
                self.marked(&label, mark);
            }
            Event::SoftBreak => {
                if let Some(inline) = self.top_inline() {
                    inline.space();
                }
            }
            // A hard break is a line break the author demanded, not
            // whitespace the source happens to contain, so it survives
            // collapsing as the newline it is.
            Event::HardBreak => {
                if let Some(inline) = self.top_inline() {
                    inline.push_verbatim("\n");
                }
            }
            Event::Rule => self
                .emitter
                .warn(pb::WarningCode::UnmappedConstruct, "thematic break"),
            Event::TaskListMarker(checked) => {
                if let Some(Capture::Item { task, .. }) = self
                    .captures
                    .iter_mut()
                    .rev()
                    .find(|capture| matches!(capture, Capture::Item { .. }))
                {
                    *task = Some(if checked {
                        pb::TaskState::Checked
                    } else {
                        pb::TaskState::Unchecked
                    });
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
                    inline: Inline::default(),
                });
            }
            Tag::Heading { level, id, .. } => {
                self.captures.push(Capture::Heading {
                    level: level as u32,
                    line,
                    anchor: id.map(|id| id.into_string()).filter(|id| !id.is_empty()),
                    inline: Inline::default(),
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
                    inline: Inline::default(),
                    marker: self.marker_at(offset),
                    depth: depth.max(1),
                    enumerated: self.lists.last().copied().unwrap_or(false),
                    task: None,
                    emitted: false,
                });
            }
            Tag::BlockQuote(kind) => {
                self.quote_depth += 1;
                self.alerts.push(kind.map(alert_kind));
            }
            Tag::FootnoteDefinition(label) => self.footnotes.push(label.into_string()),
            Tag::Table(alignments) => {
                let columns = alignments.iter().copied().map(column_spec).collect();
                self.emitter.table_start(Table {
                    source_line: Some(line),
                    columns,
                    ..Table::default()
                })?;
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
                inline: Inline::default(),
            }),
            Tag::Image {
                dest_url, title, ..
            } => {
                self.captures.push(Capture::Image {
                    line,
                    src: dest_url.into_string(),
                    title: title.into_string(),
                    inline: Inline::default(),
                });
            }
            Tag::Link {
                dest_url, title, ..
            } => {
                if let Some(inline) = self.top_inline() {
                    inline.open_link(&dest_url, Some(&title));
                }
            }
            // Emphasis and its siblings shape the run they cover: the text
            // flows into the enclosing block and the run is recorded.
            Tag::Emphasis => self.open(Mark::style(pb::SpanStyle::Italic)),
            Tag::Strong => self.open(Mark::style(pb::SpanStyle::Bold)),
            Tag::Strikethrough => self.open(Mark::style(pb::SpanStyle::Strikethrough)),
            Tag::Superscript => self.open(Mark::style(pb::SpanStyle::Superscript)),
            Tag::Subscript => self.open(Mark::style(pb::SpanStyle::Subscript)),
            Tag::DefinitionList => {
                self.flush_started_item()?;
                self.definitions += 1;
            }
            Tag::DefinitionListTitle => self.captures.push(Capture::Definition {
                line,
                role: "definition-term",
                depth: self.definitions.max(1),
                inline: Inline::default(),
            }),
            Tag::DefinitionListDefinition => self.captures.push(Capture::Definition {
                line,
                role: "definition-description",
                // A description sits one level under its term, the same
                // shape the HTML fold gives `dt` and `dd`.
                depth: self.definitions.max(1) + 1,
                inline: Inline::default(),
            }),
            // Front matter is hand-scanned before the parser runs, so a
            // metadata block reaching here is one that is not leading.
            Tag::MetadataBlock(_) => {}
        }
        Ok(())
    }

    fn end(&mut self, tag: TagEnd) -> Result<(), ParseError> {
        match tag {
            TagEnd::Paragraph => {
                let Some(Capture::Para { line, inline }) = self.captures.pop() else {
                    return Ok(());
                };
                self.finish_para(line, inline)?;
            }
            TagEnd::Heading(_) => {
                let Some(Capture::Heading {
                    level,
                    line,
                    anchor,
                    mut inline,
                }) = self.captures.pop()
                else {
                    return Ok(());
                };
                let hyperlink = inline.hyperlink();
                let (text, spans) = inline.take();
                if text.is_empty() {
                    return Ok(());
                }
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
                    source_line: Some(line),
                    spans,
                    anchor_id: anchor,
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
                    self.emit(Block {
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
                let mut inline = Inline::default();
                inline.push(&strip_tags(&buf));
                let (text, _) = inline.take();
                if !text.is_empty() {
                    self.emit(Block {
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
            TagEnd::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.alerts.pop();
            }
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
                let Some(Capture::Cell { mut inline }) = self.captures.pop() else {
                    return Ok(());
                };
                let (text, spans) = inline.take();
                if let Some(table) = self.table.as_mut() {
                    table.cells.push(pb::TableCell {
                        column_index: table.next_column,
                        text,
                        column_span: 1,
                        row_span: 1,
                        is_header: table.in_head,
                        spans,
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
                    mut inline,
                }) = self.captures.pop()
                else {
                    return Ok(());
                };
                let (alt, _) = inline.take();
                self.emitter.image(Image {
                    src,
                    alt,
                    title,
                    source_line: Some(line),
                    ..Image::default()
                })?;
            }
            TagEnd::Link => {
                if let Some(inline) = self.top_inline() {
                    inline.close_link();
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => self.close(),
            TagEnd::DefinitionList => {
                self.definitions = self.definitions.saturating_sub(1);
            }
            TagEnd::DefinitionListTitle | TagEnd::DefinitionListDefinition => {
                let Some(Capture::Definition {
                    line,
                    role,
                    depth,
                    mut inline,
                }) = self.captures.pop()
                else {
                    return Ok(());
                };
                let hyperlink = inline.hyperlink();
                let (text, spans) = inline.take();
                if text.is_empty() {
                    return Ok(());
                }
                self.emit(Block {
                    label: pb::BlockLabel::ListItem,
                    role: role.to_owned(),
                    text,
                    list_depth: Some(depth),
                    hyperlink,
                    source_line: Some(line),
                    spans,
                    ..Block::default()
                })?;
            }
            TagEnd::MetadataBlock(_) => {}
        }
        Ok(())
    }

    /// A finished paragraph: the text of the item or definition it opens,
    /// when it is that container's first, otherwise a block labelled by
    /// where it sits.
    fn finish_para(&mut self, line: u32, mut inline: Inline) -> Result<(), ParseError> {
        if inline.is_empty() {
            return Ok(());
        }
        match self.captures.last_mut() {
            Some(Capture::Item {
                inline: held,
                emitted: false,
                ..
            })
            | Some(Capture::Definition { inline: held, .. })
                if held.is_empty() =>
            {
                *held = inline;
                return Ok(());
            }
            _ => {}
        }
        let alert = self.alerts.iter().rev().find_map(|kind| *kind);
        let label = if !self.footnotes.is_empty() {
            pb::BlockLabel::Footnote
        } else if self.quote_depth > 0 {
            pb::BlockLabel::Quote
        } else {
            pb::BlockLabel::Paragraph
        };
        // A footnote body is the anchor its markers point at; the label the
        // source wrote is both its role and its anchor.
        let key = self.footnotes.last().cloned();
        let hyperlink = inline.hyperlink();
        let (text, spans) = inline.take();
        self.emit(Block {
            label,
            // A GFM alert is an admonition first; the role is the same one
            // the AsciiDoc fold writes, so both formats land together.
            role: alert.map_or_else(
                || key.clone().unwrap_or_default(),
                |_| "admonition".to_owned(),
            ),
            text,
            hyperlink,
            source_line: Some(line),
            spans,
            anchor_id: key,
            admonition: alert,
            ..Block::default()
        })
    }

    /// Emit the pending text of the innermost open list item, so the item
    /// precedes whatever interrupted it — a nested list, a further
    /// paragraph, or its own end.
    fn flush_started_item(&mut self) -> Result<(), ParseError> {
        let Some(Capture::Item {
            line,
            inline,
            marker,
            depth,
            enumerated,
            task,
            emitted,
        }) = self.captures.last_mut()
        else {
            return Ok(());
        };
        if *emitted || inline.is_empty() {
            return Ok(());
        }
        *emitted = true;
        let hyperlink = inline.hyperlink();
        let (text, spans) = inline.take();
        let block = Block {
            label: pb::BlockLabel::ListItem,
            text,
            enumerated: *enumerated,
            marker: marker.clone(),
            list_depth: Some(*depth),
            hyperlink,
            source_line: Some(*line),
            spans,
            task_state: task.take(),
            ..Block::default()
        };
        self.emit(block)
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
            Some(Capture::Code { buf, .. } | Capture::HtmlBlock { buf, .. }) => {
                buf.push_str(text);
            }
            _ => {
                if let Some(inline) = self.top_inline() {
                    inline.push(text);
                }
            }
        }
    }

    /// Push one run that arrives as a single unit: an inline code span, an
    /// inline formula, a footnote marker.
    fn marked(&mut self, text: &str, mark: Mark) {
        if let Some(Capture::Code { buf, .. } | Capture::HtmlBlock { buf, .. }) =
            self.captures.last_mut()
        {
            buf.push_str(text);
            return;
        }
        if let Some(inline) = self.top_inline() {
            inline.push_marked(text, mark);
        }
    }

    /// Open a run on the innermost capture that has a flattener.
    fn open(&mut self, mark: Mark) {
        if let Some(inline) = self.top_inline() {
            inline.open(mark);
        }
    }

    /// Close the innermost open run, pairing with [`Walk::open`].
    fn close(&mut self) {
        if let Some(inline) = self.top_inline() {
            inline.close();
        }
    }

    /// The flattener of the innermost capture that has one.
    fn top_inline(&mut self) -> Option<&mut Inline> {
        self.captures
            .iter_mut()
            .rev()
            .find_map(|capture| match capture {
                Capture::Para { inline, .. }
                | Capture::Heading { inline, .. }
                | Capture::Item { inline, .. }
                | Capture::Definition { inline, .. }
                | Capture::Image { inline, .. }
                | Capture::Cell { inline } => Some(inline),
                Capture::Code { .. } | Capture::HtmlBlock { .. } => None,
            })
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

/// One pulldown-cmark column alignment as the column geometry the wire
/// carries. A table whose delimiter row declares nothing still sends a spec
/// per column, so a consumer can index `columns` by column number.
const fn column_spec(alignment: Alignment) -> pb::ColumnSpec {
    let align = match alignment {
        Alignment::None => pb::Alignment::Unspecified,
        Alignment::Left => pb::Alignment::Left,
        Alignment::Center => pb::Alignment::Center,
        Alignment::Right => pb::Alignment::Right,
    };
    pb::ColumnSpec {
        align: align as i32,
        valign: pb::Alignment::Unspecified as i32,
        width: None,
    }
}

/// One GFM alert kind as the wire's admonition vocabulary.
const fn alert_kind(kind: BlockQuoteKind) -> pb::AdmonitionKind {
    match kind {
        BlockQuoteKind::Note => pb::AdmonitionKind::Note,
        BlockQuoteKind::Tip => pb::AdmonitionKind::Tip,
        BlockQuoteKind::Important => pb::AdmonitionKind::Important,
        BlockQuoteKind::Warning => pb::AdmonitionKind::Warning,
        BlockQuoteKind::Caution => pb::AdmonitionKind::Caution,
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
