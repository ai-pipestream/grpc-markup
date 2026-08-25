// SPDX-License-Identifier: Apache-2.0

//! Inline flattening that remembers where the runs went.
//!
//! Every fold in this crate flattens inline markup into one string. The
//! folds used to compute the emphasis, link and reference structure while
//! doing it and then drop it on the floor; [`Inline`] is the same flattener
//! with the offsets kept, so a `**bold**` run leaves a
//! [`pb::InlineSpan`](crate::proto::v1::InlineSpan) behind instead of
//! nothing.
//!
//! Two properties the folds depend on:
//!
//! - **Offsets are code points, not bytes.** The buffer is built one
//!   `char` at a time and counted as it is built, so a range is valid for a
//!   consumer indexing the same string as Unicode scalar values however
//!   many bytes the text takes.
//! - **A run starts where its first visible character lands.** Marks open
//!   before the text arrives, and the start offset is fixed at the moment a
//!   character is actually appended, so a mark that opens while a collapsed
//!   space is pending does not swallow the space.
//!
//! Nesting is a stack: [`Inline::open`] and [`Inline::close`] pair the way
//! the source's own start and end events do, and a run that ends up empty
//! (a `**` with nothing but whitespace in it) leaves no span at all.

use crate::proto::v1 as pb;

/// What one open run carries, before its offsets are known.
#[derive(Debug, Clone)]
pub struct Mark {
    /// Rendering signals of the run.
    pub styles: Vec<pb::SpanStyle>,
    /// Link target, as the source wrote it.
    pub hyperlink: Option<String>,
    /// The link's title or tooltip text.
    pub link_title: Option<String>,
    /// The anchor this run defines.
    pub anchor_id: Option<String>,
    /// The key this run points at.
    pub reference: Option<String>,
    /// What `reference` points at.
    pub reference_kind: pb::ReferenceKind,
    /// Source notation flattening lost.
    pub raw: Option<String>,
    /// The run's own font family, as the source names it.
    pub font_family: Option<String>,
    /// The run's own font size, in points.
    pub font_size_pt: Option<f64>,
    /// The run's own text color, as `#rrggbb`.
    pub color: Option<String>,
}

impl Default for Mark {
    fn default() -> Self {
        Self {
            styles: Vec::new(),
            hyperlink: None,
            link_title: None,
            anchor_id: None,
            reference: None,
            reference_kind: pb::ReferenceKind::Unspecified,
            raw: None,
            font_family: None,
            font_size_pt: None,
            color: None,
        }
    }
}

impl Mark {
    /// A run carrying one rendering signal and nothing else.
    #[must_use]
    pub fn style(style: pb::SpanStyle) -> Self {
        Self {
            styles: vec![style],
            ..Self::default()
        }
    }

    /// A linked run.
    #[must_use]
    pub fn link(url: &str, title: Option<&str>) -> Self {
        Self {
            hyperlink: Some(url.to_owned()),
            link_title: title.filter(|t| !t.is_empty()).map(str::to_owned),
            ..Self::default()
        }
    }

    /// A run pointing at a key elsewhere in the document.
    #[must_use]
    pub fn reference(key: &str, kind: pb::ReferenceKind) -> Self {
        Self {
            reference: Some(key.to_owned()),
            reference_kind: kind,
            ..Self::default()
        }
    }

    /// The same run with one more rendering signal.
    #[must_use]
    pub fn with_style(mut self, style: pb::SpanStyle) -> Self {
        self.styles.push(style);
        self
    }

    /// The same run with the source notation flattening would have lost.
    #[must_use]
    pub fn with_raw(mut self, raw: &str) -> Self {
        self.raw = Some(raw.to_owned());
        self
    }

    /// True when the run says nothing at all, so opening it is pointless.
    fn is_silent(&self) -> bool {
        self.styles.is_empty()
            && self.hyperlink.is_none()
            && self.anchor_id.is_none()
            && self.reference.is_none()
            && self.raw.is_none()
            && self.font_family.is_none()
            && self.font_size_pt.is_none()
            && self.color.is_none()
    }
}

/// One inline text being flattened, with the runs recorded.
#[derive(Debug, Default)]
pub struct Inline {
    /// The flattened text.
    buf: String,
    /// Code points in `buf`.
    chars: u32,
    /// A collapsed whitespace run waiting for the next visible character.
    pending_space: bool,
    /// Open marks, innermost last, each with the offset it started at once
    /// a character has landed inside it.
    open: Vec<(Mark, Option<u32>)>,
    /// Closed runs, in close order; [`Inline::take`] puts them in start
    /// order.
    spans: Vec<pb::InlineSpan>,
    /// Destination of the first link seen.
    url: Option<String>,
    /// Links seen.
    links: usize,
    /// Depth of open links, so text inside one is not "outside".
    open_links: usize,
    /// Non-whitespace text seen outside any link.
    outside: bool,
}

