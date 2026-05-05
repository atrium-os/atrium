//! atrium-term-socket renderer on the envelope+texture-op stack.
//!
//! Per frame: cursor RECT + one TEXTURE per visible non-blank cell.
//! Frame shrink-handling (clearing cells that became blank, scrolled
//! off, etc.) is delegated to `fresco_client::FrameBuilder`.

use crate::glyph_cache::GlyphCache;
use crate::grid::Grid;

use fresco_client::Connection;
use fresco_protocol::{RectParams, TextureParams};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    pad_x: f32,
    pad_y: f32,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache) -> Self {
        Self { cache, pad_x: 8.0, pad_y: 8.0 }
    }

    pub fn render(&self, conn: &mut Connection, grid: &Grid) -> std::io::Result<()> {
        let line_h   = self.cache.line_height;
        let cell_w   = self.cache.cell_w;
        let baseline = self.cache.baseline;

        let mut f = conn.frame()?;

        let cur_x = self.pad_x + (grid.cursor_col as f32) * cell_w;
        let cur_y = self.pad_y + (grid.cursor_row as f32) * line_h;
        f.rect(RectParams {
            x: cur_x, y: cur_y, w: cell_w, h: line_h,
            r: 1.0, g: 0.80, b: 0.20, a: 0.5,
        })?;

        let cells = grid.cells();
        for row in 0..grid.rows {
            let row_top = self.pad_y + (row as f32) * line_h;
            for col in 0..grid.cols {
                let cell = cells[row as usize * grid.cols as usize + col as usize];
                let ch = cell.ch;
                if ch == ' ' || !ch.is_ascii_graphic() { continue; }
                let g = match self.cache.lookup(ch) {
                    Some(g) => *g,
                    None    => continue,
                };
                let cx = self.pad_x + (col as f32) * cell_w;
                let cy = row_top;
                let px = cx + g.metrics.bearing_x as f32;
                let py = cy + baseline - g.metrics.bearing_y as f32;
                let w  = g.metrics.width  as f32;
                let h  = g.metrics.height as f32;
                f.texture(TextureParams { x: px, y: py, w, h, slot_id: g.slot_id })?;
            }
        }

        f.finish()
    }
}
