//! atrium-term-socket renderer on the M6.3 server-side text path.
//!
//! Per frame: cursor RECT + one OP_TEXT_RUN_INSTALL per visible non-
//! empty grid row. The grid's contiguous run of cells in each row
//! becomes one shaped run on the server side; trailing blanks are
//! trimmed before sending to keep the run short.

use stoa_term::{flags, mode, Cell, Color, Grid};

use fresco_client::Connection;
use fresco_protocol::RectParams;

/// Lays out in points and draws at the window's scale `k` (device px per
/// pt; resolution-independence.md §3): positions and the glyph size are
/// multiplied by it as they are sent, baselines on whole pixels.
pub struct Renderer {
    font_id:  u32,
    size_px:  f32,
    cell_w:   f32,
    line_h:   f32,
    /// The ascent: a text run's y on the wire is the top of its em box
    /// (fresco-protocol), one ascent above the baseline.
    baseline: f32,
    pad_x:    f32,
    pad_y:    f32,
    /// The strip above the grid that names where the shell is (W7).
    pub top:  f32,
    /// Device px per pt.
    pub k:    f32,
    /// The cell the pointer is over (for motion reports and the wheel).
    pub pointer_cell: Option<(u16, u16)>,
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32) -> Self {
        Self { font_id, size_px, cell_w, line_h, baseline,
               pad_x: 8.0, pad_y: 8.0, top: 0.0, k: 1.0, pointer_cell: None }
    }

    /// The cell under (`x`, `y`) points, clamped to the grid.
    pub fn cell_at(&self, x: f32, y: f32, grid: &Grid) -> (u16, u16) {
        let col = ((x - self.pad_x) / self.cell_w).floor().max(0.0) as u32;
        let row = ((y - self.top - self.pad_y) / self.line_h).floor().max(0.0) as u32;
        (row.min(grid.rows() as u32 - 1) as u16, col.min(grid.cols() as u32 - 1) as u16)
    }

    /// How many cells fit a `w` x `h` pt window.
    pub fn grid_for(&self, w: f32, h: f32) -> (u16, u16) {
        let cols = ((w - 2.0 * self.pad_x) / self.cell_w).floor().clamp(1.0, 500.0) as u16;
        let rows = ((h - self.top - 2.0 * self.pad_y) / self.line_h).floor().clamp(1.0, 300.0) as u16;
        (cols, rows)
    }

    /// The menu's rows: where each is, in points (x, y, w, h).
    pub fn menu_row(&self, i: usize) -> (f32, f32, f32, f32) {
        (self.pad_x, self.top + 4.0 + i as f32 * MENU_ROW, MENU_W, MENU_ROW)
    }

    pub fn render(&self, conn: &mut Connection, grid: &Grid, strip: &Strip) -> std::io::Result<()> {
        let mut f = conn.frame()?;
        let k = self.k;
        let (rows, cols) = (grid.rows(), grid.cols());
        let top = self.top;
        // The window's own background, so it is never see-through.
        f.rect(RectParams {
            x: 0.0, y: 0.0,
            w: ((2.0 * self.pad_x + cols as f32 * self.cell_w) * k).ceil(),
            h: ((top + 2.0 * self.pad_y + rows as f32 * self.line_h) * k).ceil(),
            r: 0.07, g: 0.08, b: 0.09, a: 1.0, radius: 0.0,
        })?;
        if top > 0.0 {
            f.rect(RectParams {
                x: 0.0, y: 0.0,
                w: ((2.0 * self.pad_x + cols as f32 * self.cell_w) * k).ceil(), h: (top * k).round(),
                r: 0.13, g: 0.15, b: 0.17, a: 1.0, radius: 0.0,
            })?;
            let sz = 13.0;
            f.text_run(self.font_id, sz * k, (self.pad_x * k).round(), ((top - sz) / 2.0 * k).round(),
                       [0.95, 0.85, 0.55, 1.0], &format!("{}  [change]", strip.place))?;
            let hint_x = self.pad_x + (strip.place.chars().count() + 14) as f32 * sz * 0.6;
            f.text_run(self.font_id, sz * k, (hint_x * k).round(), ((top - sz) / 2.0 * k).round(),
                       [0.55, 0.58, 0.62, 1.0], &strip.hint)?;
        }

        // Cells: background fills, then text in runs of one style, then
        // underline and strike lines, then the cursor over them.
        // Text draws over fills: rows under the open switcher are left out.
        let hidden = strip.menu.as_ref().map(|(items, _)| {
            let (_, y, _, _) = self.menu_row(0);
            (y - 4.0, y + items.len() as f32 * MENU_ROW + 4.0)
        });
        for row in 0..rows {
            let ry = top + self.pad_y + row as f32 * self.line_h;
            if hidden.is_some_and(|(a, b)| ry + self.line_h > a && ry < b) {
                continue;
            }
            for seg in segments(grid.row(row)) {
                let x = self.pad_x + seg.col as f32 * self.cell_w;
                let w = seg.cells as f32 * self.cell_w;
                if let Some(bg) = seg.style.bg {
                    let (x0, y0) = ((x * k).round(), (ry * k).round());
                    f.rect(RectParams {
                        x: x0, y: y0,
                        w: ((x + w) * k).round() - x0, h: ((ry + self.line_h) * k).round() - y0,
                        r: bg[0], g: bg[1], b: bg[2], a: 1.0, radius: 0.0,
                    })?;
                }
                let text = seg.text.trim_end_matches(' ');
                let lead = text.len() - text.trim_start_matches(' ').len();
                let text = &text[lead..];
                if !text.is_empty() {
                    let y_base = ((ry + self.baseline) * k).round();
                    f.text_run_weight(self.font_id, self.size_px * k,
                                      ((x + lead as f32 * self.cell_w) * k).round(), y_base - self.baseline * k,
                                      seg.style.fg, text, if seg.style.bold { 700 } else { 400 })?;
                }
                let line = |f: &mut fresco_client::FrameBuilder<'_>, at: f32| f.rect(RectParams {
                    x: (x * k).round(), y: ((ry + at) * k).round(),
                    w: (w * k).round(), h: k.max(1.0).round(),
                    r: seg.style.fg[0], g: seg.style.fg[1], b: seg.style.fg[2], a: seg.style.fg[3], radius: 0.0,
                });
                if seg.style.underline {
                    line(&mut f, self.baseline + 2.0)?;
                }
                if seg.style.strike {
                    line(&mut f, self.baseline * 0.65)?;
                }
            }
        }

        if !grid.mode(mode::HIDE_CURSOR) && strip.menu.is_none() {
            let (cur_row, cur_col) = grid.cursor();
            let wide = grid.cell(cur_row, cur_col).flags & flags::WIDE != 0;
            let cur_x = self.pad_x + (cur_col as f32) * self.cell_w;
            let cur_y = top + self.pad_y + (cur_row as f32) * self.line_h;
            let cw = if wide { 2.0 * self.cell_w } else { self.cell_w };
            let bar = (2.0 * k).round().max(1.0);
            let (x0, y0) = ((cur_x * k).round(), (cur_y * k).round());
            let (x1, y1) = (((cur_x + cw) * k).round(), ((cur_y + self.line_h) * k).round());
            let (x, y, w, h) = match mode::cursor_shape(grid.modes()) {
                3 | 4 => (x0, y1 - bar, x1 - x0, bar),   // underline
                5 | 6 => (x0, y0, bar, y1 - y0),         // bar
                _ => (x0, y0, x1 - x0, y1 - y0),         // block
            };
            f.rect(RectParams { x, y, w, h, r: 1.0, g: 0.80, b: 0.20, a: 0.5, radius: 0.0 })?;
        }

        // The switcher, over the grid.
        if let Some((items, sel)) = &strip.menu {
            let (x, y, w, _) = self.menu_row(0);
            f.rect(RectParams {
                x: (x * k).round(), y: ((y - 4.0) * k).round(), w: (w * k).round(), h: ((items.len() as f32 * MENU_ROW + 8.0) * k).round(),
                r: 0.18, g: 0.20, b: 0.23, a: 1.0, radius: 6.0 * k,
            })?;
            for (i, label) in items.iter().enumerate() {
                let (x, y, w, h) = self.menu_row(i);
                if i == *sel {
                    f.rect(RectParams { x: (x * k).round(), y: (y * k).round(), w: (w * k).round(), h: (h * k).round(),
                                        r: 0.30, g: 0.36, b: 0.42, a: 1.0, radius: 4.0 * k })?;
                }
                f.text_run(self.font_id, 14.0 * k, ((x + 12.0) * k).round(), ((y + (h - 14.0) / 2.0) * k).round(),
                           [0.95, 0.95, 0.95, 1.0], label)?;
            }
        }

        f.finish()
    }
}

