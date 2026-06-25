//! Render a [`Grid`] back to ANSI/VT bytes — the inverse of the emulator.
//!
//! Used for the **reattach snapshot** (stoa.md §3.3, §7): when a client
//! attaches to an already-running session, the server paints the current
//! screen so the client sees it immediately instead of a blank until the
//! next output. The output is plain bytes the client's terminal renders —
//! no client-side grid needed.
//!
//! Correctness is the round-trip: feeding `render_snapshot(g)` into a fresh
//! [`Terminal`](crate::Terminal) of the same size reproduces `g`.

use std::fmt::Write as _;

use crate::{flags, Cell, Color, Grid};

/// Emit one cell, writing an SGR sequence only when its attributes differ
/// from the last emitted (`cur`). Shared by snapshot + scrollback render.
fn emit_cell(out: &mut String, cell: &Cell, cur: &mut Option<(Color, Color, u8)>) {
    let want = (cell.fg, cell.bg, cell.flags);
    let is_default = want == (Color::Default, Color::Default, 0);
    let need = match cur {
        Some(p) => *p != want,
        None => !is_default,
    };
    if need {
        out.push_str(&sgr_for(cell.fg, cell.bg, cell.flags));
        *cur = Some(want);
    }
    out.push(cell.c);
}

/// Render the whole grid as a self-contained repaint: reset, clear, paint
/// every cell with its attributes, then place the cursor. Suitable to send
/// to a freshly-attached client.
pub fn render_snapshot(grid: &Grid) -> Vec<u8> {
    let mut out = String::new();
    // Restore the window title (OSC 2), then reset attributes, clear, home.
    if !grid.title().is_empty() {
        let _ = write!(out, "\x1b]2;{}\x07", grid.title());
    }
    out.push_str("\x1b[0m\x1b[2J\x1b[H");

    // `None` = "we just emitted SGR reset" (default attributes in effect).
    let mut cur: Option<(Color, Color, u8)> = None;

    for r in 0..grid.rows() {
        // Position at the start of each row (so trailing-blank skips don't
        // misplace the next row).
        let _ = write!(out, "\x1b[{};1H", r + 1);
        for c in 0..grid.cols() {
            emit_cell(&mut out, grid.cell(r, c), &mut cur);
        }
    }

    out.push_str("\x1b[0m");
    let (cr, cc) = grid.cursor();
    let _ = write!(out, "\x1b[{};{}H", cr + 1, cc + 1);
    out.into_bytes()
}

/// Render a scrollback viewport: `rows` lines ending `offset` lines above
/// the live bottom. `offset == 0` is the live screen (== `render_snapshot`
/// minus the cursor). The combined buffer is history (oldest→newest) then
/// the live screen rows. The cursor is hidden in scrollback.
pub fn render_scrollback(grid: &Grid, offset: usize) -> Vec<u8> {
    let rows = grid.rows() as usize;
    let cols = grid.cols() as usize;
    let hist = grid.history();
    let total = hist.len() + rows;
    let offset = offset.min(hist.len()); // can't page past the oldest line
    let bottom = total - 1 - offset;
    let top = (bottom + 1).saturating_sub(rows);

    let mut out = String::new();
    if !grid.title().is_empty() {
        let _ = write!(out, "\x1b]2;{}\x07", grid.title());
    }
    out.push_str("\x1b[0m\x1b[2J\x1b[H");

    let blank = Cell::default();
    let mut cur: Option<(Color, Color, u8)> = None;
    for (disp, i) in (top..=bottom).enumerate() {
        let _ = write!(out, "\x1b[{};1H", disp + 1);
        for c in 0..cols {
            let cell = if i < hist.len() {
                hist[i].get(c).copied().unwrap_or(blank)
            } else {
                *grid.cell((i - hist.len()) as u16, c as u16)
            };
            emit_cell(&mut out, &cell, &mut cur);
        }
    }
    out.push_str("\x1b[0m");
    out.into_bytes()
}

/// Build a full SGR sequence (always starts with reset) for a cell's
/// attributes.
fn sgr_for(fg: Color, bg: Color, flags: u8) -> String {
    let mut p: Vec<String> = vec!["0".into()]; // reset, then re-apply
    if flags & flags::BOLD != 0 { p.push("1".into()); }
    if flags & flags::DIM != 0 { p.push("2".into()); }
    if flags & flags::ITALIC != 0 { p.push("3".into()); }
    if flags & flags::UNDERLINE != 0 { p.push("4".into()); }
    if flags & flags::REVERSE != 0 { p.push("7".into()); }
    push_color(&mut p, fg, true);
    push_color(&mut p, bg, false);
    format!("\x1b[{}m", p.join(";"))
}

fn push_color(p: &mut Vec<String>, color: Color, fg: bool) {
    match color {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => p.push(((if fg { 30 } else { 40 }) + n as u16).to_string()),
        Color::Indexed(n) if n < 16 => {
            p.push(((if fg { 90 } else { 100 }) + (n as u16 - 8)).to_string())
        }
        Color::Indexed(n) => {
            p.push(if fg { "38" } else { "48" }.into());
            p.push("5".into());
            p.push(n.to_string());
        }
        Color::Rgb(r, g, b) => {
            p.push(if fg { "38" } else { "48" }.into());
            p.push("2".into());
            p.push(r.to_string());
            p.push(g.to_string());
            p.push(b.to_string());
        }
    }
}

