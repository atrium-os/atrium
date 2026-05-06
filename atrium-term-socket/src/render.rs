//! atrium-term-socket renderer on the M6.1 atrium-text bundle.
//!
//! Per frame: cursor RECT + one GLYPH_RUN node carrying every visible
//! non-blank cell as a `GlyphInstance`.

use crate::glyph_cache::GlyphCache;
use crate::grid::Grid;

use fresco_client::Connection;
use fresco_protocol::{GlyphInstance, GlyphRunParams, RectParams};

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

        let mut instances: Vec<GlyphInstance> = Vec::new();
        let cells = grid.cells();
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let cell = cells[row as usize * grid.cols as usize + col as usize];
                let ch = cell.ch;
                if ch == ' ' || !ch.is_ascii_graphic() { continue; }
                if let Some(m) = self.cache.lookup(ch) {
                    instances.push(GlyphCache::instance(
                        m, col as usize, row as usize, cell_w, line_h));
                }
            }
        }

        if !instances.is_empty() {
            f.glyph_run(GlyphRunParams {
                x: self.pad_x,
                y: self.pad_y + baseline,
                atlas_slot_id: self.cache.atlas_slot,
                atlas_width:   self.cache.atlas_width,
                atlas_height:  self.cache.atlas_height,
                r: 0.95, g: 0.95, b: 0.95, a: 1.0,
                glyphs: instances,
            })?;
        }

        f.finish()
    }
}
