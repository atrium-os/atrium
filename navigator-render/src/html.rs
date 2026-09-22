//! M2.2 — profile HTML + CSS → NSG: block and inline layout, box paint.
//!
//! The profile's fixed rules (§3.1) are not options here: every box is
//! `border-box`, margins NEVER collapse, there are no floats. So a block's
//! outer height is margin-top + height + margin-bottom, always, and the
//! next block starts right after it.
//!
//! ★ WHAT IS NOT LAID OUT YET IS COUNTED, NOT FAKED. Inline-blocks are
//! skipped and counted, as is loose text directly inside a flex or grid
//! container. `unimplemented` is the list of what the conformance number
//! cannot yet claim.

use crate::fontset::{Family, FontSet};
use crate::{scale, Link, Rect, Report, Run, Scene, Shaper, Style as FontStyle, PX, U};
use navigator_dom::{Dom, Handle, Kind};
use navigator_style::cascade::{cascade, Env, Style};
use navigator_style::sheet::{parse_sheet, Diagnostic, Stylesheet};
use navigator_style::token::Pos;
use navigator_style::values::{Calc, Color, Rgba, V};
use std::collections::BTreeMap;

/// Rows (profile §3.3–§3.9) the layout READS. A non-initial value in any
/// other row is counted as unimplemented on every element that has one —
/// so a property the layout ignores can never be dropped silently (§5.1).
/// Value-level gaps inside a read row (flex as block, italic without an
/// italic face, …) are counted where they occur.
pub const READ_ROWS: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 49, 50, 55, 57, 59, 60, 63, 64];

pub struct HtmlOut {
    pub scene: Scene,
    pub report: Report,
    pub diagnostics: Vec<Diagnostic>,
    pub unimplemented: BTreeMap<&'static str, usize>,
}

/// px (computed, f64) → 1/64 px, round half away from zero: the ONE place a
/// float becomes layout geometry.
fn u(px: f64) -> U { let v = px * PX as f64; (if v >= 0.0 { v + 0.5 } else { v - 0.5 }) as U }

fn calc_eval(c: &Calc, basis: U) -> Option<f64> {
    Some(match c {
        Calc::Num(n) => *n,
        Calc::Len(l) => l.v,
        Calc::Pct(p) => basis as f64 / PX as f64 * p / 100.0,
        Calc::Add(a, b) => calc_eval(a, basis)? + calc_eval(b, basis)?,
        Calc::Sub(a, b) => calc_eval(a, basis)? - calc_eval(b, basis)?,
        Calc::Mul(a, b) => calc_eval(a, basis)? * calc_eval(b, basis)?,
        Calc::Div(a, b) => calc_eval(a, basis)? / calc_eval(b, basis)?,
    })
}

/// A length-or-percentage against `basis`; `None` for keywords (auto…).
fn len(v: &V, basis: U) -> Option<U> {
    match v {
        V::Len(l) => Some(u(l.v)),
        V::Pct(p) => Some(u(basis as f64 / PX as f64 * p / 100.0)),
        V::Calc(c) => calc_eval(c, basis).map(u),
        _ => None,
    }
}

fn rgba(c: &Color, current: u32) -> u32 {
    match c {
        Color::Rgba(Rgba { r, g, b, a }) => (*r as u32) << 24 | (*g as u32) << 16 | (*b as u32) << 8 | *a as u32,
        Color::Current => current,
        Color::Transparent => 0,
    }
}

fn color_of(s: &Style, prop: &str) -> u32 {
    let current = match s.get("color") { V::Color(c) => rgba(c, 0x000000ff), _ => 0x000000ff };
    match s.get(prop) { V::Color(c) => rgba(c, current), _ => current }
}

fn kw<'a>(s: &'a Style, prop: &str) -> &'a str { match s.get(prop) { V::Kw(k) => k, _ => "" } }

struct Cx<'a> {
    dom: &'a Dom,
    styles: &'a [Option<Style>],
    sh: Shaper<'a>,
    scene: Scene,
    report: Report,
    links: Vec<String>,
    unimplemented: BTreeMap<&'static str, usize>,
    /// A list item's marker, waiting for the first line box its content
    /// produces (which may be inside a nested block).
    marker: Option<(String, FontStyle, Line, bool)>,
    /// `Some(None)` while probing for a box's first baseline; the first line
    /// box laid out records its baseline (absolute y) here.
    baseline_probe: Option<Option<U>>,
    /// text-indent for the next first line of a block container.
    indent: Option<U>,
    /// Extra offset of the next block's content inside its box (table cells,
    /// where vertical-align positions the content within the row's height).
    content_dy: U,
    /// Set while a table lays out one of its own cells, so the "table part
    /// outside a table" counter fires only for a STRAY part in normal flow.
    table_part: bool,
    /// The initial containing block: the viewport, which is also the
    /// containing block of every `position: fixed` box (NSG has no scroll
    /// offset, so fixed and absolute differ only in which block they use).
    viewport: (U, Option<U>),
    /// The padding box of the nearest positioned ancestor — the containing
    /// block of an `absolute` descendant (x, y, w, h).
    pos_cb: (U, U, U, Option<U>),
    /// Set by `abs_box` so the one `block()` call it makes lays the box out
    /// instead of diverting it again.
    placing_abs: bool,
    /// Set by `abs_box`: the border-box origin `block()` must use instead of
    /// the one normal flow would give it.
    abs_origin: Option<(U, U)>,
    /// Stacking contexts, as ranges of `scene.order`, pushed when the box
    /// that opened one finishes painting (so: post-order).
    contexts: Vec<(i64, usize, usize, usize)>,
    /// Ranges of `scene.order` painted by a positioned box with
    /// `z-index: auto` — layer 6, but NOT a stacking context.
    hoists: Vec<(usize, usize)>,
    diagnostics: Vec<Diagnostic>,
}

pub fn render_html(html: &str, fonts: &FontSet, env: &Env) -> HtmlOut {
    let dom = navigator_dom::parse(html);
    let mut diagnostics = vec![];
    // Stylesheets: every <style>, in document order, one author layer each
    // (profile §3.10). External sheets are inputs supplied beside the
    // document; none are supplied yet, so each is a diagnostic.
    let mut sheets: Vec<Stylesheet> = vec![];
    for h in dom.by_tag_anywhere("style") {
        let p = parse_sheet(&dom.text_content(h));
        diagnostics.extend(p.diagnostics);
        if let Some(r) = p.refused { diagnostics.push(r) } else { sheets.push(p.sheet) }
    }
    for h in dom.by_tag_anywhere("link") {
        if dom.attr(h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet"))) {
            diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "input.stylesheet-not-supplied",
                msg: format!("<link rel=stylesheet href={:?}>: external stylesheets are supplied inputs; none given", dom.attr(h, "href").unwrap_or("")) });
        }
    }
    // ★ Inline `style` attributes are not in the profile's cascade (§3.10 has
    // UA and author layers only). Reported, never silently ignored — a
    // normalizer can rewrite them into rules.
    for (i, n) in dom.nodes.iter().enumerate() {
        if matches!(n.kind, Kind::Element(_)) && n.attrs.iter().any(|(k, _)| k == "style") {
            diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "input.style-attribute",
                msg: format!("<{} style=…> (node {i}): inline style attributes are not admitted; use a <style> rule", dom.tag(i as Handle).unwrap_or("?")) });
        }
    }
    let styled = cascade(&dom, &sheets, env);
    diagnostics.extend(styled.diagnostics);
    let mut cx = Cx { dom: &dom, styles: &styled.styles, sh: Shaper::new(fonts), scene: Scene { width: u(env.width_px), ..Default::default() },
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None, baseline_probe: None, indent: None, content_dy: 0, table_part: false,
                      viewport: (u(env.width_px), Some(u(env.height_px))), pos_cb: (0, 0, u(env.width_px), Some(u(env.height_px))),
                      placing_abs: false, abs_origin: None, contexts: vec![], hoists: vec![], diagnostics: vec![] };
    cx.count_unread_rows();
    // The root element is the initial containing block's only child.
    let root = dom.element_children(dom.root()).into_iter().next();
    // The root's containing block is the viewport: definite in both axes.
    let h = match root { Some(r) => cx.block(r, 0, 0, u(env.width_px), Some(u(env.height_px)), (None, None)), None => 0 };
    cx.scene.height = h.max(u(env.height_px));
    cx.scene.order = restack(&cx.scene.order, &cx.contexts, &cx.hoists);
    diagnostics.extend(cx.diagnostics);
    HtmlOut { scene: cx.scene, report: cx.report, diagnostics, unimplemented: cx.unimplemented }
}

#[derive(Clone)]
enum Item { Word(String, FontStyle, Line), Space(FontStyle, Line), Break,
            /// A tab in preserved white space: advance to the next tab stop.
            Tab(FontStyle, Line),
            /// Preserved spaces (pre-wrap): placed like a word, but they HANG
            /// at a line end instead of wrapping.
            PreSpace(String, FontStyle, Line) }

/// Per-item line metrics and decoration.
#[derive(Clone, Copy)]
struct Line { lh: U, underline: Option<u32>, strike: Option<u32>,
               /// visibility: hidden — takes its space, paints nothing.
               hidden: bool,
               /// overflow-wrap: break-word.
               break_word: bool,
               /// tab-size, in spaces.
               tab: i64 }

impl<'a> Cx<'a> {
    fn st(&self, h: Handle) -> Option<&'a Style> { self.styles.get(h as usize).and_then(|s| s.as_ref()) }
    fn count(&mut self, what: &'static str) { *self.unimplemented.entry(what).or_default() += 1 }

    /// Every element with a non-initial value in a row the layout does not
    /// read — keyed by the row's first longhand.
    fn count_unread_rows(&mut self) {
        use navigator_style::values::{grammar, parse_value, Specified, ROWS};
        let props = navigator_style::cascade::longhands();
        let unread: Vec<(usize, &'static str, V)> = props.iter().enumerate().filter_map(|(i, p)| {
            let row = ROWS.iter().find(|r| r.props.contains(p))?;
            if READ_ROWS.contains(&row.id) { return None }
            let (_, init, _) = grammar(p)?;
            let Ok(Specified::Value(v)) = parse_value(p, &navigator_style::token::tokenize(init), &[]) else { return None };
            Some((i, row.props[0], v))
        }).collect();
        for s in self.styles.iter().flatten() {
            for (i, first, init) in &unread {
                // Lengths are computed to px; the initial 0px compares equal.
                if &s.values[*i] != init {
                    let key: &'static str = Box::leak(format!("row {first}… (not implemented)").into_boxed_str());
                    *self.unimplemented.entry(key).or_default() += 1;
                }
            }
        }
    }

    fn is_block_level(&self, h: Handle) -> bool {
        match self.st(h) { Some(s) => matches!(kw(s, "display"), "block" | "flex" | "grid" | "table" | "table-row" | "table-cell"), None => false }
    }

