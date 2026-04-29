//! Clock renderer: solid-material analog face + textured digital
//! readout, composed via the slot graph (root has both as children).

use crate::glyph_cache::GlyphCache;

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection, SlotId};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    /// Slot IDs: 1 = root, 2 = analog (solid), 3 = digital (textured).
    initialized: bool,
    text_mat: Option<[u8; 32]>,
    solid_mat: Option<[u8; 32]>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache) -> Self {
        Self { cache, initialized: false, text_mat: None, solid_mat: None }
    }

    pub fn render(&mut self, conn: &Connection,
                  hour: u32, minute: u32, second: u32) -> std::io::Result<()> {
        // Lazy: upload atlas + materials on first frame.
        if self.text_mat.is_none() {
            let tex = conn.cas_put_texture(self.cache.atlas_w, self.cache.atlas_h, &self.cache.atlas)?;
            self.text_mat = Some(conn.cas_put(&blob::material_textured(&tex, 0xffffffff))?);
        }
        if self.solid_mat.is_none() {
            // White RGBA8 = R=G=B=255, A=255 = 0xffffffff in our packing.
            self.solid_mat = Some(conn.cas_put(&blob::material_solid(1.0, 1.0, 1.0, 1.0))?);
        }

        let disp = conn.display();
        let dw = disp.width.max(1) as f32;
        let dh = disp.height.max(1) as f32;

        const CAM_DIST: f32 = 5.0;
        const CAM_FOV_Y: f32 = 0.7853982;
        let half_vy = CAM_DIST * (CAM_FOV_Y * 0.5).tan();
        let half_vx = half_vy * dw / dh;
        let s_x = 2.0 * half_vx / dw;
        let s_y = 2.0 * half_vy / dh;

        // ── Analog face: solid quads for tick marks + 3 hands ────
        // Center, in pixel space; reserve ~1/6 of height at the
        // bottom for the digital readout.
        let cx = dw * 0.5;
        let cy = dh * 0.42;
        let radius = (dw.min(dh) * 0.42).floor();

        let mut sverts: Vec<f32> = Vec::with_capacity(512);
        let mut sidx:   Vec<u16> = Vec::with_capacity(512);

        // 12 tick marks: thin rectangles radiating from r_inner to r_outer.
        let r_outer = radius;
        let r_inner = radius * 0.92;
        for i in 0..12 {
            let unit  = i as f32 / 12.0;
            let theta = unit * std::f32::consts::TAU;
            // hour-marker direction (top = sin/-cos convention so 12 is up)
            let dx = theta.sin();
            let dy = -theta.cos();
            // each tick is "thicker" than the hands — 6px
            push_segment(&mut sverts, &mut sidx,
                cx + dx * r_inner, cy + dy * r_inner,
                cx + dx * r_outer, cy + dy * r_outer,
                6.0,
                half_vx, half_vy, s_x, s_y);
        }
        // Hour hand: short and thick.
        let h_unit = ((hour % 12) as f32 + minute as f32 / 60.0) / 12.0;
        let m_unit = (minute as f32 + second as f32 / 60.0) / 60.0;
        let s_unit =  second as f32 / 60.0;
        let mk = |u: f32, len: f32, thick: f32| {
            let theta = u * std::f32::consts::TAU;
            (theta.sin(), -theta.cos(), len, thick)
        };
        // Hour
        let (dx, dy, len, thick) = mk(h_unit, radius * 0.55, 10.0);
        push_segment(&mut sverts, &mut sidx, cx, cy, cx + dx * len, cy + dy * len, thick,
                     half_vx, half_vy, s_x, s_y);
        // Minute
        let (dx, dy, len, thick) = mk(m_unit, radius * 0.80, 6.0);
        push_segment(&mut sverts, &mut sidx, cx, cy, cx + dx * len, cy + dy * len, thick,
                     half_vx, half_vy, s_x, s_y);
        // Second
        let (dx, dy, len, thick) = mk(s_unit, radius * 0.88, 2.0);
        push_segment(&mut sverts, &mut sidx, cx, cy, cx + dx * len, cy + dy * len, thick,
                     half_vx, half_vy, s_x, s_y);

        // Build solid mesh.
        let svert_h = conn.cas_put(&blob::vertex_data(&sverts))?;
        let sidx_h  = conn.cas_put(&blob::index_data(&sidx))?;
        let smesh_h = conn.cas_put(&blob::mesh(
            (sverts.len() / 3) as u32, sidx.len() as u32,
            0x0100,                         // POSITION only (stride 12)
            &svert_h, &sidx_h,
        ))?;
        let srend_h = conn.cas_put(&blob::renderable(&smesh_h, &self.solid_mat.unwrap()))?;

        // ── Digital readout: textured atlas mesh ─────────────────
        let txt = format!("{:02}:{:02}:{:02}", hour, minute, second);
        let cell_w = self.cache.cell_w;
        let line_h = self.cache.line_height;
        let baseline = self.cache.baseline;

        // Position: centered horizontally near the bottom.
        let txt_w = txt.chars().count() as f32 * cell_w;
        let origin_x = (dw - txt_w) * 0.5;
        let origin_y = dh * 0.86;       // top of text row in pixel space

        let mut tverts: Vec<f32> = Vec::with_capacity(256);
        let mut tidx:   Vec<u16> = Vec::with_capacity(256);
        let mut next_quad: u16 = 0;
        for (i, ch) in txt.chars().enumerate() {
            if ch == ' ' { continue; }
            let g = match self.cache.lookup(ch) { Some(g) => g, None => continue };
            if g.width == 0 || g.height == 0 { continue; }
            let cx_p = origin_x + i as f32 * cell_w;
            let px0 = cx_p + g.bearing_x as f32;
            let py0 = origin_y + baseline - g.bearing_y as f32;
            let px1 = px0 + g.width as f32;
            let py1 = py0 + g.height as f32;
            let x0 = px0 * s_x - half_vx;
            let x1 = px1 * s_x - half_vx;
            let y0 = half_vy - py1 * s_y;
            let y1 = half_vy - py0 * s_y;
            tverts.extend_from_slice(&[
                x0, y0, 0.0, g.u0, g.v1,
                x1, y0, 0.0, g.u1, g.v1,
                x1, y1, 0.0, g.u1, g.v0,
                x0, y1, 0.0, g.u0, g.v0,
            ]);
            tidx.extend_from_slice(&[
                next_quad, next_quad+1, next_quad+2,
                next_quad, next_quad+2, next_quad+3,
            ]);
            next_quad += 4;
        }
        let _ = line_h;

        let trend_h = if !tidx.is_empty() {
            let tvert_h = conn.cas_put(&blob::vertex_data(&tverts))?;
            let tidx_h  = conn.cas_put(&blob::index_data(&tidx))?;
            let tmesh_h = conn.cas_put(&blob::mesh(
                (tverts.len() / 5) as u32, tidx.len() as u32,
                0x0500, &tvert_h, &tidx_h,
            ))?;
            Some(conn.cas_put(&blob::renderable(&tmesh_h, &self.text_mat.unwrap()))?)
        } else { None };

        // ── Slot graph: root [1] → children [2 (analog), 3 (digital)] ─
        let root = 1u16;
        let analog: SlotId = 2;
        let digital: SlotId = 3;
        if !self.initialized {
            // Camera
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

            for s in [root, analog, digital] {
                conn.slot_alloc(s, node_type::FRESCO_NODE_RENDERABLE,
                                flags::FRESCO_SLOT_FLAG_VISIBLE)?;
                conn.slot_set_xform_inline(s, &matrix_identity())?;
            }
            conn.slot_set_children(root, &[analog, digital])?;
            conn.slot_set_root(root)?;
            self.initialized = true;
        }
        conn.slot_set_content(analog, &srend_h)?;
        if let Some(h) = trend_h {
            conn.slot_set_content(digital, &h)?;
        }
        conn.frame_begin(0)?;
        conn.frame_end()?;
        Ok(())
    }
}

/// Append a "thick line segment" as 2 triangles.
/// (x0,y0) → (x1,y1) in pixel space, perpendicular thickness `t` px.
#[allow(clippy::too_many_arguments)]
fn push_segment(
    verts: &mut Vec<f32>, idx: &mut Vec<u16>,
    x0: f32, y0: f32, x1: f32, y1: f32, thick: f32,
    half_vx: f32, half_vy: f32, s_x: f32, s_y: f32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx*dx + dy*dy).sqrt().max(1e-6);
    let nx = -dy / len * (thick * 0.5);
    let ny =  dx / len * (thick * 0.5);
    // 4 corners in pixel space → NDC.
    let pts_px = [
        (x0 + nx, y0 + ny),
        (x0 - nx, y0 - ny),
        (x1 - nx, y1 - ny),
        (x1 + nx, y1 + ny),
    ];
    let base = (verts.len() / 3) as u16;
    for &(px, py) in &pts_px {
        let xn = px * s_x - half_vx;
        let yn = half_vy - py * s_y;
        verts.extend_from_slice(&[xn, yn, 0.0]);
    }
    idx.extend_from_slice(&[base, base+1, base+2, base, base+2, base+3]);
}
