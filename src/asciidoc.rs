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
//!   / `WARNING:` / `CAUTION:` carries `role: "admonition"` and the
//!   `admonition` kind that marker named.
//! - `*` and `.` list items nest by marker repetition (`**` is depth 2);
//!   `-` and `1.` are depth-1 items; `.`-markers and numbered markers are
//!   `enumerated`; the marker rides verbatim.
//! - `|===` delimits a table; each row is a line whose cells are split on
//!   `|`. A cell specifier (`2+`, `3*`, `^.^h`) sits before the `|` that
//!   opens its cell and is honoured rather than deleted: span factors
//!   become `column_span` / `row_span`, alignments become the cell's
//!   `align` and `valign`, a duplication factor applies one specifier to
//!   the cells that follow, an `h` marks the cell a header and the
//!   emphasis style letters put a run over the cell text. The first row is
//!   the header row. A `.Caption` line before the table lands on
//!   `TableStart`.
//! - `image::path[alt, k=v]` is an `ImageBlock`, the path verbatim (no
//!   `file:` URI mangling — this service never dereferences); a pending
//!   `.Caption` follows it as a `CAPTION` block.
//! - `----` fences a listing block, kept verbatim as `BLOCK_LABEL_CODE`;
//!   a `[source,lang]` attribute line before it declares the `language`.
//! - `[[id]]` on its own line names the block below it; `[[id]]` inside a
//!   paragraph comes out of the text and stays as an anchor run at the
//!   position it stood.

use std::collections::HashMap;

use crate::parse::{Block, Emitter, Image, ParseError, Table, scalar};
use crate::proto::v1 as pb;
use crate::spans::{Inline, Mark};

/// The admonition labels AsciiDoc puts before a paragraph, each with the
/// kind it declares. The parser matched these and then kept only the fact
/// that one of them had matched.
const ADMONITIONS: &[(&str, pb::AdmonitionKind)] = &[
    ("NOTE:", pb::AdmonitionKind::Note),
    ("TIP:", pb::AdmonitionKind::Tip),
    ("IMPORTANT:", pb::AdmonitionKind::Important),
    ("WARNING:", pb::AdmonitionKind::Warning),
    ("CAUTION:", pb::AdmonitionKind::Caution),
];

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
        pending_anchor: None,
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

/// One table row: the specifier of its first cell, when the line opens
/// with one, and everything after the `|` that starts that cell.
///
/// `|Name |Role` is a row with no leading specifier; `2+^|Spanning` is a
/// row whose first cell spans two columns. A line with no `|` at all is
/// layout between rows, not a row.
fn table_row(line: &str) -> Option<(Option<CellSpec>, &str)> {
    let pipe = line.find('|')?;
    let head = line[..pipe].trim();
    let lead = if head.is_empty() {
        None
    } else {
        Some(parse_cell_spec(head)?)
    };
    Some((lead, &line[pipe + 1..]))
}

/// The id an anchor line declares, when the whole line is one anchor.
///
/// AsciiDoc writes `[[id]]` or `[[id,reference text]]` on its own line to
/// name the block below it. The reference text is the phrase a cross
/// reference renders when it has none of its own, which this plane has no
/// slot for and no renderer to use it in; the id is the name, and the name
/// is what an anchor is.
fn anchor_line(line: &str) -> Option<String> {
    anchor_id(line.trim())
}

/// The id inside one `[[...]]` form, or nothing when the text is not one.
fn anchor_id(text: &str) -> Option<String> {
    let inner = text.strip_prefix("[[")?.strip_suffix("]]")?;
    let id = inner.split(',').next()?.trim();
    (!id.is_empty() && !id.contains([' ', '[', ']'])).then(|| id.to_owned())
}

/// Take every inline `[[id]]` out of a paragraph, returning the text
/// without them and one anchor run per removal.
///
/// An anchor names a position, not a run, so its range is empty: it starts
/// and ends where the anchor stood. That is what the source says, and
/// widening it to the word that follows would be this fold guessing at a
/// span the author never wrote.
fn take_anchors(text: &str) -> (String, Vec<pb::InlineSpan>) {
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::new();
    let mut chars = 0u32;
    let mut rest = text;
    while let Some(at) = rest.find("[[") {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        chars += u32::try_from(before.chars().count()).unwrap_or(0);
        match tail
            .find("]]")
            .and_then(|end| anchor_id(&tail[..end + 2]).map(|id| (id, end + 2)))
        {
            Some((id, end)) => {
                spans.push(pb::InlineSpan {
                    range: Some(pb::IntRange {
                        start: chars,
                        end: chars,
                    }),
                    anchor_id: Some(id),
                    ..pb::InlineSpan::default()
                });
                rest = &tail[end..];
            }
            // Not an anchor: `[[` is two ordinary brackets.
            None => {
                out.push_str("[[");
                chars += 2;
                rest = &tail[2..];
            }
        }
    }
    out.push_str(rest);
    (out, spans)
}