    /// Lay out a block-level box at (x, y) in a containing block `cb_w` wide
    /// whose height is `cb_h` when DEFINITE. Returns its OUTER height
    /// (margins included, never collapsed).
    ///
    /// Percentages of the containing block's height (height, min/max-height,
    /// top/bottom) resolve against `cb_h`; when it is indefinite, CSS 2.1
    /// §10.5 makes them `auto` (none / no offset) — which is correct
    /// behaviour, not a gap, and is not counted as one.
    /// `force` = (border-box width, border-box height) imposed by a flex
    /// container on its item; `None` lets the box size itself.
    fn block(&mut self, h: Handle, x: U, y: U, cb_w: U, cb_h: Option<U>, force: (Option<U>, Option<U>)) -> U {
        let Some(s) = self.st(h) else { return 0 };
        let part = std::mem::take(&mut self.table_part);
        match kw(s, "display") {
            "none" => return 0,
            "table-row" | "table-cell" if !part => self.count("display: table-row/-cell outside a table (laid out as block)"),
            _ => {}
        }
        let positioned = kw(s, "position") != "static";
        if matches!(kw(s, "position"), "absolute" | "fixed") && !std::mem::take(&mut self.placing_abs) {
            // Out of flow: it takes no room where it was written.
            self.abs_box(h, x, y);
            return 0;
        }
        // ★ Painting order. The profile admits `position`, `z-index` and
        // `isolation` and nothing else that stacks, so a box opens a
        // stacking context when it is positioned or isolated; `z-index: auto`
        // on a positioned box counts as z = 0, which paints it above the
        // in-flow content around it (CSS 2 §9.9.1 layer 6). The deviation
        // from CSS is that a z-index INSIDE such a box stays nested instead
        // of joining the outer context, and that is counted, not hidden.
        // z-index applies to a positioned box and to a flex or grid item
        // (CSS Position 4 §6); on anything else it is ignored, as in CSS.
        let z_applies = positioned || self.dom.get(h).and_then(|n| n.parent).and_then(|p| self.st(p))
            .is_some_and(|ps| matches!(kw(ps, "display"), "flex" | "grid"));
        let z = match s.get("z-index") { V::Int(i) if z_applies => Some(*i), _ => None };
        let isolated = kw(s, "isolation") == "isolate";
        let opens_context = z.is_some() || isolated;
        let ctx_start = self.scene.order.len();
        let side = |p: &str| len(s.get(p), cb_w);
        let vpct = |p: &str| -> Option<U> { match s.get(p) { V::Pct(_) => cb_h.and_then(|b| len(s.get(p), b)), v => len(v, cb_h.unwrap_or(0)) } };
        let (bt, br, bb, bl) = ["border-top", "border-right", "border-bottom", "border-left"].map(|b| {
            if kw(s, &format!("{b}-style")) == "none" { 0 } else { len(s.get(&format!("{b}-width")), cb_w).unwrap_or(0) }
        }).into();
        let (pt, pr, pb, pl) = (side("padding-top").unwrap_or(0), side("padding-right").unwrap_or(0),
                                side("padding-bottom").unwrap_or(0), side("padding-left").unwrap_or(0));
        let (mut ml, mr) = (side("margin-left"), side("margin-right"));
        let (mt, mb) = (side("margin-top").unwrap_or(0), side("margin-bottom").unwrap_or(0));
        // ★ border-box: `width` INCLUDES padding and border (§3.1).
        let frame = bl + br + pl + pr;
        let specified_w = match s.get("width") {
            V::Kw("min-content") => Some(self.intrinsic(h).0 + frame),
            V::Kw("max-content") => Some(self.intrinsic(h).1 + frame),
            v => len(v, cb_w),
        };
        let mut w = specified_w.unwrap_or_else(|| cb_w - ml.unwrap_or(0) - mr.unwrap_or(0));
        if let Some(mx) = len(s.get("max-width"), cb_w) { w = w.min(mx) }
        if let Some(mn) = len(s.get("min-width"), cb_w) { w = w.max(mn) }
        w = w.max(frame);
        if let Some(fw) = force.0 { w = fw.max(frame) }
        // Auto margins take what is left; both auto centres.
        let free = cb_w - w - ml.unwrap_or(0) - mr.unwrap_or(0);
        match (ml, mr) {
            (None, None) => ml = Some(free / 2),
            (None, Some(_)) => ml = Some(free),
            _ => {}
        }
        let (bx, by) = (x + ml.unwrap_or(0), y + mt);
        let (rel_dx, rel_dy) = if kw(s, "position") == "relative" {
            (len(s.get("left"), cb_w).or_else(|| len(s.get("right"), cb_w).map(|r| -r)).unwrap_or(0),
             vpct("top").or_else(|| vpct("bottom").map(|b| -b)).unwrap_or(0))
        } else { (0, 0) };
        let (bx, by) = (bx + rel_dx, by + rel_dy);
        // `abs_box` has already decided where this box goes.
        let (bx, by) = self.abs_origin.take().unwrap_or((bx, by));
        // A definite height is known BEFORE the children, so they can resolve
        // their own percentages against it.
        let clamp = |v: U| {
            let v = match vpct("max-height") { Some(mx) if !matches!(s.get("max-height"), V::Kw(_)) => v.min(mx), _ => v };
            match vpct("min-height") { Some(mn) if !matches!(s.get("min-height"), V::Kw(_)) => v.max(mn), _ => v }
        };
        let definite = force.1.or_else(|| match s.get("height") { V::Kw(_) => None, V::Pct(_) => cb_h.and_then(|b| len(s.get("height"), b)), v => len(v, 0) }.map(clamp))
            // aspect-ratio: with a definite width and an auto height, the
            // height follows the ratio (CSS Sizing 4).
            .or_else(|| match s.get("aspect-ratio") {
                V::Num(r) if *r > 0.0 => Some(clamp(u(w as f64 / PX as f64 / r))),
                _ => None,
            });
        let child_cb_h = definite.map(|d| (d - pt - pb - bt - bb).max(0));
        // ★ List markers belong to `li` (the profile admits list-style-* but not
        // display: list-item): typed by the inherited list-style-type,
        // numbered among its `li` siblings from `<ol start>`.
        if self.dom.tag(h) == Some("li") {
            let ty = kw(s, "list-style-type");
            let text = match ty {
                "disc" => Some("\u{2022}".to_string()),
                "circle" => Some("\u{25E6}".to_string()),
                "square" => Some("\u{25AA}".to_string()),
                "decimal" => {
                    let parent = self.dom.get(h).and_then(|n| n.parent);
                    let start: i64 = parent.and_then(|p| self.dom.attr(p, "start")).and_then(|v| v.trim().parse().ok()).unwrap_or(1);
                    let before = parent.map(|p| self.dom.element_children(p).into_iter().take_while(|c| *c != h)
                        .filter(|c| self.dom.tag(*c) == Some("li")).count()).unwrap_or(0) as i64;
                    Some(format!("{}.", start + before))
                }
                _ => None,
            };
            if let Some(t) = text {
                let (st, line) = self.font_style(s);
                self.marker = Some((t, st, line, kw(s, "list-style-position") == "outside"));
            }
        }
        let saved_cb = self.pos_cb;
        if positioned {
            // ★ The containing block of an absolutely positioned descendant
            // is this box's PADDING box, not its content box.
            self.pos_cb = (bx + bl, by + bt, (w - bl - br).max(0), definite.map(|d| (d - bt - bb).max(0)));
        }
        // Paint the background BEFORE the children: reserve its slot now.
        let bg_slot = self.scene.order.len();
        let content_x = bx + bl + pl;
        let content_w = (w - frame).max(0);
        // text-indent (inherited) applies to this container's first line.
        self.indent = len(s.get("text-indent"), content_w).filter(|v| *v != 0);
        let mut cy = by + bt + pt + std::mem::take(&mut self.content_dy);
        // Children: block-level children stack; runs of inline content
        // between them form anonymous block boxes of line boxes.
        if kw(s, "display") == "grid" {
            cy += self.grid(h, s, content_x, cy, content_w, child_cb_h);
        } else if kw(s, "display") == "table" {
            cy += self.table(h, s, content_x, cy, content_w);
        } else if kw(s, "display") == "flex" {
            cy += self.flex(h, s, content_x, cy, content_w, child_cb_h);
        } else {
            let mut inline: Vec<Item> = vec![];
            for c in self.dom.children_of(h) {
                if self.is_block_level(c) {
                    cy += self.lines(std::mem::take(&mut inline), content_x, cy, content_w, s);
                    cy += self.block(c, content_x, cy, content_w, child_cb_h, (None, None));
                } else {
                    self.collect_inline(c, &mut inline, s);
                }
            }
            cy += self.lines(inline, content_x, cy, content_w, s);
        }
        let content_h = cy - (by + bt + pt);
        let hgt = definite.unwrap_or_else(|| clamp(content_h + pt + pb + bt + bb));
        // Paint: background over the border box, then borders — unless
        // visibility: hidden, which keeps the box and paints none of it.
        let hidden = kw(s, "visibility") == "hidden";
        let mut paint = vec![];
        // ★ border-radius: four corners, then CSS's overlap clamp — if two
        // radii on a side exceed it, ALL radii scale by the same factor.
        let mut radii = [0 as U; 4];
        for (i, p) in ["border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius"].iter().enumerate() {
            radii[i] = len(s.get(p), w).unwrap_or(0).max(0);
        }
        if radii != [0; 4] {
            let pairs = [(radii[0] + radii[1], w), (radii[3] + radii[2], w), (radii[0] + radii[3], hgt), (radii[1] + radii[2], hgt)];
            let f = pairs.iter().filter(|(sum, _)| *sum > 0).map(|(sum, side)| (*side as f64 / *sum as f64).min(1.0)).fold(1.0f64, f64::min);
            if f < 1.0 { for r in &mut radii { *r = (*r as f64 * f) as U } }
        }
        let bg = color_of(s, "background-color");
        if bg & 0xff != 0 { paint.push(Rect { x: bx, y: by, w, h: hgt, rgba: bg, radii, ring: 0 }) }
        if !matches!(s.get("background-image"), V::Kw("none")) { self.count("background-image (not painted)") }
        // (side width, x, y, length, horizontal?, style, colour)
        for (t, sx, sy, length, horiz, style_p, color_p) in [
            (bt, bx, by, w, true, "border-top-style", "border-top-color"),
            (bb, bx, by + hgt - bb, w, true, "border-bottom-style", "border-bottom-color"),
            (bl, bx, by + bt, hgt - bt - bb, false, "border-left-style", "border-left-color"),
            (br, bx + w - br, by + bt, hgt - bt - bb, false, "border-right-style", "border-right-color"),
        ] {
            if t <= 0 || length <= 0 { continue }
            paint.extend(edge(t, sx, sy, length, horiz, kw(s, style_p), color_of(s, color_p)));
        }
        // A rounded box with a UNIFORM border is one ring node; a rounded box
        // with sides that differ cannot be, so its corners stay square and
        // that is counted rather than drawn wrong.
        if radii != [0; 4] && bt > 0 {
            let uniform = [br, bb, bl].iter().all(|x| *x == bt)
                && ["border-top-style", "border-right-style", "border-bottom-style", "border-left-style"].iter().all(|p| kw(s, p) == "solid")
                && ["border-top-color", "border-right-color", "border-bottom-color", "border-left-color"].windows(2).all(|w| color_of(s, w[0]) == color_of(s, w[1]));
            if uniform {
                paint.retain(|r| r.ring != 0 || r.radii != [0; 4]);
                paint.push(Rect { x: bx, y: by, w, h: hgt, rgba: color_of(s, "border-top-color"), radii, ring: bt });
            } else {
                self.count("border-radius with sides that differ (corners squared)");
            }
        }
        if hidden { paint.clear() }
        // Outline: outside the border box, never affecting layout, painted
        // after the content (CSS paints outlines last).
        let ow = len(s.get("outline-width"), 0).unwrap_or(0);
        let ostyle = kw(s, "outline-style");
        if !hidden && ow > 0 && ostyle != "none" {
            let oc = color_of(s, "outline-color");
            let (ox, oy, owid, ohgt) = (bx - ow, by - ow, w + 2 * ow, hgt + 2 * ow);
            for (sx, sy, length, horiz) in [(ox, oy, owid, true), (ox, oy + ohgt - ow, owid, true), (ox, oy + ow, ohgt - 2 * ow, false), (ox + owid - ow, oy + ow, ohgt - 2 * ow, false)] {
                for r in edge(ow, sx, sy, length, horiz, ostyle, oc) { self.scene.rect(r) }
            }
        }
        let painted = paint.len();
        for (i, r) in paint.into_iter().enumerate() {
            self.scene.order.insert(bg_slot + i, (0, self.scene.rects.len()));
            self.scene.rects.push(r);
        }
        // ★ Inserting this box's background at the slot reserved before the
        // children SHIFTS every entry after it, so the ranges the children
        // recorded no longer point at what they painted. Move them.
        if painted > 0 {
            for c in self.contexts.iter_mut().filter(|c| c.1 >= bg_slot) { c.1 += painted; c.2 += painted }
            for h in self.hoists.iter_mut().filter(|h| h.0 >= bg_slot) { h.0 += painted; h.1 += painted }
        }
        if positioned { self.pos_cb = saved_cb }
        if opens_context { self.contexts.push((z.unwrap_or(0), ctx_start, self.scene.order.len(), painted)) }
        else if positioned { self.hoists.push((ctx_start, self.scene.order.len())) }
        mt + hgt + mb
    }

