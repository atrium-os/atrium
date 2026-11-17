//! # stoa-term — server-side terminal grid model (Stoa S2)
//!
//! A VT/ANSI emulator that consumes pty output bytes and maintains an
//! **authoritative screen grid**: a `rows × cols` array of [`Cell`]s plus a
//! cursor, the current pen attributes and the terminal's modes. This is the
//! substrate the rest of S2 builds on — `StateDiff` (stoa.md §3.3) diffs two
//! grids; multi-client coherence and the SSP predictor (§3.4) reconcile
//! against it; reattach sends a full grid snapshot.
//!
//! It is OS-agnostic and does no I/O: feed it bytes with [`Terminal::feed`],
//! read the grid. Escape-sequence tokenizing is delegated to the permissive
//! `vte` parser; this crate implements its `Perform` trait to drive the grid.
//!
//! Scope (dev-tools-plan.md M1: enough for vi, `less`, `top`, Helix and
//! Neovim, as xterm-256color):
//! - text with deferred wrap (DECAWM), insert mode (IRM), wide characters,
//!   REP, the DEC special graphics charset (G0/G1, SI/SO);
//! - the C0 controls, cursor movement (CUU…CUP, HPA/VPA, CNL/CPL, origin
//!   mode), tab stops (HT, HTS, TBC, CHT, CBT);
//! - scroll regions (DECSTBM) with IND/RI/NEL, SU/SD, IL/DL, ICH/DCH/ECH,
//!   ED/EL; scrollback only from a top-anchored region of the main screen;
//! - the alternate screen (47, 1047, 1049) with its own saved cursor;
//!   DECSC/DECRC, DECSTR, RIS, DECALN;
//! - SGR incl. colon subparameters (`4:3`, `38:2::r:g:b`), 16/256/truecolour;
//! - DEC private modes the client acts on (DECCKM, DECKPAM, DECTCEM,
//!   bracketed paste 2004, focus 1004, mouse 9/1000/1002/1003 with SGR
//!   1006, alternate scroll 1007) and the cursor shape (DECSCUSR);
//! - replies to the queries programs wait on: DA1, DA2, DSR/CPR, DECRQM,
//!   XTVERSION, OSC 10/11 colour queries, window size (18t). The embedder
//!   takes them with [`Terminal::take_replies`] and writes them to the pty;
//! - OSC 0/2 titles (bounded, control characters stripped), OSC 7 cwd,
//!   OSC 52 clipboard **writes** ([`Terminal::take_clipboard`]); reads
//!   (`?`) are never answered.
//!
//! Not done: combining characters (zero-width code points are dropped),
//! left/right margins (DECLRMM), the kitty keyboard protocol, sixel.

use vte::{Params, Parser, Perform};

mod diff;
pub use diff::{CellRun, StateDiff};

mod render;
pub use render::{compose_grid, render_composite, render_diff, render_scrollback, render_snapshot, PaneView};

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
    pub const BOLD: u16 = 1 << 0;
    pub const UNDERLINE: u16 = 1 << 1;
    pub const REVERSE: u16 = 1 << 2;
    pub const ITALIC: u16 = 1 << 3;
    pub const DIM: u16 = 1 << 4;
    /// The cell holds a double-width character; the next cell is its
    /// [`WIDE_SPACER`].
    pub const WIDE: u16 = 1 << 5;
    /// The right half of a wide character (its `c` is a space).
    pub const WIDE_SPACER: u16 = 1 << 6;
    pub const STRIKE: u16 = 1 << 7;
    /// SGR 8: drawn as blank.
    pub const HIDDEN: u16 = 1 << 8;
    pub const BLINK: u16 = 1 << 9;
}

/// Terminal modes, as a bitfield the client reads ([`Grid::modes`]): the
/// ones that change what keys, the mouse and pastes send, and how the
/// cursor is drawn. Carried in every [`StateDiff`] that changes them.
pub mod mode {
    /// DECCKM (`?1`): cursor keys send `ESC O A` instead of `ESC [ A`.
    pub const CURSOR_KEYS: u32 = 1 << 0;
    /// DECKPAM (`ESC =`): the keypad sends application sequences.
    pub const APP_KEYPAD: u32 = 1 << 1;
    /// DECTCEM reset (`?25l`): the cursor is not drawn.
    pub const HIDE_CURSOR: u32 = 1 << 2;
    /// The alternate screen is showing (`?47`, `?1047`, `?1049`).
    pub const ALT_SCREEN: u32 = 1 << 3;
    /// `?2004`: pastes arrive between `ESC [200~` and `ESC [201~`.
    pub const BRACKETED_PASTE: u32 = 1 << 4;
    /// `?1004`: focus in/out sends `ESC [I` / `ESC [O`.
    pub const FOCUS: u32 = 1 << 5;
    /// `?9`: X10 mouse, presses only.
    pub const MOUSE_X10: u32 = 1 << 6;
    /// `?1000`: presses and releases.
    pub const MOUSE_CLICK: u32 = 1 << 7;
    /// `?1002`: and motion while a button is held.
    pub const MOUSE_DRAG: u32 = 1 << 8;
    /// `?1003`: and all motion.
    pub const MOUSE_MOTION: u32 = 1 << 9;
    /// `?1006`: SGR encoding of mouse reports.
    pub const MOUSE_SGR: u32 = 1 << 10;
    /// `?1007` (on by default): on the alternate screen without mouse
    /// reporting, the wheel sends cursor up/down.
    pub const ALT_SCROLL: u32 = 1 << 11;
    /// DECAWM (`?7`, on by default).
    pub const AUTOWRAP: u32 = 1 << 12;
    /// DECOM (`?6`): cursor addressing relative to the scroll region.
    pub const ORIGIN: u32 = 1 << 13;
    /// IRM (`4`): printing inserts.
    pub const INSERT: u32 = 1 << 14;
    /// LNM (`20`): LF also returns the carriage.
    pub const NEWLINE: u32 = 1 << 15;
    /// Any mouse reporting.
    pub const MOUSE_ANY: u32 = MOUSE_X10 | MOUSE_CLICK | MOUSE_DRAG | MOUSE_MOTION;
    /// DECSCUSR's shape, in bits 24..27: 0 default, 1/2 block, 3/4
    /// underline, 5/6 bar (odd = blinking). See [`cursor_shape`].
    pub const SHAPE_SHIFT: u32 = 24;
    pub const SHAPE_MASK: u32 = 0x7 << SHAPE_SHIFT;
    /// The modes a fresh terminal starts with.
    pub const DEFAULT: u32 = AUTOWRAP | ALT_SCROLL;

