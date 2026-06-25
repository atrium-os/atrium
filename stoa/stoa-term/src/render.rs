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

use crate::{flags, Color, Grid};

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
            let cell = grid.cell(r, c);
            let want = (cell.fg, cell.bg, cell.flags);
            let is_default = want == (Color::Default, Color::Default, 0);
            let need = match cur {
                Some(p) => p != want,
                None => !is_default,
            };
            if need {
                out.push_str(&sgr_for(cell.fg, cell.bg, cell.flags));
                cur = Some(want);
            }
            out.push(cell.c);
        }
    }

    out.push_str("\x1b[0m");
    let (cr, cc) = grid.cursor();
    let _ = write!(out, "\x1b[{};{}H", cr + 1, cc + 1);
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
}
