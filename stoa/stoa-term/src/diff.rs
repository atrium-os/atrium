//! `StateDiff` — the change between two [`Grid`]s (stoa.md §3.3).
//!
//! The server holds the authoritative grid; each client tracks its
//! last-acked grid. [`StateDiff::between`] computes the minimal set of
//! changed **cell runs** (plus cursor + dimensions); the client
//! [`StateDiff::apply`]s it to converge. The key correctness property
//! (round-trip): `apply(old, between(old, new)) == new`.
//!
//! Wire serialization lives separately (the diff goes inside a `StateDiff`
//! datagram payload); this module is the pure algorithm.

use crate::{Cell, Grid};

/// A horizontal run of changed cells starting at `(row, col)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRun {
    pub row: u16,
    pub col: u16,
    pub cells: Vec<Cell>,
}

/// The difference between two grids: changed cell runs + the new cursor.
/// `resized` carries the new dimensions when they changed (the client
/// resizes before applying runs, and a resize forces a full repaint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDiff {
    pub resized: Option<(u16, u16)>, // (cols, rows) if changed
    pub runs: Vec<CellRun>,
    pub cursor: (u16, u16),
}

impl StateDiff {
    /// Compute the diff that turns `old` into `new`.
    pub fn between(old: &Grid, new: &Grid) -> StateDiff {
        let dims_changed = old.cols() != new.cols() || old.rows() != new.rows();
        let resized = dims_changed.then(|| (new.cols(), new.rows()));

        let mut runs = Vec::new();
        for r in 0..new.rows() {
            let new_row = new.row(r);
            // After a resize the client's grid is reset to blanks at the new
            // size, so every non-blank cell is "changed" → full repaint.
            let old_row: Option<&[Cell]> = if dims_changed { None } else { Some(old.row(r)) };
            let blank = Cell::default();

            let mut c = 0u16;
            while (c as usize) < new_row.len() {
                let differs = |i: usize| match old_row {
                    Some(o) => new_row[i] != o[i],
                    None => new_row[i] != blank,
                };
                if differs(c as usize) {
                    let start = c;
                    let mut run = Vec::new();
                    while (c as usize) < new_row.len() && differs(c as usize) {
                        run.push(new_row[c as usize]);
                        c += 1;
                    }
                    runs.push(CellRun { row: r, col: start, cells: run });
                } else {
                    c += 1;
                }
            }
        }

        StateDiff { resized, runs, cursor: new.cursor() }
    }

    /// Apply this diff to `grid`, converging it toward the source `new`.
    pub fn apply(&self, grid: &mut Grid) {
        if let Some((cols, rows)) = self.resized {
            // Reset to blanks at the new size — matches `between`'s
            // full-repaint assumption after a resize.
            *grid = Grid::new(cols, rows);
        }
        for run in &self.runs {
            grid.write_run(run.row, run.col, &run.cells);
        }
        let (cr, cc) = self.cursor;
        grid.set_cursor(cr, cc);
    }

    /// True if nothing changed (no runs, no resize). The cursor alone moving
    /// still counts as empty here — callers may send a cursor-only update.
    pub fn is_empty(&self) -> bool {
        self.resized.is_none() && self.runs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;

    /// The core property: applying the diff reconstructs `new`.
    fn assert_round_trip(old_feed: &[u8], new_extra: &[u8]) {
        let mut old = Terminal::new(20, 5);
        old.feed(old_feed);
        let mut new = Terminal::new(20, 5);
        new.feed(old_feed);
        new.feed(new_extra);

        let diff = StateDiff::between(old.grid(), new.grid());
        let mut client = old.grid().clone();
        diff.apply(&mut client);
        assert_eq!(&client, new.grid(), "apply(old, diff) must equal new");
    }

    #[test]
    fn round_trip_text() {
        assert_round_trip(b"hello", b" world");
    }

    #[test]
    fn round_trip_overwrite_and_color() {
        assert_round_trip(b"plain text here", b"\r\x1b[31mRED\x1b[0m");
    }

    #[test]
    fn round_trip_newlines_and_scroll() {
        assert_round_trip(b"a\r\nb\r\nc", b"\r\nd\r\ne\r\nf");
    }

    #[test]
    fn round_trip_erase() {
        assert_round_trip(b"fill\r\nlines\r\nhere", b"\x1b[2J\x1b[Hclean");
    }

    #[test]
    fn no_change_is_empty() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"abc");
        let d = StateDiff::between(t.grid(), t.grid());
        assert!(d.is_empty());
        assert_eq!(d.runs.len(), 0);
    }

    #[test]
    fn minimal_runs_only_changed_cells() {
        let mut old = Terminal::new(10, 1);
        old.feed(b"abcdefghij");
        let mut new = Terminal::new(10, 1);
        new.feed(b"abXdefYhij"); // two single-cell changes
        let d = StateDiff::between(old.grid(), new.grid());
        assert_eq!(d.runs.len(), 2);
        assert_eq!(d.runs[0].col, 2);
        assert_eq!(d.runs[1].col, 6);
    }

    #[test]
    fn resize_forces_full_repaint_and_round_trips() {
        let mut old = Terminal::new(20, 5);
        old.feed(b"before resize");
        // emulate a server-side resize + new content
        let mut newt = Terminal::new(20, 5);
        newt.feed(b"before resize");
        newt.resize(8, 3);
        newt.feed(b"\x1b[Hafter");
        let new = newt.grid().clone();

        let diff = StateDiff::between(old.grid(), &new);
        assert_eq!(diff.resized, Some((8, 3)));
        let mut client = old.grid().clone();
        diff.apply(&mut client);
        assert_eq!(&client, &new);
    }
}
