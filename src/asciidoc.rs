// SPDX-License-Identifier: Apache-2.0

//! The AsciiDoc fold: a line-oriented parser mapping the structural core
//! of the format plus the three constructs this fleet needs most —
//! delimited listing blocks as code, attribute entries as
//! metadata, and admonition paragraphs. It is a line machine, not a full
//! AsciiDoc implementation; what it deliberately skips is listed in
//! `docs/design.md`.
//!
//! Shape of the mapping, line by line:
//!
//! - `:name: value` attribute entries become `MarkupInfo.metadata`.
//! - `= Title` is `BLOCK_LABEL_TITLE` (first one; later ones are level-1
//!   section headers); `== Section` and deeper are
//!   `BLOCK_LABEL_SECTION_HEADER` at marker length minus one.
//! - Consecutive non-empty lines join into one paragraph, flushed at a
//!   blank line. A paragraph opening with `NOTE:` / `TIP:` / `IMPORTANT:`
//!   / `WARNING:` / `CAUTION:` carries `role: "admonition"`.
//! - `*` and `.` list items nest by marker repetition (`**` is depth 2);
//!   `-` and `1.` are depth-1 items; `.`-markers and numbered markers are
//!   `enumerated`; the marker rides verbatim.
//! - `|===` delimits a table; each `|` line inside is one row, cells split
//!   on `|` with AsciiDoc cell specifiers (`2+`, `^.^h`) stripped —
//!   spans are not expanded. The first row is the
//!   header row. A `.Caption` line before the table lands on `TableStart`.
//! - `image::path[alt, k=v]` is an `ImageBlock`, the path verbatim (no
//!   `file:` URI mangling — this service never dereferences); a pending
//!   `.Caption` follows it as a `CAPTION` block.
//! - `----` fences a listing block, kept verbatim as `BLOCK_LABEL_CODE`;
//!   a `[source,lang]` attribute line before it declares the `language`.

use std::collections::HashMap;

use crate::parse::{Block, Emitter, ParseError, scalar};
use crate::proto::v1 as pb;

/// The admonition labels AsciiDoc puts before a paragraph.
const ADMONITIONS: &[&str] = &["NOTE:", "TIP:", "IMPORTANT:", "WARNING:", "CAUTION:"];

/// Map one AsciiDoc document. Empty input is a valid empty parse.
///
/// # Errors
///
/// Only [`ParseError::ConsumerGone`]: like Markdown, AsciiDoc's own error
/// handling re-reads anything unrecognized as plain text.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    // Attribute entries are the document's own metadata; they are gathered
    // up front so `info` can carry them, and skipped again in the walk.
    let mut metadata = HashMap::new();
    for line in &lines {
        if let Some((key, value)) = attribute_entry(line) {
            metadata.insert(key, scalar(value));
        }
    }
    emitter.info(None, metadata)?;

    let mut walk = Walk {
        emitter,
        title_taken: false,
        paragraph: Vec::new(),
        paragraph_line: 0,
        caption: None,
        pending_language: None,
        in_table: false,
        table_row_index: 0,
        listing: None,
    };
    for (index, line) in lines.iter().enumerate() {
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        walk.line(line, line_no)?;
    }
    walk.finish()
}

/// `:name: value` (or `:name:` alone), the attribute-entry grammar.
fn attribute_entry(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix(':')?;
    let (name, value) = rest.split_once(':')?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((name.to_owned(), value.trim().to_owned()))
}

/// A parsed list-item line: (marker as written, enumerated, depth, text).
fn list_item(line: &str) -> Option<(String, bool, u32, String)> {
    let trimmed = line.trim_start();
    let (marker, enumerated, depth) = if trimmed.starts_with('*') {
        let stars = trimmed.chars().take_while(|&c| c == '*').count();
        (trimmed[..stars].to_owned(), false, stars)
    } else if trimmed.starts_with('.') {
        let dots = trimmed.chars().take_while(|&c| c == '.').count();
        (trimmed[..dots].to_owned(), true, dots)
    } else if let Some(rest) = trimmed.strip_prefix('-') {
        if !rest.starts_with(' ') {
            return None;
        }
        ("-".to_owned(), false, 1)
    } else {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || !trimmed[digits..].starts_with('.') {
            return None;
        }
        (trimmed[..=digits].to_owned(), true, 1)
    };
    let text = trimmed[marker.len()..].strip_prefix(' ')?.trim();
    if text.is_empty() {
        return None;
    }
    Some((
        marker,
        enumerated,
        u32::try_from(depth).unwrap_or(u32::MAX),
        text.to_owned(),
    ))
}

