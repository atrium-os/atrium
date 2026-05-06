//! atrium-edit-socket renderer on the M6.1 atrium-text bundle.
//!
//! Per frame: one RECT for the cursor (if visible) plus one GLYPH_RUN
//! node carrying every printable glyph in the visible page + status
//! line as `GlyphInstance` entries. Replaces the per-glyph TEXTURE
//! pattern.

use crate::buffer::Buffer;
use crate::glyph_cache::GlyphCache;

use fresco_client::Connection;
use fresco_protocol::{GlyphInstance, GlyphRunParams, RectParams};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    pad_x: f32,
    pad_y: f32,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache) -> Self {
        Self { cache, pad_x: 16.0, pad_y: 16.0 }
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

        let mut f = conn.frame()?;

        if cursor_visible {
            let cursor_row = buf.cursor_line as i64 - buf.scroll_top as i64;
            if cursor_row >= 0 && (cursor_row as usize) < viewport_rows {
                let cx = self.pad_x + (buf.cursor_col as f32) * cell_w;
                let cy = self.pad_y + (cursor_row as f32) * line_h;
                f.rect(RectParams {
                    x: cx, y: cy, w: cell_w, h: line_h,
                    r: 1.0, g: 0.80, b: 0.20, a: 1.0,
                })?;
            }
        }

        let mut instances: Vec<GlyphInstance> = Vec::new();

        let last = (buf.scroll_top + viewport_rows).min(buf.lines.len());
        for (row_in_viewport, line_idx) in (buf.scroll_top..last).enumerate() {
            let line = &buf.lines[line_idx];
            let mut col = 0usize;
            for ch in line.chars() {
                if ch == '\t' { col = ((col / 8) + 1) * 8; continue; }
                if ch == ' '  { col += 1; continue; }
                if !ch.is_ascii_graphic() { continue; }
                if let Some(m) = self.cache.lookup(ch) {
                    instances.push(GlyphCache::instance(
                        m, col, row_in_viewport, cell_w, line_h));
                }
                col += 1;
            }
        }

        let status = format!(
            "{}{}",
            if buf.modified { "[*] " } else { "" },
            if buf.status.is_empty() { format!("{:?}", buf.path) } else { buf.status.clone() },
        );
        let mut col = 0usize;
        for ch in status.chars() {
            if ch == ' ' { col += 1; continue; }
            if !ch.is_ascii_graphic() { continue; }
            if let Some(m) = self.cache.lookup(ch) {
                instances.push(GlyphCache::instance(
                    m, col, viewport_rows, cell_w, line_h));
            }
            col += 1;
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
