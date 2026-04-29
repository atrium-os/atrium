//! atrium-edit-socket renderer — emits per-glyph textured rects in
//! screen-pixel coords. No camera projection, no per-vertex UVs.
//! Each visible glyph becomes one RenderItem referencing its
//! pre-uploaded texture from `GlyphCache`.

use crate::buffer::Buffer;
use crate::glyph_cache::GlyphCache;

use fresco_server::command::protocol::{Hash256, NULL_HASH};
use fresco_socket::{wire, Connection};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    /// Static unit-rect mesh, uploaded once at startup.
    rect_mesh: Hash256,
    /// Cached solid-color material for the cursor.
    cursor_mat: Hash256,
    /// Margin from window edge for the text body, in pixels.
    pad_x: f32,
    pad_y: f32,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache, conn: &mut Connection) -> std::io::Result<Self> {
        let v = conn.upload_blob(&wire::vertex_data_xy(&[
            (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
        ]))?;
        let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
        let rect_mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;
        let cursor_mat = conn.upload_blob(&wire::solid_material([0xff, 0xcc, 0x33, 0xff]))?;
        Ok(Self { cache, rect_mesh, cursor_mat, pad_x: 16.0, pad_y: 16.0 })
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

        let mut nodes: Vec<Hash256> = Vec::new();

        // Cursor block — render first so glyph alpha shows on top.
        // Position: relative to the cursor's row/col in the buffer.
        if cursor_visible {
            let cursor_row = buf.cursor_line as i64 - buf.scroll_top as i64;
            if cursor_row >= 0 && (cursor_row as usize) < viewport_rows {
                let cx = self.pad_x + (buf.cursor_col as f32) * cell_w;
                let cy = self.pad_y + (cursor_row as f32) * line_h;
                let xform = wire::affine_2d(cell_w, line_h, cx, cy);
                let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
                let r  = conn.upload_blob(&wire::renderable(self.rect_mesh, self.cursor_mat))?;
                let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
                nodes.push(sn);
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
                    Some(g) => g,
                    None    => { col += 1; continue; }
                };

                let cx = self.pad_x + (col as f32) * cell_w;
                let cy = row_top;
                let px = cx + g.metrics.bearing_x as f32;
                let py = cy + baseline - g.metrics.bearing_y as f32;
                let w  = g.metrics.width  as f32;
                let h  = g.metrics.height as f32;

                let xform = wire::affine_2d(w, h, px, py);
                let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
                let r  = conn.upload_blob(&wire::renderable(self.rect_mesh, g.material))?;
                let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
                nodes.push(sn);
                col += 1;
            }
        }

        // Status line on the bottom row.
        let status = format!(
            "{}{}",
            if buf.modified { "[*] " } else { "" },
            if buf.status.is_empty() { format!("{:?}", buf.path) } else { buf.status.clone() },
        );
        let status_row = viewport_rows;
        let row_top = self.pad_y + (status_row as f32) * line_h;
        let mut col = 0usize;
        for ch in status.chars() {
            if ch == ' ' { col += 1; continue; }
            if !ch.is_ascii_graphic() { continue; }
            let g = match self.cache.lookup(ch) {
                Some(g) => g,
                None    => { col += 1; continue; }
            };
            let cx = self.pad_x + (col as f32) * cell_w;
            let cy = row_top;
            let px = cx + g.metrics.bearing_x as f32;
            let py = cy + baseline - g.metrics.bearing_y as f32;
            let w  = g.metrics.width  as f32;
            let h  = g.metrics.height as f32;
            let xform = wire::affine_2d(w, h, px, py);
            let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
            let r  = conn.upload_blob(&wire::renderable(self.rect_mesh, g.material))?;
            let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
            nodes.push(sn);
            col += 1;
        }

        if nodes.is_empty() {
            // Empty: clear the scene to a NULL root so the compositor
            // reverts to the in-process clock fallback.
            conn.set_root(NULL_HASH)?;
            return Ok(());
        }

        let nl = conn.upload_blob(&wire::node_list(&nodes))?;
        let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
        conn.set_root(sr)?;
        Ok(())
    }
}