    /// An `absolute` or `fixed` box: sized and placed against its containing
    /// block (the padding box of the nearest positioned ancestor, or the
    /// viewport), then laid out by `block()` at that origin. It is out of
    /// flow, so it returns nothing to the height of what contained it.
    ///
    /// `static_x`/`static_y` are where normal flow would have put it, which
    /// is what an `auto` offset resolves to (CSS 2 §10.3.7).
    fn abs_box(&mut self, h: Handle, static_x: U, static_y: U) {
        let Some(s) = self.st(h) else { return };
        let (cbx, cby, cbw, cbh) = if kw(s, "position") == "fixed" {
            (0, 0, self.viewport.0, self.viewport.1)
        } else { self.pos_cb };
        let h_off = |p: &str| len(s.get(p), cbw);
        let v_off = |p: &str| -> Option<U> { match s.get(p) { V::Pct(_) => cbh.and_then(|b| len(s.get(p), b)), v => len(v, 0) } };
        let (left, right) = (h_off("left"), h_off("right"));
        let (top, bottom) = (v_off("top"), v_off("bottom"));
        let (ml, mr) = (h_off("margin-left").unwrap_or(0), h_off("margin-right").unwrap_or(0));
        let (mt, mb) = (h_off("margin-top").unwrap_or(0), h_off("margin-bottom").unwrap_or(0));
        let (bt, br, bb, bl) = ["border-top", "border-right", "border-bottom", "border-left"].map(|b| {
            if kw(s, &format!("{b}-style")) == "none" { 0 } else { len(s.get(&format!("{b}-width")), cbw).unwrap_or(0) }
        }).into();
        let frame = bl + br + h_off("padding-left").unwrap_or(0) + h_off("padding-right").unwrap_or(0);
        let (mn, mx) = self.intrinsic(h);
        let mut w = match s.get("width") {
            V::Kw("min-content") => mn + frame,
            V::Kw("max-content") => mx + frame,
            V::Kw(_) => match (left, right) {
                // Both offsets definite: the width is what is left between them.
                (Some(l), Some(r)) => (cbw - l - r - ml - mr).max(frame),
                // Otherwise shrink-to-fit against what is available.
                _ => {
                    let avail = (cbw - left.or(right).unwrap_or(0) - ml - mr - frame).max(0);
                    frame + mx.min(avail).max(mn.min(avail))
                }
            },
            v => len(v, cbw).unwrap_or(0),
        };
        if let Some(m) = h_off("max-width") { w = w.min(m) }
        if let Some(m) = h_off("min-width") { w = w.max(m) }
        w = w.max(frame);
        // A definite height only when both offsets are given and `height` is
        // auto; otherwise the content decides and `block()` clamps it.
        let forced_h = match (top, bottom, s.get("height")) {
            (Some(t), Some(b), V::Kw(_)) => cbh.map(|ch| (ch - t - b - mt - mb).max(bt + bb)),
            _ => None,
        };
        if (bottom.is_some() && top.is_none() || right.is_some() && left.is_none()) && cbh.is_none() && bottom.is_some() {
            self.count("bottom against an indefinite containing block (treated as auto)");
        }
        let x = match (left, right) {
            (Some(l), _) => cbx + l + ml,
            (None, Some(r)) => cbx + cbw - r - mr - w,
            _ => static_x + ml,
        };
        let y = match (top, bottom) {
            (Some(t), _) => cby + t + mt,
            (None, Some(b)) => match cbh {
                // The box must be measured before it can be placed from the
                // bottom edge: `measure` rolls the trial layout back whole.
                Some(ch) => {
                    self.placing_abs = true;
                    let outer = self.measure(h, cbw, cbh, (Some(w), forced_h));
                    cby + ch - b - mb - (outer - mt - mb)
                }
                None => static_y + mt,
            },
            _ => static_y + mt,
        };
        self.abs_origin = Some((x, y));
        self.placing_abs = true;
        self.block(h, 0, 0, cbw, cbh, (Some(w), forced_h));
        self.abs_origin = None;
        self.placing_abs = false;
    }

    /// Lay `h` out somewhere the reader never sees, and return its outer
    /// height: every effect on the scene, links and counters is rolled back.
    /// First baseline of `h` laid out at its margin-box origin, measured from
    /// that origin; a box with no line boxes synthesizes one from the bottom
    /// of its border box (CSS Box Alignment).
    fn baseline(&mut self, h: Handle, cb_w: U, force: (Option<U>, Option<U>)) -> U {
        let saved = self.baseline_probe.replace(None);
        let outer = self.measure(h, cb_w, None, force);
        let found = self.baseline_probe.take().flatten();
        self.baseline_probe = saved;
        found.unwrap_or_else(|| {
            let cs = self.st(h).expect("styled");
            outer - len(cs.get("margin-bottom"), cb_w).unwrap_or(0)
        })
    }

    fn measure(&mut self, h: Handle, cb_w: U, cb_h: Option<U>, force: (Option<U>, Option<U>)) -> U {
        let (o, r, n, l) = (self.scene.order.len(), self.scene.rects.len(), self.scene.runs.len(), self.scene.links.len());
        let (links, counts, marker, report) = (self.links.len(), self.unimplemented.clone(), self.marker.clone(), self.report.clone());
        let (ctx, hoi) = (self.contexts.len(), self.hoists.len());
        let hgt = self.block(h, 0, 0, cb_w, cb_h, force);
        self.scene.order.truncate(o); self.scene.rects.truncate(r); self.scene.runs.truncate(n); self.scene.links.truncate(l);
        self.links.truncate(links); self.unimplemented = counts; self.marker = marker; self.report = report;
        self.contexts.truncate(ctx); self.hoists.truncate(hoi);
        hgt
    }

