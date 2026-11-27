//! What a page shows, as a list of simple items in layout units from the
//! page's top-left corner: glyph runs, rectangles, pictures and link areas.
//! The screen view and PDF export both draw from it, so a page looks the same
//! in both (docs/spec/officina.md §7.4, O3f).

use std::ops::Range;

use crate::layout::{object_height, size_lu, vert_shift, Fonts, LayoutCache, ParaLayout, OBJECT_FACE};
use crate::lists::Numbering;
use crate::model::{Border, BorderStyle, LineRestart, ArrowEnd, Dash, PathCmd, Wrap, Align, CharFormat, Leader, TextDoc, HEADER_FROM_EDGE, OBJECT_CHAR};
use crate::paginate::{Page, NOTE_HANG, NOTE_RULE_SPACE};
use crate::twips;

/// One glyph of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintGlyph {
    pub id: u16,
    /// From the run's origin, offsets included.
    pub x: i64,
    pub dy: i64,
    /// The text this glyph shows, in [`GlyphRun::text`] (empty for the second
    /// and later glyphs of a cluster).
    pub text: Range<u32>,
}

/// Glyphs of one face, size and colour on one baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphRun {
    pub face: u16,
    /// Font size, layout units.
    pub size: i64,
    pub color: u32,
    /// The origin: the first glyph's pen position on the baseline.
    pub x: i64,
    pub y: i64,
    pub glyphs: Vec<PaintGlyph>,
    pub text: String,
    /// Turned about the origin (a turned shape's words): a point (u, v)
    /// from the origin of the upright run lands at origin + (a u + c v,
    /// b u + d v), y down. Glyph offsets stay along the upright baseline.
    pub turn: Option<[f64; 4]>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A filled rectangle (highlights, underlines, cell rules).
    Rect { x: i64, y: i64, w: i64, h: i64, color: u32 },
    Glyphs(GlyphRun),
    /// A picture from the document's images, its top-left corner and size.
    Image { image: u32, x: i64, y: i64, w: i64, h: i64 },
    /// An area that is a link.
    Link { x: i64, y: i64, w: i64, h: i64, uri: String },
    /// Text under comments (the last one listed is the innermost): the screen
    /// marks it, a PDF leaves it out. Comes before the text it covers.
    Mark { x: i64, y: i64, w: i64, h: i64, comments: std::sync::Arc<[crate::model::CommentId]> },
    /// Tracked change over text (an insertion or formatting change) or at a
    /// point (a tombstone, `w` 0): bytes `start..end` of paragraph `block`. The
    /// screen draws a bar in the margin, and in review the edit it belongs
    /// to; a PDF leaves it out.
    Change { x: i64, y: i64, w: i64, h: i64, block: usize, start: u32, end: u32, rev: crate::model::RevId },
    /// A drawn shape's outline in page coordinates: filled with `fill`
    /// (RGB), outlined with `line` (RGB, width in layout units, at least a
    /// hairline, dashes).
    Path { cmds: Vec<PathCmd>, fill: Option<u32>, line: Option<(u32, i64, Dash)> },
    /// A drawn shape's frame: [`paint_page`] puts its outline and text in
    /// its place; [`paint_para`] leaves it for its caller.
    Shape { shape: u32, x: i64, y: i64, w: i64, h: i64 },
}

/// The items for shape `id` in the frame `x`, `y`, `w`, `h`: its outlines,
/// then its text (upright: a turned shape's text keeps its place), then
/// its group's shapes.
#[allow(clippy::too_many_arguments)]
pub fn shape_items(out: &mut Vec<Item>, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, id: u32, x: i64, y: i64, w: i64, h: i64) {
    let Some(sd) = doc.shapes.get(id) else { return };
    let m = place(&[1.0, 0.0, 0.0, 1.0, 0.0, 0.0], sd, x as f64, y as f64, w as f64, h as f64);
    draw_shape(out, doc, cache, fonts, sd, &m, w, h, 0);
}

/// An affine map `[a, b, c, d, e, f]`: (x, y) to (a x + c y + e, b x + d y + f).
type Xf = [f64; 6];

fn mul(p: &Xf, q: &Xf) -> Xf {
    [p[0] * q[0] + p[2] * q[1], p[1] * q[0] + p[3] * q[1], p[0] * q[2] + p[2] * q[3], p[1] * q[2] + p[3] * q[3], p[0] * q[4] + p[2] * q[5] + p[4], p[1] * q[4] + p[3] * q[5] + p[5]]
}

fn apply(m: &Xf, x: f64, y: f64) -> (i64, i64) {
    ((m[0] * x + m[2] * y + m[4]).round() as i64, (m[1] * x + m[3] * y + m[5]).round() as i64)
}

/// `parent` then the map from shape `sd`'s own box (`0..w`, `0..h`) onto
/// its frame at `x`, `y` in the parent's: turned and flipped about its middle.
fn place(parent: &Xf, sd: &crate::model::ShapeData, x: f64, y: f64, w: f64, h: f64) -> Xf {
    let t = (sd.rot as f64 / 60_000.0).to_radians();
    let (sin, cos) = t.sin_cos();
    let (fx, fy) = (if sd.flip_h { -1.0 } else { 1.0 }, if sd.flip_v { -1.0 } else { 1.0 });
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let rs = [cos * fx, sin * fx, -sin * fy, cos * fy, cx, cy];
    mul(parent, &mul(&rs, &[1.0, 0.0, 0.0, 1.0, -w / 2.0, -h / 2.0]))
}

