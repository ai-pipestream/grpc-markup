// SPDX-License-Identifier: Apache-2.0

//! The BoxNote fold: the JSON note format Box has exported since August
//! 2022 — a ProseMirror-style `doc` node tree — mapped onto the shared
//! block vocabulary, node for node.
//!
//! Shape of the mapping:
//!
//! - `heading` collapses its inline runs; the first level-1 heading is the
//!   title, deeper ones are section headers at their level.
//! - `paragraph` flattens to one block; a paragraph that is entirely one
//!   link (of a safe scheme) carries `hyperlink`, and every mark on every
//!   run leaves an `InlineSpan`: bold, italic, underline, strikethrough,
//!   code, highlight, sub and superscript as styles, text colour, font
//!   family and font size as the run's own typography, and each link as a
//!   link run whether or not the block holds anything else.
//! - `bullet_list` / `ordered_list` / `check_list` yield one
//!   `BLOCK_LABEL_LIST_ITEM` per item, `enumerated` for ordered lists,
//!   nesting depth tracked; BoxNote writes no markers, so `marker` stays
//!   absent. Check items record their checkbox in `role`.
//! - `code_block` keeps its text verbatim.
//! - `table` streams `TableStart` / `TableRow` / `TableEnd`; `rowspan` and
//!   `colspan` advance the column index exactly as the source's grid does,
//!   and `table_header` cells are flagged.
//! - `image` becomes an `ImageBlock` from the node's own attributes; the
//!   bytes live in Box, not in the note, and are never fetched.
//! - `blockquote` paragraphs are `BLOCK_LABEL_QUOTE`; `call_out_box` and
//!   other wrappers keep their inner blocks rather than dropping the text.
//! - A node type with no mapping and no children to recurse into gets one
//!   aggregated `WARNING_CODE_UNMAPPED_CONSTRUCT` naming it.
//!
//! Notes saved before the August 2022 release use the legacy `atext`/`pool`
//! model and are refused as `INVALID_ARGUMENT` rather than silently
//! mis-parsed.

use serde_json::Value;

use crate::parse::{Block, Emitter, Image, ParseError, Table};
use crate::proto::v1 as pb;
use crate::spans::{Inline, Mark};
use crate::table_spans::{Asked, SpanTracker};

/// Link schemes kept as hyperlinks. A stray `javascript:` or a bare string
/// is dropped rather than mis-read as a link.
const SAFE_LINK_SCHEMES: [&str; 3] = ["http://", "https://", "mailto:"];

/// Map one Box Note.
///
/// # Errors
///
/// [`ParseError::Invalid`] when the bytes are not JSON, not a note (no
/// `doc` tree), or a legacy `atext` note; [`ParseError::ConsumerGone`] when
/// the client stopped reading.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|e| ParseError::Invalid(format!("not a Box Note: invalid JSON: {e}")))?;
    let Some(root) = value.as_object() else {
        return Err(ParseError::Invalid(
            "not a Box Note: the payload is not a JSON object".to_owned(),
        ));
    };
    let doc = root.get("doc").and_then(Value::as_object);
    if doc.is_none() {
        return Err(if root.contains_key("atext") {
            ParseError::Invalid(
                "legacy Box Note (the pre-August-2022 atext/pool format) is not supported; \
                 only the current doc-tree format is mapped"
                    .to_owned(),
            )
        } else {
            ParseError::Invalid("not a Box Note: the JSON object has no `doc` node tree".to_owned())
        });
    }
    let doc = doc.expect("checked just above");

    // Some exports carry the note's display name beside the tree.
    let title = root
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    emitter.info(title, std::collections::HashMap::new())?;

    let mut walk = Walk {
        emitter,
        title_taken: false,
        quote_depth: 0,
    };
    walk.blocks(content(doc), 0)
}

/// The `content` array of a node, empty when absent.
fn content(node: &serde_json::Map<String, Value>) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// One block's flattened text, the runs inside it, and its hyperlink when
/// the whole block is one link.
type Collapsed = (String, Vec<pb::InlineSpan>, Option<String>);

