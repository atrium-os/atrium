//! Drawing the field. Bullets are rects, never glyphs, so frescod never
//! receives the typed text, not even its characters as glyph ids. The only
//! text sent is the chip's label (an account name, the app it fills for)
//! and refusals.

use std::io;

use fresco_client::Connection;
use fresco_protocol::RectParams;
use pergola::theme::Semantic;

use crate::field::{Field, Offer};

pub const FONT_PX: f32 = 13.0;
/// A rough advance for truncation; frescod shapes the real run.
const CHAR_PX: f32 = 7.0;
/// The field's edge sits inside the slot, leaving room for the focus halo
/// and ring (visual language rev. 2): halo 2 px, ring 1 px, gap 1 px.
const PAD: f32 = 6.0;
const BULLET: f32 = 7.0;
const BULLET_STEP: f32 = 10.0;

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

/// Where things go in a `w` x `h` slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub chip: Option<Rect>,
    /// Bullets that fit left of the chip.
    pub bullets_max: usize,
}

pub fn layout(f: &Field, w: f32, h: f32) -> Layout {
    let shows = f.chip().is_some() || f.notice.is_some();
    let chip = shows.then(|| {
        let cw = (w * 0.62).max(w.min(120.0)) - PAD;
        Rect { x: w - cw - PAD, y: PAD, w: cw, h: (h - 2.0 * PAD).max(1.0) }
    });
    let right = chip.map_or(w - PAD, |c| c.x - PAD);
    let bullets_max = (((right - 2.0 * PAD) / BULLET_STEP).floor().max(0.0)) as usize;
    Layout { chip, bullets_max }
}

fn truncate(s: &str, width: f32) -> String {
    let max = (width / CHAR_PX).floor().max(1.0) as usize;
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// What the chip says: the offer and who it fills for, or a refusal.
pub fn chip_text(f: &Field, recipient: &str) -> Option<String> {
    if let Some(n) = f.notice {
        return Some(n.text().to_string());
    }
    let o = f.chip()?;
    let n = f.offers().len();
    let more = if n > 1 { format!("  {}/{n}", f.selected % n + 1) } else { String::new() };
    Some(match o {
        Offer::Account(_) => format!("{} · {recipient}{more}", f.offer_label(o)),
        Offer::Generate => format!("{} · {recipient}", f.offer_label(o)),
    })
}

fn rect(x: f32, y: f32, w: f32, h: f32, c: pergola::Color, radius: f32) -> RectParams {
    RectParams { x, y, w: w.max(0.0), h: h.max(0.0), r: c.r, g: c.g, b: c.b, a: c.a, radius }
}

fn rgba(c: pergola::Color) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

/// Draw with Pergola's semantic tokens (visual language rev. 2). Light
/// until the system setting for the mode lands.
pub fn draw(conn: &mut Connection, font: u32, f: &Field, recipient: &str, hover: bool, w: f32, h: f32) -> io::Result<()> {
    let t = Semantic::LIGHT;
    let l = layout(f, w, h);
    let mut fr = conn.frame()?;
    if f.focused {
        // A 2 px accent halo, the 1 px focus ring, then the field's fill.
        fr.rect(rect(0.0, 0.0, w, h, t.accent_bg(), 6.0))?;
        fr.rect(rect(2.0, 2.0, w - 4.0, h - 4.0, t.focus_ring(), 5.0))?;
        fr.rect(rect(3.0, 3.0, w - 6.0, h - 6.0, t.bg_canvas(), 4.0))?;
    } else {
        let edge = if hover { t.border_control() } else { t.border_strong() };
        fr.rect(rect(0.0, 0.0, w, h, t.bg_canvas(), 0.0))?;
        fr.rect(rect(3.0, 3.0, w - 6.0, h - 6.0, edge, 4.0))?;
        fr.rect(rect(4.0, 4.0, w - 8.0, h - 8.0, t.bg_canvas(), 3.0))?;
    }
    let shown = f.bullets().min(l.bullets_max);
    let cy = (h - BULLET) / 2.0;
    for i in 0..shown {
        fr.rect(rect(2.0 * PAD + i as f32 * BULLET_STEP, cy, BULLET, BULLET, t.text_primary(), BULLET / 2.0))?;
    }
    if f.focused && l.chip.is_none() {
        let x = 2.0 * PAD + shown as f32 * BULLET_STEP;
        fr.rect(rect(x, h * 0.25, 1.5, h * 0.5, t.accent_fg(), 0.0))?;
    }
    if let (Some(c), Some(text)) = (l.chip, chip_text(f, recipient)) {
        let fg = if f.notice.is_some() {
            t.danger_fg()
        } else {
            fr.rect(rect(c.x, c.y, c.w, c.h, t.border_default(), 4.0))?;
            fr.rect(rect(c.x + 1.0, c.y + 1.0, c.w - 2.0, c.h - 2.0, t.bg_elevated(), 3.0))?;
            t.text_primary()
        };
        if font != 0 {
            let s = truncate(&text, c.w - 2.0 * PAD);
            fr.text_run(font, FONT_PX, c.x + PAD, c.y + c.h / 2.0 + FONT_PX * 0.35, rgba(fg), s)?;
        }
    }
    fr.finish()
}
