//! What a shape shows once its placeholder chain (slide → layout → master)
//! fills in what it leaves unset, with theme colours and fonts resolved
//! (docs/spec/officina.md §16.33).

use std::sync::Arc;

use officina_text::model::{Align, CharAttrs, LineSpacing, ParaProps};

use crate::model::*;

/// sRGB and alpha (0 to 255) as 0xRRGGBBAA.
pub type Rgba = u32;

fn hsl(rgb: u32) -> (f64, f64, f64) {
    let r = ((rgb >> 16) & 255) as f64 / 255.0;
    let g = ((rgb >> 8) & 255) as f64 / 255.0;
    let b = (rgb & 255) as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn rgb_of(h: f64, s: f64, l: f64) -> u32 {
    let f = |p: f64, q: f64, mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let (r, g, b) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        (f(p, q, h + 1.0 / 3.0), f(p, q, h), f(p, q, h - 1.0 / 3.0))
    };
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}

/// A colour's sRGB and alpha under `theme`.
pub fn color(theme: &Theme, c: &Color) -> Rgba {
    let mut rgb = match c.base {
        ColorBase::Rgb(v) => v & 0xFF_FFFF,
        ColorBase::Theme(s) => theme.colors[s as usize] & 0xFF_FFFF,
    };
    let mut alpha = 255u32;
    let pct = |v: i32| v as f64 / 100_000.0;
    for m in &c.mods {
        match *m {
            ColorMod::LumMod(_) | ColorMod::LumOff(_) | ColorMod::SatMod(_) => {
                let (h, mut s, mut l) = hsl(rgb);
                match *m {
                    ColorMod::LumMod(v) => l *= pct(v),
                    ColorMod::LumOff(v) => l += pct(v),
                    ColorMod::SatMod(v) => s *= pct(v),
                    _ => {}
                }
                rgb = rgb_of(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
            }
            ColorMod::Tint(v) | ColorMod::Shade(v) => {
                let k = pct(v).clamp(0.0, 1.0);
                let tint = matches!(m, ColorMod::Tint(_));
                let ch = |x: u32| {
                    let x = x as f64;
                    let y = if tint { 255.0 - (255.0 - x) * k } else { x * k };
                    y.round().clamp(0.0, 255.0) as u32
                };
                rgb = (ch((rgb >> 16) & 255) << 16) | (ch((rgb >> 8) & 255) << 8) | ch(rgb & 255);
            }
            ColorMod::Alpha(v) => alpha = (alpha as f64 * pct(v).clamp(0.0, 1.0)).round() as u32,
        }
    }
    (rgb << 8) | alpha
}

/// Who a shape belongs to, for finding its placeholder chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Slide(LayoutId),
    Layout(MasterId),
    Master,
}

/// The layout placeholder a slide placeholder takes from: the same index
/// (when not 0), else the first of the same kind.
pub fn layout_ph(layout: &Layout, ph: Placeholder) -> Option<&Shape> {
    let same_idx = |s: &&Shape| s.ph.is_some_and(|p| ph.idx != 0 && p.idx == ph.idx);
    let same_kind = |s: &&Shape| s.ph.is_some_and(|p| p.kind.family() == ph.kind.family());
    layout.shapes.iter().find(same_idx).or_else(|| layout.shapes.iter().find(same_kind))
}

/// The master placeholder a layout placeholder takes from: the same kind.
pub fn master_ph(master: &Master, ph: Placeholder) -> Option<&Shape> {
    let fam = ph.kind.family();
    master.shapes.iter().find(|s| s.ph.is_some_and(|p| p.kind.family() == fam))
}

/// The chain of shapes a shape inherits from, nearest first (not the shape itself).
pub fn chain<'a>(deck: &'a Deck, owner: Owner, shape: &Shape) -> Vec<&'a Shape> {
    let Some(ph) = shape.ph else { return Vec::new() };
    let mut v = Vec::new();
    let master_of = |m: MasterId| deck.master(m);
    match owner {
        Owner::Slide(lid) => {
            let Some(l) = deck.layout(lid) else { return v };
            if let Some(s) = layout_ph(l, ph) {
                v.push(s);
                if let (Some(m), Some(p)) = (master_of(l.master), s.ph) {
                    v.extend(master_ph(m, p));
                }
            } else if let Some(m) = master_of(l.master) {
                v.extend(master_ph(m, ph));
            }
        }
        Owner::Layout(mid) => {
            if let Some(m) = master_of(mid) {
                v.extend(master_ph(m, ph));
            }
        }
        Owner::Master => {}
    }
    v
}

