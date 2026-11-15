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

/// Lays out in points and draws at the window's scale `k`
/// (resolution-independence.md §3).
pub struct Renderer {
    font_id:  u32,
    size_px:  f32,
    cell_w:   f32,
    line_h:   f32,
    /// The ascent: a text run's y on the wire is the top of its em box
    /// (fresco-protocol), one ascent above the baseline.
    baseline: f32,
    pad_x:    f32,
    pad_y:    f32,
    /// Device px per pt.
    pub k:    f32,
}

impl Renderer {
    pub fn new(font_id: u32, size_px: f32,
               cell_w: f32, line_h: f32, baseline: f32) -> Self {
        Self { font_id, size_px, cell_w, line_h, baseline,
               pad_x: 16.0, pad_y: 16.0, k: 1.0 }
    }

    /// How many text rows fit a window `h` pt tall, less the status line.
    pub fn viewport_rows(&self, h: f32) -> usize {
        (((h - 2.0 * self.pad_y) / self.line_h).floor() as isize - 1).max(1) as usize
    }

    /// How many columns fit a window `w` pt wide.
    pub fn viewport_cols(&self, w: f32) -> usize {
        (((w - 2.0 * self.pad_x) / self.cell_w).floor() as isize).max(1) as usize
    }

    /// Draws `buf` from its `scroll_top` row and `scroll_left` column;
    /// `viewport_cols` columns are drawn (and one partly cut).
    pub fn render(
        &self,
        conn: &mut Connection,
        buf: &Buffer,
        viewport_rows: usize,
        viewport_cols: usize,
        cursor_visible: bool,
    ) -> std::io::Result<()> {
        let mut f = conn.frame()?;

        if cursor_visible {
            let cursor_row = buf.cursor_line as i64 - buf.scroll_top as i64;
            if cursor_row >= 0 && (cursor_row as usize) < viewport_rows {
                let cell = buf.cursor_cell().saturating_sub(buf.scroll_left);
                let cx = self.pad_x + (cell as f32) * self.cell_w;
                let cy = self.pad_y + (cursor_row as f32) * self.line_h;
                let k = self.k;
                let (x0, y0) = ((cx * k).round(), (cy * k).round());
                f.rect(RectParams {
                    x: x0, y: y0, w: ((cx + self.cell_w) * k).round() - x0, h: ((cy + self.line_h) * k).round() - y0,
                    r: 1.0, g: 0.80, b: 0.20, a: 1.0, radius: 0.0,
                })?;
            }
        }

        let last = (buf.scroll_top + viewport_rows).min(buf.lines.len());
        for (row_in_viewport, line_idx) in (buf.scroll_top..last).enumerate() {
            let line: String = buf.lines[line_idx].chars().skip(buf.scroll_left).take(viewport_cols + 1).collect();
            if line.is_empty() { continue; }
            let y_base = ((self.pad_y + (row_in_viewport as f32) * self.line_h
                       + self.baseline) * self.k).round();
            f.text_run(self.font_id, self.size_px * self.k,
                       (self.pad_x * self.k).round(), y_base - self.baseline * self.k,
                       [0.95, 0.95, 0.95, 1.0],
                       &line)?;
        }

        let status = format!(
            "{}{}",
            if buf.modified { "[*] " } else { "" },
            if buf.status.is_empty() { format!("{:?}", buf.path) } else { buf.status.clone() },
        );
        if !status.is_empty() {
            let y_base = ((self.pad_y + (viewport_rows as f32) * self.line_h
                       + self.baseline) * self.k).round();
            f.text_run(self.font_id, self.size_px * self.k,
                       (self.pad_x * self.k).round(), y_base - self.baseline * self.k,
                       [0.85, 0.85, 0.85, 1.0],
                       &status)?;
        }

        f.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Renderer;

    /// The rows follow the window's height in points, not its pixels:
    /// the same at any scale.
    #[test]
    fn rows_follow_the_window_height() {
        let mut r = Renderer::new(1, 18.0, 10.8, 24.0, 18.0);
        let short = r.viewport_rows(300.0);
        let tall = r.viewport_rows(900.0);
        assert!(tall > short && short >= 1);
        r.k = 2.0;
        assert_eq!(r.viewport_rows(900.0), tall);
        assert_eq!(r.viewport_rows(10.0), 1, "at least one");
        // Columns too, in points.
        assert_eq!(r.viewport_cols(720.0), ((720.0 - 32.0) / 10.8f32).floor() as usize);
        r.k = 1.0;
        assert_eq!(r.viewport_cols(720.0), 63);
        assert_eq!(r.viewport_cols(5.0), 1);
    }
}
