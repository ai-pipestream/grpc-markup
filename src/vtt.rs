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
//! - Payload markup is flattened out of the text and described beside it:
//!   `<b>`, `<i>` and `<u>` leave a style run, the class list of any cue tag
//!   leaves a named character style, `<lang xx>` leaves the tag on its run,
//!   a `<ruby>`'s `<rt>` reading leaves the base text alone and rides as
//!   the run's annotation,
//!   the first `<v Name>` annotation becomes `Cue.speaker`, and a karaoke
//!   `<00:00:01.000>` timestamp splits the text into `CueTiming` segments.
//!   Lines join with `\n` and the spacing the author timed against is kept
//!   verbatim, so a range indexes the string the consumer was handed.
//! - A malformed cue follows WebVTT's own error recovery: the cue is
//!   skipped, the parse continues, and the trailer aggregates one
//!   `WARNING_CODE_MALFORMED_STRUCTURE` per distinct reason.
//!
//! Each cue is one text unit whose own timings ride as track provenance and
//! whose karaoke segments ride as their own timed ranges: a cue that says
//! when each of its words is sung has stated real coordinates, and the
//! Document plane has a slot for them.

use std::collections::HashMap;

use crate::parse::{Cue, Emitter, ParseError, scalar};
use crate::proto::v1 as pb;
use crate::sniff;
use crate::spans::{Inline, Mark};

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
                metadata.insert(key.to_ascii_lowercase(), scalar(value));
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

    emitter.cue(flatten_payload(cue_id, payload, start_ms, end_ms))
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
    // Hours have no upper bound in the grammar, so a hostile one would
    // wrap; a time past u64 milliseconds is not a time, and the cue goes.
    hours
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(60)?
        .checked_add(seconds)?
        .checked_mul(1000)?
        .checked_add(millis)
}

/// Flatten a cue's payload lines into the cue the emitter sends, keeping
/// what the tags said.
///
/// The tokenizer is the spec's shape: text runs, cue span start tags with
/// optional classes and an optional annotation, end tags, and timestamp
/// tags. A start tag opens a run, its end tag closes it, an end tag with no
/// open partner is ignored, and a tag still open at the end of the payload
/// is closed there. An unterminated tag runs to the end of its line and
/// emits no text, which is what the spec's tokenizer does with it.
fn flatten_payload(cue_id: Option<String>, lines: &[&str], start_ms: u64, end_ms: u64) -> Cue {
    let mut inline = Inline::default();
    let mut speaker = None;
    // Open cue spans by tag name, innermost last, so an end tag can find
    // its partner rather than closing whatever happens to be on top.
    let mut open: Vec<String> = Vec::new();
    // Where each karaoke segment starts: (its time, its offset in `text`).
    let mut karaoke: Vec<(u64, u32)> = Vec::new();
    // The reading being collected inside an `<rt>`, which goes on the
    // enclosing ruby run rather than into the text.
    let mut reading: Option<String> = None;

    for (position, line) in lines.iter().enumerate() {
        if position > 0 {
            inline.push_verbatim("\n");
        }
        let mut rest = *line;
        while let Some(at) = rest.find('<') {
            push_text(&mut inline, &mut reading, &rest[..at]);
            let after = &rest[at + 1..];
            let Some(close) = after.find('>') else {
                rest = "";
                break;
            };
            let tag = &after[..close];
            rest = &after[close + 1..];
            if let Some(ms) = timestamp_ms(tag.trim()) {
                karaoke.push((ms, inline.len()));
            } else if let Some(name) = tag.strip_prefix('/') {
                let name = name.trim();
                if name == "rt" {
                    // The reading is finished; it belongs to the ruby run
                    // that is still open around the base text.
                    let collected = reading.take().unwrap_or_default();
                    inline.annotate(collected.trim());
                    open.pop();
                    continue;
                }
                close_span(&mut inline, &mut open, name);
            } else if tag_name(tag) == "rt" {
                reading = Some(String::new());
                open.push("rt".to_owned());
            } else {
                open_span(&mut inline, &mut open, tag, &mut speaker);
            }
        }
        push_text(&mut inline, &mut reading, rest);
    }
    // An `<rt>` the payload never closed still stated a reading.
    if let Some(collected) = reading.take() {
        inline.annotate(collected.trim());
        open.pop();
    }
    while open.pop().is_some() {
        inline.close();
    }

    let (text, spans) = inline.take();
    let length = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    Cue {
        cue_id,
        start_ms,
        end_ms,
        speaker,
        timings: segments(&karaoke, length, start_ms, end_ms),
        text,
        spans,
    }
}

