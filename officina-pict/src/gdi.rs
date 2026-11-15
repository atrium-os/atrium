//! The device context WMF and EMF records play into: GDI's state (objects,
//! mapping, the current position, paths, clipping) over a tiny-skia canvas.

use std::f64::consts::PI;
use std::rc::Rc;

use tiny_skia::{
    BlendMode, Color, FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pattern, Pixmap, Rect, Shader, SpreadMode, Stroke, StrokeDash, Transform,
};

use crate::dib::Dib;
use crate::font;
use crate::Rgba;

/// Most objects a file may hold at once.
pub const MAX_OBJECTS: usize = 65_535;
/// Most records played.
pub const MAX_RECORDS: usize = 1 << 20;
/// Most points in one call.
pub const MAX_POINTS: usize = 1 << 20;
/// Most glyphs drawn in one picture.
const MAX_GLYPHS: usize = 1 << 18;
/// Most saved states.
const MAX_SAVED: usize = 1024;

/// An affine map: x' = a x + c y + e, y' = b x + d y + f.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aff {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Aff {
    pub const ID: Aff = Aff { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }

    /// `self`, then `n`.
    pub fn then(&self, n: &Aff) -> Aff {
        Aff {
            a: n.a * self.a + n.c * self.b,
            b: n.b * self.a + n.d * self.b,
            c: n.a * self.c + n.c * self.d,
            d: n.b * self.c + n.d * self.d,
            e: n.a * self.e + n.c * self.f + n.e,
            f: n.b * self.e + n.d * self.f + n.f,
        }
    }

    pub fn det(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    pub fn scale(&self) -> f64 {
        self.det().abs().sqrt()
    }

    pub fn finite(&self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f].iter().all(|v| v.is_finite() && v.abs() < 1e12)
    }

    fn ts(&self) -> Transform {
        Transform::from_row(self.a as f32, self.b as f32, self.c as f32, self.d as f32, self.e as f32, self.f as f32)
    }
}

#[derive(Clone, Debug)]
pub struct Pen {
    /// PS_ style bits.
    pub style: u32,
    /// Logical units; 0 is one device pixel.
    pub width: f64,
    pub color: u32,
    pub dashes: Vec<f32>,
}

#[derive(Clone)]
pub enum Brush {
    Null,
    Solid(u32),
    Hatch(u32, u32),
    Pattern(Rc<Pixmap>),
    /// A monochrome pattern: set bits in the background colour, clear
    /// ones in the text colour, as they are when it is used.
    Mono(Rc<(u32, u32, Vec<bool>)>),
}

#[derive(Clone, Debug, Default)]
pub struct LogFont {
    pub height: f64,
    pub width: f64,
    /// Tenths of a degree, counter-clockwise.
    pub escapement: f64,
    pub weight: i32,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub charset: u8,
    pub pitch: u8,
    pub face: String,
}

pub enum Obj {
    Pen(Pen),
    Brush(Brush),
    Font(LogFont),
    Palette(Rc<Vec<[u8; 4]>>),
    Other,
}

#[derive(Clone)]
pub struct Dc {
    pub pen: Pen,
    pub brush: Brush,
    pub font: LogFont,
    pub text_color: u32,
    pub bk_color: u32,
    /// 1 transparent, 2 opaque.
    pub bk_mode: u32,
    pub text_align: u32,
    pub winding: bool,
    pub rop2: u32,
    pub map_mode: u32,
    pub win_org: (f64, f64),
    pub win_ext: (f64, f64),
    pub vp_org: (f64, f64),
    pub vp_ext: (f64, f64),
    pub world: Aff,
    pub clip: Option<Rc<Mask>>,
    /// Logical.
    pub cur: (f64, f64),
    pub clockwise: bool,
    pub miter: f32,
    pub palette: Rc<Vec<[u8; 4]>>,
}

pub struct Gdi {
    pub pix: Pixmap,
    pub dc: Dc,
    saved: Vec<Dc>,
    pub objs: Vec<Option<Obj>>,
    /// Device to canvas (EMF); a WMF's window maps to the canvas itself.
    pub dev: Aff,
    pub wmf: bool,
    /// A WMF that sets no window: the box its drawing covers (origin,
    /// extent), drawn across the canvas.
    pub frame: Option<((f64, f64), (f64, f64))>,
    /// Device pixels per millimetre (EMF fixed mapping modes).
    pub dev_per_mm: (f64, f64),
    /// Canvas pixels per device pixel, for pixel-sized things (cosmetic
    /// pens, hatches).
    pub px: f64,
    path: Option<PathBuilder>,
    path_last: Option<(f32, f32)>,
    done_path: Option<Path>,
    budget: f64,
    glyphs: usize,
    xor: Xor,
}

/// Office's way of clipping a fill to a shape in a metafile: the fill drawn
/// with XOR, the shape in black over it, the fill drawn with XOR again,
/// leaving the fill inside the shape. Played as layers: the first fill, the
/// shape as a mask, the second fill dropped.
enum Xor {
    None,
    /// The first fill being drawn (the canvas set aside).
    Fill { canvas: Pixmap },
    /// The shape being drawn (the fill set aside).
    Shape { canvas: Pixmap, fill: Pixmap },
    /// The second fill, drawn nowhere.
    Again { canvas: Pixmap, fill: Pixmap, shape: Pixmap },
}

pub fn rgb(c: u32) -> Color {
    Color::from_rgba8(c as u8, (c >> 8) as u8, (c >> 16) as u8, 255)
}

pub fn to_rgba(p: Pixmap) -> Rgba {
    let (w, h) = (p.width(), p.height());
    let mut data = p.take();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Rgba { width: w, height: h, data }
}

fn pixmap_of(d: &Dib) -> Option<Pixmap> {
    let mut p = Pixmap::new(d.width, d.height)?;
    for (o, i) in p.data_mut().chunks_exact_mut(4).zip(d.rgba.chunks_exact(4)) {
        let a = i[3] as u32;
        o[0] = ((i[0] as u32 * a + 127) / 255) as u8;
        o[1] = ((i[1] as u32 * a + 127) / 255) as u8;
        o[2] = ((i[2] as u32 * a + 127) / 255) as u8;
        o[3] = i[3];
    }
    Some(p)
}

