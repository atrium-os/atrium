//! Two-pane file-browser renderer (socket transport).
//!
//! Layout (in cells of `cell_w × line_height`):
//!   row 0:           cwd path (header)
//!   rows 2..=N-1:    LEFT pane — entries with `> ` prefix on selected
//!                     RIGHT pane — preview text (truncated to fit)
//!   row N (last):    footer ("[selected] N entries")
//!
//! Per-glyph render items, screen-pixel coords. The unit-rect mesh
//! and atlas texture are uploaded once at startup via `Renderer::new`;
//! per-frame work is `transform_matrix + renderable + scene_node`
//! per visible glyph plus one `node_list + scene_root + set_root`.

use crate::dir::Entry;
use crate::glyph_cache::GlyphCache;

use fresco_server::command::protocol::{Hash256, NULL_HASH};
use fresco_socket::{wire, Connection};

pub struct Renderer<'a> {
    cache:        &'a GlyphCache,
    rect_mesh:    Hash256,
    selection_mat: Hash256,
    pad_x:        f32,
    pad_y:        f32,
    win_w:        f32,
    win_h:        f32,
}

impl<'a> Renderer<'a> {
    pub fn new(
        cache: &'a GlyphCache,
        conn: &mut Connection,
        win_w: u32,
        win_h: u32,
    ) -> std::io::Result<Self> {
        let v = conn.upload_blob(&wire::vertex_data_xy(&[
            (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
        ]))?;
        let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
        let rect_mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;
        // Subtle indigo highlight under the selected row.
        let selection_mat = conn.upload_blob(&wire::solid_material([0x44, 0x4c, 0x80, 0x80]))?;
        Ok(Self {
            cache, rect_mesh, selection_mat,
            pad_x: 12.0, pad_y: 12.0,
            win_w: win_w as f32, win_h: win_h as f32,
        })
    }

    pub fn render(
        &self,
        conn: &mut Connection,
        cwd: &str,
        entries: &[Entry],
        selected: usize,
        scroll_top: usize,
        preview: &[String],
    ) -> std::io::Result<()> {
        let cell_w   = self.cache.cell_w;
        let line_h   = self.cache.line_height;
        let baseline = self.cache.baseline;

        let total_cols = ((self.win_w - self.pad_x * 2.0) / cell_w).floor() as usize;
        let left_cols  = total_cols * 35 / 100;
        let right_col0 = left_cols + 2;

        let visible_rows = ((self.win_h - self.pad_y * 2.0) / line_h).floor() as usize;
        let list_top_row = 2usize;
        let list_visible = visible_rows.saturating_sub(list_top_row + 1);

        let mut nodes: Vec<Hash256> = Vec::new();

        // Selection highlight — full row across the left pane.
        if selected >= scroll_top && selected < scroll_top + list_visible {
            let row = list_top_row + (selected - scroll_top);
            let row_top = self.pad_y + (row as f32) * line_h;
            let bar_w = (left_cols as f32) * cell_w;
            let xform = wire::affine_2d(bar_w, line_h, self.pad_x, row_top);
            let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
            let r  = conn.upload_blob(&wire::renderable(self.rect_mesh, self.selection_mat))?;
            let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
            nodes.push(sn);
        }

        // Header — cwd path.
        self.push_line(conn, &mut nodes, cwd, 0, 0)?;

        // Left pane — entry list.
        let last = (scroll_top + list_visible).min(entries.len());
        for (i, ent_idx) in (scroll_top..last).enumerate() {
            let row = list_top_row + i;
            let mark = if ent_idx == selected { "> " } else { "  " };
            let suffix = if entries[ent_idx].is_dir { "/" } else { "" };
            let s = format!("{}{}{}", mark, entries[ent_idx].name, suffix);
            // Truncate to left-pane width.
            let trunc = s.chars().take(left_cols.saturating_sub(1)).collect::<String>();
            self.push_line(conn, &mut nodes, &trunc, row, 0)?;
        }

        // Right pane — preview lines.
        for (i, line) in preview.iter().enumerate().take(list_visible) {
            let row = list_top_row + i;
            let remaining = total_cols.saturating_sub(right_col0);
            let trunc = line.chars().take(remaining).collect::<String>();
            self.push_line(conn, &mut nodes, &trunc, row, right_col0)?;
        }

        // Footer.
        let footer = format!(
            "[{}] {} entries",
            entries.get(selected).map(|e| e.name.as_str()).unwrap_or(""),
            entries.len(),
        );
        let footer_row = visible_rows.saturating_sub(1);
        self.push_line(conn, &mut nodes, &footer, footer_row, 0)?;

        if nodes.is_empty() {
            conn.set_root(NULL_HASH)?;
            return Ok(());
        }
        let nl = conn.upload_blob(&wire::node_list(&nodes))?;
        let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
        conn.set_root(sr)?;
        Ok(())
    }

    fn push_line(
        &self,
        conn: &mut Connection,
        nodes: &mut Vec<Hash256>,
        text: &str,
        row: usize,
        col0: usize,
    ) -> std::io::Result<()> {
        let cell_w   = self.cache.cell_w;
        let line_h   = self.cache.line_height;
        let baseline = self.cache.baseline;
        let row_top = self.pad_y + (row as f32) * line_h;
        let mut col = col0;
        for ch in text.chars() {
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
        Ok(())
    }
}
