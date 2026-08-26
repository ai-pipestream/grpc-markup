// SPDX-License-Identifier: Apache-2.0

//! The shared parse driver: format resolution, the fold registry, and the
//! [`Emitter`] that owns everything the contract promises about the response
//! stream — exactly one `info` first, content events in document order,
//! dense indexes across every event kind, aggregated warnings, and exactly
//! one `status` trailer.
//!
//! A format fold is a plain function from the document text to calls on the
//! emitter. Folds hold no protocol knowledge: they cannot reorder the
//! envelope, mint their own indexes, or send a trailer, because the emitter
//! is the only way onto the wire. Adding a format to this service is one
//! fold module plus one [`REGISTRY`] entry; `GetServiceInfo.formats`, the
//! dispatch and the `UNIMPLEMENTED` refusals all follow from the registry.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::document::v1 as doc;
use crate::document_fold::DocumentFold;
use crate::proto::v1 as pb;
use crate::sniff::{self, Evidence, Format};
use crate::{COLLECTOR, VERSION, asciidoc, boxnote, docling_json, html, latex, markdown, vtt};

/// Upper bound on distinct aggregated warnings kept for one parse.
///
/// Warnings aggregate by (code, message), so a normal document produces a
/// handful. A hostile one could mint a distinct message per block, which is
/// unbounded memory on the trailer; past this many the driver stops adding
/// new kinds and keeps counting the ones it has.
const MAX_WARNING_KINDS: usize = 64;

/// Consumer of parse events; returns `false` when the client is gone and the
/// parse should stop.
pub type EmitFn<'a> = &'a mut dyn FnMut(pb::ParseMarkupResponse) -> bool;

/// One format fold: the whole document text in, emitter calls out.
pub type FoldFn = fn(&str, &mut Emitter<'_>) -> Result<(), ParseError>;

/// Every format this build maps, each as one module plus this one entry.
///
/// The order here is the order `GetServiceInfo.formats` reports and the
/// order [`crate::metrics::Metrics::by_format`] indexes
/// (via [`Format::slot`]). A format in the wire enum but not in this table
/// is refused with `UNIMPLEMENTED` naming it.
pub const REGISTRY: &[(Format, FoldFn)] = &[
    (Format::Markdown, markdown::fold),
    (Format::Vtt, vtt::fold),
    (Format::Boxnote, boxnote::fold),
    (Format::DoclingJson, docling_json::fold),
    (Format::Html, html::fold),
    (Format::Asciidoc, asciidoc::fold),
    (Format::Latex, latex::fold),
];

/// The fold for a format, or `None` when this build does not map it.
#[must_use]
pub fn fold_for(format: Format) -> Option<FoldFn> {
    REGISTRY
        .iter()
        .find(|(entry, _)| *entry == format)
        .map(|(_, fold)| *fold)
}

/// Settings for one parse.
#[derive(Debug, Clone, Copy)]
pub struct ParseConfig {
    /// Format the caller asked for, or `None` to sniff.
    pub format: Option<Format>,
    /// Also fold the parse into one Document, sent just before the trailer.
    pub emit_document: bool,
    /// Carry the inline formatting, link and reference runs the folds
    /// compute while flattening. On by default; clearing it makes the
    /// stream exactly as narrow as it was before spans existed.
    pub emit_inline_spans: bool,
}

impl Default for ParseConfig {
    /// Spans on: every fold has the structure in hand at the moment it
    /// flattens, so carrying it costs a vector and losing it costs the
    /// document's link graph.
    fn default() -> Self {
        Self {
            format: None,
            emit_document: false,
            emit_inline_spans: true,
        }
    }
}

/// Why a parse ended early.
///
/// The variants are the gRPC status codes the fleet rules fix, named for the
/// cause rather than the code so the mapping in [`crate::service`] is the
/// only place that knows about gRPC.
#[derive(Debug, Clone)]
pub enum ParseError {
    /// Input the format's own error rules cannot repair. `INVALID_ARGUMENT`.
    Invalid(String),
    /// A format this build does not map, or a sniff that found nothing.
    /// `UNIMPLEMENTED`.
    Unsupported(String),
    /// A fault in this server, not in the document. `INTERNAL`.
    Internal(String),
    /// The client stopped reading. No status is sent.
    ConsumerGone,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Unsupported(m) => f.write_str(m),
            Self::Internal(m) => write!(f, "parser fault: {m}"),
            Self::ConsumerGone => f.write_str("client stopped reading"),
        }
    }
}