#[allow(clippy::too_many_arguments)]
fn draw_shape(out: &mut Vec<Item>, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, sd: &crate::model::ShapeData, m: &Xf, w: i64, h: i64, depth: usize) {
    // How much the map stretches lengths (for outline widths).
    let k = (m[0] * m[3] - m[1] * m[2]).abs().sqrt();
    if sd.fill.is_some() || sd.line.is_some() {
        let (paths, _) = officina_shapes::geom::paths(&sd.geom, w.max(1), h.max(1));
        for p in &paths {
            let sx = if p.w > 0 { w as f64 / p.w as f64 } else { 1.0 };
            let sy = if p.h > 0 { h as f64 / p.h as f64 } else { 1.0 };
            let pt = |px: i64, py: i64| apply(m, px as f64 * sx, py as f64 * sy);
            let cmds: Vec<PathCmd> = p
                .cmds
                .iter()
                .map(|c| match *c {
                    PathCmd::Move(a, b) => {
                        let (a, b) = pt(a, b);
                        PathCmd::Move(a, b)
                    }
                    PathCmd::Line(a, b) => {
                        let (a, b) = pt(a, b);
                        PathCmd::Line(a, b)
                    }
                    PathCmd::Cubic(a, b, c, d, e, f) => {
                        let ((a, b), (c, d), (e, f)) = (pt(a, b), pt(c, d), pt(e, f));
                        PathCmd::Cubic(a, b, c, d, e, f)
                    }
                    PathCmd::Close => PathCmd::Close,
                })
                .collect();
            let fill = sd.fill.filter(|_| p.filled);
            let line = sd.line.filter(|_| p.stroked).map(|l| (l.color, ((twips(l.width as i64) as f64 * k) as i64).max(crate::LU / 2), l.dash));
            let ends = sd.line.filter(|l| p.stroked && (l.head != ArrowEnd::None || l.tail != ArrowEnd::None) && !cmds.contains(&PathCmd::Close));
            let marks: Vec<Item> = match (ends, line) {
                (Some(l), Some((color, lw, _))) => arrow_ends(&cmds, l.head, l.tail, lw).into_iter().map(|(c, filled)| Item::Path { cmds: c, fill: filled.then_some(color), line: (!filled).then_some((color, lw, Dash::Solid)) }).collect(),
                _ => Vec::new(),
            };
            if fill.is_some() || line.is_some() {
                out.push(Item::Path { cmds, fill, line });
            }
            out.extend(marks);
        }
    }
    if !sd.paras.is_empty() {
        // The text box: the shape's text rectangle (an ellipse's, the
        // rectangle inside it; an arrow's, its body) less its insets,
        // upright at the frame's place.
        let [il, it, ir, ib] = sd.inset.map(|v| (twips(v as i64) as f64 * k) as i64);
        let (fw, fh) = ((w as f64 * k) as i64, (h as f64 * k) as i64);
        let [rl, rt, rr, rb] = officina_shapes::geom::text_rect(&sd.geom, fw.max(1), fh.max(1));
        let (il, it, ir, ib) = (il + rl, it + rt, ir + (fw - rr).max(0), ib + (fh - rb).max(0));
        let (cx, cy) = apply(m, w as f64 / 2.0, h as f64 / 2.0);
        let (x, y) = (cx - fw / 2, cy - fh / 2);
        let width = (fw - il - ir).max(crate::LU);
        let lays: Vec<_> = sd.paras.iter().map(|p| cache.get(doc, p, width, fonts)).collect();
        let total: i64 = lays.iter().map(|l| l.height).sum();
        let room = fh - it - ib;
        let mut ty = y + it + match sd.anchor {
            crate::model::TextAnchor::Top => 0,
            crate::model::TextAnchor::Middle => (room - total) / 2,
            crate::model::TextAnchor::Bottom => room - total,
        }
        .max(0);
        let mut words = Vec::new();
        for (p, l) in sd.paras.iter().zip(&lays) {
            for li in 0..l.lines.len() {
                line(&mut words, doc, fonts, l, li, &p.text, x + il, ty, None);
                ty += l.lines[li].height;
            }
        }
        // Laid out upright about the middle, then turned with the shape
        // (a flip doesn't mirror words; flipped upside down, they turn
        // half round, as Office draws them).
        let det = m[0] * m[3] - m[1] * m[2];
        let t = if det >= 0.0 { m[1].atan2(m[0]) } else { (-m[2]).atan2(m[3]) };
        if t.abs() > 1e-6 {
            turn_items(&mut words, t, cx as f64, cy as f64);
        }
        out.extend(words);
    }
    if depth < crate::model::MAX_GROUP_DEPTH {
        for kid in &sd.kids {
            let f = |v: i32, s: i64| v as f64 * s as f64 / 100_000.0;
            let (kw, kh) = (f(kid.w, w).max(1.0), f(kid.h, h).max(1.0));
            let km = place(m, &kid.shape, f(kid.x, w), f(kid.y, h), kw, kh);
            draw_shape(out, doc, cache, fonts, &kid.shape, &km, kw as i64, kh as i64, depth + 1);
        }
    }
}

/// Turns words laid out upright by `t` radians (clockwise on the page)
/// about (`cx`, `cy`): runs keep their glyphs and take a turn, rectangles
/// (underlines, highlights) become turned outlines.
fn turn_items(items: &mut [Item], t: f64, cx: f64, cy: f64) {
    let (sin, cos) = t.sin_cos();
    let at = |x: i64, y: i64| {
        let (u, v) = (x as f64 - cx, y as f64 - cy);
        ((cx + cos * u - sin * v).round() as i64, (cy + sin * u + cos * v).round() as i64)
    };
    for it in items.iter_mut() {
        match it {
            Item::Glyphs(r) => {
                (r.x, r.y) = at(r.x, r.y);
                r.turn = Some([cos, sin, -sin, cos]);
            }
            Item::Rect { x, y, w, h, color } => {
                let c = [at(*x, *y), at(*x + *w, *y), at(*x + *w, *y + *h), at(*x, *y + *h)];
                let mut cmds = vec![PathCmd::Move(c[0].0, c[0].1)];
                cmds.extend(c[1..].iter().map(|&(a, b)| PathCmd::Line(a, b)));
                cmds.push(PathCmd::Close);
                *it = Item::Path { cmds, fill: Some(*color), line: None };
            }
            // Links and marks keep their upright boxes round the middle.
            _ => {}
        }
    }
}

/// The marks ending an open path: at its start (`head`) and end (`tail`),
/// sized from the line width `lw` as Office's medium ends are; each
/// filled, or (an open arrow) stroked.
fn arrow_ends(cmds: &[PathCmd], head: ArrowEnd, tail: ArrowEnd, lw: i64) -> Vec<(Vec<PathCmd>, bool)> {
    let pts: Vec<(i64, i64)> = cmds
        .iter()
        .flat_map(|c| match *c {
            PathCmd::Move(x, y) | PathCmd::Line(x, y) => vec![(x, y)],
            PathCmd::Cubic(a, b, c, d, e, f) => vec![(a, b), (c, d), (e, f)],
            PathCmd::Close => vec![],
        })
        .collect();
    let mut out = Vec::new();
    // The end point and the direction the line arrives along.
    let pick = |rev: bool| -> Option<((f64, f64), (f64, f64))> {
        let it: Box<dyn Iterator<Item = &(i64, i64)>> = if rev { Box::new(pts.iter().rev()) } else { Box::new(pts.iter()) };
        let mut it = it.peekable();
        let &(ex, ey) = *it.peek()?;
        let &(px, py) = it.find(|&&(x, y)| (x, y) != (ex, ey))?;
        let (dx, dy) = ((ex - px) as f64, (ey - py) as f64);
        let n = (dx * dx + dy * dy).sqrt();
        Some(((ex as f64, ey as f64), (dx / n, dy / n)))
    };
    let size = (lw as f64 * 3.0).max(crate::LU as f64 * 4.0);
    for (kind, rev) in [(head, false), (tail, true)] {
        if kind == ArrowEnd::None {
            continue;
        }
        let Some(((ex, ey), (ux, uy))) = pick(rev) else { continue };
        // Along the line back from the end, and across it.
        let at = |along: f64, across: f64| ((ex - ux * along - uy * across).round() as i64, (ey - uy * along + ux * across).round() as i64);
        let poly = |p: &[(i64, i64)], close: bool| {
            let mut v: Vec<PathCmd> = p.iter().enumerate().map(|(i, &(x, y))| if i == 0 { PathCmd::Move(x, y) } else { PathCmd::Line(x, y) }).collect();
            if close {
                v.push(PathCmd::Close);
            }
            v
        };
        let (l, hw) = (size, size / 2.0);
        match kind {
            ArrowEnd::Triangle => out.push((poly(&[at(0.0, 0.0), at(l, hw), at(l, -hw)], true), true)),
            ArrowEnd::Stealth => out.push((poly(&[at(0.0, 0.0), at(l, hw), at(l * 0.6, 0.0), at(l, -hw)], true), true)),
            ArrowEnd::Open => out.push((poly(&[at(l, hw), at(0.0, 0.0), at(l, -hw)], false), false)),
            ArrowEnd::Diamond => out.push((poly(&[at(-hw, 0.0), at(0.0, hw), at(hw, 0.0), at(0.0, -hw)], true), true)),
            ArrowEnd::Oval => {
                let r = hw;
                let k = r * 0.552_284_75;
                let p = |a: f64, b: f64| at(a, b);
                let (a, b, c, d) = (p(0.0, r), p(r, 0.0), p(0.0, -r), p(-r, 0.0));
                let q = |(x1, y1): (i64, i64), (x2, y2): (i64, i64), (x, y): (i64, i64)| PathCmd::Cubic(x1, y1, x2, y2, x, y);
                out.push((
                    vec![PathCmd::Move(a.0, a.1), q(p(k, r), p(r, k), b), q(p(r, -k), p(k, -r), c), q(p(-k, -r), p(-r, -k), d), q(p(-r, k), p(-k, r), a), PathCmd::Close],
                    true,
                ));
            }
            ArrowEnd::None => {}
        }
    }
    out
}

