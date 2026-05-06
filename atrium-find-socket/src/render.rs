//! Two-pane file-browser renderer on the M6.3 server-side text path.
//!
//! Per frame: optional selection-highlight RECT + one
//! OP_TEXT_RUN_INSTALL per visible non-empty text line (header,
//! left-pane entries, right-pane preview lines, footer).

use crate::dir::Entry;

use fresco_client::{Connection, FrameBuilder};
use fresco_protocol::RectParams;

pub struct Renderer {
    font_id:  u32,
    size_px:  f32,
    cell_w:   f32,
    line_h:   f32,
    baseline: f32,
    pad_x:    f32,
    pad_y:    f32,
    win_w:    f32,
    win_h:    f32,
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32,
               win_w: u32, win_h: u32) -> Self {
        Self {
            font_id, size_px, cell_w, line_h, baseline,
            pad_x: 12.0, pad_y: 12.0,
            win_w: win_w as f32, win_h: win_h as f32,
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
        let total_cols   = ((self.win_w - self.pad_x * 2.0) / self.cell_w).floor() as usize;
        let left_cols    = total_cols * 35 / 100;
        let right_col0   = left_cols + 2;
        let visible_rows = ((self.win_h - self.pad_y * 2.0) / self.line_h).floor() as usize;
        let list_top_row = 2usize;
        let list_visible = visible_rows.saturating_sub(list_top_row + 1);

        let mut f = conn.frame()?;

        if selected >= scroll_top && selected < scroll_top + list_visible {
            let row = list_top_row + (selected - scroll_top);
            let row_top = self.pad_y + (row as f32) * self.line_h;
            let bar_w = (left_cols as f32) * self.cell_w;
            f.rect(RectParams {
                x: self.pad_x, y: row_top, w: bar_w, h: self.line_h,
                r: 0.27, g: 0.30, b: 0.50, a: 0.5,
            })?;
        }

        self.push_line(&mut f, cwd, 0, 0)?;

        let last = (scroll_top + list_visible).min(entries.len());
        for (i, ent_idx) in (scroll_top..last).enumerate() {
            let row = list_top_row + i;
            let mark   = if ent_idx == selected { "> " } else { "  " };
            let suffix = if entries[ent_idx].is_dir { "/" } else { "" };
            let s = format!("{}{}{}", mark, entries[ent_idx].name, suffix);
            let trunc = s.chars().take(left_cols.saturating_sub(1)).collect::<String>();
            self.push_line(&mut f, &trunc, row, 0)?;
        }

        for (i, line) in preview.iter().enumerate().take(list_visible) {
            let row = list_top_row + i;
            let remaining = total_cols.saturating_sub(right_col0);
            let trunc = line.chars().take(remaining).collect::<String>();
            self.push_line(&mut f, &trunc, row, right_col0)?;
        }

        let footer = format!(
            "[{}] {} entries",
            entries.get(selected).map(|e| e.name.as_str()).unwrap_or(""),
            entries.len(),
        );
        let footer_row = visible_rows.saturating_sub(1);
        self.push_line(&mut f, &footer, footer_row, 0)?;

        f.finish()
    }

    fn push_line(
        &self,
        f: &mut FrameBuilder,
        text: &str,
        row: usize,
        col0: usize,
    ) -> std::io::Result<()> {
        if text.is_empty() { return Ok(()); }
        let x = self.pad_x + (col0 as f32) * self.cell_w;
        let y_base = self.pad_y + (row as f32) * self.line_h + self.baseline;
        f.text_run(self.font_id, self.size_px, x, y_base,
                   [0.95, 0.95, 0.95, 1.0], text)?;
        Ok(())
    }
}