/// A paragraph property set over another: `top`'s set fields win.
pub fn para_over(top: &ParaProps, under: &ParaProps) -> ParaProps {
    let mut p = under.clone();
    macro_rules! take {
        ($($f:ident),*) => { $( if top.$f.is_some() { p.$f = top.$f.clone(); } )* };
    }
    take!(align, indent_start, indent_end, first_line, space_before, space_after, line_spacing, keep_with_next, keep_together, page_break_before, widows, outline_level, list, cell, rev, tabs, toc, section);
    p
}

/// A level style set over another.
pub fn level_over(top: &LevelStyle, under: &LevelStyle) -> LevelStyle {
    LevelStyle {
        chars: top.chars.over(&under.chars),
        // A direct `chars.color` on top beats a theme colour under it.
        color: if top.color.is_some() || top.chars.color.is_some() { top.color.clone() } else { under.color.clone() },
        para: para_over(&top.para, &under.para),
        bullet: top.bullet.clone().or_else(|| under.bullet.clone()),
    }
}

/// `top` over `under`, level by level.
pub fn list_over(top: &ListStyle, under: &ListStyle) -> ListStyle {
    (0..MAX_LEVELS.min(top.len().max(under.len())))
        .map(|i| match (top.get(i), under.get(i)) {
            (Some(t), Some(u)) => level_over(t, u),
            (Some(t), None) => t.clone(),
            (None, Some(u)) => u.clone(),
            (None, None) => LevelStyle::default(),
        })
        .collect()
}

/// What a shape shows, everything decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub frame: Rect,
    pub geom: Geom,
    pub fill: Fill,
    pub line: Stroke,
    pub inset: [Emu; 4],
    pub anchor: Anchor,
    pub autofit: Autofit,
    pub wrap: bool,
    /// Level styles, all nine, from the master's role style down to the frame's own.
    pub levels: ListStyle,
}

/// Default insets: 0.1" left and right, 0.05" top and bottom.
pub const INSET: [Emu; 4] = [91_440, 45_720, 91_440, 45_720];

/// Resolves `shape` (owned as `owner`) through its placeholder chain.
pub fn resolve(deck: &Deck, owner: Owner, shape: &Shape) -> Resolved {
    let chain = chain(deck, owner, shape);
    let all: Vec<&Shape> = std::iter::once(shape).chain(chain.iter().copied()).collect();
    let frame = all.iter().find_map(|s| s.frame).unwrap_or_default();
    let geom_of = |s: &Shape| match &s.kind {
        ShapeKind::Geom { geom, .. } => geom.clone(),
        _ => None,
    };
    let fill_of = |s: &Shape| match &s.kind {
        ShapeKind::Geom { fill, .. } => fill.clone(),
        _ => None,
    };
    let line_of = |s: &Shape| match &s.kind {
        ShapeKind::Geom { line, .. } | ShapeKind::Picture { line, .. } => line.clone(),
        _ => None,
    };
    let geom = all.iter().find_map(|s| geom_of(s)).unwrap_or_else(|| Geom::preset("rect"));
    // A placeholder with no fill or line of its own anywhere draws none.
    let fill = all.iter().find_map(|s| fill_of(s)).unwrap_or(Fill::None);
    let line = all.iter().find_map(|s| line_of(s)).unwrap_or_else(Stroke::none);
    let texts: Vec<&TextBody> = all.iter().filter_map(|s| s.text()).collect();
    let inset = texts.iter().find_map(|t| t.inset).unwrap_or(INSET);
    let anchor = texts.iter().find_map(|t| t.anchor).unwrap_or_default();
    let autofit = texts.iter().find_map(|t| t.autofit).unwrap_or_default();
    let wrap = texts.iter().find_map(|t| t.wrap).unwrap_or(true);
    // The master's style for the role, then each frame's own levels nearer.
    let role = shape.ph.map_or(TextRole::Other, |p| p.kind.text_role());
    let master = match owner {
        Owner::Slide(l) => deck.layout(l).and_then(|l| deck.master(l.master)),
        Owner::Layout(m) => deck.master(m),
        Owner::Master => deck.masters.first(),
    };
    let mut levels: ListStyle = match (master, role) {
        (Some(m), TextRole::Title) => m.title_style.clone(),
        (Some(m), TextRole::Body) => m.body_style.clone(),
        (Some(m), TextRole::Other) => m.other_style.clone(),
        (None, _) => Vec::new(),
    };
    for t in texts.iter().rev() {
        levels = list_over(&t.levels, &levels);
    }
    levels.resize(MAX_LEVELS, LevelStyle::default());
    Resolved { frame, geom, fill, line, inset, anchor, autofit, wrap, levels }
}

