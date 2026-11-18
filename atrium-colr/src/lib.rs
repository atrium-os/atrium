//! Colour glyphs to bitmaps (docs/spec/fonts.md §6).
//!
//! A COLR glyph is a small paint graph: glyph outlines as clips, solid and
//! gradient fills, transforms, and layers composited onto each other.
//! skrifa walks the graph; this crate paints it with tiny-skia into an
//! RGBA bitmap at the size asked, which the caller puts on a colour atlas
//! (frescod, Pergola) or into a PDF as an image.
//!
//! Everything here reads untrusted font data: skrifa bounds the graph's
//! depth and cycles, and the bitmap is capped at [`MAX_PX`] a side.

use std::collections::HashMap;

use skrifa::color::{Brush, ColorPainter, ColorStop, CompositeMode, Extend, Transform as FTransform};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{BlendMode, Color, FillRule, GradientStop, LinearGradient, Mask, Paint, Path, PathBuilder, Pixmap, PixmapPaint, Point, RadialGradient, Rect, Shader, SpreadMode, Transform};

/// The largest bitmap side drawn.
pub const MAX_PX: u32 = 512;
/// The most paint operations one glyph may make (a hostile graph that
/// stays within skrifa's depth limit can still be wide).
const MAX_OPS: u32 = 10_000;

/// A colour glyph drawn: straight-alpha sRGB RGBA rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// From the pen position to the bitmap's left edge, in pixels.
    pub left: i32,
    /// From the baseline up to the bitmap's top edge, in pixels.
    pub top: i32,
    pub rgba: Vec<u8>,
}

/// Whether `font` has a colour version of `gid`.
pub fn has_color(font: &FontRef<'_>, gid: u32) -> bool {
    font.color_glyphs().get_with_format(GlyphId::new(gid), skrifa::color::ColorGlyphFormat::ColrV1).is_some()
        || font.color_glyphs().get_with_format(GlyphId::new(gid), skrifa::color::ColorGlyphFormat::ColrV0).is_some()
}

/// Whether the font file has colour glyphs at all.
pub fn is_color_font(data: &[u8]) -> bool {
    FontRef::new(data).is_ok_and(|f| f.colr().is_ok() && f.cpal().is_ok())
}

/// Draw glyph `gid` of the font in `data` at `size_px` pixels per em.
/// `foreground` (straight RGBA, 0..=1) fills what the glyph paints in "the
/// text's colour". `None` when the glyph has no colour version or draws
/// nothing.
pub fn render(data: &[u8], gid: u32, size_px: f32, foreground: [f32; 4]) -> Option<Bitmap> {
    let font = FontRef::new(data).ok()?;
    let glyph = font
        .color_glyphs()
        .get_with_format(GlyphId::new(gid), skrifa::color::ColorGlyphFormat::ColrV1)
        .or_else(|| font.color_glyphs().get_with_format(GlyphId::new(gid), skrifa::color::ColorGlyphFormat::ColrV0))?;
    if !(size_px.is_finite() && size_px > 0.0 && size_px <= MAX_PX as f32) {
        return None;
    }
    let upem = font.head().ok()?.units_per_em().max(1) as f32;
    let scale = size_px / upem;
    // Font units (y up) of the area to draw: the glyph's clip box, else
    // its advance square from the descender to the ascender.
    let bounds = match glyph.format() {
        skrifa::color::ColorGlyphFormat::ColrV1 => glyph.bounding_box(LocationRef::default(), Size::unscaled()),
        _ => None,
    };
    let (x0, y0, x1, y1) = match bounds {
        Some(b) => (b.x_min, b.y_min, b.x_max, b.y_max),
        None => {
            let m = font.metrics(Size::unscaled(), LocationRef::default());
            let adv = font.glyph_metrics(Size::unscaled(), LocationRef::default()).advance_width(GlyphId::new(gid)).unwrap_or(upem);
            (0.0, m.descent, adv, m.ascent)
        }
    };
    let left = (x0 * scale).floor() as i32 - 1;
    let top = (y1 * scale).ceil() as i32 + 1;
    let right = (x1 * scale).ceil() as i32 + 1;
    let bottom = (y0 * scale).floor() as i32 - 1;
    let (w, h) = ((right - left).max(1) as u32, (top - bottom).max(1) as u32);
    if w > MAX_PX || h > MAX_PX {
        return None;
    }
    let base = Transform::from_row(scale, 0.0, 0.0, -scale, -left as f32, top as f32);
    let mut p = Painter {
        font: &font,
        palette: palette(&font),
        foreground,
        layers: vec![(Pixmap::new(w, h)?, CompositeMode::SrcOver)],
        transforms: vec![base],
        clips: vec![None],
        paths: HashMap::new(),
        ops: 0,
        w,
        h,
    };
    glyph.paint(LocationRef::default(), &mut p).ok()?;
    let pm = p.layers.into_iter().next()?.0;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    let mut any = false;
    for px in pm.pixels() {
        let c = px.demultiply();
        any |= c.alpha() > 0;
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    any.then_some(Bitmap { width: w, height: h, left, top, rgba })
}

fn palette(font: &FontRef<'_>) -> Vec<[u8; 4]> {
    let Ok(cpal) = font.cpal() else { return Vec::new() };
    let Some(Ok(records)) = cpal.color_records_array() else { return Vec::new() };
    let first = cpal.color_record_indices().first().map_or(0, |i| i.get() as usize);
    let n = cpal.num_palette_entries() as usize;
    records.iter().skip(first).take(n).map(|r| [r.red(), r.green(), r.blue(), r.alpha()]).collect()
}

struct Painter<'a> {
    font: &'a FontRef<'a>,
    palette: Vec<[u8; 4]>,
    foreground: [f32; 4],
    layers: Vec<(Pixmap, CompositeMode)>,
    transforms: Vec<Transform>,
    clips: Vec<Option<Mask>>,
    paths: HashMap<u32, Option<Path>>,
    ops: u32,
    w: u32,
    h: u32,
}