    /// The DECSCUSR shape (0..=6) in `modes`.
    pub fn cursor_shape(modes: u32) -> u8 {
        ((modes & SHAPE_MASK) >> SHAPE_SHIFT) as u8
    }
}

/// One screen cell: a character plus its colours and attribute flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

/// Two grids are equal on their **visible** state — dimensions, cursor,
/// cells, modes and title — which is exactly what a [`StateDiff`]
/// transmits. Internal emulator state (the pen's pending SGR, the
/// deferred-wrap flag, margins, tab stops, the hidden screen) is not
/// compared, so `apply(old, between(old, new)) == new` holds.
impl PartialEq for Grid {
    fn eq(&self, o: &Self) -> bool {
        self.cols == o.cols
            && self.rows == o.rows
            && self.cur_row == o.cur_row
            && self.cur_col == o.cur_col
            && self.cells == o.cells
            && self.modes == o.modes
            && self.title == o.title
    }
}
impl Eq for Grid {}

/// What DECSC saves (and DECRC, leaving the alternate screen, restores).
#[derive(Debug, Clone, Copy)]
struct Saved {
    row: u16,
    col: u16,
    pen: Cell,
    pending_wrap: bool,
    origin: bool,
    charsets: [Charset; 2],
    gl: u8,
}

impl Default for Saved {
    fn default() -> Self {
        Saved { row: 0, col: 0, pen: Cell::default(), pending_wrap: false, origin: false,
                charsets: [Charset::Ascii; 2], gl: 0 }
    }
}

/// A G0/G1 character set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Ascii,
    /// DEC special graphics (`ESC ( 0`): line drawing.
    Dec,
}

/// Bounds on what programs can hand the embedder.
pub const TITLE_MAX: usize = 256;
/// The largest OSC 52 text taken (decoded bytes).
pub const CLIPBOARD_MAX: usize = 256 * 1024;
/// Replies waiting beyond this are dropped (a program that queries in a
/// loop without reading).
const REPLIES_MAX: usize = 64 * 1024;

/// The colours a query (OSC 10/11) reports: the Atrium Terminal's.
const DEFAULT_FG: (u8, u8, u8) = (0xf2, 0xf2, 0xf2);
const DEFAULT_BG: (u8, u8, u8) = (0x12, 0x14, 0x17);

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
    /// Working directory reported by the shell via OSC 7 (`file://host/path`),
    /// if any. Used to respawn a shell in the same place after a `stoad`
    /// restart (S3a). `None` if the shell never emits OSC 7.
    cwd: Option<String>,
    /// Lines that have scrolled off the top, oldest first (capped). The
    /// scrollback the client can page up into.
    history: Vec<Vec<Cell>>,
    /// [`mode`] bits.
    modes: u32,
    /// The scroll region, inclusive rows.
    top: u16,
    bottom: u16,
    /// Tab stops, one per column.
    tabs: Vec<bool>,
    /// DECSC's slot for the showing screen, and the other screen's.
    saved: Saved,
    other_saved: Saved,
    /// The screen not showing (alternate while on main, and the reverse).
    other: Vec<Cell>,
    charsets: [Charset; 2],
    /// Which of G0/G1 is invoked (SI/SO).
    gl: u8,
    /// The last printed character, for REP.
    last: Option<char>,
    /// Bytes to write back to the pty (query replies).
    replies: Vec<u8>,
    /// The last OSC 52 write not yet taken.
    clip: Option<String>,
    /// The last `pkg` request (OSC 7770) not yet taken.
    ask: Option<String>,
}

/// Max scrollback lines retained per window.
pub const HISTORY_MAX: usize = 2000;

