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

    /// Serialize to the wire form carried inside a `StateDiff` datagram.
    ///
    /// Layout (all integers big-endian):
    /// ```text
    ///   flags    : u8     bit0 = resized present
    ///   [cols,rows : u16,u16]   if resized
    ///   cursor   : u16,u16
    ///   nruns    : u16
    ///   per run  : row:u16  col:u16  ncells:u16  ncells×Cell
    ///   Cell     : char:u32  flags:u8  fg:Color  bg:Color
    ///   Color    : 0x00 | 0x01 idx:u8 | 0x02 r:u8 g:u8 b:u8
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        o.push(if self.resized.is_some() { 1 } else { 0 });
        if let Some((cols, rows)) = self.resized {
            o.extend_from_slice(&cols.to_be_bytes());
            o.extend_from_slice(&rows.to_be_bytes());
        }
        o.extend_from_slice(&self.cursor.0.to_be_bytes());
        o.extend_from_slice(&self.cursor.1.to_be_bytes());
        o.extend_from_slice(&(self.runs.len() as u16).to_be_bytes());
        for run in &self.runs {
            o.extend_from_slice(&run.row.to_be_bytes());
            o.extend_from_slice(&run.col.to_be_bytes());
            o.extend_from_slice(&(run.cells.len() as u16).to_be_bytes());
            for cell in &run.cells {
                o.extend_from_slice(&(cell.c as u32).to_be_bytes());
                o.push(cell.flags);
                encode_color(&mut o, cell.fg);
                encode_color(&mut o, cell.bg);
            }
        }
        o
    }

    /// Parse the wire form. `None` on truncation or a malformed field, so a
    /// corrupt datagram is dropped rather than mis-applied.
    pub fn decode(buf: &[u8]) -> Option<StateDiff> {
        let mut r = Reader { buf, pos: 0 };
        let flags = r.u8()?;
        let resized = if flags & 1 != 0 { Some((r.u16()?, r.u16()?)) } else { None };
        let cursor = (r.u16()?, r.u16()?);
        let nruns = r.u16()? as usize;
        let mut runs = Vec::with_capacity(nruns.min(4096));
        for _ in 0..nruns {
            let row = r.u16()?;
            let col = r.u16()?;
            let ncells = r.u16()? as usize;
            let mut cells = Vec::with_capacity(ncells.min(4096));
            for _ in 0..ncells {
                let c = char::from_u32(r.u32()?)?;
                let cflags = r.u8()?;
                let fg = r.color()?;
                let bg = r.color()?;
                cells.push(Cell { c, fg, bg, flags: cflags });
            }
            runs.push(CellRun { row, col, cells });
        }
        Some(StateDiff { resized, runs, cursor })
    }
}

fn encode_color(o: &mut Vec<u8>, color: crate::Color) {
    use crate::Color::*;
    match color {
        Default => o.push(0),
        Indexed(n) => {
            o.push(1);
            o.push(n);
        }
        Rgb(red, g, b) => {
            o.push(2);
            o.push(red);
            o.push(g);
            o.push(b);
        }
    }
}

/// Bounds-checked big-endian reader; every accessor returns `None` past EOF.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn color(&mut self) -> Option<crate::Color> {
        use crate::Color;
        match self.u8()? {
            0 => Some(Color::Default),
            1 => Some(Color::Indexed(self.u8()?)),
            2 => Some(Color::Rgb(self.u8()?, self.u8()?, self.u8()?)),
            _ => None,
        }
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

    /// encode → decode reproduces the diff exactly, for varied content.
    fn assert_wire_round_trip(old_feed: &[u8], new_extra: &[u8]) {
        let mut old = Terminal::new(20, 5);
        old.feed(old_feed);
        let mut new = Terminal::new(20, 5);
        new.feed(old_feed);
        new.feed(new_extra);
        let diff = StateDiff::between(old.grid(), new.grid());
        let wire = diff.encode();
        assert_eq!(StateDiff::decode(&wire), Some(diff), "wire round-trip");
    }

    #[test]
    fn wire_round_trip_text() {
        assert_wire_round_trip(b"hello", b" world");
    }

    #[test]
    fn wire_round_trip_colors_attrs_unicode() {
        assert_wire_round_trip(
            b"plain",
            b"\r\x1b[1;38;5;200m\x1b[48;2;1;2;3mX\x1b[0m\xe2\x9c\x93",
        );
    }

    #[test]
    fn wire_round_trip_resize() {
        let mut old = Terminal::new(20, 5);
        old.feed(b"before");
        let mut newt = Terminal::new(20, 5);
        newt.feed(b"before");
        newt.resize(8, 3);
        newt.feed(b"\x1b[Hafter");
        let diff = StateDiff::between(old.grid(), newt.grid());
        assert!(diff.resized.is_some());
        assert_eq!(StateDiff::decode(&diff.encode()), Some(diff));
    }

    #[test]
    fn wire_empty_diff() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"abc");
        let d = StateDiff::between(t.grid(), t.grid());
        let w = d.encode();
        assert_eq!(StateDiff::decode(&w), Some(d));
    }

    #[test]
    fn wire_decode_rejects_truncation() {
        let mut old = Terminal::new(10, 1);
        old.feed(b"abcdefghij");
        let mut new = Terminal::new(10, 1);
        new.feed(b"XYZdefghij");
        let w = StateDiff::between(old.grid(), new.grid()).encode();
        // Any prefix shorter than the whole must fail cleanly (no panic).
        for n in 0..w.len() {
            assert_eq!(StateDiff::decode(&w[..n]), None, "prefix len {n}");
        }
        assert!(StateDiff::decode(&w).is_some());
    }

    #[test]
    fn wire_decode_rejects_bad_color_tag() {
        // cursor (0,0), 1 run at (0,0) with 1 cell whose fg tag = 9 (invalid).
        let mut w = vec![0u8]; // no resize
        w.extend_from_slice(&0u16.to_be_bytes()); // cursor row
        w.extend_from_slice(&0u16.to_be_bytes()); // cursor col
        w.extend_from_slice(&1u16.to_be_bytes()); // nruns
        w.extend_from_slice(&0u16.to_be_bytes()); // row
        w.extend_from_slice(&0u16.to_be_bytes()); // col
        w.extend_from_slice(&1u16.to_be_bytes()); // ncells
        w.extend_from_slice(&(b'A' as u32).to_be_bytes());
        w.push(0); // flags
        w.push(9); // bogus fg color tag
        assert_eq!(StateDiff::decode(&w), None);
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
