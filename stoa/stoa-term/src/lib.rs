//! # stoa-term — server-side terminal grid model (Stoa S2)
//!
//! A VT/ANSI emulator that consumes pty output bytes and maintains an
//! **authoritative screen grid**: a `rows × cols` array of [`Cell`]s plus a
//! cursor and the current pen attributes. This is the substrate the rest of
//! S2 builds on — `StateDiff` (stoa.md §3.3) diffs two grids; multi-client
//! coherence and the SSP predictor (§3.4) reconcile against it; reattach
//! sends a full grid snapshot.
//!
//! It is OS-agnostic and does no I/O: feed it bytes with [`Terminal::feed`],
//! read the grid. Escape-sequence tokenizing is delegated to the permissive
//! `vte` parser; this crate implements its `Perform` trait to drive the grid.
//!
//! Scope (v1): printable text with line-wrap + scroll, the C0 controls
//! (LF/CR/BS/HT), cursor movement (CUU/CUD/CUF/CUB/CUP), erase (ED/EL), and
//! SGR attributes (bold/underline/reverse + 16/256/truecolor fg+bg).
//! Deferred: scroll regions (DECSTBM), alternate screen, tab stops, OSC
//! titles, DEC private modes — added as the terminals we host need them.

use vte::{Params, Parser, Perform};

mod diff;
pub use diff::{CellRun, StateDiff};

mod render;
pub use render::{render_composite, render_scrollback, render_snapshot, PaneView};

/// A terminal colour. `Default` = the terminal's default fg/bg; `Indexed`
/// covers the 16 ANSI + 256-colour palette; `Rgb` is 24-bit truecolour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Per-cell attribute flags (bitfield).
pub mod flags {
    pub const BOLD: u8 = 1 << 0;
    pub const UNDERLINE: u8 = 1 << 1;
    pub const REVERSE: u8 = 1 << 2;
    pub const ITALIC: u8 = 1 << 3;
    pub const DIM: u8 = 1 << 4;
}

/// One screen cell: a character plus its colours and attribute flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

/// Two grids are equal on their **visible** state — dimensions, cursor, and
/// cells — which is exactly what a [`StateDiff`] transmits. Internal
/// emulator state (the pen's pending SGR, the deferred-wrap flag) is not
/// compared, so `apply(old, between(old, new)) == new` holds.
impl PartialEq for Grid {
    fn eq(&self, o: &Self) -> bool {
        self.cols == o.cols
            && self.rows == o.rows
            && self.cur_row == o.cur_row
            && self.cur_col == o.cur_col
            && self.cells == o.cells
    }
}
impl Eq for Grid {}

/// The screen grid + cursor + pen. Implements [`Perform`] so the `vte`
/// parser drives it directly.
#[derive(Debug, Clone)]
pub struct Grid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>, // rows*cols, row-major
    cur_row: u16,
    cur_col: u16,
    pen: Cell, // attributes applied to newly-printed cells (its `c` is unused)
    /// Deferred line wrap: set after printing the last column, so writing
    /// exactly `cols` chars doesn't scroll until the next char arrives.
    pending_wrap: bool,
    /// Window title set via OSC 0/2 (shells/programs report cwd or command).
    title: String,
    /// Lines that have scrolled off the top, oldest first (capped). The
    /// scrollback the client can page up into.
    history: Vec<Vec<Cell>>,
}