/// Puts each shape frame's outline and text in its place.
fn expand_shapes(out: Vec<Item>, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts) -> Vec<Item> {
    if !out.iter().any(|i| matches!(i, Item::Shape { .. })) {
        return out;
    }
    let mut v = Vec::with_capacity(out.len() + 8);
    for it in out {
        match it {
            Item::Shape { shape, x, y, w, h } => {
                let mut inner = Vec::new();
                shape_items(&mut inner, doc, cache, fonts, shape, x, y, w, h);
                v.extend(inner.into_iter().filter(|i| !matches!(i, Item::Shape { .. })));
            }
            other => v.push(other),
        }
    }
    v
}

/// A table rule's thickness: half a point.
pub const RULE: i64 = crate::LU / 2;
/// The colour of table rules.
pub const RULE_COLOR: u32 = 0x8c9199;
/// Header and footer text.
pub const HEADER_FORMAT_SIZE: u16 = 18;
pub const HEADER_COLOR: u32 = 0x555555;

/// The items of page `i` of `pages`; `pages_text` is what `{pages}` reads.
#[allow(clippy::too_many_arguments)]
pub fn paint_page(doc: &TextDoc, pages: &[Page], i: usize, col_w: i64, cache: &mut LayoutCache, fonts: &dyn Fonts, numbering: &Numbering, pages_text: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let Some(page) = pages.get(i) else { return out };
    // The page's own size and margins (its section's).
    let g = &page.setup;
    let col_w = if *g == doc.page { col_w } else { crate::paginate::content_width(g) };
    let (mx, my) = (twips(g.margin_start as i64), twips(g.margin_top as i64));
    let (pw, ph) = (twips(g.width as i64), twips(g.height as i64));

    // Header and footer, in the margins.
    let (head, foot) = doc.margins_of(i);
    for (hf, top) in [(head, true), (foot, false)] {
        let Some(hf) = hf else { continue };
        let from = twips(HEADER_FROM_EDGE as i64);
        let width = pw - mx - twips(g.margin_end as i64);
        if !hf.paras.is_empty() {
            // Its own paragraphs: the header down from its line, the
            // footer up to its.
            let paras: Vec<_> = hf.paras.iter().map(|p| crate::model::resolve_para(p, i + 1, pages_text)).collect();
            let lays: Vec<_> = paras.iter().map(|p| cache.get(doc, p, width.max(crate::LU), fonts)).collect();
            let total: i64 = lays.iter().map(|l| l.height).sum();
            let mut y = if top { from } else { ph - from - total };
            for (p, l) in paras.iter().zip(&lays) {
                for li in 0..l.lines.len() {
                    line(&mut out, doc, fonts, l, li, &p.text, mx, y, None);
                    y += l.lines[li].height;
                }
            }
            continue;
        }
        let text = hf.resolve(i + 1, pages_text);
        let fmt = CharFormat { size: HEADER_FORMAT_SIZE, color: HEADER_COLOR, ..Default::default() };
        let size = size_lu(&fmt);
        let baseline = if top { from + size * 4 / 5 } else { ph - from };
        text_line(&mut out, fonts, &text, &fmt, hf.align, mx, width, baseline);
    }

    // Table rules: each cell draws its top and left rule; the right and
    // bottom ones only where no cell continues, so shared edges come once.
    for c in &page.cells {
        let (x, y, w, h) = (mx + c.x, my + c.y, c.w, c.h);
        let right = !page.cells.iter().any(|d| d.y == c.y && d.x == c.x + c.w);
        let below = !page.cells.iter().any(|d| d.y == c.y + c.h && d.x < c.x + c.w && d.x + d.w > c.x);
        out.push(Item::Rect { x, y, w, h: RULE, color: RULE_COLOR });
        out.push(Item::Rect { x, y, w: RULE, h, color: RULE_COLOR });
        if right {
            out.push(Item::Rect { x: x + w, y, w: RULE, h: h + RULE, color: RULE_COLOR });
        }
        if below {
            out.push(Item::Rect { x, y: y + h, w: w + RULE, h: RULE, color: RULE_COLOR });
        }
    }

    // Floating pictures behind the text first; the rest go over it.
    let float = |out: &mut Vec<Item>, f: &crate::paginate::PlacedFloat| {
        out.push(object_item(&f.object, mx + f.x, my + f.y, f.w, f.h));
    };
    for f in page.floats.iter().filter(|f| f.wrap() == Wrap::Behind) {
        float(&mut out, f);
    }

    // Line numbers count on from the pages before (unless they start again
    // on each page).
    let numbering_lines = doc.settings.line_numbers;
    let mut line_no = numbering_lines.map_or(0, |n| lines_before(doc, pages, i, n.restart, cache, fonts));
    for s in &page.slices {
        let Some(p) = doc.para(s.block) else { continue };
        // The width the slice was laid out in (its column's, a cell's inside),
        // round the floats it was laid out round.
        let w = s.w;
        let l = cache.get_wrapped(doc, p, w, fonts, s.wrap.as_deref().map_or(&[], |v| v.as_slice()));
        let left = mx + s.x;
        para_box(&mut out, doc, &l, s, left, my);
        if s.lines.start == 0 {
            if let Some(label) = numbering.label(s.block) {
                list_label(&mut out, &l, &label, fonts, left, my + s.y);
            }
        }
        drop_cap(&mut out, &l, s.lines.clone(), left, my + s.y);
        let counted = p.props.cell.is_none() && l.format.line_numbers;
        let mut ly = s.y;
        for li in s.lines.clone() {
            if let (Some(n), true) = (numbering_lines, counted) {
                if numbering_lines.is_some_and(|x| x.restart == LineRestart::Section) && li == 0 && p.props.section.is_some() {
                    line_no = 0;
                }
                line_no += 1;
                let k = n.start.saturating_add(line_no - 1);
                if k % n.count_by.max(1) as u32 == 0 {
                    let fmt = CharFormat { size: doc.styles.default_char.size, color: doc.styles.default_char.color, family: doc.styles.default_char.family.clone(), ..Default::default() };
                    let gap = twips(if n.distance > 0 { n.distance } else { 360 } as i64);
                    let label = k.to_string();
                    let wn: i64 = fonts.shape(&label, &fmt).iter().map(|g| g.adv).sum();
                    runs(&mut out, fonts, &label, &fmt, mx - gap - wn, baseline_of(&l, li, my + ly));
                }
            }
            line(&mut out, doc, fonts, &l, li, &p.text, left, my + ly, Some(s.block));
            ly += l.lines[li].height;
        }
    }

    for f in page.floats.iter().filter(|f| f.wrap() != Wrap::Behind) {
        float(&mut out, f);
    }

    // Lines between columns.
    for &(x, top, bottom) in &page.rules {
        out.push(Item::Rect { x: mx + x - RULE / 2, y: my + top, w: RULE, h: (bottom - top).max(RULE), color: RULE_COLOR });
    }

    // Footnotes at the foot, endnotes after the text: a short rule above
    // each, then each note's label in its hang and its text.
    for y in [page.foot_rule, page.end_rule].into_iter().flatten() {
        out.push(Item::Rect { x: mx, y: my + y + NOTE_RULE_SPACE / 2 - RULE / 2, w: (col_w / 3).max(crate::LU), h: RULE, color: 0x000000 });
    }
    for s in &page.notes {
        let Some(note) = doc.notes.get(s.note) else { continue };
        let Some(p) = note.paras.get(s.para) else { continue };
        let l = cache.get(doc, p, (col_w - NOTE_HANG).max(crate::LU), fonts);
        let left = mx + NOTE_HANG;
        if s.para == 0 && s.lines.start == 0 && !l.lines.is_empty() {
            let mut f = l.glyphs.first().map_or(&l.formats[0], |g| &l.formats[g.fmt as usize]).clone();
            f.underline = false;
            f.strike = false;
            f.link = None;
            f.object = None;
            f.note = None;
            f.vert = crate::model::VertAlign::Super;
            let shift = vert_shift(&mut f);
            runs(&mut out, fonts, &doc.note_label(s.note), &f, mx, baseline_of(&l, 0, my + s.y) + shift);
        }
        let mut ly = s.y;
        for li in s.lines.clone() {
            if li >= l.lines.len() {
                break;
            }
            line(&mut out, doc, fonts, &l, li, &p.text, left, my + ly, None);
            ly += l.lines[li].height;
        }
    }
    expand_shapes(out, doc, cache, fonts)
}

