// SPDX-License-Identifier: Apache-2.0

//! Row and column spans placed the way the HTML table model places them,
//! shared by every fold whose source puts `colspan` / `rowspan` on a cell
//! (the HTML fold and the BoxNote fold).
//!
//! The cost is proportional to the cells, never to the area the spans
//! claim: a cell spanning down is a lane (first column, end column, first
//! row no longer covered) that later rows sweep in column order, and the
//! spans are clamped to the model's own limits before a lane opens. A cell
//! asking for the whole plane therefore costs one lane, where recording
//! each position it covers was area work and area memory from a few
//! hundred bytes of input.

/// The largest `colspan` the HTML table model honours.
pub const MAX_COLSPAN: u32 = 1000;

/// The largest `rowspan` the HTML table model honours.
pub const MAX_ROWSPAN: u32 = 65534;

/// How many cells may span down across later rows at once before further
/// `rowspan`s are dropped. Placing a cell sweeps the lanes still open, so
/// this bounds the cost of every row.
pub const MAX_CARRIED_SPANS: usize = 1024;

/// The spans a cell asks for, as the source wrote them. Zero reads as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The `colspan` the source stated.
    pub column_span: u32,
    /// The `rowspan` the source stated.
    pub row_span: u32,
}

/// Where a cell landed once its spans were settled against the lanes
/// still open and the model's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The first column the cell covers.
    pub column: u32,
    /// The `colspan` honoured.
    pub column_span: u32,
    /// The `rowspan` honoured.
    pub row_span: u32,
}

/// A cell from an earlier row still covering later ones.
#[derive(Clone, Copy, Debug)]
struct Lane {
    /// First column covered.
    start: u32,
    /// One past the last column covered.
    end: u32,
    /// First row no longer covered.
    until: u32,
}

/// The lanes open over one table, swept row by row.
#[derive(Debug)]
pub struct SpanTracker {
    row_count: u32,
    /// Lanes from earlier rows, sorted by first column.
    carried: Vec<Lane>,
    /// Lanes the current row opened; carried once the row ends.
    started_here: Vec<Lane>,
    /// How far along `carried` the current row's sweep has read.
    sweep: usize,
    row_index: u32,
    /// The column the next cell is placed at, before the sweep.
    column: u32,
}

impl SpanTracker {
    /// A tracker for a table of `row_count` rows.
    #[must_use]
    pub fn new(row_count: usize) -> Self {
        Self {
            row_count: u32::try_from(row_count).unwrap_or(u32::MAX),
            carried: Vec::new(),
            started_here: Vec::new(),
            sweep: 0,
            row_index: 0,
            column: 0,
        }
    }

    /// Begin placing row `row_index`: the lanes that stop above it are
    /// dropped and the column starts at zero.
    pub fn begin_row(&mut self, row_index: u32) {
        self.carried.retain(|lane| lane.until > row_index);
        self.started_here.clear();
        self.sweep = 0;
        self.row_index = row_index;
        self.column = 0;
    }

    /// Place the next cell of the current row: past the columns a lane
    /// still covers, with its spans clamped, and opening a lane of its
    /// own when it spans down.
    pub fn place(&mut self, asked: Asked) -> Placed {
        self.skip_covered();
        let column_span = asked.column_span.clamp(1, MAX_COLSPAN);
        let rows_left = self.row_count.saturating_sub(self.row_index).max(1);
        let mut row_span = asked.row_span.clamp(1, MAX_ROWSPAN).min(rows_left);
        if row_span > 1 {
            if self.carried.len() + self.started_here.len() < MAX_CARRIED_SPANS {
                self.started_here.push(Lane {
                    start: self.column,
                    end: self.column.saturating_add(column_span),
                    until: self.row_index.saturating_add(row_span),
                });
            } else {
                // Past this many cells spanning down at once the table is
                // not a table anyone wrote; the span is dropped so every
                // later row stays cheap to place.
                row_span = 1;
            }
        }
        let placed = Placed {
            column: self.column,
            column_span,
            row_span,
        };
        self.column = self.column.saturating_add(column_span);
        placed
    }

    /// End the current row: the lanes it opened are carried into the
    /// rows below.
    pub fn end_row(&mut self) {
        self.carried.append(&mut self.started_here);
        self.carried.sort_unstable_by_key(|lane| lane.start);
    }

    /// Move the column past every lane covering it. Lanes are sorted and
    /// disjoint, so the sweep only ever moves forward.
    fn skip_covered(&mut self) {
        while let Some(lane) = self.carried.get(self.sweep) {
            if lane.end <= self.column {
                self.sweep += 1;
            } else if lane.start <= self.column {
                self.column = lane.end;
                self.sweep += 1;
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(column_span: u32, row_span: u32) -> Asked {
        Asked {
            column_span,
            row_span,
        }
    }

    #[test]
    fn a_cell_spanning_down_pushes_the_next_row_past_its_columns() {
        let mut tracker = SpanTracker::new(2);
        tracker.begin_row(0);
        assert_eq!(
            tracker.place(asked(2, 2)),
            Placed {
                column: 0,
                column_span: 2,
                row_span: 2
            }
        );
        assert_eq!(tracker.place(asked(1, 1)).column, 2);
        tracker.end_row();
        tracker.begin_row(1);
        assert_eq!(
            tracker.place(asked(1, 1)).column,
            2,
            "the spanning cell still covers columns 0 and 1"
        );
    }

    #[test]
    fn spans_are_clamped_to_the_model_and_to_the_rows_left() {
        let mut tracker = SpanTracker::new(3);
        tracker.begin_row(1);
        let placed = tracker.place(asked(u32::MAX, u32::MAX));
        assert_eq!(placed.column_span, MAX_COLSPAN);
        assert_eq!(placed.row_span, 2, "rows 1 and 2 are all that is left");
        assert_eq!(tracker.place(asked(0, 0)).column, MAX_COLSPAN);
    }

    #[test]
    fn a_span_past_the_carried_limit_is_dropped_to_one_row() {
        let mut tracker = SpanTracker::new(10);
        tracker.begin_row(0);
        for _ in 0..MAX_CARRIED_SPANS {
            assert_eq!(tracker.place(asked(1, 5)).row_span, 5);
        }
        assert_eq!(tracker.place(asked(1, 5)).row_span, 1);
    }

    #[test]
    fn lanes_that_stop_above_a_row_no_longer_cover_it() {
        let mut tracker = SpanTracker::new(3);
        tracker.begin_row(0);
        tracker.place(asked(1, 2));
        tracker.end_row();
        tracker.begin_row(2);
        assert_eq!(tracker.place(asked(1, 1)).column, 0);
    }
}
