//! The `.oslides` native file (docs/spec/officina.md §5.1, §9.3, §16.33).
//!
//! ```text
//! SLIDE*   one chunk per slide: its id, settings and shapes
//! MEDIA*   pieces of pictures and videos, at most MEDIA_PIECE bytes each,
//!          each piece stored once however many shapes use it
//! MASTERS  the masters and layouts
//! DECK     size, id watermark, theme, the slides in order (id, chunk), the
//!          media table (id, type, hash, length, chunks)
//! ```
//!
//! Opening reads DECK and MASTERS; a slide is read when it is first shown and
//! a picture when it is first drawn, so a big deck opens like a small one.
//! Every count and length is bounded before anything is allocated, and a
//! damaged slide or piece fails alone.

use std::io::{self, Write};
use std::sync::Arc;

use officina_core::container::{ContainerError, ContainerReader, ContainerWriter, DocKind, Entry, Limits};
use officina_core::crc32::crc32;
use officina_core::wire::{Reader, WireError, Writer};
use officina_text::model::{AttrId, Block, Para, TextDoc};
use officina_text::native::{decode_attrs, decode_props, encode_attrs, encode_props};

use crate::model::*;
use crate::ops::{DeckOp, Target};

pub const SLIDE: u16 = 1;
pub const MEDIA: u16 = 2;
pub const MASTERS: u16 = 3;
pub const DECK: u16 = 4;

/// The largest media piece.
pub const MEDIA_PIECE: usize = 16 << 20;
/// The longest picture or video (pieces are read one at a time).
pub const MAX_MEDIA_LEN: u64 = 8 << 30;

const MAX_STR: u64 = 1 << 16;
const MAX_TEXT: u64 = 16 << 20;
const MAX_PARAS: u64 = 100_000;
const MAX_RUNS: u64 = 1 << 20;
const MAX_PATH_CMDS: u64 = 1 << 20;
const MAX_STOPS: u64 = 64;
const MAX_MODS: u64 = 16;
const MAX_BUILDS: u64 = 4096;
const CHUNK_HEADER: usize = 12;

/// The file a deck was read from, kept for the slides and media not yet read.
#[derive(Clone)]
pub struct Backing {
    data: Arc<dyn AsRef<[u8]> + Send + Sync>,
    entries: Arc<Vec<Entry>>,
}

impl Backing {
    fn chunk(&self, i: usize) -> Result<&[u8], Damaged> {
        let e = self.entries.get(i).ok_or_else(|| Damaged(format!("no chunk {i}")))?;
        let data = (*self.data).as_ref();
        let o = e.offset as usize;
        let crc = u32::from_le_bytes(data[o + 8..o + 12].try_into().unwrap());
        let p = &data[o + CHUNK_HEADER..o + CHUNK_HEADER + e.len as usize];
        if crc32(p) != crc {
            return Err(Damaged(format!("chunk {i} fails its checksum")));
        }
        Ok(p)
    }
    fn kind(&self, i: usize) -> Option<u16> {
        self.entries.get(i).map(|e| e.kind)
    }
}

impl From<WireError> for Damaged {
    fn from(e: WireError) -> Self {
        Damaged(e.to_string())
    }
}

