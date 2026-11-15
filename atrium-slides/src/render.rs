//! A slide's display list ([`officina_slides::draw`]) as Pergola nodes, at a
//! scale and place on the window: the editor's canvas, the slide strip's
//! thumbnails and presenting all use it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use navigator_image::image::{decode_unaddressed, Ceilings};
use officina_slides::draw::{self, DrawItem, Paint as DPaint, Xform};
use officina_slides::model::{Dash, Deck, MediaId};
use officina_text::paint::Item;
use pergola::geom::{Point, Rect};
use pergola::glyphs::Glyph as PGlyph;
use pergola::node::{Image, Node};
use pergola::shape::{Paint, PathData, Shape, Stroke};
use pergola::Color;
use pergola_document::Faces;

/// sRGB 0xRRGGBBAA to Pergola's linear colour.
pub fn rgba(c: u32) -> Color {
    let l = |v: u32| {
        let v = v as f32 / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    Color::rgba(l(c >> 24), l((c >> 16) & 255), l((c >> 8) & 255), (c & 255) as f32 / 255.0)
}

/// sRGB 0xRRGGBB to Pergola's colour.
pub fn rgb(c: u32) -> Color {
    rgba((c << 8) | 0xFF)
}

/// Where a slide goes: `k` px per EMU, its top-left at (`x`, `y`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Place {
    pub k: f64,
    pub x: f64,
    pub y: f64,
}

impl Place {
    pub fn xform(&self) -> Xform {
        [self.k, 0.0, 0.0, self.k, self.x, self.y]
    }
    pub fn to_px(&self, x: f64, y: f64) -> (f32, f32) {
        ((self.x + x * self.k) as f32, (self.y + y * self.k) as f32)
    }
    /// Window px to slide EMU.
    pub fn to_emu(&self, x: f32, y: f32) -> (i64, i64) {
        (((x as f64 - self.x) / self.k).round() as i64, ((y as f64 - self.y) / self.k).round() as i64)
    }
}

/// A path's commands in window px: its own box mapped onto the shape's
/// (`w` × `h`), then `m`. Fresco takes coordinates within ±32,768, so EMU
/// never go on the wire; the path is placed here and drawn untransformed.
fn path_data(p: &officina_slides::model::Path, w: f64, h: f64, m: &Xform) -> PathData {
    use officina_slides::model::PathCmd;
    let sx = if p.w > 0 { w / p.w as f64 } else { 1.0 };
    let sy = if p.h > 0 { h / p.h as f64 } else { 1.0 };
    let pt = |x: i64, y: i64| {
        let (x, y) = draw::apply(m, x as f64 * sx, y as f64 * sy);
        (x as f32, y as f32)
    };
    let mut d = PathData::new();
    for c in &p.cmds {
        d = match *c {
            PathCmd::Move(x, y) => {
                let (x, y) = pt(x, y);
                d.move_to(x, y)
            }
            PathCmd::Line(x, y) => {
                let (x, y) = pt(x, y);
                d.line_to(x, y)
            }
            PathCmd::Cubic(a, b, c, e, f, g) => {
                let (a, b) = pt(a, b);
                let (c, e) = pt(c, e);
                let (f, g) = pt(f, g);
                d.cubic_to(a, b, c, e, f, g)
            }
            PathCmd::Close => d.close(),
        };
    }
    d
}

/// The paint in window px (gradient geometry through `m`).
fn paint(p: &DPaint, m: &Xform) -> Option<Paint> {
    let stops = |s: &[(f32, u32)]| s.iter().map(|(o, c)| (*o, rgba(*c))).collect();
    let pt = |x: f64, y: f64| {
        let (x, y) = draw::apply(m, x, y);
        Point::new(x as f32, y as f32)
    };
    let k = (m[0] * m[3] - m[1] * m[2]).abs().sqrt();
    Some(match p {
        DPaint::Solid(c) => Paint::Solid(rgba(*c)),
        DPaint::Linear { stops: s, from, to } => Paint::Linear { from: pt(from.0, from.1), to: pt(to.0, to.1), stops: stops(s) },
        DPaint::Radial { stops: s, center, radius } => Paint::Radial { center: pt(center.0, center.1), radius: (*radius * k) as f32, stops: stops(s) },
        DPaint::Picture(_) => return None,
    })
}

