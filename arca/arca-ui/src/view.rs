//! Drawing the field. Bullets are rects, never glyphs, so frescod never
//! receives the typed text, not even its characters as glyph ids. The only
//! text sent is the chip's label (an account name, the app it fills for)
//! and refusals.

use std::io;

use fresco_client::{Connection, FrameBuilder};
use fresco_protocol::RectParams;
use pergola::surface::{hairline, snap_rect};
use pergola::theme::Semantic;

use crate::field::{Field, Offer};

/// The chip's text size in points at text size 1.
pub const FONT_PT: f32 = 13.0;
/// A rough advance for truncation; frescod shapes the real run.
const CHAR_PT: f32 = 7.0;
/// The field's edge sits inside the slot, leaving room for the focus halo
/// and ring (visual language rev. 2): halo 2 pt, ring 1 pt, gap 1 pt.
const PAD: f32 = 6.0;
const BULLET: f32 = 7.0;
const BULLET_STEP: f32 = 10.0;

/// Device px per pt (the window's scale) and the system text size: the
/// field lays out in points and grows its text, bullets and chip with the
/// text size (resolution-independence.md §3, §6); only what is drawn is
/// in device pixels.
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

/// Where things go in a `w` x `h` pt slot at text size `text`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub chip: Option<Rect>,
    /// Bullets that fit left of the chip.
    pub bullets_max: usize,
}

pub fn layout(f: &Field, w: f32, h: f32, text: f32) -> Layout {
    let shows = f.chip().is_some() || f.notice.is_some();
    let chip = shows.then(|| {
        let cw = (w * 0.62).max(w.min(120.0 * text)) - PAD;
        Rect { x: w - cw - PAD, y: PAD, w: cw, h: (h - 2.0 * PAD).max(1.0) }
    });
    let right = chip.map_or(w - PAD, |c| c.x - PAD);
    let bullets_max = (((right - 2.0 * PAD) / (BULLET_STEP * text)).floor().max(0.0)) as usize;
    Layout { chip, bullets_max }
}

fn truncate(s: &str, width: f32, text: f32) -> String {
    let max = (width / (CHAR_PT * text)).floor().max(1.0) as usize;
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

fn rgba(c: pergola::Color) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

/// Emits points as device pixels.
struct Px<'f, 'a> {
    fr: &'f mut FrameBuilder<'a>,
    k: f32,
}

impl Px<'_, '_> {
    /// A rect already in device pixels; `radius` in points.
    fn raw(&mut self, (x, y, w, h): (f32, f32, f32, f32), c: pergola::Color, radius: f32) -> io::Result<()> {
        self.fr.rect(RectParams { x, y, w: w.max(0.0), h: h.max(0.0), r: c.r, g: c.g, b: c.b, a: c.a, radius: radius * self.k }).map(drop)
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: pergola::Color, radius: f32) -> io::Result<()> {
        self.raw(snap_rect(x, y, w, h, self.k), c, radius)
    }

    fn text(&mut self, font: u32, size: f32, x: f32, y: f32, c: pergola::Color, s: String) -> io::Result<()> {
        self.fr.text_run(font, size * self.k, (x * self.k).round(), (y * self.k).round(), rgba(c), s).map(drop)
    }
}

/// `r` (device px) inset by `d` device px on every side.
fn inset((x, y, w, h): (f32, f32, f32, f32), d: f32) -> (f32, f32, f32, f32) {
    (x + d, y + d, w - 2.0 * d, h - 2.0 * d)
}

/// Pointer state the field's look depends on.
#[derive(Clone, Copy, Default)]
pub struct Look {
    /// The pointer is over the field.
    pub hover: bool,
    /// Focus came by keyboard: show the focus ring (rev. 2 §14.1). A
    /// click focuses without it.
    pub ring: bool,
}

/// Draw a `w` x `h` pt slot with Pergola's semantic tokens (visual language rev. 2). Light
/// until the system setting for the mode lands.
pub fn draw(conn: &mut Connection, font: u32, f: &Field, recipient: &str, look: Look, w: f32, h: f32, sc: Scale) -> io::Result<()> {
    let t = Semantic::LIGHT;
    let tx = sc.text;
    let l = layout(f, w, h, tx);
    let mut fr = conn.frame()?;
    let mut px = Px { fr: &mut fr, k: sc.k };
    // Edges in whole device pixels: the halo 2 pt, ring and border 1 pt.
    let line = hairline(sc.k);
    let halo = (2.0 * sc.k).round().max(2.0);
    let slot = snap_rect(0.0, 0.0, w, h, sc.k);
    if f.focused && look.ring {
        // The accent halo, the focus ring, then the field's fill.
        px.raw(slot, t.accent_bg(), 6.0)?;
        px.raw(inset(slot, halo), t.focus_ring(), 5.0)?;
        px.raw(inset(slot, halo + line), t.bg_canvas(), 4.0)?;
    } else {
        let edge = if look.hover || f.focused { t.border_control() } else { t.border_strong() };
        px.raw(slot, t.bg_canvas(), 0.0)?;
        px.raw(inset(slot, halo + line), edge, 4.0)?;
        px.raw(inset(slot, halo + 2.0 * line), t.bg_canvas(), 3.0)?;
    }
    let shown = f.bullets().min(l.bullets_max);
    let (bullet, step) = (BULLET * tx, BULLET_STEP * tx);
    let cy = (h - bullet) / 2.0;
    for i in 0..shown {
        px.rect(2.0 * PAD + i as f32 * step, cy, bullet, bullet, t.text_primary(), bullet / 2.0)?;
    }
    if f.focused && l.chip.is_none() {
        let x = 2.0 * PAD + shown as f32 * step;
        px.rect(x, h * 0.25, 1.5, h * 0.5, t.accent_fg(), 0.0)?;
    }
    if let (Some(c), Some(text)) = (l.chip, chip_text(f, recipient)) {
        let fg = if f.notice.is_some() {
            t.danger_fg()
        } else {
            let r = snap_rect(c.x, c.y, c.w, c.h, sc.k);
            px.raw(r, t.border_default(), 4.0)?;
            px.raw(inset(r, line), t.bg_elevated(), 3.0)?;
            t.text_primary()
        };
        if font != 0 {
            let size = FONT_PT * tx;
            let s = truncate(&text, c.w - 2.0 * PAD, tx);
            px.text(font, size, c.x + PAD, c.y + c.h / 2.0 + size * 0.35, fg, s)?;
        }
    }
    drop(px);
    fr.finish()
}
