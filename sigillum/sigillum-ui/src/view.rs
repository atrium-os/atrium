//! Drawing a sheet. PIN and passcode bullets are rects, never glyphs, so
//! frescod never receives what was typed, not even as glyph ids.
//!
//! Layout is in points and grows with the system text size; only what is
//! drawn is in device pixels, at the window's scale
//! (resolution-independence.md §3, §6).

use std::io;

use fresco_client::{Connection, FrameBuilder};
use fresco_protocol::RectParams;
use pergola::surface::{hairline, snap_rect};
use pergola::theme::Semantic;

use crate::sheet::{Stage, State};

/// The sheet's size in points at text size 1.
pub const W: f32 = 440.0;
pub const H: f32 = 260.0;
/// Gutters stay put; everything below is sized from text.
const PAD: f32 = 20.0;
const ROW_GAP: f32 = 8.0;
const TITLE_PT: f32 = 17.0;
const BODY_PT: f32 = 13.0;
const ROW_H: f32 = 32.0;
const BULLET: f32 = 8.0;
const CHAR_PT: f32 = 7.0;

/// Device px per pt (the window's scale) and the system text size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale {
    pub k: f32,
    pub text: f32,
}

impl Default for Scale {
    fn default() -> Self {
        Scale { k: 1.0, text: 1.0 }
    }
}

/// The sheet's size in points at text size `text`: only the gutters don't
/// grow with it.
pub fn size(text: f32) -> (f32, f32) {
    let t = text.max(0.5);
    ((W - 2.0 * PAD) * t + 2.0 * PAD, (H - 2.0 * PAD) * t + 2.0 * PAD)
}

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

/// The rows' rects in points, bottom-up from the sheet's foot (Cancel last).
pub fn rows(s: &State, w: f32, h: f32, text: f32) -> Vec<Rect> {
    let n = s.rows.len() as f32;
    let row_h = ROW_H * text;
    let top = h - PAD - n * row_h - (n - 1.0) * ROW_GAP;
    (0..s.rows.len()).map(|i| Rect { x: PAD, y: top + i as f32 * (row_h + ROW_GAP), w: w - 2.0 * PAD, h: row_h }).collect()
}

/// Which row a click at (x, y), in points, hit.
pub fn hit(s: &State, w: f32, h: f32, text: f32, x: f32, y: f32) -> Option<usize> {
    if s.takes_secret() {
        return None;
    }
    rows(s, w, h, text).iter().position(|r| r.contains(x, y))
}

fn truncate(s: &str, width: f32, size: f32) -> String {
    let max = (width / (CHAR_PT * size / BODY_PT)).floor().max(1.0) as usize;
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('\u{2026}');
    t
}

fn rgba(c: pergola::Color) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

/// Emits points as device pixels.
struct Px<'f, 'a> {
    fr: &'f mut FrameBuilder<'a>,
    k: f32,
}

impl Px<'_, '_> {
    fn params(&self, x: f32, y: f32, w: f32, h: f32, c: pergola::Color, radius: f32) -> RectParams {
        RectParams { x, y, w: w.max(0.0), h: h.max(0.0), r: c.r, g: c.g, b: c.b, a: c.a, radius: radius * self.k }
    }

    fn rect(&mut self, r: Rect, c: pergola::Color, radius: f32) -> io::Result<()> {
        let (x, y, w, h) = snap_rect(r.x, r.y, r.w, r.h, self.k);
        let p = self.params(x, y, w, h, c, radius);
        self.fr.rect(p).map(drop)
    }

    /// A box with a 1 pt edge: the edge is a whole number of device
    /// pixels on every side.
    fn boxed(&mut self, r: Rect, edge: pergola::Color, fill: pergola::Color, radius: f32) -> io::Result<()> {
        let (x, y, w, h) = snap_rect(r.x, r.y, r.w, r.h, self.k);
        let b = hairline(self.k);
        let p = self.params(x, y, w, h, edge, radius);
        self.fr.rect(p)?;
        let p = self.params(x + b, y + b, w - 2.0 * b, h - 2.0 * b, fill, (radius - 1.0).max(0.0));
        self.fr.rect(p).map(drop)
    }

    /// A run with its baseline on a device pixel.
    fn text(&mut self, font: u32, size: f32, x: f32, y: f32, c: pergola::Color, s: String) -> io::Result<()> {
        self.fr.text_run(font, size * self.k, (x * self.k).round(), (y * self.k).round(), rgba(c), s).map(drop)
    }
}