/// The walk state of one note.
struct Walk<'w, 'e> {
    emitter: &'w mut Emitter<'e>,
    title_taken: bool,
    quote_depth: u32,
}

impl Walk<'_, '_> {
    /// Send one block, stamping how deep inside a `blockquote` it sat.
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

    /// Map a run of sibling block nodes. `list_depth` is how many lists are
    /// open above them, 0 outside any list.
    fn blocks(&mut self, nodes: &[Value], list_depth: u32) -> Result<(), ParseError> {
        for node in nodes {
            self.block(node, list_depth)?;
        }
        Ok(())
    }

    fn block(&mut self, node: &Value, list_depth: u32) -> Result<(), ParseError> {
        let Some(node) = node.as_object() else {
            return Ok(());
        };
        let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
        match node_type {
            "heading" => self.heading(node),
            "paragraph" => self.paragraph(node),
            "bullet_list" | "check_list" => self.list(node, false, list_depth),
            "ordered_list" => self.list(node, true, list_depth),
            "code_block" => self.code(node),
            "table" => self.table(node),
            "image" => self.image(node),
            "blockquote" => {
                self.quote_depth += 1;
                let result = self.blocks(content(node), list_depth);
                self.quote_depth -= 1;
                result
            }
            _ => {
                let children = content(node);
                if children.is_empty() {
                    // Nothing inside to save; say what was dropped.
                    self.emitter.warn(
                        pb::WarningCode::UnmappedConstruct,
                        &format!("BoxNote node type `{node_type}` has no mapping"),
                    );
                    Ok(())
                } else {
                    // call_out_box and other wrappers: keep their inner
                    // blocks rather than dropping the text.
                    self.blocks(children, list_depth)
                }
            }
        }
    }

