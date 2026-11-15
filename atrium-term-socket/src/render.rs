//! atrium-term-socket renderer on the M6.3 server-side text path.
//!
//! Per frame: cursor RECT + one OP_TEXT_RUN_INSTALL per visible non-
//! empty grid row. The grid's contiguous run of cells in each row
//! becomes one shaped run on the server side; trailing blanks are
//! trimmed before sending to keep the run short.

use stoa_term::Grid;

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
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32) -> Self {
        Self { font_id, size_px, cell_w, line_h, baseline,
               pad_x: 8.0, pad_y: 8.0, top: 0.0, k: 1.0 }
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

        let (cur_row, cur_col) = grid.cursor();
        let cur_x = self.pad_x + (cur_col as f32) * self.cell_w;
        let cur_y = top + self.pad_y + (cur_row as f32) * self.line_h;
        let (x0, y0) = ((cur_x * k).round(), (cur_y * k).round());
        f.rect(RectParams {
            x: x0, y: y0,
            w: ((cur_x + self.cell_w) * k).round() - x0, h: ((cur_y + self.line_h) * k).round() - y0,
            r: 1.0, g: 0.80, b: 0.20, a: 0.5, radius: 0.0,
        })?;

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
            /* Build the row string, trimming trailing spaces. */
            let s: String = grid.row(row).iter().map(|c| if c.c == '\0' { ' ' } else { c.c }).collect();
            let trimmed = s.trim_end_matches(' ');
            if trimmed.is_empty() { continue; }
            let y_base = ((top + self.pad_y + (row as f32) * self.line_h + self.baseline) * k).round();
            f.text_run(self.font_id, self.size_px * k,
                       (self.pad_x * k).round(), y_base - self.baseline * k,
                       [0.95, 0.95, 0.95, 1.0],
                       trimmed)?;
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
