// SPDX-License-Identifier: Apache-2.0

//! Hostile-input tests: the small documents a security review used to
//! abort, exhaust or stall the server, each run through the whole parse
//! driver on a thread with the 2 MiB stack tokio's blocking pool gives a
//! real parse.
//!
//! A stack overflow is an abort, not a panic, so a regression here kills
//! the test binary outright rather than failing one test; that is the
//! point. Every case also asserts a generous wall-clock bound, because the
//! other half of the review was inputs that are tiny and take minutes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use grpc_markup::parse::{self, Cancel, ParseConfig, ParseError};
use grpc_markup::proto::v1 as pb;
use grpc_markup::sniff::Format;

/// The blocking pool's default thread stack.
const BLOCKING_STACK: usize = 2 * 1024 * 1024;

/// What one hostile parse produced.
struct Outcome {
    result: Result<Format, ParseError>,
    events: Vec<pb::ParseMarkupResponse>,
    elapsed: Duration,
}

impl Outcome {
    /// The trailer, asserting the parse succeeded.
    fn status(&self) -> &pb::ParseStatus {
        assert!(self.result.is_ok(), "parse failed: {:?}", self.result);
        match self.events.last().and_then(|event| event.event.as_ref()) {
            Some(pb::parse_markup_response::Event::Status(status)) => status,
            other => panic!("the last event is the trailer, got {other:?}"),
        }
    }

    /// True when the trailer carries a warning of `code` whose message
    /// contains `needle`.
    fn warned(&self, code: pb::WarningCode, needle: &str) -> bool {
        self.status()
            .warnings
            .iter()
            .any(|w| w.code == code as i32 && w.message.contains(needle))
    }

    /// Every text block's text, joined.
    fn text(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            if let Some(pb::parse_markup_response::Event::TextBlock(block)) = event.event.as_ref() {
                out.push_str(&block.text);
                out.push('\n');
            }
        }
        out
    }

    /// Every table row, in order.
    fn rows(&self) -> Vec<&pb::TableRow> {
        self.events
            .iter()
            .filter_map(|event| match event.event.as_ref() {
                Some(pb::parse_markup_response::Event::TableRow(row)) => Some(row),
                _ => None,
            })
            .collect()
    }

    /// Assert the parse finished inside `limit`.
    fn within(&self, limit: Duration) -> &Self {
        assert!(
            self.elapsed < limit,
            "took {:?}, the bound is {limit:?}",
            self.elapsed
        );
        self
    }
}

/// Parse `document` as `format` on a 2 MiB thread, also folding the
/// Document so the event-driven fold sees the same input.
fn hostile(format: Format, document: String) -> Outcome {
    hostile_with(format, document, Cancel::default())
}

fn hostile_with(format: Format, document: String, cancel: Cancel) -> Outcome {
    std::thread::Builder::new()
        .name("hostile-parse".to_owned())
        .stack_size(BLOCKING_STACK)
        .spawn(move || {
            let config = ParseConfig {
                format: Some(format),
                emit_document: true,
                ..ParseConfig::default()
            };
            let mut events = Vec::new();
            let mut emit = |event: pb::ParseMarkupResponse| {
                events.push(event);
                true
            };
            let started = Instant::now();
            let result = parse::run_with(document.as_bytes(), &config, &cancel, started, &mut emit);
            let elapsed = started.elapsed();
            // The document and every event are dropped on this thread too:
            // dropping a deep tree recurses just as building one does.
            drop(document);
            Outcome {
                result,
                events,
                elapsed,
            }
        })
        .expect("spawn")
        .join()
        .expect("the parse thread did not panic")
}

const BOUND: Duration = Duration::from_secs(20);

// ------------------------------------------------------------ cancellation

#[test]
fn a_passed_deadline_stops_the_parse_with_deadline_exceeded() {
    let cancel = Cancel::new(Some(Instant::now()), None);
    let outcome = hostile_with(Format::Markdown, "# T\n\nbody\n".to_owned(), cancel);
    assert!(
        matches!(outcome.result, Err(ParseError::DeadlineExceeded(_))),
        "{:?}",
        outcome.result
    );
    assert!(outcome.events.is_empty());
}

