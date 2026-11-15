//! Folium's renderer, CPU path (folium.md §7): a display list to one
//! tile of pixels at device scale. Paths are mapped to the tile in f64
//! before they become f32, so tiles line up at any zoom (§7.1). Images
//! are decoded at the painted size and kept in an LRU under a budget.

pub mod image;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use folium_content::color::Rgba as Color;
use folium_content::{Cap, ClipKind, DisplayList, El, Item, Join, Matrix, Path, Rect, StrokeStyle, TextMode};
use folium_syntax::{Document, Ref};
use tiny_skia as sk;

/// Decoded image pixels kept per document.
pub const IMAGE_CACHE_BYTES: usize = 256 << 20;

/// How a page is shown: its crop box, rotation and scale.
#[derive(Debug, Clone, Copy)]
pub struct View {
    pub crop: Rect,
    /// 0, 90, 180 or 270.
    pub rotate: u16,
    /// Device pixels per point.
    pub scale: f64,
}

impl View {
    /// The page's size in device pixels.
    pub fn size(&self) -> (f64, f64) {
        let (w, h) = (self.crop.width() * self.scale, self.crop.height() * self.scale);
        if self.rotate % 180 == 90 { (h, w) } else { (w, h) }
    }
    /// Page space to device pixels (y down, origin top-left of the page).
    pub fn matrix(&self) -> Matrix {
        let s = self.scale;
        let (w, h) = (self.crop.width() * s, self.crop.height() * s);
        let base = Matrix::new(s, 0.0, 0.0, -s, -self.crop.x0 * s, self.crop.y1 * s);
        let rot = match self.rotate {
            90 => Matrix::new(0.0, 1.0, -1.0, 0.0, h, 0.0),
            180 => Matrix::new(-1.0, 0.0, 0.0, -1.0, w, h),
            270 => Matrix::new(0.0, -1.0, 1.0, 0.0, 0.0, w),
            _ => Matrix::IDENTITY,
        };
        base.then(&rot)
    }
}

#[derive(Default)]
struct ImageCache {
    map: HashMap<(Ref, u32, u32), (Arc<sk::Pixmap>, u64)>,
    bytes: usize,
    tick: u64,
}

/// Per-document renderer state: the image cache.
#[derive(Default)]
pub struct Renderer {
    images: Mutex<ImageCache>,
}

/// What a render did, for the dump tool and the scale gates.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub items_drawn: usize,
    pub items_culled: usize,
    pub images_decoded: usize,
    pub images_failed: usize,
}

fn to_sk(p: &Path, m: &Matrix) -> Option<sk::Path> {
    let mut b = sk::PathBuilder::with_capacity(p.els.len(), p.els.len() * 3);
    for e in &p.els {
        match *e {
            El::Move(x, y) => {
                let (x, y) = m.apply(x, y);
                b.move_to(x as f32, y as f32);
            }
            El::Line(x, y) => {
                let (x, y) = m.apply(x, y);
                b.line_to(x as f32, y as f32);
            }
            El::Cubic(a, c, d, e2, x, y) => {
                let (a, c) = m.apply(a, c);
                let (d, e2) = m.apply(d, e2);
                let (x, y) = m.apply(x, y);
                b.cubic_to(a as f32, c as f32, d as f32, e2 as f32, x as f32, y as f32);
            }
            El::Close => b.close(),
        }
    }
    b.finish()
}

fn paint(c: Color) -> sk::Paint<'static> {
    let mut p = sk::Paint::default();
    p.set_color(sk::Color::from_rgba(c.r, c.g, c.b, c.a).unwrap_or(sk::Color::BLACK));
    p.anti_alias = true;
    p
}

fn sk_matrix(m: &Matrix) -> sk::Transform { sk::Transform::from_row(m.a as f32, m.b as f32, m.c as f32, m.d as f32, m.e as f32, m.f as f32) }

fn sk_stroke(s: &StrokeStyle, device_width: bool) -> sk::Stroke {
    let mut k = sk::Stroke {
        // Width 0 is the thinnest line the device draws (§8.4.3.2).
        width: if device_width { 0.0 } else { s.width as f32 },
        miter_limit: s.miter as f32,
        line_cap: match s.cap {
            Cap::Butt => sk::LineCap::Butt,
            Cap::Round => sk::LineCap::Round,
            Cap::Square => sk::LineCap::Square,
        },
        line_join: match s.join {
            Join::Miter => sk::LineJoin::Miter,
            Join::Round => sk::LineJoin::Round,
            Join::Bevel => sk::LineJoin::Bevel,
        },
        dash: None,
    };
    if !s.dash.is_empty() {
        let mut d: Vec<f32> = s.dash.iter().map(|v| *v as f32).collect();
        if d.len() % 2 == 1 {
            d.extend(d.clone());
        }
        k.dash = sk::StrokeDash::new(d, s.phase as f32);
    }
    k
}

impl Renderer {
    pub fn new() -> Self { Self::default() }

