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

use grpc_markup::document_fold::integrity_errors;
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
            // Every fold a hostile input completes is still a merge-safe
            // fragment, however much of it was flattened or clamped.
            if result.is_ok() {
                for event in &events {
                    if let Some(pb::parse_markup_response::Event::Document(fold)) =
                        event.event.as_ref()
                    {
                        assert_eq!(integrity_errors(fold), Vec::<String>::new());
                    }
                }
            }
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

// -------------------------------------------------------------------- HTML

#[test]
fn ten_thousand_nested_divs_flatten_instead_of_overflowing() {
    let outcome = hostile(Format::Html, format!("{}deep text", "<div>".repeat(10_000)));
    outcome.within(BOUND);
    assert!(outcome.text().contains("deep text"), "{}", outcome.text());
    assert!(outcome.warned(pb::WarningCode::UnmappedConstruct, "nested deeper"));
}

#[test]
fn deep_inline_list_and_table_nesting_all_survive_a_small_stack() {
    for document in [
        // html5ever's own scope checks are quadratic in nesting depth, so
        // the list and quote cases stay at a depth (10k elements) that
        // still overflowed before the cap without making the test slow.
        format!("{}x", "<span>".repeat(50_000)),
        format!("{}x", "<ul><li>".repeat(5_000)),
        format!("{}x", "<table><tr><td>".repeat(2_000)),
        format!("{}x", "<blockquote>".repeat(10_000)),
    ] {
        let outcome = hostile(Format::Html, document);
        outcome.within(BOUND);
        assert!(outcome.warned(pb::WarningCode::UnmappedConstruct, "nested deeper"));
    }
}

#[test]
fn a_parse_past_its_deadline_stops_inside_the_html_tree_builder() {
    // Quadratic inside html5ever: the slice-by-slice feed is what lets the
    // deadline interrupt it rather than waiting for the whole tree.
    let cancel = Cancel::new(Some(Instant::now() + Duration::from_millis(200)), None);
    let outcome = hostile_with(Format::Html, "<ul><li>".repeat(200_000), cancel);
    assert!(
        matches!(outcome.result, Err(ParseError::DeadlineExceeded(_))),
        "{:?}",
        outcome.result
    );
    outcome.within(Duration::from_secs(10));
}

#[test]
fn a_huge_colspan_and_rowspan_are_clamped_to_the_table_model() {
    let outcome = hostile(
        Format::Html,
        "<table><tr><td colspan=65534 rowspan=65534>x</td></tr></table>".to_owned(),
    );
    outcome.within(BOUND);
    let rows = outcome.rows();
    assert_eq!(rows.len(), 1);
    let cell = &rows[0].cells[0];
    assert_eq!(cell.column_span, 1000, "colspan clamps to 1000");
    assert_eq!(cell.row_span, 1, "rowspan clamps to the rows that exist");
}

#[test]
fn spans_still_place_later_cells_after_clamping() {
    let outcome = hostile(
        Format::Html,
        "<table><tr><td colspan=4000 rowspan=4000>a</td><td>b</td></tr>\
         <tr><td>c</td></tr></table>"
            .to_owned(),
    );
    outcome.within(BOUND);
    let rows = outcome.rows();
    assert_eq!(rows[0].cells[0].row_span, 2);
    assert_eq!(rows[0].cells[1].column_index, 1000);
    assert_eq!(
        rows[1].cells[0].column_index, 1000,
        "the spanning cell still covers the next row"
    );
}