    fn heading(&mut self, node: &serde_json::Map<String, Value>) -> Result<(), ParseError> {
        let (text, spans, hyperlink) = collapse(content(node));
        if text.is_empty() {
            return Ok(());
        }
        let level = node
            .get("attrs")
            .and_then(|attrs| attrs.get("level"))
            .and_then(Value::as_u64)
            .and_then(|level| u32::try_from(level).ok())
            .filter(|level| *level >= 1)
            .unwrap_or(1);
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
            ..Block::default()
        })
    }

    fn paragraph(&mut self, node: &serde_json::Map<String, Value>) -> Result<(), ParseError> {
        let (text, spans, hyperlink) = collapse(content(node));
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
            text,
            hyperlink,
            spans,
            ..Block::default()
        })
    }

    fn list(
        &mut self,
        node: &serde_json::Map<String, Value>,
        enumerated: bool,
        list_depth: u32,
    ) -> Result<(), ParseError> {
        let depth = list_depth + 1;
        for item in content(node) {
            let Some(item) = item.as_object() else {
                continue;
            };
            let checked = match item.get("type").and_then(Value::as_str) {
                Some("check_list_item") => Some(
                    item.get("attrs")
                        .and_then(|attrs| attrs.get("checked"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
                _ => None,
            };
            self.list_item(item, enumerated, depth, checked)?;
        }
        Ok(())
    }

    /// One list item: its leading paragraph is the item's own text, and
    /// everything after it — nested lists, further paragraphs — follows as
    /// its own blocks.
    fn list_item(
        &mut self,
        item: &serde_json::Map<String, Value>,
        enumerated: bool,
        depth: u32,
        checked: Option<bool>,
    ) -> Result<(), ParseError> {
        let mut nested: Vec<&Value> = Vec::new();
        let mut text = String::new();
        let mut spans = Vec::new();
        let mut hyperlink = None;
        for child in content(item) {
            let is_leading_paragraph =
                text.is_empty() && child.get("type").and_then(Value::as_str) == Some("paragraph");
            if is_leading_paragraph {
                let child_content = child.as_object().map_or(&[][..], |node| content(node));
                (text, spans, hyperlink) = collapse(child_content);
            } else {
                nested.push(child);
            }
        }
        if !text.is_empty() {
            self.emit(Block {
                label: pb::BlockLabel::ListItem,
                task_state: checked.map(|ticked| {
                    if ticked {
                        pb::TaskState::Checked
                    } else {
                        pb::TaskState::Unchecked
                    }
                }),
                text,
                enumerated,
                // BoxNote leaves numbering implicit, so there is no marker
                // "as written" to carry.
                marker: None,
                list_depth: Some(depth),
                hyperlink,
                spans,
                ..Block::default()
            })?;
        }
        for child in nested {
            self.block(child, depth)?;
        }
        Ok(())
    }

    fn code(&mut self, node: &serde_json::Map<String, Value>) -> Result<(), ParseError> {
        let text = plain_text(content(node));
        if text.is_empty() {
            return Ok(());
        }
        let language = node
            .get("attrs")
            .and_then(|attrs| attrs.get("language"))
            .and_then(Value::as_str)
            .filter(|language| !language.is_empty())
            .map(str::to_owned);
        self.emit(Block {
            label: pb::BlockLabel::Code,
            text,
            language,
            ..Block::default()
        })
    }

    /// One table, streamed row by row with the source's own span geometry:
    /// a cell's column index advances past the columns that cells above it
    /// still span into.
    fn table(&mut self, node: &serde_json::Map<String, Value>) -> Result<(), ParseError> {
        let rows: Vec<&serde_json::Map<String, Value>> = content(node)
            .iter()
            .filter_map(Value::as_object)
            .filter(|row| row.get("type").and_then(Value::as_str) == Some("table_row"))
            .collect();
        if rows.is_empty() {
            return Ok(());
        }
        self.emitter.table_start(Table::default())?;
        // Spans are placed as lanes and clamped to the table model, so a
        // cell claiming the whole plane costs one lane rather than every
        // position it claims.
        let mut tracker = SpanTracker::new(rows.len());
        for (row_index, row) in rows.iter().enumerate() {
            let row_index = u32::try_from(row_index).unwrap_or(u32::MAX);
            tracker.begin_row(row_index);
            let mut cells = Vec::new();
            let mut all_headers = true;
            for cell in content(row) {
                let Some(cell) = cell.as_object() else {
                    continue;
                };
                let cell_type = cell.get("type").and_then(Value::as_str).unwrap_or("");
                if cell_type != "table_cell" && cell_type != "table_header" {
                    continue;
                }
                let span = |name: &str| {
                    cell.get("attrs")
                        .and_then(|attrs| attrs.get(name))
                        .and_then(Value::as_u64)
                        .map(|span| u32::try_from(span).unwrap_or(u32::MAX))
                        .unwrap_or(1)
                };
                let placed = tracker.place(Asked {
                    column_span: span("colspan"),
                    row_span: span("rowspan"),
                });
                let is_header = cell_type == "table_header";
                all_headers &= is_header;
                let (text, spans) = cell_text(content(cell));
                cells.push(pb::TableCell {
                    column_index: placed.column,
                    text,
                    column_span: placed.column_span,
                    row_span: placed.row_span,
                    is_header,
                    spans,
                    ..pb::TableCell::default()
                });
            }
            tracker.end_row();
            if cells.is_empty() {
                continue;
            }
            let is_header_row = all_headers;
            self.emitter.table_row(is_header_row, cells)?;
        }
        self.emitter.table_end()
    }

    fn image(&mut self, node: &serde_json::Map<String, Value>) -> Result<(), ParseError> {
        let attrs = node.get("attrs").and_then(Value::as_object);
        let attr = |name: &str| {
            attrs
                .and_then(|attrs| attrs.get(name))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        // The note stores a pointer into Box, not pixels: whichever locator
        // the export wrote is the reference, verbatim.
        let src = attr("boxSharedLink")
            .or_else(|| attr("src"))
            .or_else(|| attr("fileName"))
            .unwrap_or_default();
        self.emitter.image(Image {
            src,
            alt: attr("alt").unwrap_or_default(),
            title: attr("fileName").unwrap_or_default(),
            ..Image::default()
        })
    }
}

/// Flatten inline content, keeping every run the note's marks describe.
///
/// The fold used to read one mark type, `link`, and only when the block
/// held exactly one run: a paragraph of two runs lost its URL as well as
/// its bold. Every mark now leaves a run, and `hyperlink` keeps its old
/// meaning, "this whole block is one link", which is a question about the
/// block rather than about the runs.
fn collapse(nodes: &[Value]) -> Collapsed {
    let mut inline = Inline::default();
    collapse_into(nodes, &mut inline);
    let hyperlink = inline.hyperlink();
    let (text, spans) = inline.take();
    (text, spans, hyperlink)
}

/// Everything one text node's marks say about the run they cover.
fn run_mark(node: &serde_json::Map<String, Value>) -> Mark {
    let mut mark = Mark::default();
    let Some(marks) = node.get("marks").and_then(Value::as_array) else {
        return mark;
    };
    for entry in marks {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let name = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let attr = |key: &str| {
            entry
                .get("attrs")
                .and_then(|attrs| attrs.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        };
        match name {
            "strong" | "bold" => mark.styles.push(pb::SpanStyle::Bold),
            "em" | "italic" => mark.styles.push(pb::SpanStyle::Italic),
            "underline" => mark.styles.push(pb::SpanStyle::Underline),
            "strikethrough" | "strike" | "del" => {
                mark.styles.push(pb::SpanStyle::Strikethrough);
            }
            "code" => mark.styles.push(pb::SpanStyle::Code),
            "highlight" | "background_color" => mark.styles.push(pb::SpanStyle::Mark),
            "subscript" => mark.styles.push(pb::SpanStyle::Subscript),
            "superscript" => mark.styles.push(pb::SpanStyle::Superscript),
            "font_color" | "text_color" | "color" => {
                mark.color = attr("color").or_else(|| attr("value")).map(str::to_owned);
            }
            "font_family" | "font" => {
                mark.font_family = attr("font").or_else(|| attr("value")).map(str::to_owned);
            }
            "font_size" => {
                mark.font_size_pt = attr("size")
                    .or_else(|| attr("value"))
                    .and_then(|size| size.trim_end_matches("pt").trim().parse().ok());
            }
            "link" => {
                if let Some(href) = attr("href").filter(|href| {
                    let lower = href.to_ascii_lowercase();
                    SAFE_LINK_SCHEMES
                        .iter()
                        .any(|scheme| lower.starts_with(scheme))
                }) {
                    mark.hyperlink = Some(href.to_owned());
                    mark.link_title = attr("title").map(str::to_owned);
                }
            }
            _ => {}
        }
    }
    mark
}

/// Every text of a node tree, joined, hard breaks as newlines. Used for
/// code blocks, which keep their whitespace.
fn plain_text(nodes: &[Value]) -> String {
    let mut out = String::new();
    for node in nodes {
        let Some(node) = node.as_object() else {
            continue;
        };
        match node.get("type").and_then(Value::as_str) {
            Some("text") => out.push_str(node.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("hard_break") => out.push('\n'),
            _ => out.push_str(&plain_text(content(node))),
        }
    }
    out
}

/// One table cell's flattened text and the runs inside it: the cell's
/// blocks joined with single spaces, each block's own marks kept.
fn cell_text(blocks: &[Value]) -> (String, Vec<pb::InlineSpan>) {
    let mut inline = Inline::default();
    for block in blocks.iter().filter_map(Value::as_object) {
        inline.space();
        collapse_into(content(block), &mut inline);
    }
    inline.take()
}

/// Flatten inline content into an existing flattener, so a cell's blocks
/// share one offset space. A node that is neither text nor a break is a
/// wrapper whose own content is what carries the text.
fn collapse_into(nodes: &[Value], inline: &mut Inline) {
    for node in nodes {
        let Some(node) = node.as_object() else {
            continue;
        };
        match node.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = node.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                let mark = run_mark(node);
                if mark.hyperlink.is_some() {
                    inline.open_link_mark(mark);
                    inline.push(text);
                    inline.close_link();
                } else {
                    inline.open(mark);
                    inline.push(text);
                    inline.close();
                }
            }
            Some("hard_break") => inline.space(),
            // A cell holds blocks; their inline content is what carries
            // the text.
            _ => collapse_into(content(node), inline),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text one run covers, sliced by code points.
    fn covered(text: &str, span: &pb::InlineSpan) -> String {
        let range = span.range.expect("a run has a range");
        text.chars()
            .skip(range.start as usize)
            .take((range.end - range.start) as usize)
            .collect()
    }

    #[test]
    fn a_whole_block_link_still_lands_on_the_block_and_never_only_there() {
        let single = serde_json::json!([
            {"type": "text", "text": "Box docs",
             "marks": [{"type": "link", "attrs": {"href": "https://box.com"}}]}
        ]);
        let (text, spans, link) = collapse(single.as_array().unwrap());
        assert_eq!(text, "Box docs");
        assert_eq!(link.as_deref(), Some("https://box.com"));
        assert_eq!(spans[0].hyperlink.as_deref(), Some("https://box.com"));

        let mixed = serde_json::json!([
            {"type": "text", "text": "see "},
            {"type": "text", "text": "docs",
             "marks": [{"type": "link", "attrs": {"href": "https://box.com"}}]}
        ]);
        let (text, spans, link) = collapse(mixed.as_array().unwrap());
        assert_eq!(text, "see docs");
        assert!(link.is_none(), "the block is not one link");
        assert_eq!(
            covered(&text, &spans[0]),
            "docs",
            "the URL survives the second run, which used to delete it"
        );
        assert_eq!(spans[0].hyperlink.as_deref(), Some("https://box.com"));
    }

    #[test]
    fn every_mark_the_note_carries_leaves_a_run() {
        let nodes = serde_json::json!([
            {"type": "text", "text": "café ",
             "marks": [{"type": "strong"}, {"type": "underline"}]},
            {"type": "text", "text": "prêt",
             "marks": [{"type": "em"},
                       {"type": "font_color", "attrs": {"color": "#ff0000"}},
                       {"type": "font_size", "attrs": {"size": "14pt"}}]}
        ]);
        let (text, spans, _) = collapse(nodes.as_array().unwrap());
        assert_eq!(text, "café prêt");
        assert_eq!(covered(&text, &spans[0]), "café");
        assert_eq!(
            spans[0].styles,
            vec![pb::SpanStyle::Bold as i32, pb::SpanStyle::Underline as i32],
            "one run may carry several marks"
        );
        assert_eq!(
            covered(&text, &spans[1]),
            "prêt",
            "the accented first run does not shift the second's range"
        );
        assert_eq!(spans[1].styles, vec![pb::SpanStyle::Italic as i32]);
        assert_eq!(spans[1].color.as_deref(), Some("#ff0000"));
        assert_eq!(spans[1].font_size_pt, Some(14.0));
    }

    #[test]
    fn unsafe_link_schemes_are_dropped() {
        let node = serde_json::json!([
            {"type": "text", "text": "x",
             "marks": [{"type": "link", "attrs": {"href": "javascript:alert(1)"}}]}
        ]);
        let (_, spans, link) = collapse(node.as_array().unwrap());
        assert!(link.is_none());
        assert!(spans.is_empty(), "and it is not smuggled in as a run");
    }

    #[test]
    fn hard_breaks_become_spaces_in_runs_and_newlines_in_code() {
        let nodes = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "hard_break"},
            {"type": "text", "text": "b"}
        ]);
        let (text, _, _) = collapse(nodes.as_array().unwrap());
        assert_eq!(text, "a b");
        assert_eq!(plain_text(nodes.as_array().unwrap()), "a\nb");
    }
}