/// Decoded pictures, by deck, media and drawn size, with a byte budget.
#[derive(Default)]
struct Cache {
    map: HashMap<(u64, MediaId, u32, u32), (Option<Image>, usize, u64)>,
    bytes: usize,
    tick: u64,
}

/// Decoded bytes kept for pictures across the app.
pub const BUDGET: usize = 256 << 20;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

fn bucket(v: f64) -> u32 {
    (v.ceil().max(1.0) as u32).div_ceil(32) * 32
}

/// `media` of `deck` (known to the app as `deck_key`) decoded for drawing at
/// about `w` × `h` device px: decoded at that size, never bigger (§9.3).
pub fn picture(deck: &Deck, deck_key: u64, media: MediaId, w: f64, h: f64) -> Option<Image> {
    let (tw, th) = (bucket(w), bucket(h));
    let key = (deck_key, media, tw, th);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick += 1;
        let tick = c.tick;
        if let Some(e) = c.map.get_mut(&key) {
            e.2 = tick;
            return e.0.clone();
        }
        let bytes = deck.media_bytes(media).ok()?;
        let ceil = Ceilings { target: Some((tw, th)), ..Ceilings::default() };
        let image = decode_unaddressed(&bytes, ceil).ok().map(|px| Image::new(px.width, px.height, px.rgba));
        let n = image.as_ref().map_or(0, |i| i.data().rgba.len());
        while c.bytes + n > BUDGET && !c.map.is_empty() {
            let (&k, _) = c.map.iter().min_by_key(|(_, e)| e.2).expect("not empty");
            let e = c.map.remove(&k).expect("present");
            c.bytes -= e.1;
        }
        c.bytes += n;
        c.map.insert(key, (image.clone(), n, tick));
        image
    })
}