#[test]
fn a_big_cell_spanning_many_positions_is_not_copied_into_the_grid() {
    // 65 x 1000 positions, under the grid's position floor, and a cell of
    // a few megabytes at the top left: the dense grid would be a copy of
    // that cell per position. The flat cells are complete and the grid is
    // left out.
    let mut document = format!(
        "<table><tr><td colspan=1000 rowspan=65>{}</td><td>y</td></tr>",
        "x".repeat(3 << 20)
    );
    for _ in 0..64 {
        document.push_str("<tr><td>y</td></tr>");
    }
    document.push_str("</table>");
    let outcome = hostile(Format::Html, document);
    outcome.within(BOUND);
    let fold = outcome
        .events
        .iter()
        .find_map(|event| match event.event.as_ref() {
            Some(pb::parse_markup_response::Event::Document(fold)) => Some(fold),
            _ => None,
        })
        .expect("the fold was requested");
    let data = fold.tables[0].data.as_ref().expect("the table has data");
    assert_eq!((data.num_rows, data.num_cols), (65, 1001));
    assert_eq!(data.table_cells.len(), 66);
    assert!(
        data.grid.is_empty(),
        "no grid of sixty-five thousand copies"
    );
    let bytes: usize = data.table_cells.iter().map(|cell| cell.text.len()).sum();
    assert!(
        bytes < (3 << 20) + 1_000,
        "the table is one copy of its cells"
    );
}

#[test]
fn many_spanning_cells_stay_linear() {
    // A few thousand cells each claiming the whole table below them used
    // to be area work; now each later row sweeps a bounded span list.
    let mut document = String::from("<table><tr>");
    for _ in 0..5_000 {
        document.push_str("<td colspan=1000 rowspan=65534>x</td>");
    }
    document.push_str("</tr>");
    for _ in 0..5_000 {
        document.push_str("<tr><td>y</td></tr>");
    }
    document.push_str("</table>");
    let outcome = hostile(Format::Html, document);
    outcome.within(BOUND);
    assert_eq!(outcome.rows().len(), 5_001);
}

// ------------------------------------------------------------------- LaTeX

#[test]
fn five_thousand_open_braces_read_as_text_instead_of_overflowing() {
    let outcome = hostile(Format::Latex, format!("{}deep text", "{".repeat(5_000)));
    outcome.within(BOUND);
    assert!(outcome.text().contains("deep text"), "{}", outcome.text());
    assert!(outcome.warned(pb::WarningCode::UnmappedConstruct, "nested deeper"));
}

#[test]
fn fifty_thousand_nested_environments_survive_a_small_stack() {
    let outcome = hostile(
        Format::Latex,
        format!("{}x", "\\begin{itemize}".repeat(50_000)),
    );
    outcome.within(BOUND);
    assert!(outcome.warned(pb::WarningCode::UnmappedConstruct, "nested deeper"));
}

#[test]
fn a_self_referencing_newcommand_expands_once_not_exponentially() {
    // 137 bytes that took ten seconds of CPU at five levels of expansion.
    let document = format!(
        "\\newcommand{{\\a}}{{xyzw{}}}\n\\begin{{document}}\n\\a\n\\end{{document}}\n",
        "\\a".repeat(40)
    );
    let outcome = hostile(Format::Latex, document);
    outcome.within(Duration::from_secs(5));
    assert!(outcome.text().contains("xyzw"), "{}", outcome.text());
}

#[test]
fn newcommand_expansion_stops_at_its_budget() {
    // A 1 KiB definition used a hundred thousand times is 100 MiB of text
    // from 300 KiB of source; the budget stops it at a multiple of the
    // input and says so.
    let document = format!(
        "\\newcommand{{\\big}}{{{}}}\n\\begin{{document}}\n{}\n\\end{{document}}\n",
        "w ".repeat(512),
        "\\big ".repeat(100_000)
    );
    let outcome = hostile(Format::Latex, document);
    outcome.within(BOUND);
    assert!(outcome.warned(pb::WarningCode::UnmappedConstruct, "expansion budget"));
}

#[test]
fn unclosed_end_and_begin_runs_are_linear() {
    // 1 MB of `\end{` took 7.6 s and 1.4 MB of `\begin{` 4.9 s in release.
    for opener in [
        "\\end{",
        "\\begin{",
        "\\item[",
        "\\newcommand{",
        "\\newcommand\\a{",
    ] {
        let outcome = hostile(Format::Latex, opener.repeat(1_000_000 / opener.len()));
        outcome.within(Duration::from_secs(10));
        assert!(outcome.result.is_ok(), "{opener}: {:?}", outcome.result);
    }
}