impl Gdi {
    pub fn new(width: u32, height: u32, wmf: bool) -> Option<Gdi> {
        let pix = Pixmap::new(width, height)?;
        let area = width as f64 * height as f64;
        Some(Gdi {
            pix,
            dc: Dc {
                pen: Pen { style: 0, width: 0.0, color: 0, dashes: Vec::new() },
                brush: Brush::Solid(0xFF_FFFF),
                font: LogFont { height: 12.0, weight: 400, ..Default::default() },
                text_color: 0,
                bk_color: 0xFF_FFFF,
                bk_mode: 2,
                text_align: 0,
                winding: false,
                rop2: 13,
                map_mode: 1,
                win_org: (0.0, 0.0),
                win_ext: (1.0, 1.0),
                vp_org: (0.0, 0.0),
                vp_ext: (1.0, 1.0),
                world: Aff::ID,
                clip: None,
                cur: (0.0, 0.0),
                clockwise: false,
                miter: 10.0,
                palette: Rc::new(Vec::new()),
            },
            saved: Vec::new(),
            objs: Vec::new(),
            dev: Aff::ID,
            wmf,
            frame: None,
            dev_per_mm: (3.78, 3.78),
            px: 1.0,
            path: None,
            path_last: None,
            done_path: None,
            budget: (area * 64.0).max(64e6),
            glyphs: 0,
            xor: Xor::None,
        })
    }

    // ---- mapping ----

    pub fn page(&self) -> Aff {
        let d = &self.dc;
        let nz = |v: f64| if v == 0.0 || !v.is_finite() { 1.0 } else { v };
        if self.wmf {
            let (org, ext) = match self.frame {
                Some((o, e)) => ((d.win_org.0 + o.0, d.win_org.1 + o.1), e),
                None => (d.win_org, d.win_ext),
            };
            let (sx, sy) = (self.pix.width() as f64 / nz(ext.0), self.pix.height() as f64 / nz(ext.1));
            return Aff { a: sx, b: 0.0, c: 0.0, d: sy, e: -org.0 * sx, f: -org.1 * sy };
        }
        let (sx, sy) = match d.map_mode {
            2..=6 => {
                let mm = [0.1, 0.01, 0.254, 0.0254, 25.4 / 1440.0][d.map_mode as usize - 2];
                (mm * self.dev_per_mm.0, -mm * self.dev_per_mm.1)
            }
            7 => {
                let (x, y) = (nz(d.vp_ext.0) / nz(d.win_ext.0), nz(d.vp_ext.1) / nz(d.win_ext.1));
                let s = x.abs().min(y.abs());
                (s * x.signum(), s * y.signum())
            }
            8 => (nz(d.vp_ext.0) / nz(d.win_ext.0), nz(d.vp_ext.1) / nz(d.win_ext.1)),
            _ => (1.0, 1.0),
        };
        Aff { a: sx, b: 0.0, c: 0.0, d: sy, e: d.vp_org.0 - d.win_org.0 * sx, f: d.vp_org.1 - d.win_org.1 * sy }
    }

    /// Logical to canvas.
    pub fn m(&self) -> Aff {
        self.dc.world.then(&self.page()).then(&self.dev)
    }

    pub fn pt(&self, x: f64, y: f64) -> (f32, f32) {
        let (x, y) = self.m().apply(x, y);
        (x as f32, y as f32)
    }

    // ---- objects and state ----

    pub fn add_obj(&mut self, o: Obj) {
        if let Some(slot) = self.objs.iter_mut().find(|s| s.is_none()) {
            *slot = Some(o);
        } else if self.objs.len() < MAX_OBJECTS {
            self.objs.push(Some(o));
        }
    }

    pub fn put_obj(&mut self, i: usize, o: Obj) {
        if i < MAX_OBJECTS {
            if self.objs.len() <= i {
                self.objs.resize_with(i + 1, || None);
            }
            self.objs[i] = Some(o);
        }
    }

    pub fn select(&mut self, i: usize) {
        match self.objs.get(i).and_then(|o| o.as_ref()) {
            Some(Obj::Pen(p)) => self.dc.pen = p.clone(),
            Some(Obj::Brush(b)) => self.dc.brush = b.clone(),
            Some(Obj::Font(f)) => self.dc.font = f.clone(),
            Some(Obj::Palette(p)) => self.dc.palette = p.clone(),
            _ => {}
        }
    }

    /// EMF stock objects (index with the high bit set).
    pub fn select_stock(&mut self, i: u32) {
        let pen = |c: u32, style: u32| Pen { style, width: 0.0, color: c, dashes: Vec::new() };
        match i {
            0 => self.dc.brush = Brush::Solid(0xFF_FFFF),
            1 => self.dc.brush = Brush::Solid(0xC0_C0C0),
            2 => self.dc.brush = Brush::Solid(0x80_8080),
            3 => self.dc.brush = Brush::Solid(0x40_4040),
            4 => self.dc.brush = Brush::Solid(0),
            5 => self.dc.brush = Brush::Null,
            6 => self.dc.pen = pen(0xFF_FFFF, 0),
            7 => self.dc.pen = pen(0, 0),
            8 => self.dc.pen = pen(0, 5),
            10 | 11 | 16 => self.dc.font = LogFont { height: 12.0, weight: 400, face: "Courier New".into(), ..Default::default() },
            12 | 13 | 14 | 17 => self.dc.font = LogFont { height: 12.0, weight: 400, face: "Arial".into(), ..Default::default() },
            _ => {}
        }
    }

    pub fn delete(&mut self, i: usize) {
        if let Some(s) = self.objs.get_mut(i) {
            *s = None;
        }
    }

    pub fn save(&mut self) {
        if self.saved.len() < MAX_SAVED {
            self.saved.push(self.dc.clone());
        }
    }

    /// RestoreDC: `n` < 0 is relative, > 0 an absolute level.
    pub fn restore(&mut self, n: i32) {
        let len = self.saved.len() as i64;
        let target = if n < 0 { len + n as i64 } else { n as i64 - 1 };
        if target < 0 || target >= len {
            return;
        }
        self.saved.truncate(target as usize + 1);
        if let Some(d) = self.saved.pop() {
            self.dc = d;
        }
    }