/// How many numbered lines the pages before page `i` hold since numbering
/// last started again.
fn lines_before(doc: &TextDoc, pages: &[Page], i: usize, restart: LineRestart, cache: &mut LayoutCache, fonts: &dyn Fonts) -> u32 {
    if restart == LineRestart::Page {
        return 0;
    }
    let mut n = 0u32;
    for page in &pages[..i.min(pages.len())] {
        for s in &page.slices {
            let Some(p) = doc.para(s.block) else { continue };
            if p.props.cell.is_some() {
                continue;
            }
            if restart == LineRestart::Section && s.lines.start == 0 && p.props.section.is_some() {
                n = 0;
            }
            let l = cache.get_wrapped(doc, p, s.w, fonts, s.wrap.as_deref().map_or(&[], |v| v.as_slice()));
            if l.format.line_numbers {
                n = n.saturating_add(s.lines.len() as u32);
            }
        }
    }
    n
}

/// Whether block `b` is a paragraph in the same box as one whose format
/// is `l` (the same borders and shading, both in or out of a cell).
fn same_box(doc: &TextDoc, b: usize, l: &ParaLayout, cell: bool) -> bool {
    let Some(q) = doc.para(b) else { return false };
    if q.props.cell.is_some() != cell {
        return false;
    }
    let (f, _) = doc.styles.para_format(q.style, &q.props);
    f.borders == l.format.borders && f.shading == l.format.shading && f.indent_start == l.format.indent_start && f.indent_end == l.format.indent_end
}

/// Slice `s`'s shading and borders, behind its text. Paragraphs in one box
/// draw the top border above the first and the bottom below the last, the
/// shading over the space between them.
fn para_box(out: &mut Vec<Item>, doc: &TextDoc, l: &ParaLayout, s: &crate::paginate::Slice, left: i64, my: i64) {
    let b = l.format.borders;
    if b.is_empty() && l.format.shading.is_none() {
        return;
    }
    let cell = doc.para(s.block).is_some_and(|p| p.props.cell.is_some());
    let above = s.block > 0 && same_box(doc, s.block - 1, l, cell);
    let below = same_box(doc, s.block + 1, l, cell);
    let starts = s.lines.start == 0;
    let ends = s.lines.end >= l.lines.len();
    let lines_h: i64 = s.lines.clone().filter_map(|k| l.lines.get(k)).map(|x| x.height).sum();
    let top = my + s.y - if starts { if above { l.space_before } else { l.pad_top } } else { 0 };
    let bottom = my + s.y + lines_h + if ends { if below { l.space_after } else { l.pad_bottom } } else { 0 };
    let room = |x: Option<Border>| x.map_or(0, |x| twips(x.space as i64 * 20 + x.thickness() as i64));
    let x0 = left + twips(l.format.indent_start as i64) - room(b.start);
    let x1 = left + s.w - twips(l.format.indent_end as i64) + room(b.end);
    if let Some(c) = l.format.shading {
        out.push(Item::Rect { x: x0, y: top, w: (x1 - x0).max(0), h: (bottom - top).max(0), color: c });
    }
    let thick = |x: &Border| twips(x.thickness() as i64).max(1);
    if starts {
        let edge = if above { b.between } else { b.top };
        if let Some(x) = edge {
            rule(out, &x, x0, top, x1 - x0, thick(&x), true);
        }
    }
    if ends && !below {
        if let Some(x) = b.bottom {
            rule(out, &x, x0, bottom - thick(&x), x1 - x0, thick(&x), true);
        }
    }
    if let Some(x) = b.start {
        rule(out, &x, x0, top, bottom - top, thick(&x), false);
    }
    if let Some(x) = b.end {
        rule(out, &x, x1 - thick(&x), top, bottom - top, thick(&x), false);
    }
}

/// A border line `len` long from (`x`, `y`), across (`flat`) or down,
/// `t` thick, in its style.
fn rule(out: &mut Vec<Item>, b: &Border, x: i64, y: i64, len: i64, t: i64, flat: bool) {
    let color = b.color.unwrap_or(0);
    let mut seg = |a: i64, n: i64, off: i64, th: i64| {
        if flat {
            out.push(Item::Rect { x: x + a, y: y + off, w: n, h: th, color });
        } else {
            out.push(Item::Rect { x: x + off, y: y + a, w: th, h: n, color });
        }
    };
    match b.style {
        BorderStyle::Single | BorderStyle::Thick => seg(0, len, 0, t),
        BorderStyle::Double => {
            let th = (t / 3).max(1);
            seg(0, len, 0, th);
            seg(0, len, t - th, th);
        }
        BorderStyle::Dotted | BorderStyle::Dashed => {
            let (on, off) = if b.style == BorderStyle::Dotted { (t, t) } else { (t * 3, t * 2) };
            let mut a = 0;
            while a < len {
                seg(a, on.min(len - a), 0, t);
                a += on + off;
            }
        }
    }
}

