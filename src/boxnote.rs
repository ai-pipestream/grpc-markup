// SPDX-License-Identifier: Apache-2.0

//! The BoxNote fold: the JSON note format Box has exported since August
//! 2022 — a ProseMirror-style `doc` node tree — mapped onto the shared
//! block vocabulary. The walk mirrors docling's `boxnote_backend.py` node
//! for node.
//!
//! Shape of the mapping:
//!
//! - `heading` collapses its inline runs; the first level-1 heading is the
//!   title, deeper ones are section headers at their level.
//! - `paragraph` flattens to one block; a paragraph that is entirely one
//!   link (of a safe scheme) carries `hyperlink`.
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
//! mis-parsed, exactly as the docling backend refuses them.

use serde_json::Value;

use crate::parse::{Block, Emitter, ParseError};
use crate::proto::v1 as pb;

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

/// (text, hyperlink) for one styled span of a block.
type Run = (String, Option<String>);

/// The walk state of one note.
struct Walk<'w, 'e> {
    emitter: &'w mut Emitter<'e>,
    title_taken: bool,
    quote_depth: u32,
}

impl Walk<'_, '_> {
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
        let (text, hyperlink) = collapse(content(node));
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
        self.emitter.text_block(Block {
            label,
            text,
            level,
            hyperlink,
            ..Block::default()
        })
    }

    fn paragraph(&mut self, node: &serde_json::Map<String, Value>) -> Result<(), ParseError> {
        let (text, hyperlink) = collapse(content(node));
        if text.is_empty() {
            return Ok(());
        }
        let label = if self.quote_depth > 0 {
            pb::BlockLabel::Quote
        } else {
            pb::BlockLabel::Paragraph
        };
        self.emitter.text_block(Block {
            label,
            text,
            hyperlink,
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
        let mut hyperlink = None;
        for child in content(item) {
            let is_leading_paragraph =
                text.is_empty() && child.get("type").and_then(Value::as_str) == Some("paragraph");
            if is_leading_paragraph {
                let child_content = child.as_object().map_or(&[][..], |node| content(node));
                (text, hyperlink) = collapse(child_content);
            } else {
                nested.push(child);
            }
        }
        if !text.is_empty() {
            self.emitter.text_block(Block {
                label: pb::BlockLabel::ListItem,
                role: match checked {
                    Some(true) => "task-item-checked".to_owned(),
                    Some(false) => "task-item".to_owned(),
                    None => String::new(),
                },
                text,
                enumerated,
                // BoxNote leaves numbering implicit, so there is no marker
                // "as written" to carry.
                marker: None,
                list_depth: Some(depth),
                hyperlink,
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
        self.emitter.text_block(Block {
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
        self.emitter.table_start(None, None)?;
        let mut occupied: std::collections::BTreeSet<(u32, u32)> =
            std::collections::BTreeSet::new();
        for (row_index, row) in rows.iter().enumerate() {
            let row_index = u32::try_from(row_index).unwrap_or(u32::MAX);
            let mut column = 0u32;
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
                while occupied.contains(&(row_index, column)) {
                    column += 1;
                }
                let span = |name: &str| {
                    cell.get("attrs")
                        .and_then(|attrs| attrs.get(name))
                        .and_then(Value::as_u64)
                        .and_then(|span| u32::try_from(span).ok())
                        .filter(|span| *span >= 1)
                        .unwrap_or(1)
                };
                let row_span = span("rowspan");
                let column_span = span("colspan");
                for r in row_index..row_index + row_span {
                    for c in column..column + column_span {
                        occupied.insert((r, c));
                    }
                }
                let is_header = cell_type == "table_header";
                all_headers &= is_header;
                cells.push(pb::TableCell {
                    column_index: column,
                    text: cell_text(content(cell)),
                    column_span,
                    row_span,
                    is_header,
                });
                column += column_span;
            }
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
        self.emitter.image(
            src,
            attr("alt").unwrap_or_default(),
            attr("fileName").unwrap_or_default(),
            None,
        )
    }
}

/// Reduce inline content to a single run.
///
/// Headings and list items take one text and one hyperlink, so a single
/// styled span keeps its link and a mixed span falls back to plain joined
/// text — the same rule as the docling backend's `_collapse`.
fn collapse(nodes: &[Value]) -> Run {
    let mut runs = runs(nodes);
    if runs.len() == 1 {
        let (text, hyperlink) = runs.remove(0);
        return (normalize(&text), hyperlink);
    }
    let joined: String = runs.into_iter().map(|(text, _)| text).collect();
    (normalize(&joined), None)
}

/// The styled spans of an inline `content` array.
fn runs(nodes: &[Value]) -> Vec<Run> {
    let mut out = Vec::new();
    for node in nodes {
        let Some(node) = node.as_object() else {
            continue;
        };
        match node.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = node.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    out.push((text.to_owned(), link_mark(node)));
                }
            }
            Some("hard_break") => out.push((" ".to_owned(), None)),
            _ => {}
        }
    }
    out
}

/// The hyperlink of a text node's marks, when it carries a safe one.
fn link_mark(node: &serde_json::Map<String, Value>) -> Option<String> {
    let marks = node.get("marks")?.as_array()?;
    for mark in marks {
        let mark = mark.as_object()?;
        if mark.get("type").and_then(Value::as_str) == Some("link")
            && let Some(href) = mark
                .get("attrs")
                .and_then(|attrs| attrs.get("href"))
                .and_then(Value::as_str)
            && SAFE_LINK_SCHEMES
                .iter()
                .any(|scheme| href.to_ascii_lowercase().starts_with(scheme))
        {
            return Some(href.to_owned());
        }
    }
    None
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

/// One table cell's flattened text: block texts joined with single spaces.
fn cell_text(blocks: &[Value]) -> String {
    let texts: Vec<String> = blocks
        .iter()
        .filter_map(Value::as_object)
        .map(|block| normalize(&plain_text(content(block))))
        .filter(|text| !text.is_empty())
        .collect();
    texts.join(" ")
}

/// Runs of whitespace collapsed to single spaces, ends trimmed.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_linked_run_keeps_its_link_and_a_mixed_span_drops_it() {
        let single = serde_json::json!([
            {"type": "text", "text": "Box docs",
             "marks": [{"type": "link", "attrs": {"href": "https://box.com"}}]}
        ]);
        let (text, link) = collapse(single.as_array().unwrap());
        assert_eq!(text, "Box docs");
        assert_eq!(link.as_deref(), Some("https://box.com"));

        let mixed = serde_json::json!([
            {"type": "text", "text": "see "},
            {"type": "text", "text": "docs",
             "marks": [{"type": "link", "attrs": {"href": "https://box.com"}}]}
        ]);
        let (text, link) = collapse(mixed.as_array().unwrap());
        assert_eq!(text, "see docs");
        assert!(link.is_none());
    }

    #[test]
    fn unsafe_link_schemes_are_dropped() {
        let node = serde_json::json!([
            {"type": "text", "text": "x",
             "marks": [{"type": "link", "attrs": {"href": "javascript:alert(1)"}}]}
        ]);
        let (_, link) = collapse(node.as_array().unwrap());
        assert!(link.is_none());
    }

    #[test]
    fn hard_breaks_become_spaces_in_runs_and_newlines_in_code() {
        let nodes = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "hard_break"},
            {"type": "text", "text": "b"}
        ]);
        let (text, _) = collapse(nodes.as_array().unwrap());
        assert_eq!(text, "a b");
        assert_eq!(plain_text(nodes.as_array().unwrap()), "a\nb");
    }
}