fn default_tabs(cols: u16) -> Vec<bool> {
    (0..cols).map(|c| c > 0 && c % 8 == 0).collect()
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let n = cols as usize * rows as usize;
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); n],
            cur_row: 0,
            cur_col: 0,
            pen: Cell::default(),
            pending_wrap: false,
            title: String::new(),
            cwd: None,
            history: Vec::new(),
            modes: mode::DEFAULT,
            top: 0,
            bottom: rows - 1,
            tabs: default_tabs(cols),
            saved: Saved::default(),
            other_saved: Saved::default(),
            other: vec![Cell::default(); n],
            charsets: [Charset::Ascii; 2],
            gl: 0,
            last: None,
            replies: Vec::new(),
            clip: None,
            ask: None,
        }
    }

    pub fn cols(&self) -> u16 { self.cols }
    pub fn rows(&self) -> u16 { self.rows }
    pub fn cursor(&self) -> (u16, u16) { (self.cur_row, self.cur_col) }
    /// The window title (OSC 0/2), empty if none set.
    pub fn title(&self) -> &str { &self.title }
    /// The shell's working directory (OSC 7), if it has reported one.
    pub fn cwd(&self) -> Option<&str> { self.cwd.as_deref() }
    /// Scrolled-off lines, oldest first (the scrollback).
    pub fn history(&self) -> &[Vec<Cell>] { &self.history }
    /// The [`mode`] bits.
    pub fn modes(&self) -> u32 { self.modes }
    /// Is mode bit `m` set?
    pub fn mode(&self, m: u32) -> bool { self.modes & m != 0 }

    /// Set the modes and title wholesale — used by [`StateDiff::apply`] and
    /// by a server compositing panes into one grid (the active pane's).
    pub fn set_meta(&mut self, modes: u32, title: &str) {
        self.modes = modes;
        self.title = title.to_string();
    }

    /// Set the modes alone (see [`Grid::set_meta`]).
    pub fn set_modes(&mut self, modes: u32) {
        self.modes = modes;
    }

    /// Set the title alone (see [`Grid::set_meta`]).
    pub fn set_title(&mut self, title: &str) {
        self.title = title.to_string();
    }

    /// Take the replies waiting to go back to the program (write them to
    /// the pty).
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Take the text the program last wrote to the clipboard (OSC 52).
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clip.take()
    }

    /// Take the words of the last `pkg` request (OSC 7770, homes.md §5.5):
    /// `ESC ] 7770 ; pkg ; <words> BEL` from a workspace's `/atrium/bin/pkg`.
    /// The words are package-name characters and single spaces only; the
    /// terminal shows them and asks the person, it never acts on them alone.
    pub fn take_ask(&mut self) -> Option<String> {
        self.ask.take()
    }

    /// The whole scrollback as plain text (oldest first): the history rows
    /// followed by the current screen rows, trailing blank lines trimmed.
    /// Used to persist a window's scrollback (S3a); attributes are dropped —
    /// content over colour for restored history.
    pub fn scrollback_text(&self) -> Vec<String> {
        let row_text = |cells: &[Cell]| {
            let s: String = cells.iter().filter(|c| c.flags & flags::WIDE_SPACER == 0).map(|c| c.c).collect();
            s.trim_end().to_string()
        };
        let mut out: Vec<String> = self.history.iter().map(|r| row_text(r)).collect();
        // The main screen's rows, even while a full-screen program shows
        // the alternate one.
        let main = if self.mode(mode::ALT_SCREEN) { &self.other } else { &self.cells };
        let w = self.cols as usize;
        for r in 0..self.rows as usize {
            out.push(row_text(&main[r * w..(r + 1) * w]));
        }
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out
    }

    /// Replace the scrollback history with these text lines (default
    /// attributes), keeping at most [`HISTORY_MAX`]. Used to restore a
    /// respawned window's scrollback after a `stoad` restart (S3a).
    pub fn restore_scrollback(&mut self, lines: &[String]) {
        let start = lines.len().saturating_sub(HISTORY_MAX);
        self.history = lines[start..]
            .iter()
            .map(|l| l.chars().map(|c| Cell { c, ..Cell::default() }).collect())
            .collect();
    }

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

    /// Resize, preserving the top-left content that still fits on both
    /// screens and clamping the cursor. The scroll region resets to the
    /// whole screen. (A reflow-preserving resize is a later refinement.)
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let refit = |old: &[Cell], ocols: u16, orows: u16| {
            let mut next = vec![Cell::default(); cols as usize * rows as usize];
            for r in 0..rows.min(orows) {
                for c in 0..cols.min(ocols) {
                    next[r as usize * cols as usize + c as usize] = old[r as usize * ocols as usize + c as usize];
                }
                // A wide character cut in half at the new right edge.
                if cols < ocols {
                    let last = &mut next[r as usize * cols as usize + cols as usize - 1];
                    if last.flags & flags::WIDE != 0 {
                        *last = Cell { c: ' ', flags: last.flags & !flags::WIDE, ..*last };
                    }
                }
            }
            next
        };
        self.cells = refit(&self.cells, self.cols, self.rows);
        self.other = refit(&self.other, self.cols, self.rows);
        let mut tabs = default_tabs(cols);
        for (c, t) in self.tabs.iter().enumerate().take(cols as usize) {
            tabs[c] = *t;
        }
        self.tabs = tabs;
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.cur_row = self.cur_row.min(rows - 1);
        self.cur_col = self.cur_col.min(cols - 1);
        for s in [&mut self.saved, &mut self.other_saved] {
            s.row = s.row.min(rows - 1);
            s.col = s.col.min(cols - 1);
        }
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

    fn reply(&mut self, s: &str) {
        if self.replies.len() + s.len() <= REPLIES_MAX {
            self.replies.extend_from_slice(s.as_bytes());
        }
    }

    // ---- scrolling -------------------------------------------------------

    /// Scroll rows `top..=bottom` up by `n`, blanking at the bottom. Lines
    /// leave into the scrollback only from the top of the main screen.
    fn scroll_up_in(&mut self, top: u16, bottom: u16, n: u16) {
        self.scroll_up_keep(top, bottom, n, true);
    }

    /// As [`Grid::scroll_up_in`]; `keep` false never saves lines (DL).
    fn scroll_up_keep(&mut self, top: u16, bottom: u16, n: u16, keep: bool) {
        if top > bottom {
            return;
        }
        let height = bottom - top + 1;
        let n = n.min(height);
        let w = self.cols as usize;
        if keep && top == 0 && !self.mode(mode::ALT_SCREEN) {
            for r in 0..n as usize {
                self.history.push(self.cells[r * w..(r + 1) * w].to_vec());
            }
            let overflow = self.history.len().saturating_sub(HISTORY_MAX);
            if overflow > 0 {
                self.history.drain(0..overflow);
            }
        }
        let (start, end) = (top as usize * w, (bottom as usize + 1) * w);
        self.cells[start..end].rotate_left(n as usize * w);
        let blank = self.blank();
        for cell in &mut self.cells[end - n as usize * w..end] {
            *cell = blank;
        }
    }

    /// Scroll rows `top..=bottom` down by `n`, blanking at the top.
    fn scroll_down_in(&mut self, top: u16, bottom: u16, n: u16) {
        if top > bottom {
            return;
        }
        let height = bottom - top + 1;
        let n = n.min(height);
        let w = self.cols as usize;
        let (start, end) = (top as usize * w, (bottom as usize + 1) * w);
        self.cells[start..end].rotate_right(n as usize * w);
        let blank = self.blank();
        for cell in &mut self.cells[start..start + n as usize * w] {
            *cell = blank;
        }
    }

    /// IND / LF: down a row, scrolling the region at its bottom margin.
    fn index(&mut self) {
        if self.cur_row == self.bottom {
            self.scroll_up_in(self.top, self.bottom, 1);
        } else if self.cur_row + 1 < self.rows {
            self.cur_row += 1;
        }
    }

    /// RI: up a row, scrolling the region down at its top margin.
    fn reverse_index(&mut self) {
        if self.cur_row == self.top {
            self.scroll_down_in(self.top, self.bottom, 1);
        } else if self.cur_row > 0 {
            self.cur_row -= 1;
        }
    }

    fn newline(&mut self) {
        self.index();
    }

    // ---- printing --------------------------------------------------------

    /// Clear the other half of a wide character at `(row, col)` before the
    /// cell is overwritten, so no half-character is left behind.
    fn break_wide(&mut self, row: u16, col: u16) {
        let i = self.idx(row, col);
        let f = self.cells[i].flags;
        if f & flags::WIDE_SPACER != 0 && col > 0 {
            let j = i - 1;
            self.cells[j] = Cell { c: ' ', flags: self.cells[j].flags & !flags::WIDE, ..self.cells[j] };
        }
        if f & flags::WIDE != 0 && col + 1 < self.cols {
            let j = i + 1;
            self.cells[j] = Cell { c: ' ', flags: self.cells[j].flags & !flags::WIDE_SPACER, ..self.cells[j] };
        }
    }

    /// Shift the cursor row's cells from the cursor right by `n` (insert).
    fn insert_blanks(&mut self, n: u16) {
        let row = self.cur_row;
        let col = self.cur_col;
        let n = n.min(self.cols - col) as usize;
        self.break_wide(row, col);
        let start = self.idx(row, col);
        let end = self.idx(row, 0) + self.cols as usize;
        self.cells[start..end].rotate_right(n);
        let blank = self.blank();
        for cell in &mut self.cells[start..start + n] {
            *cell = blank;
        }
        // A wide character pushed half off the right edge.
        let last = end - 1;
        if self.cells[last].flags & flags::WIDE != 0 {
            self.cells[last] = Cell { c: ' ', flags: self.cells[last].flags & !flags::WIDE, ..self.cells[last] };
        }
    }

    fn put(&mut self, c: char) {
        let c = if self.charsets[self.gl as usize] == Charset::Dec { dec_graphics(c) } else { c };
        let width = char_width(c);
        if width == 0 {
            return; // combining marks and other zero-width code points: not kept
        }
        if self.pending_wrap && self.mode(mode::AUTOWRAP) {
            self.cur_col = 0;
            self.newline();
        }
        self.pending_wrap = false;
        // A wide character with no room for its right half.
        if width == 2 && self.cur_col + 1 >= self.cols {
            if self.cols < 2 {
                return;
            }
            if self.mode(mode::AUTOWRAP) {
                let (r, cc) = (self.cur_row, self.cur_col);
                self.break_wide(r, cc);
                let i = self.idx(r, cc);
                self.cells[i] = self.blank();
                self.cur_col = 0;
                self.newline();
            } else {
                self.cur_col = self.cols - 2;
            }
        }
        if self.mode(mode::INSERT) {
            self.insert_blanks(width);
        }
        let (r, col) = (self.cur_row, self.cur_col);
        self.break_wide(r, col);
        let i = self.idx(r, col);
        if width == 2 {
            self.break_wide(r, col + 1);
            self.cells[i] = Cell { c, flags: self.pen.flags | flags::WIDE, ..self.pen };
            self.cells[i + 1] = Cell { c: ' ', flags: self.pen.flags | flags::WIDE_SPACER, ..self.pen };
        } else {
            self.cells[i] = Cell { c, ..self.pen };
        }
        self.last = Some(c);
        let next = self.cur_col + width;
        if next >= self.cols {
            // Park on the last column; wrap when the next character comes.
            self.cur_col = self.cols - 1;
            self.pending_wrap = self.mode(mode::AUTOWRAP);
        } else {
            self.cur_col = next;
        }
    }

    // ---- cursor ----------------------------------------------------------

    /// The rows the cursor may be addressed in (the region under DECOM).
    fn origin_rows(&self) -> (u16, u16) {
        if self.mode(mode::ORIGIN) { (self.top, self.bottom) } else { (0, self.rows - 1) }
    }

    fn move_to(&mut self, row: u16, col: u16) {
        self.cur_row = row.min(self.rows - 1);
        self.cur_col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    /// CUP with origin mode: `row`/`col` 0-based within the addressable area.
    fn goto(&mut self, row: u16, col: u16) {
        let (lo, hi) = self.origin_rows();
        let r = lo.saturating_add(row).min(hi);
        self.move_to(r, col);
    }

    fn cursor_up(&mut self, n: u16) {
        let floor = if self.cur_row >= self.top { self.top } else { 0 };
        self.cur_row = self.cur_row.saturating_sub(n).max(floor);
        self.pending_wrap = false;
    }

    fn cursor_down(&mut self, n: u16) {
        let ceil = if self.cur_row <= self.bottom { self.bottom } else { self.rows - 1 };
        self.cur_row = self.cur_row.saturating_add(n).min(ceil);
        self.pending_wrap = false;
    }

    fn tab_forward(&mut self, n: u16) {
        for _ in 0..n {
            let mut c = self.cur_col + 1;
            while c < self.cols - 1 && !self.tabs[c as usize] {
                c += 1;
            }
            self.cur_col = c.min(self.cols - 1);
        }
        self.pending_wrap = false;
    }

    fn tab_back(&mut self, n: u16) {
        for _ in 0..n {
            let mut c = self.cur_col.saturating_sub(1);
            while c > 0 && !self.tabs[c as usize] {
                c -= 1;
            }
            self.cur_col = c;
        }
        self.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved = Saved {
            row: self.cur_row,
            col: self.cur_col,
            pen: self.pen,
            pending_wrap: self.pending_wrap,
            origin: self.mode(mode::ORIGIN),
            charsets: self.charsets,
            gl: self.gl,
        };
    }

    fn restore_cursor(&mut self) {
        let s = self.saved;
        self.cur_row = s.row.min(self.rows - 1);
        self.cur_col = s.col.min(self.cols - 1);
        self.pen = s.pen;
        self.pending_wrap = s.pending_wrap;
        self.set_mode(mode::ORIGIN, s.origin);
        self.charsets = s.charsets;
        self.gl = s.gl;
    }

    fn set_mode(&mut self, m: u32, on: bool) {
        if on { self.modes |= m } else { self.modes &= !m }
    }

    // ---- the alternate screen --------------------------------------------

    fn swap_screens(&mut self) {
        std::mem::swap(&mut self.cells, &mut self.other);
        std::mem::swap(&mut self.saved, &mut self.other_saved);
        self.modes ^= mode::ALT_SCREEN;
    }

    /// Enter the alternate screen; `clear` it first (1047/1049).
    fn enter_alt(&mut self, clear: bool) {
        if self.mode(mode::ALT_SCREEN) {
            return;
        }
        self.swap_screens();
        if clear {
            let blank = self.blank();
            for c in &mut self.cells {
                *c = blank;
            }
        }
    }

    /// Back to the main screen; `clear` the alternate one on the way (1047).
    fn leave_alt(&mut self, clear: bool) {
        if !self.mode(mode::ALT_SCREEN) {
            return;
        }
        if clear {
            let blank = self.blank();
            for c in &mut self.cells {
                *c = blank;
            }
        }
        self.swap_screens();
    }

    // ---- erasing and editing ---------------------------------------------

    fn erase_range(&mut self, from: usize, to: usize) {
        let blank = self.blank();
        let w = self.cols as usize;
        // Wide characters straddling either edge lose their other half too.
        if from < to {
            let (r, c) = ((from / w) as u16, (from % w) as u16);
            self.break_wide(r, c);
            let (r, c) = (((to - 1) / w) as u16, ((to - 1) % w) as u16);
            self.break_wide(r, c);
        }
        for cell in &mut self.cells[from..to] {
            *cell = blank;
        }
    }

    /// ED — erase in display. mode 0: cursor→end, 1: start→cursor, 2: all,
    /// 3: the scrollback.
    fn erase_display(&mut self, mode: u16) {
        let cur = self.idx(self.cur_row, self.cur_col);
        let end = self.cells.len();
        match mode {
            0 => self.erase_range(cur, end),
            1 => self.erase_range(0, (cur + 1).min(end)),
            2 => self.erase_range(0, end),
            3 => self.history.clear(),
            _ => {}
        }
        self.pending_wrap = false;
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
        self.pending_wrap = false;
    }

    /// DCH: delete `n` cells at the cursor, pulling the rest left.
    fn delete_chars(&mut self, n: u16) {
        let n = n.min(self.cols - self.cur_col) as usize;
        let (r, c) = (self.cur_row, self.cur_col);
        self.break_wide(r, c);
        if (c as usize + n) < self.cols as usize {
            self.break_wide(r, c + n as u16);
        }
        let start = self.idx(r, c);
        let end = self.idx(r, 0) + self.cols as usize;
        self.cells[start..end].rotate_left(n);
        let blank = self.blank();
        for cell in &mut self.cells[end - n..end] {
            *cell = blank;
        }
        self.pending_wrap = false;
    }

    /// ECH: blank `n` cells from the cursor, nothing moves.
    fn erase_chars(&mut self, n: u16) {
        let n = n.min(self.cols - self.cur_col) as usize;
        let start = self.idx(self.cur_row, self.cur_col);
        self.erase_range(start, start + n);
        self.pending_wrap = false;
    }

    /// IL / DL: only inside the scroll region; the cursor goes to column 0.
    fn insert_lines(&mut self, n: u16) {
        if self.cur_row < self.top || self.cur_row > self.bottom {
            return;
        }
        self.scroll_down_in(self.cur_row, self.bottom, n);
        self.cur_col = 0;
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: u16) {
        if self.cur_row < self.top || self.cur_row > self.bottom {
            return;
        }
        // Never into the scrollback: these lines are deleted, not scrolled.
        self.scroll_up_keep(self.cur_row, self.bottom, n, false);
        self.cur_col = 0;
        self.pending_wrap = false;
    }

    /// DECSTBM: rows are 1-based, 0 = default.
    fn set_margins(&mut self, top: u16, bottom: u16) {
        let t = top.max(1) - 1;
        let b = if bottom == 0 { self.rows } else { bottom.min(self.rows) } - 1;
        if t < b {
            self.top = t;
            self.bottom = b;
            self.goto(0, 0);
        }
    }

    /// DECSTR: soft reset (screen contents stay).
    fn soft_reset(&mut self) {
        let keep = self.modes & (mode::ALT_SCREEN | mode::ALT_SCROLL);
        self.modes = (mode::DEFAULT & !mode::ALT_SCROLL) | keep;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.pen = Cell::default();
        self.charsets = [Charset::Ascii; 2];
        self.gl = 0;
        self.saved = Saved::default();
        self.pending_wrap = false;
    }

    /// RIS: full reset. The scrollback and the title stay (they belong to
    /// the window, not the program).
    fn full_reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        let history = std::mem::take(&mut self.history);
        let title = std::mem::take(&mut self.title);
        let cwd = self.cwd.take();
        let replies = std::mem::take(&mut self.replies);
        let clip = self.clip.take();
        *self = Grid::new(cols, rows);
        self.history = history;
        self.title = title;
        self.cwd = cwd;
        self.replies = replies;
        self.clip = clip;
    }

    // ---- SGR -------------------------------------------------------------

    /// Apply an SGR parameter list (each entry a parameter with its colon
    /// subparameters) to the pen.
    fn sgr(&mut self, ps: &[&[u16]]) {
        if ps.is_empty() {
            self.pen = Cell::default();
            return;
        }
        let mut i = 0;
        while i < ps.len() {
            let p = ps[i];
            let v = p.first().copied().unwrap_or(0);
            match v {
                0 => self.pen = Cell::default(),
                1 => self.pen.flags |= flags::BOLD,
                2 => self.pen.flags |= flags::DIM,
                3 => self.pen.flags |= flags::ITALIC,
                4 => {
                    // `4:0` turns underline off; `4:1`..`4:5` are styles.
                    if p.get(1) == Some(&0) {
                        self.pen.flags &= !flags::UNDERLINE;
                    } else {
                        self.pen.flags |= flags::UNDERLINE;
                    }
                }
                5 | 6 => self.pen.flags |= flags::BLINK,
                7 => self.pen.flags |= flags::REVERSE,
                8 => self.pen.flags |= flags::HIDDEN,
                9 => self.pen.flags |= flags::STRIKE,
                21 => self.pen.flags |= flags::UNDERLINE,
                22 => self.pen.flags &= !(flags::BOLD | flags::DIM),
                23 => self.pen.flags &= !flags::ITALIC,
                24 => self.pen.flags &= !flags::UNDERLINE,
                25 => self.pen.flags &= !flags::BLINK,
                27 => self.pen.flags &= !flags::REVERSE,
                28 => self.pen.flags &= !flags::HIDDEN,
                29 => self.pen.flags &= !flags::STRIKE,
                30..=37 => self.pen.fg = Color::Indexed((v - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((v - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((v - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((v - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let (color, used) = extended_color(ps, i);
                    match (v, color) {
                        (38, Some(c)) => self.pen.fg = c,
                        (48, Some(c)) => self.pen.bg = c,
                        _ => {} // 58: underline colour, not drawn
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }

    // ---- modes -----------------------------------------------------------

    fn dec_mode(&mut self, n: u16, on: bool) {
        match n {
            1 => self.set_mode(mode::CURSOR_KEYS, on),
            6 => {
                self.set_mode(mode::ORIGIN, on);
                self.goto(0, 0);
            }
            7 => {
                self.set_mode(mode::AUTOWRAP, on);
                if !on {
                    self.pending_wrap = false;
                }
            }
            9 => self.set_mouse(mode::MOUSE_X10, on),
            25 => self.set_mode(mode::HIDE_CURSOR, !on),
            47 => if on { self.enter_alt(false) } else { self.leave_alt(false) },
            1000 => self.set_mouse(mode::MOUSE_CLICK, on),
            1002 => self.set_mouse(mode::MOUSE_DRAG, on),
            1003 => self.set_mouse(mode::MOUSE_MOTION, on),
            1004 => self.set_mode(mode::FOCUS, on),
            1006 => self.set_mode(mode::MOUSE_SGR, on),
            1007 => self.set_mode(mode::ALT_SCROLL, on),
            1047 => if on { self.enter_alt(false) } else { self.leave_alt(true) },
            1048 => if on { self.save_cursor() } else { self.restore_cursor() },
            1049 => {
                if on {
                    if !self.mode(mode::ALT_SCREEN) {
                        self.save_cursor();
                        self.enter_alt(true);
                    }
                } else if self.mode(mode::ALT_SCREEN) {
                    self.leave_alt(false);
                    self.restore_cursor();
                }
            }
            2004 => self.set_mode(mode::BRACKETED_PASTE, on),
            _ => {}
        }
    }

    /// The mouse modes replace each other (the last one set wins).
    fn set_mouse(&mut self, m: u32, on: bool) {
        if on {
            self.modes &= !mode::MOUSE_ANY;
        }
        self.set_mode(m, on);
    }

    /// DECRQM's answer for DEC private mode `n`: 1 set, 2 reset, 0 unknown.
    fn dec_mode_state(&self, n: u16) -> u8 {
        let bit = match n {
            1 => mode::CURSOR_KEYS,
            6 => mode::ORIGIN,
            7 => mode::AUTOWRAP,
            9 => mode::MOUSE_X10,
            25 => return if self.mode(mode::HIDE_CURSOR) { 2 } else { 1 },
            47 | 1047 | 1049 => mode::ALT_SCREEN,
            1000 => mode::MOUSE_CLICK,
            1002 => mode::MOUSE_DRAG,
            1003 => mode::MOUSE_MOTION,
            1004 => mode::FOCUS,
            1006 => mode::MOUSE_SGR,
            1007 => mode::ALT_SCROLL,
            2004 => mode::BRACKETED_PASTE,
            _ => return 0,
        };
        if self.mode(bit) { 1 } else { 2 }
    }

    fn ansi_mode(&mut self, n: u16, on: bool) {
        match n {
            4 => self.set_mode(mode::INSERT, on),
            20 => self.set_mode(mode::NEWLINE, on),
            _ => {}
        }
    }

    // ---- OSC -------------------------------------------------------------

    fn set_title_from(&mut self, raw: &[u8]) {
        let s = String::from_utf8_lossy(raw);
        self.title = s.chars().filter(|c| !c.is_control()).take(TITLE_MAX).collect();
    }

    /// OSC 52: `Pc ; Pd`, with `Pd` base64 or `?`. Only writes are taken.
    fn osc52(&mut self, data: &[u8]) {
        if data == b"?" {
            return; // a read: never answered
        }
        if data.len() > CLIPBOARD_MAX * 4 / 3 + 4 {
            return;
        }
        if let Some(bytes) = base64_decode(data) {
            if bytes.len() <= CLIPBOARD_MAX {
                self.clip = Some(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
}

/// The longest `pkg` request taken (bytes).
pub const ASK_MAX: usize = 256;

/// A `pkg` request's words, if they are only package-name characters
/// (`A-Za-z0-9._+@/:=,~^*-`, as portcullisd's `check_pkg_argv`) separated
/// by spaces; normalised to single spaces.
pub fn pkg_words(raw: &[u8]) -> Option<String> {
    if raw.len() > ASK_MAX || !raw.iter().all(|&b| b.is_ascii_alphanumeric() || b"._+@/:=,~^*- ".contains(&b)) {
        return None;
    }
    let words: Vec<&str> = std::str::from_utf8(raw).ok()?.split(' ').filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return None;
    }
    Some(words.join(" "))
}

/// `38`/`48`/`58` colour at `ps[i]`, in either form: colon subparameters
/// (`38:5:n`, `38:2:r:g:b`, `38:2:cs:r:g:b`) or the following semicolon
/// parameters (`38;5;n`, `38;2;r;g;b`). Returns the colour and how many
/// EXTRA parameters it used.
fn extended_color(ps: &[&[u16]], i: usize) -> (Option<Color>, usize) {
    let p = ps[i];
    if p.len() > 1 {
        let c = match p[1] {
            5 => p.get(2).map(|&n| Color::Indexed(n as u8)),
            2 => {
                let rgb = if p.len() >= 6 { &p[3..6] } else if p.len() == 5 { &p[2..5] } else { &[][..] };
                (rgb.len() == 3).then(|| Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
            }
            _ => None,
        };
        return (c, 0);
    }
    let at = |k: usize| ps.get(i + k).and_then(|q| q.first()).copied();
    match at(1) {
        Some(5) => match at(2) {
            Some(n) => (Some(Color::Indexed(n as u8)), 2),
            None => (None, 1),
        },
        Some(2) => match (at(2), at(3), at(4)) {
            (Some(r), Some(g), Some(b)) => (Some(Color::Rgb(r as u8, g as u8, b as u8)), 4),
            _ => (None, ps.len() - i - 1),
        },
        Some(_) => (None, 1),
        None => (None, 0),
    }
}

/// How many cells `c` takes: 0 (combining, controls), 1, or 2 (East Asian
/// wide and fullwidth, most emoji).
fn char_width(c: char) -> u16 {
    if (c as u32) < 0x7f {
        return if (c as u32) >= 0x20 { 1 } else { 0 };
    }
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16
}

/// DEC special graphics (`ESC ( 0`): the line-drawing set ncurses uses for
/// borders.
fn dec_graphics(c: char) -> char {
    match c {
        '`' => '◆', 'a' => '▒', 'b' => '␉', 'c' => '␌', 'd' => '␍', 'e' => '␊',
        'f' => '°', 'g' => '±', 'h' => '␤', 'i' => '␋', 'j' => '┘', 'k' => '┐',
        'l' => '┌', 'm' => '└', 'n' => '┼', 'o' => '⎺', 'p' => '⎻', 'q' => '─',
        'r' => '⎼', 's' => '⎽', 't' => '├', 'u' => '┤', 'v' => '┴', 'w' => '┬',
        'x' => '│', 'y' => '≤', 'z' => '≥', '{' => 'π', '|' => '≠', '}' => '£',
        '~' => '·', '_' => ' ',
        other => other,
    }
}

/// Standard base64 (padding optional, whitespace not allowed); `None` on
/// anything else.
fn base64_decode(s: &[u8]) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let s = match s.iter().position(|&b| b == b'=') {
        Some(p) => {
            if s[p..].iter().any(|&b| b != b'=') || s.len() - p > 2 {
                return None;
            }
            &s[..p]
        }
        None => s,
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in s {
        acc = (acc << 6) | val(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The parameters as slices (each a parameter and its subparameters).
fn param_list(params: &Params) -> Vec<&[u16]> {
    params.iter().collect()
}

/// Parameter `i` (0-based), `def` when absent or 0.
fn nth(params: &Params, i: usize, def: u16) -> u16 {
    match params.iter().nth(i).and_then(|p| p.first()).copied() {
        Some(0) | None => def,
        Some(v) => v,
    }
}

/// Parameter `i`, raw (0 when absent).
fn raw(params: &Params, i: usize) -> u16 {
    params.iter().nth(i).and_then(|p| p.first()).copied().unwrap_or(0)
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0A | 0x0B | 0x0C => {
                // LF, VT, FF
                if self.mode(mode::NEWLINE) {
                    self.cur_col = 0;
                }
                self.pending_wrap = false;
                self.newline();
            }
            0x0D => {
                self.cur_col = 0;
                self.pending_wrap = false;
            }
            0x08 => {
                // BS
                self.pending_wrap = false;
                self.cur_col = self.cur_col.saturating_sub(1);
            }
            0x09 => self.tab_forward(1),
            0x0E => self.gl = 1, // SO
            0x0F => self.gl = 0, // SI
            _ => {}              // BEL etc. ignored
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return; // too many parameters or intermediates: not a sequence we know
        }
        match (intermediates, action) {
            ([], '@') => self.insert_blanks(nth(params, 0, 1)),
            ([], 'A') => self.cursor_up(nth(params, 0, 1)),
            ([], 'B') | ([], 'e') => self.cursor_down(nth(params, 0, 1)),
            ([], 'C') | ([], 'a') => {
                self.cur_col = self.cur_col.saturating_add(nth(params, 0, 1)).min(self.cols - 1);
                self.pending_wrap = false;
            }
            ([], 'D') => {
                self.cur_col = self.cur_col.saturating_sub(nth(params, 0, 1));
                self.pending_wrap = false;
            }
            ([], 'E') => {
                self.cursor_down(nth(params, 0, 1));
                self.cur_col = 0;
            }
            ([], 'F') => {
                self.cursor_up(nth(params, 0, 1));
                self.cur_col = 0;
            }
            ([], 'G') | ([], '`') => {
                self.cur_col = (nth(params, 0, 1) - 1).min(self.cols - 1);
                self.pending_wrap = false;
            }
            ([], 'd') => {
                let col = self.cur_col;
                self.goto(nth(params, 0, 1) - 1, col);
            }
            ([], 'H') | ([], 'f') => self.goto(nth(params, 0, 1) - 1, nth(params, 1, 1) - 1),
            ([], 'I') => self.tab_forward(nth(params, 0, 1)),
            ([], 'Z') => self.tab_back(nth(params, 0, 1)),
            ([], 'J') | ([b'?'], 'J') => self.erase_display(raw(params, 0)),
            ([], 'K') | ([b'?'], 'K') => self.erase_line(raw(params, 0)),
            ([], 'L') => self.insert_lines(nth(params, 0, 1)),
            ([], 'M') => self.delete_lines(nth(params, 0, 1)),
            ([], 'P') => self.delete_chars(nth(params, 0, 1)),
            ([], 'X') => self.erase_chars(nth(params, 0, 1)),
            ([], 'S') => self.scroll_up_in(self.top, self.bottom, nth(params, 0, 1)),
            // `CSI T` with five parameters is mouse tracking: not ours.
            ([], 'T') if params.len() <= 1 => self.scroll_down_in(self.top, self.bottom, nth(params, 0, 1)),
            ([], 'b') => {
                if let Some(c) = self.last {
                    for _ in 0..nth(params, 0, 1).min(self.cols * self.rows) {
                        self.put(c);
                    }
                }
            }
            ([], 'g') => match raw(params, 0) {
                0 => {
                    let c = self.cur_col as usize;
                    self.tabs[c] = false;
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            ([], 'h') => params.iter().for_each(|p| self.ansi_mode(p[0], true)),
            ([], 'l') => params.iter().for_each(|p| self.ansi_mode(p[0], false)),
            ([b'?'], 'h') => params.iter().for_each(|p| self.dec_mode(p[0], true)),
            ([b'?'], 'l') => params.iter().for_each(|p| self.dec_mode(p[0], false)),
            ([], 'm') => {
                let ps = param_list(params);
                self.sgr(&ps);
            }
            ([], 'r') => self.set_margins(raw(params, 0), raw(params, 1)),
            ([], 's') => self.save_cursor(),
            ([], 'u') => self.restore_cursor(),
            // DA1: a VT220 with ANSI colour.
            ([], 'c') if raw(params, 0) == 0 => self.reply("\x1b[?62;22c"),
            // DA2.
            ([b'>'], 'c') if raw(params, 0) == 0 => self.reply("\x1b[>1;10;0c"),
            ([], 'n') => match raw(params, 0) {
                5 => self.reply("\x1b[0n"),
                6 => {
                    let (lo, _) = self.origin_rows();
                    let s = format!("\x1b[{};{}R", self.cur_row - lo.min(self.cur_row) + 1, self.cur_col + 1);
                    self.reply(&s);
                }
                _ => {}
            },
            ([b'?'], 'n') if raw(params, 0) == 6 => {
                let (lo, _) = self.origin_rows();
                let s = format!("\x1b[?{};{};1R", self.cur_row - lo.min(self.cur_row) + 1, self.cur_col + 1);
                self.reply(&s);
            }
            // DECRQM.
            ([b'?', b'$'], 'p') => {
                let n = raw(params, 0);
                let s = format!("\x1b[?{};{}$y", n, self.dec_mode_state(n));
                self.reply(&s);
            }
            ([b'$'], 'p') => {
                let n = raw(params, 0);
                let st = match n {
                    4 => if self.mode(mode::INSERT) { 1 } else { 2 },
                    20 => if self.mode(mode::NEWLINE) { 1 } else { 2 },
                    _ => 0,
                };
                let s = format!("\x1b[{n};{st}$y");
                self.reply(&s);
            }
            ([b'!'], 'p') => self.soft_reset(),
            // DECSCUSR.
            ([b' '], 'q') => {
                let shape = raw(params, 0).min(6) as u32;
                self.modes = (self.modes & !mode::SHAPE_MASK) | (shape << mode::SHAPE_SHIFT);
            }
            // XTVERSION.
            ([b'>'], 'q') => self.reply("\x1bP>|stoa-term(1)\x1b\\"),
            // Window size in characters.
            ([], 't') if raw(params, 0) == 18 => {
                let s = format!("\x1b[8;{};{}t", self.rows, self.cols);
                self.reply(&s);
            }
            _ => {} // anything else (kitty keyboard, XTMODKEYS, ...): ignored
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => {
                self.pending_wrap = false;
                self.index();
            }
            ([], b'E') => {
                self.cur_col = 0;
                self.pending_wrap = false;
                self.index();
            }
            ([], b'M') => {
                self.pending_wrap = false;
                self.reverse_index();
            }
            ([], b'H') => {
                let c = self.cur_col as usize;
                self.tabs[c] = true;
            }
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.modes |= mode::APP_KEYPAD,
            ([], b'>') => self.modes &= !mode::APP_KEYPAD,
            ([b'('], b'0') => self.charsets[0] = Charset::Dec,
            ([b'('], _) => self.charsets[0] = Charset::Ascii,
            ([b')'], b'0') => self.charsets[1] = Charset::Dec,
            ([b')'], _) => self.charsets[1] = Charset::Ascii,
            ([b'#'], b'8') => {
                // DECALN: fill the screen with E.
                for c in &mut self.cells {
                    *c = Cell { c: 'E', ..Cell::default() };
                }
                self.top = 0;
                self.bottom = self.rows - 1;
                self.move_to(0, 0);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let end = if bell_terminated { "\x07" } else { "\x1b\\" };
        match params {
            // OSC 0 (icon name + title) / 2 (title) → window title.
            [kind, title, ..] if matches!(*kind, b"0" | b"2") => self.set_title_from(title),
            // OSC 7 (`file://host/path`) → working directory.
            [b"7", uri, ..] => {
                self.cwd = parse_osc7_cwd(&String::from_utf8_lossy(uri));
            }
            // OSC 10/11 `?`: the default foreground/background.
            [kind @ (b"10" | b"11"), b"?"] => {
                let (r, g, b) = if *kind == b"10" { DEFAULT_FG } else { DEFAULT_BG };
                let k = if *kind == b"10" { 10 } else { 11 };
                let s = format!("\x1b]{k};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}{end}");
                self.reply(&s);
            }
            // OSC 52 (`52;c;base64`): a clipboard write.
            [b"52", _target, data] => self.osc52(data),
            // OSC 7770 (`7770;pkg;words`): a workspace asks to run pkg.
            [b"7770", b"pkg", words] => self.ask = pkg_words(words),
            _ => {}
        }
    }
}

/// Parse an OSC 7 value (`file://host/abs/path`, or `file:///abs/path`) into a
/// local absolute path, percent-decoded. Returns `None` if it isn't a
/// `file://` URI with an absolute path. The host part (between `//` and the
/// first `/` of the path) is ignored — we only respawn locally.
fn parse_osc7_cwd(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // The path begins at the first '/' (after an optional host). `file:///x`
    // → rest is `/x`; `file://h/x` → rest is `h/x`, path starts at `/x`.
    let slash = rest.find('/')?;
    let path = &rest[slash..];
    let decoded = percent_decode(path);
    decoded.starts_with('/').then_some(decoded)
}

/// Minimal percent-decoding (`%20` → space, etc.); leaves malformed escapes
/// as-is. Enough for OSC 7 paths, which only really encode spaces/specials.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

    /// Replies to the program's queries: write these to the pty.
    pub fn take_replies(&mut self) -> Vec<u8> { self.grid.take_replies() }

    /// The program's last clipboard write (OSC 52), if any.
    pub fn take_clipboard(&mut self) -> Option<String> { self.grid.take_clipboard() }
    /// The last `pkg` request (OSC 7770), if any: see [`Grid::take_ask`].
    pub fn take_ask(&mut self) -> Option<String> { self.grid.take_ask() }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
    }

    /// The window's scrollback as text (history + screen), for persistence.
    pub fn scrollback_text(&self) -> Vec<String> {
        self.grid.scrollback_text()
    }

    /// Restore a persisted scrollback into this terminal's history.
    pub fn restore_scrollback(&mut self, lines: &[String]) {
        self.grid.restore_scrollback(lines);
    }

    /// The visible screen as text, one `String` per row (trailing blanks
    /// trimmed; a wide character's right half left out). Convenience for
    /// tests/snapshots.
    pub fn rows_text(&self) -> Vec<String> {
        (0..self.grid.rows)
            .map(|r| {
                let s: String = self.grid.row(r).iter()
                    .filter(|c| c.flags & flags::WIDE_SPACER == 0)
                    .map(|c| c.c).collect();
                s.trim_end().to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
