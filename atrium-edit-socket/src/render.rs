//! atrium-edit-socket renderer on the M6.3 server-side text path.
//!
//! Per frame:
//!   - Optional cursor RECT (one OP_SCENE_NODE_SET).
//!   - One OP_TEXT_RUN_INSTALL per visible non-empty line.
//!   - One OP_TEXT_RUN_INSTALL for the status bar.
//!
//! Stale-node cleanup (lines that disappeared since last frame) is
//! delegated to `fresco_client::FrameBuilder`.

use crate::buffer::Buffer;

use fresco_client::Connection;
use fresco_protocol::RectParams;

pub struct Renderer {
    font_id:  u32,
    size_px:  f32,
    cell_w:   f32,
    line_h:   f32,
    baseline: f32,
    pad_x:    f32,
    pad_y:    f32,
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32) -> Self {
        Self { font_id, size_px, cell_w, line_h, baseline,
               pad_x: 16.0, pad_y: 16.0 }
    }

    pub fn render(
        &self,
        conn: &mut Connection,
        buf: &Buffer,
        viewport_rows: usize,
        cursor_visible: bool,
    ) -> std::io::Result<()> {
        let mut f = conn.frame()?;

        if cursor_visible {
            let cursor_row = buf.cursor_line as i64 - buf.scroll_top as i64;
            if cursor_row >= 0 && (cursor_row as usize) < viewport_rows {
                let cx = self.pad_x + (buf.cursor_col as f32) * self.cell_w;
                let cy = self.pad_y + (cursor_row as f32) * self.line_h;
                f.rect(RectParams {
                    x: cx, y: cy, w: self.cell_w, h: self.line_h,
                    r: 1.0, g: 0.80, b: 0.20, a: 1.0,
                })?;
            }
        }

        let last = (buf.scroll_top + viewport_rows).min(buf.lines.len());
        for (row_in_viewport, line_idx) in (buf.scroll_top..last).enumerate() {
            let line = &buf.lines[line_idx];
            if line.is_empty() { continue; }
            let y_base = self.pad_y + (row_in_viewport as f32) * self.line_h
                       + self.baseline;
            f.text_run(self.font_id, self.size_px,
                       self.pad_x, y_base,
                       [0.95, 0.95, 0.95, 1.0],
                       line)?;
        }

        let status = format!(
            "{}{}",
            if buf.modified { "[*] " } else { "" },
            if buf.status.is_empty() { format!("{:?}", buf.path) } else { buf.status.clone() },
        );
        if !status.is_empty() {
            let y_base = self.pad_y + (viewport_rows as f32) * self.line_h
                       + self.baseline;
            f.text_run(self.font_id, self.size_px,
                       self.pad_x, y_base,
                       [0.85, 0.85, 0.85, 1.0],
                       &status)?;
        }

        f.finish()
    }
}