/// The strip's height, in points.
pub const STRIP_H: f32 = 30.0;
const MENU_ROW: f32 = 30.0;
const MENU_W: f32 = 300.0;

/// What the strip shows: where the shell is, a hint, and the switcher
/// when it is open (its rows and the chosen one).
#[derive(Default)]
pub struct Strip {
    pub place: String,
    pub hint:  String,
    pub menu:  Option<(Vec<String>, usize)>,
}

/// The default colours, as the background fill above draws them.
pub const DEFAULT_FG: [f32; 4] = [0.95, 0.95, 0.95, 1.0];
pub const DEFAULT_BG: [f32; 3] = [0.07, 0.08, 0.09];

/// How a run of cells is drawn: resolved colours (`bg` None = the window's
/// own) and the lines.
#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: [f32; 4],
    bg: Option<[f32; 3]>,
    bold: bool,
    underline: bool,
    strike: bool,
}

/// A run of cells with one style, starting at `col`, `cells` wide.
struct Segment {
    col: u16,
    cells: u16,
    text: String,
    style: Style,
}

fn style_of(c: &Cell) -> Style {
    let mut fg = match c.fg {
        Color::Default => DEFAULT_FG,
        other => rgb(other, c.flags & flags::BOLD != 0),
    };
    let mut bg = match c.bg {
        Color::Default => None,
        other => { let v = rgb(other, false); Some([v[0], v[1], v[2]]) }
    };
    if c.flags & flags::REVERSE != 0 {
        let b = bg.unwrap_or(DEFAULT_BG);
        bg = Some([fg[0], fg[1], fg[2]]);
        fg = [b[0], b[1], b[2], 1.0];
    }
    if c.flags & flags::DIM != 0 {
        fg[3] = 0.6;
    }
    if c.flags & flags::HIDDEN != 0 {
        fg[3] = 0.0;
    }
    Style {
        fg, bg,
        bold: c.flags & flags::BOLD != 0,
        underline: c.flags & flags::UNDERLINE != 0,
        strike: c.flags & flags::STRIKE != 0,
    }
}