/// Max scrollback lines retained per window.
pub const HISTORY_MAX: usize = 2000;

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cur_row: 0,
            cur_col: 0,
            pen: Cell::default(),
            pending_wrap: false,
            title: String::new(),
            history: Vec::new(),
        }
    }

    pub fn cols(&self) -> u16 { self.cols }
    pub fn rows(&self) -> u16 { self.rows }
    pub fn cursor(&self) -> (u16, u16) { (self.cur_row, self.cur_col) }
    /// The window title (OSC 0/2), empty if none set.
    pub fn title(&self) -> &str { &self.title }
    /// Scrolled-off lines, oldest first (the scrollback).
    pub fn history(&self) -> &[Vec<Cell>] { &self.history }

    /// Overwrite a horizontal run of cells starting at `(row, col)` —
    /// used by [`StateDiff::apply`]. Out-of-bounds cells are skipped.
    pub fn write_run(&mut self, row: u16, col: u16, cells: &[Cell]) {
        if row >= self.rows {
            return;
        }
        for (k, &cell) in cells.iter().enumerate() {
            let c = col as usize + k;
            if c < self.cols as usize {
                let i = self.idx(row, c as u16);
                self.cells[i] = cell;
            }
        }
    }

    /// Move the cursor (clamped) — used by [`StateDiff::apply`].
    pub fn set_cursor(&mut self, row: u16, col: u16) {
        self.cur_row = row.min(self.rows - 1);
        self.cur_col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    /// The cell at `(row, col)` (0-based). Panics out of bounds.
    pub fn cell(&self, row: u16, col: u16) -> &Cell {
        &self.cells[row as usize * self.cols as usize + col as usize]
    }

    /// One row's cells.
    pub fn row(&self, row: u16) -> &[Cell] {
        let w = self.cols as usize;
        let off = row as usize * w;
        &self.cells[off..off + w]
    }

    /// Resize, preserving the top-left content that still fits and clamping
    /// the cursor. (A reflow-preserving resize is a later refinement.)
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut next = vec![Cell::default(); cols as usize * rows as usize];
        for r in 0..rows.min(self.rows) {
            for c in 0..cols.min(self.cols) {
                next[r as usize * cols as usize + c as usize] = *self.cell(r, c);
            }
        }
        self.cells = next;
        self.cols = cols;
        self.rows = rows;
        self.cur_row = self.cur_row.min(rows - 1);
        self.cur_col = self.cur_col.min(cols - 1);
        self.pending_wrap = false;
    }

    fn idx(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    fn blank(&self) -> Cell {
        // Erased cells take the current background (so a coloured-bg clear
        // looks right), default fg, no flags.
        Cell { c: ' ', fg: Color::Default, bg: self.pen.bg, flags: 0 }
    }

    fn scroll_up(&mut self, n: u16) {
        let n = n.min(self.rows);
        let w = self.cols as usize;
        // Push the rows about to scroll off into history (capped), then drop
        // them and append n blank rows at the bottom.
        for r in 0..n as usize {
            self.history.push(self.cells[r * w..(r + 1) * w].to_vec());
        }
        let overflow = self.history.len().saturating_sub(HISTORY_MAX);
        if overflow > 0 {
            self.history.drain(0..overflow);
        }
        self.cells.drain(0..n as usize * w);
        let blank = self.blank();
        self.cells.resize(self.rows as usize * w, blank);
    }

    fn newline(&mut self) {
        if self.cur_row + 1 >= self.rows {
            self.scroll_up(1);
        } else {
            self.cur_row += 1;
        }
    }

    fn put(&mut self, c: char) {
        if self.pending_wrap {
            self.cur_col = 0;
            self.newline();
            self.pending_wrap = false;
        }
        let i = self.idx(self.cur_row, self.cur_col);
        self.cells[i] = Cell { c, ..self.pen };
        if self.cur_col + 1 >= self.cols {
            self.pending_wrap = true; // defer the wrap
        } else {
            self.cur_col += 1;
        }
    }

    fn move_to(&mut self, row: u16, col: u16) {
        self.cur_row = row.min(self.rows - 1);
        self.cur_col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    fn erase_range(&mut self, from: usize, to: usize) {
        let blank = self.blank();
        for cell in &mut self.cells[from..to] {
            *cell = blank;
        }
    }

    /// ED — erase in display. mode 0: cursor→end, 1: start→cursor, 2: all.
    fn erase_display(&mut self, mode: u16) {
        let cur = self.idx(self.cur_row, self.cur_col);
        let end = self.cells.len();
        match mode {
            0 => self.erase_range(cur, end),
            1 => self.erase_range(0, (cur + 1).min(end)),
            2 | 3 => self.erase_range(0, end),
            _ => {}
        }
    }

    /// EL — erase in line. mode 0: cursor→eol, 1: bol→cursor, 2: whole line.
    fn erase_line(&mut self, mode: u16) {
        let row_start = self.idx(self.cur_row, 0);
        let row_end = row_start + self.cols as usize;
        let cur = self.idx(self.cur_row, self.cur_col);
        match mode {
            0 => self.erase_range(cur, row_end),
            1 => self.erase_range(row_start, (cur + 1).min(row_end)),
            2 => self.erase_range(row_start, row_end),
            _ => {}
        }
    }

    /// Apply an SGR (Select Graphic Rendition) parameter sequence to the pen.
    fn sgr(&mut self, flat: &[u16]) {
        if flat.is_empty() {
            self.pen = Cell::default();
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            match flat[i] {
                0 => self.pen = Cell::default(),
                1 => self.pen.flags |= flags::BOLD,
                2 => self.pen.flags |= flags::DIM,
                3 => self.pen.flags |= flags::ITALIC,
                4 => self.pen.flags |= flags::UNDERLINE,
                7 => self.pen.flags |= flags::REVERSE,
                22 => self.pen.flags &= !(flags::BOLD | flags::DIM),
                23 => self.pen.flags &= !flags::ITALIC,
                24 => self.pen.flags &= !flags::UNDERLINE,
                27 => self.pen.flags &= !flags::REVERSE,
                30..=37 => self.pen.fg = Color::Indexed((flat[i] - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((flat[i] - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((flat[i] - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((flat[i] - 100 + 8) as u8),
                38 => i += self.sgr_extended(&flat[i..], true),
                48 => i += self.sgr_extended(&flat[i..], false),
                _ => {}
            }
            i += 1;
        }
    }

    /// Handle `38`/`48` extended colour: `;5;n` (256) or `;2;r;g;b` (rgb).
    /// `rest[0]` is the 38/48. Returns how many EXTRA params were consumed.
    fn sgr_extended(&mut self, rest: &[u16], fg: bool) -> usize {
        match rest.get(1) {
            Some(5) => {
                if let Some(&n) = rest.get(2) {
                    let col = Color::Indexed(n as u8);
                    if fg { self.pen.fg = col } else { self.pen.bg = col }
                    return 2;
                }
                1
            }
            Some(2) => {
                if let (Some(&r), Some(&g), Some(&b)) = (rest.get(2), rest.get(3), rest.get(4)) {
                    let col = Color::Rgb(r as u8, g as u8, b as u8);
                    if fg { self.pen.fg = col } else { self.pen.bg = col }
                    return 4;
                }
                1
            }
            _ => 1,
        }
    }
}

/// First CSI param as a 1-based count (default `def` when absent/zero).
fn p1(params: &Params, def: u16) -> u16 {
    match params.iter().next().and_then(|p| p.first()).copied() {
        Some(0) | None => def,
        Some(v) => v,
    }
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0A | 0x0B | 0x0C => self.newline(), // LF, VT, FF
            0x0D => {
                self.cur_col = 0;
                self.pending_wrap = false;
            }
            0x08 => {
                // BS
                self.pending_wrap = false;
                self.cur_col = self.cur_col.saturating_sub(1);
            }
            0x09 => {
                // HT → next multiple of 8
                self.pending_wrap = false;
                let next = (self.cur_col / 8 + 1) * 8;
                self.cur_col = next.min(self.cols - 1);
            }
            _ => {} // BEL etc. ignored
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'A' => self.cur_row = self.cur_row.saturating_sub(p1(params, 1)),
            'B' => self.cur_row = (self.cur_row + p1(params, 1)).min(self.rows - 1),
            'C' => self.cur_col = (self.cur_col + p1(params, 1)).min(self.cols - 1),
            'D' => self.cur_col = self.cur_col.saturating_sub(p1(params, 1)),
            'G' => self.cur_col = (p1(params, 1) - 1).min(self.cols - 1), // CHA (col)
            'd' => self.cur_row = (p1(params, 1) - 1).min(self.rows - 1), // VPA (row)
            'H' | 'f' => {
                // CUP/HVP: row;col, 1-based
                let mut it = params.iter();
                let row = it.next().and_then(|p| p.first()).copied().unwrap_or(1).max(1);
                let col = it.next().and_then(|p| p.first()).copied().unwrap_or(1).max(1);
                self.move_to(row - 1, col - 1);
            }
            'J' => self.erase_display(p1(params, 0)),
            'K' => self.erase_line(p1(params, 0)),
            'm' => {
                let flat: Vec<u16> = params.iter().flatten().copied().collect();
                self.sgr(&flat);
            }
            _ => {} // unhandled CSI ignored in v1
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 (icon name + title) / 2 (title) → window title.
        if let [kind, title, ..] = params {
            if matches!(*kind, b"0" | b"2") {
                self.title = String::from_utf8_lossy(title).into_owned();
            }
        }
    }
    // esc_dispatch / hook / put / unhook: ignored in v1.
}

/// A terminal: the `vte` parser + the [`Grid`] it drives. (`vte::Parser`
/// isn't `Debug`, so neither is this; the [`Grid`] is.)
pub struct Terminal {
    parser: Parser,
    grid: Grid,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Terminal { parser: Parser::new(), grid: Grid::new(cols, rows) }
    }

    /// Feed pty output bytes, updating the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        let Terminal { parser, grid } = self;
        for &b in bytes {
            parser.advance(grid, b);
        }
    }

    pub fn grid(&self) -> &Grid { &self.grid }

    /// The window title (OSC 0/2), empty if none.
    pub fn title(&self) -> &str { self.grid.title() }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
    }

    /// The visible screen as text, one `String` per row (trailing blanks
    /// trimmed). Convenience for tests/snapshots.
    pub fn rows_text(&self) -> Vec<String> {
        (0..self.grid.rows)
            .map(|r| {
                let s: String = self.grid.row(r).iter().map(|c| c.c).collect();
                s.trim_end().to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term() -> Terminal { Terminal::new(20, 5) }

    #[test]
    fn prints_text_and_advances_cursor() {
        let mut t = term();
        t.feed(b"hello");
        assert_eq!(t.grid().cell(0, 0).c, 'h');
        assert_eq!(t.grid().cell(0, 4).c, 'o');
        assert_eq!(t.grid().cursor(), (0, 5));
    }

    #[test]
    fn cr_lf_moves_to_next_line() {
        let mut t = term();
        t.feed(b"a\r\nb");
        assert_eq!(t.grid().cell(0, 0).c, 'a');
        assert_eq!(t.grid().cell(1, 0).c, 'b');
        assert_eq!(t.grid().cursor(), (1, 1));
    }

    #[test]
    fn backspace_and_tab() {
        let mut t = term();
        t.feed(b"ab\x08X"); // a, b, BS over b, X
        assert_eq!(t.grid().cell(0, 1).c, 'X');
        let mut t2 = term();
        t2.feed(b"\tx"); // tab to col 8, then x
        assert_eq!(t2.grid().cursor(), (0, 9));
        assert_eq!(t2.grid().cell(0, 8).c, 'x');
    }

    #[test]
    fn deferred_wrap_at_right_margin() {
        let mut t = Terminal::new(3, 4);
        t.feed(b"abc"); // exactly fills row 0 — no scroll yet (deferred)
        assert_eq!(t.grid().cursor(), (0, 2)); // cursor parked on last col
        t.feed(b"d"); // now wraps to row 1
        assert_eq!(t.grid().cell(0, 2).c, 'c');
        assert_eq!(t.grid().cell(1, 0).c, 'd');
    }

    #[test]
    fn scrolls_at_bottom() {
        let mut t = Terminal::new(5, 2);
        t.feed(b"one\r\ntwo\r\nthree");
        // "one" scrolled off; row 0 = two, row 1 = three
        assert_eq!(t.rows_text(), vec!["two".to_string(), "three".to_string()]);
    }

    #[test]
    fn cup_positions_cursor_1based() {
        let mut t = term();
        t.feed(b"\x1b[3;5HX");
        assert_eq!(t.grid().cell(2, 4).c, 'X'); // row 3 col 5 → (2,4)
    }

    #[test]
    fn sgr_bold_and_color() {
        let mut t = term();
        t.feed(b"\x1b[1;31mR\x1b[0mN");
        let r = t.grid().cell(0, 0);
        assert!(r.flags & flags::BOLD != 0);
        assert_eq!(r.fg, Color::Indexed(1));
        let n = t.grid().cell(0, 1);
        assert_eq!(n.flags, 0);
        assert_eq!(n.fg, Color::Default);
    }

    #[test]
    fn sgr_256_and_truecolor() {
        let mut t = term();
        t.feed(b"\x1b[38;5;200mA\x1b[38;2;10;20;30mB");
        assert_eq!(t.grid().cell(0, 0).fg, Color::Indexed(200));
        assert_eq!(t.grid().cell(0, 1).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn erase_line_and_display() {
        let mut t = term();
        t.feed(b"abcde\r\x1b[2K"); // write, CR to col0, erase whole line
        assert_eq!(t.rows_text()[0], "");
        let mut t2 = term();
        t2.feed(b"x\r\ny\r\nz\x1b[2J"); // erase whole display
        assert!(t2.rows_text().iter().all(|r| r.is_empty()));
    }

    #[test]
    fn osc_sets_window_title() {
        let mut t = term();
        t.feed(b"\x1b]2;my-shell: ~/src\x07text");
        assert_eq!(t.title(), "my-shell: ~/src");
        assert_eq!(t.grid().cell(0, 0).c, 't'); // the text after still prints
        // OSC 0 (icon+title) also sets it; bell- or ST-terminated.
        t.feed(b"\x1b]0;other\x1b\\");
        assert_eq!(t.title(), "other");
    }

    #[test]
    fn resize_preserves_topleft_and_clamps_cursor() {
        let mut t = Terminal::new(10, 4);
        t.feed(b"\x1b[4;8Hhello"); // cursor near bottom-right
        t.resize(5, 2);
        assert_eq!(t.grid().cols(), 5);
        assert_eq!(t.grid().rows(), 2);
        let (r, c) = t.grid().cursor();
        assert!(r < 2 && c < 5);
    }
}