/// Parse one document, emitting events as the fold produces them.
///
/// `started` is when the first request frame arrived, so the trailer's
/// `elapsed_millis` covers the upload and not only the fold.
///
/// # Errors
///
/// Any [`ParseError`]. A parse that returns `Ok` has already emitted its
/// `ParseStatus` trailer; a parse that returns `Err` has emitted none.
pub fn run(
    bytes: &[u8],
    config: &ParseConfig,
    started: Instant,
    emit: EmitFn<'_>,
) -> Result<Format, ParseError> {
    let (text, lossy) = decode(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let (format, evidence) =
        sniff::resolve(config.format, text).map_err(|e| ParseError::Unsupported(e.to_string()))?;
    let Some(fold) = fold_for(format) else {
        return Err(ParseError::Unsupported(unmapped(format, evidence)));
    };

    let mut emitter = Emitter::new(format, evidence, bytes.len() as u64, started, config, emit);
    if lossy {
        emitter.warn(
            pb::WarningCode::EncodingSubstitution,
            "bytes were not valid UTF-8; malformed sequences were replaced with U+FFFD",
        );
    }
    fold(text, &mut emitter)?;
    emitter.finish()?;
    Ok(format)
}

/// The document bytes as text, and whether any byte had to be replaced.
fn decode(bytes: &[u8]) -> (String, bool) {
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => (text, false),
        Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
    }
}

/// A [`pb::MetaValue`] holding one verbatim scalar, so a fold whose
/// metadata is naturally flat stays a one-liner per key.
#[must_use]
pub fn scalar(text: impl Into<String>) -> pb::MetaValue {
    pb::MetaValue {
        kind: Some(pb::meta_value::Kind::Value(text.into())),
    }
}

/// The `UNIMPLEMENTED` message for a format outside this build.
fn unmapped(format: Format, evidence: Evidence) -> String {
    let mapped = "this build maps markdown, vtt, boxnote, docling-json, html, asciidoc and latex";
    match evidence {
        Evidence::Requested => {
            format!("the {} fold is not in this build; {mapped}", format.model())
        }
        Evidence::Signature | Evidence::Heuristic => format!(
            "the bytes sniff as {}, but the {} fold is not in this build; {mapped}",
            format.model(),
            format.model()
        ),
    }
}

/// A block the folds hand to [`Emitter::text_block`]: every field of a
/// [`pb::TextBlock`] the fold decides, with the stream index and the
/// provenance left to the emitter.
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// Structural label.
    pub label: pb::BlockLabel,
    /// Format-specific refinement of the label; empty when the label says
    /// everything.
    pub role: String,
    /// The block's text. A fold never hands over an empty one.
    pub text: String,
    /// Heading depth for section headers.
    pub level: Option<u32>,
    /// For list items: true when the list is numbered.
    pub enumerated: bool,
    /// For list items: the marker as written.
    pub marker: Option<String>,
    /// For list items: nesting depth of the list, starting at 1.
    pub list_depth: Option<u32>,
    /// For code: the language the source declares, verbatim.
    pub language: Option<String>,
    /// Hyperlink target when the entire block is one link.
    pub hyperlink: Option<String>,
    /// One-based source line, when the fold tracks lines.
    pub source_line: Option<u32>,
    /// Formatting, link and reference runs inside `text`, in start order.
    pub spans: Vec<pb::InlineSpan>,
    /// The anchor the source declares on this block.
    pub anchor_id: Option<String>,
    /// The callout kind the source marks the block as.
    pub admonition: Option<pb::AdmonitionKind>,
    /// For list items: the checkbox the source put on the item.
    pub task_state: Option<pb::TaskState>,
    /// For quotes: nesting depth of the quotation, starting at 1.
    pub quote_depth: Option<u32>,
}

