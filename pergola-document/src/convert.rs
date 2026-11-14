//! A document scene (navigator_render's `Scene`, 1/64 px) as Pergola nodes
//! (logical px), so a band is drawn through Pergola's own surface.
//!
//! One to one where Pergola has the node: rects (rounded or ringed ones as
//! fills), shaped runs as [`Node::Glyphs`], vector paths as
//! [`Node::Fill`], linear gradients as gradient fills, images through the
//! app's resolver. Counted, not drawn yet: box shadows (Fresco has no
//! shadow op) and paint transforms other than a translation on anything
//! but a fill. Opacity groups multiply into their nodes' alpha, which is
//! exact only when a group's nodes don't overlap.

use std::sync::Arc;

use fresco_protocol::{FillPaint, FillParams, GradientStop};
use navigator_render::fontset::FontSet;
use navigator_render::{path, Attrs, Scene, Tiling, U};
use pergola::geom::{Point, Rect};
use pergola::glyphs::{Face, Glyph};
use pergola::node::{Image, Node};
use pergola::Color;

/// The document's faces registered with Pergola, by `FontSet` index.
pub struct Faces(Vec<Face>);

impl Faces {
    pub fn new(fonts: &FontSet) -> Self {
        Faces(fonts.faces.iter().map(|f| Face::new(f.bytes.clone(), 0)).collect())
    }

    /// The face registered for `FontSet` index `i`.
    pub fn get(&self, i: usize) -> Option<&Face> {
        self.0.get(i)
    }

    /// The shared set's faces, registered once per process (so every
    /// document in a window shares atlas entries).
    pub fn shared() -> Result<&'static Faces, String> {
        static F: std::sync::OnceLock<Result<Faces, String>> = std::sync::OnceLock::new();
        F.get_or_init(|| FontSet::shared().map(Faces::new)).as_ref().map_err(Clone::clone)
    }
}

/// What a conversion left out, counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Skipped {
    pub shadows: usize,
    pub images: usize,
    pub transformed: usize,
    pub clipped_runs: usize,
}

impl std::ops::AddAssign for Skipped {
    fn add_assign(&mut self, o: Self) {
        self.shadows += o.shadows;
        self.images += o.images;
        self.transformed += o.transformed;
        self.clipped_runs += o.clipped_runs;
    }
}

/// Resolves an image's content address to its pixels.
pub type Images<'a> = &'a dyn Fn(&str) -> Option<Image>;

fn px(v: U) -> f32 { v as f32 / 64.0 }

fn srgb(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// `rrggbbaa` (sRGB, straight) as Pergola's linear colour.
fn color(rgba: u32, alpha: f32) -> Color {
    Color::rgba(srgb((rgba >> 24) as u8), srgb((rgba >> 16) as u8), srgb((rgba >> 8) as u8), (rgba & 0xff) as f32 / 255.0 * alpha)
}

/// The window-pixel clip `[x0, y0, x1, y1)` and alpha a node's attrs give,
/// and its translation if its transform is one.
struct Ctx<'a> {
    scene: &'a Scene,
}

impl Ctx<'_> {
    fn clip(&self, a: Attrs) -> Option<[f32; 4]> {
        let &(x, y, w, h) = self.scene.clips.get(a.0? as usize)?;
        Some([px(x), px(y), px(x + w), px(y + h)])
    }
    fn alpha(&self, a: Attrs) -> f32 {
        let mut k = 1.0;
        let mut g = a.1;
        // Bounded: a malformed parent chain can't loop forever.
        for _ in 0..64 {
            let Some(&(alpha, parent)) = g.and_then(|g| self.scene.groups.get(g as usize)) else { break };
            k *= alpha as f32 / 255.0;
            g = parent;
        }
        k
    }
    /// The node's affine, `None` for identity.
    fn xform(&self, a: Attrs) -> Option<[f32; 6]> {
        let t = self.scene.xforms.get(a.2? as usize)?;
        let s = |v: i64| v as f32 / 65536.0;
        Some([s(t[0]), s(t[1]), s(t[2]), s(t[3]), px(t[4]), px(t[5])])
    }
}

