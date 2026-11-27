//! A slide as a display list (docs/spec/officina.md §9.1): paths, pictures,
//! video posters and text, each with the transform that puts it on the
//! slide. The editor's canvas, thumbnails, presenting and PDF all draw from
//! it, so a slide looks the same in each.

use std::sync::Arc;

use officina_text::layout::{layout_para, size_lu, Fonts, ParaLayout};
use officina_text::model::{CharAttrs, CharFormat, ListKind, ListRef, Para, ParaProps, Style, StyleKind, StyleSheet, TextDoc};
use officina_text::paint::{list_label_format, paint_para, Item};

use crate::geom;
use crate::model::*;
use crate::resolve::{self, color, Owner, Resolved, Rgba};

/// An affine map `[a, b, c, d, e, f]`: x' = a·x + c·y + e, y' = b·x + d·y + f.
pub type Xform = [f64; 6];

pub const IDENTITY: Xform = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `m` after `n`: a point goes through `n`, then `m`.
pub fn mul(m: &Xform, n: &Xform) -> Xform {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

pub fn apply(m: &Xform, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    Solid(Rgba),
    /// From `from` to `to` in the item's own coordinates; stops 0 to 1.
    Linear { stops: Vec<(f32, Rgba)>, from: (f64, f64), to: (f64, f64) },
    Radial { stops: Vec<(f32, Rgba)>, center: (f64, f64), radius: f64 },
    /// A picture stretched over the item's box.
    Picture(MediaId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrokePaint {
    pub color: Rgba,
    /// EMU, in the item's own coordinates.
    pub width: f64,
    pub dash: Dash,
    pub head: Arrow,
    pub tail: Arrow,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DrawItem {
    /// Paths in a `w` × `h` box (EMU), `xf` onto the slide.
    Path { xf: Xform, w: f64, h: f64, paths: Vec<Path>, fill: Option<Paint>, stroke: Option<StrokePaint> },
    /// A picture filling `0..w`, `0..h`, cropped (thousandths of a percent).
    Picture { xf: Xform, w: f64, h: f64, media: MediaId, crop: [i32; 4] },
    /// A video's place: its poster until the media player takes over.
    Video { xf: Xform, w: f64, h: f64, media: MediaId, poster: Option<MediaId> },
    /// Text in layout units, `xf` onto the slide; `shape` is the frame's shape.
    Text { xf: Xform, shape: ShapeId, items: Vec<Item> },
}

/// What a drawing pass noticed (presets drawn as boxes, slides not readable).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Notes {
    pub unknown_presets: Vec<Arc<str>>,
    /// A text frame overflowed or its paint stream omitted or reordered text.
    pub incomplete_text: bool,
    verify_text: bool,
}

impl Notes {
    /// Ask the drawing pass to check text coverage and frame height.
    pub fn checking_text() -> Self {
        Self { verify_text: true, ..Self::default() }
    }
}

/// The map from a shape's own box (`0..w`, `0..h`) onto its parent's
/// coordinates: rotated about its middle, flipped.
pub fn shape_xform(f: Rect, rot: i32, flip_h: bool, flip_v: bool) -> Xform {
    let (cx, cy) = (f.x as f64 + f.w as f64 / 2.0, f.y as f64 + f.h as f64 / 2.0);
    let t = (rot as f64 / 60_000.0).to_radians();
    let (s, c) = t.sin_cos();
    let (fx, fy) = (if flip_h { -1.0 } else { 1.0 }, if flip_v { -1.0 } else { 1.0 });
    let rs = [c * fx, s * fx, -s * fy, c * fy, cx, cy];
    mul(&rs, &[1.0, 0.0, 0.0, 1.0, -(f.w as f64) / 2.0, -(f.h as f64) / 2.0])
}

fn stops(theme: &Theme, s: &[GradientStop]) -> Vec<(f32, Rgba)> {
    s.iter().map(|g| (g.pos.min(100_000) as f32 / 100_000.0, color(theme, &g.color))).collect()
}

/// A fill as a paint over a `w` × `h` box.
pub fn paint(theme: &Theme, f: &Fill, w: f64, h: f64) -> Option<Paint> {
    match f {
        Fill::None => None,
        Fill::Solid(c) => Some(Paint::Solid(color(theme, c))),
        Fill::Picture(m) => Some(Paint::Picture(*m)),
        Fill::Gradient { stops: s, angle, radial } => {
            let st = stops(theme, s);
            if *radial {
                return Some(Paint::Radial { stops: st, center: (w / 2.0, h / 2.0), radius: (w * w + h * h).sqrt() / 2.0 });
            }
            let t = (*angle as f64 / 60_000.0).to_radians();
            let (dx, dy) = (t.cos(), t.sin());
            let half = ((w * dx).abs() + (h * dy).abs()) / 2.0;
            let (cx, cy) = (w / 2.0, h / 2.0);
            Some(Paint::Linear { stops: st, from: (cx - dx * half, cy - dy * half), to: (cx + dx * half, cy + dy * half) })
        }
    }
}

fn stroke(theme: &Theme, s: &Stroke) -> Option<StrokePaint> {
    let c = s.color.as_ref()?;
    Some(StrokePaint { color: color(theme, c), width: s.width.max(0) as f64, dash: s.dash, head: s.head, tail: s.tail })
}

/// Slide `i`'s display list.
pub fn draw_slide(deck: &mut Deck, i: usize, fonts: &dyn Fonts, notes: &mut Notes) -> Result<Vec<DrawItem>, Damaged> {
    deck.load_slide(i)?;
    let deck = &*deck;
    let slide = deck.slide_if_loaded(i).ok_or_else(|| Damaged(format!("slide {i}")))?;
    let layout = deck.layout(slide.layout);
    let master = layout.and_then(|l| deck.master(l.master));
    let mut out = Vec::new();
    let bg = slide.background.as_ref().or(layout.and_then(|l| l.background.as_ref())).or(master.map(|m| &m.background));
    background(deck, bg, &mut out);
    let n = (i + 1).to_string();
    if !slide.hide_layout_shapes {
        if let (Some(m), Some(l)) = (master, layout) {
            if !l.hide_master_shapes {
                shapes(deck, Owner::Master, &m.shapes, &IDENTITY, false, &n, fonts, &mut out, notes);
            }
            shapes(deck, Owner::Layout(m.id), &l.shapes, &IDENTITY, false, &n, fonts, &mut out, notes);
        }
    }
    shapes(deck, Owner::Slide(slide.layout), &slide.shapes, &IDENTITY, true, &n, fonts, &mut out, notes);
    Ok(out)
}

/// A run of a slide's display list: what one shape a build moves draws
/// (`shape`), or the shapes between such (`None`), in drawing order.
#[derive(Debug, Clone)]
pub struct Part {
    pub shape: Option<ShapeId>,
    pub items: Vec<DrawItem>,
}

/// Slide `i`'s display list cut into runs, each top-level shape of `apart`
/// in a run of its own, so presenting can show, hide and move it alone
/// (a build) while keeping the drawing order.
pub fn draw_slide_parts(deck: &mut Deck, i: usize, apart: &[ShapeId], fonts: &dyn Fonts, notes: &mut Notes) -> Result<Vec<Part>, Damaged> {
    deck.load_slide(i)?;
    let deck = &*deck;
    let slide = deck.slide_if_loaded(i).ok_or_else(|| Damaged(format!("slide {i}")))?;
    let layout = deck.layout(slide.layout);
    let master = layout.and_then(|l| deck.master(l.master));
    let mut base = Vec::new();
    let bg = slide.background.as_ref().or(layout.and_then(|l| l.background.as_ref())).or(master.map(|m| &m.background));
    background(deck, bg, &mut base);
    let n = (i + 1).to_string();
    if !slide.hide_layout_shapes {
        if let (Some(m), Some(l)) = (master, layout) {
            if !l.hide_master_shapes {
                shapes(deck, Owner::Master, &m.shapes, &IDENTITY, false, &n, fonts, &mut base, notes);
            }
            shapes(deck, Owner::Layout(m.id), &l.shapes, &IDENTITY, false, &n, fonts, &mut base, notes);
        }
    }
    let mut parts = vec![Part { shape: None, items: base }];
    for s in &slide.shapes {
        if s.hidden {
            continue;
        }
        let own = apart.contains(&s.id);
        if own || parts.last().is_some_and(|p| p.shape.is_some()) {
            parts.push(Part { shape: own.then_some(s.id), items: Vec::new() });
        }
        let last = parts.last_mut().expect("a part");
        shape(deck, Owner::Slide(slide.layout), s, &IDENTITY, &n, fonts, &mut last.items, notes);
    }
    parts.retain(|p| p.shape.is_some() || !p.items.is_empty());
    Ok(parts)
}

/// A layout's display list, placeholders shown (for editing layouts).
pub fn draw_layout(deck: &Deck, id: LayoutId, fonts: &dyn Fonts, notes: &mut Notes) -> Vec<DrawItem> {
    let mut out = Vec::new();
    let Some(l) = deck.layout(id) else { return out };
    let m = deck.master(l.master);
    background(deck, l.background.as_ref().or(m.map(|m| &m.background)), &mut out);
    if let (Some(m), false) = (m, l.hide_master_shapes) {
        shapes(deck, Owner::Master, &m.shapes, &IDENTITY, false, "‹#›", fonts, &mut out, notes);
    }
    shapes(deck, Owner::Layout(l.master), &l.shapes, &IDENTITY, true, "‹#›", fonts, &mut out, notes);
    out
}

fn background(deck: &Deck, bg: Option<&Fill>, out: &mut Vec<DrawItem>) {
    let (w, h) = (deck.size.0 as f64, deck.size.1 as f64);
    let fill = bg.and_then(|f| paint(&deck.theme, f, w, h)).unwrap_or(Paint::Solid(0xFFFF_FFFF));
    out.push(DrawItem::Path { xf: IDENTITY, w, h, paths: geom::paths(&Geom::preset("rect"), w as i64, h as i64).0, fill: Some(fill), stroke: None });
}

/// Draws `list`; placeholders only when `with_ph` (a slide's own, or a
/// layout being edited), since a layout's placeholders are prompts.
#[allow(clippy::too_many_arguments)]
fn shapes(deck: &Deck, owner: Owner, list: &[Shape], base: &Xform, with_ph: bool, slide_no: &str, fonts: &dyn Fonts, out: &mut Vec<DrawItem>, notes: &mut Notes) {
    for s in list {
        if s.hidden || (!with_ph && s.ph.is_some()) {
            continue;
        }
        shape(deck, owner, s, base, slide_no, fonts, out, notes);
    }
}

#[allow(clippy::too_many_arguments)]
fn shape(deck: &Deck, owner: Owner, s: &Shape, base: &Xform, slide_no: &str, fonts: &dyn Fonts, out: &mut Vec<DrawItem>, notes: &mut Notes) {
    let r = resolve::resolve(deck, owner, s);
    let f = r.frame;
    let xf = mul(base, &shape_xform(f, s.rot, s.flip_h, s.flip_v));
    let (w, h) = (f.w as f64, f.h as f64);
    match &s.kind {
        ShapeKind::Geom { text, .. } => {
            let (paths, known) = geom::paths(&r.geom, f.w, f.h);
            if !known {
                if let Geom::Preset { name, .. } = &r.geom {
                    if !notes.unknown_presets.contains(name) {
                        notes.unknown_presets.push(name.clone());
                    }
                }
            }
            let fill = paint(&deck.theme, &r.fill, w, h);
            let line = stroke(&deck.theme, &r.line);
            if fill.is_some() || line.is_some() {
                out.push(DrawItem::Path { xf, w, h, paths, fill, stroke: line });
            }
            if let Some(t) = text {
                // Text turns with its shape but isn't mirrored.
                let txf = mul(base, &shape_xform(f, s.rot, false, false));
                frame_text(deck, &r, t, s.id, &txf, slide_no, fonts, out, notes);
            }
        }
        ShapeKind::Picture { media, crop, .. } => {
            out.push(DrawItem::Picture { xf, w, h, media: *media, crop: *crop });
            if let Some(line) = stroke(&deck.theme, &r.line) {
                out.push(DrawItem::Path { xf, w, h, paths: geom::paths(&Geom::preset("rect"), f.w, f.h).0, fill: None, stroke: Some(line) });
            }
        }
        ShapeKind::Video { media, poster } => out.push(DrawItem::Video { xf, w, h, media: *media, poster: *poster }),
        ShapeKind::Group { child, shapes: kids } => {
            let sx = if child.w > 0 { w / child.w as f64 } else { 1.0 };
            let sy = if child.h > 0 { h / child.h as f64 } else { 1.0 };
            let inner = mul(&xf, &[sx, 0.0, 0.0, sy, -(child.x as f64) * sx, -(child.y as f64) * sy]);
            for k in kids {
                if !k.hidden {
                    shape(deck, owner, k, &inner, slide_no, fonts, out, notes);
                }
            }
        }
    }
}

/// The label of a numbered paragraph: `n` in OOXML scheme `scheme`.
pub fn number_label(scheme: &str, n: u32) -> String {
    let alpha = |n: u32, upper: bool| {
        let mut s = String::new();
        let mut n = n.max(1);
        while n > 0 {
            n -= 1;
            s.insert(0, (if upper { b'A' } else { b'a' } + (n % 26) as u8) as char);
            n /= 26;
        }
        s
    };
    let roman = |n: u32, upper: bool| {
        let mut s = String::new();
        let mut n = n.clamp(1, 3999);
        for (v, r) in [(1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")] {
            while n >= v {
                s.push_str(r);
                n -= v;
            }
        }
        if upper {
            s.to_uppercase()
        } else {
            s
        }
    };
    let body = if scheme.starts_with("alphaLc") {
        alpha(n, false)
    } else if scheme.starts_with("alphaUc") {
        alpha(n, true)
    } else if scheme.starts_with("romanLc") {
        roman(n, false)
    } else if scheme.starts_with("romanUc") {
        roman(n, true)
    } else {
        n.to_string()
    };
    if scheme.ends_with("ParenBoth") {
        format!("({body})")
    } else if scheme.ends_with("ParenR") {
        format!("{body})")
    } else if scheme.ends_with("Plain") {
        body
    } else {
        format!("{body}.")
    }
}

/// A frame's text as the layout engine sees it: the frame's document with
/// its level styles made the paragraphs' own formatting, theme fonts and
/// colours resolved, sizes scaled for shrink-to-fit.
pub fn styled_doc(theme: &Theme, levels: &ListStyle, bullets: &[Bullet], body: &TextDoc, scale: u32) -> TextDoc {
    let mut d = body.clone();
    let scale_size = |s: u16| ((s as u64 * scale as u64 / 100_000) as u16).max(2);
    let fix = |a: &CharAttrs| {
        let mut a = a.clone();
        if let Some(f) = &a.family {
            if f.starts_with('+') {
                a.family = Some(resolve::font(theme, f));
            }
        }
        if let (Some(s), true) = (a.size, scale != 100_000) {
            a.size = Some(scale_size(s));
        }
        a
    };
    let mut styles = Vec::with_capacity(MAX_LEVELS);
    for (i, l) in levels.iter().enumerate().take(MAX_LEVELS) {
        let mut chars = fix(&l.chars);
        if let Some(c) = &l.color {
            if l.chars.color.is_none() {
                chars.color = Some(color(theme, c) >> 8);
            }
        }
        styles.push(Style { name: format!("Level {}", i + 1).into(), kind: StyleKind::Para, parent: None, next: Some(i as u16), para: ParaProps::default(), chars });
    }
    let mut def = CharFormat { family: theme.minor.clone(), size: scale_size(36), color: theme.colors[0], ..CharFormat::default() };
    def.family = theme.minor.clone();
    d.styles = StyleSheet { styles, default_char: def, lists: Default::default() };
    // Runs: theme fonts named, sizes scaled.
    let mut table = officina_text::model::AttrTable::default();
    let n = d.blocks.len();
    for bi in 0..n {
        let Some(p) = d.para(bi) else { continue };
        let lvl = (p.style as usize).min(MAX_LEVELS - 1);
        let runs: Vec<(u32, u32)> = p.runs.iter().map(|&(at, id)| (at, table.intern(fix(d.attrs.get(id))))).collect();
        let lv = &levels[lvl];
        let mut props = resolve::para_over(&p.props, &lv.para);
        // The level's indents are the paragraph's own (the list hang must not
        // move them).
        props.indent_start.get_or_insert(0);
        props.first_line.get_or_insert(0);
        props.space_after.get_or_insert(0);
        props.space_before.get_or_insert(0);
        if props.line_spacing.is_none() {
            props.line_spacing = Some(officina_text::model::LineSpacing::Multiple(100));
        }
        // The paragraph's own bullet (`bullets[id - 1]`), else its level's;
        // the laid-out paragraph names it as list 1 (the level's) or 1 + n.
        let (b, id) = match &p.props.list {
            Some(Some(r)) if r.id >= 1 => (bullets.get(r.id as usize - 1), r.id + 1),
            Some(_) => (None, 1),
            None => (lv.bullet.as_ref(), 1),
        };
        let bullet = match b {
            Some(Bullet::Char { .. }) => Some(ListKind::Bullet),
            Some(Bullet::Number { .. }) => Some(ListKind::Numbered),
            _ => None,
        };
        // An empty paragraph shows no bullet.
        props.list = Some(bullet.filter(|_| !p.text.is_empty()).map(|kind| ListRef { id, kind, level: lvl as u8 }));
        d.blocks.update(bi, |b| {
            if let Some(p) = b.as_para_mut() {
                p.runs = runs.into_iter().map(|(a, i)| (a, i as _)).collect();
                p.props = props;
                p.style = lvl as u16;
            }
        });
    }
    d.attrs = table;
    d
}

/// A frame's paragraphs laid out at `width` (layout units): each paragraph's
/// layout and its top.
pub struct FrameLayout {
    pub doc: TextDoc,
    pub paras: Vec<(officina_text::layout::ParaLayout, i64)>,
    pub height: i64,
}

pub fn layout_frame(doc: TextDoc, width: i64, fonts: &dyn Fonts) -> FrameLayout {
    layout_frame_with(doc, width, fonts, |_, _, _| {})
}

fn layout_frame_with(
    doc: TextDoc,
    width: i64,
    fonts: &dyn Fonts,
    mut adjust: impl FnMut(&TextDoc, &Para, &mut ParaLayout),
) -> FrameLayout {
    let mut paras = Vec::new();
    let mut y = 0;
    let n = doc.blocks.len();
    for i in 0..n {
        let Some(p) = doc.para(i) else { continue };
        let mut l = layout_para(&doc, p, width.max(officina_text::LU), fonts);
        adjust(&doc, p, &mut l);
        if i > 0 {
            y += l.space_before;
        }
        let top = y;
        y += l.height;
        if i + 1 < n {
            y += l.space_after;
        }
        paras.push((l, top));
    }
    FrameLayout { doc, paras, height: y }
}

/// A frame's text laid out as drawn: the layout, and the map from its layout
/// units onto the slide (insets and vertical anchoring included).
pub struct FrameGeom {
    pub layout: FrameLayout,
    pub xf: Xform,
    pub resolved: Resolved,
    labels: Vec<Option<(String, CharFormat)>>,
}

/// The geometry of `shape`'s text frame (owned as `owner`, inside groups
/// mapped by `base`); `None` if it has no frame.
pub fn frame_geometry(deck: &Deck, owner: Owner, shape: &Shape, base: &Xform, fonts: &dyn Fonts) -> Option<FrameGeom> {
    let body = shape.text()?;
    let r = resolve::resolve(deck, owner, shape);
    let txf = mul(base, &shape_xform(r.frame, shape.rot, false, false));
    Some(frame_geom(deck, r, body, &txf, None, fonts))
}

fn frame_geom(deck: &Deck, r: Resolved, body: &TextBody, txf: &Xform, slide_no: Option<&str>, fonts: &dyn Fonts) -> FrameGeom {
    let scale = match r.autofit {
        Autofit::Shrink { scale, .. } => scale.clamp(1_000, 100_000),
        _ => 100_000,
    };
    let mut doc = styled_doc(&deck.theme, &r.levels, &body.bullets, &body.doc, scale);
    if let Some(n) = slide_no {
        for i in 0..doc.blocks.len() {
            if doc.para(i).is_some_and(|p| p.text.contains("‹#›")) {
                doc.blocks.update(i, |b| {
                    if let Some(p) = b.as_para_mut() {
                        replace_keep_runs(p, "‹#›", n);
                    }
                });
            }
        }
    }
    let [l, t, rt, b] = r.inset;
    let width_emu = (r.frame.w - l - rt).max(0);
    let width = if r.wrap { crate::emu_to_lu(width_emu) } else { i64::MAX / 4 };
    let width = width.max(officina_text::LU);
    let mut labels = Vec::new();
    let mut counters = [0; MAX_LEVELS];
    let fl = layout_frame_with(doc, width, fonts, |doc, para, layout| {
        let label = paragraph_label(&deck.theme, &r.levels, body, para, layout, &mut counters);
        if let Some((text, format)) = &label {
            reserve_label_gap(doc, para, layout, width, fonts, text, format);
        }
        labels.push(label);
    });
    let avail = crate::emu_to_lu((r.frame.h - t - b).max(0));
    let dy = match r.anchor {
        Anchor::Top => 0,
        Anchor::Middle => (avail - fl.height) / 2,
        Anchor::Bottom => avail - fl.height,
    };
    let k = crate::EMU_PER_PT as f64 / officina_text::LU as f64;
    let local = [k, 0.0, 0.0, k, l as f64, t as f64 + dy as f64 * k];
    FrameGeom { layout: fl, xf: mul(txf, &local), resolved: r, labels }
}

#[allow(clippy::too_many_arguments)]
fn frame_text(deck: &Deck, r: &Resolved, body: &TextBody, id: ShapeId, xf: &Xform, slide_no: &str, fonts: &dyn Fonts, out: &mut Vec<DrawItem>, notes: &mut Notes) {
    let g = frame_geom(deck, r.clone(), body, xf, Some(slide_no), fonts);
    let fl = &g.layout;
    let [_, top_inset, _, bottom_inset] = g.resolved.inset;
    if notes.verify_text && fl.doc.blocks.iter().any(|b| b.as_para().is_some_and(|p| !p.text.is_empty()))
        && fl.height > crate::emu_to_lu((r.frame.h - top_inset - bottom_inset).max(0))
    {
        notes.incomplete_text = true;
    }
    let mut items = Vec::new();
    for (i, ((pl, top), label)) in fl.paras.iter().zip(&g.labels).enumerate() {
        let Some(p) = fl.doc.para(i) else { continue };
        let start = items.len();
        paint_para(&mut items, &fl.doc, fonts, pl, &p.text, 0, *top, label.as_ref().map(|(s, f)| (s.as_str(), Some(f.clone()))));
        if notes.verify_text {
            let painted = items[start..].iter().filter_map(|item| match item {
                Item::Glyphs(run) => Some(run.text.as_str()),
                _ => None,
            }).flat_map(str::chars);
            let expected = label.as_ref().into_iter().flat_map(|(s, _)| s.chars()).chain(p.text.chars());
            // Line wrapping can consume an ordinary space at the break.
            if !painted.filter(|&c| c != ' ').eq(expected.filter(|&c| c != ' ')) {
                notes.incomplete_text = true;
            }
        }
    }
    if items.is_empty() {
        return;
    }
    out.push(DrawItem::Text { xf: g.xf, shape: id, items });
}

fn paragraph_label(
    theme: &Theme,
    levels: &ListStyle,
    body: &TextBody,
    p: &Para,
    layout: &ParaLayout,
    counters: &mut [u32; MAX_LEVELS],
) -> Option<(String, CharFormat)> {
    let lvl = (p.style as usize).min(MAX_LEVELS - 1);
    // styled_doc reserves list 1 for the inherited level style.
    let bullet = match &p.props.list {
        Some(Some(list)) if list.id > 1 => body.bullets.get(list.id as usize - 2),
        Some(Some(_)) => levels[lvl].bullet.as_ref(),
        _ => None,
    };
    let bullet = bullet?;
    let mut format = list_label_format(layout);
    let text = match bullet {
        Bullet::Char { ch, font, color: c, size_pct } => {
            counters.fill(0);
            if let Some(family) = font {
                format.family = resolve::font(theme, family);
            }
            if let Some(c) = c {
                format.color = color(theme, c) >> 8;
            }
            if let Some(pct) = size_pct {
                format.size = ((format.size as u64 * *pct as u64 / 100_000) as u16).max(2);
            }
            ch.to_string()
        }
        Bullet::Number { scheme, start } => {
            counters[lvl] += 1;
            counters[lvl + 1..].fill(0);
            number_label(scheme, start.saturating_add(counters[lvl] - 1))
        }
        Bullet::None => return None,
    };
    Some((text, format))
}

fn reserve_label_gap(
    doc: &TextDoc,
    para: &Para,
    layout: &mut ParaLayout,
    width: i64,
    fonts: &dyn Fonts,
    label: &str,
    format: &CharFormat,
) {
    if layout.rtl {
        return;
    }
    let label_width: i64 = fonts.shape(label, format).iter().map(|g| g.adv).sum();
    // Keep the label at the saved position and reserve a quarter em after it.
    let room = (label_width + size_lu(format) / 4).max(0);
    let hang = ((room * 20 + officina_text::LU - 1) / officina_text::LU) as i32;
    let first = layout.format.first_line;
    let label_start = layout.format.indent_start.saturating_add(first);
    let body_start = label_start.saturating_add(hang);
    if first <= -hang {
        return;
    }
    let mut adjusted = para.clone();
    adjusted.props.indent_start = Some(body_start);
    adjusted.props.first_line = Some(-hang);
    *layout = layout_para(doc, &adjusted, width, fonts);
}

/// Replaces `from` with `to` in a paragraph, keeping the run each piece is in.
fn replace_keep_runs(p: &mut Para, from: &str, to: &str) {
    while let Some(at) = p.text.find(from) {
        let (a, b) = (at as u32, (at + from.len()) as u32);
        let delta = to.len() as i64 - from.len() as i64;
        p.text.replace_range(at..at + from.len(), to);
        for r in p.runs.iter_mut() {
            if r.0 >= b {
                r.0 = (r.0 as i64 + delta) as u32;
            } else if r.0 > a {
                r.0 = a;
            }
        }
        p.runs.dedup_by(|x, y| x.0 == y.0);
    }
}

/// The map from a group's child coordinates onto its parent's.
pub fn group_xform(deck: &Deck, owner: Owner, g: &Shape) -> Xform {
    let ShapeKind::Group { child, .. } = &g.kind else { return IDENTITY };
    let f = resolve::resolve(deck, owner, g).frame;
    let sx = if child.w > 0 { f.w as f64 / child.w as f64 } else { 1.0 };
    let sy = if child.h > 0 { f.h as f64 / child.h as f64 } else { 1.0 };
    mul(&shape_xform(f, g.rot, g.flip_h, g.flip_v), &[sx, 0.0, 0.0, sy, -(child.x as f64) * sx, -(child.y as f64) * sy])
}

/// The map from the coordinates shape `id` sits in (the slide's, or its
/// group's) onto the slide.
pub fn parent_xform(deck: &Deck, owner: Owner, shapes: &[Shape], id: ShapeId) -> Option<Xform> {
    fn go(deck: &Deck, owner: Owner, shapes: &[Shape], id: ShapeId, base: Xform) -> Option<Xform> {
        for s in shapes {
            if s.id == id {
                return Some(base);
            }
            if let ShapeKind::Group { shapes: kids, .. } = &s.kind {
                if let Some(x) = go(deck, owner, kids, id, mul(&base, &group_xform(deck, owner, s))) {
                    return Some(x);
                }
            }
        }
        None
    }
    go(deck, owner, shapes, id, IDENTITY)
}

/// The map from shape `id`'s own box (`0..w`, `0..h`, rotated and flipped)
/// onto the slide, and its box size.
pub fn shape_box(deck: &Deck, owner: Owner, shapes: &[Shape], id: ShapeId) -> Option<(Xform, f64, f64)> {
    let s = find(shapes, id)?;
    let base = parent_xform(deck, owner, shapes, id)?;
    let f = resolve::resolve(deck, owner, s).frame;
    Some((mul(&base, &shape_xform(f, s.rot, s.flip_h, s.flip_v)), f.w as f64, f.h as f64))
}