/// An image reference a fold hands to [`Emitter::image`]: every field of a
/// [`pb::ImageBlock`] the fold decides, with the stream index and the
/// provenance left to the emitter.
#[derive(Debug, Clone, Default)]
pub struct Image {
    /// The reference as written, never dereferenced.
    pub src: String,
    /// Alternative text, empty when the source has none.
    pub alt: String,
    /// Title or tooltip text, empty when the source has none.
    pub title: String,
    /// One-based source line, when the fold tracks lines.
    pub source_line: Option<u32>,
    /// The width the source declares, verbatim in its own units.
    pub width: Option<String>,
    /// The height the source declares, verbatim in its own units.
    pub height: Option<String>,
}

/// A cue the WebVTT fold hands to [`Emitter::cue`]: every field of a
/// [`pb::Cue`] the fold decides, with the stream index and the provenance
/// left to the emitter.
#[derive(Debug, Clone, Default)]
pub struct Cue {
    /// The cue identifier line, when the cue has one.
    pub cue_id: Option<String>,
    /// Cue start, in milliseconds from the start of the media.
    pub start_ms: u64,
    /// Cue end, in milliseconds from the start of the media.
    pub end_ms: u64,
    /// The speaker a `<v>` annotation named, when the cue names one.
    pub speaker: Option<String>,
    /// The payload with its tags removed, lines joined with `\n`.
    pub text: String,
    /// Style, class and language runs inside `text`, in start order.
    pub spans: Vec<pb::InlineSpan>,
    /// Karaoke segments of `text`, empty when the cue times nothing inside
    /// itself.
    pub timings: Vec<pb::CueTiming>,
}

/// The one way onto the wire.
///
/// Owns the response-stream contract so the folds cannot break it: `info`
/// first exactly once, one dense index series across text blocks, table
/// starts, images and cues, warnings aggregated per (code, message), the
/// optional Document immediately before the trailer, and the trailer last
/// exactly once — via [`finish`](Self::finish), which only the driver calls.
pub struct Emitter<'a> {
    format: Format,
    evidence: Evidence,
    bytes_consumed: u64,
    started: Instant,
    emit: EmitFn<'a>,
    source: pb::CollectorSource,
    info_sent: bool,
    index: u64,
    counts: pb::ParseCounts,
    warnings: BTreeMap<(i32, String), u64>,
    /// Tables opened so far, for minting `table_ref`s.
    tables_opened: u64,
    /// The open table: (ref, rows sent, widest row after span expansion).
    open_table: Option<(String, u32, u32)>,
    /// The event-driven fold, when the caller asked for a Document.
    fold: Option<DocumentFold>,
    /// A fold that ingests a whole Document ([`docling_json`]) sets the
    /// result here instead of relying on the event fold.
    document: Option<doc::Document>,
    /// Whether the inline runs the folds compute reach the wire.
    emit_inline_spans: bool,
}

impl<'a> Emitter<'a> {
    /// An emitter for one parse.
    fn new(
        format: Format,
        evidence: Evidence,
        bytes_consumed: u64,
        started: Instant,
        config: &ParseConfig,
        emit: EmitFn<'a>,
    ) -> Self {
        Self {
            format,
            evidence,
            bytes_consumed,
            started,
            emit,
            source: pb::CollectorSource {
                collector: COLLECTOR.to_owned(),
                model: Some(format.model().to_owned()),
                version: Some(VERSION.to_owned()),
                confidence: None,
            },
            info_sent: false,
            index: 0,
            counts: pb::ParseCounts::default(),
            warnings: BTreeMap::new(),
            tables_opened: 0,
            open_table: None,
            fold: config.emit_document.then(DocumentFold::new),
            document: None,
            emit_inline_spans: config.emit_inline_spans,
        }
    }