impl From<ContainerError> for Damaged {
    fn from(e: ContainerError) -> Self {
        Damaged(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Encoding.

fn put_rect(w: &mut Writer, r: &Rect) {
    for v in [r.x, r.y, r.w, r.h] {
        w.ivar(v);
    }
}

fn put_color(w: &mut Writer, c: &Color) {
    match c.base {
        ColorBase::Rgb(v) => {
            w.u8(0);
            w.u32(v);
        }
        ColorBase::Theme(s) => {
            w.u8(1);
            w.u8(s as u8);
        }
    }
    w.var(c.mods.len() as u64);
    for m in &c.mods {
        let (k, v) = match *m {
            ColorMod::LumMod(v) => (0, v),
            ColorMod::LumOff(v) => (1, v),
            ColorMod::Tint(v) => (2, v),
            ColorMod::Shade(v) => (3, v),
            ColorMod::SatMod(v) => (4, v),
            ColorMod::Alpha(v) => (5, v),
        };
        w.u8(k);
        w.ivar(v as i64);
    }
}

fn put_opt<T>(w: &mut Writer, v: &Option<T>, f: impl FnOnce(&mut Writer, &T)) {
    match v {
        None => w.u8(0),
        Some(x) => {
            w.u8(1);
            f(w, x);
        }
    }
}

fn put_fill(w: &mut Writer, f: &Fill) {
    match f {
        Fill::None => w.u8(0),
        Fill::Solid(c) => {
            w.u8(1);
            put_color(w, c);
        }
        Fill::Gradient { stops, angle, radial } => {
            w.u8(2);
            w.var(stops.len() as u64);
            for s in stops {
                w.var(s.pos as u64);
                put_color(w, &s.color);
            }
            w.ivar(*angle as i64);
            w.u8(*radial as u8);
        }
        Fill::Picture(m) => {
            w.u8(3);
            w.var(*m as u64);
        }
    }
}

fn put_stroke(w: &mut Writer, s: &Stroke) {
    put_opt(w, &s.color, put_color);
    w.ivar(s.width);
    w.u8(s.dash as u8);
    w.u8(s.head as u8);
    w.u8(s.tail as u8);
}

fn put_geom(w: &mut Writer, g: &Geom) {
    match g {
        Geom::Preset { name, adj } => {
            w.u8(0);
            w.str(name);
            w.var(adj.len() as u64);
            for (n, v) in adj {
                w.str(n);
                w.ivar(*v);
            }
        }
        Geom::Custom(paths) => {
            w.u8(1);
            w.var(paths.len() as u64);
            for p in paths {
                w.ivar(p.w);
                w.ivar(p.h);
                w.u8(p.filled as u8 | (p.stroked as u8) << 1);
                w.var(p.cmds.len() as u64);
                for c in &p.cmds {
                    match *c {
                        PathCmd::Move(x, y) => {
                            w.u8(0);
                            w.ivar(x);
                            w.ivar(y);
                        }
                        PathCmd::Line(x, y) => {
                            w.u8(1);
                            w.ivar(x);
                            w.ivar(y);
                        }
                        PathCmd::Cubic(a, b, c, d, e, f) => {
                            w.u8(2);
                            for v in [a, b, c, d, e, f] {
                                w.ivar(v);
                            }
                        }
                        PathCmd::Close => w.u8(3),
                    }
                }
            }
        }
    }
}

fn put_bullet(w: &mut Writer, b: &Bullet) {
    match b {
        Bullet::None => w.u8(0),
        Bullet::Char { ch, font, color, size_pct } => {
            w.u8(1);
            w.u32(*ch as u32);
            put_opt(w, font, |w, f| w.str(f));
            put_opt(w, color, put_color);
            put_opt(w, size_pct, |w, v| w.var(*v as u64));
        }
        Bullet::Number { scheme, start } => {
            w.u8(2);
            w.str(scheme);
            w.var(*start as u64);
        }
    }
}

fn put_levels(w: &mut Writer, l: &ListStyle) {
    w.var(l.len().min(MAX_LEVELS) as u64);
    for s in l.iter().take(MAX_LEVELS) {
        encode_attrs(w, &s.chars);
        put_opt(w, &s.color, put_color);
        encode_props(w, &s.para);
        put_opt(w, &s.bullet, put_bullet);
    }
}

/// A frame's paragraphs: text, level, properties and runs with their attributes.
pub fn put_text_doc(w: &mut Writer, d: &TextDoc) {
    let paras: Vec<&Para> = d.blocks.iter().filter_map(|b| b.as_para()).collect();
    w.var(paras.len() as u64);
    for p in paras {
        w.str(&p.text);
        w.var(p.style as u64);
        encode_props(w, &p.props);
        w.var(p.runs.len() as u64);
        for &(at, id) in &p.runs {
            w.var(at as u64);
            encode_attrs(w, d.attrs.get(id));
        }
    }
}

fn put_body(w: &mut Writer, t: &TextBody) {
    put_text_doc(w, &t.doc);
    put_opt(w, &t.inset, |w, i| {
        for v in i {
            w.ivar(*v);
        }
    });
    put_opt(w, &t.anchor, |w, a| w.u8(*a as u8));
    put_opt(w, &t.autofit, |w, a| match a {
        Autofit::None => w.u8(0),
        Autofit::Shrink { scale, spacing_cut } => {
            w.u8(1);
            w.var(*scale as u64);
            w.var(*spacing_cut as u64);
        }
        Autofit::Resize => w.u8(2),
    });
    put_opt(w, &t.wrap, |w, v| w.u8(*v as u8));
    put_levels(w, &t.levels);
}

fn put_shape(w: &mut Writer, s: &Shape) {
    w.var(s.id as u64);
    w.str(&s.name);
    w.str(&s.descr);
    put_opt(w, &s.ph, |w, p| {
        w.u8(p.kind as u8);
        w.var(p.idx as u64);
    });
    put_opt(w, &s.frame, put_rect);
    w.ivar(s.rot as i64);
    w.u8(s.flip_h as u8 | (s.flip_v as u8) << 1 | (s.hidden as u8) << 2);
    match &s.kind {
        ShapeKind::Geom { geom, fill, line, text } => {
            w.u8(0);
            put_opt(w, geom, put_geom);
            put_opt(w, fill, put_fill);
            put_opt(w, line, put_stroke);
            put_opt(w, text, put_body);
        }
        ShapeKind::Picture { media, crop, line } => {
            w.u8(1);
            w.var(*media as u64);
            for v in crop {
                w.ivar(*v as i64);
            }
            put_opt(w, line, put_stroke);
        }
        ShapeKind::Video { media, poster } => {
            w.u8(2);
            w.var(*media as u64);
            put_opt(w, poster, |w, p| w.var(*p as u64));
        }
        ShapeKind::Group { child, shapes } => {
            w.u8(3);
            put_rect(w, child);
            w.var(shapes.len() as u64);
            for c in shapes {
                put_shape(w, c);
            }
        }
    }
}

fn put_shapes(w: &mut Writer, v: &[Shape]) {
    w.var(v.len() as u64);
    for s in v {
        put_shape(w, s);
    }
}

fn dir_u8(d: Dir) -> u8 {
    d as u8
}

fn put_transition(w: &mut Writer, t: &Transition) {
    let (k, a, b) = match t.kind {
        TransitionKind::Cut => (0, 0, 0),
        TransitionKind::Fade => (1, 0, 0),
        TransitionKind::Push(d) => (2, dir_u8(d), 0),
        TransitionKind::Wipe(d) => (3, dir_u8(d), 0),
        TransitionKind::Cover(d) => (4, dir_u8(d), 0),
        TransitionKind::Uncover(d) => (5, dir_u8(d), 0),
        TransitionKind::Split { vertical, out } => (6, vertical as u8, out as u8),
        TransitionKind::Zoom { out } => (7, out as u8, 0),
        TransitionKind::Dissolve => (8, 0, 0),
    };
    w.u8(k);
    w.u8(a);
    w.u8(b);
    w.var(t.ms as u64);
    w.u8(t.on_click as u8);
    put_opt(w, &t.after_ms, |w, v| w.var(*v as u64));
}

fn put_build(w: &mut Writer, b: &Build) {
    w.var(b.shape as u64);
    let (k, d) = match b.effect {
        Effect::Appear => (0, 0),
        Effect::Fade => (1, 0),
        Effect::FlyIn(d) => (2, dir_u8(d)),
        Effect::Wipe(d) => (3, dir_u8(d)),
        Effect::Zoom => (4, 0),
        Effect::Disappear => (5, 0),
        Effect::FadeOut => (6, 0),
    };
    w.u8(k);
    w.u8(d);
    w.u8(b.trigger as u8);
    w.var(b.ms as u64);
    w.var(b.delay_ms as u64);
    w.u8(b.by_paragraph as u8);
}

fn put_props(w: &mut Writer, p: &SlideProps) {
    w.var(p.layout as u64);
    put_opt(w, &p.background, put_fill);
    put_opt(w, &p.notes, put_body);
    put_opt(w, &p.transition, put_transition);
    w.var(p.builds.len() as u64);
    for b in &p.builds {
        put_build(w, b);
    }
    w.u8(p.hidden as u8 | (p.hide_layout_shapes as u8) << 1);
}

pub fn put_slide(w: &mut Writer, s: &Slide) {
    w.var(s.id as u64);
    put_props(w, &s.props());
    put_shapes(w, &s.shapes);
}

fn put_theme(w: &mut Writer, t: &Theme) {
    w.str(&t.name);
    for c in t.colors {
        w.u32(c);
    }
    w.str(&t.major);
    w.str(&t.minor);
}

fn put_master(w: &mut Writer, m: &Master) {
    w.var(m.id as u64);
    w.str(&m.name);
    put_fill(w, &m.background);
    put_shapes(w, &m.shapes);
    put_levels(w, &m.title_style);
    put_levels(w, &m.body_style);
    put_levels(w, &m.other_style);
}

fn put_layout(w: &mut Writer, l: &Layout) {
    w.var(l.id as u64);
    w.var(l.master as u64);
    w.str(&l.name);
    w.str(&l.kind);
    put_opt(w, &l.background, put_fill);
    put_shapes(w, &l.shapes);
    w.u8(l.hide_master_shapes as u8);
}

// ---------------------------------------------------------------------------
// Decoding.

fn rect(r: &mut Reader) -> Result<Rect, WireError> {
    Ok(Rect { x: r.ivar()?, y: r.ivar()?, w: r.ivar()?, h: r.ivar()? })
}

fn i32v(r: &mut Reader) -> Result<i32, WireError> {
    Ok(r.ivar()?.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

fn u32v(r: &mut Reader, what: &'static str) -> Result<u32, WireError> {
    Ok(r.len(what, u32::MAX as u64)? as u32)
}

fn bad(what: &'static str) -> WireError {
    WireError::Invalid(what)
}

fn opt<T>(r: &mut Reader, f: impl FnOnce(&mut Reader) -> Result<T, WireError>) -> Result<Option<T>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(f(r)?)),
        _ => Err(bad("option tag")),
    }
}

fn color(r: &mut Reader) -> Result<Color, WireError> {
    let base = match r.u8()? {
        0 => ColorBase::Rgb(r.u32()? & 0xFF_FFFF),
        1 => ColorBase::Theme(Slot::from_u8(r.u8()?).ok_or(bad("theme slot"))?),
        _ => return Err(bad("colour")),
    };
    let n = r.count("colour changes", MAX_MODS, 2)?;
    let mut mods = Vec::with_capacity(n);
    for _ in 0..n {
        let k = r.u8()?;
        let v = i32v(r)?;
        mods.push(match k {
            0 => ColorMod::LumMod(v),
            1 => ColorMod::LumOff(v),
            2 => ColorMod::Tint(v),
            3 => ColorMod::Shade(v),
            4 => ColorMod::SatMod(v),
            5 => ColorMod::Alpha(v),
            _ => return Err(bad("colour change")),
        });
    }
    Ok(Color { base, mods })
}

fn fill(r: &mut Reader) -> Result<Fill, WireError> {
    Ok(match r.u8()? {
        0 => Fill::None,
        1 => Fill::Solid(color(r)?),
        2 => {
            let n = r.count("gradient stops", MAX_STOPS, 3)?;
            let mut stops = Vec::with_capacity(n);
            for _ in 0..n {
                let pos = r.len("stop", 100_000)? as u32;
                stops.push(GradientStop { pos, color: color(r)? });
            }
            Fill::Gradient { stops, angle: i32v(r)?, radial: r.u8()? != 0 }
        }
        3 => Fill::Picture(u32v(r, "media")?),
        _ => return Err(bad("fill")),
    })
}

fn dash(v: u8) -> Result<Dash, WireError> {
    [Dash::Solid, Dash::Dot, Dash::Dash, Dash::DashDot, Dash::LongDash].get(v as usize).copied().ok_or(bad("dash"))
}

fn arrow(v: u8) -> Result<Arrow, WireError> {
    [Arrow::None, Arrow::Triangle, Arrow::Open, Arrow::Stealth, Arrow::Diamond, Arrow::Oval].get(v as usize).copied().ok_or(bad("arrow"))
}

fn stroke(r: &mut Reader) -> Result<Stroke, WireError> {
    Ok(Stroke { color: opt(r, color)?, width: r.ivar()?.clamp(0, 20 * crate::EMU_PER_INCH), dash: dash(r.u8()?)?, head: arrow(r.u8()?)?, tail: arrow(r.u8()?)? })
}

fn geom(r: &mut Reader) -> Result<Geom, WireError> {
    Ok(match r.u8()? {
        0 => {
            let name: Arc<str> = r.str("preset", 64)?.into();
            let n = r.count("adjust values", 16, 2)?;
            let mut adj = Vec::with_capacity(n);
            for _ in 0..n {
                adj.push((Arc::from(r.str("adjust name", 64)?), r.ivar()?));
            }
            Geom::Preset { name, adj }
        }
        1 => {
            let n = r.count("paths", 1024, 4)?;
            let mut paths = Vec::with_capacity(n);
            for _ in 0..n {
                let (w, h) = (r.ivar()?, r.ivar()?);
                let f = r.u8()?;
                let m = r.count("path steps", MAX_PATH_CMDS, 1)?;
                let mut cmds = Vec::with_capacity(m);
                for _ in 0..m {
                    cmds.push(match r.u8()? {
                        0 => PathCmd::Move(r.ivar()?, r.ivar()?),
                        1 => PathCmd::Line(r.ivar()?, r.ivar()?),
                        2 => PathCmd::Cubic(r.ivar()?, r.ivar()?, r.ivar()?, r.ivar()?, r.ivar()?, r.ivar()?),
                        3 => PathCmd::Close,
                        _ => return Err(bad("path step")),
                    });
                }
                paths.push(Path { w, h, cmds, filled: f & 1 != 0, stroked: f & 2 != 0 });
            }
            Geom::Custom(paths)
        }
        _ => return Err(bad("geometry")),
    })
}

fn bullet(r: &mut Reader) -> Result<Bullet, WireError> {
    Ok(match r.u8()? {
        0 => Bullet::None,
        1 => Bullet::Char { ch: char::from_u32(r.u32()?).ok_or(bad("bullet"))?, font: opt(r, |r| Ok(Arc::from(r.str("font", 256)?)))?, color: opt(r, color)?, size_pct: opt(r, |r| Ok(r.len("bullet size", 400_000)? as u32))? },
        2 => Bullet::Number { scheme: r.str("scheme", 64)?.into(), start: u32v(r, "start")? },
        _ => return Err(bad("bullet")),
    })
}

fn levels(r: &mut Reader) -> Result<ListStyle, WireError> {
    let n = r.len("levels", MAX_LEVELS as u64)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(LevelStyle { chars: decode_attrs(r)?, color: opt(r, color)?, para: decode_props(r)?, bullet: opt(r, bullet)? });
    }
    Ok(v)
}

pub fn text_doc(r: &mut Reader) -> Result<TextDoc, WireError> {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let n = r.count("paragraphs", MAX_PARAS, 4)?;
    for i in 0..n {
        let text = r.str("text", MAX_TEXT)?.to_string();
        let style = r.len("level", MAX_LEVELS as u64 - 1)? as u16;
        let props = decode_props(r)?;
        let m = r.count("runs", MAX_RUNS, 2)?;
        let mut runs: Vec<(u32, AttrId)> = Vec::with_capacity(m);
        for _ in 0..m {
            let at = r.len("run start", text.len() as u64)? as u32;
            let a = decode_attrs(r)?;
            if !text.is_char_boundary(at as usize) || runs.last().is_some_and(|l| l.0 >= at) || (runs.is_empty() && at != 0) {
                return Err(bad("run start"));
            }
            runs.push((at, d.attrs.intern(a)));
        }
        if runs.is_empty() {
            runs.push((0, 0));
        }
        let id = d.new_id();
        let mut p = Para::new(id, "", style);
        p.text = text;
        p.runs = runs;
        p.props = props;
        d.blocks.insert(i, Block::Para(p));
    }
    if d.blocks.len() == 0 {
        let id = d.new_id();
        d.blocks.insert(0, Block::Para(Para::new(id, "", 0)));
    }
    d.take_changes();
    Ok(d)
}

fn body(r: &mut Reader) -> Result<TextBody, WireError> {
    let doc = text_doc(r)?;
    let inset = opt(r, |r| Ok([r.ivar()?, r.ivar()?, r.ivar()?, r.ivar()?]))?;
    let anchor = opt(r, |r| [Anchor::Top, Anchor::Middle, Anchor::Bottom].get(r.u8()? as usize).copied().ok_or(bad("anchor")))?;
    let autofit = opt(r, |r| {
        Ok(match r.u8()? {
            0 => Autofit::None,
            1 => Autofit::Shrink { scale: r.len("scale", 100_000)? as u32, spacing_cut: r.len("spacing", 100_000)? as u32 },
            2 => Autofit::Resize,
            _ => return Err(bad("autofit")),
        })
    })?;
    let wrap = opt(r, |r| Ok(r.u8()? != 0))?;
    Ok(TextBody { doc, inset, anchor, autofit, wrap, levels: levels(r)? })
}

fn shape(r: &mut Reader, depth: usize, count: &mut usize) -> Result<Shape, WireError> {
    *count += 1;
    if depth > MAX_DEPTH || *count > MAX_SHAPES {
        return Err(bad("shapes"));
    }
    let id = u32v(r, "shape id")?;
    let name = r.str("name", MAX_STR)?.into();
    let descr = r.str("description", MAX_STR)?.into();
    let ph = opt(r, |r| Ok(Placeholder { kind: PhKind::from_u8(r.u8()?).ok_or(bad("placeholder"))?, idx: u32v(r, "placeholder index")? }))?;
    let frame = opt(r, rect)?;
    let rot = i32v(r)?;
    let flags = r.u8()?;
    let kind = match r.u8()? {
        0 => ShapeKind::Geom { geom: opt(r, geom)?, fill: opt(r, fill)?, line: opt(r, stroke)?, text: opt(r, body)? },
        1 => ShapeKind::Picture { media: u32v(r, "media")?, crop: [i32v(r)?, i32v(r)?, i32v(r)?, i32v(r)?], line: opt(r, stroke)? },
        2 => ShapeKind::Video { media: u32v(r, "media")?, poster: opt(r, |r| u32v(r, "poster"))? },
        3 => {
            let child = rect(r)?;
            let n = r.count("group shapes", MAX_SHAPES as u64, 8)?;
            let mut shapes = Vec::with_capacity(n);
            for _ in 0..n {
                shapes.push(shape(r, depth + 1, count)?);
            }
            ShapeKind::Group { child, shapes }
        }
        _ => return Err(bad("shape kind")),
    };
    Ok(Shape { id, name, descr, ph, frame, rot, flip_h: flags & 1 != 0, flip_v: flags & 2 != 0, hidden: flags & 4 != 0, kind })
}

fn shapes(r: &mut Reader) -> Result<Vec<Shape>, WireError> {
    let n = r.count("shapes", MAX_SHAPES as u64, 8)?;
    let mut v = Vec::with_capacity(n);
    let mut count = 0;
    for _ in 0..n {
        v.push(shape(r, 0, &mut count)?);
    }
    Ok(v)
}

fn dir(v: u8) -> Result<Dir, WireError> {
    Dir::from_u8(v).ok_or(bad("direction"))
}

fn transition(r: &mut Reader) -> Result<Transition, WireError> {
    let (k, a, b) = (r.u8()?, r.u8()?, r.u8()?);
    let kind = match k {
        0 => TransitionKind::Cut,
        1 => TransitionKind::Fade,
        2 => TransitionKind::Push(dir(a)?),
        3 => TransitionKind::Wipe(dir(a)?),
        4 => TransitionKind::Cover(dir(a)?),
        5 => TransitionKind::Uncover(dir(a)?),
        6 => TransitionKind::Split { vertical: a != 0, out: b != 0 },
        7 => TransitionKind::Zoom { out: a != 0 },
        8 => TransitionKind::Dissolve,
        _ => return Err(bad("transition")),
    };
    Ok(Transition { kind, ms: r.len("duration", 600_000)? as u32, on_click: r.u8()? != 0, after_ms: opt(r, |r| Ok(r.len("advance", 86_400_000)? as u32))? })
}

fn build(r: &mut Reader) -> Result<Build, WireError> {
    let shape = u32v(r, "build shape")?;
    let (k, d) = (r.u8()?, r.u8()?);
    let effect = match k {
        0 => Effect::Appear,
        1 => Effect::Fade,
        2 => Effect::FlyIn(dir(d)?),
        3 => Effect::Wipe(dir(d)?),
        4 => Effect::Zoom,
        5 => Effect::Disappear,
        6 => Effect::FadeOut,
        _ => return Err(bad("effect")),
    };
    let trigger = [Trigger::OnClick, Trigger::WithPrevious, Trigger::AfterPrevious].get(r.u8()? as usize).copied().ok_or(bad("trigger"))?;
    Ok(Build { shape, effect, trigger, ms: r.len("duration", 600_000)? as u32, delay_ms: r.len("delay", 600_000)? as u32, by_paragraph: r.u8()? != 0 })
}

fn props(r: &mut Reader) -> Result<SlideProps, WireError> {
    let layout = u32v(r, "layout")?;
    let background = opt(r, fill)?;
    let notes = opt(r, body)?;
    let transition = opt(r, transition)?;
    let n = r.count("builds", MAX_BUILDS, 7)?;
    let mut builds = Vec::with_capacity(n);
    for _ in 0..n {
        builds.push(build(r)?);
    }
    let f = r.u8()?;
    Ok(SlideProps { layout, background, notes, transition, builds, hidden: f & 1 != 0, hide_layout_shapes: f & 2 != 0 })
}

pub fn slide(r: &mut Reader) -> Result<Slide, WireError> {
    let id = u32v(r, "slide id")?;
    let p = props(r)?;
    let mut s = Slide::new(id, p.layout);
    s.set_props(p);
    s.shapes = shapes(r)?;
    Ok(s)
}

fn theme(r: &mut Reader) -> Result<Theme, WireError> {
    let name = r.str("theme", MAX_STR)?.into();
    let mut colors = [0u32; 12];
    for c in colors.iter_mut() {
        *c = r.u32()? & 0xFF_FFFF;
    }
    Ok(Theme { name, colors, major: r.str("font", 256)?.into(), minor: r.str("font", 256)?.into() })
}

fn master(r: &mut Reader) -> Result<Master, WireError> {
    Ok(Master { id: u32v(r, "master id")?, name: r.str("name", MAX_STR)?.into(), background: fill(r)?, shapes: shapes(r)?, title_style: levels(r)?, body_style: levels(r)?, other_style: levels(r)? })
}

fn layout(r: &mut Reader) -> Result<Layout, WireError> {
    Ok(Layout { id: u32v(r, "layout id")?, master: u32v(r, "master")?, name: r.str("name", MAX_STR)?.into(), kind: r.str("kind", 64)?.into(), background: opt(r, fill)?, shapes: shapes(r)?, hide_master_shapes: r.u8()? != 0 })
}

fn done(r: &Reader) -> Result<(), WireError> {
    if r.is_empty() {
        Ok(())
    } else {
        Err(bad("trailing bytes"))
    }
}

// ---------------------------------------------------------------------------
// Files.

/// Writes `deck`. Slides and media still in the deck's file are copied
/// from it as they are, without decoding.
pub fn save<W: Write>(deck: &Deck, out: W) -> io::Result<W> {
    let err = |d: Damaged| io::Error::new(io::ErrorKind::InvalidData, d.0);
    let mut c = ContainerWriter::new(out, DocKind::Slides)?;
    let mut slides = Vec::with_capacity(deck.slides.len());
    let mut used = std::collections::BTreeSet::new();
    for slot in &deck.slides {
        match slot {
            SlideSlot::Loaded(s) => {
                let mut w = Writer::new();
                put_slide(&mut w, s);
                slides.push((s.id, c.add(SLIDE, &w.into_bytes())?));
                media_of_slide(s, &mut used);
            }
            SlideSlot::InFile { id, chunk } => {
                let b = deck.backing.as_ref().ok_or_else(|| err(Damaged("no file".into())))?;
                let bytes = b.chunk(*chunk).map_err(err)?;
                slides.push((*id, c.add(SLIDE, bytes)?));
                // Its media: read the slide to know which (not kept).
                let s = slide(&mut Reader::new(bytes)).map_err(|e| err(e.into()))?;
                media_of_slide(&s, &mut used);
            }
        }
    }
    for m in &deck.masters {
        media_of_shapes(&m.shapes, &mut used);
        if let Fill::Picture(id) = m.background {
            used.insert(id);
        }
    }
    for l in &deck.layouts {
        media_of_shapes(&l.shapes, &mut used);
        if let Some(Fill::Picture(id)) = l.background {
            used.insert(id);
        }
    }
    let mut table = Vec::new();
    for id in used {
        let Some(m) = deck.media.get(&id) else { continue };
        let mut pieces = Vec::new();
        match &m.bytes {
            MediaBytes::Loaded(b) => {
                for p in b.chunks(MEDIA_PIECE) {
                    pieces.push(c.add(MEDIA, p)?);
                }
                if b.is_empty() {
                    pieces.push(c.add(MEDIA, &[])?);
                }
            }
            MediaBytes::InFile(chunks) => {
                let b = deck.backing.as_ref().ok_or_else(|| err(Damaged("no file".into())))?;
                for &k in chunks {
                    pieces.push(c.add(MEDIA, b.chunk(k).map_err(err)?)?);
                }
            }
        }
        table.push((id, m, pieces));
    }
    let mut w = Writer::new();
    w.var(deck.masters.len() as u64);
    for m in &deck.masters {
        put_master(&mut w, m);
    }
    w.var(deck.layouts.len() as u64);
    for l in &deck.layouts {
        put_layout(&mut w, l);
    }
    c.add(MASTERS, &w.into_bytes())?;
    let mut w = Writer::new();
    w.ivar(deck.size.0);
    w.ivar(deck.size.1);
    w.var(deck.next_id as u64);
    put_theme(&mut w, &deck.theme);
    w.var(slides.len() as u64);
    for (id, k) in &slides {
        w.var(*id as u64);
        w.var(*k as u64);
    }
    w.var(table.len() as u64);
    for (id, m, pieces) in &table {
        w.var(*id as u64);
        w.str(&m.mime);
        w.bytes(&m.hash);
        w.var(m.len);
        w.var(pieces.len() as u64);
        for p in pieces {
            w.var(*p as u64);
        }
    }
    c.add(DECK, &w.into_bytes())?;
    c.finish()
}

pub fn save_to_vec(deck: &Deck) -> Vec<u8> {
    save(deck, Vec::new()).expect("writing to memory")
}

fn media_of_shapes(v: &[Shape], used: &mut std::collections::BTreeSet<MediaId>) {
    for s in v {
        s.walk(&mut |x| match &x.kind {
            ShapeKind::Picture { media, .. } => {
                used.insert(*media);
            }
            ShapeKind::Video { media, poster } => {
                used.insert(*media);
                used.extend(*poster);
            }
            ShapeKind::Geom { fill: Some(Fill::Picture(m)), .. } => {
                used.insert(*m);
            }
            _ => {}
        });
    }
}

fn media_of_slide(s: &Slide, used: &mut std::collections::BTreeSet<MediaId>) {
    media_of_shapes(&s.shapes, used);
    if let Some(Fill::Picture(m)) = s.background {
        used.insert(m);
    }
}

/// Opens a deck: reads the index, DECK and MASTERS; slides and media stay in
/// `data` until needed. Never panics on any input.
pub fn load(data: Arc<dyn AsRef<[u8]> + Send + Sync>) -> Result<Deck, Damaged> {
    let bytes: &[u8] = (*data).as_ref();
    let c = ContainerReader::open(bytes, Limits { max_chunk: MEDIA_PIECE as u32 + (1 << 20), max_chunks: 1 << 22 })?;
    if c.kind != DocKind::Slides {
        return Err(Damaged("not a slides document".into()));
    }
    let one = |k: u16, what: &str| -> Result<usize, Damaged> {
        let mut it = c.entries().iter().enumerate().filter(|(_, e)| e.kind == k);
        let (i, _) = it.next().ok_or_else(|| Damaged(format!("no {what}")))?;
        if it.next().is_some() {
            return Err(Damaged(format!("two {what}")));
        }
        Ok(i)
    };
    let backing = Backing { entries: Arc::new(c.entries().to_vec()), data: data.clone() };
    let mut deck = Deck::empty();
    let dk = one(DECK, "deck")?;
    let mut r = Reader::new(c.chunk(dk)?);
    let side = |v: i64| v.clamp(crate::ops::EMU_MIN_SIDE, crate::ops::EMU_MAX_SIDE);
    deck.size = (side(r.ivar()?), side(r.ivar()?));
    let watermark = u32v(&mut r, "ids")?;
    deck.theme = theme(&mut r)?;
    let n = r.count("slides", MAX_SLIDES as u64, 2)?;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..n {
        let id = u32v(&mut r, "slide id")?;
        let chunk = r.len("slide chunk", u32::MAX as u64)?;
        if backing.kind(chunk) != Some(SLIDE) || !seen.insert(id) {
            return Err(Damaged("slide list".into()));
        }
        deck.slides.push(SlideSlot::InFile { id, chunk });
    }
    let n = r.count("media", MAX_MEDIA as u64, 20)?;
    for _ in 0..n {
        let id = u32v(&mut r, "media id")?;
        let mime: Arc<str> = r.str("media type", 256)?.into();
        let h = r.bytes("hash", 16)?;
        let hash: [u8; 16] = h.try_into().map_err(|_| Damaged("media hash".into()))?;
        let len = r.len("media length", MAX_MEDIA_LEN)? as u64;
        let k = r.count("media pieces", MAX_MEDIA_LEN / MEDIA_PIECE as u64 + 1, 1)?;
        let mut chunks = Vec::with_capacity(k);
        let mut total = 0u64;
        for _ in 0..k {
            let ch = r.len("media chunk", u32::MAX as u64)?;
            if backing.kind(ch) != Some(MEDIA) {
                return Err(Damaged("media table".into()));
            }
            total += backing.entries[ch].len as u64;
            chunks.push(ch);
        }
        if total != len || deck.media.contains_key(&id) {
            return Err(Damaged("media table".into()));
        }
        deck.by_hash.entry(hash).or_insert(id);
        deck.media.insert(id, MediaItem { mime, hash, len, bytes: MediaBytes::InFile(chunks) });
    }
    done(&r)?;
    let mk = one(MASTERS, "masters")?;
    let mut r = Reader::new(c.chunk(mk)?);
    let n = r.count("masters", MAX_MASTERS as u64, 8)?;
    for _ in 0..n {
        deck.masters.push(master(&mut r)?);
    }
    let n = r.count("layouts", MAX_LAYOUTS as u64, 8)?;
    for _ in 0..n {
        deck.layouts.push(layout(&mut r)?);
    }
    done(&r)?;
    deck.next_id = watermark.max(1);
    let ids: Vec<u32> = deck.masters.iter().map(|m| m.id).chain(deck.layouts.iter().map(|l| l.id)).chain(deck.media.keys().copied()).chain(deck.slides.iter().map(|s| s.id())).collect();
    for id in ids {
        deck.see_id(id);
    }
    deck.backing = Some(backing);
    Ok(deck)
}

pub(crate) fn read_slide(deck: &Deck, chunk: usize) -> Result<Slide, Damaged> {
    let b = deck.backing.as_ref().ok_or_else(|| Damaged("no file".into()))?;
    let mut r = Reader::new(b.chunk(chunk)?);
    let s = slide(&mut r)?;
    done(&r)?;
    Ok(s)
}

pub(crate) fn read_media(deck: &Deck, chunks: &[usize], len: u64) -> Result<Arc<[u8]>, Damaged> {
    let b = deck.backing.as_ref().ok_or_else(|| Damaged("no file".into()))?;
    let mut v = Vec::with_capacity(len as usize);
    for &k in chunks {
        v.extend_from_slice(b.chunk(k)?);
    }
    Ok(v.into())
}

// ---------------------------------------------------------------------------
// The autosave journal: one record per op. A text edit is journaled as the
// frame it left (frames are small), so a record never depends on a frame's
// attribute table.

fn put_target(w: &mut Writer, t: Target) {
    let (k, id) = match t {
        Target::Slide(i) => (0, i),
        Target::Layout(i) => (1, i),
        Target::Master(i) => (2, i),
    };
    w.u8(k);
    w.var(id as u64);
}

fn target(r: &mut Reader) -> Result<Target, WireError> {
    let k = r.u8()?;
    let id = u32v(r, "target")?;
    Ok(match k {
        0 => Target::Slide(id),
        1 => Target::Layout(id),
        2 => Target::Master(id),
        _ => return Err(bad("target")),
    })
}

/// The journal record for `op`, applied to `deck` already.
pub fn encode_record(deck: &Deck, op: &DeckOp) -> Vec<u8> {
    let mut w = Writer::new();
    match op {
        DeckOp::Nop => w.u8(0),
        DeckOp::InsertSlide { at, slide } => {
            w.u8(1);
            w.var(*at as u64);
            put_slide(&mut w, slide);
        }
        DeckOp::RemoveSlide { id } => {
            w.u8(2);
            w.var(*id as u64);
        }
        DeckOp::MoveSlide { id, to } => {
            w.u8(3);
            w.var(*id as u64);
            w.var(*to as u64);
        }
        DeckOp::SetSlide { id, props } => {
            w.u8(4);
            w.var(*id as u64);
            put_props(&mut w, props);
        }
        DeckOp::InsertShape { target, group, at, shape } => {
            w.u8(5);
            put_target(&mut w, *target);
            put_opt(&mut w, group, |w, g| w.var(*g as u64));
            w.var(*at as u64);
            put_shape(&mut w, shape);
        }
        DeckOp::RemoveShape { target, id } => {
            w.u8(6);
            put_target(&mut w, *target);
            w.var(*id as u64);
        }
        DeckOp::SetShape { target, shape } => {
            w.u8(7);
            put_target(&mut w, *target);
            put_shape(&mut w, shape);
        }
        DeckOp::MoveShape { target, id, to } => {
            w.u8(8);
            put_target(&mut w, *target);
            w.var(*id as u64);
            w.var(*to as u64);
        }
        DeckOp::Text { target, shape, .. } => {
            // The frame as it is now.
            let now = frame_now(deck, *target, *shape);
            w.u8(9);
            put_target(&mut w, *target);
            w.var(*shape as u64);
            put_opt(&mut w, &now, put_body);
        }
        DeckOp::SetTheme(t) => {
            w.u8(10);
            put_theme(&mut w, t);
        }
        DeckOp::SetSize(a, b) => {
            w.u8(11);
            w.ivar(*a);
            w.ivar(*b);
        }
        DeckOp::PutMaster { id, at, master } => {
            w.u8(12);
            w.var(*id as u64);
            w.var(*at as u64);
            put_opt(&mut w, master, |w, m| put_master(w, m));
        }
        DeckOp::PutLayout { id, at, layout } => {
            w.u8(13);
            w.var(*id as u64);
            w.var(*at as u64);
            put_opt(&mut w, layout, |w, l| put_layout(w, l));
        }
    }
    w.into_bytes()
}

fn frame_now(deck: &Deck, t: Target, shape: ShapeId) -> Option<TextBody> {
    let shapes = match t {
        Target::Slide(id) => {
            let s = deck.slide_if_loaded(deck.slide_index(id)?)?;
            if shape == crate::ops::NOTES {
                return s.notes.clone();
            }
            &s.shapes
        }
        Target::Layout(id) => &deck.layout(id)?.shapes,
        Target::Master(id) => &deck.master(id)?.shapes,
    };
    find(shapes, shape)?.text().cloned()
}

/// A journal record as the op that redoes it on `deck`.
pub fn decode_record(deck: &mut Deck, b: &[u8]) -> Result<DeckOp, Damaged> {
    let mut r = Reader::new(b);
    let op = match r.u8()? {
        0 => DeckOp::Nop,
        1 => DeckOp::InsertSlide { at: r.len("index", MAX_SLIDES as u64)?, slide: Box::new(slide(&mut r)?) },
        2 => DeckOp::RemoveSlide { id: u32v(&mut r, "slide")? },
        3 => DeckOp::MoveSlide { id: u32v(&mut r, "slide")?, to: r.len("index", MAX_SLIDES as u64)? },
        4 => DeckOp::SetSlide { id: u32v(&mut r, "slide")?, props: Box::new(props(&mut r)?) },
        5 => {
            let target = target(&mut r)?;
            let group = opt(&mut r, |r| u32v(r, "group"))?;
            let at = r.len("index", MAX_SHAPES as u64)?;
            DeckOp::InsertShape { target, group, at, shape: Box::new(shape(&mut r, 0, &mut 0)?) }
        }
        6 => DeckOp::RemoveShape { target: target(&mut r)?, id: u32v(&mut r, "shape")? },
        7 => DeckOp::SetShape { target: target(&mut r)?, shape: Box::new(shape(&mut r, 0, &mut 0)?) },
        8 => DeckOp::MoveShape { target: target(&mut r)?, id: u32v(&mut r, "shape")?, to: r.len("index", MAX_SHAPES as u64)? },
        9 => {
            let t = target(&mut r)?;
            let id = u32v(&mut r, "shape")?;
            let now = opt(&mut r, body)?;
            done(&r)?;
            return Ok(frame_op(deck, t, id, now));
        }
        10 => DeckOp::SetTheme(Box::new(theme(&mut r)?)),
        11 => DeckOp::SetSize(r.ivar()?, r.ivar()?),
        12 => DeckOp::PutMaster { id: u32v(&mut r, "master")?, at: r.len("index", MAX_MASTERS as u64)?, master: opt(&mut r, master)?.map(Box::new) },
        13 => DeckOp::PutLayout { id: u32v(&mut r, "layout")?, at: r.len("index", MAX_LAYOUTS as u64)?, layout: opt(&mut r, layout)?.map(Box::new) },
        _ => return Err(Damaged("journal record".into())),
    };
    done(&r)?;
    Ok(op)
}

/// The op that sets a frame's text to `now`.
fn frame_op(deck: &mut Deck, t: Target, id: ShapeId, now: Option<TextBody>) -> DeckOp {
    if id == crate::ops::NOTES {
        let Target::Slide(sid) = t else { return DeckOp::Nop };
        let Some(i) = deck.slide_index(sid) else { return DeckOp::Nop };
        let Ok(s) = deck.slide(i) else { return DeckOp::Nop };
        let mut p = s.props();
        p.notes = now;
        return DeckOp::SetSlide { id: sid, props: Box::new(p) };
    }
    let shapes = match t {
        Target::Slide(sid) => {
            let Some(i) = deck.slide_index(sid) else { return DeckOp::Nop };
            match deck.slide(i) {
                Ok(s) => &s.shapes,
                Err(_) => return DeckOp::Nop,
            }
        }
        Target::Layout(l) => match deck.layout(l) {
            Some(l) => &l.shapes,
            None => return DeckOp::Nop,
        },
        Target::Master(m) => match deck.master(m) {
            Some(m) => &m.shapes,
            None => return DeckOp::Nop,
        },
    };
    let Some(mut s) = find(shapes, id).cloned() else { return DeckOp::Nop };
    match &mut s.kind {
        ShapeKind::Geom { text, .. } => *text = now,
        _ => return DeckOp::Nop,
    }
    DeckOp::SetShape { target: t, shape: Box::new(s) }
}

/// Media referred to by id but missing from the deck, by owner (for checks).
pub fn missing_media(deck: &mut Deck) -> Result<Vec<MediaId>, Damaged> {
    let mut used = std::collections::BTreeSet::new();
    for i in 0..deck.slide_count() {
        media_of_slide(deck.slide(i)?, &mut used);
    }
    for m in &deck.masters {
        media_of_shapes(&m.shapes, &mut used);
    }
    for l in &deck.layouts {
        media_of_shapes(&l.shapes, &mut used);
    }
    Ok(used.into_iter().filter(|id| !deck.media.contains_key(id)).collect())
}
