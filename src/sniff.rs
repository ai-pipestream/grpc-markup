// SPDX-License-Identifier: Apache-2.0

//! Resolving which markup format a document belongs to.
//!
//! The order is the one design.md fixes: an explicit request wins outright,
//! then unambiguous signatures — the `WEBVTT` header, a JSON payload whose
//! shape names itself, an HTML doctype or root, a LaTeX preamble — and only
//! when no signature matched may a structure heuristic classify the bytes as
//! Markdown or AsciiDoc. The rule that matters is that the heuristic is a
//! *fallback*, not a vote: a document with a signature is never overruled by
//! its texture.
//!
//! The server never guesses between two candidates it cannot separate. Bytes
//! that look like both Markdown and AsciiDoc, or that look like nothing at
//! all, fail closed with `UNIMPLEMENTED` and a message naming what was seen,
//! rather than being parsed as whichever format was tried first.

use crate::proto::v1 as pb;

/// The seven input families the contract names.
///
/// A Rust-side mirror of [`pb::MarkupFormat`] minus its unspecified variant,
/// so the folds can match exhaustively on a value that is known to be real.
/// Which of these a build actually maps is [`crate::parse::REGISTRY`]'s
/// business, not this enum's: the sniff recognizes a format outside the
/// build so it can *name* it in an `UNIMPLEMENTED` message instead of
/// mistaking it for Markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// CommonMark Markdown, with pipe tables and YAML front matter.
    Markdown,
    /// HTML or XHTML, whole documents and fragments alike.
    Html,
    /// AsciiDoc.
    Asciidoc,
    /// LaTeX source.
    Latex,
    /// WebVTT subtitle and caption files.
    Vtt,
    /// Box Notes, the JSON note format exported by Box.
    Boxnote,
    /// A `DoclingDocument` serialized as JSON, re-ingested.
    DoclingJson,
}

impl Format {
    /// The fold name recorded in `CollectorSource.model`.
    #[must_use]
    pub const fn model(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Asciidoc => "asciidoc",
            Self::Latex => "latex",
            Self::Vtt => "vtt",
            Self::Boxnote => "boxnote",
            Self::DoclingJson => "docling-json",
        }
    }

    /// The wire enum value.
    #[must_use]
    pub const fn to_proto(self) -> pb::MarkupFormat {
        match self {
            Self::Markdown => pb::MarkupFormat::Markdown,
            Self::Html => pb::MarkupFormat::Html,
            Self::Asciidoc => pb::MarkupFormat::Asciidoc,
            Self::Latex => pb::MarkupFormat::Latex,
            Self::Vtt => pb::MarkupFormat::Vtt,
            Self::Boxnote => pb::MarkupFormat::Boxnote,
            Self::DoclingJson => pb::MarkupFormat::DoclingJson,
        }
    }

    /// The format a request asked for, or `None` for "sniff it".
    #[must_use]
    pub const fn from_proto(value: pb::MarkupFormat) -> Option<Self> {
        match value {
            pb::MarkupFormat::Unspecified => None,
            pb::MarkupFormat::Markdown => Some(Self::Markdown),
            pb::MarkupFormat::Html => Some(Self::Html),
            pb::MarkupFormat::Asciidoc => Some(Self::Asciidoc),
            pb::MarkupFormat::Latex => Some(Self::Latex),
            pb::MarkupFormat::Vtt => Some(Self::Vtt),
            pb::MarkupFormat::Boxnote => Some(Self::Boxnote),
            pb::MarkupFormat::DoclingJson => Some(Self::DoclingJson),
        }
    }

    /// The metrics slot for a format this build maps, in
    /// [`crate::parse::REGISTRY`] order; `None` for the rest.
    #[must_use]
    pub const fn slot(self) -> Option<usize> {
        match self {
            Self::Markdown => Some(0),
            Self::Vtt => Some(1),
            Self::Boxnote => Some(2),
            Self::DoclingJson => Some(3),
            Self::Html => Some(4),
            Self::Asciidoc => Some(5),
            Self::Latex => Some(6),
        }
    }
}

/// The signal a match came from, reported on `MarkupInfo` so a surprising
/// mapping can be explained without re-running the sniff by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The request stated the format.
    Requested,
    /// An unambiguous signature matched.
    Signature,
    /// No signature matched and the bytes were classified by structure.
    Heuristic,
}