// ------------------------------------------------------------ Docling JSON

#[test]
fn a_hundred_thousand_chained_groups_project_without_recursing() {
    const DEPTH: usize = 100_000;
    let mut groups = String::new();
    for index in 0..DEPTH {
        if index > 0 {
            groups.push(',');
        }
        let child = if index + 1 < DEPTH {
            format!("#/groups/{}", index + 1)
        } else {
            "#/texts/0".to_owned()
        };
        let parent = if index == 0 {
            "#/body".to_owned()
        } else {
            format!("#/groups/{}", index - 1)
        };
        groups.push_str(&format!(
            r##"{{"self_ref":"#/groups/{index}","parent":{{"$ref":"{parent}"}},"children":[{{"$ref":"{child}"}}],"name":"g","label":"unspecified"}}"##
        ));
    }
    let document = format!(
        r##"{{"schema_name":"DoclingDocument","version":"1.3.0","name":"chain",
"furniture":{{"self_ref":"#/furniture","children":[],"name":"_root_","label":"unspecified"}},
"body":{{"self_ref":"#/body","children":[{{"$ref":"#/groups/0"}}],"name":"_root_","label":"unspecified"}},
"groups":[{groups}],
"texts":[{{"self_ref":"#/texts/0","parent":{{"$ref":"#/groups/{last}"}},"children":[],"label":"text","prov":[],"orig":"bottom","text":"bottom"}}]}}"##,
        last = DEPTH - 1
    );
    let outcome = hostile(Format::DoclingJson, document);
    outcome.within(BOUND);
    assert!(outcome.text().contains("bottom"), "{}", outcome.text());
}

// ----------------------------------------------------------------- BoxNote

/// A one-row BoxNote table whose only cell claims `colspan` and `rowspan`.
fn boxnote_table(colspan: u64, rowspan: u64) -> String {
    format!(
        r#"{{"doc":{{"type":"doc","content":[{{"type":"table","content":[{{"type":"table_row","content":[{{"type":"table_cell","attrs":{{"colspan":{colspan},"rowspan":{rowspan}}},"content":[{{"type":"paragraph","content":[{{"type":"text","text":"x"}}]}}]}}]}}]}}]}}}}"#
    )
}

#[test]
fn boxnote_cell_spans_are_clamped_to_the_table_model() {
    // The fold swept every (row, column) pair a cell's spans claimed into
    // an occupancy set, so a cell claiming the whole plane was area work
    // and area memory from a few hundred bytes. The spans are clamped the
    // way the HTML fold clamps them: `colspan` to the table model's 1000,
    // `rowspan` to the rows the table has.
    let outcome = hostile(Format::Boxnote, boxnote_table(65_534, 1));
    outcome.within(BOUND);
    let rows = outcome.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cells[0].column_span, 1000, "colspan clamps to 1000");

    let outcome = hostile(Format::Boxnote, boxnote_table(1, 65_534));
    outcome.within(BOUND);
    let rows = outcome.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].cells[0].row_span, 1,
        "rowspan clamps to the rows that exist"
    );

    // The area cases: the whole plane, and spans past u32 (the column
    // arithmetic used to overflow on these).
    for (colspan, rowspan) in [(65_534, 65_534), (u64::from(u32::MAX), u64::from(u32::MAX))] {
        let outcome = hostile(Format::Boxnote, boxnote_table(colspan, rowspan));
        outcome.within(Duration::from_secs(2));
        let rows = outcome.rows();
        assert_eq!(
            (rows[0].cells[0].column_span, rows[0].cells[0].row_span),
            (1000, 1)
        );
    }
}

