//! atrium-term-socket renderer — emits per-cell glyph render items in
//! screen-pixel coords. The terminal grid is dense, so we re-emit
//! the visible cells every frame; the unit-rect mesh + atlas texture
//! are uploaded once and shared by every cell, so per-frame work is
//! transform_matrix + scene_node blob uploads (CAS-deduped where
//! identical) plus one node_list + scene_root.

use crate::glyph_cache::GlyphCache;
use crate::grid::Grid;

use fresco_server::command::protocol::{Hash256, NULL_HASH};
use fresco_socket::{wire, Connection};

pub struct Renderer<'a> {
    cache:        &'a GlyphCache,
    rect_mesh:    Hash256,
    cursor_mat:   Hash256,
    pad_x:        f32,
    pad_y:        f32,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache, conn: &mut Connection) -> std::io::Result<Self> {
        let v = conn.upload_blob(&wire::vertex_data_xy(&[
            (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
        ]))?;
        let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
        let rect_mesh  = conn.upload_blob(&wire::mesh(4, 6, v, i))?;
        let cursor_mat = conn.upload_blob(&wire::solid_material([0xff, 0xcc, 0x33, 0x80]))?;
        Ok(Self { cache, rect_mesh, cursor_mat, pad_x: 8.0, pad_y: 8.0 })
    }

    pub fn render(&self, conn: &mut Connection, grid: &Grid) -> std::io::Result<()> {
        let line_h   = self.cache.line_height;
        let cell_w   = self.cache.cell_w;
        let baseline = self.cache.baseline;

        let mut nodes: Vec<Hash256> = Vec::new();

        // Cursor block first so glyphs alpha-blend on top.
        let cx = self.pad_x + (grid.cursor_col as f32) * cell_w;
        let cy = self.pad_y + (grid.cursor_row as f32) * line_h;
        let xform = wire::affine_2d(cell_w, line_h, cx, cy);
        let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
        let r  = conn.upload_blob(&wire::renderable(self.rect_mesh, self.cursor_mat))?;
        let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
        nodes.push(sn);

        // Cells.
        let cells = grid.cells();
        for row in 0..grid.rows {
            let row_top = self.pad_y + (row as f32) * line_h;
            for col in 0..grid.cols {
                let cell = cells[row as usize * grid.cols as usize + col as usize];
                let ch = cell.ch;
                if ch == ' ' || !ch.is_ascii_graphic() { continue; }
                let g = match self.cache.lookup(ch) {
                    Some(g) => g,
                    None    => continue,
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
            }
        }

        if nodes.is_empty() {
            conn.set_root(NULL_HASH)?;
            return Ok(());
        }
        let nl = conn.upload_blob(&wire::node_list(&nodes))?;
        let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
        conn.set_root(sr)?;
        Ok(())
    }
}
