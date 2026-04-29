//! Two-pane file-browser renderer.
//!
//! Left pane: directory entries with `> ` prefix on the selected row.
//! Right pane: preview of the selected entry (first N lines of a
//! text file, or `(directory)` / size for non-text).
//!
//! Single textured mesh (atlas + glyphs), single slot. The "selected"
//! visual is just a `> ` prefix — keeping this app's render to one
//! material/slot until we earn a reason to add a highlight rect.

use crate::dir::Entry;
use crate::glyph_cache::{GlyphCache, GlyphEntry};

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    slot:  u16,
    initialized: bool,
    mat_hash: Option<[u8; 32]>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache, slot: u16) -> Self {
        Self { cache, slot, initialized: false, mat_hash: None }
    }

    pub fn render(
        &mut self, conn: &Connection,
        cwd: &str, entries: &[Entry], selected: usize,
        scroll_top: usize,
        preview: &[String], visible_rows: usize,
    ) -> std::io::Result<()> {
        if self.mat_hash.is_none() {
            let h = conn.cas_put_texture(self.cache.atlas_w, self.cache.atlas_h, &self.cache.atlas)?;
            let mat = conn.cas_put(&blob::material_textured(&h, 0xffffffff))?;
            self.mat_hash = Some(mat);
        }
        let mat_h = self.mat_hash.unwrap();

        let disp = conn.display();
        let dw = disp.width.max(1) as f32;
        let dh = disp.height.max(1) as f32;

        const CAM_DIST: f32 = 5.0;
        const CAM_FOV_Y: f32 = 0.7853982;
        let half_vy = CAM_DIST * (CAM_FOV_Y * 0.5).tan();
        let half_vx = half_vy * dw / dh;
        let s_x = 2.0 * half_vx / dw;
        let s_y = 2.0 * half_vy / dh;

        let cell_w = self.cache.cell_w;
        let line_h = self.cache.line_height;
        let baseline = self.cache.baseline;

        // Pane geometry, in cells: split roughly 35% / 65%.
        let total_cols = (dw / cell_w) as usize;
        let left_cols  = total_cols * 35 / 100;
        let _gap       = 2;
        let right_col0 = left_cols + 2;

        let mut verts: Vec<f32> = Vec::with_capacity(8192);
        let mut idx:   Vec<u16> = Vec::with_capacity(8192);
        let mut next_quad: u16 = 0;

        // Header on row 0: cwd path.
        push_line(&mut verts, &mut idx, &mut next_quad, self.cache,
                  cwd, 0, 0, half_vx, half_vy, s_x, s_y, cell_w, line_h, baseline);

        // Left pane: entry list starting row 2.
        let list_top_row = 2usize;
        let list_visible = visible_rows.saturating_sub(list_top_row);
        let last = (scroll_top + list_visible).min(entries.len());
        for (i, ent_idx) in (scroll_top..last).enumerate() {
            let row = list_top_row + i;
            let mark = if ent_idx == selected { "> " } else { "  " };
            let suffix = if entries[ent_idx].is_dir { "/" } else { "" };
            let s = format!("{}{}{}", mark, entries[ent_idx].name, suffix);
            push_line(&mut verts, &mut idx, &mut next_quad, self.cache,
                      &s, row, 0, half_vx, half_vy, s_x, s_y, cell_w, line_h, baseline);
        }

        // Right pane: preview lines starting row 2.
        for (i, line) in preview.iter().enumerate().take(visible_rows.saturating_sub(2)) {
            let row = list_top_row + i;
            // Truncate to fit pane width.
            let remaining = total_cols.saturating_sub(right_col0);
            let truncated = line.chars().take(remaining).collect::<String>();
            push_line(&mut verts, &mut idx, &mut next_quad, self.cache,
                      &truncated, row, right_col0, half_vx, half_vy, s_x, s_y, cell_w, line_h, baseline);
        }

        // Footer hint on the last row.
        let footer = format!("[{}] {} entries", entries.get(selected).map(|e| e.name.as_str()).unwrap_or(""), entries.len());
        push_line(&mut verts, &mut idx, &mut next_quad, self.cache,
                  &footer, visible_rows, 0, half_vx, half_vy, s_x, s_y, cell_w, line_h, baseline);

        if idx.is_empty() {
            // Nothing to draw — skip but still set up the slot graph
            // on first call so the scene has a root.
            if self.initialized { return Ok(()); }
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

#[allow(clippy::too_many_arguments)]
fn push_line(
    verts: &mut Vec<f32>, idx: &mut Vec<u16>, next_quad: &mut u16,
    cache: &GlyphCache,
    text: &str, row: usize, col0: usize,
    half_vx: f32, half_vy: f32, s_x: f32, s_y: f32,
    cell_w: f32, line_h: f32, baseline: f32,
) {
    let row_top = row as f32 * line_h;
    let mut col = col0;
    for ch in text.chars() {
        if ch == ' ' { col += 1; continue; }
        if !ch.is_ascii_graphic() { continue; }
        let g: &GlyphEntry = match cache.lookup(ch) {
            Some(g) => g,
            None => { col += 1; continue; }
        };
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
            *next_quad, *next_quad+1, *next_quad+2,
            *next_quad, *next_quad+2, *next_quad+3,
        ]);
        *next_quad += 4;
        col += 1;
    }
}