/// One pane placed at `(top, left)` in the window, showing `grid`.
pub struct PaneView<'a> {
    pub top: u16,
    pub left: u16,
    pub grid: &'a Grid,
}

/// Composite several panes into one `cols`×`rows` screen: blit each pane's
/// grid at its position, draw vertical (`vdivs` columns) and horizontal
/// (`hdivs` rows) dividers, and place the cursor (the active pane's, in
/// window coordinates). Lets a multi-pane window be sent as one repaint —
/// the client stays a plain renderer.
pub fn render_composite(
    cols: u16,
    rows: u16,
    panes: &[PaneView],
    vdivs: &[u16],
    hdivs: &[u16],
    cursor: (u16, u16),
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str("\x1b[0m\x1b[2J\x1b[H");

    let mut cur: Option<(Color, Color, u8)> = None;
    for p in panes {
        let g = p.grid;
        for r in 0..g.rows() {
            let wr = p.top + r;
            if wr >= rows {
                break;
            }
            let _ = write!(out, "\x1b[{};{}H", wr + 1, p.left + 1);
            for c in 0..g.cols() {
                if p.left + c >= cols {
                    break;
                }
                emit_cell(&mut out, g.cell(r, c), &mut cur);
            }
        }
    }

    // Dividers in default attributes.
    out.push_str("\x1b[0m");
    for &dc in vdivs {
        if dc < cols {
            for r in 0..rows {
                let _ = write!(out, "\x1b[{};{}H\u{2502}", r + 1, dc + 1); // │
            }
        }
    }
    for &dr in hdivs {
        if dr < rows {
            let _ = write!(out, "\x1b[{};1H", dr + 1);
            for _ in 0..cols {
                out.push('\u{2500}'); // ─
            }
        }
    }

    let (cr, cc) = cursor;
    let _ = write!(out, "\x1b[{};{}H", cr + 1, cc + 1);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;

    /// render → re-emulate must reproduce the grid (visible state).
    fn assert_snapshot_round_trip(cols: u16, rows: u16, feed: &[u8]) {
        let mut src = Terminal::new(cols, rows);
        src.feed(feed);
        let snap = render_snapshot(src.grid());
        let mut painted = Terminal::new(cols, rows);
        painted.feed(&snap);
        assert_eq!(painted.grid(), src.grid(), "snapshot must reproduce the grid");
    }

    #[test]
    fn snapshot_plain_text() {
        assert_snapshot_round_trip(20, 4, b"hello\r\nworld");
    }

    #[test]
    fn snapshot_colours_and_attrs() {
        assert_snapshot_round_trip(
            20,
            3,
            b"\x1b[1;31mbold red\x1b[0m \x1b[38;5;200m256\x1b[0m \x1b[38;2;1;2;3mrgb",
        );
    }

    #[test]
    fn snapshot_cursor_position() {
        let mut src = Terminal::new(20, 5);
        src.feed(b"\x1b[3;7Hx");
        let snap = render_snapshot(src.grid());
        let mut painted = Terminal::new(20, 5);
        painted.feed(&snap);
        assert_eq!(painted.grid().cursor(), src.grid().cursor());
    }

    #[test]
    fn snapshot_after_scroll() {
        assert_snapshot_round_trip(8, 2, b"l1\r\nl2\r\nl3\r\nl4");
    }

    #[test]
    fn composite_places_panes_side_by_side() {
        // Two 3-col panes split vertically in an 7-col window (divider col 3).
        let mut left = Terminal::new(3, 2);
        left.feed(b"L0\r\nL1");
        let mut right = Terminal::new(3, 2);
        right.feed(b"R0\r\nR1");
        let panes = [
            PaneView { top: 0, left: 0, grid: left.grid() },
            PaneView { top: 0, left: 4, grid: right.grid() },
        ];
        let bytes = render_composite(7, 2, &panes, &[3], &[], (0, 0));
        let mut win = Terminal::new(7, 2);
        win.feed(&bytes);
        // row 0: "L0 │ R0", row 1: "L1 │ R1"
        assert_eq!(win.grid().cell(0, 0).c, 'L');
        assert_eq!(win.grid().cell(0, 1).c, '0');
        assert_eq!(win.grid().cell(0, 3).c, '\u{2502}'); // divider
        assert_eq!(win.grid().cell(0, 4).c, 'R');
        assert_eq!(win.grid().cell(1, 4).c, 'R');
        assert_eq!(win.grid().cell(1, 5).c, '1');
    }

    #[test]
    fn scrollback_pages_into_history() {
        // 2-row screen; feed 4 lines → 2 scrolled into history.
        let mut t = Terminal::new(8, 2);
        t.feed(b"l1\r\nl2\r\nl3\r\nl4");
        assert_eq!(t.grid().history().len(), 2); // l1, l2 scrolled off

        // offset 0 = live screen (l3, l4).
        let mut live = Terminal::new(8, 2);
        live.feed(&render_scrollback(t.grid(), 0));
        assert_eq!(live.rows_text(), vec!["l3".to_string(), "l4".to_string()]);

        // offset 2 = paged all the way up (l1, l2).
        let mut up = Terminal::new(8, 2);
        up.feed(&render_scrollback(t.grid(), 2));
        assert_eq!(up.rows_text(), vec!["l1".to_string(), "l2".to_string()]);

        // offset 1 = one line up (l2, l3).
        let mut mid = Terminal::new(8, 2);
        mid.feed(&render_scrollback(t.grid(), 1));
        assert_eq!(mid.rows_text(), vec!["l2".to_string(), "l3".to_string()]);
    }
}