#[test]
fn boxnote_spans_still_place_later_cells_after_clamping() {
    let document = r#"{"doc":{"type":"doc","content":[{"type":"table","content":[
        {"type":"table_row","content":[
            {"type":"table_cell","attrs":{"colspan":4000,"rowspan":4000},"content":[{"type":"paragraph","content":[{"type":"text","text":"a"}]}]},
            {"type":"table_cell","content":[{"type":"paragraph","content":[{"type":"text","text":"b"}]}]}]},
        {"type":"table_row","content":[
            {"type":"table_cell","content":[{"type":"paragraph","content":[{"type":"text","text":"c"}]}]}]}]}]}}"#;
    let outcome = hostile(Format::Boxnote, document.to_owned());
    outcome.within(BOUND);
    let rows = outcome.rows();
    assert_eq!(rows[0].cells[0].row_span, 2);
    assert_eq!(rows[0].cells[1].column_index, 1000);
    assert_eq!(
        rows[1].cells[0].column_index, 1000,
        "the spanning cell still covers the next row"
    );
}

// ---------------------------------------------------------------- Markdown

#[test]
fn a_yaml_alias_bomb_in_front_matter_is_skipped_with_a_warning() {
    // The review's 433-byte bomb peaked at 22.7 GB before emitting a byte.
    let mut front = String::from(
        "---\na: &a [\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\"]\n",
    );
    let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
    for pair in names.windows(2) {
        let (previous, name) = (pair[0], pair[1]);
        let aliases = vec![format!("*{previous}"); 10].join(",");
        front.push_str(&format!("{name}: &{name} [{aliases}]\n"));
    }
    front.push_str("---\n# Body\n");
    assert!(front.len() < 600, "{}", front.len());
    let outcome = hostile(Format::Markdown, front);
    outcome.within(BOUND);
    assert!(outcome.warned(pb::WarningCode::Metadata, "aliases"));
    assert!(outcome.text().contains("Body"));
}

#[test]
fn deeply_nested_toml_front_matter_is_skipped_instead_of_overflowing() {
    // The TOML reader recurses once per array or inline table, so these
    // aborted the process before the nesting bound.
    for opener in ["[", "{b="] {
        let document = format!("+++\na = {}\n+++\n# Body\n", opener.repeat(100_000));
        let outcome = hostile(Format::Markdown, document);
        outcome.within(BOUND);
        assert!(outcome.warned(pb::WarningCode::Metadata, "not a TOML mapping"));
        assert!(outcome.text().contains("Body"));
    }
}

#[test]
fn a_few_honest_aliases_still_load() {
    let document = "---\nbase: &base\n  owner: ada\ncopy: *base\ntitle: Aliased\n---\n\nbody\n";
    let outcome = hostile(Format::Markdown, document.to_owned());
    let Some(pb::parse_markup_response::Event::Info(info)) = outcome
        .events
        .first()
        .and_then(|event| event.event.as_ref())
    else {
        panic!("info first");
    };
    assert_eq!(info.title.as_deref(), Some("Aliased"));
    assert!(info.metadata.contains_key("copy"));
    assert!(outcome.status().warnings.is_empty());
}

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

#[test]
fn a_client_that_hangs_up_stops_the_latex_reader_before_its_first_event() {
    // The reader works a long time before the fold emits anything, so it
    // checks the token itself: the probe says "gone" from its second call
    // on, which is from inside the reader.
    let calls = Arc::new(AtomicU32::new(0));
    let probe = Arc::clone(&calls);
    let cancel = Cancel::new(
        None,
        Some(Arc::new(move || probe.fetch_add(1, Ordering::Relaxed) >= 1)),
    );
    let outcome = hostile_with(Format::Latex, "word ".repeat(200_000), cancel);
    assert!(
        matches!(outcome.result, Err(ParseError::ConsumerGone)),
        "{:?}",
        outcome.result
    );
    assert!(
        outcome.events.is_empty(),
        "stopped inside the reader, before info"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}
