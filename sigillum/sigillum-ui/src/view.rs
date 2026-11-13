//! Drawing a sheet. PIN and passcode bullets are rects, never glyphs, so
//! frescod never receives what was typed, not even as glyph ids.

use std::io;

use fresco_client::Connection;
use fresco_protocol::RectParams;
use pergola::theme::Semantic;

use crate::sheet::{Stage, State};

pub const W: f32 = 440.0;
pub const H: f32 = 260.0;
const PAD: f32 = 20.0;
const TITLE_PX: f32 = 17.0;
const BODY_PX: f32 = 13.0;
const ROW_H: f32 = 32.0;
const ROW_GAP: f32 = 8.0;
const BULLET: f32 = 8.0;
const CHAR_PX: f32 = 7.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// The rows' rects, bottom-up from the sheet's foot (Cancel last).
pub fn rows(s: &State, w: f32, h: f32) -> Vec<Rect> {
    let n = s.rows.len() as f32;
    let top = h - PAD - n * ROW_H - (n - 1.0) * ROW_GAP;
    (0..s.rows.len()).map(|i| Rect { x: PAD, y: top + i as f32 * (ROW_H + ROW_GAP), w: w - 2.0 * PAD, h: ROW_H }).collect()
}

/// Which row a click at (x, y) hit.
pub fn hit(s: &State, w: f32, h: f32, x: f32, y: f32) -> Option<usize> {
    if s.takes_secret() {
        return None;
    }
    rows(s, w, h).iter().position(|r| r.contains(x, y))
}

fn rect(r: Rect, c: pergola::Color, radius: f32) -> RectParams {
    RectParams { x: r.x, y: r.y, w: r.w.max(0.0), h: r.h.max(0.0), r: c.r, g: c.g, b: c.b, a: c.a, radius }
}

fn rgba(c: pergola::Color) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

fn truncate(s: &str, width: f32, px: f32) -> String {
    let max = (width / (CHAR_PX * px / BODY_PX)).floor().max(1.0) as usize;
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('\u{2026}');
    t
}

pub fn draw(conn: &mut Connection, font: u32, s: &State, w: f32, h: f32) -> io::Result<()> {
    let t = Semantic::LIGHT;
    let mut fr = conn.frame()?;
    fr.rect(rect(Rect { x: 0.0, y: 0.0, w, h }, t.border_strong(), 10.0))?;
    fr.rect(rect(Rect { x: 1.0, y: 1.0, w: w - 2.0, h: h - 2.0 }, t.bg_elevated(), 9.0))?;
    let (title, lines) = s.text();
    let mut y = PAD + TITLE_PX;
    if font != 0 {
        fr.text_run(font, TITLE_PX, PAD, y, rgba(t.text_primary()), truncate(&title, w - 2.0 * PAD, TITLE_PX))?;
        for l in &lines {
            y += BODY_PX * 1.6;
            fr.text_run(font, BODY_PX, PAD, y, rgba(t.text_secondary()), truncate(l, w - 2.0 * PAD, BODY_PX))?;
        }
        let status = match &s.stage {
            Stage::Touch { .. } => Some("Touch your security key now."),
            Stage::Working { .. } => Some("Waiting for your security key\u{2026}"),
            Stage::Pin { .. } => Some("Enter the PIN of your security key, then press Enter."),
            Stage::Passcode => Some("Enter your passcode, then press Enter."),
            Stage::Choose => None,
        };
        if let Some(n) = s.notice {
            y += BODY_PX * 1.8;
            fr.text_run(font, BODY_PX, PAD, y, rgba(t.danger_fg()), n.to_string())?;
        }
        if let Some(st) = status {
            y += BODY_PX * 1.8;
            fr.text_run(font, BODY_PX, PAD, y, rgba(t.text_primary()), st.to_string())?;
        }
    }
    if s.takes_secret() {
        // The secret field: an outlined box with one bullet per byte.
        let field = Rect { x: PAD, y: y + 12.0, w: w - 2.0 * PAD, h: ROW_H };
        fr.rect(rect(field, t.focus_ring(), 5.0))?;
        fr.rect(rect(Rect { x: field.x + 1.0, y: field.y + 1.0, w: field.w - 2.0, h: field.h - 2.0 }, t.bg_canvas(), 4.0))?;
        let max = ((field.w - 2.0 * PAD) / (BULLET + 4.0)).floor() as usize;
        let cy = field.y + (field.h - BULLET) / 2.0;
        for i in 0..s.bullets().min(max) {
            fr.rect(rect(Rect { x: field.x + 10.0 + i as f32 * (BULLET + 4.0), y: cy, w: BULLET, h: BULLET }, t.text_primary(), BULLET / 2.0))?;
        }
    }
    for (i, (row, r)) in s.rows.iter().zip(rows(s, w, h)).enumerate() {
        let focused = i == s.focus && !s.takes_secret();
        let edge = if focused { t.focus_ring() } else { t.border_default() };
        fr.rect(rect(r, edge, 6.0))?;
        fr.rect(rect(Rect { x: r.x + 1.0, y: r.y + 1.0, w: r.w - 2.0, h: r.h - 2.0 }, t.bg_canvas(), 5.0))?;
        if font != 0 {
            fr.text_run(font, BODY_PX, r.x + 12.0, r.y + r.h / 2.0 + BODY_PX * 0.35, rgba(t.text_primary()), truncate(&row.label, r.w - 24.0, BODY_PX))?;
        }
    }
    fr.finish()
}