/// True for a listing-block fence: a line of four or more dashes.
fn is_listing_fence(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.len() >= 4 && trimmed.bytes().all(|b| b == b'-')
}

/// One AsciiDoc cell specifier, parsed rather than deleted.
///
/// The grammar the fold already recognized: a duplication factor (`3*`), a
/// span factor (`2+`, `2.3+` for columns and rows), an alignment (`<`, `^`,
/// `>`, and the same three after a `.` for the vertical one) and one style
/// letter. Every part of it lands on the cell now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellSpec {
    /// How many following cells this specifier applies to.
    repeat: u32,
    column_span: u32,
    row_span: u32,
    align: pb::Alignment,
    valign: pb::Alignment,
    /// The style letter, when the specifier names one.
    style: Option<char>,
}

impl Default for CellSpec {
    fn default() -> Self {
        Self {
            repeat: 1,
            column_span: 1,
            row_span: 1,
            align: pb::Alignment::Unspecified,
            valign: pb::Alignment::Unspecified,
            style: None,
        }
    }
}

impl CellSpec {
    /// True when the style letter marks the cell as a header.
    const fn is_header(self) -> bool {
        matches!(self.style, Some('h'))
    }

    /// The run the style letter puts over the whole cell text, if any.
    fn style_mark(self) -> Option<Mark> {
        match self.style? {
            'e' => Some(Mark::style(pb::SpanStyle::Italic)),
            's' => Some(Mark::style(pb::SpanStyle::Bold)),
            'm' | 'l' => Some(Mark::style(pb::SpanStyle::Code)),
            _ => None,
        }
    }
}

/// Split a row fragment into the text of the cell it closes and the
/// specifier of the cell it opens.
///
/// A specifier sits immediately before the `|` that starts its cell, which
/// is why it is found at the *end* of the previous fragment: only when it
/// stands alone as the last whitespace-separated token.
fn split_cell_spec(text: &str) -> (&str, Option<CellSpec>) {
    let trimmed = text.trim_end();
    // The token starts after the last whitespace character, which may be
    // more than one byte long (a no-break space is two).
    let token_start = trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(at, c)| at + c.len_utf8());
    let token = &trimmed[token_start..];
    match (!token.is_empty())
        .then(|| parse_cell_spec(token))
        .flatten()
    {
        Some(spec) => (trimmed[..token_start].trim_end(), Some(spec)),
        None => (trimmed, None),
    }
}

