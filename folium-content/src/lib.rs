//! Folium's content stream interpreter (folium.md §5, §7). One page's
//! content becomes a [`DisplayList`]: fills (in page space), strokes (in
//! user space with their matrix, so a non-uniform matrix strokes right),
//! glyph runs and images, each with a clip and bounds. Two things keep
//! heavy pages small (§7.2, §7.3):
//!
//! - clip elision: a rectangle clip that contains the current clip is no
//!   clip, rectangles intersect into one, and identical clips share an id;
//! - coalescing: consecutive opaque fills (and strokes) with the same
//!   paint and clip become one item.
//!
//! Page space is PDF user space of the page (points, y up); the renderer
//! maps it to the screen.

pub mod color;
pub mod ops;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use folium_font::geom::{El, Matrix, Path, Rect};
use folium_font::Font;
use folium_syntax::lexer::{Lexer, Token};
use folium_syntax::{Dict, Document, Error, Name, Object, Ref, Result, Stream, StreamData};

use color::{Rgba, Space};

/// Operators interpreted for one page, Form XObjects included.
pub const OPS_MAX: usize = 64 << 20;
/// Path elements held by one display list.
pub const ELEMENTS_MAX: usize = 32 << 20;
/// Form XObjects inside Form XObjects.
pub const FORM_DEPTH_MAX: usize = 24;
/// q without Q.
pub const SAVE_DEPTH_MAX: usize = 256;
/// How often cancellation is checked, in operators.
const CANCEL_EVERY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    Miter,
    Round,
    Bevel,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrokeStyle {
    pub width: f64,
    pub cap: Cap,
    pub join: Join,
    pub miter: f64,
    pub dash: Vec<f64>,
    pub phase: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClipKind {
    Rect(Rect),
    Path { path: Arc<Path>, even_odd: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// 0 is "no clip"; clips form a tree through `parent`.
    pub parent: u32,
    pub kind: ClipKind,
    /// The intersection of this clip and its parents.
    pub bounds: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMode {
    Fill,
    Stroke,
    FillStroke,
    Invisible,
}

#[derive(Clone)]
pub enum Item {
    Fill { path: Path, even_odd: bool, color: Rgba, clip: u32, bounds: Rect },
    Stroke { path: Path, ctm: Matrix, style: StrokeStyle, color: Rgba, clip: u32, bounds: Rect },
    Glyphs { font: Arc<Font>, glyphs: Vec<(u32, Matrix)>, mode: TextMode, fill: Rgba, stroke: Rgba, style: StrokeStyle, ctm: Matrix, clip: u32, bounds: Rect },
    /// An image: the unit square through `matrix` to page space.
    Image { image: Arc<Stream>, key: Option<Ref>, matrix: Matrix, alpha: f32, clip: u32, bounds: Rect },
    /// A 1-bit stencil painted in `color`.
    Mask { image: Arc<Stream>, key: Option<Ref>, matrix: Matrix, color: Rgba, clip: u32, bounds: Rect },
}

impl Item {
    pub fn bounds(&self) -> Rect {
        match self {
            Item::Fill { bounds, .. }
            | Item::Stroke { bounds, .. }
            | Item::Glyphs { bounds, .. }
            | Item::Image { bounds, .. }
            | Item::Mask { bounds, .. } => *bounds,
        }
    }
    pub fn clip(&self) -> u32 {
        match self {
            Item::Fill { clip, .. } | Item::Stroke { clip, .. } | Item::Glyphs { clip, .. } | Item::Image { clip, .. } | Item::Mask { clip, .. } => *clip,
        }
    }
}

#[derive(Clone, Default)]
pub struct DisplayList {
    pub items: Vec<Item>,
    pub clips: Vec<Clip>,
    /// Operators run, for the dump tool and budgets.
    pub ops: usize,
    pub elements: usize,
    /// Things not drawn yet (shadings, Type 3, …), by name, for reports.
    pub skipped: HashMap<&'static str, usize>,
}

impl DisplayList {
    pub fn clip(&self, id: u32) -> Option<&Clip> { id.checked_sub(1).and_then(|i| self.clips.get(i as usize)) }
    /// The visible bounds of an item: its bounds within its clip.
    pub fn visible(&self, it: &Item) -> Rect {
        let b = it.bounds();
        match self.clip(it.clip()) {
            Some(c) => b.intersect(&c.bounds),
            None => b,
        }
    }
}

/// Fonts shared by every page of a document (keyed by object).
#[derive(Default)]
pub struct Shared {
    fonts: Mutex<HashMap<Ref, Arc<Font>>>,
}

impl Shared {
    pub fn new() -> Self { Self::default() }
    fn font(&self, doc: &Document, o: &Object) -> Option<Arc<Font>> {
        let r = o.as_ref();
        if let Some(r) = r {
            if let Some(f) = self.fonts.lock().unwrap().get(&r) {
                return Some(f.clone());
            }
        }
        let d = doc.resolve(o).ok()?;
        let f = Arc::new(Font::load(doc, d.as_dict()?));
        if let Some(r) = r {
            let mut c = self.fonts.lock().unwrap();
            if c.len() > 4096 {
                c.clear();
            }
            c.insert(r, f.clone());
        }
        Some(f)
    }
}

#[derive(Clone)]
struct TextState {
    font: Option<Arc<Font>>,
    size: f64,
    char_space: f64,
    word_space: f64,
    scale: f64,
    leading: f64,
    rise: f64,
    mode: u8,
}

#[derive(Clone)]
struct State {
    ctm: Matrix,
    clip: u32,
    fill_space: Arc<Space>,
    fill: Vec<f32>,
    stroke_space: Arc<Space>,
    stroke: Vec<f32>,
    style: StrokeStyle,
    fill_alpha: f32,
    stroke_alpha: f32,
    text: TextState,
}

impl State {
    fn fill_rgba(&self) -> Rgba {
        let mut c = self.fill_space.to_rgb(&self.fill);
        c.a = self.fill_alpha;
        c
    }
    fn stroke_rgba(&self) -> Rgba {
        let mut c = self.stroke_space.to_rgb(&self.stroke);
        c.a = self.stroke_alpha;
        c
    }
}

pub struct Interp<'a> {
    doc: &'a Document,
    shared: &'a Shared,
    cancel: &'a dyn Fn() -> bool,
    out: DisplayList,
    clip_ids: HashMap<(u32, [u64; 4]), u32>,
    merge_fills: bool,
    gs: State,
    stack: Vec<State>,
    path: Path,
    /// A clip waiting for the painting operator (W n).
    pending_clip: Option<bool>,
    tm: Matrix,
    tlm: Matrix,
    forms: Vec<Ref>,
}

fn num(o: &Object) -> f64 { o.as_f64().unwrap_or(0.0) }

impl<'a> Interp<'a> {
    pub fn new(doc: &'a Document, shared: &'a Shared, ctm: Matrix, clip: Option<Rect>, cancel: &'a dyn Fn() -> bool) -> Interp<'a> {
        let gray = Arc::new(Space::Gray);
        let mut it = Interp {
            doc,
            shared,
            cancel,
            out: DisplayList::default(),
            clip_ids: HashMap::new(),
            merge_fills: true,
            gs: State {
                ctm,
                clip: 0,
                fill_space: gray.clone(),
                fill: vec![0.0],
                stroke_space: gray,
                stroke: vec![0.0],
                style: StrokeStyle { width: 1.0, cap: Cap::Butt, join: Join::Miter, miter: 10.0, dash: Vec::new(), phase: 0.0 },
                fill_alpha: 1.0,
                stroke_alpha: 1.0,
                text: TextState { font: None, size: 0.0, char_space: 0.0, word_space: 0.0, scale: 1.0, leading: 0.0, rise: 0.0, mode: 0 },
            },
            stack: Vec::new(),
            path: Path::new(),
            pending_clip: None,
            tm: Matrix::IDENTITY,
            tlm: Matrix::IDENTITY,
            forms: Vec::new(),
        };
        if let Some(r) = clip {
            let mut p = Path::new();
            p.rect(r.x0, r.y0, r.width(), r.height());
            it.add_clip(&p, false, &Matrix::IDENTITY);
        }
        it
    }

    /// Oppositely wound paths from separate fill operations must not cancel
    /// each other in a visual copy, even when their colours are identical.
    pub fn keep_fills_separate(&mut self) { self.merge_fills = false; }

    pub fn finish(self) -> DisplayList { self.out }

    /// Run one content stream (already decoded) with its resources.
    pub fn run(&mut self, content: &[u8], res: &Dict) -> Result<()> {
        let mut lx = Lexer::new(content, 0);
        let mut args: Vec<Object> = Vec::with_capacity(8);
        loop {
            let t = match lx.next() {
                Ok(Some(t)) => t,
                Ok(None) => break,
                // Bad syntax: skip a byte and go on, as readers do.
                Err(Error::Truncated) => break,
                Err(_) => {
                    lx.pos += 1;
                    args.clear();
                    continue;
                }
            };
            match t {
                Token::Keyword(op) => {
                    self.out.ops += 1;
                    if self.out.ops > OPS_MAX {
                        return Err(Error::Limit("operators on one page"));
                    }
                    if self.out.ops % CANCEL_EVERY == 0 && (self.cancel)() {
                        return Err(Error::Limit("cancelled"));
                    }
                    if op == b"BI" {
                        self.inline_image(&mut lx, res);
                    } else {
                        self.op(op, &args, res)?;
                    }
                    args.clear();
                }
                t => match lx.object_from(t, 1) {
                    Ok(o) => {
                        if args.len() < 64 {
                            args.push(o);
                        }
                    }
                    Err(Error::Truncated) => break,
                    Err(_) => args.clear(),
                },
            }
        }
        Ok(())
    }

    fn res_get(&self, res: &Dict, cat: &str, name: &Name) -> Option<Object> {
        let d = self.doc.get_in(res, cat).ok()?;
        let v = d.as_dict()?.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone());
        v
    }

    fn op(&mut self, op: &[u8], a: &[Object], res: &Dict) -> Result<()> {
        let n = |i: usize| a.get(i).map(num).unwrap_or(0.0);
        let last_n = |k: usize| -> Vec<f64> { a.iter().rev().take(k).rev().map(num).collect() };
        match op {
            b"q" => {
                if self.stack.len() < SAVE_DEPTH_MAX {
                    self.stack.push(self.gs.clone());
                }
            }
            b"Q" => {
                if let Some(s) = self.stack.pop() {
                    self.gs = s;
                }
            }
            b"cm" => {
                if a.len() >= 6 {
                    let v = last_n(6);
                    if let Some(m) = Matrix::from_array(&v) {
                        self.gs.ctm = m.then(&self.gs.ctm);
                    }
                }
            }
            b"w" => self.gs.style.width = n(0).abs(),
            b"J" => self.gs.style.cap = match n(0) as i64 { 1 => Cap::Round, 2 => Cap::Square, _ => Cap::Butt },
            b"j" => self.gs.style.join = match n(0) as i64 { 1 => Join::Round, 2 => Join::Bevel, _ => Join::Miter },
            b"M" => self.gs.style.miter = n(0).max(1.0),
            b"d" => {
                if let Some(arr) = a.first().and_then(Object::as_array) {
                    let d: Vec<f64> = arr.iter().map(num).filter(|v| v.is_finite() && *v >= 0.0).take(32).collect();
                    self.gs.style.dash = if d.iter().sum::<f64>() > 0.0 { d } else { Vec::new() };
                    self.gs.style.phase = n(1);
                }
            }
            b"gs" => {
                if let Some(Object::Name(nm)) = a.first() {
                    if let Some(o) = self.res_get(res, "ExtGState", nm) {
                        self.ext_gstate(&o);
                    }
                }
            }
            b"ri" | b"i" => {}
            // Paths.
            b"m" => {
                let (x, y) = (n(0), n(1));
                self.path.move_to(x, y);
            }
            b"l" => {
                self.ensure_start();
                self.path.line_to(n(0), n(1));
            }
            b"c" => {
                self.ensure_start();
                self.path.cubic_to(n(0), n(1), n(2), n(3), n(4), n(5));
            }
            b"v" => {
                self.ensure_start();
                let (x0, y0) = self.current();
                self.path.cubic_to(x0, y0, n(0), n(1), n(2), n(3));
            }
            b"y" => {
                self.ensure_start();
                self.path.cubic_to(n(0), n(1), n(2), n(3), n(2), n(3));
            }
            b"h" => {
                if !self.path.is_empty() {
                    self.path.close();
                }
            }
            b"re" => self.path.rect(n(0), n(1), n(2), n(3)),
            b"S" => self.paint(false, false, true),
            b"s" => {
                self.path.close();
                self.paint(false, false, true)
            }
            b"f" | b"F" => self.paint(true, false, false),
            b"f*" => self.paint(true, true, false),
            b"B" => self.paint(true, false, true),
            b"B*" => self.paint(true, true, true),
            b"b" => {
                self.path.close();
                self.paint(true, false, true)
            }
            b"b*" => {
                self.path.close();
                self.paint(true, true, true)
            }
            b"n" => self.paint(false, false, false),
            b"W" => self.pending_clip = Some(false),
            b"W*" => self.pending_clip = Some(true),
            // Text.
            b"BT" => {
                self.tm = Matrix::IDENTITY;
                self.tlm = Matrix::IDENTITY;
            }
            b"ET" => {}
            b"Tc" => self.gs.text.char_space = n(0),
            b"Tw" => self.gs.text.word_space = n(0),
            b"Tz" => self.gs.text.scale = n(0) / 100.0,
            b"TL" => self.gs.text.leading = n(0),
            b"Ts" => self.gs.text.rise = n(0),
            b"Tr" => self.gs.text.mode = n(0).clamp(0.0, 7.0) as u8,
            b"Tf" => {
                if let Some(Object::Name(nm)) = a.first() {
                    self.gs.text.font = self.res_get(res, "Font", nm).and_then(|o| self.shared.font(self.doc, &o));
                }
                self.gs.text.size = n(1);
            }
            b"Td" => self.next_line(n(0), n(1)),
            b"TD" => {
                self.gs.text.leading = -n(1);
                self.next_line(n(0), n(1));
            }
            b"Tm" => {
                if let Some(m) = Matrix::from_array(&last_n(6)) {
                    self.tm = m;
                    self.tlm = m;
                }
            }
            b"T*" => self.next_line(0.0, -self.gs.text.leading),
            b"Tj" => {
                if let Some(s) = a.first().and_then(Object::as_str) {
                    self.show(s);
                }
            }
            b"'" => {
                self.next_line(0.0, -self.gs.text.leading);
                if let Some(s) = a.first().and_then(Object::as_str) {
                    self.show(s);
                }
            }
            b"\"" => {
                self.gs.text.word_space = n(0);
                self.gs.text.char_space = n(1);
                self.next_line(0.0, -self.gs.text.leading);
                if let Some(s) = a.get(2).and_then(Object::as_str) {
                    self.show(s);
                }
            }
            b"TJ" => {
                if let Some(arr) = a.first().and_then(Object::as_array) {
                    for o in arr {
                        match o {
                            Object::String(s) => self.show(s),
                            o => {
                                let adj = num(o) / 1000.0 * self.gs.text.size;
                                if self.gs.text.font.as_ref().is_some_and(|f| f.vertical()) {
                                    self.tm = Matrix::translate(0.0, -adj).then(&self.tm);
                                } else {
                                    self.tm = Matrix::translate(-adj * self.gs.text.scale, 0.0).then(&self.tm);
                                }
                            }
                        }
                    }
                }
            }
            // Colour.
            b"CS" | b"cs" => {
                let sp = a.first().map(|o| self.space(o, res)).unwrap_or(Space::Gray);
                let init = sp.initial();
                if op == b"CS" {
                    self.gs.stroke_space = Arc::new(sp);
                    self.gs.stroke = init;
                } else {
                    self.gs.fill_space = Arc::new(sp);
                    self.gs.fill = init;
                }
            }
            b"SC" | b"SCN" => self.gs.stroke = a.iter().filter_map(Object::as_f64).map(|v| v as f32).collect(),
            b"sc" | b"scn" => self.gs.fill = a.iter().filter_map(Object::as_f64).map(|v| v as f32).collect(),
            b"G" | b"g" | b"RG" | b"rg" | b"K" | b"k" => {
                let (sp, k) = match op {
                    b"G" | b"g" => (Space::Gray, 1),
                    b"RG" | b"rg" => (Space::Rgb, 3),
                    _ => (Space::Cmyk, 4),
                };
                let v: Vec<f32> = last_n(k).into_iter().map(|v| v as f32).collect();
                if op[0].is_ascii_uppercase() {
                    self.gs.stroke_space = Arc::new(sp);
                    self.gs.stroke = v;
                } else {
                    self.gs.fill_space = Arc::new(sp);
                    self.gs.fill = v;
                }
            }
            // XObjects.
            b"Do" => {
                if let Some(Object::Name(nm)) = a.first() {
                    if let Some(o) = self.res_get(res, "XObject", nm) {
                        self.xobject(&o, res)?;
                    }
                }
            }
            b"sh" => *self.out.skipped.entry("shading").or_default() += 1,
            b"d0" | b"d1" | b"BMC" | b"BDC" | b"EMC" | b"MP" | b"DP" | b"BX" | b"EX" => {}
            _ => {}
        }
        Ok(())
    }

    fn ensure_start(&mut self) {
        if self.path.is_empty() {
            self.path.move_to(0.0, 0.0);
        }
    }

    fn current(&self) -> (f64, f64) {
        match self.path.els.last() {
            Some(El::Move(x, y) | El::Line(x, y) | El::Cubic(_, _, _, _, x, y)) => (*x, *y),
            _ => (0.0, 0.0),
        }
    }

    fn space(&self, o: &Object, res: &Dict) -> Space {
        if let Object::Name(nm) = o {
            if !matches!(nm.as_bytes(), b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" | b"Pattern" | b"G" | b"RGB" | b"CMYK") {
                if let Some(cs) = self.res_get(res, "ColorSpace", nm) {
                    return Space::read(self.doc, &cs, 0);
                }
            }
        }
        Space::read(self.doc, o, 0)
    }

    fn ext_gstate(&mut self, o: &Object) {
        let Ok(Object::Dict(d)) = self.doc.resolve(o) else { return };
        for (k, v) in d.iter() {
            let v = self.doc.resolve(v).unwrap_or(Object::Null);
            match k.as_bytes() {
                b"LW" => self.gs.style.width = num(&v).abs(),
                b"LC" => self.gs.style.cap = match num(&v) as i64 { 1 => Cap::Round, 2 => Cap::Square, _ => Cap::Butt },
                b"LJ" => self.gs.style.join = match num(&v) as i64 { 1 => Join::Round, 2 => Join::Bevel, _ => Join::Miter },
                b"ML" => self.gs.style.miter = num(&v).max(1.0),
                b"CA" => self.gs.stroke_alpha = (num(&v) as f32).clamp(0.0, 1.0),
                b"ca" => self.gs.fill_alpha = (num(&v) as f32).clamp(0.0, 1.0),
                b"D" => {
                    if let Some(arr) = v.as_array() {
                        if let Some(d) = arr.first().and_then(Object::as_array) {
                            self.gs.style.dash = d.iter().map(num).filter(|x| *x >= 0.0).take(32).collect();
                            if self.gs.style.dash.iter().sum::<f64>() <= 0.0 {
                                self.gs.style.dash.clear();
                            }
                        }
                        self.gs.style.phase = arr.get(1).map(num).unwrap_or(0.0);
                    }
                }
                b"Font" => {
                    if let Some(arr) = v.as_array() {
                        if let Some(f) = arr.first() {
                            self.gs.text.font = self.shared.font(self.doc, f);
                        }
                        self.gs.text.size = arr.get(1).map(num).unwrap_or(self.gs.text.size);
                    }
                }
                b"SMask" if !matches!(v, Object::Name(_)) => *self.out.skipped.entry("soft mask").or_default() += 1,
                b"BM" if !matches!(&v, Object::Name(n) if n.is("Normal") || n.is("Compatible")) => {
                    *self.out.skipped.entry("blend mode").or_default() += 1
                }
                _ => {}
            }
        }
    }

    /// Paint the current path, then apply a pending clip.
    fn paint(&mut self, fill: bool, even_odd: bool, stroke: bool) {
        let path = std::mem::take(&mut self.path);
        if !path.is_empty() {
            self.out.elements += path.els.len();
            if self.out.elements > ELEMENTS_MAX {
                *self.out.skipped.entry("over the path budget").or_default() += 1;
            } else {
                if fill {
                    let p = path.transform(&self.gs.ctm);
                    let color = self.gs.fill_rgba();
                    self.push_fill(p, even_odd, color);
                }
                if stroke {
                    self.push_stroke(&path);
                }
            }
            if let Some(eo) = self.pending_clip.take() {
                let ctm = self.gs.ctm;
                self.add_clip(&path, eo, &ctm);
            }
        }
        self.pending_clip = None;
    }

    fn push_fill(&mut self, p: Path, even_odd: bool, color: Rgba) {
        let bounds = p.bounds();
        if bounds.is_empty() && !(bounds.width() == 0.0 || bounds.height() == 0.0) {
            return;
        }
        let clip = self.gs.clip;
        if let Some(Item::Fill { path, even_odd: e, color: c, clip: k, bounds: b }) = self.out.items.last_mut() {
            if self.merge_fills && *e == even_odd && *c == color && *k == clip && color.a >= 1.0 && path.els.len() < 1 << 16 && !even_odd {
                path.extend(&p);
                *b = b.union(&bounds);
                return;
            }
        }
        self.out.items.push(Item::Fill { path: p, even_odd, color, clip, bounds });
    }

    fn push_stroke(&mut self, path: &Path) {
        let ctm = self.gs.ctm;
        let style = self.gs.style.clone();
        let color = self.gs.stroke_rgba();
        let clip = self.gs.clip;
        // Bounds: the path's, grown by the line width (×miter for joins).
        let pb = path.transform(&ctm).bounds();
        let w = style.width.max(1e-3) * ctm.max_scale() * if style.join == Join::Miter { style.miter.min(4.0) } else { 1.0 };
        let bounds = pb.inflate(w / 2.0 + 0.5);
        if let Some(Item::Stroke { path: p, ctm: m, style: s, color: c, clip: k, bounds: b }) = self.out.items.last_mut() {
            if *m == ctm && *s == style && *c == color && *k == clip && color.a >= 1.0 && p.els.len() < 1 << 16 {
                p.extend(path);
                *b = b.union(&bounds);
                return;
            }
        }
        self.out.items.push(Item::Stroke { path: path.clone(), ctm, style, color, clip, bounds });
    }

    /// Intersect the clip with `path` (user space, through `m`).
    fn add_clip(&mut self, path: &Path, even_odd: bool, m: &Matrix) {
        let cur = self.gs.clip;
        let cur_bounds = self.out.clip(cur).map(|c| c.bounds);
        // In page space, within a thousandth of a point: Word's matrices
        // carry rounding-error skews (folium.md §7.3).
        let paged = path.transform(m);
        let rect = paged.as_rect_within(1e-3);
        let (parent, kind, bounds) = match rect {
            Some(r) => {
                if let Some(cb) = cur_bounds {
                    // Elide: a rectangle containing the current clip.
                    if r.contains(&cb) {
                        return;
                    }
                }
                // Rectangles merge into one with the parent's rectangle.
                match self.out.clip(cur) {
                    Some(Clip { parent, kind: ClipKind::Rect(pr), .. }) => {
                        let i = pr.intersect(&r);
                        (*parent, ClipKind::Rect(i), i)
                    }
                    Some(c) => (cur, ClipKind::Rect(r), r.intersect(&c.bounds)),
                    None => (0, ClipKind::Rect(r), r),
                }
            }
            None => {
                let p = paged;
                let b = p.bounds();
                let bounds = match cur_bounds {
                    Some(cb) => b.intersect(&cb),
                    None => b,
                };
                (cur, ClipKind::Path { path: Arc::new(p), even_odd }, bounds)
            }
        };
        // Share identical rectangle clips.
        if let ClipKind::Rect(r) = &kind {
            let key = (parent, [r.x0.to_bits(), r.y0.to_bits(), r.x1.to_bits(), r.y1.to_bits()]);
            if let Some(&id) = self.clip_ids.get(&key) {
                self.gs.clip = id;
                return;
            }
            self.out.clips.push(Clip { parent, kind, bounds });
            let id = self.out.clips.len() as u32;
            self.clip_ids.insert(key, id);
            self.gs.clip = id;
            return;
        }
        // Share identical path clips too (one per element in Word's charts).
        let key = match &kind {
            ClipKind::Path { path, even_odd } => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                even_odd.hash(&mut h);
                for e in &path.els {
                    match *e {
                        El::Move(x, y) | El::Line(x, y) => [x, y].iter().for_each(|v| v.to_bits().hash(&mut h)),
                        El::Cubic(a, b, c, d, x, y) => [a, b, c, d, x, y].iter().for_each(|v| v.to_bits().hash(&mut h)),
                        El::Close => 7u8.hash(&mut h),
                    }
                }
                Some((parent, [h.finish(), path.els.len() as u64, u64::MAX, u64::MAX]))
            }
            _ => None,
        };
        if let Some(k) = key {
            if let Some(&id) = self.clip_ids.get(&k) {
                if self.out.clip(id).is_some_and(|c| c.kind == kind) {
                    self.gs.clip = id;
                    return;
                }
            }
            self.out.clips.push(Clip { parent, kind, bounds });
            let id = self.out.clips.len() as u32;
            self.clip_ids.insert(k, id);
            self.gs.clip = id;
            return;
        }
        self.out.clips.push(Clip { parent, kind, bounds });
        self.gs.clip = self.out.clips.len() as u32;
    }

    fn next_line(&mut self, tx: f64, ty: f64) {
        self.tlm = Matrix::translate(tx, ty).then(&self.tlm);
        self.tm = self.tlm;
    }

    fn show(&mut self, s: &[u8]) {
        let Some(font) = self.gs.text.font.clone() else { return };
        let t = &self.gs.text;
        let (size, cs, ws, th, rise) = (t.size, t.char_space, t.word_space, t.scale, t.rise);
        let mode = match t.mode % 4 {
            0 => TextMode::Fill,
            1 => TextMode::Stroke,
            2 => TextMode::FillStroke,
            _ => TextMode::Invisible,
        };
        if t.mode >= 4 {
            *self.out.skipped.entry("text clip").or_default() += 1;
        }
        let vertical = font.vertical();
        let mut glyphs = Vec::new();
        let mut bounds = Rect::EMPTY;
        let ctm = self.gs.ctm;
        for g in font.glyphs(s) {
            let gm = Matrix::new(size * th, 0.0, 0.0, size, 0.0, rise).then(&self.tm).then(&ctm);
            if mode != TextMode::Invisible && !font.type3 {
                glyphs.push((g.gid, gm));
                let adv = if vertical { 1.0 } else { g.advance.max(0.3) };
                bounds = bounds.union(&Rect::new(0.0, -0.25, adv, 1.0).transform(&gm));
            }
            let space = if g.len == 1 && g.code == 32 { ws } else { 0.0 };
            if vertical {
                let ty = -size + cs + space;
                self.tm = Matrix::translate(0.0, ty).then(&self.tm);
            } else {
                let tx = (g.advance * size + cs + space) * th;
                self.tm = Matrix::translate(tx, 0.0).then(&self.tm);
            }
        }
        if font.type3 {
            *self.out.skipped.entry("Type 3 text").or_default() += 1;
        }
        if glyphs.is_empty() {
            return;
        }
        let item = Item::Glyphs {
            font,
            glyphs,
            mode,
            fill: self.gs.fill_rgba(),
            stroke: self.gs.stroke_rgba(),
            style: self.gs.style.clone(),
            ctm,
            clip: self.gs.clip,
            bounds,
        };
        // Runs in one font, paint and clip join (a line of TJ pieces).
        if let (Some(Item::Glyphs { font: f, glyphs: g, mode: m, fill: fc, stroke: sc, clip: k, bounds: b, .. }), Item::Glyphs { font: f2, glyphs: g2, mode: m2, fill: fc2, stroke: sc2, clip: k2, bounds: b2, .. }) =
            (self.out.items.last_mut(), &item)
        {
            if Arc::ptr_eq(f, f2) && m == m2 && fc == fc2 && sc == sc2 && k == k2 && g.len() < 4096 {
                g.extend_from_slice(g2);
                *b = b.union(b2);
                return;
            }
        }
        self.out.items.push(item);
    }

    fn xobject(&mut self, o: &Object, res: &Dict) -> Result<()> {
        let key = o.as_ref();
        let Ok(Object::Stream(st)) = self.doc.resolve(o) else { return Ok(()) };
        let d = &st.dict;
        let sub = d.name("Subtype");
        if sub.is_some_and(|s| s.is("Image")) {
            self.image(Arc::new(st.clone()), key);
        } else if sub.is_some_and(|s| s.is("Form")) {
            if self.forms.len() >= FORM_DEPTH_MAX || key.is_some_and(|k| self.forms.contains(&k)) {
                *self.out.skipped.entry("form loop or depth").or_default() += 1;
                return Ok(());
            }
            let Ok(data) = self.doc.decode(&st) else { return Ok(()) };
            let form_res = match self.doc.get_in(d, "Resources") {
                Ok(Object::Dict(r)) => r,
                _ => res.clone(),
            };
            let saved = (self.gs.clone(), self.stack.len(), std::mem::take(&mut self.path));
            if let Some(m) = d.get("Matrix").and_then(Object::as_array).and_then(|a| Matrix::from_array(&a.iter().map(num).collect::<Vec<_>>())) {
                self.gs.ctm = m.then(&self.gs.ctm);
            }
            if let Some(b) = d.get("BBox").and_then(Object::as_array).filter(|a| a.len() == 4) {
                let r = Rect::new(num(&b[0]), num(&b[1]), num(&b[2]), num(&b[3]));
                let mut p = Path::new();
                p.rect(r.x0, r.y0, r.width(), r.height());
                let ctm = self.gs.ctm;
                self.add_clip(&p, false, &ctm);
            }
            if d.get("Group").is_some() {
                *self.out.skipped.entry("transparency group (drawn plainly)").or_default() += 1;
            }
            if let Some(k) = key {
                self.forms.push(k);
            }
            let r = self.run(&data.data, &form_res);
            if key.is_some() {
                self.forms.pop();
            }
            self.gs = saved.0;
            self.stack.truncate(saved.1);
            self.path = saved.2;
            r?;
        }
        Ok(())
    }

    fn image(&mut self, st: Arc<Stream>, key: Option<Ref>) {
        let m = self.gs.ctm;
        let bounds = Rect::new(0.0, 0.0, 1.0, 1.0).transform(&m);
        let clip = self.gs.clip;
        let is_mask = st.dict.get("ImageMask").or_else(|| st.dict.get("IM")).and_then(Object::as_bool).unwrap_or(false);
        if is_mask {
            let color = self.gs.fill_rgba();
            self.out.items.push(Item::Mask { image: st, key, matrix: m, color, clip, bounds });
        } else {
            let alpha = self.gs.fill_alpha;
            self.out.items.push(Item::Image { image: st, key, matrix: m, alpha, clip, bounds });
        }
    }

    /// `BI … ID <data> EI`: the dictionary with abbreviations expanded.
    fn inline_image(&mut self, lx: &mut Lexer, res: &Dict) {
        let mut d = Dict::new();
        loop {
            match lx.next() {
                Ok(Some(Token::Keyword(b"ID"))) => break,
                Ok(Some(Token::Name(k))) => {
                    let Ok(Some(t)) = lx.next() else { return };
                    let Ok(v) = lx.object_from(t, 1) else { return };
                    let full = match k.as_bytes() {
                        b"BPC" => "BitsPerComponent",
                        b"CS" => "ColorSpace",
                        b"D" => "Decode",
                        b"DP" => "DecodeParms",
                        b"F" => "Filter",
                        b"H" => "Height",
                        b"IM" => "ImageMask",
                        b"I" => "Interpolate",
                        b"W" => "Width",
                        b"L" => "Length",
                        other => {
                            d.0.push((Name::new(other), v));
                            continue;
                        }
                    };
                    d.insert(full, v);
                }
                _ => return,
            }
        }
        // Abbreviated colour space names, or a resource name.
        if let Some(Object::Name(n)) = d.get("ColorSpace").cloned() {
            let full = match n.as_bytes() {
                b"G" => Some("DeviceGray"),
                b"RGB" => Some("DeviceRGB"),
                b"CMYK" => Some("DeviceCMYK"),
                b"I" => Some("Indexed"),
                _ => None,
            };
            match full {
                Some(f) => d.insert("ColorSpace", Object::Name(Name::new(f.as_bytes()))),
                None => {
                    if let Some(cs) = self.res_get(res, "ColorSpace", &n) {
                        let cs = self.doc.resolve(&cs).unwrap_or(Object::Null);
                        d.insert("ColorSpace", cs);
                    }
                }
            }
        }
        // One white-space byte after ID, then the data up to `EI`.
        let start = lx.pos + 1;
        let buf = lx.buf;
        let len = d.int("Length").filter(|l| *l > 0).map(|l| l as usize);
        let end = match len {
            Some(l) if start + l <= buf.len() => start + l,
            _ => {
                let mut i = start;
                let mut found = buf.len();
                while i + 2 <= buf.len() {
                    if buf[i] == b'E'
                        && buf[i + 1] == b'I'
                        && (i == start || folium_syntax::lexer::is_white(buf[i - 1]))
                        && buf.get(i + 2).is_none_or(|&c| folium_syntax::lexer::is_white(c) || folium_syntax::lexer::is_delim(c))
                    {
                        found = i.saturating_sub(1).max(start);
                        break;
                    }
                    i += 1;
                }
                found
            }
        };
        let data: Arc<[u8]> = Arc::from(&buf[start.min(buf.len())..end.min(buf.len())]);
        lx.pos = (end + 1).min(buf.len());
        // Skip to after "EI".
        while lx.pos + 1 < buf.len() && !(buf[lx.pos] == b'E' && buf[lx.pos + 1] == b'I') {
            lx.pos += 1;
            if lx.pos > end + 8 {
                break;
            }
        }
        if lx.pos + 1 < buf.len() && buf[lx.pos] == b'E' {
            lx.pos += 2;
        }
        let st = Stream { dict: d, data: StreamData::Bytes(data), owner: None };
        self.image(Arc::new(st), None);
    }
}

/// Interpret a page: its content streams joined, in page space, clipped
/// to `clip` (the crop box). `cancel` is polled between operators.
pub fn interpret(doc: &Document, shared: &Shared, contents: &[Stream], resources: &Dict, clip: Option<Rect>, cancel: &dyn Fn() -> bool) -> Result<DisplayList> {
    let mut data = Vec::new();
    for s in contents {
        match doc.decode(s) {
            Ok(d) => {
                data.extend_from_slice(&d.data);
                data.push(b'\n');
            }
            Err(Error::Limit(w)) => return Err(Error::Limit(w)),
            Err(_) => {}
        }
        if data.len() > folium_syntax::limits::STREAM_DECODED_MAX {
            return Err(Error::Limit("a page's content"));
        }
    }
    let mut it = Interp::new(doc, shared, Matrix::IDENTITY, clip, cancel);
    match it.run(&data, resources) {
        Ok(()) | Err(Error::Limit("operators on one page")) => {}
        Err(e) => return Err(e),
    }
    Ok(it.finish())
}