fn is_translate(t: &[f32; 6]) -> bool {
    t[0] == 1.0 && t[1] == 0.0 && t[2] == 0.0 && t[3] == 1.0
}

/// `r` cut to `clip`, or `None` if nothing is left.
fn cut(r: Rect, clip: Option<[f32; 4]>) -> Option<Rect> {
    let Some([a, b, c, d]) = clip else { return Some(r) };
    let (l, t) = (r.x().max(a), r.y().max(b));
    let (rr, bb) = ((r.x() + r.w()).min(c), (r.y() + r.h()).min(d));
    (rr > l && bb > t).then(|| Rect::new(l, t, rr - l, bb - t))
}

/// A rounded rectangle as fill verbs and points (cubic corners).
fn round_rect(out: &mut (Vec<u8>, Vec<[f32; 2]>), x: f32, y: f32, w: f32, h: f32, r: [f32; 4]) {
    const K: f32 = 0.552_284_8;
    let m = (w.min(h) / 2.0).max(0.0);
    let [tl, tr, br, bl] = r.map(|v| v.clamp(0.0, m));
    let (v, p) = (&mut out.0, &mut out.1);
    v.push(0);
    p.push([x + tl, y]);
    v.push(1);
    p.push([x + w - tr, y]);
    if tr > 0.0 {
        v.push(3);
        p.extend([[x + w - tr + K * tr, y], [x + w, y + tr - K * tr], [x + w, y + tr]]);
    }
    v.push(1);
    p.push([x + w, y + h - br]);
    if br > 0.0 {
        v.push(3);
        p.extend([[x + w, y + h - br + K * br], [x + w - br + K * br, y + h], [x + w - br, y + h]]);
    }
    v.push(1);
    p.push([x + bl, y + h]);
    if bl > 0.0 {
        v.push(3);
        p.extend([[x + bl - K * bl, y + h], [x, y + h - bl + K * bl], [x, y + h - bl]]);
    }
    v.push(1);
    p.push([x, y + tl]);
    if tl > 0.0 {
        v.push(3);
        p.extend([[x, y + tl - K * tl], [x + tl - K * tl, y], [x + tl, y]]);
    }
    v.push(4);
}

fn fill(verbs: Vec<u8>, points: Vec<[f32; 2]>, even_odd: bool, paint: FillPaint, opacity: f32, clip: Option<[f32; 4]>, t: Option<[f32; 6]>) -> Node {
    Node::Fill(Arc::new(FillParams {
        rule: u8::from(even_odd),
        transform: t.unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
        paint,
        opacity,
        clip,
        verbs,
        points,
    }))
}

/// The CSS gradient line for `angle` (1/64 degree, clockwise from "to
/// top") across a `w × h` tile at `(x, y)`.
fn gradient_line(t: &Tiling, angle: i64) -> [f32; 4] {
    let (x, y, w, h) = (px(t.tx), px(t.ty), px(t.tw), px(t.th));
    let a = (angle as f32 / 64.0).to_radians();
    let (s, c) = a.sin_cos();
    let len = (w * s).abs() + (h * c).abs();
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (dx, dy) = (s * len / 2.0, -c * len / 2.0);
    [cx - dx, cy - dy, cx + dx, cy + dy]
}