impl Evidence {
    /// The wire enum value.
    #[must_use]
    pub const fn to_proto(self) -> pb::FormatEvidence {
        match self {
            Self::Requested => pb::FormatEvidence::Requested,
            Self::Signature => pb::FormatEvidence::Signature,
            Self::Heuristic => pb::FormatEvidence::Heuristic,
        }
    }
}

/// Why a document could not be assigned a format. Maps to `UNIMPLEMENTED`:
/// the bytes may be a perfectly good document, they are simply not one this
/// service can name — or they name two formats at once, and the server never
/// guesses between candidates it cannot separate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniffError {
    /// What the sniff saw, quoted back to the caller.
    pub detail: String,
}

impl std::fmt::Display for SniffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot sniff a markup format from these bytes: {}; state `format` on the request",
            self.detail
        )
    }
}

/// Resolve a format from an explicit request plus the bytes themselves.
///
/// `text` is the document with any leading BOM already stripped.
///
/// # Errors
///
/// [`SniffError`] when the request left the format unspecified and no
/// signature or heuristic matched — or two heuristics matched at once.
pub fn resolve(requested: Option<Format>, text: &str) -> Result<(Format, Evidence), SniffError> {
    if let Some(format) = requested {
        return Ok((format, Evidence::Requested));
    }
    if let Some(format) = signature(text) {
        return Ok((format, Evidence::Signature));
    }
    match (looks_like_markdown(text), looks_like_asciidoc(text)) {
        (true, false) => Ok((Format::Markdown, Evidence::Heuristic)),
        (false, true) => Ok((Format::Asciidoc, Evidence::Heuristic)),
        (true, true) => Err(SniffError {
            detail: "the structure matches both Markdown and AsciiDoc".to_owned(),
        }),
        (false, false) => Err(SniffError {
            detail: "no signature matched and the structure is neither Markdown nor AsciiDoc"
                .to_owned(),
        }),
    }
}

/// Match an unambiguous signature, or nothing.
#[must_use]
pub fn signature(text: &str) -> Option<Format> {
    if is_webvtt(text) {
        return Some(Format::Vtt);
    }
    if let Some(format) = json_signature(text) {
        return Some(format);
    }
    let head = text.trim_start();
    let lower = head
        .get(..head.len().min(64))
        .unwrap_or(head)
        .to_ascii_lowercase();
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        return Some(Format::Html);
    }
    if text.contains("\\documentclass") || text.contains("\\begin{document}") {
        return Some(Format::Latex);
    }
    None
}

/// True when the first line is the `WEBVTT` file signature: the six magic
/// bytes alone, or followed by a space or tab and an optional title.
#[must_use]
pub fn is_webvtt(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("WEBVTT") else {
        return false;
    };
    matches!(rest.chars().next(), None | Some(' ' | '\t' | '\n' | '\r'))
}

/// Match a JSON payload that names its own format.
///
/// A `schema_name` mentioning docling is a `DoclingDocument`; the ProseMirror
/// `doc` tree of a current Box Note — or the `atext` key of a legacy one —
/// is a Box Note. Bytes that merely *start* like JSON but do not parse are
/// not a signature: they fall through to the heuristics, which will refuse
/// them with a better message than a JSON syntax error.
#[must_use]
pub fn json_signature(text: &str) -> Option<Format> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let object = value.as_object()?;
    if object
        .get("schema_name")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| name.to_ascii_lowercase().contains("docling"))
    {
        return Some(Format::DoclingJson);
    }
    if object.get("doc").is_some_and(serde_json::Value::is_object) || object.contains_key("atext") {
        return Some(Format::Boxnote);
    }
    None
}

/// True when the bytes carry Markdown's own punctuation: an ATX heading, a
/// fence, a front-matter block, or an inline link.
///
/// Deliberately narrow. Plain prose has none of these and should fail the
/// sniff rather than be blessed as Markdown by default; a caller who knows
/// their bytes are Markdown states the format.
#[must_use]
pub fn looks_like_markdown(text: &str) -> bool {
    if text.starts_with("---\n") || text.starts_with("---\r\n") {
        return true;
    }
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("```") || line.contains("](") || {
            let hashes = line.chars().take_while(|&c| c == '#').count();
            (1..=6).contains(&hashes) && line[hashes..].starts_with(' ')
        }
    })
}

/// True when the bytes carry AsciiDoc's own punctuation: a `= ` document
/// title on the first non-empty line, `== ` section titles, `:name: value`
/// attribute entries, or a `----` listing fence.
#[must_use]
pub fn looks_like_asciidoc(text: &str) -> bool {
    if text
        .lines()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.starts_with("= "))
    {
        return true;
    }
    text.lines().any(|line| {
        line.starts_with("== ") || line.trim_end() == "----" || is_asciidoc_attribute(line)
    })
}