struct Pen(PathBuilder);

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.0.quad_to(cx, cy, x, y);
    }
    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        self.0.cubic_to(c0x, c0y, c1x, c1y, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

fn tf(t: FTransform) -> Transform {
    Transform::from_row(t.xx, t.yx, t.xy, t.yy, t.dx, t.dy)
}

fn mode(m: CompositeMode) -> BlendMode {
    use CompositeMode as C;
    match m {
        C::Clear => BlendMode::Clear,
        C::Src => BlendMode::Source,
        C::Dest => BlendMode::Destination,
        C::SrcOver => BlendMode::SourceOver,
        C::DestOver => BlendMode::DestinationOver,
        C::SrcIn => BlendMode::SourceIn,
        C::DestIn => BlendMode::DestinationIn,
        C::SrcOut => BlendMode::SourceOut,
        C::DestOut => BlendMode::DestinationOut,
        C::SrcAtop => BlendMode::SourceAtop,
        C::DestAtop => BlendMode::DestinationAtop,
        C::Xor => BlendMode::Xor,
        C::Plus => BlendMode::Plus,
        C::Screen => BlendMode::Screen,
        C::Overlay => BlendMode::Overlay,
        C::Darken => BlendMode::Darken,
        C::Lighten => BlendMode::Lighten,
        C::ColorDodge => BlendMode::ColorDodge,
        C::ColorBurn => BlendMode::ColorBurn,
        C::HardLight => BlendMode::HardLight,
        C::SoftLight => BlendMode::SoftLight,
        C::Difference => BlendMode::Difference,
        C::Exclusion => BlendMode::Exclusion,
        C::Multiply => BlendMode::Multiply,
        C::HslHue => BlendMode::Hue,
        C::HslSaturation => BlendMode::Saturation,
        C::HslColor => BlendMode::Color,
        C::HslLuminosity => BlendMode::Luminosity,
        _ => BlendMode::SourceOver,
    }
}

impl Painter<'_> {
    fn color(&self, index: u16, alpha: f32) -> Color {
        let [r, g, b, a] = if index == 0xFFFF {
            self.foreground.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
        } else {
            self.palette.get(index as usize).copied().unwrap_or([0, 0, 0, 255])
        };
        Color::from_rgba8(r, g, b, (a as f32 * alpha.clamp(0.0, 1.0)).round() as u8)
    }

    fn stops(&self, s: &[ColorStop]) -> Vec<(f32, Color)> {
        s.iter().map(|c| (c.offset.clamp(0.0, 1.0), self.color(c.palette_index, c.alpha))).collect()
    }

    fn path(&mut self, gid: GlyphId) -> Option<Path> {
        let font = self.font;
        self.paths
            .entry(gid.to_u32())
            .or_insert_with(|| {
                let g = font.outline_glyphs().get(gid)?;
                let mut pen = Pen(PathBuilder::new());
                g.draw(DrawSettings::unhinted(Size::unscaled(), LocationRef::default()), &mut pen).ok()?;
                pen.0.finish()
            })
            .clone()
    }

    fn transform(&self) -> Transform {
        *self.transforms.last().unwrap_or(&Transform::identity())
    }

    fn budget(&mut self) -> bool {
        self.ops += 1;
        self.ops <= MAX_OPS
    }

    fn push_clip_path(&mut self, path: Option<Path>) {
        let mut m = self.clips.last().cloned().flatten().unwrap_or_else(|| {
            let mut m = Mask::new(self.w, self.h).expect("size checked");
            m.data_mut().fill(255);
            m
        });
        match path {
            Some(p) => m.intersect_path(&p, FillRule::Winding, true, self.transform()),
            None => m.data_mut().fill(0),
        }
        self.clips.push(Some(m));
    }
}