/// The font a family names: `+mj-lt` and `+mn-lt` are the theme's.
pub fn font(theme: &Theme, family: &str) -> Arc<str> {
    match family {
        "+mj-lt" | "+mj-ea" | "+mj-cs" => theme.major.clone(),
        "+mn-lt" | "+mn-ea" | "+mn-cs" => theme.minor.clone(),
        f => f.into(),
    }
}

// ---------------------------------------------------------------------------
// The default master and layouts.

fn level(size_pt: u16, mar_l: Emu, indent: Emu, bullet: Option<char>) -> LevelStyle {
    let tw = |e: Emu| (e / 635) as i32;
    LevelStyle {
        chars: CharAttrs { size: Some(size_pt * 2), family: Some("+mn-lt".into()), ..Default::default() },
        color: Some(Color::theme(Slot::Dk1)),
        para: ParaProps { indent_start: Some(tw(mar_l)), first_line: Some(tw(indent)), space_before: Some(200), space_after: Some(0), line_spacing: Some(LineSpacing::Multiple(90)), align: Some(Align::Start), ..Default::default() },
        bullet: Some(bullet.map_or(Bullet::None, |ch| Bullet::Char { ch, font: Some("Arial".into()), color: None, size_pct: None })),
    }
}

fn ph(deck: &mut Deck, kind: PhKind, idx: u32, frame: Rect, levels: ListStyle, anchor: Option<Anchor>) -> Shape {
    let id = deck.new_id();
    let mut s = Shape::placeholder(id, kind, idx, "");
    s.frame = Some(frame);
    s.name = format!("{} Placeholder {id}", kind.name()).into();
    if let Some(t) = s.text_mut() {
        t.levels = levels;
        t.anchor = anchor;
    }
    s
}

