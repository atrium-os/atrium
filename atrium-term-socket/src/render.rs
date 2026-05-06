//! atrium-term-socket renderer on the M6.3 server-side text path.
//!
//! Per frame: cursor RECT + one OP_TEXT_RUN_INSTALL per visible non-
//! empty grid row. The grid's contiguous run of cells in each row
//! becomes one shaped run on the server side; trailing blanks are
//! trimmed before sending to keep the run short.

use crate::grid::Grid;

use fresco_client::Connection;
use fresco_protocol::RectParams;

pub struct Renderer {
    font_id:  u32,
    size_px:  f32,
    cell_w:   f32,
    line_h:   f32,
    baseline: f32,
    pad_x:    f32,
    pad_y:    f32,
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32) -> Self {
        Self { font_id, size_px, cell_w, line_h, baseline,
               pad_x: 8.0, pad_y: 8.0 }
    }

    pub fn render(&self, conn: &mut Connection, grid: &Grid) -> std::io::Result<()> {
        let mut f = conn.frame()?;

        let cur_x = self.pad_x + (grid.cursor_col as f32) * self.cell_w;
        let cur_y = self.pad_y + (grid.cursor_row as f32) * self.line_h;
        f.rect(RectParams {
            x: cur_x, y: cur_y, w: self.cell_w, h: self.line_h,
            r: 1.0, g: 0.80, b: 0.20, a: 0.5,
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
            let y_base = self.pad_y + (row as f32) * self.line_h + self.baseline;
            f.text_run(self.font_id, self.size_px,
                       self.pad_x, y_base,
                       [0.95, 0.95, 0.95, 1.0],
                       trimmed)?;
        }

        f.finish()
    }
}