impl Inline {
    /// Append inline text, collapsing runs of whitespace to one space.
    pub fn push(&mut self, text: &str) {
        if self.open_links == 0 && !text.trim().is_empty() {
            self.outside = true;
        }
        for c in text.chars() {
            if c.is_whitespace() {
                self.pending_space = !self.buf.is_empty();
            } else {
                if self.pending_space {
                    self.buf.push(' ');
                    self.chars += 1;
                    self.pending_space = false;
                }
                self.start_open();
                self.buf.push(c);
                self.chars += 1;
            }
        }
    }

    /// Record a word break: a soft or hard line break, an element boundary.
    pub fn space(&mut self) {
        self.pending_space = !self.buf.is_empty();
    }

    /// True when nothing visible has been pushed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The code points appended so far, which is where the next character
    /// lands.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.chars
    }

    /// Open a run. Every open run is closed by [`Inline::close`], innermost
    /// first, exactly as the source's own start and end events pair.
    pub fn open(&mut self, mark: Mark) {
        if mark.is_silent() {
            // Nothing to say about the run; still stacked so `close` stays
            // paired with `open` from the caller's side.
            self.open.push((mark, Some(u32::MAX)));
            return;
        }
        self.open.push((mark, None));
    }

    /// Close the innermost open run, recording it when it covered text.
    pub fn close(&mut self) {
        let Some((mark, start)) = self.open.pop() else {
            return;
        };
        let Some(start) = start.filter(|start| *start < self.chars) else {
            return;
        };
        self.spans.push(pb::InlineSpan {
            range: Some(pb::IntRange {
                start,
                end: self.chars,
            }),
            styles: mark.styles.iter().map(|style| *style as i32).collect(),
            hyperlink: mark.hyperlink,
            link_title: mark.link_title,
            anchor_id: mark.anchor_id,
            reference: mark.reference,
            reference_kind: mark.reference_kind as i32,
            raw: mark.raw,
            font_family: mark.font_family,
            font_size_pt: mark.font_size_pt,
            color: mark.color,
            // A per-run language tag and a field code are things a word
            // processor declares; no markup format in this build does.
            language: None,
            field_code: None,
        });
    }

    /// Open a link run and count it for the whole-block link rule.
    pub fn open_link(&mut self, url: &str, title: Option<&str>) {
        self.open_link_mark(Mark::link(url, title));
    }

    /// Open a link run described by a whole mark, so a link that also
    /// declares an anchor or points at a fragment stays one run rather
    /// than two overlapping ones.
    pub fn open_link_mark(&mut self, mark: Mark) {
        if self.links == 0 {
            self.url.clone_from(&mark.hyperlink);
        }
        self.links += 1;
        self.open_links += 1;
        self.open(mark);
    }

    /// Close the innermost link run.
    pub fn close_link(&mut self) {
        self.open_links = self.open_links.saturating_sub(1);
        self.close();
    }

    /// Push one run of text wrapped in a mark, for a source construct that
    /// arrives as a single unit (a footnote marker, a citation key).
    pub fn push_marked(&mut self, text: &str, mark: Mark) {
        self.open(mark);
        self.push(text);
        self.close();
    }

    /// Record that content the flattener cannot see sits outside any link,
    /// so the block is more than one link even though no text says so.
    pub fn mark_outside(&mut self) {
        if self.open_links == 0 {
            self.outside = true;
        }
    }

    /// The block's hyperlink when the block is, in its entirety, one link.
    ///
    /// Kept for `TextBlock.hyperlink`, which predates spans and stays the
    /// answer to "is this whole item a link"; every other link now survives
    /// as a span.
    #[must_use]
    pub fn hyperlink(&self) -> Option<String> {
        if self.links == 1 && !self.outside {
            self.url.clone()
        } else {
            None
        }
    }

    /// The flattened text and its runs, in start order, outermost first
    /// where two runs start together. The flattener is empty afterwards.
    pub fn take(&mut self) -> (String, Vec<pb::InlineSpan>) {
        self.pending_space = false;
        self.chars = 0;
        self.open.clear();
        let mut spans = std::mem::take(&mut self.spans);
        spans.sort_by_key(|span| {
            let range = span.range.unwrap_or_default();
            (range.start, u32::MAX - range.end)
        });
        (std::mem::take(&mut self.buf), spans)
    }

    /// Fix the start offset of every open run that has not seen a character
    /// yet: the run begins where the character about to be pushed lands.
    fn start_open(&mut self) {
        for (_, start) in &mut self.open {
            if start.is_none() {
                *start = Some(self.chars);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(span: &pb::InlineSpan) -> (u32, u32) {
        let range = span.range.unwrap_or_default();
        (range.start, range.end)
    }

    #[test]
    fn a_run_covers_exactly_its_own_text() {
        let mut inline = Inline::default();
        inline.push("see ");
        inline.open(Mark::style(pb::SpanStyle::Bold));
        inline.push("this");
        inline.close();
        inline.push(" now");
        let (text, spans) = inline.take();
        assert_eq!(text, "see this now");
        assert_eq!(spans.len(), 1);
        assert_eq!(range(&spans[0]), (4, 8));
        assert_eq!(&text[4..8], "this");
    }

    #[test]
    fn offsets_count_code_points_not_bytes() {
        let mut inline = Inline::default();
        inline.push("héllo wörld ");
        inline.open(Mark::style(pb::SpanStyle::Italic));
        inline.push("λόγος");
        inline.close();
        let (text, spans) = inline.take();
        assert_eq!(text, "héllo wörld λόγος");
        // Twelve code points precede the run; the bytes are far more.
        assert_eq!(range(&spans[0]), (12, 17));
        assert!(text.len() > 17, "the byte length is not the code point one");
        let run: String = text.chars().skip(12).take(5).collect();
        assert_eq!(run, "λόγος");
    }

    #[test]
    fn a_pending_space_stays_outside_the_run_that_follows_it() {
        let mut inline = Inline::default();
        inline.push("a\n");
        inline.open(Mark::style(pb::SpanStyle::Bold));
        inline.push("  b");
        inline.close();
        let (text, spans) = inline.take();
        assert_eq!(text, "a b");
        assert_eq!(range(&spans[0]), (2, 3), "the collapsed space is not bold");
    }

    #[test]
    fn nested_runs_both_survive_and_come_back_in_start_order() {
        let mut inline = Inline::default();
        inline.open(Mark::style(pb::SpanStyle::Bold));
        inline.push("bold ");
        inline.open(Mark::style(pb::SpanStyle::Italic));
        inline.push("both");
        inline.close();
        inline.close();
        let (_, spans) = inline.take();
        assert_eq!(range(&spans[0]), (0, 9), "the outer run comes first");
        assert_eq!(spans[0].styles, vec![pb::SpanStyle::Bold as i32]);
        assert_eq!(range(&spans[1]), (5, 9));
        assert_eq!(spans[1].styles, vec![pb::SpanStyle::Italic as i32]);
    }

    #[test]
    fn an_empty_run_leaves_no_span() {
        let mut inline = Inline::default();
        inline.open(Mark::style(pb::SpanStyle::Bold));
        inline.push("   ");
        inline.close();
        inline.push("x");
        let (text, spans) = inline.take();
        assert_eq!(text, "x");
        assert!(spans.is_empty());
    }

    #[test]
    fn every_link_becomes_a_span_and_the_whole_block_rule_still_holds() {
        let mut whole = Inline::default();
        whole.open_link("https://example.com", Some("Home"));
        whole.push("Docs");
        whole.close_link();
        assert_eq!(whole.hyperlink().as_deref(), Some("https://example.com"));
        let (_, spans) = whole.take();
        assert_eq!(spans[0].hyperlink.as_deref(), Some("https://example.com"));
        assert_eq!(spans[0].link_title.as_deref(), Some("Home"));

        let mut prose = Inline::default();
        prose.push("see ");
        prose.open_link("https://a.example", None);
        prose.push("one");
        prose.close_link();
        prose.push(" and ");
        prose.open_link("https://b.example", None);
        prose.push("two");
        prose.close_link();
        assert!(prose.hyperlink().is_none(), "two links is not one link");
        let (text, spans) = prose.take();
        assert_eq!(text, "see one and two");
        assert_eq!(spans.len(), 2, "both URLs survive as spans");
        assert_eq!(range(&spans[0]), (4, 7));
        assert_eq!(range(&spans[1]), (12, 15));
    }

    #[test]
    fn a_reference_run_carries_its_key_and_kind() {
        let mut inline = Inline::default();
        inline.push("as shown in ");
        inline.push_marked(
            "knuth84",
            Mark::reference("knuth84", pb::ReferenceKind::Citation),
        );
        let (text, spans) = inline.take();
        assert_eq!(text, "as shown in knuth84");
        assert_eq!(spans[0].reference.as_deref(), Some("knuth84"));
        assert_eq!(
            spans[0].reference_kind,
            pb::ReferenceKind::Citation as i32,
            "the kind survives, not a bracketed string"
        );
    }
}