/// Puts PowerPoint's default master (Office theme) and its usual layouts
/// in `deck`, sized for 16:9.
pub fn default_master(deck: &mut Deck) {
    let (w, h) = deck.size;
    let sx = |v: Emu| v * w / 12_192_000;
    let sy = |v: Emu| v * h / 6_858_000;
    let r = |x, y, ww, hh| Rect::new(sx(x), sy(y), sx(ww), sy(hh));
    let title_lvl = LevelStyle {
        chars: CharAttrs { size: Some(88), family: Some("+mj-lt".into()), ..Default::default() },
        color: Some(Color::theme(Slot::Dk1)),
        para: ParaProps { line_spacing: Some(LineSpacing::Multiple(90)), space_before: Some(0), space_after: Some(0), align: Some(Align::Start), ..Default::default() },
        bullet: Some(Bullet::None),
    };
    let mut body = vec![level(28, 228_600, -228_600, Some('•')), level(24, 685_800, -228_600, Some('•')), level(20, 1_143_000, -228_600, Some('•'))];
    for i in 3..9 {
        body.push(level(18, 1_600_200 + (i - 3) * 457_200, -228_600, Some('•')));
    }
    let other: ListStyle = (0..9).map(|i| level(18, i * 457_200, 0, None)).collect();
    let mid = deck.new_id();
    let small = |align: Align| vec![LevelStyle { chars: CharAttrs { size: Some(24), ..Default::default() }, color: Some(Color::theme(Slot::Dk1).with(ColorMod::Tint(75_000))), para: ParaProps { align: Some(align), ..Default::default() }, bullet: None }];
    let shapes = vec![
        ph(deck, PhKind::Title, 0, r(838_200, 365_125, 10_515_600, 1_325_563), Vec::new(), Some(Anchor::Middle)),
        ph(deck, PhKind::Body, 1, r(838_200, 1_825_625, 10_515_600, 4_351_338), Vec::new(), None),
        ph(deck, PhKind::Date, 2, r(838_200, 6_356_350, 2_743_200, 365_125), small(Align::Start), Some(Anchor::Middle)),
        ph(deck, PhKind::Footer, 3, r(4_038_600, 6_356_350, 4_114_800, 365_125), small(Align::Center), Some(Anchor::Middle)),
        ph(deck, PhKind::SlideNum, 4, r(8_610_600, 6_356_350, 2_743_200, 365_125), small(Align::End), Some(Anchor::Middle)),
    ];
    deck.masters.push(Master { id: mid, name: "Office Theme".into(), background: Fill::Solid(Color::theme(Slot::Lt1)), shapes, title_style: vec![title_lvl], body_style: body, other_style: other });

    let centered = |size: u16| vec![LevelStyle { chars: CharAttrs { size: Some(size * 2), ..Default::default() }, color: None, para: ParaProps { align: Some(Align::Center), indent_start: Some(0), first_line: Some(0), ..Default::default() }, bullet: Some(Bullet::None) }];
    let footers = |deck: &mut Deck| vec![ph(deck, PhKind::Date, 10, r(838_200, 6_356_350, 2_743_200, 365_125), Vec::new(), None), ph(deck, PhKind::Footer, 11, r(4_038_600, 6_356_350, 4_114_800, 365_125), Vec::new(), None), ph(deck, PhKind::SlideNum, 12, r(8_610_600, 6_356_350, 2_743_200, 365_125), Vec::new(), None)];
    let add = |deck: &mut Deck, name: &str, kind: &str, mut shapes: Vec<Shape>| {
        shapes.extend(footers(deck));
        let id = deck.new_id();
        deck.layouts.push(Layout { id, master: mid, name: name.into(), kind: kind.into(), background: None, shapes, hide_master_shapes: false });
    };
    let s = vec![ph(deck, PhKind::CtrTitle, 0, r(1_524_000, 1_122_363, 9_144_000, 2_387_600), centered(60), Some(Anchor::Bottom)), ph(deck, PhKind::SubTitle, 1, r(1_524_000, 3_602_038, 9_144_000, 1_655_762), centered(24), None)];
    add(deck, "Title Slide", "title", s);
    let s = vec![ph(deck, PhKind::Title, 0, r(838_200, 365_125, 10_515_600, 1_325_563), Vec::new(), None), ph(deck, PhKind::Body, 1, r(838_200, 1_825_625, 10_515_600, 4_351_338), Vec::new(), None)];
    add(deck, "Title and Content", "obj", s);
    let s = vec![ph(deck, PhKind::Title, 0, r(831_850, 1_709_738, 10_515_600, 2_852_737), vec![LevelStyle { chars: CharAttrs { size: Some(120), ..Default::default() }, ..Default::default() }], Some(Anchor::Bottom)), ph(deck, PhKind::Body, 1, r(831_850, 4_589_463, 10_515_600, 1_500_187), vec![LevelStyle { chars: CharAttrs { size: Some(48), ..Default::default() }, bullet: Some(Bullet::None), para: ParaProps { indent_start: Some(0), first_line: Some(0), ..Default::default() }, ..Default::default() }], None)];
    add(deck, "Section Header", "secHead", s);
    let s = vec![ph(deck, PhKind::Title, 0, r(838_200, 365_125, 10_515_600, 1_325_563), Vec::new(), None), ph(deck, PhKind::Body, 1, r(838_200, 1_825_625, 5_181_600, 4_351_338), Vec::new(), None), ph(deck, PhKind::Body, 2, r(6_172_200, 1_825_625, 5_181_600, 4_351_338), Vec::new(), None)];
    add(deck, "Two Content", "twoObj", s);
    let s = vec![ph(deck, PhKind::Title, 0, r(838_200, 365_125, 10_515_600, 1_325_563), Vec::new(), None)];
    add(deck, "Title Only", "titleOnly", s);
    add(deck, "Blank", "blank", Vec::new());
}