/// True for an AsciiDoc attribute entry line, `:name: value`.
fn is_asciidoc_attribute(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(':') else {
        return false;
    };
    let Some((name, _)) = rest.split_once(": ") else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_short_circuits_every_signal() {
        let got = resolve(Some(Format::Markdown), "WEBVTT\n").unwrap();
        assert_eq!(got, (Format::Markdown, Evidence::Requested));
    }

    #[test]
    fn the_webvtt_header_is_a_signature() {
        for text in ["WEBVTT", "WEBVTT\n", "WEBVTT - a title\n", "WEBVTT\tx\n"] {
            assert_eq!(
                resolve(None, text).unwrap(),
                (Format::Vtt, Evidence::Signature),
                "{text:?}"
            );
        }
        assert!(!is_webvtt("WEBVTTX\n"), "the magic must end the token");
    }

    #[test]
    fn a_docling_schema_name_is_a_signature() {
        let text = r#"{"schema_name": "DoclingDocument", "version": "1.7.0"}"#;
        assert_eq!(
            resolve(None, text).unwrap(),
            (Format::DoclingJson, Evidence::Signature)
        );
    }

    #[test]
    fn a_boxnote_doc_tree_and_a_legacy_atext_are_both_signatures() {
        for text in [
            r#"{"doc": {"type": "doc", "content": []}}"#,
            r#"{"atext": {"text": "old"}, "pool": {}}"#,
        ] {
            assert_eq!(
                resolve(None, text).unwrap(),
                (Format::Boxnote, Evidence::Signature),
                "{text}"
            );
        }
    }

    #[test]
    fn html_and_latex_are_recognized_so_they_can_be_named() {
        assert_eq!(
            signature("<!DOCTYPE html><html></html>"),
            Some(Format::Html)
        );
        assert_eq!(signature("<html lang=\"en\">"), Some(Format::Html));
        assert_eq!(
            signature("\\documentclass{article}\n\\begin{document}x\\end{document}"),
            Some(Format::Latex)
        );
    }

    #[test]
    fn markdown_texture_is_a_heuristic_not_a_signature() {
        let got = resolve(None, "# Title\n\nSome prose.\n").unwrap();
        assert_eq!(got, (Format::Markdown, Evidence::Heuristic));
        let got = resolve(None, "---\ntitle: x\n---\nbody\n").unwrap();
        assert_eq!(got, (Format::Markdown, Evidence::Heuristic));
    }

    #[test]
    fn asciidoc_texture_resolves_but_only_alone() {
        let got = resolve(None, "= Title\n\nSome prose.\n").unwrap();
        assert_eq!(got, (Format::Asciidoc, Evidence::Heuristic));
        // Both textures at once: fail closed rather than pick one.
        let err = resolve(None, "= Title\n\n# Also a title\n").unwrap_err();
        assert!(err.to_string().contains("both"), "{err}");
    }

    #[test]
    fn plain_prose_and_broken_json_are_unrecognized() {
        for text in ["Just a sentence with no markup at all.\n", "{not json", ""] {
            let err = resolve(None, text).unwrap_err();
            assert!(err.to_string().contains("state `format`"), "{text:?}");
        }
    }

    #[test]
    fn a_json_object_that_names_nothing_is_unrecognized() {
        let err = resolve(None, r#"{"hello": "world"}"#).unwrap_err();
        assert!(err.to_string().contains("cannot sniff"), "{err}");
    }

    #[test]
    fn a_signature_beats_the_markdown_texture_inside_the_same_bytes() {
        // A LaTeX document full of markdown-ish lines is still LaTeX.
        let text =
            "\\documentclass{article}\n% # not a heading\n\\begin{document}\nx\n\\end{document}\n";
        assert_eq!(
            resolve(None, text).unwrap(),
            (Format::Latex, Evidence::Signature)
        );
    }

    #[test]
    fn every_wire_format_round_trips() {
        for format in [
            Format::Markdown,
            Format::Html,
            Format::Asciidoc,
            Format::Latex,
            Format::Vtt,
            Format::Boxnote,
            Format::DoclingJson,
        ] {
            assert_eq!(Format::from_proto(format.to_proto()), Some(format));
        }
        assert_eq!(Format::from_proto(pb::MarkupFormat::Unspecified), None);
    }
}
