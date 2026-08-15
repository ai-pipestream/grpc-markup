// SPDX-License-Identifier: Apache-2.0

//! The WebVTT fold: a hand-written parser for the subtitle and caption
//! format of <https://www.w3.org/TR/webvtt1/>, mapped to `Cue` events.
//!
//! What it does, cue by cue:
//!
//! - The `WEBVTT` signature line is mandatory; a file without it is
//!   `INVALID_ARGUMENT`. Text after the signature becomes `MarkupInfo.title`,
//!   and `Key: value` lines in the rest of the header block land in
//!   `MarkupInfo.metadata`.
//! - `NOTE`, `STYLE` and `REGION` blocks are skipped: comments and rendering
//!   rules, not content.
//! - A cue block is an optional identifier line, a timings line
//!   (`mm:ss.ttt --> mm:ss.ttt`, hours optional), and payload lines. Timings
//!   become milliseconds on the `Cue`.
//! - Payload markup is flattened the way a renderer's text track reads:
//!   voice (`<v Name>`), styling (`<b>`, `<i>`, `<u>`, `<c>`, `<ruby>`,
//!   `<lang>`) and karaoke timestamp tags are all stripped, the first voice
//!   annotation becomes `Cue.speaker`, and lines join with `\n`.
//! - A malformed cue follows WebVTT's own error recovery: the cue is
//!   skipped, the parse continues, and the trailer aggregates one
//!   `WARNING_CODE_MALFORMED_STRUCTURE` per distinct reason.
//!
//! This mirrors docling's `webvtt_backend.py` in effect — each cue one text
//! unit, its timings carried as track provenance — without the inline
//! formatting model this wire deliberately flattens away.

use std::collections::HashMap;

use crate::parse::{Emitter, ParseError};
use crate::proto::v1 as pb;
use crate::sniff;

/// Map one WebVTT file.
///
/// # Errors
///
/// [`ParseError::Invalid`] when the mandatory `WEBVTT` header is missing —
/// which includes empty input — and [`ParseError::ConsumerGone`] when the
/// client stopped reading. Malformed cues are warnings, not errors.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    if !sniff::is_webvtt(text) {
        return Err(ParseError::Invalid(
            "not a WebVTT file: the mandatory WEBVTT header line is missing".to_owned(),
        ));
    }
    // One line-terminator vocabulary: the spec admits CRLF, CR and LF.
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    // The signature line, and the header lines under it up to the first
    // blank: a title after the magic, `Key: value` pairs below it.
    let title = lines[0]
        .strip_prefix("WEBVTT")
        .map(str::trim)
        .filter(|rest| !rest.is_empty())
        .map(|rest| rest.strip_prefix("- ").unwrap_or(rest).to_owned());
    let mut metadata = HashMap::new();
    let mut at = 1;
    while at < lines.len() && !lines[at].trim().is_empty() {
        if let Some((key, value)) = lines[at].split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() && !value.is_empty() && !key.contains(' ') {
                metadata.insert(key.to_ascii_lowercase(), value.to_owned());
            }
        }
        at += 1;
    }
    emitter.info(title, metadata)?;

    while at < lines.len() {
        // Skip the blank separators between blocks.
        while at < lines.len() && lines[at].trim().is_empty() {
            at += 1;
        }
        if at >= lines.len() {
            break;
        }
        let block_start = at;
        while at < lines.len() && !lines[at].trim().is_empty() {
            at += 1;
        }
        cue_block(&lines[block_start..at], emitter)?;
    }
    Ok(())
}

/// Map one non-blank block: a cue, or a skipped NOTE / STYLE / REGION.
fn cue_block(block: &[&str], emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let first = block[0];
    if is_block_keyword(first, "NOTE")
        || is_block_keyword(first, "STYLE")
        || is_block_keyword(first, "REGION")
    {
        return Ok(());
    }

    // An identifier line is any line before the timings that does not
    // contain the arrow; the arrow line itself carries the timings.
    let (cue_id, timings_line, payload) = if first.contains("-->") {
        (None, first, &block[1..])
    } else if block.len() >= 2 && block[1].contains("-->") {
        (Some(first.trim().to_owned()), block[1], &block[2..])
    } else {
        emitter.warn(
            pb::WarningCode::MalformedStructure,
            "cue block without a timings line was skipped",
        );
        return Ok(());
    };

    let Some((start_ms, end_ms)) = timings(timings_line) else {
        emitter.warn(
            pb::WarningCode::MalformedStructure,
            "cue with unparseable timings was skipped",
        );
        return Ok(());
    };
    if end_ms < start_ms {
        emitter.warn(
            pb::WarningCode::MalformedStructure,
            "cue whose end precedes its start was skipped",
        );
        return Ok(());
    }

    let mut speaker = None;
    let text = payload
        .iter()
        .map(|line| flatten_cue_text(line, &mut speaker))
        .collect::<Vec<_>>()
        .join("\n");
    emitter.cue(cue_id, start_ms, end_ms, speaker.unwrap_or_default(), text)
}

