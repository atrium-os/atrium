//! Two-pane file-browser renderer on the envelope stack.
//!
//! Layout (in cells of `cell_w × line_height`):
//!   row 0:           cwd path (header)
//!   rows 2..=N-1:    LEFT pane — entries with `> ` prefix on selected
//!                     RIGHT pane — preview text (truncated to fit)
//!   row N (last):    footer ("[selected] N entries")
//!
//! Per-frame nodes: optional selection-highlight RECT + per-glyph
//! TEXTURE nodes for header, list, preview, footer. Sequential ids
//! 1..=N with last_max_id-driven SCENE_NODE_CLEAR for shrinkage.

use crate::dir::Entry;
use crate::glyph_cache::GlyphCache;

use fresco_client::Connection;
use fresco_protocol::{RectParams, TextureParams};

pub struct Renderer<'a> {
    cache: &'a GlyphCache,
    pad_x: f32,
    pad_y: f32,
    win_w: f32,
    win_h: f32,
    last_max_id: std::cell::Cell<u32>,
}

impl<'a> Renderer<'a> {
    pub fn new(cache: &'a GlyphCache, win_w: u32, win_h: u32) -> Self {
        Self {
            cache,
            pad_x: 12.0, pad_y: 12.0,
            win_w: win_w as f32, win_h: win_h as f32,
            last_max_id: std::cell::Cell::new(0),
        }
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

        let total_cols   = ((self.win_w - self.pad_x * 2.0) / cell_w).floor() as usize;
        let left_cols    = total_cols * 35 / 100;
        let right_col0   = left_cols + 2;
        let visible_rows = ((self.win_h - self.pad_y * 2.0) / line_h).floor() as usize;
        let list_top_row = 2usize;
        let list_visible = visible_rows.saturating_sub(list_top_row + 1);

        conn.scene_frame_begin()?;
        let mut next_id: u32 = 1;

        /* Selection highlight — full row across the left pane. */
        if selected >= scroll_top && selected < scroll_top + list_visible {
            let row = list_top_row + (selected - scroll_top);
            let row_top = self.pad_y + (row as f32) * line_h;
            let bar_w = (left_cols as f32) * cell_w;
            conn.scene_node_rect(next_id, RectParams {
                x: self.pad_x, y: row_top, w: bar_w, h: line_h,
                r: 0.27, g: 0.30, b: 0.50, a: 0.5,
            })?;
            next_id += 1;
        }

        /* Header — cwd path. */
        self.push_line(conn, &mut next_id, cwd, 0, 0)?;

        /* Left pane — entry list. */
        let last = (scroll_top + list_visible).min(entries.len());
        for (i, ent_idx) in (scroll_top..last).enumerate() {
            let row = list_top_row + i;
            let mark   = if ent_idx == selected { "> " } else { "  " };
            let suffix = if entries[ent_idx].is_dir { "/" } else { "" };
            let s = format!("{}{}{}", mark, entries[ent_idx].name, suffix);
            let trunc = s.chars().take(left_cols.saturating_sub(1)).collect::<String>();
            self.push_line(conn, &mut next_id, &trunc, row, 0)?;
        }

        /* Right pane — preview lines. */
        for (i, line) in preview.iter().enumerate().take(list_visible) {
            let row = list_top_row + i;
            let remaining = total_cols.saturating_sub(right_col0);
            let trunc = line.chars().take(remaining).collect::<String>();
            self.push_line(conn, &mut next_id, &trunc, row, right_col0)?;
        }

        /* Footer. */
        let footer = format!(
            "[{}] {} entries",
            entries.get(selected).map(|e| e.name.as_str()).unwrap_or(""),
            entries.len(),
        );
        let footer_row = visible_rows.saturating_sub(1);
        self.push_line(conn, &mut next_id, &footer, footer_row, 0)?;

        let new_max = next_id - 1;
        let old_max = self.last_max_id.get();
        for stale in (new_max + 1)..=old_max {
            conn.scene_node_clear(stale)?;
        }
        self.last_max_id.set(new_max);

        conn.scene_frame_end()?;
        Ok(())
    }

    fn push_line(
        &self,
        conn: &mut Connection,
        next_id: &mut u32,
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
                Some(g) => *g,
                None    => { col += 1; continue; }
            };

            let cx = self.pad_x + (col as f32) * cell_w;
            let px = cx + g.metrics.bearing_x as f32;
            let py = row_top + baseline - g.metrics.bearing_y as f32;
            let w  = g.metrics.width  as f32;
            let h  = g.metrics.height as f32;

            conn.scene_node_texture(*next_id, TextureParams {
                x: px, y: py, w, h, slot_id: g.slot_id,
            })?;
            *next_id += 1;
            col += 1;
        }
        Ok(())
    }
}
