//! A slide's display list ([`officina_slides::draw`]) as Pergola nodes, at a
//! scale and place on the window: the editor's canvas, the slide strip's
//! thumbnails and presenting all use it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

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
    /// Full decodes asked of the workers and not back yet.
    asked: std::collections::HashSet<(u64, MediaId, u32, u32)>,
    bytes: usize,
    tick: u64,
}

/// Decoded bytes kept for pictures across the app.
pub const BUDGET: usize = 256 << 20;

/// The size key a preview's own decode is cached under.
const PREVIEW_KEY: u32 = u32::MAX;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

fn bucket(v: f64) -> u32 {
    (v.ceil().max(1.0) as u32).div_ceil(32) * 32
}

impl Cache {
    fn get(&mut self, key: &(u64, MediaId, u32, u32)) -> Option<Option<Image>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|e| {
            e.2 = tick;
            e.0.clone()
        })
    }

    fn put(&mut self, key: (u64, MediaId, u32, u32), image: Option<Image>) {
        let n = image.as_ref().map_or(0, |i| i.data().rgba.len());
        while self.bytes + n > BUDGET && !self.map.is_empty() {
            let (&k, _) = self.map.iter().min_by_key(|(_, e)| e.2).expect("not empty");
            let e = self.map.remove(&k).expect("present");
            self.bytes -= e.1;
        }
        self.tick += 1;
        self.bytes += n;
        if let Some(old) = self.map.insert(key, (image, n, self.tick)) {
            self.bytes -= old.1;
        }
    }
}

fn image_of(px: navigator_image::image::Pixels) -> Image {
    Image::new(px.width, px.height, px.rgba)
}

/// `media` of `deck` (known to the app as `deck_key`) decoded for drawing at
/// about `w` × `h` device px: decoded at that size, never bigger (§9.3).
///
/// A picture with a preview in the deck draws from it when the preview is
/// big enough (thumbnails, the sorter); when it is not, the full decode goes
/// to a worker ([`crate::decode`]) and the preview is drawn meanwhile: a
/// slide shows at once and sharpens when [`pump`] brings the decode back.
/// A picture without a preview decodes here, as it is drawn.
pub fn picture(deck: &Deck, deck_key: u64, media: MediaId, w: f64, h: f64) -> Option<Image> {
    let (tw, th) = (bucket(w), bucket(h));
    let key = (deck_key, media, tw, th);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(i) = c.get(&key) {
            return i;
        }
        if let Some((pw, ph)) = deck.preview(media) {
            let pkey = (deck_key, media, PREVIEW_KEY, PREVIEW_KEY);
            let small = match c.get(&pkey) {
                Some(i) => i,
                None => {
                    let i = deck.preview_bytes(media).and_then(|b| b.ok()).and_then(|b| crate::decode::decode(&b, pw, ph)).map(image_of);
                    c.put(pkey, i.clone());
                    i
                }
            };
            // Big enough: the preview is what this size would decode to.
            if small.is_some() && (pw >= tw || ph >= th) {
                return small;
            }
            if small.is_some() {
                if c.asked.insert(key) {
                    match deck.media_bytes(media) {
                        Ok(b) => crate::decode::want_decode(key, b),
                        Err(_) => {
                            c.asked.remove(&key);
                            c.put(key, small.clone());
                        }
                    }
                }
                return small;
            }
        }
        let image = deck.media_bytes(media).ok().and_then(|b| crate::decode::decode(&b, tw, th)).map(image_of);
        c.put(key, image.clone());
        image
    })
}

/// Brings back the decodes the workers finished: true when any did (the
/// window paints again to show them).
pub fn pump(done: Vec<crate::decode::Done>) -> bool {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let mut any = false;
        for d in done {
            if let crate::decode::Done::Decoded(key, px) = d {
                if c.asked.remove(&key) {
                    c.put(key, px.map(image_of));
                    any = true;
                }
            }
        }
        any
    })
}