/// A dropped capital, on the baseline of its last line, when slice
/// `lines` holds that line (`top`: the slice's first line's top).
fn drop_cap(out: &mut Vec<Item>, l: &ParaLayout, lines: Range<usize>, left: i64, top: i64) {
    let Some(d) = &l.drop else { return };
    let last = (d.lines as usize).min(l.lines.len()).saturating_sub(1);
    if !lines.contains(&last) || lines.start != 0 {
        return;
    }
    let y: i64 = l.lines[..last].iter().map(|x| x.height).sum();
    let baseline = baseline_of(l, last, top + y);
    let mut run = GlyphRun { face: d.glyphs.first().map_or(0, |g| g.face), size: size_lu(&d.format), color: d.format.color, x: left + d.x, y: baseline, glyphs: Vec::new(), text: d.text.clone(), turn: None };
    let mut pen = 0;
    for (k, g) in d.glyphs.iter().enumerate() {
        let range = if k == 0 { 0..d.text.len() as u32 } else { d.text.len() as u32..d.text.len() as u32 };
        run.glyphs.push(PaintGlyph { id: g.id, x: pen + g.dx, dy: g.dy, text: range });
        pen += g.adv;
    }
    if !run.glyphs.is_empty() {
        out.push(Item::Glyphs(run));
    }
}

/// A picture's or shape's item in the frame.
fn object_item(o: &crate::model::ObjectRef, x: i64, y: i64, w: i64, h: i64) -> Item {
    if o.shape {
        Item::Shape { shape: o.image, x, y, w, h }
    } else {
        Item::Image { image: o.image, x, y, w, h }
    }
}

/// The baseline of line `li` whose top is `top`.
fn baseline_of(l: &ParaLayout, li: usize, top: i64) -> i64 {
    let line = &l.lines[li];
    let natural = line.ascent + line.descent;
    top + line.skip + line.ascent + (line.height - line.skip - natural).max(0) / 2
}