    /// Draw the part of the page in `tile` (device pixels, page origin
    /// top-left) on white.
    pub fn render(&self, doc: &Document, dl: &DisplayList, view: &View, tile: (i32, i32, u32, u32), cancel: &dyn Fn() -> bool) -> (sk::Pixmap, Stats) {
        let (tx, ty, tw, th) = tile;
        let mut pm = sk::Pixmap::new(tw.max(1), th.max(1)).expect("tile size");
        pm.fill(sk::Color::WHITE);
        let dev = view.matrix().then(&Matrix::translate(-tx as f64, -ty as f64));
        let tile_rect = Rect::new(0.0, 0.0, tw as f64, th as f64);
        let mut st = Stats::default();
        let mut masks: HashMap<u32, Option<sk::Mask>> = HashMap::new();
        for (n, it) in dl.items.iter().enumerate() {
            if n % 512 == 0 && cancel() {
                break;
            }
            let vis = dl.visible(it).transform(&dev);
            if !vis.overlaps(&tile_rect) {
                st.items_culled += 1;
                continue;
            }
            st.items_drawn += 1;
            let clip = it.clip();
            if clip != 0 && !masks.contains_key(&clip) {
                let m = self.clip_mask(dl, clip, &dev, tw, th);
                masks.insert(clip, m);
            }
            // A clip that is a rectangle covering the tile needs no mask.
            let mask = masks.get(&clip).and_then(|m| m.as_ref());
            match it {
                Item::Fill { path, even_odd, color, .. } => {
                    // Rules drawn as thin filled rectangles cover at least
                    // one device pixel, as other readers draw them.
                    let path = &thin_rects(path, &dev);
                    if let Some(p) = to_sk(path, &Matrix::IDENTITY) {
                        let rule = if *even_odd { sk::FillRule::EvenOdd } else { sk::FillRule::Winding };
                        pm.fill_path(&p, &paint(*color), rule, sk::Transform::identity(), mask);
                    }
                }
                Item::Stroke { path, ctm, style, color, .. } => {
                    let m = ctm.then(&dev);
                    let thin = style.width * m.max_scale() < 1.0;
                    if thin {
                        // Hairlines: map the path, then stroke 1 device pixel.
                        if let Some(p) = to_sk(path, &m) {
                            pm.stroke_path(&p, &paint(*color), &sk_stroke(style, true), sk::Transform::identity(), mask);
                        }
                    } else if let Some(p) = to_sk(path, &Matrix::IDENTITY) {
                        pm.stroke_path(&p, &paint(*color), &sk_stroke(style, false), sk_matrix(&m), mask);
                    }
                }
                Item::Glyphs { font, glyphs, mode, fill, stroke, style, ctm, .. } => {
                    let mut all = Path::new();
                    for (gid, gm) in glyphs {
                        if let Some(o) = font.outline(*gid) {
                            all.extend(&o.transform(&gm.then(&dev)));
                        }
                    }
                    if let Some(p) = to_sk(&all, &Matrix::IDENTITY) {
                        if matches!(mode, TextMode::Fill | TextMode::FillStroke) {
                            pm.fill_path(&p, &paint(*fill), sk::FillRule::Winding, sk::Transform::identity(), mask);
                        }
                        if matches!(mode, TextMode::Stroke | TextMode::FillStroke) {
                            let mut s = sk_stroke(style, false);
                            s.width = (style.width * ctm.then(&dev).max_scale()).max(0.0) as f32;
                            pm.stroke_path(&p, &paint(*stroke), &s, sk::Transform::identity(), mask);
                        }
                    }
                }
                Item::Image { image, key, matrix, alpha, .. } => {
                    let m = matrix.then(&dev);
                    match self.image(doc, image, *key, &m, false, Color::BLACK) {
                        Some(img) => {
                            st.images_decoded += 1;
                            draw_image(&mut pm, &img, &m, *alpha, mask);
                        }
                        None => {
                            st.images_failed += 1;
                            placeholder(&mut pm, &m, mask);
                        }
                    }
                }
                Item::Mask { image, key, matrix, color, .. } => {
                    let m = matrix.then(&dev);
                    if let Some(img) = self.image(doc, image, *key, &m, true, *color) {
                        draw_image(&mut pm, &img, &m, 1.0, mask);
                    }
                }
            }
        }
        (pm, st)
    }

    /// The coverage mask of a clip and its parents, for this tile.
    fn clip_mask(&self, dl: &DisplayList, id: u32, dev: &Matrix, w: u32, h: u32) -> Option<sk::Mask> {
        let mut chain = Vec::new();
        let mut c = id;
        while let Some(cl) = dl.clip(c) {
            chain.push(cl);
            c = cl.parent;
            if chain.len() > 4096 {
                break;
            }
        }
        let mut mask: Option<sk::Mask> = None;
        for cl in chain.iter().rev() {
            let (p, rule) = match &cl.kind {
                ClipKind::Rect(r) => {
                    let d = r.transform(dev);
                    // Covers the whole tile: no effect.
                    if d.x0 <= 0.0 && d.y0 <= 0.0 && d.x1 >= w as f64 && d.y1 >= h as f64 {
                        continue;
                    }
                    let mut p = Path::new();
                    p.rect(r.x0, r.y0, r.width(), r.height());
                    (to_sk(&p, dev), sk::FillRule::Winding)
                }
                ClipKind::Path { path, even_odd } => (to_sk(path, dev), if *even_odd { sk::FillRule::EvenOdd } else { sk::FillRule::Winding }),
            };
            let Some(p) = p else {
                // An empty clip hides everything.
                return sk::Mask::new(w, h);
            };
            match &mut mask {
                None => {
                    let mut m = sk::Mask::new(w, h)?;
                    m.fill_path(&p, rule, true, sk::Transform::identity());
                    mask = Some(m);
                }
                Some(m) => m.intersect_path(&p, rule, true, sk::Transform::identity()),
            }
        }
        mask
    }