thread_local! {
    /// Previews asked for and not back: (deck key, media).
    static PREVIEWS_ASKED: RefCell<std::collections::HashSet<(u64, MediaId)>> = RefCell::new(Default::default());
}

/// The window's idle work for `deck` (known as `deck_key`): brings back
/// decodes and previews, and asks for previews of the pictures without one,
/// a few at a time so decodes for the screen never wait long behind them.
/// True when the window should paint again.
pub fn tend(deck: &mut Deck, deck_key: u64) -> bool {
    let mut decoded = Vec::new();
    for d in crate::decode::take() {
        match d {
            crate::decode::Done::Preview(k, media, made) => {
                PREVIEWS_ASKED.with(|a| a.borrow_mut().remove(&(k, media)));
                if let (true, Some((mime, w, h, bytes))) = (k == deck_key, made) {
                    deck.set_preview(media, mime, w, h, &bytes);
                }
            }
            d => decoded.push(d),
        }
    }
    let repaint = pump(decoded);
    if crate::decode::pending().0 == 0 {
        let want: Vec<MediaId> = PREVIEWS_ASKED.with(|a| {
            let a = a.borrow();
            deck.pictures_without_preview().into_iter().filter(|m| !a.contains(&(deck_key, *m))).take(8).collect()
        });
        for m in want {
            if let Ok(b) = deck.media_bytes(m) {
                PREVIEWS_ASKED.with(|a| a.borrow_mut().insert((deck_key, m)));
                crate::decode::want_preview(deck_key, m, b);
            }
        }
    }
    repaint
}

/// Previews asked for and not back.
pub fn previewing() -> usize {
    PREVIEWS_ASKED.with(|a| a.borrow().len())
}

/// Full decodes asked for and not back.
pub fn decoding() -> usize {
    CACHE.with(|c| c.borrow().asked.len())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % w) as u8, (i / w) as u8, 90, alpha]).collect();
            e.write_header().unwrap().write_image_data(&px).unwrap();
        }
        out
    }

    #[test]
    fn previews_are_small_jpegs_or_pngs() {
        let (mime, w, h, b) = crate::decode::make_preview(&png(2000, 1000, 255)).unwrap();
        assert_eq!((mime, w, h), ("image/jpeg", 512, 256));
        assert!(b.len() < 100_000, "{}", b.len());
        let (mime, w, h, _) = crate::decode::make_preview(&png(150, 300, 128)).unwrap();
        assert_eq!((mime, w, h), ("image/png", 150, 300), "never enlarged; transparency kept");
        assert!(crate::decode::make_preview(b"not a picture").is_none());
    }

    #[test]
    fn a_picture_shows_its_preview_and_sharpens() {
        let mut deck = Deck::new();
        let m = deck.add_media("image/png", &png(1600, 1000, 255));
        let (mime, w, h, b) = crate::decode::make_preview(&deck.media_bytes(m).unwrap()).unwrap();
        assert!(deck.set_preview(m, mime, w, h, &b));
        let key = 0x7e57;
        // A thumbnail draws from the preview, decoding nothing else.
        let t = picture(&deck, key, m, 200.0, 125.0).unwrap();
        assert_eq!((t.data().width, t.data().height), (512, 320));
        assert_eq!(decoding(), 0);
        // Full screen: the preview at once, the full decode on a worker.
        let first = picture(&deck, key, m, 1280.0, 800.0).unwrap();
        assert_eq!(first.data().width, 512);
        assert_eq!(decoding(), 1);
        let t0 = std::time::Instant::now();
        while !pump(crate::decode::take_decoded()) {
            assert!(t0.elapsed().as_secs() < 20, "the decode never came back");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let sharp = picture(&deck, key, m, 1280.0, 800.0).unwrap();
        assert_eq!((sharp.data().width, sharp.data().height), (1280, 800));
        assert_eq!(decoding(), 0);
    }
}