/// The text a glyph shows: its cluster, up to the next cluster.
fn cluster_text(text: &str, start: u32, next: Option<u32>) -> &str {
    let s = start as usize;
    let e = next.map_or_else(|| text[s..].chars().next().map_or(s, |c| s + c.len_utf8()), |n| n as usize);
    text.get(s..e.max(s)).unwrap_or("")
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn line(out: &mut Vec<Item>, doc: &TextDoc, fonts: &dyn Fonts, l: &ParaLayout, li: usize, text: &str, left: i64, top: i64, block: Option<usize>) {
    let line = &l.lines[li];
    let baseline = baseline_of(l, li, top);
    // Room skipped for floats above the line isn't the line's.
    let top = top + line.skip;
    let lh = line.height - line.skip;
    let bytes = text.as_bytes();
    // Drawn left to right: the line's visual order (bidi).
    let vis = !line.visual.is_empty();
    let seq: Vec<crate::layout::Glyph> = l.visual_order(li).into_iter().map(|k| l.glyphs[k]).collect();
    let gl = &seq[..];
    let mut pen = line.x;
    let mut i = 0;
    while i < gl.len() {
        let (face, fmt) = (gl[i].face, gl[i].fmt);
        let f = &l.formats[fmt as usize];
        if face == OBJECT_FACE {
            let g = gl[i];
            if let (Some(t), Some(b)) = (f.deleted, block) {
                if let Some(d) = doc.deletions.get(t) {
                    out.push(Item::Change { x: left + pen, y: top, w: 0, h: lh, block: b, start: g.cluster, end: g.cluster + OBJECT_CHAR.len_utf8() as u32, rev: d.rev });
                }
            }
            if let Some(o) = f.object.as_ref().filter(|o| o.float.is_none()) {
                let h = object_height(o, g.adv);
                out.push(object_item(o, left + pen, baseline - h, g.adv, h));
                if let Some(u) = &f.link {
                    out.push(Item::Link { x: left + pen, y: baseline - h, w: g.adv, h, uri: u.to_string() });
                }
            }
            pen += g.adv;
            i += 1;
            continue;
        }
        let x0 = pen;
        let first = i;
        let mut run = GlyphRun { face, size: size_lu(f), color: f.color, x: left + x0, y: baseline, glyphs: Vec::new(), text: String::new(), turn: None };
        let mut last_cluster = None;
        while i < gl.len() && gl[i].face == face && gl[i].fmt == fmt {
            let g = gl[i];
            let ch = text[g.cluster as usize..].chars().next().unwrap_or(' ');
            if !ch.is_control() && ch != '\u{2028}' && ch != '\t' {
                let range = if last_cluster == Some(g.cluster) {
                    let n = run.text.len() as u32;
                    n..n
                } else {
                    // The cluster runs to the next glyph's cluster in the line
                    // (or one char at the line's end).
                    let next = if vis { gl.iter().map(|n| n.cluster).filter(|&c| c > g.cluster).min() } else { gl[i + 1..].iter().map(|n| n.cluster).find(|&c| c > g.cluster) };
                    let next = next.or(Some(line.end)).filter(|&c| c > g.cluster && c <= line.end);
                    let t = cluster_text(text, g.cluster, next);
                    let n = run.text.len() as u32;
                    // A note anchor's glyphs show its label.
                    match f.note {
                        Some(id) if ch == OBJECT_CHAR => run.text.push_str(&doc.note_label(id)),
                        _ => match f.field {
                            Some(x) if ch == OBJECT_CHAR => run.text.push_str(&doc.field_text(x)),
                            _ => run.text.push_str(t),
                        },
                    }
                    n..run.text.len() as u32
                };
                run.glyphs.push(PaintGlyph { id: g.id, x: pen - x0 + g.dx, dy: g.dy, text: range });
            }
            last_cluster = Some(g.cluster);
            pen += g.adv;
            if line.space_extra != 0 && bytes.get(g.cluster as usize) == Some(&b' ') {
                pen += line.space_extra;
            }
            i += 1;
        }
        let (rx, rw) = (left + x0, pen - x0);
        let size = run.size;
        if let (Some(rev), Some(b)) = (f.rev.or(f.fmt.map(|m| m.rev)), block) {
            let (start, end) = if vis {
                let run = &gl[first..i];
                let lo = run.iter().map(|g| g.cluster).min().unwrap_or(line.start);
                let hi = run.iter().map(|g| g.cluster).max().unwrap_or(lo);
                (lo, gl.iter().map(|g| g.cluster).filter(|&c| c > hi).min().unwrap_or(line.end))
            } else {
                let start = gl.get(first).map_or(line.start, |g| g.cluster);
                (start, gl.get(i).map_or(line.end, |g| g.cluster).max(start))
            };
            out.push(Item::Change { x: rx, y: top, w: rw, h: lh, block: b, start, end, rev });
        }
        if let Some(h) = f.highlight {
            out.push(Item::Rect { x: rx, y: top, w: rw, h: lh, color: h });
        }
        if let Some(c) = &f.comments {
            out.push(Item::Mark { x: rx, y: top, w: rw, h: lh, comments: c.clone() });
        }
        let color = f.color;
        if !run.glyphs.is_empty() {
            effects(out, &mut run, f.effects, size);
            out.push(Item::Glyphs(run));
        }
        let thick = (size / 14).max(crate::LU / 2);
        if f.underline || f.link.is_some() {
            out.push(Item::Rect { x: rx, y: baseline + size * 12 / 100, w: rw, h: thick, color });
        }
        if f.strike && !f.dstrike {
            out.push(Item::Rect { x: rx, y: baseline - size * 28 / 100, w: rw, h: thick, color });
        }
        if f.dstrike {
            for y in [baseline - size * 33 / 100, baseline - size * 22 / 100] {
                out.push(Item::Rect { x: rx, y, w: rw, h: (thick * 3 / 4).max(1), color });
            }
        }
        if let Some(u) = &f.link {
            out.push(Item::Link { x: rx, y: top, w: rw, h: lh, uri: u.to_string() });
        }
    }
    if let Some(fi) = line.hyphen {
        if let Some(f) = l.formats.get(fi as usize) {
            runs(out, fonts, "-", f, left + pen, baseline);
        }
    }
    // Tab leaders, in the format of the text after the tab.
    for &(a, b, leader) in &line.leaders {
        let fi = gl.iter().find(|g| bytes.get(g.cluster as usize) == Some(&b'\t')).map_or(0, |g| g.fmt);
        let f = &l.formats[fi as usize];
        let size = size_lu(f);
        let thick = (size / 14).max(crate::LU / 2);
        match leader {
            Leader::None => {}
            Leader::Line => out.push(Item::Rect { x: left + a, y: baseline + size * 12 / 100, w: b - a, h: thick, color: f.color }),
            Leader::Dots | Leader::Dashes => {
                let mark = if leader == Leader::Dots { "." } else { "-" };
                let Some(g) = fonts.shape(mark, f).first().copied() else { continue };
                // On a grid from the paragraph's edge, so leaders line up.
                let pitch = (g.adv * 2).max(crate::LU);
                let mut x = (a + pitch / 2 + pitch - 1) / pitch * pitch;
                let mut run = GlyphRun { face: g.face, size, color: f.color, x: left + x, y: baseline, glyphs: Vec::new(), text: String::new(), turn: None };
                let origin = x;
                while x + g.adv <= b - pitch / 2 {
                    let n = run.text.len() as u32;
                    run.text.push_str(mark);
                    run.glyphs.push(PaintGlyph { id: g.id, x: x - origin + g.dx, dy: g.dy, text: n..n + 1 });
                    x += pitch;
                }
                if !run.glyphs.is_empty() {
                    out.push(Item::Glyphs(run));
                }
            }
        }
    }
}

/// Glyphs for `text` shaped in `fmt` from `x`, as runs by face.
fn runs(out: &mut Vec<Item>, fonts: &dyn Fonts, text: &str, fmt: &CharFormat, x: i64, baseline: i64) {
    let glyphs = crate::bidi::reorder(fonts.shape(text, fmt), text);
    let vis = crate::bidi::levels(text).is_some();
    let size = size_lu(fmt);
    let mut pen = 0i64;
    let mut i = 0;
    while i < glyphs.len() {
        let face = glyphs[i].face;
        let start = pen;
        let mut run = GlyphRun { face, size, color: fmt.color, x: x + start, y: baseline, glyphs: Vec::new(), text: String::new(), turn: None };
        while i < glyphs.len() && glyphs[i].face == face {
            let g = glyphs[i];
            let first = i == 0 || glyphs[i - 1].cluster != g.cluster;
            let range = if first {
                let next = if vis { glyphs.iter().map(|n| n.cluster).filter(|&c| c > g.cluster).min() } else { glyphs[i + 1..].iter().map(|n| n.cluster).find(|&c| c > g.cluster) };
                let n = run.text.len() as u32;
                run.text.push_str(cluster_text(text, g.cluster, next));
                n..run.text.len() as u32
            } else {
                let n = run.text.len() as u32;
                n..n
            };
            run.glyphs.push(PaintGlyph { id: g.id, x: pen - start + g.dx, dy: g.dy, text: range });
            pen += g.adv;
            i += 1;
        }
        out.push(Item::Glyphs(run));
    }
}

/// The items of laid-out paragraph `l` (text `text`) with its first line's
/// top at `top` and its start edge at `left`, and `label` in its hang if it
/// has one: a paragraph outside a page (a slide's text frame).
#[allow(clippy::too_many_arguments)]
pub fn paint_para(out: &mut Vec<Item>, doc: &TextDoc, fonts: &dyn Fonts, l: &ParaLayout, text: &str, left: i64, top: i64, label: Option<(&str, Option<CharFormat>)>) {
    if let Some((label, fmt)) = label {
        match fmt {
            Some(f) if !l.lines.is_empty() => {
                let x0 = twips((l.format.indent_start + l.format.first_line) as i64);
                runs(out, fonts, label, &f, left + x0, baseline_of(l, 0, top));
            }
            _ => list_label(out, l, label, fonts, left, top),
        }
    }
    drop_cap(out, l, 0..l.lines.len(), left, top);
    let mut y = top;
    for li in 0..l.lines.len() {
        line(out, doc, fonts, l, li, text, left, y, None);
        y += l.lines[li].height;
    }
}

/// A list item's label in its hang, on the first line's baseline, in the
/// format the item's text starts with.
fn list_label(out: &mut Vec<Item>, l: &ParaLayout, label: &str, fonts: &dyn Fonts, left: i64, top: i64) {
    if l.lines.is_empty() {
        return;
    }
    let x0 = twips((l.format.indent_start + l.format.first_line) as i64);
    runs(out, fonts, label, &list_label_format(l), left + x0, baseline_of(l, 0, top));
}

/// The default format for a list label, shared by measurement and painting.
pub fn list_label_format(l: &ParaLayout) -> CharFormat {
    let mut f = l.glyphs.first().map_or(&l.formats[0], |g| &l.formats[g.fmt as usize]).clone();
    f.underline = false;
    f.strike = false;
    f.link = None;
    f.object = None;
    f
}

/// One line of text in `fmt`, aligned in a box `width` wide from `left`.
#[allow(clippy::too_many_arguments)]
fn text_line(out: &mut Vec<Item>, fonts: &dyn Fonts, text: &str, fmt: &CharFormat, align: Align, left: i64, width: i64, baseline: i64) {
    let w: i64 = fonts.shape(text, fmt).iter().map(|g| g.adv).sum();
    let x0 = match align {
        Align::Center => left + (width - w) / 2,
        Align::End => left + width - w,
        _ => left,
    };
    runs(out, fonts, text, fmt, x0, baseline);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FixedFonts;
    use crate::model::{CharAttrs, HeaderFooter, ImageData, ObjectRef};
    use crate::paginate::Paginator;

    #[test]
    fn a_page_paints_text_rules_pictures_links_and_footer() {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let link = d.attrs.intern(CharAttrs { link: Some("https://example.org/".into()), ..Default::default() });
        d.push_para("Hello world", 0, vec![(0, 0), (6, link)]);
        let img = d.images.add(ImageData { bytes: std::sync::Arc::from(vec![0u8; 4]), width: 10, height: 10 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 1440, height: 720, float: None, shape: false }), ..Default::default() });
        d.push_para("a\u{FFFC}b", 0, vec![(0, 0), (1, pic), (4, 0)]);
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center, paras: Vec::new() });
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "1");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        assert_eq!(texts, ["Page 1 of 1", "Hello ", "world", "a", "b"]);
        assert!(items.iter().any(|i| matches!(i, Item::Link { uri, .. } if uri == "https://example.org/")));
        assert!(items.iter().any(|i| matches!(i, Item::Image { w, h, .. } if *w == twips(1440) && *h == twips(720))));
        // Underline under the link.
        assert!(items.iter().any(|i| matches!(i, Item::Rect { .. })));
        // Every glyph of a run points at its text.
        for i in &items {
            if let Item::Glyphs(r) = i {
                let joined: String = r.glyphs.iter().map(|g| &r.text[g.text.start as usize..g.text.end as usize]).collect();
                assert_eq!(joined, r.text);
            }
        }
    }

    /// Two paragraphs with the same borders and shading are one box: one
    /// top and one bottom border, the shading over the gap between; a
    /// dropped capital is drawn large; line numbers stand in the margin.
    #[test]
    fn boxes_drop_caps_and_line_numbers_paint() {
        use crate::model::{Border, DropCap, LineNumbering, ParaBorders};
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let b = Border { color: Some(0x0000ff), ..Default::default() };
        for t in ["One", "Two", "Plain"] {
            let k = d.push_para(t, 0, vec![]);
            if t != "Plain" {
                d.blocks.update(k, |x| {
                    let p = &mut x.as_para_mut().unwrap().props;
                    p.borders = Some(ParaBorders { top: Some(b), bottom: Some(b), ..Default::default() });
                    p.shading = Some(Some(0xeeeeee));
                });
            }
        }
        d.blocks.update(2, |x| x.as_para_mut().unwrap().props.drop_cap = Some(Some(DropCap { lines: 2, distance: 0, margin: false })));
        d.settings.line_numbers = Some(LineNumbering::default());
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "1");
        let rects = |c: u32| items.iter().filter_map(|i| if let Item::Rect { y, h, color, .. } = i { (*color == c).then_some((*y, *h)) } else { None }).collect::<Vec<_>>();
        assert_eq!(rects(0x0000ff).len(), 2, "one top and one bottom border");
        let shade = rects(0xeeeeee);
        assert_eq!(shade.len(), 2);
        assert_eq!(shade[0].0 + shade[0].1, shade[1].0, "the shading runs on over the gap");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        assert_eq!(texts, ["1", "One", "2", "Two", "P", "3", "lain"]);
        let big = items.iter().find_map(|i| if let Item::Glyphs(r) = i { (r.text == "P").then_some(r.size) } else { None }).unwrap();
        assert!(big > size_lu(&CharFormat::default()) * 2);
    }

    #[test]
    fn shapes_paint_their_outline_and_text() {
        use crate::model::{Float, Geom, Para, ShapeData, ShapeLine, TextAnchor};
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let mut sd = ShapeData::new(Geom::preset("ellipse"));
        sd.fill = Some(0x4472C4);
        sd.line = Some(ShapeLine { color: 0x000000, width: 0, dash: Dash::Dash, head: Default::default(), tail: Default::default() });
        sd.anchor = TextAnchor::Middle;
        sd.paras.push(Para::new(d.new_id(), "Inside", 0));
        let id = d.shapes.add(sd).unwrap();
        let mut box_ = ShapeData::new(Geom::preset("rect"));
        box_.rot = 90 * 60_000;
        let id2 = d.shapes.add(box_).unwrap();
        let a = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: id, width: 2880, height: 1440, float: None, shape: true }), ..Default::default() });
        let f = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: id2, width: 1440, height: 720, float: Some(Float::default()), shape: true }), ..Default::default() });
        d.push_para("x\u{FFFC}y\u{FFFC}", 0, vec![(0, 0), (1, a), (4, 0), (5, f)]);
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "1");
        assert!(!items.iter().any(|i| matches!(i, Item::Shape { .. } | Item::Image { .. })));
        let paths: Vec<_> = items.iter().filter_map(|i| if let Item::Path { cmds, fill, line } = i { Some((cmds, fill, line)) } else { None }).collect();
        assert_eq!(paths.len(), 1, "the unfilled, unoutlined box draws nothing");
        let (cmds, fill, line) = paths[0];
        assert_eq!(*fill, Some(0x4472C4));
        assert_eq!(line.map(|l| (l.1, l.2)), Some((crate::LU / 2, Dash::Dash)), "a hairline at least");
        // The ellipse's points span its frame: 2 in by 1 in.
        let xs: Vec<i64> = cmds.iter().filter_map(|c| if let PathCmd::Move(x, _) | PathCmd::Line(x, _) | PathCmd::Cubic(_, _, _, _, x, _) = c { Some(*x) } else { None }).collect();
        let span = xs.iter().max().unwrap() - xs.iter().min().unwrap();
        assert!((span - twips(2880)).abs() <= 2, "{span}");
        // Its text in the middle, after the outline.
        let ti = items.iter().position(|i| matches!(i, Item::Glyphs(r) if r.text == "Inside")).unwrap();
        let pi = items.iter().position(|i| matches!(i, Item::Path { .. })).unwrap();
        assert!(ti > pi);
    }

    #[test]
    fn a_turned_shapes_words_turn_with_it() {
        use crate::model::{Geom, Para, ShapeData};
        let mut d = TextDoc::new();
        let u = d.attrs.intern(CharAttrs { underline: Some(true), ..Default::default() });
        let shape = |d: &mut TextDoc, rot: i32, flip_v: bool| {
            let mut sd = ShapeData::new(Geom::preset("rect"));
            sd.rot = rot;
            sd.flip_v = flip_v;
            let mut p = Para::new(d.new_id(), "Turned", 0);
            p.runs = vec![(0, u)];
            sd.paras.push(p);
            d.shapes.add(sd).unwrap()
        };
        let (up, quarter, flipped) = (shape(&mut d, 0, false), shape(&mut d, 90 * 60_000, false), shape(&mut d, 0, true));
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let items = |d: &TextDoc, cache: &mut LayoutCache, id: u32| {
            let mut out = Vec::new();
            shape_items(&mut out, d, cache, &fonts, id, 0, 0, 20_000, 10_000);
            out
        };
        let run = |v: &[Item]| v.iter().find_map(|i| if let Item::Glyphs(r) = i { Some(r.clone()) } else { None }).unwrap();
        let a = items(&d, &mut cache, up);
        let r0 = run(&a);
        assert_eq!(r0.turn, None);
        assert!(a.iter().any(|i| matches!(i, Item::Rect { .. })), "an upright underline");
        // A quarter turn clockwise about the middle (10000, 5000).
        let b = items(&d, &mut cache, quarter);
        let r1 = run(&b);
        let t = r1.turn.unwrap();
        assert!(t[0].abs() < 1e-9 && (t[1] - 1.0).abs() < 1e-9 && (t[2] + 1.0).abs() < 1e-9, "{t:?}");
        let (u, v) = (r0.x - 10_000, r0.y - 5_000);
        assert!((r1.x - (10_000 - v)).abs() <= 1 && (r1.y - (5_000 + u)).abs() <= 1, "({}, {}) from ({}, {})", r1.x, r1.y, r0.x, r0.y);
        assert_eq!(r1.glyphs, r0.glyphs, "the glyphs keep their upright offsets");
        assert!(!b.iter().any(|i| matches!(i, Item::Rect { .. })), "the underline turns too");
        // Flipped upside down: words half round, not mirrored.
        let r2 = run(&items(&d, &mut cache, flipped));
        let t = r2.turn.unwrap();
        assert!((t[0] + 1.0).abs() < 1e-9 && t[1].abs() < 1e-9, "{t:?}");
    }

    #[test]
    fn headers_of_their_own_paragraphs_paint_pictures_and_page_numbers() {
        use crate::model::Para;
        let mut d = TextDoc::new();
        d.push_para("Body", 0, vec![]);
        let img = d.images.add(ImageData { bytes: std::sync::Arc::from(vec![0u8; 4]), width: 10, height: 10 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 720, height: 360, float: None, shape: false }), ..Default::default() });
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        let mut logo = Para::new(d.new_id(), "\u{FFFC} Acme", 0);
        logo.runs = vec![(0, pic), (3, bold)];
        let mut num = Para::new(d.new_id(), "Page {page} of {pages}", 0);
        num.runs = vec![(0, 0), (5, bold), (11, 0)];
        d.header = Some(HeaderFooter { text: "Acme".into(), align: Align::Start, paras: vec![logo] });
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Start, paras: vec![num] });
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "7");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        assert!(texts.iter().any(|t| t.contains("Acme")), "{texts:?}");
        // The page number keeps the bold its placeholder had.
        assert!(texts.windows(3).any(|w| w == ["Page ", "1", " of 7"]), "{texts:?}");
        let pic_y = items.iter().find_map(|i| if let Item::Image { y, h, .. } = i { Some(*y + *h) } else { None }).unwrap();
        // In the top margin.
        assert!(pic_y <= twips(d.page.margin_top as i64), "{pic_y}");
    }

    #[test]
    fn first_and_even_pages_paint_their_own_footers() {
        use crate::model::Margins;
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for i in 0..150 {
            d.push_para(&format!("Paragraph {i} of the body text."), 0, vec![]);
        }
        d.footer = Some(HeaderFooter::line("Odd {page}", Align::Center));
        d.first_page = Some(Margins { header: None, footer: Some(HeaderFooter::line("Title", Align::Center)) });
        d.even_pages = Some(Margins { header: None, footer: Some(HeaderFooter::line("Even {page}", Align::Center)) });
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        assert!(p.pages().len() >= 4);
        let foot = |i: usize, cache: &mut LayoutCache| {
            let items = paint_page(&d, p.pages(), i, p.width(), cache, &fonts, &Numbering::build(&d), "9");
            items.iter().find_map(|x| if let Item::Glyphs(r) = x { Some(r.text.clone()) } else { None }).unwrap()
        };
        assert_eq!([foot(0, &mut cache), foot(1, &mut cache), foot(2, &mut cache), foot(3, &mut cache)], ["Title", "Even 2", "Odd 3", "Even 4"]);
    }

    #[test]
    fn retext_keeps_the_format_round_the_change() {
        use crate::model::Para;
        let mut d = TextDoc::new();
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        let img = d.images.add(ImageData { bytes: std::sync::Arc::from(vec![1u8; 4]), width: 1, height: 1 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 20, height: 20, float: None, shape: false }), ..Default::default() });
        let p = Para { runs: vec![(0, pic), (3, 0), (4, bold), (8, 0)], ..Para::new(1, "\u{FFFC} Acme Ltd", 0) };
        // A word typed after the bold one: plain, as the space before it.
        let q = p.retext("\u{FFFC} Acme Big Ltd", &d.attrs, 0);
        assert_eq!(q.runs, [(0, pic), (3, 0), (4, bold), (8, 0)]);
        // Typed inside the bold word: bold; the picture kept.
        let q = p.retext("\u{FFFC} Acmes Ltd", &d.attrs, 0);
        assert_eq!(q.runs, [(0, pic), (3, 0), (4, bold), (9, 0)]);
        // Typed right after the picture: plain, not a second picture.
        let q = p.retext("\u{FFFC}X Acme Ltd", &d.attrs, 0);
        assert_eq!(q.runs, [(0, pic), (3, 0), (5, bold), (9, 0)]);
        // Everything replaced.
        assert_eq!(p.retext("New", &d.attrs, 0).runs, [(0, 0)]);
        assert_eq!(p.retext("", &d.attrs, 0).text, "");
    }

    #[test]
    fn a_headers_table_rows_become_lines() {
        use crate::model::{rows_as_lines, CellRef, Para};
        let cell = |row, col| Some(CellRef { table: 1, row, col, width: 1000 });
        let mk = |t: &str, c| Para { props: crate::model::ParaProps { cell: c, ..Default::default() }, ..Para::new(0, t, 0) };
        let v = rows_as_lines(vec![mk("Title", None), mk("A", cell(0, 0)), mk("B", cell(0, 1)), mk("b2", cell(0, 1)), mk("C", cell(1, 0)), mk("", cell(1, 1)), mk("D", cell(1, 2))]);
        assert_eq!(v.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Title", "A\tB b2", "C\tD"]);
        assert!(v.iter().all(|p| p.props.cell.is_none() && p.runs.iter().all(|r| r.0 as usize <= p.text.len())));
    }

    #[test]
    fn notes_paint_numbers_rules_and_text() {
        use crate::model::NoteKind;
        use officina_core::history::Op;
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let k = d.push_para("Body text here", 0, vec![]);
        for (at, kind, t) in [(4u32, NoteKind::Foot, "foot text"), (9, NoteKind::End, "end text")] {
            for op in d.insert_note_ops(k, at, kind, t, 0) {
                op.apply(&mut d);
            }
        }
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        assert_eq!(p.pages().len(), 1);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "1");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        // The anchors show their numbers, raised and smaller; each note its number and text.
        assert!(texts.contains(&"1".to_string()) && texts.contains(&"i".to_string()), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("foot")) && texts.iter().any(|t| t.contains("end")), "{texts:?}");
        let small = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r) } else { None }).find(|r| r.text == "1").unwrap();
        assert!(small.size < crate::pt(11));
        assert!(p.pages()[0].foot_rule.is_some() && p.pages()[0].end_rule.is_some());
    }
}

