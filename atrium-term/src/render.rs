//! Build a single textured-quad mesh from the cell grid + glyph cache,
//! upload to Fresco, attach to the scene root.

use crate::glyph_cache::GlyphCache;
use crate::grid::Grid;

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

/// Render the current grid state. On first call this allocates the
/// scene slot + camera; subsequent calls just upload a new mesh and
/// re-bind it to the slot. Atlas (texture) and material are only
/// uploaded once via fresco's CAS dedup.
pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    /// Slot ID we own.
    slot: u16,
    /// Logical pixel size of the destination window/FBO.
    view_w: u32,
    view_h: u32,
    initialized: bool,
    atlas_hash: Option<[u8; 32]>,
    mat_hash:   Option<[u8; 32]>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache, slot: u16, view_w: u32, view_h: u32) -> Self {
        Self { cache, slot, view_w, view_h,
               initialized: false, atlas_hash: None, mat_hash: None }
    }

    pub fn set_view_size(&mut self, w: u32, h: u32) {
        self.view_w = w;
        self.view_h = h;
    }

    pub fn render(&mut self, conn: &Connection, grid: &Grid) -> std::io::Result<()> {
        // Atlas upload (once — same content → same hash → dedup).
        if self.atlas_hash.is_none() {
            let h = conn.cas_put_texture(self.cache.atlas_w, self.cache.atlas_h, &self.cache.atlas)?;
            self.atlas_hash = Some(h);
            let mat = conn.cas_put(&blob::material_textured(&h, 0xffffffff))?;
            self.mat_hash = Some(mat);
        }
        let mat_h = self.mat_hash.unwrap();

        // Window-pixel → NDC scale.
        let dw = self.view_w.max(1) as f32;
        let dh = self.view_h.max(1) as f32;

        // Our camera (set below): position (0,0,5), fov_y = π/4. With
        // perspective the visible vertical extent at z=0 is
        //   half_vy = dist * tan(fov_y/2) ≈ 5 * 0.4142 ≈ 2.071
        // so screen edges = NDC ±half_vy, NOT ±1. Earlier examples
        // hard-coded ±1 and got 1/2-size content centered — fix here.
        const CAM_DIST: f32 = 5.0;
        const CAM_FOV_Y: f32 = 0.7853982;
        let half_vy = CAM_DIST * (CAM_FOV_Y * 0.5).tan();
        let half_vx = half_vy * dw / dh;
        let s_x = 2.0 * half_vx / dw;
        let s_y = 2.0 * half_vy / dh;

        // Build per-cell quads.
        let cell_w = self.cache.cell_w;
        let cell_h = self.cache.line_height;
        let baseline = self.cache.baseline;

        let mut verts: Vec<f32> = Vec::with_capacity(grid.cells().len() * 20);
        let mut idx:   Vec<u16> = Vec::with_capacity(grid.cells().len() * 6);

        for row in 0..grid.rows {
            let row_top = row as f32 * cell_h;
            for col in 0..grid.cols {
                let i = row as usize * grid.cols as usize + col as usize;
                let ch = grid.cells()[i].ch;
                if ch == ' ' { continue; }
                let g = match self.cache.lookup(ch) { Some(g) => g, None => continue };
                if g.width == 0 || g.height == 0 { continue; }

                // Cell origin in pixel space (top-left), scaled to NDC.
                let cx = col as f32 * cell_w;
                let cy = row_top;

                // Glyph dest in pixel space (cell origin + bearing).
                let px0 = cx + g.bearing_x as f32;
                let py0 = cy + baseline - g.bearing_y as f32;
                let px1 = px0 + g.width  as f32;
                let py1 = py0 + g.height as f32;

                // Pixel-space [0..dw] × [0..dh] → NDC [-half_vx..+half_vx] × [+half_vy..-half_vy]
                let x0 = px0 * s_x - half_vx;
                let x1 = px1 * s_x - half_vx;
                let y0 = half_vy - py1 * s_y;     // flip y
                let y1 = half_vy - py0 * s_y;

                let base = (idx.len() / 6) as u16 * 4;
                verts.extend_from_slice(&[
                    x0, y0, 0.0, g.u0, g.v1,
                    x1, y0, 0.0, g.u1, g.v1,
                    x1, y1, 0.0, g.u1, g.v0,
                    x0, y1, 0.0, g.u0, g.v0,
                ]);
                idx.extend_from_slice(&[base, base+1, base+2, base, base+2, base+3]);
            }
        }

        // Empty grid? Skip — the slot stays at whatever it was.
        if idx.is_empty() {
            return Ok(());
        }

        let vert_h = conn.cas_put(&blob::vertex_data(&verts))?;
        let idx_h  = conn.cas_put(&blob::index_data(&idx))?;
        let mesh_h = conn.cas_put(&blob::mesh(
            (verts.len() / 5) as u32,
            idx.len() as u32,
            0x0500,                 // POSITION + UV
            &vert_h, &idx_h,
        ))?;
        let rend_h = conn.cas_put(&blob::renderable(&mesh_h, &mat_h))?;

        if !self.initialized {
            // First frame: set up camera + slot.
            let cam_xform = [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 5.0, 1.0,
            ];
            let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
            let aspect = dw / dh;
            // Narrow FOV approximates orthographic from a far camera.
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
