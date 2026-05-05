//! atrium-edit-socket renderer on the envelope+texture-op stack.
//!
//! Each visible glyph is one TextureParams node; the cursor is one
//! RectParams node (axis-aligned, no rotation needed). Frame
//! shrink-handling (clearing nodes that disappeared since last frame)
//! is delegated to `fresco_client::FrameBuilder`.

use crate::buffer::Buffer;
use crate::glyph_cache::GlyphCache;

use fresco_client::{Connection, FrameBuilder};
use fresco_protocol::{RectParams, TextureParams};

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
                emit_glyph(&mut f, &g, self.pad_x, row_top, col, baseline, cell_w)?;
                col += 1;
            }
        }

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
            emit_glyph(&mut f, &g, self.pad_x, row_top, col, baseline, cell_w)?;
            col += 1;
        }

        f.finish()
    }
}

fn emit_glyph(
    f:        &mut FrameBuilder,
    g:        &crate::glyph_cache::CachedGlyph,
    pad_x:    f32,
    row_top:  f32,
    col:      usize,
    baseline: f32,
    cell_w:   f32,
) -> std::io::Result<()> {
    let cx = pad_x + (col as f32) * cell_w;
    let px = cx + g.metrics.bearing_x as f32;
    let py = row_top + baseline - g.metrics.bearing_y as f32;
    let w  = g.metrics.width  as f32;
    let h  = g.metrics.height as f32;
    f.texture(TextureParams { x: px, y: py, w, h, slot_id: g.slot_id })?;
    Ok(())
}