/// Route one text run: into the reading being collected when an `<rt>` is
/// open, into the cue text otherwise.
///
/// A ruby reading is printed over the base text, not inside it, so letting
/// it into the string would interleave two readings of the same words.
fn push_text(inline: &mut Inline, reading: &mut Option<String>, text: &str) {
    match reading.as_mut() {
        Some(collected) => collected.push_str(&decoded(text)),
        None => push_decoded(inline, text),
    }
}

/// The tag name of one start tag: everything before its classes or its
/// annotation.
fn tag_name(tag: &str) -> String {
    tag.chars()
        .take_while(|c| *c != '.' && !c.is_whitespace())
        .collect()
}

/// Open the run one cue span start tag describes, and name the speaker when
/// it is the first voice tag of the cue.
///
/// Classes ride on every tag the grammar admits them on, not only on `<c>`,
/// so `<b.loud>` keeps its class the same way `<c.loud>` does.
fn open_span(inline: &mut Inline, open: &mut Vec<String>, tag: &str, speaker: &mut Option<String>) {
    let name: String = tag
        .chars()
        .take_while(|c| *c != '.' && !c.is_whitespace())
        .collect();
    let qualifiers = &tag[name.len()..];
    let (classes, annotation) = match qualifiers.find(char::is_whitespace) {
        Some(at) => (&qualifiers[..at], qualifiers[at..].trim()),
        None => (qualifiers, ""),
    };
    let classes = classes.trim_start_matches('.');
    let mut mark = match name.as_str() {
        "b" => Mark::style(pb::SpanStyle::Bold),
        "i" => Mark::style(pb::SpanStyle::Italic),
        "u" => Mark::style(pb::SpanStyle::Underline),
        // A `ruby` opens a run whose reading arrives later, from the `<rt>`
        // inside it; `v` is already answered by `Cue.speaker`.
        _ => Mark::default(),
    };
    if !classes.is_empty() {
        mark.style_name = Some(classes.to_owned());
    }
    if name == "lang" && !annotation.is_empty() {
        mark.language = Some(annotation.to_owned());
    }
    if name == "v" && speaker.is_none() && !annotation.is_empty() {
        *speaker = Some(annotation.to_owned());
    }
    open.push(name);
    inline.open(mark);
}

/// Close the innermost run whose tag name matches, and every run inside it.
/// An end tag naming nothing open is ignored, which is how a renderer's
/// tokenizer treats it.
fn close_span(inline: &mut Inline, open: &mut Vec<String>, name: &str) {
    if !open.iter().any(|tag| tag == name) {
        return;
    }
    while let Some(tag) = open.pop() {
        inline.close();
        if tag == name {
            return;
        }
    }
}

/// The karaoke segments a cue's timestamp tags cut its text into.
///
/// A cue with no timestamp tag gets none: its timing is the cue's own, and
/// repeating that as a segment would add no fact. A cue with one gets a
/// segment for every run, the text before the first tag included, so the
/// segments tile the whole string. A tag with nothing after it before the
/// next tag leaves no segment.
fn segments(karaoke: &[(u64, u32)], length: u32, start_ms: u64, end_ms: u64) -> Vec<pb::CueTiming> {
    if karaoke.is_empty() {
        return Vec::new();
    }
    let mut cuts: Vec<(u64, u32)> = Vec::with_capacity(karaoke.len() + 1);
    if karaoke[0].1 > 0 {
        cuts.push((start_ms, 0));
    }
    cuts.extend_from_slice(karaoke);
    (0..cuts.len())
        .filter_map(|at| {
            let (from, start) = cuts[at];
            let (to, end) = cuts
                .get(at + 1)
                .map_or((end_ms, length), |(next, offset)| (*next, *offset));
            (start < end).then_some(pb::CueTiming {
                range: Some(pb::IntRange { start, end }),
                start_ms: from,
                end_ms: to.max(from),
            })
        })
        .collect()
}

/// One text run with its entities decoded, for the reading inside an `<rt>`,
/// which is collected as a string rather than appended to the cue text.
fn decoded(text: &str) -> String {
    let mut inline = Inline::default();
    push_decoded(&mut inline, text);
    let (decoded, _) = inline.take();
    decoded
}