/// Clip `nodes` to the rows `[y0, y1)` (a band's range): rects and images
/// are cut, fills take the clip, glyph runs wholly outside go. A band's
/// scene can hold a box it cuts with a height not known yet, so the clip
/// is not optional.
pub fn clip_rows(nodes: &mut Vec<Node>, y0: f32, y1: f32) {
    nodes.retain_mut(|n| match n {
        Node::Rect { rect, .. } => match cut(*rect, Some([f32::MIN, y0, f32::MAX, y1])) {
            Some(r) => {
                *rect = r;
                true
            }
            None => false,
        },
        Node::Image { rect, .. } => rect.y() < y1 && rect.y() + rect.h() > y0,
        Node::Fill(p) => {
            let c = p.clip.unwrap_or([f32::MIN, f32::MIN, f32::MAX, f32::MAX]);
            let c = [c[0], c[1].max(y0), c[2], c[3].min(y1)];
            if c[3] <= c[1] {
                return false;
            }
            Arc::make_mut(p).clip = Some(c);
            true
        }
        Node::Glyphs { origin, size, .. } => origin.y + 0.3 * *size > y0 && origin.y - *size < y1,
        _ => true,
    });
}

/// The nodes for `scene`, in its paint order.
pub fn scene_nodes(scene: &Scene, faces: &Faces, images: Images, out: &mut Vec<Node>) -> Skipped {
    let cx = Ctx { scene };
    let mut skipped = Skipped::default();
    for &(kind, i) in &scene.order {
        match kind {
            0 => {
                let r = &scene.rects[i];
                let a = scene.rect_attrs[i];
                let (clip, alpha, t) = (cx.clip(a), cx.alpha(a), cx.xform(a));
                let rect = Rect::new(px(r.x), px(r.y), px(r.w), px(r.h));
                let radii = r.radii.map(px);
                let plain = r.ring == 0 && radii.iter().all(|&v| v == radii[0]) && (radii[0] == 0.0 || clip.is_none());
                match t {
                    Some(t) if !is_translate(&t) || !plain => {
                        let mut p = (vec![], vec![]);
                        round_rect(&mut p, rect.x(), rect.y(), rect.w(), rect.h(), radii);
                        let c = color(r.rgba, alpha);
                        out.push(fill(p.0, p.1, false, FillPaint::Solid { r: c.r, g: c.g, b: c.b, a: c.a }, 1.0, clip, Some(t)));
                    }
                    t if plain => {
                        let rect = t.map_or(rect, |t| Rect::new(rect.x() + t[4], rect.y() + t[5], rect.w(), rect.h()));
                        if let Some(rect) = cut(rect, clip) {
                            out.push(Node::Rect { rect, fill: color(r.rgba, alpha), radius: radii[0] });
                        }
                    }
                    _ => {
                        // A ring, or uneven or clipped round corners: a fill.
                        let mut p = (vec![], vec![]);
                        round_rect(&mut p, rect.x(), rect.y(), rect.w(), rect.h(), radii);
                        let ring = px(r.ring);
                        if ring > 0.0 {
                            let inner = radii.map(|v| (v - ring).max(0.0));
                            round_rect(&mut p, rect.x() + ring, rect.y() + ring, rect.w() - 2.0 * ring, rect.h() - 2.0 * ring, inner);
                        }
                        let c = color(r.rgba, alpha);
                        out.push(fill(p.0, p.1, ring > 0.0, FillPaint::Solid { r: c.r, g: c.g, b: c.b, a: c.a }, 1.0, clip, None));
                    }
                }
            }
            1 => {
                let r = &scene.runs[i];
                let a = scene.run_attrs[i];
                let Some(face) = faces.0.get(r.face) else { continue };
                let mut origin = Point::new(px(r.x), px(r.y));
                if let Some(t) = cx.xform(a) {
                    if !is_translate(&t) {
                        skipped.transformed += 1;
                    }
                    origin = Point::new(origin.x + t[4], origin.y + t[5]);
                }
                let size = px(r.size);
                if let Some([x0, y0, x1, y1]) = cx.clip(a) {
                    let end = origin.x + r.glyphs.last().map_or(0.0, |g| px(g.1)) + size;
                    let (top, bottom) = (origin.y - size, origin.y + 0.3 * size);
                    if end <= x0 || origin.x >= x1 || bottom <= y0 || top >= y1 {
                        continue;
                    }
                    if origin.x < x0 || end > x1 || top < y0 || bottom > y1 {
                        skipped.clipped_runs += 1;
                    }
                }
                let glyphs: Arc<[Glyph]> = r.glyphs.iter().map(|&(id, dx, dy)| Glyph { id, dx: px(dx), dy: px(dy) }).collect();
                out.push(Node::Glyphs { origin, face: face.clone(), size, color: color(r.rgba, cx.alpha(a)), glyphs });
            }
            3 => skipped.shadows += 1,
            4 => {
                let g = &scene.grads[i];
                let a = scene.grad_attrs[i];
                let area = &g.area;
                let mut p = (vec![], vec![]);
                round_rect(&mut p, px(area.x), px(area.y), px(area.w), px(area.h), [0.0; 4]);
                let [x0, y0, x1, y1] = gradient_line(area, g.angle);
                let stops = g.stops.iter().map(|&(c, pos)| GradientStop {
                    offset: (pos as f32 / 1024.0).clamp(0.0, 1.0),
                    r: (c >> 24) as f32 / 255.0, g: ((c >> 16) & 0xff) as f32 / 255.0, b: ((c >> 8) & 0xff) as f32 / 255.0, a: (c & 0xff) as f32 / 255.0,
                }).collect();
                out.push(fill(p.0, p.1, false, FillPaint::Linear { x0, y0, x1, y1, stops }, cx.alpha(a), cx.clip(a), cx.xform(a)));
            }
            5 => {
                let im = &scene.images[i];
                let a = scene.image_attrs[i];
                let t = &im.area;
                let rect = Rect::new(px(t.tx), px(t.ty), px(t.tw), px(t.th));
                // Tiles and fit wait for a texture op with UVs: one tile.
                match (images(&im.address), cut(rect, cx.clip(a))) {
                    (Some(image), Some(_)) => out.push(Node::Image { rect, image }),
                    (None, Some(_)) => skipped.images += 1,
                    _ => {}
                }
            }
            6 => {
                let p = &scene.paths[i];
                let a = scene.path_attrs[i];
                let mut f = path::fill_params(p, cx.clip(a));
                f.opacity *= cx.alpha(a);
                out.push(Node::Fill(Arc::new(f)));
            }
            _ => {}
        }
    }
    skipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ringed_round_rect_is_an_even_odd_fill_of_two_outlines() {
        let mut p = (vec![], vec![]);
        round_rect(&mut p, 0.0, 0.0, 40.0, 20.0, [4.0; 4]);
        let one = p.0.len();
        assert_eq!(p.0.iter().filter(|&&v| v == 3).count(), 4, "four corners");
        round_rect(&mut p, 2.0, 2.0, 36.0, 16.0, [2.0; 4]);
        assert_eq!(p.0.len(), 2 * one);
        assert!(p.1.iter().all(|&[x, y]| (0.0..=40.0).contains(&x) && (0.0..=20.0).contains(&y)));
    }

    #[test]
    fn the_gradient_line_runs_corner_to_corner_at_45_degrees() {
        let t = Tiling { tx: 0, ty: 0, tw: 100 * 64, th: 100 * 64, ..Default::default() };
        let [x0, y0, x1, y1] = gradient_line(&t, 180 * 64);
        assert!((x0 - 50.0).abs() < 1e-3 && (y0 - 0.0).abs() < 1e-3 && (x1 - 50.0).abs() < 1e-3 && (y1 - 100.0).abs() < 1e-3, "to bottom");
        let [x0, y0, x1, y1] = gradient_line(&t, 45 * 64);
        assert!(x0 < 1e-3 && (y0 - 100.0).abs() < 1e-3 && (x1 - 100.0).abs() < 1e-3 && y1.abs() < 1e-3, "to top right");
    }
}