/// The cell-specifier grammar, or `None` when the token is ordinary text.
fn parse_cell_spec(token: &str) -> Option<CellSpec> {
    let mut spec = CellSpec::default();
    let mut rest = token;
    let mut saw_factor = false;
    // (digits[.digits][*+])*
    loop {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        let first: u32 = rest[..digits].parse().ok()?;
        let mut second = None;
        let mut after = &rest[digits..];
        if let Some(stripped) = after.strip_prefix('.') {
            let fraction = stripped.chars().take_while(char::is_ascii_digit).count();
            if fraction == 0 {
                return None;
            }
            second = stripped[..fraction].parse::<u32>().ok();
            after = &stripped[fraction..];
        }
        match after.chars().next() {
            // `2+` spans columns; `2.3+` spans columns and rows.
            Some('+') => {
                spec.column_span = first.max(1);
                spec.row_span = second.unwrap_or(1).max(1);
            }
            // `3*` applies this specifier to the next three cells.
            Some('*') => spec.repeat = first.max(1),
            _ => return None,
        }
        saw_factor = true;
        rest = &after[1..];
    }
    let horizontal = |c: char| match c {
        '<' => pb::Alignment::Left,
        '^' => pb::Alignment::Center,
        _ => pb::Alignment::Right,
    };
    let mut saw_alignment = false;
    if let Some(first) = rest.chars().next().filter(|c| "<^>".contains(*c)) {
        spec.align = horizontal(first);
        saw_alignment = true;
        rest = &rest[1..];
    }
    if let Some(stripped) = rest.strip_prefix('.') {
        let vertical = stripped.chars().next().filter(|c| "<^>".contains(*c))?;
        spec.valign = match vertical {
            '<' => pb::Alignment::Top,
            '^' => pb::Alignment::Middle,
            _ => pb::Alignment::Bottom,
        };
        saw_alignment = true;
        rest = &stripped[1..];
    }
    let mut saw_style = false;
    if let Some(style) = rest.chars().next().filter(|c| "adehlms".contains(*c)) {
        spec.style = Some(style);
        saw_style = true;
        rest = &rest[1..];
    }
    // The token has to be a specifier and nothing else, and it has to say
    // something: a bare `h|` is the header-cell style, but a token that
    // matched no part of the grammar is prose.
    if !rest.is_empty() || !(saw_factor || saw_alignment || saw_style) {
        return None;
    }
    Some(spec)
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
    /// The id a `[[id]]` line declared, waiting for the block it names.
    pending_anchor: Option<String>,
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

        if let Some(id) = anchor_line(line) {
            self.flush_paragraph()?;
            self.pending_anchor = Some(id);
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
            self.emitter.table_start(Table {
                caption,
                source_line: Some(line_no),
                ..Table::default()
            })?;
            return Ok(());
        }
        if self.in_table {
            if let Some((lead, row)) = table_row(line) {
                let is_header = self.table_row_index == 0;
                let cells = self.row_cells(lead, row, is_header);
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
                anchor_id: self.pending_anchor.take(),
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
                anchor_id: self.pending_anchor.take(),
                ..Block::default()
            });
        }

        if let Some((path, alt)) = image_macro(line) {
            self.flush_paragraph()?;
            let caption = self.caption.take();
            self.emitter.image(Image {
                src: path,
                alt,
                source_line: Some(line_no),
                ..Image::default()
            })?;
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

    /// One `|`-delimited row as cells, with each cell's specifier honoured
    /// rather than deleted.
    ///
    /// A specifier sits before the `|` that opens its cell, so it is found
    /// at the end of the fragment that closes the cell before it; the one
    /// exception is a row that opens `|2+|`, where the first fragment is
    /// the specifier itself and no cell precedes it.
    fn row_cells(&self, lead: Option<CellSpec>, row: &str, header_row: bool) -> Vec<pb::TableCell> {
        let mut cells = Vec::new();
        let mut column = 0u32;
        let mut pending: Option<(CellSpec, u32)> = lead.map(|spec| (spec, spec.repeat));
        for fragment in row.split('|') {
            let (text, next) = split_cell_spec(fragment);
            let spec = match pending.as_mut() {
                Some((spec, left)) => {
                    let spec = *spec;
                    *left -= 1;
                    if *left == 0 {
                        pending = None;
                    }
                    spec
                }
                None => CellSpec::default(),
            };
            let mut inline = Inline::default();
            if let Some(mark) = spec.style_mark() {
                inline.open(mark);
                inline.push(text.trim());
                inline.close();
            } else {
                inline.push(text.trim());
            }
            let (text, spans) = inline.take();
            cells.push(pb::TableCell {
                column_index: column,
                text,
                column_span: spec.column_span,
                row_span: spec.row_span,
                is_header: header_row || spec.is_header(),
                spans,
                align: spec.align as i32,
                valign: spec.valign as i32,
                // An AsciiDoc cell is flat text by construction.
                blocks: Vec::new(),
            });
            column = column.saturating_add(spec.column_span);
            if let Some(spec) = next {
                pending = Some((spec, spec.repeat));
            }
        }
        cells
    }

    /// Emit the accumulated paragraph, admonition-labelled when its first
    /// line opens with one of the AsciiDoc admonition markers.
    fn flush_paragraph(&mut self) -> Result<(), ParseError> {
        if self.paragraph.is_empty() {
            return Ok(());
        }
        let mut text = std::mem::take(&mut self.paragraph).join(" ");
        let mut role = String::new();
        let mut admonition = None;
        if let Some((marker, kind)) = ADMONITIONS
            .iter()
            .find(|(marker, _)| text.starts_with(*marker))
        {
            role = "admonition".to_owned();
            // Which of the five it was: matched here since the fold was
            // written, and now kept.
            admonition = Some(*kind);
            text = text[marker.len()..].trim_start().to_owned();
        }
        if text.is_empty() {
            return Ok(());
        }
        // The anchors come out after the admonition marker, so their
        // positions index the text the block actually carries.
        let (text, spans) = take_anchors(&text);
        self.emitter.text_block(Block {
            label: pb::BlockLabel::Paragraph,
            role,
            text,
            source_line: Some(self.paragraph_line),
            spans,
            admonition,
            anchor_id: self.pending_anchor.take(),
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
            anchor_id: self.pending_anchor.take(),
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
    fn cell_specs_are_parsed_where_they_used_to_be_deleted() {
        let (text, spec) = split_cell_spec("value 2+");
        assert_eq!(text, "value");
        let spec = spec.expect("a span factor");
        assert_eq!(spec.column_span, 2, "the factor lands on the cell");
        assert_eq!(spec.row_span, 1);

        let (text, spec) = split_cell_spec("value 2.3+^.^h");
        assert_eq!(text, "value");
        let spec = spec.expect("spans, alignment and a style letter");
        assert_eq!((spec.column_span, spec.row_span), (2, 3));
        assert_eq!(spec.align, pb::Alignment::Center);
        assert_eq!(spec.valign, pb::Alignment::Middle);
        assert!(spec.is_header());

        let (text, spec) = split_cell_spec("3*");
        assert_eq!(text, "");
        assert_eq!(spec.expect("a duplication factor").repeat, 3);
    }

    #[test]
    fn prose_is_not_mistaken_for_a_specifier() {
        for prose in ["Eth", "plain", "value 12", "value >x"] {
            let (text, spec) = split_cell_spec(prose);
            assert_eq!(text, prose, "{prose}");
            assert!(spec.is_none(), "{prose}");
        }
    }

    #[test]
    fn a_multi_byte_whitespace_before_the_last_word_is_a_boundary_not_a_crash() {
        for text in ["Nom\u{a0}complet", "fin\u{2003}", "\u{3000}mot"] {
            let (prose, spec) = split_cell_spec(text);
            assert!(spec.is_none(), "{text:?}: {spec:?}");
            assert_eq!(prose, text.trim_end());
        }
        for text in ["Nom\u{a0}complet\u{a0}2+", "Nom\u{a0}complet\u{3000}2+"] {
            let (prose, spec) = split_cell_spec(text);
            assert_eq!(prose, "Nom\u{a0}complet");
            assert_eq!(spec.expect("a span factor after the space").column_span, 2);
        }
    }

    #[test]
    fn a_row_may_open_with_a_specifier_instead_of_a_pipe() {
        let (lead, row) = table_row("2+^|Spanning").expect("a row");
        let lead = lead.expect("a leading specifier");
        assert_eq!(lead.column_span, 2);
        assert_eq!(lead.align, pb::Alignment::Center);
        assert_eq!(row, "Spanning");

        let (lead, row) = table_row("|Name |Role").expect("a row");
        assert!(lead.is_none());
        assert_eq!(row, "Name |Role");

        assert!(table_row("layout between rows").is_none());
    }

    #[test]
    fn an_anchor_line_is_recognized_and_prose_brackets_are_not() {
        assert_eq!(anchor_line("[[intro]]").as_deref(), Some("intro"));
        assert_eq!(
            anchor_line("[[intro,Introduction générale]]").as_deref(),
            Some("intro"),
            "the reference text is not the name"
        );
        assert_eq!(anchor_line("  [[intro]]  ").as_deref(), Some("intro"));
        assert_eq!(anchor_line("[[ ]]"), None);
        assert_eq!(anchor_line("[[a b]]"), None);
        assert_eq!(anchor_line("voir [[intro]] plus bas"), None);
    }

    #[test]
    fn inline_anchors_come_out_of_the_text_at_the_position_they_stood() {
        let (text, spans) = take_anchors("Le café [[goût]] est prêt [[fin]]");
        assert_eq!(text, "Le café  est prêt ");
        assert_eq!(spans.len(), 2);
        let first = spans[0].range.expect("a range");
        assert_eq!(
            (first.start, first.end),
            (8, 8),
            "an anchor names a position, and the offsets count code points"
        );
        assert_eq!(spans[0].anchor_id.as_deref(), Some("goût"));
        let second = spans[1].range.expect("a range");
        assert_eq!((second.start, second.end), (18, 18));
        assert_eq!(spans[1].anchor_id.as_deref(), Some("fin"));
    }

    #[test]
    fn brackets_that_are_not_an_anchor_stay_in_the_text() {
        let (text, spans) = take_anchors("un tableau [[1]] et [[a b]] et [[ouvert");
        assert_eq!(text, "un tableau  et [[a b]] et [[ouvert");
        assert_eq!(spans.len(), 1, "only the well-formed id is an anchor");
        assert_eq!(spans[0].anchor_id.as_deref(), Some("1"));
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