/// Draw the sheet laid out in a `w` x `h` pt window.
pub fn draw(conn: &mut Connection, font: u32, s: &State, w: f32, h: f32, sc: Scale) -> io::Result<()> {
    let t = Semantic::LIGHT;
    let tx = sc.text;
    let (title_pt, body_pt) = (TITLE_PT * tx, BODY_PT * tx);
    let mut fr = conn.frame()?;
    let mut px = Px { fr: &mut fr, k: sc.k };
    px.boxed(Rect { x: 0.0, y: 0.0, w, h }, t.border_strong(), t.bg_elevated(), 10.0)?;
    let (title, lines) = s.text();
    let mut y = PAD + title_pt;
    if font != 0 {
        px.text(font, title_pt, PAD, y, t.text_primary(), truncate(&title, w - 2.0 * PAD, title_pt))?;
        for l in &lines {
            y += body_pt * 1.6;
            px.text(font, body_pt, PAD, y, t.text_secondary(), truncate(l, w - 2.0 * PAD, body_pt))?;
        }
        let status = match &s.stage {
            Stage::Touch { .. } => Some("Touch your security key now."),
            Stage::Working { .. } => Some("Waiting for your security key\u{2026}"),
            Stage::Arca => Some("Using the passkey on this device\u{2026}"),
            Stage::Pin { .. } => Some("Enter the PIN of your security key, then press Enter."),
            Stage::Passcode => Some("Enter your passcode, then press Enter."),
            Stage::Choose => None,
        };
        if let Some(n) = s.notice {
            y += body_pt * 1.8;
            px.text(font, body_pt, PAD, y, t.danger_fg(), n.to_string())?;
        }
        if let Some(st) = status {
            y += body_pt * 1.8;
            px.text(font, body_pt, PAD, y, t.text_primary(), st.to_string())?;
        }
    }
    if s.takes_secret() {
        // The secret field: an outlined box with one bullet per byte.
        let field = Rect { x: PAD, y: y + 12.0, w: w - 2.0 * PAD, h: ROW_H * tx };
        px.boxed(field, t.focus_ring(), t.bg_canvas(), 5.0)?;
        let (bullet, step) = (BULLET * tx, (BULLET + 4.0) * tx);
        let max = ((field.w - 2.0 * PAD) / step).floor() as usize;
        let cy = field.y + (field.h - bullet) / 2.0;
        for i in 0..s.bullets().min(max) {
            px.rect(Rect { x: field.x + 10.0 + i as f32 * step, y: cy, w: bullet, h: bullet }, t.text_primary(), bullet / 2.0)?;
        }
    }
    for (i, (row, r)) in s.rows.iter().zip(rows(s, w, h, tx)).enumerate() {
        let focused = i == s.focus && !s.takes_secret();
        let edge = if focused { t.focus_ring() } else { t.border_default() };
        px.boxed(r, edge, t.bg_canvas(), 6.0)?;
        if font != 0 {
            px.text(font, body_pt, r.x + 12.0, r.y + r.h / 2.0 + body_pt * 0.35, t.text_primary(), truncate(&row.label, r.w - 24.0, body_pt))?;
        }
    }
    drop(px);
    fr.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigillum_proto::client::Purpose;
    use sigillum_proto::ui::{DeviceView, Sheet};

    fn sheet() -> State {
        let key = DeviceView { id: 9, name: "YubiKey".into() };
        State::new(1, Sheet::Verify { requester: "Lock screen".into(), user: "alice".into(), purpose: Purpose::Unlock, key: true }, vec![key])
    }

    #[test]
    fn rows_grow_with_the_text_size_and_stay_inside_the_sheet() {
        let s = sheet();
        for text in [0.85, 1.0, 1.5, 2.0] {
            let (w, h) = size(text);
            let rows = rows(&s, w, h, text);
            assert_eq!(rows[0].h, ROW_H * text);
            // Below the title, the body and the status line.
            let body_end = PAD + TITLE_PT * text + 2.0 * BODY_PT * text * 1.8;
            assert!(rows[0].y > body_end, "text {text}: rows start at {} under text ending {body_end}", rows[0].y);
            assert!(rows.last().unwrap().y + rows.last().unwrap().h <= h - PAD + 0.01);
        }
        assert_eq!(size(1.0), (W, H));
    }

    #[test]
    fn a_click_in_points_hits_the_row_under_it_at_any_text_size() {
        let s = sheet();
        for text in [1.0, 2.0] {
            let (w, h) = size(text);
            for (i, r) in rows(&s, w, h, text).iter().enumerate() {
                assert_eq!(hit(&s, w, h, text, r.x + 5.0, r.y + r.h / 2.0), Some(i));
            }
        }
    }
}