    // ---- clipping ----

    fn clip_with(&mut self, path: &Path, rule: FillRule) {
        let mask = match self.dc.clip.take() {
            Some(rc) => {
                let mut m = Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone());
                m.intersect_path(&tame(path, m.width(), m.height()), rule, false, Transform::identity());
                m
            }
            None => {
                let Some(mut m) = Mask::new(self.pix.width(), self.pix.height()) else { return };
                m.fill_path(&tame(path, m.width(), m.height()), rule, false, Transform::identity());
                m
            }
        };
        self.dc.clip = Some(Rc::new(mask));
    }

    fn quad(&self, x0: f64, y0: f64, x1: f64, y1: f64, m: &Aff) -> Option<Path> {
        let mut pb = PathBuilder::new();
        let p = |x, y| {
            let (x, y) = m.apply(x, y);
            (x as f32, y as f32)
        };
        let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x1, y1), p(x0, y1));
        pb.move_to(a.0, a.1);
        pb.line_to(b.0, b.1);
        pb.line_to(c.0, c.1);
        pb.line_to(d.0, d.1);
        pb.close();
        pb.finish()
    }

    /// IntersectClipRect, logical.
    pub fn clip_rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        match self.quad(x0, y0, x1, y1, &self.m()) {
            Some(p) => self.clip_with(&p, FillRule::Winding),
            None => self.clip_none(),
        }
    }

    /// ExcludeClipRect, logical.
    pub fn exclude_rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        let Some(inner) = self.quad(x0, y0, x1, y1, &self.m()) else { return };
        let (w, h) = (self.pix.width() as f32, self.pix.height() as f32);
        let mut pb = PathBuilder::new();
        pb.push_rect(Rect::from_ltrb(-1.0, -1.0, w + 1.0, h + 1.0).unwrap());
        pb.push_path(&inner);
        if let Some(p) = pb.finish() {
            self.clip_with(&p, FillRule::EvenOdd);
        }
    }

    /// Nothing drawn until the clip is reset.
    pub fn clip_none(&mut self) {
        self.dc.clip = Mask::new(self.pix.width(), self.pix.height()).map(Rc::new);
    }

    /// A region (rectangles in `space` units) as the clip; `mode` RGN_AND 1,
    /// RGN_OR 2, RGN_COPY 5; no rectangles with RGN_COPY resets it.
    pub fn clip_region(&mut self, rects: &[(f64, f64, f64, f64)], mode: u32, space: &Aff) {
        if rects.is_empty() {
            if mode == 5 {
                self.dc.clip = None;
            }
            return;
        }
        let mut pb = PathBuilder::new();
        for &(l, t, r, b) in rects.iter().take(10_000) {
            if let Some(p) = self.quad(l, t, r, b, space) {
                pb.push_path(&p);
            }
        }
        let Some(path) = pb.finish() else { return };
        match mode {
            1 => self.clip_with(&path, FillRule::Winding),
            2 => {
                if let Some(rc) = self.dc.clip.take() {
                    let mut m = Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone());
                    m.fill_path(&tame(&path, m.width(), m.height()), FillRule::Winding, false, Transform::identity());
                    self.dc.clip = Some(Rc::new(m));
                }
            }
            _ => {
                self.dc.clip = None;
                self.clip_with(&path, FillRule::Winding);
            }
        }
    }

    /// SelectClipPath with the path just ended.
    pub fn clip_path(&mut self, mode: u32) {
        if let Some(p) = self.done_path.take() {
            let rule = if self.dc.winding { FillRule::Winding } else { FillRule::EvenOdd };
            if mode == 5 {
                self.dc.clip = None;
            }
            self.clip_with(&p, rule);
        }
    }

    // ---- paths ----

    pub fn begin_path(&mut self) {
        self.path = Some(PathBuilder::new());
        self.path_last = None;
        self.done_path = None;
    }

    pub fn end_path(&mut self) {
        if let Some(pb) = self.path.take() {
            self.done_path = pb.finish();
        }
    }

    pub fn abort_path(&mut self) {
        self.path = None;
        self.done_path = None;
    }

    pub fn close_figure(&mut self) {
        if let Some(pb) = &mut self.path {
            pb.close();
        }
    }

    pub fn draw_path(&mut self, fill: bool, stroke: bool) {
        if let Some(p) = self.done_path.take() {
            if fill {
                self.fill(&p);
            }
            if stroke {
                self.stroke(&p);
            }
        }
    }

    // ---- drawing ----

    fn blank(&self) -> Pixmap {
        Pixmap::new(self.pix.width(), self.pix.height()).expect("same size as the canvas")
    }

    /// Moves between the XOR layers as the raster op changes.
    fn xor_step(&mut self) {
        let xor = self.dc.rop2 == 7;
        if matches!((&self.xor, xor), (Xor::None, false) | (Xor::Fill { .. } | Xor::Again { .. }, true) | (Xor::Shape { .. }, false)) {
            return;
        }
        let blank = self.blank();
        self.xor = match (std::mem::replace(&mut self.xor, Xor::None), xor) {
            (Xor::None, true) => Xor::Fill { canvas: std::mem::replace(&mut self.pix, blank) },
            (Xor::Fill { canvas }, false) => Xor::Shape { canvas, fill: std::mem::replace(&mut self.pix, blank) },
            (Xor::Shape { canvas, fill }, true) => Xor::Again { canvas, fill, shape: std::mem::replace(&mut self.pix, blank) },
            (Xor::Again { canvas, fill, shape }, false) => {
                self.pix = canvas;
                self.xor_done(fill, Some(shape));
                Xor::None
            }
            (x, _) => x,
        };
    }

    fn xor_done(&mut self, fill: Pixmap, shape: Option<Pixmap>) {
        let mask = shape.map(|s| Mask::from_pixmap(s.as_ref(), tiny_skia::MaskType::Alpha));
        self.pix.draw_pixmap(0, 0, fill.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), mask.as_ref());
    }

    /// The canvas, with any XOR layers still open put down.
    pub fn finish(mut self) -> Pixmap {
        match std::mem::replace(&mut self.xor, Xor::None) {
            Xor::None => {}
            Xor::Fill { canvas } => {
                let fill = std::mem::replace(&mut self.pix, canvas);
                self.xor_done(fill, None);
            }
            Xor::Shape { canvas, fill } => {
                let shape = std::mem::replace(&mut self.pix, canvas);
                self.xor_done(fill, None);
                self.xor_done(shape, None);
            }
            Xor::Again { canvas, fill, shape } => {
                self.pix = canvas;
                self.xor_done(fill, Some(shape));
            }
        }
        self.pix
    }

    fn spend(&mut self, p: &Path) -> bool {
        self.xor_step();
        let b = p.bounds();
        let (w, h) = (self.pix.width() as f32, self.pix.height() as f32);
        let area = ((b.right().min(w) - b.left().max(0.0)).max(1.0) * (b.bottom().min(h) - b.top().max(0.0)).max(1.0)) as f64;
        self.budget -= area;
        self.budget > 0.0 && b.right() >= 0.0 && b.bottom() >= 0.0 && b.left() <= w && b.top() <= h
    }

    fn brush_paint<'a>(&self, brush: &'a Brush) -> Option<Paint<'a>> {
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        match brush {
            Brush::Null => return None,
            Brush::Solid(c) => paint.set_color(rgb(*c)),
            Brush::Hatch(..) | Brush::Mono(_) => unreachable!("made a pattern first"),
            Brush::Pattern(p) => {
                paint.shader = Pattern::new(p.as_ref().as_ref(), SpreadMode::Repeat, FilterQuality::Nearest, 1.0, Transform::from_scale(self.px as f32, self.px as f32));
            }
        }
        Some(paint)
    }

    fn hatch(&self, style: u32, color: u32) -> Brush {
        let Some(mut p) = Pixmap::new(8, 8) else { return Brush::Solid(color) };
        if self.dc.bk_mode == 2 {
            p.fill(rgb(self.dc.bk_color));
        }
        let c = rgb(color).premultiply().to_color_u8();
        for y in 0..8u32 {
            for x in 0..8u32 {
                let on = match style {
                    0 => y == 4,
                    1 => x == 4,
                    2 => x == y,
                    3 => x + y == 7,
                    4 => x == 4 || y == 4,
                    _ => x == y || x + y == 7,
                };
                if on {
                    let i = ((y * 8 + x) * 4) as usize;
                    p.data_mut()[i..i + 4].copy_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
                }
            }
        }
        Brush::Pattern(Rc::new(p))
    }

    fn mono(&self, m: &(u32, u32, Vec<bool>)) -> Brush {
        let Some(mut p) = Pixmap::new(m.0, m.1) else { return Brush::Solid(self.dc.text_color) };
        let (on, off) = (rgb(self.dc.bk_color).premultiply().to_color_u8(), rgb(self.dc.text_color).premultiply().to_color_u8());
        for (o, &bit) in p.data_mut().chunks_exact_mut(4).zip(m.2.iter()) {
            let c = if bit { on } else { off };
            o.copy_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        Brush::Pattern(Rc::new(p))
    }

    pub fn fill(&mut self, p: &Path) {
        if let Some(pb) = &mut self.path {
            pb.push_path(p);
            return;
        }
        let brush = match &self.dc.brush {
            Brush::Hatch(s, c) => self.hatch(*s, *c),
            Brush::Mono(m) => self.mono(m),
            b => b.clone(),
        };
        if matches!(brush, Brush::Null) || self.dc.rop2 == 11 || !self.spend(p) {
            return;
        }
        let Some(paint) = self.brush_paint(&brush) else { return };
        let rule = if self.dc.winding { FillRule::Winding } else { FillRule::EvenOdd };
        let clip = self.dc.clip.clone();
        self.pix.fill_path(&tame(p, self.pix.width(), self.pix.height()), &paint, rule, Transform::identity(), clip.as_deref());
    }

    pub fn stroke(&mut self, p: &Path) {
        if let Some(pb) = &mut self.path {
            pb.push_path(p);
            return;
        }
        if self.dc.pen.style & 0xF == 5 || self.dc.rop2 == 11 || !self.spend(p) {
            return;
        }
        let pen = self.dc.pen.clone();
        let geometric = pen.style & 0x0001_0000 != 0 || pen.width > 0.0;
        let w = if pen.width > 0.0 { (pen.width * self.m().scale()).max(1.0) } else { 1.0_f64.max(self.px * 0.75) } as f32;
        let mut stroke = Stroke { width: w, miter_limit: self.dc.miter.max(1.0), ..Default::default() };
        // Cosmetic pens and GDI's default geometric pen: round.
        stroke.line_cap = match pen.style & 0xF00 {
            0x100 => LineCap::Square,
            0x200 => LineCap::Butt,
            _ => LineCap::Round,
        };
        stroke.line_join = match pen.style & 0xF000 {
            0x1000 => LineJoin::Bevel,
            0x2000 => LineJoin::Miter,
            _ => LineJoin::Round,
        };
        let (unit, pattern): (f32, &[f32]) = match (pen.style & 0xF, geometric) {
            (1, true) => (w, &[3.0, 1.0]),
            (2, true) => (w, &[1.0, 1.0]),
            (3, true) => (w, &[3.0, 1.0, 1.0, 1.0]),
            (4, true) => (w, &[3.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
            (1, false) => (self.px as f32, &[18.0, 6.0]),
            (2, false) => (self.px as f32, &[3.0, 3.0]),
            (3, false) => (self.px as f32, &[9.0, 6.0, 3.0, 6.0]),
            (4, false) => (self.px as f32, &[9.0, 3.0, 3.0, 3.0, 3.0, 3.0]),
            // User styles: canvas pixels already.
            (7, _) => (1.0, &pen.dashes),
            _ => (1.0, &[]),
        };
        if pattern.len() >= 2 && pattern.len() % 2 == 0 {
            stroke.dash = StrokeDash::new(pattern.iter().map(|d| (d * unit).max(0.5)).collect(), 0.0);
            if geometric {
                stroke.line_cap = LineCap::Butt;
            }
        }
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        paint.set_color(rgb(match self.dc.rop2 {
            1 => 0,
            16 => 0xFF_FFFF,
            _ => pen.color,
        }));
        let clip = self.dc.clip.clone();
        self.pix.stroke_path(&tame(p, self.pix.width(), self.pix.height()), &paint, &stroke, Transform::identity(), clip.as_deref());
    }

    /// Points (logical) as a path.
    pub fn poly(&self, pts: &[(f64, f64)], close: bool) -> Option<Path> {
        let m = self.m();
        let mut pb = PathBuilder::new();
        for (i, &(x, y)) in pts.iter().enumerate() {
            let (x, y) = m.apply(x, y);
            if i == 0 {
                pb.move_to(x as f32, y as f32);
            } else {
                pb.line_to(x as f32, y as f32);
            }
        }
        if close {
            pb.close();
        }
        pb.finish()
    }

    pub fn polygon(&mut self, polys: &[Vec<(f64, f64)>]) {
        let mut pb = PathBuilder::new();
        for p in polys {
            if p.len() >= 2 {
                if let Some(p) = self.poly(p, true) {
                    pb.push_path(&p);
                }
            }
        }
        if let Some(p) = pb.finish() {
            self.fill(&p);
            if self.path.is_none() {
                self.stroke(&p);
            }
        }
    }

    pub fn polyline(&mut self, pts: &[(f64, f64)]) {
        if pts.len() < 2 {
            return;
        }
        if let Some(p) = self.poly(pts, false) {
            self.stroke(&p);
        }
    }

    /// Lines or curves from the current position (…To calls).
    pub fn to(&mut self, pts: &[(f64, f64)], bezier: bool) {
        let Some(&last) = pts.last() else { return };
        let m = self.m();
        let c = m.apply(self.dc.cur.0, self.dc.cur.1);
        let mut pb = PathBuilder::new();
        pb.move_to(c.0 as f32, c.1 as f32);
        let p = |&(x, y): &(f64, f64)| {
            let (x, y) = m.apply(x, y);
            (x as f32, y as f32)
        };
        if bezier {
            for t in pts.chunks_exact(3) {
                let (a, b, c) = (p(&t[0]), p(&t[1]), p(&t[2]));
                pb.cubic_to(a.0, a.1, b.0, b.1, c.0, c.1);
            }
        } else {
            for q in pts {
                let q = p(q);
                pb.line_to(q.0, q.1);
            }
        }
        self.dc.cur = last;
        let Some(path) = pb.finish() else { return };
        if let Some(rec) = &mut self.path {
            // Continue the figure the path is on.
            let start = (c.0 as f32, c.1 as f32);
            let mut it = path.segments();
            if self.path_last != Some(start) {
                rec.move_to(start.0, start.1);
            }
            it.next();
            for s in it {
                match s {
                    tiny_skia::PathSegment::LineTo(p) => rec.line_to(p.x, p.y),
                    tiny_skia::PathSegment::CubicTo(a, b, p) => rec.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y),
                    _ => {}
                }
            }
            let l = m.apply(last.0, last.1);
            self.path_last = Some((l.0 as f32, l.1 as f32));
        } else {
            self.stroke(&path);
        }
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        self.dc.cur = (x, y);
        let (px, py) = self.m().apply(x, y);
        if let Some(pb) = &mut self.path {
            pb.move_to(px as f32, py as f32);
            self.path_last = Some((px as f32, py as f32));
        }
    }

    pub fn bezier(&mut self, pts: &[(f64, f64)]) {
        if pts.len() < 4 {
            return;
        }
        let m = self.m();
        let p = |&(x, y): &(f64, f64)| {
            let (x, y) = m.apply(x, y);
            (x as f32, y as f32)
        };
        let mut pb = PathBuilder::new();
        let s = p(&pts[0]);
        pb.move_to(s.0, s.1);
        for t in pts[1..].chunks_exact(3) {
            let (a, b, c) = (p(&t[0]), p(&t[1]), p(&t[2]));
            pb.cubic_to(a.0, a.1, b.0, b.1, c.0, c.1);
        }
        if let Some(path) = pb.finish() {
            self.stroke(&path);
        }
    }

    pub fn rect(&mut self, l: f64, t: f64, r: f64, b: f64) {
        if let Some(p) = self.quad(l, t, r, b, &self.m()) {
            self.fill(&p);
            self.stroke(&p);
        }
    }

    /// An elliptic arc of the box (logical) from angle `a0`, sweeping `sw`
    /// (radians, y down: positive is anticlockwise on the page), appended
    /// to `pb` in canvas space.
    fn arc_into(&self, pb: &mut PathBuilder, bx: (f64, f64, f64, f64), a0: f64, sw: f64, first: bool) {
        let m = self.m();
        let (cx, cy, rx, ry) = ((bx.0 + bx.2) / 2.0, (bx.1 + bx.3) / 2.0, (bx.2 - bx.0).abs() / 2.0, (bx.3 - bx.1).abs() / 2.0);
        let at = |t: f64| (cx + rx * t.cos(), cy - ry * t.sin());
        let d = |t: f64| (-rx * t.sin(), -ry * t.cos());
        let n = ((sw.abs() / (PI / 2.0)).ceil() as usize).clamp(1, 8);
        let step = sw / n as f64;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let c = |p: (f64, f64)| {
            let (x, y) = m.apply(p.0, p.1);
            (x as f32, y as f32)
        };
        let s = c(at(a0));
        if first {
            pb.move_to(s.0, s.1);
        } else {
            pb.line_to(s.0, s.1);
        }
        for i in 0..n {
            let (t1, t2) = (a0 + step * i as f64, a0 + step * (i + 1) as f64);
            let (p1, d1, p2, d2) = (at(t1), d(t1), at(t2), d(t2));
            let a = c((p1.0 + k * d1.0, p1.1 + k * d1.1));
            let b = c((p2.0 - k * d2.0, p2.1 - k * d2.1));
            let e = c(p2);
            pb.cubic_to(a.0, a.1, b.0, b.1, e.0, e.1);
        }
    }

    pub fn ellipse(&mut self, l: f64, t: f64, r: f64, b: f64) {
        let mut pb = PathBuilder::new();
        self.arc_into(&mut pb, (l, t, r, b), 0.0, 2.0 * PI, true);
        pb.close();
        if let Some(p) = pb.finish() {
            self.fill(&p);
            self.stroke(&p);
        }
    }

    pub fn round_rect(&mut self, l: f64, t: f64, r: f64, b: f64, cw: f64, ch: f64) {
        let (l, r) = (l.min(r), l.max(r));
        let (t, b) = (t.min(b), t.max(b));
        let (cw, ch) = (cw.abs().min(r - l), ch.abs().min(b - t));
        if cw <= 0.0 || ch <= 0.0 {
            return self.rect(l, t, r, b);
        }
        let mut pb = PathBuilder::new();
        self.arc_into(&mut pb, (r - cw, t, r, t + ch), 0.0, PI / 2.0, true);
        self.arc_into(&mut pb, (l, t, l + cw, t + ch), PI / 2.0, PI / 2.0, false);
        self.arc_into(&mut pb, (l, b - ch, l + cw, b), PI, PI / 2.0, false);
        self.arc_into(&mut pb, (r - cw, b - ch, r, b), 1.5 * PI, PI / 2.0, false);
        pb.close();
        if let Some(p) = pb.finish() {
            self.fill(&p);
            self.stroke(&p);
        }
    }

    /// Arc (0), chord (1), pie (2), arc-to (3): the box and the radial
    /// points, logical.
    pub fn arc(&mut self, bx: (f64, f64, f64, f64), s: (f64, f64), e: (f64, f64), kind: u8) {
        let (cx, cy) = ((bx.0 + bx.2) / 2.0, (bx.1 + bx.3) / 2.0);
        let (rx, ry) = (((bx.2 - bx.0) / 2.0).abs().max(1e-9), ((bx.3 - bx.1) / 2.0).abs().max(1e-9));
        let ang = |p: (f64, f64)| (-(p.1 - cy) / ry).atan2((p.0 - cx) / rx);
        let (a0, a1) = (ang(s), ang(e));
        // GDI draws anticlockwise on the device unless told otherwise.
        let flip = self.m().det() < 0.0;
        let cw = self.dc.clockwise != flip;
        let mut sw = a1 - a0;
        if cw {
            if sw >= 0.0 {
                sw -= 2.0 * PI;
            }
        } else if sw <= 0.0 {
            sw += 2.0 * PI;
        }
        let mut pb = PathBuilder::new();
        if kind == 2 {
            let c = self.pt(cx, cy);
            pb.move_to(c.0, c.1);
            self.arc_into(&mut pb, bx, a0, sw, false);
            pb.close();
        } else if kind == 3 {
            let c = self.pt(self.dc.cur.0, self.dc.cur.1);
            pb.move_to(c.0, c.1);
            self.arc_into(&mut pb, bx, a0, sw, false);
            let end = a0 + sw;
            self.dc.cur = (cx + rx * end.cos(), cy - ry * end.sin());
        } else {
            self.arc_into(&mut pb, bx, a0, sw, true);
            if kind == 1 {
                pb.close();
            }
        }
        if let Some(p) = pb.finish() {
            if kind == 1 || kind == 2 {
                self.fill(&p);
            }
            self.stroke(&p);
        }
    }

    /// PatBlt-style fills of a logical rectangle.
    pub fn pat_blt(&mut self, l: f64, t: f64, w: f64, h: f64, rop: u32) {
        let Some(p) = self.quad(l, t, l + w, t + h, &self.m()) else { return };
        let saved = (self.dc.brush.clone(), self.dc.pen.clone());
        match rop {
            0x00F0_0021 => {}
            0x0000_0042 => self.dc.brush = Brush::Solid(0),
            0x00FF_0062 => self.dc.brush = Brush::Solid(0xFF_FFFF),
            _ => return,
        }
        self.fill(&p);
        (self.dc.brush, self.dc.pen) = saved;
    }

    /// A bitmap's `src` part (pixels) into the logical rectangle `dst`.
    pub fn blit(&mut self, dib: Dib, src: (f64, f64, f64, f64), dst: (f64, f64, f64, f64), rop: u32, opacity: f32) {
        if self.path.is_some() {
            return;
        }
        let Some(mut img) = pixmap_of(&dib) else { return };
        drop(dib);
        match rop {
            // SRCAND: white leaves the page alone.
            0x0088_00C6 => {
                for p in img.data_mut().chunks_exact_mut(4) {
                    if p[0] == 255 && p[1] == 255 && p[2] == 255 {
                        p.fill(0);
                    }
                }
            }
            // SRCPAINT, SRCINVERT: black does.
            0x00EE_0086 | 0x0066_0046 => {
                for p in img.data_mut().chunks_exact_mut(4) {
                    if p[0] == 0 && p[1] == 0 && p[2] == 0 {
                        p.fill(0);
                    }
                }
            }
            // NOTSRCCOPY.
            0x0033_0008 => {
                for p in img.data_mut().chunks_exact_mut(4) {
                    let a = p[3];
                    for c in &mut p[..3] {
                        *c = a - (*c).min(a);
                    }
                }
            }
            0x00CC_0020 | 0x00C0_00CA | 0x00B8_074A | 0x00E2_0746 => {}
            _ => return,
        }
        let (sx, sy, sw, sh) = src;
        let (dx, dy, dw, dh) = dst;
        if sw == 0.0 || sh == 0.0 || dw == 0.0 || dh == 0.0 {
            return;
        }
        let m = self.m();
        let Some(area) = self.quad(dx, dy, dx + dw, dy + dh, &m) else { return };
        if !self.spend(&area) {
            return;
        }
        let place = Aff { a: dw / sw, b: 0.0, c: 0.0, d: dh / sh, e: dx - sx * dw / sw, f: dy - sy * dh / sh }.then(&m);
        if !place.finite() {
            return;
        }
        // Clip to the source rectangle.
        let (iw, ih) = (img.width() as f64, img.height() as f64);
        let (x0, y0, x1, y1) = (sx.max(0.0), sy.max(0.0), (sx + sw).min(iw), (sy + sh).min(ih));
        let Some(area) = (if x1 > x0 && y1 > y0 { self.quad(x0, y0, x1, y1, &place) } else { None }) else { return };
        let up = place.scale() > 1.5;
        let mut paint = Paint { anti_alias: false, ..Default::default() };
        paint.shader = Pattern::new(img.as_ref(), SpreadMode::Pad, if up { FilterQuality::Nearest } else { FilterQuality::Bilinear }, opacity, place.ts());
        paint.blend_mode = BlendMode::SourceOver;
        let clip = self.dc.clip.clone();
        self.pix.fill_path(&tame(&area, self.pix.width(), self.pix.height()), &paint, FillRule::Winding, Transform::identity(), clip.as_deref());
    }

    /// A linear gradient across a logical rectangle (GradientFill).
    pub fn gradient_rect(&mut self, (l, t, r, b): (f64, f64, f64, f64), c0: Color, c1: Color, vertical: bool) {
        let m = self.m();
        let Some(area) = self.quad(l, t, r, b, &m) else { return };
        if !self.spend(&area) {
            return;
        }
        let s = m.apply(l, t);
        let e = if vertical { m.apply(l, b) } else { m.apply(r, t) };
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        paint.shader = tiny_skia::LinearGradient::new(
            tiny_skia::Point::from_xy(s.0 as f32, s.1 as f32),
            tiny_skia::Point::from_xy(e.0 as f32, e.1 as f32),
            vec![tiny_skia::GradientStop::new(0.0, c0), tiny_skia::GradientStop::new(1.0, c1)],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .unwrap_or(Shader::SolidColor(c0));
        let clip = self.dc.clip.clone();
        self.pix.fill_path(&tame(&area, self.pix.width(), self.pix.height()), &paint, FillRule::Winding, Transform::identity(), clip.as_deref());
    }

    pub fn fill_with(&mut self, p: &Path, color: Color) {
        if !self.spend(p) {
            return;
        }
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        paint.set_color(color);
        let clip = self.dc.clip.clone();
        self.pix.fill_path(&tame(p, self.pix.width(), self.pix.height()), &paint, FillRule::Winding, Transform::identity(), clip.as_deref());
    }

    // ---- text ----

    /// Draws `text` at logical `(x, y)` (or the current position, with
    /// TA_UPDATECP). `dx`: logical advances, one per character. `opaque`:
    /// a logical rectangle filled with the background first; `clip`: one
    /// the text is clipped to.
    pub fn text(&mut self, x: f64, y: f64, text: &[u32], dx: Option<&[f64]>, opaque: Option<(f64, f64, f64, f64)>, clip: Option<(f64, f64, f64, f64)>) {
        let saved_clip = self.dc.clip.clone();
        if let Some((l, t, r, b)) = clip {
            self.clip_rect(l, t, r, b);
        }
        if let Some((l, t, r, b)) = opaque {
            if let Some(p) = self.quad(l, t, r, b, &self.m()) {
                self.fill_with(&p, rgb(self.dc.bk_color));
            }
        }
        self.draw_text(x, y, text, dx);
        self.dc.clip = saved_clip;
    }

    fn draw_text(&mut self, x: f64, y: f64, text: &[u32], dx: Option<&[f64]>) {
        if text.is_empty() || self.glyphs >= MAX_GLYPHS {
            return;
        }
        let f = self.dc.font.clone();
        let sym = font::symbols(&f.face, f.charset);
        if matches!(sym, font::Symbols::Dingbats) {
            return;
        }
        let Some(face) = font::face(font::pick(&f.face, f.weight, f.pitch)) else { return };
        let upem = face.units_per_em().max(1) as f64;
        let (asc, desc) = match face.tables().os2 {
            Some(o) => (o.windows_ascender() as f64, -(o.windows_descender() as f64).abs()),
            None => (face.ascender() as f64, face.descender() as f64),
        };
        let em = if f.height < 0.0 {
            -f.height
        } else if f.height > 0.0 {
            f.height * upem / (asc - desc).max(1.0)
        } else {
            // A default size: 12 pt in device pixels.
            16.0 / self.page().scale().max(1e-9)
        };
        let s = em / upem;
        let avg = face
            .raw_face()
            .table(ttf_parser::Tag::from_bytes(b"OS/2"))
            .and_then(|t| t.get(2..4))
            .map(|b| i16::from_be_bytes([b[0], b[1]]) as f64)
            .filter(|v| *v > 0.0)
            .unwrap_or(upem * 0.5);
        let xs = if f.width > 0.0 { (f.width / (avg * s)).clamp(0.05, 20.0) } else { 1.0 };
        // Advances along the baseline, logical.
        let adv: Vec<f64> = text
            .iter()
            .enumerate()
            .map(|(i, &c)| match dx.and_then(|d| d.get(i)) {
                Some(&d) => d,
                None => {
                    let ch = match sym {
                        font::Symbols::Symbol => font::symbol_char(c),
                        _ => char::from_u32(c),
                    };
                    ch.and_then(|ch| font::glyph(face, ch)).and_then(|g| face.glyph_hor_advance(g)).unwrap_or((upem * 0.5) as u16) as f64 * s * xs
                }
            })
            .collect();
        let total: f64 = adv.iter().sum();
        // The file's advances are its own font's: a wider face is narrowed
        // to them, so letters don't run into each other.
        let xs = match dx {
            Some(_) => {
                let natural: f64 = text
                    .iter()
                    .map(|&c| {
                        let ch = match sym {
                            font::Symbols::Symbol => font::symbol_char(c),
                            _ => char::from_u32(c),
                        };
                        ch.and_then(|ch| font::glyph(face, ch)).and_then(|g| face.glyph_hor_advance(g)).unwrap_or(0) as f64 * s * xs
                    })
                    .sum();
                if natural > 0.0 && total.abs() < natural { xs * (total.abs() / natural).max(0.6) } else { xs }
            }
            None => xs,
        };
        let update = self.dc.text_align & 1 != 0;
        let (ox, oy) = if update { self.dc.cur } else { (x, y) };
        let shift_u = match self.dc.text_align & 6 {
            6 => -total / 2.0,
            2 => -total,
            _ => 0.0,
        };
        let shift_v = match self.dc.text_align & 24 {
            24 => 0.0,
            8 => -desc * s,
            _ => -asc * s,
        };
        let m = self.m();
        let flip = if m.det() < 0.0 { 1.0 } else { -1.0 };
        let th = f.escapement / 10.0 * PI / 180.0;
        let (c, sn) = (th.cos(), th.sin());
        // Text space (u along the baseline, v up) to logical.
        let t2l = Aff { a: c, b: flip * sn, c: -sn, d: flip * c, e: ox, f: oy };
        let t2c = t2l.then(&m);
        if update {
            let (ex, ey) = t2l.apply(total, 0.0);
            self.dc.cur = if self.dc.text_align & 2 != 0 && self.dc.text_align & 4 == 0 { (ox, oy) } else { (ex, ey) };
        }
        if self.dc.bk_mode == 2 && self.path.is_none() {
            let q = |u: f64, v: f64| t2c.apply(u + shift_u, v + shift_v);
            let mut pb = PathBuilder::new();
            let (a, b, cc, d) = (q(0.0, asc * s), q(total, asc * s), q(total, desc * s), q(0.0, desc * s));
            pb.move_to(a.0 as f32, a.1 as f32);
            pb.line_to(b.0 as f32, b.1 as f32);
            pb.line_to(cc.0 as f32, cc.1 as f32);
            pb.line_to(d.0 as f32, d.1 as f32);
            pb.close();
            if let Some(p) = pb.finish() {
                self.fill_with(&p, rgb(self.dc.bk_color));
            }
        }
        let mut pb = PathBuilder::new();
        let mut u = shift_u;
        let slant = if f.italic { 0.2 } else { 0.0 };
        for (i, &c) in text.iter().enumerate() {
            let ch = match sym {
                font::Symbols::Symbol => font::symbol_char(c),
                _ => char::from_u32(if (0xF020..=0xF0FF).contains(&c) { c - 0xF000 } else { c }),
            };
            if let Some(g) = ch.filter(|c| !c.is_whitespace()).and_then(|ch| font::glyph(face, ch)) {
                self.glyphs += 1;
                if self.glyphs > MAX_GLYPHS {
                    break;
                }
                // Glyph units to text space, then to the canvas.
                let g2t = Aff { a: s * xs, b: 0.0, c: s * slant, d: s, e: u, f: shift_v };
                let g2c = g2t.then(&t2c);
                if !g2c.finite() {
                    return;
                }
                let mut o = font::Outline { pb: &mut pb, m: [g2c.a as f32, g2c.b as f32, g2c.c as f32, g2c.d as f32, g2c.e as f32, g2c.f as f32] };
                face.outline_glyph(g, &mut o);
            }
            u += adv.get(i).copied().unwrap_or(0.0);
        }
        let line = |pb: &mut PathBuilder, pos: f64, thick: f64| {
            let q = |u: f64, v: f64| {
                let (x, y) = t2c.apply(u, v + shift_v);
                (x as f32, y as f32)
            };
            let (a, b, c, d) = (q(shift_u, pos), q(shift_u + total, pos), q(shift_u + total, pos - thick), q(shift_u, pos - thick));
            pb.move_to(a.0, a.1);
            pb.line_to(b.0, b.1);
            pb.line_to(c.0, c.1);
            pb.line_to(d.0, d.1);
            pb.close();
        };
        let thick = (em / 14.0).max(1.0 / m.scale().max(1e-9));
        if f.underline {
            line(&mut pb, -em * 0.12, thick);
        }
        if f.strike {
            line(&mut pb, em * 0.3, thick);
        }
        let Some(path) = pb.finish() else { return };
        if let Some(rec) = &mut self.path {
            rec.push_path(&path);
            return;
        }
        if !self.spend(&path) {
            return;
        }
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        paint.set_color(rgb(self.dc.text_color));
        let clip = self.dc.clip.clone();
        self.pix.fill_path(&tame(&path, self.pix.width(), self.pix.height()), &paint, FillRule::Winding, Transform::identity(), clip.as_deref());
    }
}

/// Points far outside the canvas pulled in to a margin around it: the
/// rasteriser's fixed-point arithmetic doesn't take arbitrary coordinates.
/// Exact for the edges that matter (the ones crossing the canvas, when
/// they are axis-aligned); a slanted edge from far away bends a little.
fn tame(p: &Path, w: u32, h: u32) -> std::borrow::Cow<'_, Path> {
    let l = (w.max(h) as f32 * 4.0).max(4096.0);
    let b = p.bounds();
    if b.left() >= -l && b.top() >= -l && b.right() <= l && b.bottom() <= l {
        return std::borrow::Cow::Borrowed(p);
    }
    let c = |q: tiny_skia::Point| (q.x.clamp(-l, l), q.y.clamp(-l, l));
    let mut pb = PathBuilder::new();
    for seg in p.segments() {
        match seg {
            tiny_skia::PathSegment::MoveTo(a) => {
                let a = c(a);
                pb.move_to(a.0, a.1)
            }
            tiny_skia::PathSegment::LineTo(a) => {
                let a = c(a);
                pb.line_to(a.0, a.1)
            }
            tiny_skia::PathSegment::QuadTo(a, b) => {
                let (a, b) = (c(a), c(b));
                pb.quad_to(a.0, a.1, b.0, b.1)
            }
            tiny_skia::PathSegment::CubicTo(a, b, d) => {
                let (a, b, d) = (c(a), c(b), c(d));
                pb.cubic_to(a.0, a.1, b.0, b.1, d.0, d.1)
            }
            tiny_skia::PathSegment::Close => pb.close(),
        }
    }
    match pb.finish() {
        Some(q) => std::borrow::Cow::Owned(q),
        None => std::borrow::Cow::Owned(PathBuilder::from_rect(Rect::from_xywh(-1.0, -1.0, 0.5, 0.5).unwrap())),
    }
}

/// Windows-1252 bytes as code points.
pub fn cp1252(b: &[u8]) -> Vec<u32> {
    crate::charset::decode(b, 0)
}