/// Split a row into runs of one style. ASCII text shares a run (the font
/// is monospaced); any other character gets a run of its own, placed on
/// its cell, since a fallback font's glyphs need not have the cell's
/// advance.
fn segments(row: &[Cell]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut col = 0usize;
    while col < row.len() {
        let c = &row[col];
        if c.flags & flags::WIDE_SPACER != 0 {
            col += 1; // drawn with its left half
            continue;
        }
        let style = style_of(c);
        let width = if c.flags & flags::WIDE != 0 { 2 } else { 1 };
        let ch = if c.flags & flags::HIDDEN != 0 || c.c == '\0' { ' ' } else { c.c };
        let ascii = ch.is_ascii();
        match out.last_mut() {
            Some(s) if ascii && s.style == style && s.text.is_ascii()
                && (s.col + s.cells) as usize == col => {
                s.text.push(ch);
                s.cells += 1;
            }
            // A non-ASCII character starts a run that nothing joins.
            _ => out.push(Segment { col: col as u16, cells: width, text: ch.to_string(), style }),
        }
        col += width as usize;
    }
    // Drop runs with nothing to draw.
    out.retain(|s| s.style.bg.is_some() || s.style.underline || s.style.strike || !s.text.trim().is_empty());
    out
}

/// The xterm palette: 16 system colours, the 6x6x6 cube, 24 greys. Bold
/// brightens the first eight.
fn rgb(c: Color, bold: bool) -> [f32; 4] {
    const SYS: [(u8, u8, u8); 16] = [
        (0x1d, 0x1f, 0x21), (0xcc, 0x44, 0x44), (0x6a, 0xb0, 0x4c), (0xd7, 0xa6, 0x3c),
        (0x4f, 0x8f, 0xd6), (0xb2, 0x6c, 0xc9), (0x3f, 0xb3, 0xb3), (0xc8, 0xc8, 0xc8),
        (0x6c, 0x70, 0x76), (0xf0, 0x6a, 0x6a), (0x92, 0xd4, 0x6f), (0xf2, 0xcc, 0x63),
        (0x7a, 0xb3, 0xf2), (0xd3, 0x93, 0xea), (0x6c, 0xd9, 0xd9), (0xf5, 0xf5, 0xf5),
    ];
    let (r, g, b) = match c {
        Color::Default => return DEFAULT_FG,
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(n) if n < 16 => SYS[if bold && n < 8 { n + 8 } else { n } as usize],
        Color::Indexed(n) if n < 232 => {
            let i = n - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        Color::Indexed(n) => {
            let v = 8 + (n - 232) * 10;
            (v, v, v)
        }
    };
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(bytes: &[u8]) -> Vec<Cell> {
        let mut t = stoa_term::Terminal::new(20, 1);
        t.feed(bytes);
        t.grid().row(0).to_vec()
    }

    #[test]
    fn plain_text_is_one_run() {
        let s = segments(&row(b"hello world"));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].text.trim_end(), "hello world");
    }

    #[test]
    fn styles_split_runs_and_reverse_fills() {
        let s = segments(&row(b"ab\x1b[7mcd\x1b[0mef"));
        assert_eq!(s.len(), 3);
        assert_eq!((s[1].col, s[1].cells), (2, 2));
        assert!(s[1].style.bg.is_some(), "reverse video fills the cells");
        assert_eq!(s[2].col, 4);
    }

    #[test]
    fn non_ascii_is_placed_on_its_cell() {
        let s = segments(&row("a─b漢c".as_bytes()));
        let at: Vec<(u16, &str)> = s.iter().map(|s| (s.col, s.text.trim_end())).collect();
        assert_eq!(at, vec![(0, "a"), (1, "─"), (2, "b"), (3, "漢"), (5, "c")]);
        assert_eq!(s[3].cells, 2);
    }

    #[test]
    fn palette() {
        assert_eq!(rgb(Color::Indexed(16), false), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(rgb(Color::Indexed(231), false), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(rgb(Color::Indexed(1), true), rgb(Color::Indexed(9), false));
    }
}