/// `= Title` through `====== Deep`: (marker length, text).
fn heading(line: &str) -> Option<(usize, String)> {
    let equals = line.chars().take_while(|&c| c == '=').count();
    if equals == 0 {
        return None;
    }
    let text = line[equals..].strip_prefix(' ')?.trim();
    if text.is_empty() {
        return None;
    }
    Some((equals, text.to_owned()))
}

/// `image::path[attrs]`: (path, alt).
fn image_macro(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("image::")?;
    let open = rest.find('[')?;
    let close = rest.rfind(']')?;
    if close < open {
        return None;
    }
    let path = rest[..open].trim().to_owned();
    // The first attribute and any later bare attribute join into the alt
    // text; `key=value` attributes are geometry and are not text.
    let alt = rest[open + 1..close]
        .split(',')
        .map(str::trim)
        .filter(|attr| !attr.is_empty() && !attr.contains('='))
        .collect::<Vec<_>>()
        .join(", ");
    Some((path, alt))
}

/// A `[source,lang]` (or bare `[source]`) block attribute line.
fn source_attribute(line: &str) -> Option<Option<String>> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let mut parts = inner.split(',').map(str::trim);
    if parts.next() != Some("source") {
        return None;
    }
    Some(
        parts
            .next()
            .filter(|lang| !lang.is_empty())
            .map(str::to_owned),
    )
}

/// True for a listing-block fence: a line of four or more dashes.
fn is_listing_fence(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.len() >= 4 && trimmed.bytes().all(|b| b == b'-')
}

/// Strip a trailing AsciiDoc cell specifier (`2+`, `^.^h`, `3*`) from the
/// text before a `|`, the way docling's `_CELL_SPEC` regex strips it: only
/// when it stands alone as the last whitespace-separated token.
fn strip_cell_spec(text: &str) -> &str {
    let trimmed = text.trim_end();
    let token_start = trimmed
        .rfind(char::is_whitespace)
        .map_or(0, |space| space + 1);
    let token = &trimmed[token_start..];
    if !token.is_empty() && is_cell_spec(token) {
        trimmed[..token_start].trim_end()
    } else {
        trimmed
    }
}

/// The cell-specifier grammar: span factors (`3*`, `2+`), alignment
/// (`<`, `^`, `>`, optionally `.` + one more), one style letter.
fn is_cell_spec(token: &str) -> bool {
    let mut rest = token;
    // (digits[.digits][*+])*
    loop {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        let mut after = &rest[digits..];
        if let Some(stripped) = after.strip_prefix('.') {
            let fraction = stripped.chars().take_while(char::is_ascii_digit).count();
            if fraction == 0 {
                return false;
            }
            after = &stripped[fraction..];
        }
        if let Some(stripped) = after.strip_prefix(['*', '+']) {
            rest = stripped;
        } else {
            return false;
        }
    }
    if let Some(stripped) = rest.strip_prefix(['<', '^', '>']) {
        rest = stripped;
    }
    if let Some(stripped) = rest.strip_prefix('.') {
        let Some(stripped) = stripped.strip_prefix(['<', '^', '>']) else {
            return false;
        };
        rest = stripped;
    }
    if let Some(stripped) = rest.strip_prefix(['a', 'd', 'e', 'h', 'l', 'm', 's']) {
        rest = stripped;
    }
    rest.is_empty()
}

/// An open listing block being captured.
struct Listing {
    language: Option<String>,
    line: u32,
    lines: Vec<String>,
}

/// The line-machine state of one document.
struct Walk<'w, 'e> {
    emitter: &'w mut Emitter<'e>,
    title_taken: bool,
    /// Lines of the paragraph being accumulated.
    paragraph: Vec<String>,
    /// Where that paragraph started.
    paragraph_line: u32,
    /// A `.Caption` line waiting for the table or image it captions.
    caption: Option<String>,
    /// The language of a `[source,lang]` line waiting for its listing.
    pending_language: Option<String>,
    in_table: bool,
    table_row_index: u32,
    listing: Option<Listing>,
}