/// Text effects as copies of the run under it: a shadow down and right in
/// grey; emboss and engrave as light and dark edges; an outline as the
/// letters' edge in their colour with the inside left white.
fn effects(out: &mut Vec<Item>, run: &mut GlyphRun, fx: u8, size: i64) {
    use crate::model::{EFFECT_EMBOSS, EFFECT_ENGRAVE, EFFECT_OUTLINE, EFFECT_SHADOW};
    if fx == 0 {
        return;
    }
    let d = (size / 20).max(crate::LU / 2);
    let copy = |out: &mut Vec<Item>, run: &GlyphRun, dx: i64, dy: i64, color: u32| {
        out.push(Item::Glyphs(GlyphRun { x: run.x + dx, y: run.y + dy, color, text: String::new(), ..run.clone() }));
    };
    if fx & EFFECT_SHADOW != 0 {
        copy(out, run, d, d, 0x9a9a9a);
    }
    if fx & (EFFECT_EMBOSS | EFFECT_ENGRAVE) != 0 {
        let (hi, lo) = if fx & EFFECT_EMBOSS != 0 { (0xffffff, 0x808080) } else { (0x808080, 0xffffff) };
        copy(out, run, -d / 2, -d / 2, hi);
        copy(out, run, d / 2, d / 2, lo);
    }
    if fx & EFFECT_OUTLINE != 0 {
        let t = (d / 2).max(1);
        for (dx, dy) in [(-t, 0), (t, 0), (0, -t), (0, t)] {
            copy(out, run, dx, dy, run.color);
        }
        run.color = 0xffffff;
    }
}