    /// The image at about its painted size, from the cache when it can be.
    fn image(&self, doc: &Document, st: &folium_syntax::Stream, key: Option<Ref>, m: &Matrix, stencil: bool, color: Color) -> Option<Arc<sk::Pixmap>> {
        // Painted size: the lengths of the unit square's sides on the device.
        let pw = (m.a * m.a + m.b * m.b).sqrt();
        let ph = (m.c * m.c + m.d * m.d).sqrt();
        let step = |v: f64| (v.max(1.0).min(16384.0) as u32).next_power_of_two();
        let (sw, sh) = (step(pw), step(ph));
        let ck = key.filter(|_| !stencil).map(|k| (k, sw, sh));
        if let Some(ck) = ck {
            let mut c = self.images.lock().unwrap();
            c.tick += 1;
            let t = c.tick;
            if let Some(e) = c.map.get_mut(&ck) {
                e.1 = t;
                return Some(e.0.clone());
            }
        }
        let pix = if stencil {
            let (w, h, cov) = image::decode_mask(doc, st)?;
            let mut px = Vec::with_capacity(cov.len() * 4);
            for a in cov {
                let a = a as f32 / 255.0 * color.a;
                px.extend_from_slice(&[(color.r * a * 255.0) as u8, (color.g * a * 255.0) as u8, (color.b * a * 255.0) as u8, (a * 255.0) as u8]);
            }
            sk::Pixmap::from_vec(px, sk::IntSize::from_wh(w, h)?)?
        } else {
            let img = image::decode(doc, st, sw, sh)?;
            sk::Pixmap::from_vec(img.px, sk::IntSize::from_wh(img.w, img.h)?)?
        };
        let pix = Arc::new(pix);
        if let Some(ck) = ck {
            let mut c = self.images.lock().unwrap();
            let bytes = pix.data().len();
            c.bytes += bytes;
            let t = c.tick;
            c.map.insert(ck, (pix.clone(), t));
            while c.bytes > IMAGE_CACHE_BYTES && c.map.len() > 1 {
                let Some((&k, _)) = c.map.iter().min_by_key(|(_, v)| v.1) else { break };
                if let Some((p, _)) = c.map.remove(&k) {
                    c.bytes -= p.data().len();
                }
            }
        }
        Some(pix)
    }
}

/// Map `p` to the device, widening each sub-path that is a rectangle
/// thinner than a pixel to one pixel about its centre line.
fn thin_rects(p: &Path, dev: &Matrix) -> Path {
    let d = p.transform(dev);
    let mut out = Path::new();
    let mut start = 0;
    let flush = |out: &mut Path, sub: &[El]| {
        let sp = Path { els: sub.to_vec() };
        match sp.as_rect_within(1e-6) {
            Some(r) if r.width() < 1.0 || r.height() < 1.0 => {
                let (cx, cy) = ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
                let (w, h) = (r.width().max(1.0), r.height().max(1.0));
                out.rect(cx - w / 2.0, cy - h / 2.0, w, h);
            }
            _ => out.els.extend_from_slice(sub),
        }
    };
    for i in 1..=d.els.len() {
        if i == d.els.len() || matches!(d.els[i], El::Move(..)) {
            flush(&mut out, &d.els[start..i]);
            start = i;
        }
    }
    out
}

/// The unit square through `m`; the image's row 0 is at the top (v = 1).
fn draw_image(pm: &mut sk::Pixmap, img: &sk::Pixmap, m: &Matrix, alpha: f32, mask: Option<&sk::Mask>) {
    let (w, h) = (img.width() as f64, img.height() as f64);
    let to_unit = Matrix::new(1.0 / w, 0.0, 0.0, -1.0 / h, 0.0, 1.0);
    let t = to_unit.then(m);
    let paint = sk::PixmapPaint { opacity: alpha, blend_mode: sk::BlendMode::SourceOver, quality: sk::FilterQuality::Bilinear };
    pm.draw_pixmap(0, 0, img.as_ref(), &paint, sk_matrix(&t), mask);
}

fn placeholder(pm: &mut sk::Pixmap, m: &Matrix, mask: Option<&sk::Mask>) {
    let mut p = Path::new();
    p.rect(0.0, 0.0, 1.0, 1.0);
    if let Some(sp) = to_sk(&p, m) {
        pm.fill_path(&sp, &paint(Color { r: 0.9, g: 0.9, b: 0.9, a: 1.0 }), sk::FillRule::Winding, sk::Transform::identity(), mask);
    }
}
