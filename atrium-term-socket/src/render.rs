//! atrium-term-socket renderer on the M6.3 server-side text path.
//!
//! Per frame: cursor RECT + one OP_TEXT_RUN_INSTALL per visible non-
//! empty grid row. The grid's contiguous run of cells in each row
//! becomes one shaped run on the server side; trailing blanks are
//! trimmed before sending to keep the run short.

use crate::grid::Grid;

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
    /// Device px per pt.
    pub k:    f32,
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32) -> Self {
        Self { font_id, size_px, cell_w, line_h, baseline,
               pad_x: 8.0, pad_y: 8.0, k: 1.0 }
    }

    /// How many cells fit a `w` x `h` pt window.
    pub fn grid_for(&self, w: f32, h: f32) -> (u16, u16) {
        let cols = ((w - 2.0 * self.pad_x) / self.cell_w).floor().clamp(1.0, 500.0) as u16;
        let rows = ((h - 2.0 * self.pad_y) / self.line_h).floor().clamp(1.0, 300.0) as u16;
        (cols, rows)
    }

    pub fn render(&self, conn: &mut Connection, grid: &Grid) -> std::io::Result<()> {
        let mut f = conn.frame()?;

        let cur_x = self.pad_x + (grid.cursor_col as f32) * self.cell_w;
        let cur_y = self.pad_y + (grid.cursor_row as f32) * self.line_h;
        let k = self.k;
        let (x0, y0) = ((cur_x * k).round(), (cur_y * k).round());
        f.rect(RectParams {
            x: x0, y: y0,
            w: ((cur_x + self.cell_w) * k).round() - x0, h: ((cur_y + self.line_h) * k).round() - y0,
            r: 1.0, g: 0.80, b: 0.20, a: 0.5, radius: 0.0,
        })?;

        let cells = grid.cells();
        for row in 0..grid.rows {
            /* Build the row string, trimming trailing spaces. */
            let mut s = String::with_capacity(grid.cols as usize);
            for col in 0..grid.cols {
                let cell = cells[row as usize * grid.cols as usize + col as usize];
                s.push(cell.ch);
            }
            let trimmed = s.trim_end_matches(' ');
            if trimmed.is_empty() { continue; }
            let y_base = ((self.pad_y + (row as f32) * self.line_h + self.baseline) * k).round();
            f.text_run(self.font_id, self.size_px * k,
                       (self.pad_x * k).round(), y_base - self.baseline * k,
                       [0.95, 0.95, 0.95, 1.0],
                       trimmed)?;
        }

        f.finish()
    }
}