/// Append one text run, decoding the character entities the cue-text grammar
/// admits as it goes.
///
/// Decoding during the walk rather than over the finished string is what
/// keeps the offsets honest: `&amp;` is five code points in the source and
/// one in the text a consumer indexes.
fn push_decoded(inline: &mut Inline, text: &str) {
    /// The entities the WebVTT cue-text grammar admits, longest match first
    /// where one name prefixes another.
    const ENTITIES: &[(&str, &str)] = &[
        ("&nbsp;", "\u{a0}"),
        ("&lrm;", "\u{200e}"),
        ("&rlm;", "\u{200f}"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&amp;", "&"),
    ];
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        inline.push_verbatim(&rest[..at]);
        let tail = &rest[at..];
        match ENTITIES
            .iter()
            .find(|(name, _)| tail.starts_with(name))
            .copied()
        {
            Some((name, value)) => {
                inline.push_verbatim(value);
                rest = &tail[name.len()..];
            }
            // Not an entity this grammar knows: the ampersand is text.
            None => {
                inline.push_verbatim("&");
                rest = &tail[1..];
            }
        }
    }
    inline.push_verbatim(rest);
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
    fn a_timestamp_past_u64_milliseconds_is_refused_not_wrapped() {
        assert_eq!(timestamp_ms("18446744073709551:00:00.000"), None);
        assert_eq!(timestamp_ms("5124095576031:00:00.000"), None);
        assert_eq!(
            timestamp_ms("5124095576:00:00.000"),
            Some(5_124_095_576 * 3_600_000)
        );
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

    /// One payload line, timed over a whole second.
    fn one(line: &str) -> Cue {
        flatten_payload(None, &[line], 0, 1_000)
    }

    /// The code points a run covers, sliced out of the text it indexes.
    fn run(payload: &Cue, at: usize) -> String {
        let range = payload.spans[at].range.expect("every run has a range");
        payload
            .text
            .chars()
            .skip(range.start as usize)
            .take((range.end - range.start) as usize)
            .collect()
    }

    #[test]
    fn voice_and_styling_tags_flatten_and_the_first_speaker_wins() {
        let payload = one("<v Ada Lovelace><b>Hello</b> <i>there</i></v>");
        assert_eq!(payload.text, "Hello there");
        assert_eq!(payload.speaker.as_deref(), Some("Ada Lovelace"));

        let second = flatten_payload(None, &["<v First>a", "<v Other>b"], 0, 1_000);
        assert_eq!(
            second.speaker.as_deref(),
            Some("First"),
            "the first annotation is the cue's voice"
        );
    }

    #[test]
    fn styling_tags_leave_the_run_they_cover() {
        let payload = one("plain <b>bold</b> and <i>italic</i> and <u>under</u>");
        assert_eq!(payload.text, "plain bold and italic and under");
        assert_eq!(payload.spans.len(), 3);
        assert_eq!(run(&payload, 0), "bold");
        assert_eq!(payload.spans[0].styles, vec![pb::SpanStyle::Bold as i32]);
        assert_eq!(run(&payload, 1), "italic");
        assert_eq!(payload.spans[1].styles, vec![pb::SpanStyle::Italic as i32]);
        assert_eq!(run(&payload, 2), "under");
        assert_eq!(
            payload.spans[2].styles,
            vec![pb::SpanStyle::Underline as i32]
        );
    }

    #[test]
    fn a_class_becomes_a_named_style_and_a_lang_tag_becomes_a_language() {
        let payload = one("<c.loud.narrow>SHOUT</c> then <lang fr>bonjour</lang>");
        assert_eq!(payload.text, "SHOUT then bonjour");
        assert_eq!(run(&payload, 0), "SHOUT");
        assert_eq!(
            payload.spans[0].style_name.as_deref(),
            Some("loud.narrow"),
            "the class list rides verbatim, dots and all"
        );
        assert!(payload.spans[0].styles.is_empty(), "a class is not a style");
        assert_eq!(run(&payload, 1), "bonjour");
        assert_eq!(payload.spans[1].language.as_deref(), Some("fr"));
    }

    #[test]
    fn a_ruby_reading_rides_over_the_base_text_instead_of_inside_it() {
        let payload = one("<ruby>東京<rt>とうきょう</rt></ruby> です");
        assert_eq!(
            payload.text, "東京 です",
            "the reading is printed over the base text, not interleaved into it"
        );
        assert_eq!(payload.spans.len(), 1);
        assert_eq!(run(&payload, 0), "東京");
        assert_eq!(payload.spans[0].annotation.as_deref(), Some("とうきょう"));
        let range = payload.spans[0].range.expect("a range");
        assert_eq!(
            (range.start, range.end),
            (0, 2),
            "the run covers the base text, counted in code points"
        );
    }

    #[test]
    fn a_ruby_with_no_reading_leaves_no_run_at_all() {
        let payload = one("<ruby>東京</ruby> です");
        assert_eq!(payload.text, "東京 です");
        assert!(
            payload.spans.is_empty(),
            "a run that says nothing is still not a span"
        );
    }

    #[test]
    fn an_unclosed_rt_still_states_its_reading() {
        let payload = one("<ruby>東京<rt>とうきょう");
        assert_eq!(payload.text, "東京");
        assert_eq!(payload.spans[0].annotation.as_deref(), Some("とうきょう"));
    }

    #[test]
    fn a_class_on_a_styling_tag_rides_beside_its_style() {
        let payload = one("<b.loud>hey</b>");
        assert_eq!(payload.spans[0].styles, vec![pb::SpanStyle::Bold as i32]);
        assert_eq!(payload.spans[0].style_name.as_deref(), Some("loud"));
    }

    #[test]
    fn a_voice_span_with_classes_still_names_the_speaker() {
        let payload = one("<v.loud.fast Grace Hopper>Mark!");
        assert_eq!(payload.text, "Mark!");
        assert_eq!(payload.speaker.as_deref(), Some("Grace Hopper"));
        assert_eq!(
            payload.spans[0].style_name.as_deref(),
            Some("loud.fast"),
            "a voice tag's classes are still a named style"
        );
    }

    #[test]
    fn karaoke_timestamps_cut_the_text_into_timed_segments() {
        let payload = flatten_payload(
            None,
            &["Never<00:00:01.000> gonna<00:00:02.000> give"],
            500,
            3_000,
        );
        assert_eq!(payload.text, "Never gonna give");
        assert!(payload.speaker.is_none(), "a timestamp tag is not a voice");
        let ranges: Vec<(u32, u32, u64, u64)> = payload
            .timings
            .iter()
            .map(|timing| {
                let range = timing.range.expect("every segment has a range");
                (range.start, range.end, timing.start_ms, timing.end_ms)
            })
            .collect();
        assert_eq!(
            ranges,
            [
                (0, 5, 500, 1_000),
                (5, 11, 1_000, 2_000),
                (11, 16, 2_000, 3_000),
            ],
            "the segments tile the text, the leading run from the cue's own start"
        );
    }

    #[test]
    fn a_cue_without_timestamp_tags_states_no_segments() {
        assert!(
            one("plain text").timings.is_empty(),
            "the cue's own timing is not repeated as a segment"
        );
    }

    #[test]
    fn entities_decode_in_cue_text_and_the_offsets_count_the_decoded_text() {
        let payload = one("a &amp; b <b>&lt;c&gt;</b>");
        assert_eq!(payload.text, "a & b <c>");
        assert_eq!(
            run(&payload, 0),
            "<c>",
            "five entity code points became one"
        );
    }

    #[test]
    fn offsets_over_a_multi_byte_cue_count_code_points() {
        let payload = one("héllo wörld <b>λόγος</b>");
        assert_eq!(payload.text, "héllo wörld λόγος");
        let range = payload.spans[0].range.expect("a range");
        assert_eq!((range.start, range.end), (12, 17));
        assert!(
            payload.text.len() > 17,
            "the byte length is not the code point one"
        );
        assert_eq!(run(&payload, 0), "λόγος");
    }

    #[test]
    fn spacing_and_line_breaks_survive_and_the_ranges_count_them() {
        let payload = flatten_payload(None, &["one  two", "<b>three</b>"], 0, 1_000);
        assert_eq!(
            payload.text, "one  two\nthree",
            "cue text is not collapsed prose"
        );
        assert_eq!(run(&payload, 0), "three");
    }

    #[test]
    fn an_end_tag_with_no_partner_is_ignored_and_an_open_tag_closes_at_the_end() {
        let payload = one("</i>a <b>b");
        assert_eq!(payload.text, "a b");
        assert_eq!(payload.spans.len(), 1);
        assert_eq!(run(&payload, 0), "b");
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