    /// Flex layout (profile §3.4; CSS Flexbox §9 without `order` or any
    /// `*-reverse` — the profile removes them so visual order is reading
    /// order). Lays the items out inside the container's content box at
    /// (x, y), `w` wide, and returns the content height used.
    ///
    /// Items are the element children; a run of loose text between them is
    /// not an item here and is counted. Margins of items are honoured as
    /// fixed lengths (`auto` margins are treated as 0 and counted).
    fn flex(&mut self, h: Handle, s: &Style, x: U, y: U, w: U, cb_h: Option<U>) -> U {
        let row = kw(s, "flex-direction") != "column";
        let wrap = kw(s, "flex-wrap") == "wrap";
        let main_gap = len(s.get(if row { "column-gap" } else { "row-gap" }), if row { w } else { cb_h.unwrap_or(0) }).unwrap_or(0);
        let cross_gap = len(s.get(if row { "row-gap" } else { "column-gap" }), if row { cb_h.unwrap_or(0) } else { w }).unwrap_or(0);
        let main_avail = if row { Some(w) } else { cb_h };
        struct It { h: Handle, base: U, hypo: U, grow: f64, shrink: f64, min: U, max: U, main: U, cross: U,
                    m_start: U, m_end: U, c_start: U, c_end: U, align: &'static str, cross_auto: bool }
        let mut items: Vec<It> = vec![];
        for c in self.dom.children_of(h) {
            match self.dom.get(c).map(|n| &n.kind) {
                Some(Kind::Text(t)) if !t.trim().is_empty() => { self.count("flex: loose text in a flex container (not an item)"); continue }
                Some(Kind::Element(_)) => {}
                _ => continue,
            }
            let Some(cs) = self.st(c) else { continue };
            if kw(cs, "display") == "none" { continue }
            // Out of flow: not an item, placed against its containing block.
            if matches!(kw(cs, "position"), "absolute" | "fixed") { self.abs_box(c, x, y); continue }
            let px = |p: &str, basis: U| len(cs.get(p), basis);
            let (mw, mh) = (["margin-left", "margin-right"], ["margin-top", "margin-bottom"]);
            let (mm, cm) = if row { (mw, mh) } else { (mh, mw) };
            for p in mw.iter().chain(mh.iter()) { if matches!(cs.get(p), V::Kw("auto")) { self.count("flex: auto margins (as 0)") } }
            let m = |p: &str| px(p, w).unwrap_or(0);
            let main_prop = if row { "width" } else { "height" };
            let basis_of = |this: &mut Self| -> U {
                match cs.get("flex-basis") {
                    V::Kw("content") => this.content_main(c, row, w),
                    V::Kw(_) => match (cs.get(main_prop), row) {
                        (V::Kw(_), _) => this.content_main(c, row, w),
                        (V::Pct(_), false) if cb_h.is_none() => this.content_main(c, row, w),
                        (v, true) => len(v, w).unwrap_or_else(|| this.content_main(c, row, w)),
                        (v, false) => len(v, cb_h.unwrap_or(0)).unwrap_or_else(|| this.content_main(c, row, w)),
                    },
                    v => len(v, main_avail.unwrap_or(0)).unwrap_or(0),
                }
            };
            let base = basis_of(self);
            let (minp, maxp) = if row { ("min-width", "max-width") } else { ("min-height", "max-height") };
            // ★ min-width/min-height `auto` on a flex item is CSS's AUTOMATIC
            // MINIMUM SIZE: min(specified size if definite, content's
            // min-content size) — an item never shrinks below its content.
            // (Treating auto as 0 let a row squeeze text out of its box.)
            let min = match cs.get(minp) {
                V::Kw("auto") => {
                    let content_min = if row {
                        let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { len(cs.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
                        self.intrinsic(c).0 + len(cs.get("padding-left"), w).unwrap_or(0) + len(cs.get("padding-right"), w).unwrap_or(0) + b("left") + b("right")
                    } else { self.content_main(c, false, w) };
                    let specified = if row { len(cs.get("width"), w) } else { match cs.get("height") { V::Pct(_) => None, v => len(v, 0) } };
                    specified.map(|sp| sp.min(content_min)).unwrap_or(content_min)
                }
                _ => px(minp, main_avail.unwrap_or(0)).unwrap_or(0),
            };
            let max = px(maxp, main_avail.unwrap_or(0)).unwrap_or(U::MAX);
            let hypo = base.clamp(min, max.max(min));
            let num = |p: &str| match cs.get(p) { V::Num(n) => *n, _ => 0.0 };
            let align = match kw(cs, "align-self") { "auto" | "" => kw(s, "align-items"), a => a };
            let cross_prop = if row { "height" } else { "width" };
            items.push(It { h: c, base, hypo, grow: num("flex-grow"), shrink: num("flex-shrink"), min, max,
                            main: hypo, cross: 0, m_start: m(mm[0]), m_end: m(mm[1]), c_start: m(cm[0]), c_end: m(cm[1]),
                            align: match align { "flex-start" | "start" => "start", "flex-end" | "end" => "end", "center" => "center",
                                                 "baseline" => "baseline", _ => "stretch" },
                            cross_auto: matches!(cs.get(cross_prop), V::Kw("auto")) });
        }
        // Lines.
        let limit = main_avail.unwrap_or(U::MAX);
        let mut lines: Vec<std::ops::Range<usize>> = vec![];
        let (mut start, mut used) = (0usize, 0 as U);
        for (i, it) in items.iter().enumerate() {
            let outer = it.hypo + it.m_start + it.m_end;
            let add = if i > start { main_gap + outer } else { outer };
            if wrap && i > start && used.saturating_add(add) > limit { lines.push(start..i); start = i; used = outer } else { used += add }
        }
        if start < items.len() || items.is_empty() { lines.push(start..items.len()) }
        // Resolve flexible lengths per line (one pass, then clamp).
        for r in &lines {
            let outer: U = items[r.clone()].iter().map(|i| i.hypo + i.m_start + i.m_end).sum::<U>() + main_gap * (r.len() as U).saturating_sub(1);
            let Some(avail) = main_avail else { continue };
            let free = avail - outer;
            if free > 0 {
                let g: f64 = items[r.clone()].iter().map(|i| i.grow).sum();
                if g > 0.0 { for i in &mut items[r.clone()] { i.main = (i.hypo + u(free as f64 / PX as f64 * i.grow / g.max(1.0))).clamp(i.min, i.max.max(i.min)) } }
            } else if free < 0 {
                let sb: f64 = items[r.clone()].iter().map(|i| i.shrink * i.base as f64).sum();
                if sb > 0.0 { for i in &mut items[r.clone()] { i.main = (i.hypo + u(free as f64 / PX as f64 * (i.shrink * i.base as f64) / sb)).clamp(i.min, i.max.max(i.min)) } }
            }
        }
        // Cross sizes: measured at the resolved main size.
        for i in &mut items {
            i.cross = if row { self.measure(i.h, i.main, None, (Some(i.main), None)) - i.c_start - i.c_end }
                      else { self.intrinsic_outer_w(i.h, w) };
        }
        let container_cross = if row { cb_h } else { Some(w) };
        let line_cross: Vec<U> = lines.iter().map(|r| {
            let m = items[r.clone()].iter().map(|i| i.cross + i.c_start + i.c_end).max().unwrap_or(0);
            // A single line in a container of definite cross size fills it.
            if lines.len() == 1 && !wrap { container_cross.unwrap_or(m) } else { m }
        }).collect();
        // align-content: distribute the lines in the cross axis.
        let total: U = line_cross.iter().sum::<U>() + cross_gap * (lines.len() as U).saturating_sub(1);
        let (mut cpos, cstep, cstretch) = match (container_cross, lines.len() > 1 || wrap) {
            (Some(cc), true) if cc > total => {
                let free = cc - total;
                match kw(s, "align-content") { "flex-end" => (free, 0, 0), "center" => (free / 2, 0, 0),
                    "stretch" => (0, 0, free / lines.len() as U), _ => (0, 0, 0) }
            }
            _ => (0, 0, 0),
        };
        // align-content: baseline on a flex container that is not itself in
        // a baseline-sharing group falls back to `start` (CSS Box Alignment):
        // laying it out as flex-start IS the specified behaviour.
        let jc = kw(s, "justify-content");
        for (li, r) in lines.iter().enumerate() {
            let lc = line_cross[li] + cstretch;
            let used: U = items[r.clone()].iter().map(|i| i.main + i.m_start + i.m_end).sum::<U>() + main_gap * (r.len() as U).saturating_sub(1);
            let free = main_avail.map(|a| a - used).unwrap_or(0).max(0);
            let n = r.len() as U;
            // Baseline alignment (row lines only — in a column the item's
            // baseline is not in the cross axis, and CSS falls back to start).
            let mut bl: Vec<(usize, U)> = vec![];
            if row {
                for idx in r.clone() {
                    if items[idx].align == "baseline" {
                        let (hh, main, cross) = (items[idx].h, items[idx].main, items[idx].cross);
                        let b = self.baseline(hh, main, (Some(main), Some(cross)));
                        bl.push((idx, b));
                    }
                }
            }
            let max_b = bl.iter().map(|x| x.1).max().unwrap_or(0);
            let (mut mpos, between) = match jc {
                "flex-end" => (free, 0), "center" => (free / 2, 0),
                "space-between" if n > 1 => (0, free / (n - 1)),
                "space-around" if n > 0 => (free / n / 2, free / n),
                "space-evenly" if n > 0 => (free / (n + 1), free / (n + 1)),
                _ => (0, 0),
            };
            for idx in r.clone() {
                let i = &items[idx];
                let slot = lc - i.c_start - i.c_end;
                let (cross, coff) = match i.align {
                    "stretch" if i.cross_auto => (slot, 0),
                    "end" => (i.cross, slot - i.cross),
                    "center" => (i.cross, (slot - i.cross) / 2),
                    "baseline" => (i.cross, bl.iter().find(|x| x.0 == idx).map(|x| max_b - x.1).unwrap_or(0)),
                    _ => (i.cross, 0),
                };
                let m0 = mpos + i.m_start;
                let c0 = cpos + i.c_start + coff;
                let (ix, iy, fw, fh) = if row { (x + m0, y + c0, i.main, cross) } else { (x + c0, y + m0, cross, i.main) };
                // The item's own margins are applied here, so it is placed
                // with them zeroed: lay out at the margin box's corner minus
                // the margin the block would add.
                let (ml, mt) = { let cs = self.st(i.h).expect("styled"); (len(cs.get("margin-left"), w).unwrap_or(0), len(cs.get("margin-top"), w).unwrap_or(0)) };
                self.block(i.h, ix - ml, iy - mt, fw + ml, None, (Some(fw), Some(fh)));
                mpos = m0 + i.main + i.m_end + main_gap + between;
            }
            cpos += lc + cross_gap + cstep;
        }
        let _ = cstep;
        // Content height: a row container is as tall as its lines stacked in
        // the cross axis; a column container as its longest line's main extent.
        if row { total.max(0) } else {
            lines.iter().map(|r| items[r.clone()].iter().map(|i| i.main + i.m_start + i.m_end).sum::<U>()
                + main_gap * (r.len() as U).saturating_sub(1)).max().unwrap_or(0)
        }
    }

    /// Grid layout (profile §3.5; CSS Grid without named lines, `subgrid` or
    /// dense packing — the profile excludes all three). Lays the items out in
    /// the container's content box at (x, y), `w` wide; returns the height.
    fn grid(&mut self, h: Handle, s: &Style, x: U, y: U, w: U, cb_h: Option<U>) -> U {
        use navigator_style::values::{GridLine, Track};
        let tracks_of = |v: &V| -> Vec<Track> { match v { V::Tracks(t) => t.clone(), _ => vec![] } };
        let cols_tpl = tracks_of(s.get("grid-template-columns"));
        let rows_tpl = tracks_of(s.get("grid-template-rows"));
        let auto_col = match s.get("grid-auto-columns") { V::Track(t) => t.clone(), _ => Track::Auto };
        let auto_row = match s.get("grid-auto-rows") { V::Track(t) => t.clone(), _ => Track::Auto };
        let flow_row = kw(s, "grid-auto-flow") != "column";
        let cgap = len(s.get("column-gap"), w).unwrap_or(0);
        let rgap = len(s.get("row-gap"), cb_h.unwrap_or(0)).unwrap_or(0);

        struct GItem { h: Handle, col: (usize, usize), row: (usize, usize), justify: String, align: String }
        let line = |v: &V| -> GridLine { match v { V::Line(l) => *l, _ => GridLine::Auto } };
        let mut raw = vec![];
        for c in self.dom.children_of(h) {
            match self.dom.get(c).map(|n| &n.kind) {
                Some(Kind::Text(t)) if !t.trim().is_empty() => { self.count("grid: loose text in a grid container (not an item)"); continue }
                Some(Kind::Element(_)) => {}
                _ => continue,
            }
            let Some(cs) = self.st(c) else { continue };
            if kw(cs, "display") == "none" { continue }
            // Out of flow: not an item, placed against its containing block.
            if matches!(kw(cs, "position"), "absolute" | "fixed") { self.abs_box(c, x, y); continue }
            let pick = |own: &str, parent: &str| -> String {
                match kw(cs, own) { "auto" | "" => match kw(s, parent) { "" => "stretch", a => a }, a => a }.to_string()
            };
            raw.push((c, line(cs.get("grid-column-start")), line(cs.get("grid-column-end")),
                      line(cs.get("grid-row-start")), line(cs.get("grid-row-end")),
                      pick("justify-self", "justify-items"), pick("align-self", "align-items")));
        }
        // Placement. A definite line is 1-based; `span n` sizes the area.
        let span_of = |a: GridLine, b: GridLine| -> (Option<usize>, usize) {
            match (a, b) {
                (GridLine::Line(s0), GridLine::Line(e)) if e > s0 => (Some((s0 - 1).max(0) as usize), (e - s0) as usize),
                (GridLine::Line(s0), GridLine::Span(n)) => (Some((s0 - 1).max(0) as usize), n.max(1) as usize),
                (GridLine::Line(s0), _) => (Some((s0 - 1).max(0) as usize), 1),
                (GridLine::Auto, GridLine::Line(e)) if e > 1 => (Some((e - 2).max(0) as usize), 1),
                (GridLine::Auto, GridLine::Span(n)) => (None, n.max(1) as usize),
                _ => (None, 1),
            }
        };
        let fixed_len = if flow_row { cols_tpl.len().max(1) } else { rows_tpl.len().max(1) };
        let mut occupied: Vec<Vec<bool>> = vec![];
        let mut items: Vec<GItem> = vec![];
        let (mut cursor_major, mut cursor_minor) = (0usize, 0usize);
        for (c, cs0, ce, rs, re, justify, align) in raw {
            let (cstart, cspan) = span_of(cs0, ce);
            let (rstart, rspan) = span_of(rs, re);
            // Major axis = the flow axis; minor = the fixed one.
            let (mut major, mut minor, mspan, nspan) = if flow_row {
                (rstart, cstart, rspan, cspan)
            } else {
                (cstart, rstart, cspan, rspan)
            };
            // Auto placement: the first free slot at or after the cursor. A
            // definite minor position (e.g. a column) still needs a free
            // MAJOR position searched for it — that is a whole row of items
            // landing on top of each other if it is skipped.
            if major.is_none() {
                let fixed_minor = minor;
                let (mut mj, mut mn) = (cursor_major, fixed_minor.unwrap_or(cursor_minor));
                loop {
                    if mn + nspan > fixed_len {
                        if fixed_minor.is_some() { mj += 1 } else { mj += 1; mn = 0 }
                        continue;
                    }
                    let free = (0..mspan).all(|a| (0..nspan).all(|b| !*occupied.get(mj + a).and_then(|r: &Vec<bool>| r.get(mn + b)).unwrap_or(&false)));
                    if free { break }
                    match fixed_minor { Some(_) => mj += 1, None => mn += 1 }
                }
                major = Some(mj); minor = Some(mn);
                cursor_major = mj; cursor_minor = mn + nspan;
            }
            let (mj, mn) = (major.unwrap_or(0), minor.unwrap_or(0));
            while occupied.len() < mj + mspan { occupied.push(vec![false; fixed_len]) }
            for a in 0..mspan { for b in 0..nspan {
                let row = &mut occupied[mj + a];
                while row.len() <= mn + b { row.push(false) }
                row[mn + b] = true;
            } }
            let (col, row) = if flow_row { ((mn, nspan), (mj, mspan)) } else { ((mj, mspan), (mn, nspan)) };
            items.push(GItem { h: c, col, row, justify, align });
        }
        // Track lists, extended with implicit tracks where items reach past.
        let need = |tpl: &[Track], auto: &Track, n: usize| -> Vec<Track> {
            let mut v = tpl.to_vec();
            while v.len() < n { v.push(auto.clone()) }
            if v.is_empty() { v.push(auto.clone()) }
            v
        };
        let ncols = items.iter().map(|i| i.col.0 + i.col.1).max().unwrap_or(0).max(cols_tpl.len());
        let nrows = items.iter().map(|i| i.row.0 + i.row.1).max().unwrap_or(0).max(rows_tpl.len());
        let coltracks = need(&cols_tpl, &auto_col, ncols);
        let rowtracks = need(&rows_tpl, &auto_row, nrows);
        // Column sizes: intrinsic contributions come from single-track items.
        let mut cmin = vec![0 as U; coltracks.len()];
        let mut cmax = vec![0 as U; coltracks.len()];
        for it in &items {
            if it.col.1 != 1 { continue }
            let (mn, mx) = self.intrinsic(it.h);
            let cs = self.st(it.h).expect("styled");
            let frame = len(cs.get("padding-left"), w).unwrap_or(0) + len(cs.get("padding-right"), w).unwrap_or(0);
            cmin[it.col.0] = cmin[it.col.0].max(mn + frame);
            cmax[it.col.0] = cmax[it.col.0].max(mx + frame);
        }
        let cols = size_tracks(&coltracks, Some(w), cgap, &cmin, &cmax);
        // Row sizes: content heights measured at the item's column width.
        let mut rmin = vec![0 as U; rowtracks.len()];
        for it in &items {
            if it.row.1 != 1 { continue }
            let cw: U = (it.col.0..it.col.0 + it.col.1).map(|i| cols.get(i).copied().unwrap_or(0)).sum::<U>() + cgap * (it.col.1 as U - 1);
            let hgt = self.measure(it.h, cw, None, (Some(cw), None));
            rmin[it.row.0] = rmin[it.row.0].max(hgt);
        }
        let rows = size_tracks(&rowtracks, cb_h, rgap, &rmin, &rmin);
        // Place.
        let pos = |sizes: &[U], gap: U, i: usize| -> U { sizes[..i.min(sizes.len())].iter().sum::<U>() + gap * i as U };
        for it in &items {
            let ax = x + pos(&cols, cgap, it.col.0);
            let ay = y + pos(&rows, rgap, it.row.0);
            let aw = (it.col.0..it.col.0 + it.col.1).map(|i| cols.get(i).copied().unwrap_or(0)).sum::<U>() + cgap * (it.col.1 as U - 1);
            let ah = (it.row.0..it.row.0 + it.row.1).map(|i| rows.get(i).copied().unwrap_or(0)).sum::<U>() + rgap * (it.row.1 as U - 1);
            // A definite specified size wins over the intrinsic one: an empty
            // box with `width: 40px` is 40 wide, not 0.
            let specified_w = { let cs = self.st(it.h).expect("styled"); len(cs.get("width"), aw) };
            let iw = match it.justify.as_str() {
                "stretch" => specified_w.unwrap_or(aw),
                _ => specified_w.unwrap_or_else(|| self.intrinsic_outer_w(it.h, aw)).min(aw),
            };
            let specified_h = { let cs = self.st(it.h).expect("styled"); match cs.get("height") { V::Kw(_) => None, V::Pct(_) => len(cs.get("height"), ah), v => len(v, 0) } };
            let ih = match it.align.as_str() {
                "stretch" => specified_h.unwrap_or(ah),
                _ => specified_h.unwrap_or_else(|| self.measure(it.h, iw, None, (Some(iw), None))).min(ah),
            };
            let dx = match it.justify.as_str() { "end" => aw - iw, "center" => (aw - iw) / 2, _ => 0 };
            let dy = match it.align.as_str() { "end" => ah - ih, "center" => (ah - ih) / 2, _ => 0 };
            let forced_h = (it.align == "stretch" || specified_h.is_some()).then_some(ih);
            self.block(it.h, ax + dx, ay + dy, iw, Some(ah), (Some(iw), forced_h));
        }
        rows.iter().sum::<U>() + rgap * (rows.len() as U).saturating_sub(1)
    }

    /// Table layout, separate borders (profile §3.9: `border-collapse` is
    /// absent, borders are always separate).
    ///
    /// ★ COLUMN WIDTHS ARE DECLARED, NEVER MEASURED (§3.13). The first row's
    /// cells give the columns; a table whose first row has an `auto` width is
    /// REFUSED with a diagnostic and not laid out. There is deliberately no
    /// fallback measurement path — that is how the unbounded algorithm creeps
    /// back in.
    ///
    /// The parser inserts `<tbody>`, and the profile's `display` has no
    /// `table-row-group`, so elements between the table and its rows are
    /// flattened (decision).
    fn table(&mut self, h: Handle, s: &Style, x: U, y: U, w: U) -> U {
        let (sx, sy) = match s.get("border-spacing") {
            V::Pair(a, b) => (len(a, w).unwrap_or(0), len(b, 0).unwrap_or(0)),
            _ => (0, 0),
        };
        // Rows, flattening any wrapper the parser inserted.
        let mut rows = vec![];
        let mut stack: Vec<Handle> = self.dom.element_children(h).into_iter().rev().collect();
        while let Some(c) = stack.pop() {
            match self.st(c).map(|cs| kw(cs, "display")) {
                Some("table-row") => rows.push(c),
                Some("none") => {}
                _ => for g in self.dom.element_children(c).into_iter().rev() { stack.push(g) },
            }
        }
        let cells_of = |this: &Self, r: Handle| -> Vec<Handle> {
            this.dom.element_children(r).into_iter()
                .filter(|c| this.st(*c).map(|cs| kw(cs, "display")) == Some("table-cell")).collect()
        };
        let Some(first) = rows.first().copied() else { return 0 };
        let mut cols: Vec<U> = vec![];
        for c in cells_of(self, first) {
            let cs = self.st(c).expect("styled");
            match len(cs.get("width"), w) {
                Some(cw) => cols.push(cw),
                None => {
                    self.diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "table.column-width-undeclared",
                        msg: "a table's first row must declare every column width (§3.13: content that would need unbounded measurement arrives measured); table refused".into() });
                    return 0;
                }
            }
        }
        if cols.is_empty() { return 0 }
        let mut cy = y + sy;
        for r in rows {
            let cells = cells_of(self, r);
            // Natural heights first: the row is as tall as its tallest cell.
            let nat: Vec<U> = cells.iter().enumerate()
                .map(|(i, c)| { let cw = *cols.get(i).unwrap_or(cols.last().expect("non-empty")); self.table_part = true; self.measure(*c, cw, None, (Some(cw), None)) })
                .collect();
            let row_h = nat.iter().copied().max().unwrap_or(0);
            // Baselines, for cells aligned on one (the row's shared baseline).
            let bases: Vec<Option<U>> = cells.iter().enumerate().map(|(i, c)| {
                let cs = self.st(*c).expect("styled");
                (kw(cs, "vertical-align") == "baseline").then(|| {
                    let cw = *cols.get(i).unwrap_or(cols.last().expect("non-empty"));
                    self.table_part = true;
                    self.baseline(*c, cw, (Some(cw), None))
                })
            }).collect();
            let max_b = bases.iter().flatten().copied().max().unwrap_or(0);
            let mut cx = x + sx;
            for (i, c) in cells.iter().enumerate() {
                let cw = *cols.get(i).unwrap_or(cols.last().expect("non-empty"));
                let cs = self.st(*c).expect("styled");
                // vertical-align positions the CONTENT inside the row's height;
                // the cell box itself fills the row (separate-borders model).
                self.content_dy = match kw(cs, "vertical-align") {
                    "middle" => (row_h - nat[i]) / 2,
                    "bottom" => row_h - nat[i],
                    "baseline" => max_b - bases[i].unwrap_or(0),
                    _ => 0,
                };
                self.table_part = true;
                self.block(*c, cx, cy, cw, Some(row_h), (Some(cw), Some(row_h)));
                self.content_dy = 0;
                cx += cw + sx;
            }
            cy += row_h + sy;
        }
        cy - y
    }

    /// An item's content size along the main axis (its max-content width in
    /// a row, its laid-out height in a column), as a border-box size.
    fn content_main(&mut self, h: Handle, row: bool, cb_w: U) -> U {
        if row { self.intrinsic_outer_w(h, cb_w) } else {
            let cs = self.st(h).expect("styled");
            let m = len(cs.get("margin-top"), cb_w).unwrap_or(0) + len(cs.get("margin-bottom"), cb_w).unwrap_or(0);
            self.measure(h, cb_w, None, (None, None)) - m
        }
    }

    /// Max-content border-box width of `h` (its width if definite).
    fn intrinsic_outer_w(&mut self, h: Handle, cb_w: U) -> U {
        let cs = self.st(h).expect("styled");
        if let Some(wd) = len(cs.get("width"), cb_w) { return wd }
        let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { len(cs.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
        let frame = len(cs.get("padding-left"), cb_w).unwrap_or(0) + len(cs.get("padding-right"), cb_w).unwrap_or(0) + b("left") + b("right");
        (self.intrinsic(h).1 + frame).min(cb_w.max(0))
    }

    /// (min-content, max-content) width of `h`'s CONTENT box: the widest
    /// unbreakable word, and the widest line when nothing wraps. Measured by
    /// shaping the same items layout would place — with the side effects of
    /// collecting them (link table, counters) rolled back.
    fn intrinsic(&mut self, h: Handle) -> (U, U) {
        let (links, counts) = (self.links.len(), self.unimplemented.clone());
        let Some(s) = self.st(h) else { return (0, 0) };
        let (mut mn, mut mx) = (0, 0);
        let mut items = vec![];
        for c in self.dom.children_of(h) {
            if self.is_block_level(c) {
                let Some(cs) = self.st(c) else { continue };
                let px = |p: &str| len(cs.get(p), 0).unwrap_or(0);
                let frame = px("padding-left") + px("padding-right") + px("margin-left") + px("margin-right")
                    + if kw(cs, "border-left-style") == "none" { 0 } else { px("border-left-width") }
                    + if kw(cs, "border-right-style") == "none" { 0 } else { px("border-right-width") };
                let (a, b) = match cs.get("width") { V::Len(l) => { let w = u(l.v); (w, w) } _ => self.intrinsic(c) };
                mn = mn.max(a + frame);
                mx = mx.max(b + frame);
            } else {
                self.collect_inline(c, &mut items, s);
            }
        }
        let mut line = 0;
        for it in &items {
            match it {
                Item::Break => { mx = mx.max(line); line = 0 }
                Item::Word(t, st, _) | Item::PreSpace(t, st, _) => {
                    let w: U = self.sh.shape(t, st, &mut self.report).iter().map(|p| p.width).sum();
                    if matches!(it, Item::Word(..)) { mn = mn.max(w) }
                    line += w;
                }
                Item::Space(st, _) => line += self.sh.shape(" ", st, &mut self.report).iter().map(|p| p.width).sum::<U>(),
                Item::Tab(st, l) => {
                    let sp: U = self.sh.shape(" ", st, &mut self.report).iter().map(|p| p.width).sum();
                    let stop = (sp * l.tab as U).max(1);
                    line = (line / stop + 1) * stop;
                }
            }
        }
        mx = mx.max(line).max(mn);
        self.links.truncate(links);
        self.unimplemented = counts;
        (mn, mx)
    }

    fn font_style(&mut self, s: &Style) -> (FontStyle, Line) {
        let family = match s.get("font-family") {
            V::Family { web, stack } => {
                if !web.is_empty() { self.count("web font (shipped stack used)") }
                if *stack == "mono" { Family::Mono } else { Family::Sans }
            }
            _ => Family::Sans,
        };
        let weight = match s.get("font-weight") { V::Int(w) => *w, _ => 400 };
        if weight != 400 && weight != 700 { self.count("font-weight not shipped (nearest used)") }
        let em = kw(s, "font-style") == "italic";
        if em { self.count("font-style: italic (no italic face shipped; drawn upright)") }
        let size = u(s.font_size_px);
        let lh = match s.get("line-height") {
            V::Num(n) => u(s.font_size_px * n),
            V::Len(l) => u(l.v),
            V::Pct(p) => u(s.font_size_px * p / 100.0),
            _ => size * 3 / 2,
        };
        let color = color_of(s, "color");
        let deco = color_of(s, "text-decoration-color");
        let line = Line { lh, underline: (kw(s, "text-decoration-line") == "underline").then_some(deco),
                          strike: (kw(s, "text-decoration-line") == "line-through").then_some(deco),
                          hidden: kw(s, "visibility") == "hidden",
                          break_word: kw(s, "overflow-wrap") == "break-word",
                          tab: match s.get("tab-size") { V::Int(n) => (*n).clamp(0, 64), _ => 8 } };
        let sp = |p: &str| match s.get(p) { V::Len(l) => u(l.v), V::Calc(c) => calc_eval(c, 0).map(u).unwrap_or(0), _ => 0 };
        (FontStyle { family, bold: weight >= 600, em, size, rgba: color, link: None,
                     ls: sp("letter-spacing"), ws: sp("word-spacing"), tnum: kw(s, "font-variant-numeric") == "tabular-nums" }, line)
    }

    /// Turn an inline-level node (and its inline descendants) into items.
    fn collect_inline(&mut self, h: Handle, out: &mut Vec<Item>, parent: &Style) {
        match self.dom.get(h).map(|n| &n.kind) {
            Some(Kind::Text(t)) => {
                let t = t.clone();
                self.text_items(&t, parent, None, out);
            }
            Some(Kind::Element(tag)) => {
                let tag = tag.clone();
                let Some(s) = self.st(h) else { return };
                match kw(s, "display") {
                    "none" => return,
                    "inline-block" => { self.count("inline-block (skipped)"); return }
                    _ => {}
                }
                if tag == "br" { out.push(Item::Break); return }
                let link = (tag == "a").then(|| self.dom.attr(h, "href")).flatten().map(|href| { self.links.push(href.to_string()); self.links.len() - 1 });
                let before = out.len();
                for c in self.dom.children_of(h) { self.collect_inline(c, out, s) }
                if let Some(li) = link {
                    for it in &mut out[before..] {
                        if let Item::Word(_, st, _) | Item::Space(st, _) = it { st.link = Some(li) }
                    }
                }
            }
            _ => {}
        }
    }

    fn text_items(&mut self, t: &str, s: &Style, _link: Option<usize>, out: &mut Vec<Item>) {
        let (st, line) = self.font_style(s);
        let t = match kw(s, "text-transform") {
            "uppercase" => t.to_uppercase(),
            "lowercase" => t.to_lowercase(),
            "capitalize" => t.split_inclusive(char::is_whitespace).map(|w| {
                let mut c = w.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() }).collect(),
            _ => t.to_string(),
        };
        match kw(s, "white-space") {
            "pre" => {
                for (i, l) in t.split('\n').enumerate() {
                    if i > 0 { out.push(Item::Break) }
                    for (j, seg) in l.split('\t').enumerate() {
                        if j > 0 { out.push(Item::Tab(st.clone(), line)) }
                        if !seg.is_empty() { out.push(Item::Word(seg.to_string(), st.clone(), line)) }
                    }
                }
            }
            // pre-wrap: spaces and newlines preserved, but lines wrap — at the
            // boundaries between runs of spaces and runs of anything else.
            "pre-wrap" => {
                for (i, l) in t.split('\n').enumerate() {
                    if i > 0 { out.push(Item::Break) }
                    let mut run = String::new();
                    let mut in_space = false;
                    for ch in l.chars() {
                        let sp = ch == ' ';
                        if (sp != in_space || ch == '\t') && !run.is_empty() {
                            let r = std::mem::take(&mut run);
                            out.push(if in_space { Item::PreSpace(r, st.clone(), line) } else { Item::Word(r, st.clone(), line) });
                        }
                        if ch == '\t' { out.push(Item::Tab(st.clone(), line)); in_space = false; continue }
                        in_space = sp;
                        run.push(ch);
                    }
                    if !run.is_empty() { out.push(if in_space { Item::PreSpace(run, st.clone(), line) } else { Item::Word(run, st.clone(), line) }) }
                }
            }
            // ★ nowrap: whitespace collapses exactly as in `normal`, but it
            // is NOT a break opportunity — the whole run is one unbreakable
            // word. (It used to flush a word at every space, which made every
            // space breakable: a nowrap line wrapped. Found reviewing a golden.)
            "nowrap" => {
                let mut run = String::new();
                for ch in t.chars() {
                    if ch.is_whitespace() {
                        let at_start = run.is_empty() && matches!(out.last(), Some(Item::Space(..)) | Some(Item::Break) | None);
                        if !at_start && !run.ends_with(' ') { run.push(' ') }
                    } else { run.push(ch) }
                }
                if !run.is_empty() { out.push(Item::Word(run, st, line)) }
            }
            _ => {
                let mut word = String::new();
                for ch in t.chars() {
                    if ch.is_whitespace() {
                        if !word.is_empty() { out.push(Item::Word(std::mem::take(&mut word), st.clone(), line)) }
                        // Collapse: at most one space, never at a line start.
                        if !matches!(out.last(), Some(Item::Space(..)) | Some(Item::Break) | None) {
                            out.push(Item::Space(st.clone(), line))
                        }
                    } else { word.push(ch) }
                }
                if !word.is_empty() { out.push(Item::Word(word, st, line)) }
            }
        }
    }

    /// Greedy line breaking of `items` into line boxes; returns their height.
    fn lines(&mut self, mut items: Vec<Item>, x: U, y: U, w: U, block: &Style) -> U {
        if items.iter().all(|i| matches!(i, Item::Space(..))) { return 0 }
        // A pending list marker: `inside` is the first inline item; `outside`
        // hangs left of the first line box, on its baseline.
        let mut outside_marker = None;
        if let Some((text, st, line, outside)) = self.marker.take() {
            if outside { outside_marker = Some((text, st)) }
            else { items.insert(0, Item::Space(st.clone(), line)); items.insert(0, Item::Word(text, st, line)) }
        }
        struct Placed { x: U, st: FontStyle, line: Line, pieces: Vec<crate::Piece>, gap: bool }
        let mut lines: Vec<Vec<Placed>> = vec![vec![]];
        // Whether each line was ended by a forced break (never justified).
        let mut forced: Vec<bool> = vec![false];
        // text-indent: the first line of the block container starts in.
        let first_indent = self.indent.take().unwrap_or(0);
        let (mut cx, mut pending): (U, Option<(FontStyle, Line)>) = (first_indent, None);
        for it in items {
            match it {
                Item::Tab(st, l) => {
                    // Tab stops: multiples of tab-size × the space's advance,
                    // measured from the line's start (exact for any font).
                    let sp: U = self.sh.shape(" ", &st, &mut self.report).iter().map(|p| p.width).sum();
                    let stop = (sp * l.tab as U).max(1);
                    cx = (cx / stop + 1) * stop;
                    pending = None;
                }
                Item::Break => { *forced.last_mut().expect("line") = true; lines.push(vec![]); forced.push(false); cx = 0; pending = None }
                Item::Space(st, l) => pending = Some((st, l)),
                Item::PreSpace(text, st, l) => {
                    let pieces = self.sh.shape(&text, &st, &mut self.report);
                    let ww: U = pieces.iter().map(|p| p.width).sum();
                    // ★ Preserved spaces HANG at a line end (pre-wrap): they
                    // never push the line past the edge, and never wrap.
                    if cx + ww > w && !lines.last().expect("line").is_empty() { pending = None; continue }
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces, gap: false });
                    cx += ww;
                    pending = None;
                }
                Item::Word(text, st, l) => {
                    let pieces = self.sh.shape(&text, &st, &mut self.report);
                    let ww: U = pieces.iter().map(|p| p.width).sum();
                    let sw = match (&pending, lines.last().map(|l| l.is_empty())) {
                        (Some((ps, _)), Some(false)) => self.sh.shape(" ", ps, &mut self.report).iter().map(|p| p.width).sum(),
                        _ => 0,
                    };
                    let wrap = cx + sw + ww > w && !lines.last().expect("line").is_empty();
                    if wrap { lines.push(vec![]); forced.push(false); cx = 0 } else { cx += sw }
                    if ww > w && l.break_word {
                        // overflow-wrap: break-word — a word wider than a whole
                        // line breaks between characters, greedily.
                        let chars: Vec<char> = text.chars().collect();
                        let mut i = 0;
                        while i < chars.len() {
                            let mut j = i + 1;
                            while j < chars.len() {
                                let t: String = chars[i..j + 1].iter().collect();
                                let tw: U = self.sh.shape(&t, &st, &mut self.report).iter().map(|p| p.width).sum();
                                if cx + tw > w { break }
                                j += 1;
                            }
                            let chunk: String = chars[i..j].iter().collect();
                            let pieces = self.sh.shape(&chunk, &st, &mut self.report);
                            let cw: U = pieces.iter().map(|p| p.width).sum();
                            lines.last_mut().expect("line").push(Placed { x: cx, st: st.clone(), line: l, pieces, gap: false });
                            cx += cw;
                            i = j;
                            if i < chars.len() { lines.push(vec![]); forced.push(false); cx = 0 }
                        }
                        pending = None;
                        continue;
                    }
                    if ww > w { self.report.overflow_lines += 1 }
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces, gap: !wrap && sw > 0 });
                    cx += ww;
                    pending = None;
                }
            }
        }
        let align = kw(block, "text-align");
        let n_lines = lines.len();
        let mut yy = y;
        for (li, mut line) in lines.into_iter().enumerate() {
            if line.is_empty() {
                let (st, l) = self.font_style(block);
                let _ = st; yy += l.lh; continue
            }
            let lh = line.iter().map(|p| p.line.lh).max().unwrap_or(0);
            let base = line.iter().map(|p| { let (a, d) = self.sh.asc_desc(&p.st); (p.line.lh - a - d) / 2 + a }).max().unwrap_or(0);
            let last = line.last().expect("non-empty");
            let used = last.x + last.pieces.iter().map(|p| p.width).sum::<U>();
            // ★ justify: the free space goes into the collapsible spaces of
            // every line except the block's last and those ended by <br>.
            // Integer division; the remainder goes one unit at a time to the
            // first gaps — one stated rule, reproducible.
            if align == "justify" && li + 1 < n_lines && !forced[li] {
                let gaps = line.iter().filter(|p| p.gap).count() as U;
                let free = w - used;
                if gaps > 0 && free > 0 {
                    let (each, mut rem, mut add) = (free / gaps, free % gaps, 0);
                    for p in line.iter_mut() {
                        if p.gap { add += each + if rem > 0 { rem -= 1; 1 } else { 0 } }
                        p.x += add;
                    }
                }
            }
            let shift = match align { "center" => (w - used) / 2, "end" => w - used, _ => 0 }.max(0);
            if let Some(None) = self.baseline_probe { self.baseline_probe = Some(Some(yy + base)) }
            if let Some((text, st)) = outside_marker.take() {
                let pieces = self.sh.shape(&text, &st, &mut self.report);
                let mw: U = pieces.iter().map(|p| p.width).sum();
                let mut mx = x - mw - st.size / 2;
                for piece in pieces {
                    self.scene.run(Run { face: piece.face, size: st.size, rgba: st.rgba, x: mx, y: yy + base, em: st.em, glyphs: piece.glyphs, text: piece.text });
                    mx += piece.width;
                }
            }
            let mut link_span: Option<(usize, U, U)> = None;
            // Decorations span the gaps between words of one decorated run
            // (CSS draws one line under `<a>link with a region</a>`, not four).
            let mut deco: Vec<(u32, U, U, U, U)> = vec![]; // (colour, y, x0, x1, thickness)
            for p in &line {
                let mut px = x + shift + p.x;
                let start = px;
                // visibility: hidden — the space is taken (positions are
                // already fixed), nothing is painted, and hidden text is not
                // hit-testable, so it contributes no link region either.
                if p.line.hidden { continue }
                for piece in &p.pieces {
                    self.scene.run(Run { face: piece.face, size: p.st.size, rgba: p.st.rgba, x: px, y: yy + base, em: p.st.em,
                                         glyphs: piece.glyphs.clone(), text: piece.text.clone() });
                    if p.st.em { self.report.em_upright += 1 }
                    px += piece.width;
                }
                let thick = (p.st.size / 16).max(PX);
                for (c, dy) in [(p.line.underline, thick), (p.line.strike, -scale(p.st.size, 1, 3))] {
                    let Some(c) = c else { continue };
                    let yline = yy + base + dy;
                    match deco.iter_mut().find(|d| d.0 == c && d.1 == yline && d.4 == thick) {
                        Some(d) => d.3 = px,
                        None => deco.push((c, yline, start, px, thick)),
                    }
                }
                match (p.st.link, &mut link_span) {
                    (Some(i), Some((j, _, x1))) if *j == i => *x1 = px,
                    (Some(i), span) => {
                        if let Some((j, x0, x1)) = span.take() { self.scene.link(Link { x: x0, y: yy, w: x1 - x0, h: lh, href: self.links[j].clone() }) }
                        *span = Some((i, start, px));
                    }
                    (None, span) => if let Some((j, x0, x1)) = span.take() { self.scene.link(Link { x: x0, y: yy, w: x1 - x0, h: lh, href: self.links[j].clone() }) },
                }
            }
            if let Some((j, x0, x1)) = link_span.take() { self.scene.link(Link { x: x0, y: yy, w: x1 - x0, h: lh, href: self.links[j].clone() }) }
            for (c, yline, x0, x1, t) in deco { self.scene.rect(Rect { x: x0, y: yline, w: x1 - x0, h: t, rgba: c, radii: [0; 4], ring: 0 }) }
            yy += lh;
        }
        yy - y
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fontset::FontSet;

