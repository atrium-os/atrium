//! Drawing the field. Bullets are rects, never glyphs, so frescod never
//! receives the typed text, not even its characters as glyph ids. The only
//! text sent is the chip's label (an account name, the app it fills for)
//! and refusals.

use std::io;

use fresco_client::Connection;
use fresco_protocol::RectParams;

use crate::field::{Field, Offer};

pub const FONT_PX: f32 = 13.0;
/// A rough advance for truncation; frescod shapes the real run.
const CHAR_PX: f32 = 7.0;
const PAD: f32 = 4.0;
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

fn rect(x: f32, y: f32, w: f32, h: f32, c: [f32; 4], radius: f32) -> RectParams {
    RectParams { x, y, w, h, r: c[0], g: c[1], b: c[2], a: c[3], radius }
}

pub fn draw(conn: &mut Connection, font: u32, f: &Field, recipient: &str, w: f32, h: f32) -> io::Result<()> {
    let l = layout(f, w, h);
    let border = if f.focused { [0.20, 0.42, 0.85, 1.0] } else { [0.62, 0.64, 0.68, 1.0] };
    let mut fr = conn.frame()?;
    fr.rect(rect(0.0, 0.0, w, h, border, 5.0))?;
    fr.rect(rect(1.5, 1.5, w - 3.0, h - 3.0, [1.0, 1.0, 1.0, 1.0], 4.0))?;
    let shown = f.bullets().min(l.bullets_max);
    let cy = (h - BULLET) / 2.0;
    for i in 0..shown {
        fr.rect(rect(2.0 * PAD + i as f32 * BULLET_STEP, cy, BULLET, BULLET, [0.12, 0.12, 0.14, 1.0], BULLET / 2.0))?;
    }
    if f.focused && l.chip.is_none() {
        let x = 2.0 * PAD + shown as f32 * BULLET_STEP;
        fr.rect(rect(x, h * 0.22, 1.5, h * 0.56, [0.20, 0.42, 0.85, 1.0], 0.0))?;
    }
    if let (Some(c), Some(text)) = (l.chip, chip_text(f, recipient)) {
        let refusal = f.notice.is_some();
        let bg = if refusal { [0.99, 0.91, 0.90, 1.0] } else { [0.90, 0.94, 1.0, 1.0] };
        let fg = if refusal { [0.65, 0.10, 0.08, 1.0] } else { [0.08, 0.20, 0.50, 1.0] };
        fr.rect(rect(c.x, c.y, c.w, c.h, bg, 4.0))?;
        if font != 0 {
            let t = truncate(&text, c.w - 2.0 * PAD);
            fr.text_run(font, FONT_PX, c.x + PAD + 2.0, c.y + c.h / 2.0 + FONT_PX * 0.35, fg, t)?;
        }
    }
    fr.finish()
}