/// Pergola nodes for `items` placed at `at`; `scale` is the display's
/// device px per point (pictures decode for it).
pub fn nodes(deck: &Deck, deck_key: u64, items: &[DrawItem], at: Place, scale: f64, out: &mut Vec<Node>) {
    let faces = Faces::shared().ok();
    let base = at.xform();
    for it in items {
        match it {
            DrawItem::Path { xf, w, h, paths, fill, stroke } => {
                let m = draw::mul(&base, xf);
                // Window px per EMU along the shape (for stroke widths).
                let k = (m[0] * m[3] - m[1] * m[2]).abs().sqrt();
                for p in paths {
                    let data = path_data(p, *w, *h, &m);
                    let mut sh = Shape::new(data);
                    if p.filled {
                        match fill {
                            Some(DPaint::Picture(media)) => picture_box(deck, deck_key, *media, &m, *w, *h, scale, out),
                            Some(f) => {
                                if let Some(pt) = paint(f, &m) {
                                    sh = sh.fill(pt);
                                }
                            }
                            None => {}
                        }
                    }
                    if let (Some(s), true) = (stroke, p.stroked) {
                        // Hairlines stay a device pixel wide.
                        let wd = (s.width * k).max(0.75 / scale.max(1.0)) as f32;
                        let mut st = Stroke::new(Paint::Solid(rgba(s.color)), wd);
                        st.dash = match s.dash {
                            Dash::Solid => None,
                            Dash::Dot => Some((vec![wd, wd], 0.0)),
                            Dash::Dash => Some((vec![4.0 * wd, 3.0 * wd], 0.0)),
                            Dash::DashDot => Some((vec![4.0 * wd, 3.0 * wd, wd, 3.0 * wd], 0.0)),
                            Dash::LongDash => Some((vec![8.0 * wd, 3.0 * wd], 0.0)),
                        };
                        sh = sh.stroke(st);
                    }
                    out.extend(sh.fills().into_iter().map(|f| Node::Fill(Arc::new(f))));
                }
            }
            DrawItem::Picture { xf, w, h, media, .. } => picture_box(deck, deck_key, *media, &draw::mul(&base, xf), *w, *h, scale, out),
            DrawItem::Video { xf, w, h, poster, .. } => {
                let m = draw::mul(&base, xf);
                match poster {
                    Some(p) => picture_box(deck, deck_key, *p, &m, *w, *h, scale, out),
                    None => {
                        let r = bounds(&m, *w, *h);
                        out.push(Node::Rect { rect: r, fill: Color::rgba(0.0, 0.0, 0.0, 1.0), radius: 0.0 });
                    }
                }
                // A play mark in the middle.
                let r = bounds(&m, *w, *h);
                let s = (r.w().min(r.h()) * 0.18).clamp(8.0, 64.0);
                let (cx, cy) = (r.x() + r.w() / 2.0, r.y() + r.h() / 2.0);
                let tri = PathData::new().move_to(cx - s * 0.35, cy - s * 0.5).line_to(cx + s * 0.55, cy).line_to(cx - s * 0.35, cy + s * 0.5).close();
                out.extend(Shape::new(PathData::ellipse(Point::new(cx, cy), s, s)).fill(Paint::Solid(Color::rgba(0.0, 0.0, 0.0, 0.55))).fills().into_iter().map(|f| Node::Fill(Arc::new(f))));
                out.extend(Shape::new(tri).fill(Paint::Solid(Color::rgba(1.0, 1.0, 1.0, 0.95))).fills().into_iter().map(|f| Node::Fill(Arc::new(f))));
            }
            DrawItem::Text { xf, items, .. } => {
                let m = draw::mul(&base, xf);
                // Glyph runs are drawn upright at their transformed origin
                // (turned text keeps its place, not its angle, for now).
                let s = (m[0] * m[0] + m[1] * m[1]).sqrt();
                for t in items {
                    match t {
                        Item::Glyphs(r) => {
                            let Some(face) = faces.and_then(|fs| fs.get(r.face as usize)) else { continue };
                            let (x, y) = draw::apply(&m, r.x as f64, r.y as f64);
                            let run: Vec<PGlyph> = r.glyphs.iter().map(|g| PGlyph { id: g.id, dx: (g.x as f64 * s) as f32, dy: (g.dy as f64 * s) as f32 }).collect();
                            out.push(Node::Glyphs { origin: Point::new(x as f32, y as f32), face: face.clone(), size: (r.size as f64 * s) as f32, color: rgb(r.color), glyphs: Arc::from(run) });
                        }
                        Item::Rect { x, y, w, h, color } => {
                            let (px, py) = draw::apply(&m, *x as f64, *y as f64);
                            out.push(Node::Rect { rect: Rect::new(px as f32, py as f32, ((*w as f64 * s) as f32).max(1.0), ((*h as f64 * s) as f32).max(1.0)), fill: rgb(*color), radius: 0.0 });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// The axis-aligned box `0..w`, `0..h` under `m` covers.
pub fn bounds(m: &Xform, w: f64, h: f64) -> Rect {
    let pts = [draw::apply(m, 0.0, 0.0), draw::apply(m, w, 0.0), draw::apply(m, 0.0, h), draw::apply(m, w, h)];
    let (x0, x1) = pts.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
    let (y0, y1) = pts.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
    Rect::new(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32)
}

#[allow(clippy::too_many_arguments)]
fn picture_box(deck: &Deck, deck_key: u64, media: MediaId, m: &Xform, w: f64, h: f64, scale: f64, out: &mut Vec<Node>) {
    let r = bounds(m, w, h);
    match picture(deck, deck_key, media, r.w() as f64 * scale, r.h() as f64 * scale) {
        Some(image) => out.push(Node::Image { rect: r, image }),
        None => out.push(Node::Rect { rect: r, fill: Color::rgba(0.85, 0.86, 0.88, 1.0), radius: 0.0 }),
    }
}