    /// The format this parse resolved to, for fold-side decisions.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.format
    }

    /// True when the caller asked for the Document projection, so a fold
    /// that can build one directly knows whether to bother.
    #[must_use]
    pub const fn wants_document(&self) -> bool {
        self.fold.is_some() || self.document.is_some()
    }

    /// Send the `MarkupInfo` header. Must be the fold's first emission.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn info(
        &mut self,
        title: Option<String>,
        metadata: std::collections::HashMap<String, pb::MetaValue>,
    ) -> Result<(), ParseError> {
        assert!(!self.info_sent, "MarkupInfo is sent exactly once");
        self.info_sent = true;
        self.send(pb::parse_markup_response::Event::Info(pb::MarkupInfo {
            format: self.format.to_proto() as i32,
            evidence: self.evidence.to_proto() as i32,
            title,
            metadata,
        }))
    }

    /// Send one text block, stamping its index and provenance.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn text_block(&mut self, block: Block) -> Result<(), ParseError> {
        debug_assert!(!block.text.is_empty(), "TextBlock.text is never empty");
        let index = self.next_index();
        self.counts.text_blocks += 1;
        let spans = self.kept_spans(block.spans);
        self.send(pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
            index,
            label: block.label as i32,
            role: block.role,
            text: block.text,
            level: block.level,
            enumerated: block.enumerated,
            marker: block.marker,
            list_depth: block.list_depth,
            language: block.language,
            hyperlink: block.hyperlink,
            source_line: block.source_line,
            source: Some(self.source.clone()),
            spans,
            anchor_id: block.anchor_id,
            admonition: block.admonition.unwrap_or(pb::AdmonitionKind::Unspecified) as i32,
            task_state: block.task_state.unwrap_or(pb::TaskState::Unspecified) as i32,
            quote_depth: block.quote_depth,
        }))
    }

    /// Open a table, returning the `table_ref` its rows and end must carry.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn table_start(
        &mut self,
        caption: Option<String>,
        source_line: Option<u32>,
    ) -> Result<String, ParseError> {
        self.table_start_with_columns(caption, source_line, Vec::new())
    }

    /// Open a table whose source declares its column geometry.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn table_start_with_columns(
        &mut self,
        caption: Option<String>,
        source_line: Option<u32>,
        columns: Vec<pb::ColumnSpec>,
    ) -> Result<String, ParseError> {
        let table_ref = format!("t{}", self.tables_opened);
        self.tables_opened += 1;
        let index = self.next_index();
        self.counts.tables += 1;
        self.open_table = Some((table_ref.clone(), 0, 0));
        self.send(pb::parse_markup_response::Event::TableStart(
            pb::TableStart {
                index,
                table_ref: table_ref.clone(),
                caption,
                source_line,
                source: Some(self.source.clone()),
                columns,
            },
        ))?;
        Ok(table_ref)
    }

    /// Send one row of the open table.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn table_row(
        &mut self,
        is_header: bool,
        mut cells: Vec<pb::TableCell>,
    ) -> Result<(), ParseError> {
        if !self.emit_inline_spans {
            for cell in &mut cells {
                cell.spans.clear();
            }
        }
        let (table_ref, row_index) = {
            let open = self
                .open_table
                .as_mut()
                .expect("table_row is only sent inside an open table");
            let row_index = open.1;
            open.1 += 1;
            // The widest row after span expansion, for the closing event.
            let row_width = cells
                .last()
                .map_or(0, |cell| cell.column_index + cell.column_span.max(1));
            open.2 = open.2.max(row_width);
            (open.0.clone(), row_index)
        };
        self.counts.table_rows += 1;
        self.send(pb::parse_markup_response::Event::TableRow(pb::TableRow {
            table_ref,
            row_index,
            is_header,
            cells,
        }))
    }

    /// Close the open table, reporting its row and column counts.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn table_end(&mut self) -> Result<(), ParseError> {
        let (table_ref, row_count, column_count) = self
            .open_table
            .take()
            .expect("table_end closes an open table");
        self.send(pb::parse_markup_response::Event::TableEnd(pb::TableEnd {
            table_ref,
            row_count,
            column_count,
        }))
    }

    /// Send one image reference.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn image(&mut self, image: Image) -> Result<(), ParseError> {
        let index = self.next_index();
        self.counts.images += 1;
        self.send(pb::parse_markup_response::Event::Image(pb::ImageBlock {
            index,
            src: image.src,
            alt: image.alt,
            title: image.title,
            source_line: image.source_line,
            source: Some(self.source.clone()),
            width: image.width,
            height: image.height,
        }))
    }

    /// Send one WebVTT cue.
    ///
    /// `timings` are not gated by `emit_inline_spans`: a karaoke segment is
    /// a coordinate the source stated, not a rendering signal, and a
    /// consumer that asked for flat text still asked for the timings.
    ///
    /// # Errors
    ///
    /// [`ParseError::ConsumerGone`] when the client stopped reading.
    pub fn cue(&mut self, cue: Cue) -> Result<(), ParseError> {
        let index = self.next_index();
        self.counts.cues += 1;
        let spans = self.kept_spans(cue.spans);
        self.send(pb::parse_markup_response::Event::Cue(pb::Cue {
            index,
            cue_id: cue.cue_id,
            start_ms: cue.start_ms,
            end_ms: cue.end_ms,
            speaker: cue.speaker.unwrap_or_default(),
            text: cue.text,
            source: Some(self.source.clone()),
            spans,
            timings: cue.timings,
        }))
    }

    /// Record one non-fatal finding, aggregated per (code, message).
    pub fn warn(&mut self, code: pb::WarningCode, message: &str) {
        self.warn_n(code, message, 1);
    }

    /// Record a finding the fold has already counted, so ten thousand
    /// occurrences cost one call rather than ten thousand.
    pub fn warn_n(&mut self, code: pb::WarningCode, message: &str, count: u64) {
        if count == 0 {
            return;
        }
        let key = (code as i32, message.to_owned());
        if let Some(existing) = self.warnings.get_mut(&key) {
            *existing += count;
        } else if self.warnings.len() < MAX_WARNING_KINDS {
            self.warnings.insert(key, count);
        }
    }

    /// Hand over a fully built Document instead of the event-driven fold.
    ///
    /// [`docling_json`] uses this: its payload already is a Document, and
    /// carrying it over loses nothing, whereas re-folding the projected
    /// events would. A no-op when the caller did not ask for a Document.
    pub fn set_document(&mut self, document: doc::Document) {
        if self.fold.take().is_some() || self.document.is_some() {
            self.document = Some(document);
        }
    }

    /// Send the optional Document and the `ParseStatus` trailer.
    fn finish(&mut self) -> Result<(), ParseError> {
        assert!(self.info_sent, "a fold emits MarkupInfo before finishing");
        assert!(
            self.open_table.is_none(),
            "a fold closes every table it opens"
        );
        let document = self
            .document
            .take()
            .or_else(|| self.fold.take().map(|mut fold| fold.take()));
        if let Some(document) = document {
            self.send(pb::parse_markup_response::Event::Document(document))?;
        }
        let warnings = self
            .warnings
            .iter()
            .map(|((code, message), count)| pb::ParseWarning {
                code: *code,
                message: message.clone(),
                count: *count,
            })
            .collect();
        let status = pb::ParseStatus {
            format: self.format.to_proto() as i32,
            counts: Some(self.counts),
            warnings,
            bytes_consumed: self.bytes_consumed,
            elapsed_millis: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        self.send(pb::parse_markup_response::Event::Status(status))
    }

    /// The runs a block keeps: all of them, or none when the request asked
    /// for a flat stream.
    fn kept_spans(&self, spans: Vec<pb::InlineSpan>) -> Vec<pb::InlineSpan> {
        if self.emit_inline_spans {
            spans
        } else {
            Vec::new()
        }
    }

    /// The next stream index, shared across every indexed event kind.
    fn next_index(&mut self) -> u64 {
        let index = self.index;
        self.index += 1;
        index
    }

    /// Put one event on the wire, feeding the Document fold on the way out.
    fn send(&mut self, event: pb::parse_markup_response::Event) -> Result<(), ParseError> {
        if let Some(fold) = self.fold.as_mut() {
            fold.consume(&event);
        }
        if (self.emit)(pb::ParseMarkupResponse { event: Some(event) }) {
            Ok(())
        } else {
            Err(ParseError::ConsumerGone)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a parse against an in-memory sink, collecting every event.
    fn collect(
        bytes: &[u8],
        config: &ParseConfig,
    ) -> Result<(Format, Vec<pb::ParseMarkupResponse>), ParseError> {
        let mut events = Vec::new();
        let mut emit = |event: pb::ParseMarkupResponse| {
            events.push(event);
            true
        };
        let format = run(bytes, config, Instant::now(), &mut emit)?;
        Ok((format, events))
    }

    #[test]
    fn the_registry_maps_every_contract_format() {
        let formats: Vec<Format> = REGISTRY.iter().map(|(format, _)| *format).collect();
        assert_eq!(
            formats,
            [
                Format::Markdown,
                Format::Vtt,
                Format::Boxnote,
                Format::DoclingJson,
                Format::Html,
                Format::Asciidoc,
                Format::Latex,
            ]
        );
        for (format, _) in REGISTRY {
            assert!(fold_for(*format).is_some(), "{format:?}");
        }
    }

    #[test]
    fn the_unmapped_refusal_still_names_a_format_the_registry_would_lose() {
        // Every contract format is mapped today; the refusal path stays
        // because the registry, not the enum, is the source of truth. This
        // pins the message shape a future partial build would produce.
        let requested = unmapped(Format::Latex, Evidence::Requested);
        assert!(requested.contains("latex fold is not in this build"));
        let sniffed = unmapped(Format::Html, Evidence::Signature);
        assert!(sniffed.contains("sniff as html"), "{sniffed}");
    }

    #[test]
    fn invalid_utf8_is_replaced_and_warned_not_refused() {
        let mut bytes = b"# Title\n\nbody ".to_vec();
        bytes.push(0xFF);
        bytes.push(b'!');
        let config = ParseConfig {
            format: Some(Format::Markdown),
            emit_document: false,
            ..ParseConfig::default()
        };
        let (_, events) = collect(&bytes, &config).expect("lossy parse succeeds");
        let Some(pb::parse_markup_response::Event::Status(status)) =
            events.last().and_then(|e| e.event.as_ref())
        else {
            panic!("last event is the trailer");
        };
        assert!(
            status
                .warnings
                .iter()
                .any(|w| w.code == pb::WarningCode::EncodingSubstitution as i32),
            "{:?}",
            status.warnings
        );
        assert_eq!(status.bytes_consumed, bytes.len() as u64);
    }

    #[test]
    fn a_bom_is_stripped_before_sniffing() {
        let text = "\u{feff}WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n";
        let (format, _) = collect(text.as_bytes(), &ParseConfig::default()).expect("parses");
        assert_eq!(format, Format::Vtt);
    }

    #[test]
    fn warnings_aggregate_per_code_and_message() {
        let mut events = Vec::new();
        let mut emit = |event: pb::ParseMarkupResponse| {
            events.push(event);
            true
        };
        let config = ParseConfig::default();
        let mut emitter = Emitter::new(
            Format::Markdown,
            Evidence::Requested,
            0,
            Instant::now(),
            &config,
            &mut emit,
        );
        for _ in 0..10_000 {
            emitter.warn(pb::WarningCode::UnmappedConstruct, "thematic break");
        }
        emitter.warn(pb::WarningCode::UnmappedConstruct, "something else");
        emitter
            .info(None, std::collections::HashMap::new())
            .unwrap();
        emitter.finish().unwrap();
        let Some(pb::parse_markup_response::Event::Status(status)) =
            events.last().and_then(|e| e.event.as_ref())
        else {
            panic!("trailer last");
        };
        assert_eq!(status.warnings.len(), 2);
        let big = status
            .warnings
            .iter()
            .find(|w| w.message == "thematic break")
            .expect("aggregated");
        assert_eq!(big.count, 10_000);
    }

    #[test]
    fn the_warning_table_is_bounded_against_hostile_variety() {
        let mut emit = |_| true;
        let config = ParseConfig::default();
        let mut emitter = Emitter::new(
            Format::Markdown,
            Evidence::Requested,
            0,
            Instant::now(),
            &config,
            &mut emit,
        );
        for n in 0..10_000 {
            emitter.warn(pb::WarningCode::UnmappedConstruct, &format!("kind {n}"));
        }
        assert_eq!(emitter.warnings.len(), MAX_WARNING_KINDS);
    }

    #[test]
    fn a_gone_consumer_stops_the_parse() {
        let config = ParseConfig {
            format: Some(Format::Markdown),
            emit_document: false,
            ..ParseConfig::default()
        };
        let mut emit = |_| false;
        let result = run(b"# T\n\nbody\n", &config, Instant::now(), &mut emit);
        assert!(matches!(result, Err(ParseError::ConsumerGone)));
    }
}