/// True when a line opens a keyword block: the keyword alone, or the
/// keyword followed by whitespace and its own content.
fn is_block_keyword(line: &str, keyword: &str) -> bool {
    line.strip_prefix(keyword)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// Parse a `start --> end` line into milliseconds, ignoring cue settings
/// after the end timestamp.
fn timings(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    Some((timestamp_ms(start.trim())?, timestamp_ms(end)?))
}

/// One WebVTT timestamp as milliseconds: `mm:ss.ttt`, with optional hours.
///
/// The grammar is the spec's: minutes and seconds are exactly two digits,
/// seconds below 60, milliseconds exactly three digits, hours two or more
/// digits when present.
fn timestamp_ms(text: &str) -> Option<u64> {
    let (rest, millis_text) = text.split_once('.')?;
    if millis_text.len() != 3 || !millis_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: u64 = millis_text.parse().ok()?;
    let parts: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => ("0", *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => return None,
    };
    if minutes.len() != 2 || seconds.len() != 2 || hours.is_empty() {
        return None;
    }
    let hours: u64 = hours.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    let seconds: u64 = seconds.parse().ok()?;
    if minutes > 59 || seconds > 59 {
        return None;
    }
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + millis)
}

/// One payload line with cue markup flattened away.
///
/// Tags — voice, styling, classes, ruby, language, karaoke timestamps — are
/// spans between `<` and `>` and are dropped; the first voice annotation is
/// captured as the speaker; the basic character entities decode.
fn flatten_cue_text(line: &str, speaker: &mut Option<String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            // An unterminated tag runs to the end of the line; the spec's
            // tokenizer consumes it without emitting text.
            rest = "";
            break;
        };
        let tag = &after[..close];
        if speaker.is_none()
            && let Some(voice) = tag.strip_prefix('v')
        {
            // Between the `v` and its annotation sit optional `.class`
            // qualifiers; the annotation is everything after the first
            // whitespace.
            let annotation = voice
                .trim_start_matches(|c: char| c == '.' || !c.is_whitespace())
                .trim();
            if !annotation.is_empty() {
                *speaker = Some(annotation.to_owned());
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

/// The character entities the WebVTT cue-text grammar admits.
fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", "\u{a0}")
        .replace("&lrm;", "\u{200e}")
        .replace("&rlm;", "\u{200f}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_with_and_without_hours() {
        assert_eq!(timestamp_ms("00:01.000"), Some(1_000));
        assert_eq!(timestamp_ms("01:02:03.456"), Some(3_723_456));
        assert_eq!(timestamp_ms("123:00:00.000"), Some(123 * 3_600_000));
    }

    #[test]
    fn malformed_timestamps_are_refused() {
        for bad in [
            "1:00.000",     // minutes must be two digits
            "00:60.000",    // seconds out of range
            "00:99:00.000", // minutes out of range
            "00:00.00",     // milliseconds must be three digits
            "00:00",        // no milliseconds at all
            "xx:00.000",
        ] {
            assert_eq!(timestamp_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn voice_and_styling_tags_flatten_and_the_first_speaker_wins() {
        let mut speaker = None;
        let text = flatten_cue_text(
            "<v Ada Lovelace><b>Hello</b> <i>there</i></v>",
            &mut speaker,
        );
        assert_eq!(text, "Hello there");
        assert_eq!(speaker.as_deref(), Some("Ada Lovelace"));

        let mut second = Some("Kept".to_owned());
        flatten_cue_text("<v Other>hi", &mut second);
        assert_eq!(second.as_deref(), Some("Kept"));
    }

    #[test]
    fn a_voice_span_with_classes_still_names_the_speaker() {
        let mut speaker = None;
        let text = flatten_cue_text("<v.loud.fast Grace Hopper>Mark!", &mut speaker);
        assert_eq!(text, "Mark!");
        assert_eq!(speaker.as_deref(), Some("Grace Hopper"));
    }

    #[test]
    fn karaoke_timestamps_are_removed_from_the_text() {
        let mut speaker = None;
        let text = flatten_cue_text("Never<00:00:01.000> gonna<00:00:02.000> give", &mut speaker);
        assert_eq!(text, "Never gonna give");
        assert!(speaker.is_none(), "a timestamp tag is not a voice");
    }

    #[test]
    fn entities_decode_in_cue_text() {
        let mut speaker = None;
        assert_eq!(
            flatten_cue_text("a &amp; b &lt;c&gt;", &mut speaker),
            "a & b <c>"
        );
    }

    #[test]
    fn keyword_blocks_are_recognized_exactly() {
        assert!(is_block_keyword("NOTE", "NOTE"));
        assert!(is_block_keyword("NOTE a comment", "NOTE"));
        assert!(!is_block_keyword("NOTEBOOK", "NOTE"));
        assert!(is_block_keyword("STYLE", "STYLE"));
        assert!(is_block_keyword("REGION", "REGION"));
    }
}