/// A sweep gradient's colour at `deg` (counter-clockwise from +x, font
/// space): where it falls between the start and end angles, extended as
/// `extend` says, then between the stops (straight RGBA).
fn sweep_color(stops: &[(f32, Color)], start: f32, end: f32, extend: Extend, deg: f32) -> Color {
    let span = end - start;
    let mut t = if span.abs() < 1e-6 { if deg < start { 0.0 } else { 1.0 } } else { (deg - start) / span };
    t = match extend {
        Extend::Repeat => t.rem_euclid(1.0),
        Extend::Reflect => {
            let r = t.rem_euclid(2.0);
            if r > 1.0 { 2.0 - r } else { r }
        }
        _ => t.clamp(0.0, 1.0),
    };
    let first = stops[0];
    if t <= first.0 {
        return first.1;
    }
    for pair in stops.windows(2) {
        let ((a, ca), (b, cb)) = (pair[0], pair[1]);
        if t <= b {
            let k = if b - a > 1e-6 { (t - a) / (b - a) } else { 1.0 };
            let l = |x: f32, y: f32| x + (y - x) * k;
            return Color::from_rgba(l(ca.red(), cb.red()), l(ca.green(), cb.green()), l(ca.blue(), cb.blue()), l(ca.alpha(), cb.alpha())).unwrap_or(cb);
        }
    }
    stops[stops.len() - 1].1
}

fn gs(s: &[(f32, Color)]) -> Vec<GradientStop> {
    s.iter().map(|&(o, c)| GradientStop::new(o, c)).collect()
}

fn spread(e: Extend) -> SpreadMode {
    match e {
        Extend::Repeat => SpreadMode::Repeat,
        Extend::Reflect => SpreadMode::Reflect,
        _ => SpreadMode::Pad,
    }
}

