//! Drawing the install sheet. Laid out in points, sized from the system
//! text size, drawn in device pixels at the window's scale
//! (resolution-independence.md §3, §6), as Monstrator's sheet is.

use std::io;

use fresco_client::{Connection, FrameBuilder};
use fresco_protocol::RectParams;
use pergola::surface::{hairline, snap_rect};
use pergola::theme::Semantic;

use crate::sheet::{Sheet, Tone, ROWS};

/// The sheet's width in points at text size 1.
pub const W: f32 = 460.0;
const PAD: f32 = 20.0;
const ROW_GAP: f32 = 8.0;
const TITLE_PT: f32 = 17.0;
const BODY_PT: f32 = 13.0;
const LINE: f32 = 1.6;
const ROW_H: f32 = 32.0;
const CHAR_PT: f32 = 7.0;

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

/// The text's height above the rows, for `lines` lines under the title.
fn head(lines: usize, t: f32) -> f32 {
    PAD + TITLE_PT * t + lines as f32 * BODY_PT * t * LINE + 16.0
}

/// The sheet's size in points, for `lines` lines at text size `text`.
pub fn size(lines: usize, text: f32) -> (f32, f32) {
    let t = text.max(0.5);
    let n = ROWS.len() as f32;
    ((W - 2.0 * PAD) * t + 2.0 * PAD, head(lines, t) + n * ROW_H * t + (n - 1.0) * ROW_GAP + PAD)
}

/// The rows' rects in points, top down.
pub fn rows(lines: usize, w: f32, text: f32) -> Vec<Rect> {
    let t = text.max(0.5);
    (0..ROWS.len()).map(|i| Rect { x: PAD, y: head(lines, t) + i as f32 * (ROW_H * t + ROW_GAP), w: w - 2.0 * PAD, h: ROW_H * t }).collect()
}

/// Both rows: the window's guard. Its height is fixed by `lines`, so the
/// notice (one more line) is counted in from the start: the guard never
/// moves after it is set.
pub fn guard(lines: usize, w: f32, text: f32) -> Rect {
    let r = rows(lines, w, text);
    let (a, b) = (r[0], r[r.len() - 1]);
    Rect { x: a.x, y: a.y, w: a.w, h: b.y + b.h - a.y }
}

pub fn hit(lines: usize, w: f32, text: f32, x: f32, y: f32) -> Option<usize> {
    rows(lines, w, text).iter().position(|r| r.contains(x, y))
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Font {
    pub id: u32,
    pub ascent: f32,
}

impl Font {
    pub fn open(conn: &mut Connection) -> Font {
        match conn.font_open("system-sans") {
            Ok(f) if f.units_per_em > 0 && f.ascent_units > 0 => Font { id: f.font_id, ascent: f.ascent_units as f32 / f.units_per_em as f32 },
            Ok(f) => Font { id: f.font_id, ascent: 1.025 },
            Err(_) => Font { id: 0, ascent: 1.025 },
        }
    }
}

struct Px<'f, 'a> {
    fr: &'f mut FrameBuilder<'a>,
    k: f32,
}

impl Px<'_, '_> {
    fn params(&self, x: f32, y: f32, w: f32, h: f32, c: pergola::Color, radius: f32) -> RectParams {
        RectParams { x, y, w: w.max(0.0), h: h.max(0.0), r: c.r, g: c.g, b: c.b, a: c.a, radius: radius * self.k }
    }

    fn boxed(&mut self, r: Rect, edge: pergola::Color, fill: pergola::Color, radius: f32) -> io::Result<()> {
        let (x, y, w, h) = snap_rect(r.x, r.y, r.w, r.h, self.k);
        let b = hairline(self.k);
        let p = self.params(x, y, w, h, edge, radius);
        self.fr.rect(p)?;
        let p = self.params(x + b, y + b, w - 2.0 * b, h - 2.0 * b, fill, (radius - 1.0).max(0.0));
        self.fr.rect(p).map(drop)
    }

    fn text(&mut self, font: Font, size: f32, x: f32, y: f32, c: pergola::Color, s: String) -> io::Result<()> {
        let px = size * self.k;
        let top = (y * self.k).round() - font.ascent * px;
        self.fr.text_run(font.id, px, (x * self.k).round(), top, [c.r, c.g, c.b, c.a], s).map(drop)
    }
}

/// Draw `s`, laid out for `lines` lines, in a `w` x `h` pt window at scale
/// `k` and text size `text`.
#[allow(clippy::too_many_arguments)]
pub fn draw(conn: &mut Connection, font: Font, s: &Sheet, lines: usize, w: f32, h: f32, k: f32, text: f32) -> io::Result<()> {
    let th = Semantic::LIGHT;
    let t = text.max(0.5);
    let (title_pt, body_pt) = (TITLE_PT * t, BODY_PT * t);
    let mut fr = conn.frame()?;
    let mut px = Px { fr: &mut fr, k };
    px.boxed(Rect { x: 0.0, y: 0.0, w, h }, th.border_strong(), th.bg_elevated(), 10.0)?;
    if font.id != 0 {
        let mut y = PAD + title_pt;
        px.text(font, title_pt, PAD, y, th.text_primary(), truncate(&s.title(), w - 2.0 * PAD, title_pt))?;
        for (l, tone) in s.lines().into_iter().take(lines) {
            y += body_pt * LINE;
            let c = match tone {
                Tone::Body => th.text_primary(),
                Tone::Quiet => th.text_secondary(),
                Tone::Marked => th.danger_fg(),
            };
            px.text(font, body_pt, PAD, y, c, truncate(&l, w - 2.0 * PAD, body_pt))?;
        }
    }
    for (i, (row, r)) in ROWS.iter().zip(rows(lines, w, t)).enumerate() {
        let edge = if i == s.focus { th.focus_ring() } else { th.border_default() };
        px.boxed(r, edge, th.bg_canvas(), 6.0)?;
        if font.id != 0 {
            px.text(font, body_pt, r.x + 12.0, r.y + r.h / 2.0 + body_pt * 0.35, th.text_primary(), s.label(*row).into())?;
        }
    }
    drop(px);
    fr.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_sit_under_the_text_and_clicks_hit_them() {
        for text in [0.85, 1.0, 2.0] {
            let lines = 8;
            let (w, h) = size(lines, text);
            let r = rows(lines, w, text);
            assert!(r[0].y >= PAD + TITLE_PT * text + lines as f32 * BODY_PT * text * LINE);
            assert!(r[1].y + r[1].h <= h - PAD + 0.01);
            for (i, q) in r.iter().enumerate() {
                assert_eq!(hit(lines, w, text, q.x + 3.0, q.y + q.h / 2.0), Some(i));
            }
            let g = guard(lines, w, text);
            assert!(g.contains(r[0].x + 1.0, r[0].y + 1.0) && g.contains(r[1].x + 1.0, r[1].y + r[1].h - 1.0));
        }
        assert_eq!(hit(8, W, 1.0, 1.0, 1.0), None);
    }
}
