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

/// The font the chip's text is drawn in, and its ascender in em. frescod
/// places a text run by the top of its em box, one ascender above the
/// baseline (fresco-protocol's `TextRunInstallPayload`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Font {
    pub id: u32,
    pub ascent: f32,
}

impl Font {
    /// IBM Plex Sans's, should frescod not say.
    const ASCENT: f32 = 1.025;

    pub fn open(conn: &mut Connection) -> Font {
        match conn.font_open("system-sans") {
            Ok(f) if f.units_per_em > 0 && f.ascent_units > 0 => Font { id: f.font_id, ascent: f.ascent_units as f32 / f.units_per_em as f32 },
            Ok(f) => Font { id: f.font_id, ascent: Self::ASCENT },
            Err(_) => Font { id: 0, ascent: Self::ASCENT },
        }
    }
}

/// A bullet's size and pitch in pt: grown with the text size, but never
/// past what the parent's slot height leaves (the parent sizes the slot).
pub fn bullet(h: f32, text: f32) -> (f32, f32) {
    let size = (BULLET * text).min(((h - 2.0 * PAD) * 0.6).max(2.0));
    (size, size * BULLET_STEP / BULLET)
}

/// The chip's text size in pt: grown with the text size, but kept inside
/// the chip, whose height is the parent's slot's.
pub fn chip_font(chip: &Rect, text: f32) -> f32 {
    (FONT_PT * text).min(chip.h * 0.75)
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
        // At least 120 pt (grown with the text), but inside the field's
        // outline on the left too.
        let cw = ((w * 0.62).max(w.min(120.0 * text)) - PAD).min(w - 3.0 * PAD);
        Rect { x: w - cw - PAD, y: PAD, w: cw, h: (h - 2.0 * PAD).max(1.0) }
    });
    let right = chip.map_or(w - PAD, |c| c.x - PAD);
    let bullets_max = (((right - 2.0 * PAD) / bullet(h, text).1).floor().max(0.0)) as usize;
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

    /// A run with its baseline at `y` (pt), on a device pixel. The wire
    /// takes the top of the em box, one ascender above the baseline.
    fn text(&mut self, font: Font, size: f32, x: f32, y: f32, c: pergola::Color, s: String) -> io::Result<()> {
        let px = size * self.k;
        let top = (y * self.k).round() - font.ascent * px;
        self.fr.text_run(font.id, px, (x * self.k).round(), top, rgba(c), s).map(drop)
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
pub fn draw(conn: &mut Connection, font: Font, f: &Field, recipient: &str, look: Look, w: f32, h: f32, sc: Scale) -> io::Result<()> {
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
    let mut end = 2.0 * PAD;
    match f.shown_text().filter(|_| font.id != 0) {
        Some(s) => {
            // The end of what was typed stays in view.
            let size = (FONT_PT * tx).min(h * 0.6);
            let right = l.chip.map_or(w - PAD, |c| c.x - PAD);
            let max = (((right - 3.0 * PAD) / (CHAR_PT * size / FONT_PT)).floor().max(1.0)) as usize;
            let n = s.chars().count();
            let s: String = if n > max { std::iter::once('…').chain(s.chars().skip(n + 1 - max)).collect() } else { s };
            end += s.chars().count() as f32 * CHAR_PT * size / FONT_PT;
            px.text(font, size, 2.0 * PAD, h / 2.0 + size * 0.35, t.text_primary(), s)?;
        }
        None => {
            let shown = f.bullets().min(l.bullets_max);
            let (bullet, step) = bullet(h, tx);
            let cy = (h - bullet) / 2.0;
            for i in 0..shown {
                px.rect(2.0 * PAD + i as f32 * step, cy, bullet, bullet, t.text_primary(), bullet / 2.0)?;
            }
            end += shown as f32 * step;
        }
    }
    if f.focused && l.chip.is_none() {
        let x = end;
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
        if font.id != 0 {
            let size = chip_font(&c, tx);
            let s = truncate(&text, c.w - 2.0 * PAD, size / FONT_PT);
            px.text(font, size, c.x + PAD, c.y + c.h / 2.0 + size * 0.35, fg, s)?;
        }
    }
    drop(px);
    fr.finish()
}
