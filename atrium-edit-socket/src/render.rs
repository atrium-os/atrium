//! atrium-edit-socket renderer on the envelope+texture-op stack.
//!
//! Each visible glyph is one `OP_SCENE_NODE_SET(TextureParams)` with
//! the glyph's pre-bound slot. The cursor is one `OP_SCENE_NODE_SET(
//! RectParams)` rendered before glyphs (insertion order in the
//! per-window state has no meaning to the renderer, but conceptually
//! the cursor sits behind glyphs and the texture-on-rect blend just
//! works).
//!
//! Per-frame node-id scheme: cursor at id 1, glyphs at sequential ids
//! starting at 2. Each render assigns 1..=N for the N visible nodes;
//! ids beyond the previous frame's high-water mark get
//! `SCENE_NODE_CLEAR` to drop stale entries from the server's
//! per-window scene state. (No "wipe scene" op exists — the closest
//! analogue is per-id clear.)

use crate::buffer::Buffer;
use crate::glyph_cache::GlyphCache;

use fresco_client::Connection;
use fresco_protocol::{RectParams, TextureParams};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    pad_x: f32,
    pad_y: f32,
    /// High-water mark of node ids emitted in the previous frame.
    /// Used to compute the SCENE_NODE_CLEAR set when this frame emits
    /// fewer nodes (e.g. backspace shortened a line, or scroll
    /// removed text from the viewport).
    last_max_id: std::cell::Cell<u32>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache) -> Self {
        Self {
            cache,
            pad_x: 16.0,
            pad_y: 16.0,
            last_max_id: std::cell::Cell::new(0),
        }
    }

    pub fn render(
        &self,
        conn: &mut Connection,
        buf: &Buffer,
        viewport_rows: usize,
        cursor_visible: bool,
    ) -> std::io::Result<()> {
        let line_h   = self.cache.line_height;
        let cell_w   = self.cache.cell_w;
        let baseline = self.cache.baseline;

        conn.scene_frame_begin()?;
        let mut next_id: u32 = 1;

        /* Cursor block. Always assigned id 1. If cursor is invisible
         * or off-screen, skip — id 1 may then be reused by the first
         * glyph. */
        if cursor_visible {
            let cursor_row = buf.cursor_line as i64 - buf.scroll_top as i64;
            if cursor_row >= 0 && (cursor_row as usize) < viewport_rows {
                let cx = self.pad_x + (buf.cursor_col as f32) * cell_w;
                let cy = self.pad_y + (cursor_row as f32) * line_h;
                conn.scene_node_rect(next_id, RectParams {
                    x: cx, y: cy, w: cell_w, h: line_h,
                    r: 1.0, g: 0.80, b: 0.20, a: 1.0,
                })?;
                next_id += 1;
            }
        }

        /* Visible text rows. */
        let last = (buf.scroll_top + viewport_rows).min(buf.lines.len());
        for (row_in_viewport, line_idx) in (buf.scroll_top..last).enumerate() {
            let line = &buf.lines[line_idx];
            let row_top = self.pad_y + (row_in_viewport as f32) * line_h;
            let mut col = 0usize;
            for ch in line.chars() {
                if ch == '\t' { col = ((col / 8) + 1) * 8; continue; }
                if ch == ' '  { col += 1; continue; }
                if !ch.is_ascii_graphic() { continue; }
                let g = match self.cache.lookup(ch) {
                    Some(g) => *g,
                    None    => { col += 1; continue; }
                };
                self.emit_glyph(conn, &mut next_id, &g, row_top, col, baseline, cell_w)?;
                col += 1;
            }
        }

        /* Status line on the row below the viewport. */
        let status = format!(
            "{}{}",
            if buf.modified { "[*] " } else { "" },
            if buf.status.is_empty() { format!("{:?}", buf.path) } else { buf.status.clone() },
        );
        let row_top = self.pad_y + (viewport_rows as f32) * line_h;
        let mut col = 0usize;
        for ch in status.chars() {
            if ch == ' ' { col += 1; continue; }
            if !ch.is_ascii_graphic() { continue; }
            let g = match self.cache.lookup(ch) {
                Some(g) => *g,
                None    => { col += 1; continue; }
            };
            self.emit_glyph(conn, &mut next_id, &g, row_top, col, baseline, cell_w)?;
            col += 1;
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

    fn emit_glyph(
        &self,
        conn: &mut Connection,
        next_id: &mut u32,
        g: &crate::glyph_cache::CachedGlyph,
        row_top: f32, col: usize,
        baseline: f32, cell_w: f32,
    ) -> std::io::Result<()> {
        let cx = self.pad_x + (col as f32) * cell_w;
        let px = cx + g.metrics.bearing_x as f32;
        let py = row_top + baseline - g.metrics.bearing_y as f32;
        let w  = g.metrics.width  as f32;
        let h  = g.metrics.height as f32;
        conn.scene_node_texture(*next_id, TextureParams {
            x: px, y: py, w, h, slot_id: g.slot_id,
        })?;
        *next_id += 1;
        Ok(())
    }
}