impl ColorPainter for Painter<'_> {
    fn push_transform(&mut self, t: FTransform) {
        let m = self.transform().pre_concat(tf(t));
        self.transforms.push(m);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, gid: GlyphId) {
        let p = if self.budget() { self.path(gid) } else { None };
        self.push_clip_path(p);
    }

    fn push_clip_box(&mut self, b: skrifa::raw::types::BoundingBox<f32>) {
        let p = Rect::from_ltrb(b.x_min, b.y_min, b.x_max, b.y_max).map(PathBuilder::from_rect);
        self.push_clip_path(p);
    }

    fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    fn fill(&mut self, brush: Brush<'_>) {
        if !self.budget() {
            return;
        }
        let t = self.transform();
        let shader = match brush {
            Brush::Solid { palette_index, alpha } => Shader::SolidColor(self.color(palette_index, alpha)),
            Brush::LinearGradient { p0, p1, color_stops, extend } => {
                let stops = self.stops(color_stops);
                match stops.len() {
                    0 => return,
                    1 => Shader::SolidColor(stops[0].1),
                    _ => match LinearGradient::new(Point::from_xy(p0.x, p0.y), Point::from_xy(p1.x, p1.y), gs(&stops), spread(extend), t) {
                        Some(s) => s,
                        None => Shader::SolidColor(stops[stops.len() - 1].1),
                    },
                }
            }
            Brush::RadialGradient { c0, r0: _, c1, r1, color_stops, extend } => {
                let stops = self.stops(color_stops);
                match stops.len() {
                    0 => return,
                    1 => Shader::SolidColor(stops[0].1),
                    // tiny-skia's radial gradient is two-point conical with
                    // a focal start: right for COLR's usual r0 = 0.
                    _ => match RadialGradient::new(Point::from_xy(c0.x, c0.y), Point::from_xy(c1.x, c1.y), r1, gs(&stops), spread(extend), t) {
                        Some(s) => s,
                        None => Shader::SolidColor(stops[stops.len() - 1].1),
                    },
                }
            }
            // No sweep gradients in tiny-skia: drawn here, a pixel at a
            // time, then laid on through the clip.
            Brush::SweepGradient { c0, start_angle, end_angle, color_stops, extend } => {
                let stops = self.stops(color_stops);
                if stops.is_empty() {
                    return;
                }
                let Some(inv) = t.invert() else { return };
                let Some(mut src) = Pixmap::new(self.w, self.h) else { return };
                let w = self.w as usize;
                for (i, px) in src.pixels_mut().iter_mut().enumerate() {
                    let mut p = Point::from_xy((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
                    inv.map_point(&mut p);
                    let deg = (p.y - c0.y).atan2(p.x - c0.x).to_degrees().rem_euclid(360.0);
                    let c = sweep_color(&stops, start_angle, end_angle, extend, deg);
                    *px = c.premultiply().to_color_u8();
                }
                let clip = self.clips.last().cloned().flatten();
                if let Some((pm, _)) = self.layers.last_mut() {
                    pm.draw_pixmap(0, 0, src.as_ref(), &PixmapPaint::default(), Transform::identity(), clip.as_ref());
                }
                return;
            }
        };
        let paint = Paint { shader, anti_alias: true, ..Paint::default() };
        let rect = Rect::from_xywh(0.0, 0.0, self.w as f32, self.h as f32).expect("size checked");
        let clip = self.clips.last().cloned().flatten();
        if let Some((pm, _)) = self.layers.last_mut() {
            pm.fill_rect(rect, &paint, Transform::identity(), clip.as_ref());
        }
    }

    fn push_layer(&mut self, m: CompositeMode) {
        if self.layers.len() > 64 || !self.budget() {
            // Too deep: draw into the current layer instead.
            let top = self.layers.last().map(|l| l.0.clone());
            if let Some(pm) = top {
                self.layers.push((pm, CompositeMode::Src));
            }
            return;
        }
        if let Some(pm) = Pixmap::new(self.w, self.h) {
            self.layers.push((pm, m));
        }
    }

    fn pop_layer(&mut self) {
        if self.layers.len() < 2 {
            return;
        }
        let (top, m) = self.layers.pop().expect("two layers");
        if let Some((under, _)) = self.layers.last_mut() {
            let paint = PixmapPaint { blend_mode: mode(m), ..PixmapPaint::default() };
            under.draw_pixmap(0, 0, top.as_ref(), &paint, Transform::identity(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_is_not_a_color_font() {
        assert!(!is_color_font(b"not a font"));
        assert!(render(b"not a font", 1, 32.0, [0.0, 0.0, 0.0, 1.0]).is_none());
    }

    #[test]
    fn sweep_colours_follow_the_angle() {
        let (r, b) = (Color::from_rgba8(255, 0, 0, 255), Color::from_rgba8(0, 0, 255, 255));
        let stops = [(0.0, r), (1.0, b)];
        let at = |e, d| sweep_color(&stops, 0.0, 180.0, e, d);
        assert_eq!(at(Extend::Pad, 0.0), r);
        assert_eq!(at(Extend::Pad, 180.0), b);
        let mid = at(Extend::Pad, 90.0);
        assert!((mid.red() - 0.5).abs() < 0.01 && (mid.blue() - 0.5).abs() < 0.01);
        assert_eq!(at(Extend::Pad, 270.0), b, "padded past the end");
        assert!((at(Extend::Repeat, 270.0).red() - 0.5).abs() < 0.01, "repeats");
        assert!((at(Extend::Reflect, 225.0).blue() - 0.75).abs() < 0.01, "reflects");
    }

    /// The shipped Noto Color Emoji: 😀 comes out yellow, about an em and a
    /// quarter across; an outline font is not a colour one.
    #[test]
    fn noto_color_emoji_draws_in_colour() {
        use skrifa::{FontRef, MetadataProvider};
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts/noto/NotoColorEmoji-Regular.ttf")).unwrap();
        assert!(is_color_font(&data));
        let gid = FontRef::new(&data).unwrap().charmap().map('😀').unwrap().to_u32();
        let b = render(&data, gid, 32.0, [0.0, 0.0, 0.0, 1.0]).unwrap();
        assert!((36..=46).contains(&b.width) && (36..=46).contains(&b.height), "{}x{}", b.width, b.height);
        let yellow = b.rgba.chunks(4).filter(|p| p[3] > 200 && p[0] > 200 && p[1] > 150 && p[2] < 120).count();
        assert!(yellow * 3 > (b.width * b.height) as usize, "{yellow}");
        let outline = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts/noto/NotoEmoji-Regular.ttf")).unwrap();
        assert!(!is_color_font(&outline));
    }
}