impl Walk<'_, '_> {
    fn line(&mut self, line: &str, line_no: u32) -> Result<(), ParseError> {
        // A listing block swallows everything up to its closing fence.
        if let Some(listing) = self.listing.as_mut() {
            if is_listing_fence(line) {
                let listing = self.listing.take().expect("just matched");
                return self.emit_listing(listing);
            }
            listing.lines.push(line.to_owned());
            return Ok(());
        }
        if is_listing_fence(line) {
            self.flush_paragraph()?;
            self.listing = Some(Listing {
                language: self.pending_language.take(),
                line: line_no,
                lines: Vec::new(),
            });
            return Ok(());
        }
        if let Some(language) = source_attribute(line) {
            self.flush_paragraph()?;
            self.pending_language = language;
            return Ok(());
        }

        // Table delimiter and rows.
        if line.trim() == "|===" {
            self.flush_paragraph()?;
            if self.in_table {
                self.in_table = false;
                return self.emitter.table_end();
            }
            self.in_table = true;
            self.table_row_index = 0;
            let caption = self.caption.take();
            self.emitter.table_start(caption, Some(line_no))?;
            return Ok(());
        }
        if self.in_table {
            if let Some(cells) = line.strip_prefix('|') {
                let cells: Vec<pb::TableCell> = cells
                    .split('|')
                    .enumerate()
                    .map(|(column, cell)| pb::TableCell {
                        column_index: u32::try_from(column).unwrap_or(u32::MAX),
                        text: strip_cell_spec(cell).trim().to_owned(),
                        column_span: 1,
                        row_span: 1,
                        is_header: self.table_row_index == 0,
                        ..pb::TableCell::default()
                    })
                    .collect();
                let is_header = self.table_row_index == 0;
                self.table_row_index += 1;
                return self.emitter.table_row(is_header, cells);
            }
            // Blank separators and stray prose between rows are layout,
            // not cells.
            return Ok(());
        }

        if attribute_entry(line).is_some() {
            // Already in MarkupInfo.metadata from the prescan.
            return self.flush_paragraph();
        }

        if let Some((equals, text)) = heading(line) {
            self.flush_paragraph()?;
            let (label, level) = if equals == 1 {
                if self.title_taken {
                    (pb::BlockLabel::SectionHeader, Some(1))
                } else {
                    self.title_taken = true;
                    (pb::BlockLabel::Title, None)
                }
            } else {
                // docling's own level rule: `==` is level 1.
                (
                    pb::BlockLabel::SectionHeader,
                    Some(u32::try_from(equals - 1).unwrap_or(u32::MAX)),
                )
            };
            return self.emitter.text_block(Block {
                label,
                text,
                level,
                source_line: Some(line_no),
                ..Block::default()
            });
        }

        if let Some((marker, enumerated, depth, text)) = list_item(line) {
            self.flush_paragraph()?;
            return self.emitter.text_block(Block {
                label: pb::BlockLabel::ListItem,
                text,
                enumerated,
                marker: Some(marker),
                list_depth: Some(depth),
                source_line: Some(line_no),
                ..Block::default()
            });
        }

        if let Some((path, alt)) = image_macro(line) {
            self.flush_paragraph()?;
            let caption = self.caption.take();
            self.emitter
                .image(path, alt, String::new(), Some(line_no))?;
            if let Some(caption) = caption {
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Caption,
                    text: caption,
                    source_line: Some(line_no),
                    ..Block::default()
                })?;
            }
            return Ok(());
        }

        // `.Caption` — after the list branch, so `. item` and `.. item`
        // stay list items. One line only.
        if self.caption.is_none()
            && self.paragraph.is_empty()
            && let Some(text) = line.strip_prefix('.')
            && !text.starts_with(['.', ' '])
            && !text.trim().is_empty()
        {
            self.caption = Some(text.trim().to_owned());
            return Ok(());
        }

        if line.trim().is_empty() {
            self.flush_paragraph()?;
            // A caption nothing consumed is still text the author wrote.
            if let Some(caption) = self.caption.take() {
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Caption,
                    text: caption,
                    source_line: Some(line_no.saturating_sub(1)),
                    ..Block::default()
                })?;
            }
            return Ok(());
        }

        if self.paragraph.is_empty() {
            self.paragraph_line = line_no;
        }
        self.paragraph.push(line.trim().to_owned());
        Ok(())
    }

    /// Emit the accumulated paragraph, admonition-labelled when its first
    /// line opens with one of the AsciiDoc admonition markers.
    fn flush_paragraph(&mut self) -> Result<(), ParseError> {
        if self.paragraph.is_empty() {
            return Ok(());
        }
        let mut text = std::mem::take(&mut self.paragraph).join(" ");
        let mut role = String::new();
        if let Some(marker) = ADMONITIONS.iter().find(|marker| text.starts_with(*marker)) {
            role = "admonition".to_owned();
            text = text[marker.len()..].trim_start().to_owned();
        }
        if text.is_empty() {
            return Ok(());
        }
        self.emitter.text_block(Block {
            label: pb::BlockLabel::Paragraph,
            role,
            text,
            source_line: Some(self.paragraph_line),
            ..Block::default()
        })
    }

    /// A finished (or truncated) listing block as one code block.
    fn emit_listing(&mut self, listing: Listing) -> Result<(), ParseError> {
        let text = listing.lines.join("\n");
        let text = text.trim_matches('\n');
        if text.is_empty() {
            return Ok(());
        }
        self.emitter.text_block(Block {
            label: pb::BlockLabel::Code,
            text: text.to_owned(),
            language: listing.language,
            source_line: Some(listing.line),
            ..Block::default()
        })
    }

    /// End of input: everything still open flushes, and a fence that never
    /// closed is said out loud.
    fn finish(&mut self) -> Result<(), ParseError> {
        self.flush_paragraph()?;
        if let Some(listing) = self.listing.take() {
            self.emitter.warn(
                pb::WarningCode::MalformedStructure,
                "a ---- listing block was not closed; its content is kept as code",
            );
            self.emit_listing(listing)?;
        }
        if self.in_table {
            self.emitter.warn(
                pb::WarningCode::MalformedStructure,
                "a |=== table was not closed",
            );
            self.in_table = false;
            self.emitter.table_end()?;
        }
        if let Some(caption) = self.caption.take() {
            self.emitter.text_block(Block {
                label: pb::BlockLabel::Caption,
                text: caption,
                ..Block::default()
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_entries_parse_and_prose_colons_do_not() {
        assert_eq!(
            attribute_entry(":author: Ada Lovelace"),
            Some(("author".to_owned(), "Ada Lovelace".to_owned()))
        );
        assert_eq!(
            attribute_entry(":toc:"),
            Some(("toc".to_owned(), String::new()))
        );
        assert_eq!(attribute_entry("not: an attribute"), None);
        assert_eq!(attribute_entry(": bad :"), None);
    }

    #[test]
    fn list_markers_carry_kind_and_marker_depth() {
        assert_eq!(
            list_item("* top"),
            Some(("*".to_owned(), false, 1, "top".to_owned()))
        );
        assert_eq!(
            list_item("** nested"),
            Some(("**".to_owned(), false, 2, "nested".to_owned()))
        );
        assert_eq!(
            list_item(". first"),
            Some((".".to_owned(), true, 1, "first".to_owned()))
        );
        assert_eq!(
            list_item("... deep"),
            Some(("...".to_owned(), true, 3, "deep".to_owned()))
        );
        assert_eq!(
            list_item("3. third"),
            Some(("3.".to_owned(), true, 1, "third".to_owned()))
        );
        assert_eq!(
            list_item("- dash"),
            Some(("-".to_owned(), false, 1, "dash".to_owned()))
        );
        assert_eq!(list_item(".Caption line"), None, "no space, no item");
        assert_eq!(list_item("*bold* text"), None);
        assert_eq!(list_item("plain text"), None);
    }

    #[test]
    fn headings_use_doclings_level_rule() {
        assert_eq!(heading("= Title"), Some((1, "Title".to_owned())));
        assert_eq!(heading("=== Deep"), Some((3, "Deep".to_owned())));
        assert_eq!(heading("=no space"), None);
        assert_eq!(heading("prose = prose"), None);
    }

    #[test]
    fn image_macros_split_path_and_alt_from_geometry() {
        assert_eq!(
            image_macro("image::figs/a.png[The engine, width=200, height=100]"),
            Some(("figs/a.png".to_owned(), "The engine".to_owned()))
        );
        assert_eq!(
            image_macro("image::b.png[]"),
            Some(("b.png".to_owned(), String::new()))
        );
        assert_eq!(image_macro("image:inline.png[x]"), None, "block form only");
    }

    #[test]
    fn cell_specs_strip_only_when_they_stand_alone() {
        assert_eq!(strip_cell_spec("value 2+"), "value");
        assert_eq!(strip_cell_spec("value ^.^h"), "value");
        assert_eq!(strip_cell_spec("Eth"), "Eth", "prose survives");
        assert_eq!(strip_cell_spec("3*"), "");
        assert_eq!(strip_cell_spec("plain"), "plain");
    }

    #[test]
    fn source_attributes_declare_the_language() {
        assert_eq!(
            source_attribute("[source,rust]"),
            Some(Some("rust".to_owned()))
        );
        assert_eq!(
            source_attribute("[source, python ]"),
            Some(Some("python".to_owned()))
        );
        assert_eq!(source_attribute("[source]"), Some(None));
        assert_eq!(source_attribute("[quote]"), None);
    }
}