    fn render(html: &str) -> HtmlOut { render_html(html, &FontSet::load().unwrap(), &Env::default()) }
    fn rects(o: &HtmlOut) -> Vec<&Rect> { o.scene.order.iter().filter(|(k, _)| *k == 0).map(|(_, i)| &o.scene.rects[*i]).collect() }

    #[test]
    fn border_box_and_no_margin_collapse() {
        let o = render(r#"<html><style>
            html { margin-top: 0px }
            body { margin-top: 0px; margin-left: 0px; margin-right: 0px; margin-bottom: 0px }
            .a { width: 200px; height: 50px; padding-left: 20px; border-left-width: 5px; border-left-style: solid; background-color: #ff0000; margin-bottom: 30px }
            .b { height: 40px; background-color: #0000ff; margin-top: 20px }
            </style><body><div class="a"></div><div class="b"></div></body></html>"#);
        let r = rects(&o);
        let red = r.iter().find(|r| r.rgba == 0xff0000ff).expect("red box");
        assert_eq!((red.x, red.y, red.w, red.h), (0, 0, 200 * PX, 50 * PX), "border-box: width includes padding and border");
        let blue = r.iter().find(|r| r.rgba == 0x0000ffff).expect("blue box");
        assert_eq!(blue.y, (50 + 30 + 20) * PX, "margins add, never collapse (§3.1)");
        assert_eq!(blue.w, 800 * PX, "auto width fills the containing block");
    }

    #[test]
    fn auto_margins_centre_a_sized_box() {
        let o = render(r#"<style>body { margin-left: 0px; margin-right: 0px } .c { width: 400px; margin-left: auto; margin-right: auto; height: 10px; background-color: #00ff00 }</style><div class="c"></div>"#);
        let g = rects(&o).into_iter().find(|r| r.rgba == 0x00ff00ff).expect("box");
        assert_eq!(g.x, 200 * PX);
    }

    #[test]
    fn text_wraps_and_aligns() {
        let words = "word ".repeat(80);
        let o = render(&format!("<style>p {{ text-align: center; width: 300px }}</style><p>{words}</p>"));
        let runs: Vec<&Run> = o.scene.runs.iter().collect();
        assert!(runs.iter().map(|r| r.y).collect::<std::collections::BTreeSet<_>>().len() > 3, "several lines");
        assert!(runs.iter().all(|r| r.x > 8 * PX), "centred lines start right of the left edge");
    }

    #[test]
    fn nowrap_never_wraps_and_justify_fills_all_but_the_last_line() {
        let long = "never wraps even past the edge of a narrow box at all";
        let o = render(&format!("<style>p {{ width: 150px; white-space: nowrap }}</style><p>{long}</p>"));
        let ys: std::collections::BTreeSet<U> = o.scene.runs.iter().map(|r| r.y).collect();
        assert_eq!(ys.len(), 1, "nowrap stays on one line");
        let o = render(&format!("<style>p {{ width: 300px; text-align: justify }}</style><p>{}</p>", "word ".repeat(40)));
        let mut lines: BTreeMap<U, (U, U)> = BTreeMap::new();
        for r in &o.scene.runs {
            let right = r.x + r.glyphs.last().map(|g| g.1).unwrap_or(0);
            let e = lines.entry(r.y).or_insert((r.x, right)); e.0 = e.0.min(r.x); e.1 = e.1.max(right);
        }
        let rights: Vec<U> = lines.values().map(|v| v.1).collect();
        assert!(rights.len() > 2);
        let full = &rights[..rights.len() - 1];
        assert!(full.windows(2).all(|w| (w[0] - w[1]).abs() <= 2 * PX), "justified lines end together: {full:?}");
        assert!(rights.last().unwrap() < &full[0], "the last line is not justified");
    }

    fn boxes(o: &HtmlOut, rgba: u32) -> Vec<(U, U, U, U)> {
        rects(o).into_iter().filter(|r| r.rgba == rgba).map(|r| (r.x / PX, r.y / PX, r.w / PX, r.h / PX)).collect()
    }

    #[test]
    fn flex_row_grows_and_justifies() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px; margin-right: 0px }
            .f { display: flex; width: 600px; column-gap: 10px }
            .a { width: 100px; height: 20px; background-color: #ff0000 }
            .b { flex-grow: 1; height: 20px; background-color: #00ff00 }
            .c { width: 100px; height: 20px; background-color: #0000ff }</style>
            <div class="f"><div class="a"></div><div class="b"></div><div class="c"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 100, 20)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(110, 0, 380, 20)], "grows into the free space, gaps honoured");
        assert_eq!(boxes(&o, 0x0000ffff), vec![(500, 0, 100, 20)]);
    }

    #[test]
    fn flex_column_and_stretch() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; flex-direction: column; width: 300px; row-gap: 5px }
            .a { height: 30px; background-color: #ff0000 }
            .b { height: 20px; width: 100px; align-self: center; background-color: #00ff00 }</style>
            <div class="f"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 300, 30)], "stretch fills the cross axis");
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 35, 100, 20)], "centred on the cross axis, after the gap");
    }

    #[test]
    fn flex_wrap_breaks_lines_and_shrink_takes_back() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .w { display: flex; flex-wrap: wrap; width: 250px }
            .w > div { width: 100px; height: 10px; background-color: #ff0000 }
            .s { display: flex; width: 150px; margin-top: 10px }
            .s > div { width: 100px; flex-shrink: 1; height: 10px; background-color: #0000ff }</style>
            <div class="w"><div></div><div></div><div></div></div><div class="s"><div></div><div></div></div>"#);
        let red = boxes(&o, 0xff0000ff);
        assert_eq!(red, vec![(0, 0, 100, 10), (100, 0, 100, 10), (0, 10, 100, 10)], "third item wraps");
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 30, 75, 10), (75, 30, 75, 10)], "equal shrink of equal bases");
    }

    #[test]
    fn inline_styles_links_and_decoration() {
        let o = render(r#"<p>plain <strong>bold</strong> <a href="https://x.example/">link</a></p>"#);
        assert!(o.scene.runs.iter().any(|r| r.text.contains("bold") && o.scene.runs.iter().any(|q| q.face != r.face)), "bold switches face");
        assert_eq!(o.scene.links.len(), 1);
        assert_eq!(o.scene.links[0].href, "https://x.example/");
        assert!(rects(&o).iter().any(|r| r.rgba == 0x0969daff), "the UA underlines links");
    }

    #[test]
    fn an_underline_spans_the_spaces_of_its_run() {
        let o = render(r#"<p><a href="x">link with a region</a> plain</p>"#);
        let under: Vec<&Rect> = rects(&o).into_iter().filter(|r| r.rgba == 0x0969daff).collect();
        assert_eq!(under.len(), 1, "one continuous underline, not one per word");
        assert_eq!(under[0].w, o.scene.links[0].w, "it spans exactly the link's text");
    }

    #[test]
    fn unimplemented_layout_is_counted_not_faked() {
        let o = render(r#"<style>.f { display: inline-block }</style><span class="f">a</span>"#);
        assert!(o.unimplemented.contains_key("inline-block (skipped)"));
    }

    #[test]
    fn absolute_boxes_are_placed_against_their_containing_block() {
        // The containing block is the nearest POSITIONED ancestor's padding
        // box: 20px border + 10px padding in from the relative box at (0, 0).
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .r { position: relative; width: 300px; height: 200px; padding-left: 10px; padding-top: 10px;
                 border-left-width: 20px; border-top-width: 20px; border-left-style: solid; border-top-style: solid;
                 border-left-color: #000000; border-top-color: #000000 }
            .a { position: absolute; left: 5px; top: 5px; width: 40px; height: 10px; background-color: #ff0000 }
            .b { position: absolute; right: 0px; bottom: 0px; width: 40px; height: 10px; background-color: #00ff00 }
            .f { position: fixed; left: 0px; top: 0px; width: 40px; height: 10px; background-color: #0000ff }</style>
            <div class="r"><div class="a"></div><div class="b"></div><div class="f"></div></div>"#);
        // Padding box starts at (20, 20) and is 280 x 180.
        assert_eq!(boxes(&o, 0xff0000ff), vec![(25, 25, 40, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(260, 190, 40, 10)]);
        // `fixed` uses the viewport, not the positioned ancestor.
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 0, 40, 10)]);
    }

    #[test]
    fn an_absolute_box_takes_no_room_and_shrinks_to_fit() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            div { height: 10px } .a { background-color: #ff0000 } .b { background-color: #00ff00 }
            .p { position: absolute; background-color: #0000ff; height: 10px }</style>
            <div class="a"></div><div class="p">ab</div><div class="b"></div>"#);
        // The absolute box is out of flow: `b` sits directly under `a`.
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 10, 792, 10)]); // 800 less body's right margin
        // With no offsets it stays at its static position, shrink-to-fit
        // around its text rather than filling the containing block.
        let p = boxes(&o, 0x0000ffff);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].0, p[0].1), (0, 10));
        assert!(p[0].2 > 0 && p[0].2 < 100, "shrink-to-fit, not 800: {}", p[0].2);
    }

    /// The case that says whether `z-index: auto` was treated as a stacking
    /// context: a negative-z descendant of a positioned `z-index: auto` box
    /// belongs to the OUTER context, so it paints below the in-flow content —
    /// not above it, which is where nesting would put it.
    #[test]
    fn z_index_auto_does_not_trap_its_descendants() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .s { width: 200px; height: 20px; background-color: #00ff00 }
            .p { position: relative; z-index: auto; width: 200px; height: 20px; background-color: #0000ff }
            .c { position: absolute; left: 0px; top: 0px; width: 200px; height: 20px; z-index: -1; background-color: #ff0000 }</style>
            <div><div class="s"></div><div class="p"><div class="c"></div></div></div>"#);
        let order: Vec<u32> = o.scene.order.iter().filter(|(k, _)| *k == 0).map(|(_, i)| o.scene.rects[*i].rgba).collect();
        // red (z = -1, outer context) · green (in flow) · blue (layer 6).
        assert_eq!(order, vec![0xff0000ff, 0x00ff00ff, 0x0000ffff]);
    }

    /// `isolation: isolate` must be visibly different from `auto`: it makes
    /// a stacking context, so a negative-z child paints above the isolating
    /// box's own background instead of disappearing behind it.
    #[test]
    fn isolation_contains_a_negative_child() {
        let css = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .box { width: 100px; height: 20px; background-color: #00ff00 }
            .u { position: relative; z-index: -1; width: 100px; height: 20px; background-color: #ff0000 }</style>"#;
        let iso = render(&format!(r#"{css}<style>.box {{ isolation: isolate }}</style><div class="box"><div class="u"></div></div>"#));
        let auto = render(&format!(r#"{css}<style>.box {{ isolation: auto }}</style><div class="box"><div class="u"></div></div>"#));
        let order = |o: &HtmlOut| -> Vec<u32> { o.scene.order.iter().filter(|(k, _)| *k == 0).map(|(_, i)| o.scene.rects[*i].rgba).collect() };
        assert_eq!(order(&iso), vec![0x00ff00ff, 0xff0000ff], "isolate: the red child is inside, so it paints over");
        assert_eq!(order(&auto), vec![0xff0000ff, 0x00ff00ff], "auto: the red child joins the root context and paints under");
    }

    #[test]
    fn z_index_orders_painting() {
        // Tree order would paint red last; z-index puts it under both, and
        // the negative one under the in-flow content of the context.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .u { position: relative; z-index: -1; width: 10px; height: 10px; background-color: #00ffff }
            .a { position: absolute; left: 0px; top: 0px; width: 10px; height: 10px; z-index: 2; background-color: #ff0000 }
            .b { position: absolute; left: 0px; top: 0px; width: 10px; height: 10px; z-index: 5; background-color: #00ff00 }
            .c { position: absolute; left: 0px; top: 0px; width: 10px; height: 10px; z-index: 3; background-color: #0000ff }</style>
            <div><div class="u"></div><div class="a"></div><div class="b"></div><div class="c"></div></div>"#);
        let order: Vec<u32> = o.scene.order.iter().filter(|(k, _)| *k == 0).map(|(_, i)| o.scene.rects[*i].rgba).collect();
        assert_eq!(order, vec![0x00ffffff, 0xff0000ff, 0x0000ffff, 0x00ff00ff]);
    }

    /// The control for count_unread_rows: a property the layout never reads
    /// must show up, and its initial value must not.
    #[test]
    fn an_unread_property_is_counted_and_its_initial_value_is_not() {
        let o = render(r#"<style>.a { opacity: 0.5 } .b { opacity: 1 }</style><div class="a">x</div><div class="b">y</div>"#);
        assert_eq!(o.unimplemented.get("row opacity… (not implemented)"), Some(&1), "{:?}", o.unimplemented);
    }

    /// CSS automatic minimum size: a flex item does not shrink below its
    /// content's min-content width. Control: an EMPTY item in the same
    /// position does shrink — so the clamp is caused by the content.
    #[test]
    fn a_flex_item_does_not_shrink_below_its_content() {
        let css = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; width: 100px } .f > div { flex-basis: 100px; height: 10px }
            .a { background-color: #ff0000 } .b { background-color: #0000ff }</style>"#;
        let with = render(&format!(r#"{css}<div class="f"><div class="a">Unbreakableword</div><div class="b"></div></div>"#));
        let without = render(&format!(r#"{css}<div class="f"><div class="a"></div><div class="b"></div></div>"#));
        let aw = boxes(&with, 0xff0000ff)[0].2;
        let ew = boxes(&without, 0xff0000ff)[0].2;
        assert_eq!(ew, 50, "control: two empty items share the shrink equally");
        assert!(aw > 50, "the word keeps its item from shrinking to 50 (got {aw})");
    }

    #[test]
    fn grid_places_sizes_and_spans() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 620px; grid-template-columns: 100px 1fr 1fr; column-gap: 10px; row-gap: 5px }
            .g > div { height: 20px }
            .a { background-color: #ff0000 } .b { background-color: #00ff00 } .c { background-color: #0000ff }
            .d { background-color: #ffff00; grid-column-start: 2; grid-column-end: span 2 }</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="c"></div><div class="d"></div></div>"#);
        // 620 = 100 + 2×10 gaps + two fr tracks of 250 each.
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 100, 20)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(110, 0, 250, 20)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(370, 0, 250, 20)]);
        // Second row, spanning both fr columns and the gap between them.
        assert_eq!(boxes(&o, 0xffff00ff), vec![(110, 25, 510, 20)]);
    }

    #[test]
    fn grid_stretches_auto_tracks() {
        // `none` template: one implicit `auto` column, which must grow to the
        // container width (Grid §12.8) or the item is invisible.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 300px; grid-template-columns: none }
            .g > div { height: 10px; background-color: #ff0000 }</style>
            <div class="g"><div></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 300, 10)]);
        // Two auto tracks share it, minus the gap.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 310px; column-gap: 10px; grid-template-columns: auto auto }
            .g > div { height: 10px; background-color: #ff0000 }</style>
            <div class="g"><div></div><div></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 150, 10), (160, 0, 150, 10)]);
        // Control: an `fr` track takes the free space instead, and the fixed
        // track beside it keeps its own width.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 300px; grid-template-columns: 100px 1fr }
            .g > div { height: 10px; background-color: #ff0000 }</style>
            <div class="g"><div></div><div></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 100, 10), (100, 0, 200, 10)]);
    }

    #[test]
    fn grid_alignment_and_auto_rows() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 300px; grid-template-columns: 150px 150px; grid-auto-rows: 60px; align-items: center; justify-items: end }
            .g > div { width: 40px; height: 20px; background-color: #ff0000 }
            /* stretch applies only to an AUTO size (CSS Box Alignment); with a
               specified 40×20 it would behave as start. */
            .g > .s { align-self: stretch; justify-self: stretch; background-color: #00ff00; width: auto; height: auto }</style>
            <div class="g"><div></div><div class="s"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(110, 20, 40, 20)], "end + centre inside a 150×60 area");
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(150, 0, 150, 60)], "stretch fills its area");
    }

    #[test]
    fn a_table_uses_declared_columns_and_refuses_undeclared_ones() {
        let css = r#"<style>body { margin-left: 0px; margin-top: 0px }
            table { border-spacing: 10px 6px } td { padding-top: 0px; padding-right: 0px; padding-bottom: 0px; padding-left: 0px;
            background-color: #0000ff; height: 20px }</style>"#;
        let o = render(&format!(r#"{css}<table><tr><td style2="" class="a"></td><td class="b"></td></tr><tr><td></td><td></td></tr></table>
            <style>.a {{ width: 80px }} .b {{ width: 120px }}</style>"#));
        let b = boxes(&o, 0x0000ffff);
        assert_eq!(b, vec![(10, 6, 80, 20), (100, 6, 120, 20), (10, 32, 80, 20), (100, 32, 120, 20)],
                   "columns from the first row; spacing around and between cells");
        // No declared width in the first row: refused with a diagnostic.
        let bad = render(&format!(r#"{css}<table><tr><td></td></tr></table>"#));
        assert!(bad.diagnostics.iter().any(|d| d.code == "table.column-width-undeclared"));
        assert!(boxes(&bad, 0x0000ffff).is_empty(), "refused, not laid out");
    }

    #[test]
    fn an_inline_style_attribute_is_reported_not_ignored() {
        let o = render(r#"<p style="color: red">x</p>"#);
        assert!(o.diagnostics.iter().any(|d| d.code == "input.style-attribute"));
    }

    #[test]
    fn diagnostics_travel_with_the_render() {
        let o = render(r#"<style>nav a { color: red } p { margin: 0 }</style><p>x</p>"#);
        let codes: Vec<&str> = o.diagnostics.iter().map(|d| d.code).collect();
        assert!(codes.contains(&"selector.unadmitted") && codes.contains(&"property.shorthand"), "{codes:?}");
    }
}

/// One side of a border or outline, `t` thick: solid, or dashes 3t long with
/// 2t gaps, or round dots t across with t gaps — starting at the side's
/// origin and clipped at its end (one stated rule, no fitting).
fn edge(t: U, sx: U, sy: U, length: U, horiz: bool, style: &str, rgba: u32) -> Vec<Rect> {
    let seg = |at: U, n: U, radius: U| { let radii = [radius; 4];
        if horiz { Rect { x: sx + at, y: sy, w: n, h: t, rgba, radii, ring: 0 } }
        else { Rect { x: sx, y: sy + at, w: t, h: n, rgba, radii, ring: 0 } } };
    match style {
        "dashed" | "dotted" => {
            let (on, off, r) = if style == "dashed" { (3 * t, 2 * t, 0) } else { (t, t, t / 2) };
            let mut out = vec![];
            let mut at = 0;
            while at < length { out.push(seg(at, on.min(length - at), r)); at += on + off }
            out
        }
        _ => vec![seg(0, length, 0)],
    }
}

/// Size one axis of a grid: bases from the track kinds and the items'
/// intrinsic contributions, then `fr` shares whatever space is left.
/// Painting order, as CSS 2 §9.9.1 reduced to what the profile admits
/// (`position`, `z-index`, `isolation` — no floats, no opacity groups, no
/// blend modes).
///
/// A box paints its subtree contiguously, so both inputs are RANGES of
/// `order`:
/// - `real` — stacking contexts: a positioned box with a `z-index`, or an
///   `isolation: isolate` box. They nest, and a `z-index` is compared only
///   among the children of the same one.
/// - `hoist` — positioned boxes with `z-index: auto`. CSS does NOT make
///   these stacking contexts: they paint in layer 6 (above the in-flow
///   content around them) but their z-indexed descendants belong to the
///   enclosing context, NOT to them. Nesting them would trap those
///   descendants, which is why this is not simply a context with z = 0.
///
/// Every entry gets a key — the chain of enclosing context z values, then
/// 1 if a `hoist` range holds it — and the sort is stable, so ties keep
/// tree order.
fn restack(order: &[(u8, usize)], real: &[(i64, usize, usize, usize)], hoist: &[(usize, usize)]) -> Vec<(u8, usize)> {
    if real.is_empty() && hoist.is_empty() { return order.to_vec() }
    let key = |i: usize| -> (Vec<i64>, u8) {
        let mut chain: Vec<&(i64, usize, usize, usize)> = real.iter().filter(|c| i >= c.1 && i < c.2).collect();
        // Outer ranges first: they start earlier, and on a tie they end later.
        chain.sort_by_key(|c| (c.1, std::cmp::Reverse(c.2)));
        let innermost = chain.last().copied();
        let inner = innermost.map(|c| c.1).unwrap_or(0);
        let rank = hoist.iter().any(|h| i >= h.0 && i < h.1 && h.0 >= inner) as u8;
        let mut ks: Vec<i64> = chain.into_iter().map(|c| c.0).collect();
        // ★ The context root's OWN background and borders are layer 1: they
        // paint BEFORE its negative-z children, not with the content around
        // them. Without this an `isolation: isolate` box is indistinguishable
        // from `auto`, because the negative child hides under the background
        // either way.
        if innermost.is_some_and(|c| i < c.1 + c.3) { ks.push(i64::MIN) }
        (ks, rank)
    };
    let keys: Vec<(Vec<i64>, u8)> = (0..order.len()).map(key).collect();
    let mut idx: Vec<usize> = (0..order.len()).collect();
    idx.sort_by(|a, b| {
        let (ka, kb) = (&keys[*a], &keys[*b]);
        for (x, y) in ka.0.iter().zip(kb.0.iter()) { if x != y { return x.cmp(y) } }
        // One chain is a prefix of the other: the shorter is the parent's own
        // content, which paints after a negative child and before the rest.
        let ord = match ka.0.len().cmp(&kb.0.len()) {
            std::cmp::Ordering::Less => if kb.0[ka.0.len()] < 0 { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less },
            std::cmp::Ordering::Greater => if ka.0[kb.0.len()] < 0 { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater },
            std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
        };
        ord.then(ka.1.cmp(&kb.1)).then(a.cmp(b))
    });
    idx.into_iter().map(|i| order[i]).collect()
}

fn size_tracks(tracks: &[navigator_style::values::Track], avail: Option<U>, gap: U, mins: &[U], maxs: &[U]) -> Vec<U> {
    use navigator_style::values::Track;
    fn base(t: &Track, avail: Option<U>, mn: U, mx: U) -> U {
        match t {
            Track::Len(l) => u(l.v),
            Track::Pct(p) => avail.map(|a| u(a as f64 / PX as f64 * p / 100.0)).unwrap_or(0),
            Track::Fr(_) => 0,
            Track::MinContent => mn,
            Track::MaxContent | Track::Auto => mx,
            Track::MinMax(a, b) => base(a, avail, mn, mx).max(base(b, avail, mn, mx).min(avail.unwrap_or(U::MAX))),
        }
    }
    let mut sizes: Vec<U> = tracks.iter().enumerate()
        .map(|(i, t)| base(t, avail, mins.get(i).copied().unwrap_or(0), maxs.get(i).copied().unwrap_or(0))).collect();
    let fr: f64 = tracks.iter().map(|t| match t { Track::Fr(f) => *f, _ => 0.0 }).sum();
    if fr > 0.0 {
        if let Some(a) = avail {
            let used: U = sizes.iter().sum::<U>() + gap * (tracks.len() as U).saturating_sub(1);
            let free = (a - used).max(0);
            for (i, t) in tracks.iter().enumerate() {
                if let Track::Fr(f) = t { sizes[i] = u(free as f64 / PX as f64 * f / fr) }
            }
        }
    } else if let Some(a) = avail {
        // Grid §12.8 "Stretch auto Tracks". The profile admits no
        // content-distribution property for a grid container (§3.5), so the
        // distribution is always the initial `normal` and auto-max tracks
        // always share what is left over. Without this an implicit `auto`
        // column (`grid-template-columns: none`) is zero wide and every item
        // in it disappears.
        let is_auto = |t: &Track| match t { Track::Auto => true, Track::MinMax(_, b) => matches!(**b, Track::Auto), _ => false };
        let auto: Vec<usize> = tracks.iter().enumerate().filter(|(_, t)| is_auto(t)).map(|(i, _)| i).collect();
        if !auto.is_empty() {
            let used: U = sizes.iter().sum::<U>() + gap * (tracks.len() as U).saturating_sub(1);
            let free = (a - used).max(0);
            let share = free / auto.len() as U;
            for (n, &i) in auto.iter().enumerate() {
                sizes[i] += if n + 1 == auto.len() { free - share * (auto.len() as U - 1) } else { share };
            }
        }
    }
    sizes
}
