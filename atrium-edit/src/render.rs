//! Editor renderer: text mesh + (placeholder) cursor underline.
//!
//! Phase 1: text only. Cursor visualization is a TODO — once we add
//! a second material (solid color), we'll add a child slot for an
//! underline rectangle at the cursor cell.

use crate::buffer::Buffer;
use crate::glyph_cache::GlyphCache;

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    slot:  u16,
    /// Logical pixel dimensions of the destination window/FBO. Used
    /// to scale text-pixel coordinates into the camera's NDC.
    view_w: u32,
    view_h: u32,
    initialized: bool,
    mat_hash: Option<[u8; 32]>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache, slot: u16, view_w: u32, view_h: u32) -> Self {
        Self { cache, slot, view_w, view_h, initialized: false, mat_hash: None }
    }

    /// Reset the renderer to a new view (window) size. The next
    /// render() call will rebuild geometry against the new dims.
    pub fn set_view_size(&mut self, w: u32, h: u32) {
        self.view_w = w;
        self.view_h = h;
    }

    pub fn render(&mut self, conn: &Connection, buf: &Buffer, viewport_rows: usize) -> std::io::Result<()> {
        if self.mat_hash.is_none() {
            let h = conn.cas_put_texture(self.cache.atlas_w, self.cache.atlas_h, &self.cache.atlas)?;
            let mat = conn.cas_put(&blob::material_textured(&h, 0xffffffff))?;
            self.mat_hash = Some(mat);
        }
        let mat_h = self.mat_hash.unwrap();

        let dw = self.view_w.max(1) as f32;
        let dh = self.view_h.max(1) as f32;

        // Camera (set below): position (0,0,5), fov_y = π/4 → half_vy
        // = 5*tan(π/8) ≈ 2.071. Pixel coords are scaled into the
        // camera's visible NDC extent (not [-1..1]).
        const CAM_DIST: f32 = 5.0;
        const CAM_FOV_Y: f32 = 0.7853982;
        let half_vy = CAM_DIST * (CAM_FOV_Y * 0.5).tan();
        let half_vx = half_vy * dw / dh;
        let s_x = 2.0 * half_vx / dw;
        let s_y = 2.0 * half_vy / dh;

        let cell_w = self.cache.cell_w;
        let line_h = self.cache.line_height;
        let baseline = self.cache.baseline;

        let mut verts: Vec<f32> = Vec::with_capacity(8192);
        let mut idx:   Vec<u16> = Vec::with_capacity(8192);
        let mut next_quad: u16 = 0;

        // Render the visible window of buffer lines.
        let last = (buf.scroll_top + viewport_rows).min(buf.lines.len());
        for (row_in_viewport, line_idx) in (buf.scroll_top..last).enumerate() {
            let line = &buf.lines[line_idx];
            let row_top = row_in_viewport as f32 * line_h;
            let mut col = 0usize;
            for ch in line.chars() {
                if ch == '\t' {
                    col = ((col / 8) + 1) * 8;
                    continue;
                }
                if ch == ' ' { col += 1; continue; }                  // space = blank cell, no glyph
                if !ch.is_ascii_graphic() { continue; }                // skip non-printables silently
                let g = match self.cache.lookup(ch) { Some(g) => g, None => { col += 1; continue; } };
                if g.width == 0 || g.height == 0 { col += 1; continue; }

                let cx = col as f32 * cell_w;
                let cy = row_top;
                let px0 = cx + g.bearing_x as f32;
                let py0 = cy + baseline - g.bearing_y as f32;
                let px1 = px0 + g.width  as f32;
                let py1 = py0 + g.height as f32;

                let x0 = px0 * s_x - half_vx;
                let x1 = px1 * s_x - half_vx;
                let y0 = half_vy - py1 * s_y;
                let y1 = half_vy - py0 * s_y;

                verts.extend_from_slice(&[
                    x0, y0, 0.0, g.u0, g.v1,
                    x1, y0, 0.0, g.u1, g.v1,
                    x1, y1, 0.0, g.u1, g.v0,
                    x0, y1, 0.0, g.u0, g.v0,
                ]);
                idx.extend_from_slice(&[
                    next_quad, next_quad+1, next_quad+2,
                    next_quad, next_quad+2, next_quad+3,
                ]);
                next_quad += 4;
                col += 1;
                if next_quad as u32 + 4 > u16::MAX as u32 { break; }   // index overflow guard
            }
        }

        // Status line — render `buf.status` on the bottom row, right-aligned.
        let status = format!("  {}{}",
            if buf.modified { "[*] " } else { "" },
            buf.status,
        );
        let status_row = viewport_rows;       // one row below the viewport
        let row_top = status_row as f32 * line_h;
        let mut col = 0usize;
        for ch in status.chars() {
            if ch == ' ' { col += 1; continue; }
            if !ch.is_ascii_graphic() { continue; }
            let g = match self.cache.lookup(ch) { Some(g) => g, None => { col += 1; continue; } };
            if g.width == 0 || g.height == 0 { col += 1; continue; }
            let cx = col as f32 * cell_w;
            let cy = row_top;
            let px0 = cx + g.bearing_x as f32;
            let py0 = cy + baseline - g.bearing_y as f32;
            let px1 = px0 + g.width as f32;
            let py1 = py0 + g.height as f32;
            let x0 = px0 * s_x - half_vx;
            let x1 = px1 * s_x - half_vx;
            let y0 = half_vy - py1 * s_y;
            let y1 = half_vy - py0 * s_y;
            verts.extend_from_slice(&[
                x0, y0, 0.0, g.u0, g.v1,
                x1, y0, 0.0, g.u1, g.v1,
                x1, y1, 0.0, g.u1, g.v0,
                x0, y1, 0.0, g.u0, g.v0,
            ]);
            idx.extend_from_slice(&[
                next_quad, next_quad+1, next_quad+2,
                next_quad, next_quad+2, next_quad+3,
            ]);
            next_quad += 4;
            col += 1;
        }

        if idx.is_empty() {
            // Empty buffer — push a tiny invisible placeholder so the
            // slot has *something*. Actually just skip; the slot keeps
            // its previous content (or is blank if first frame).
            if !self.initialized { /* fall through to set up scene */ }
            else { return Ok(()); }
        }

        let vert_h = conn.cas_put(&blob::vertex_data(&verts))?;
        let idx_h  = conn.cas_put(&blob::index_data(&idx))?;
        let mesh_h = conn.cas_put(&blob::mesh(
            (verts.len() / 5) as u32,
            idx.len() as u32,
            0x0500,
            &vert_h, &idx_h,
        ))?;
        let rend_h = conn.cas_put(&blob::renderable(&mesh_h, &mat_h))?;

        if !self.initialized {
            let cam_xform = [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 5.0, 1.0,
            ];
            let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
            let aspect = dw / dh;
            let cam_h = conn.cas_put(&blob::camera(0.7853982, aspect, 0.1, 100.0, &cam_xform_h))?;
            conn.set_camera(&cam_h)?;
            conn.slot_alloc(self.slot, node_type::FRESCO_NODE_RENDERABLE,
                            flags::FRESCO_SLOT_FLAG_VISIBLE)?;
            conn.slot_set_xform_inline(self.slot, &matrix_identity())?;
            conn.slot_set_root(self.slot)?;
            self.initialized = true;
        }
        conn.slot_set_content(self.slot, &rend_h)?;
        conn.frame_begin(0)?;
        conn.frame_end()?;
        Ok(())
    }
}
