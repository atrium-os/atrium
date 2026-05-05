//! atrium-term-socket renderer on the envelope+texture-op stack.
//!
//! Per frame: cursor (RECT) + one TEXTURE per visible non-blank cell.
//! Re-emits the visible cells every frame; ids 1..=N where N varies
//! with how much text is on the screen. `last_max_id` tracks the
//! previous frame's high water mark and emits `OP_SCENE_NODE_CLEAR`
//! for ids that went away (cells that became blank, terminal output
//! that scrolled off, etc.).

use crate::glyph_cache::GlyphCache;
use crate::grid::Grid;

use fresco_client::Connection;
use fresco_protocol::{RectParams, TextureParams};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    pad_x: f32,
    pad_y: f32,
    last_max_id: std::cell::Cell<u32>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache) -> Self {
        Self {
            cache,
            pad_x: 8.0, pad_y: 8.0,
            last_max_id: std::cell::Cell::new(0),
        }
    }

    pub fn render(&self, conn: &mut Connection, grid: &Grid) -> std::io::Result<()> {
        let line_h   = self.cache.line_height;
        let cell_w   = self.cache.cell_w;
        let baseline = self.cache.baseline;

        conn.scene_frame_begin()?;
        let mut next_id: u32 = 1;

        /* Cursor block. */
        let cur_x = self.pad_x + (grid.cursor_col as f32) * cell_w;
        let cur_y = self.pad_y + (grid.cursor_row as f32) * line_h;
        conn.scene_node_rect(next_id, RectParams {
            x: cur_x, y: cur_y, w: cell_w, h: line_h,
            r: 1.0, g: 0.80, b: 0.20, a: 0.5,
        })?;
        next_id += 1;

        /* Cells. */
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
                conn.scene_node_texture(next_id, TextureParams {
                    x: px, y: py, w, h, slot_id: g.slot_id,
                })?;
                next_id += 1;
            }
        }

        let new_max = next_id - 1;
        let old_max = self.last_max_id.get();
        for stale in (new_max + 1)..=old_max {
            conn.scene_node_clear(stale)?;
        }
        self.last_max_id.set(new_max);

        conn.scene_frame_end()?;
        Ok(())
    }
}
