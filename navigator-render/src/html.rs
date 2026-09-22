//! M2.2 — profile HTML + CSS → NSG: block and inline layout, box paint.
//!
//! The profile's fixed rules (§3.1) are not options here: every box is
//! `border-box`, margins NEVER collapse, there are no floats. So a block's
//! outer height is margin-top + height + margin-bottom, always, and the
//! next block starts right after it.
//!
//! ★ WHAT IS NOT LAID OUT YET IS COUNTED, NOT FAKED. flex, grid and table
//! containers lay out as blocks and are counted; positioned boxes other than
//! `relative`, and inline-blocks, are skipped and counted. `unimplemented`
//! is the list of what the conformance number cannot yet claim.

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
pub const READ_ROWS: &[u8] = &[1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 17, 18, 19, 20, 21, 22, 23, 24, 25, 32, 33, 34, 35, 36, 37, 39, 41, 42, 43, 44, 49, 50, 59, 60];

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
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None, baseline_probe: None };
    cx.count_unread_rows();
    // The root element is the initial containing block's only child.
    let root = dom.element_children(dom.root()).into_iter().next();
    // The root's containing block is the viewport: definite in both axes.
    let h = match root { Some(r) => cx.block(r, 0, 0, u(env.width_px), Some(u(env.height_px)), (None, None)), None => 0 };
    cx.scene.height = h.max(u(env.height_px));
    HtmlOut { scene: cx.scene, report: cx.report, diagnostics, unimplemented: cx.unimplemented }
}

#[derive(Clone)]
enum Item { Word(String, FontStyle, Line), Space(FontStyle, Line), Break,
            /// Preserved spaces (pre-wrap): placed like a word, but they HANG
            /// at a line end instead of wrapping.
            PreSpace(String, FontStyle, Line) }

/// Per-item line metrics and decoration.
#[derive(Clone, Copy)]
struct Line { lh: U, underline: Option<u32>, strike: Option<u32> }

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
        match kw(s, "display") {
            "none" => return 0,
            "grid" => self.count("display: grid (laid out as block)"),
            "table" | "table-row" | "table-cell" => self.count("display: table* (laid out as block)"),
            _ => {}
        }
        match kw(s, "position") {
            "absolute" | "fixed" => { self.count("position: absolute/fixed (skipped)"); return 0 }
            _ => {}
        }
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
        // A definite height is known BEFORE the children, so they can resolve
        // their own percentages against it.
        let clamp = |v: U| {
            let v = match vpct("max-height") { Some(mx) if !matches!(s.get("max-height"), V::Kw(_)) => v.min(mx), _ => v };
            match vpct("min-height") { Some(mn) if !matches!(s.get("min-height"), V::Kw(_)) => v.max(mn), _ => v }
        };
        let definite = force.1.or_else(|| match s.get("height") { V::Kw(_) => None, V::Pct(_) => cb_h.and_then(|b| len(s.get("height"), b)), v => len(v, 0) }.map(clamp));
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
        // Paint the background BEFORE the children: reserve its slot now.
        let bg_slot = self.scene.order.len();
        let content_x = bx + bl + pl;
        let content_w = (w - frame).max(0);
        let mut cy = by + bt + pt;
        // Children: block-level children stack; runs of inline content
        // between them form anonymous block boxes of line boxes.
        if kw(s, "display") == "flex" {
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
        // Paint: background over the border box, then borders.
        let mut paint = vec![];
        let bg = color_of(s, "background-color");
        if bg & 0xff != 0 { paint.push(Rect { x: bx, y: by, w, h: hgt, rgba: bg, radius: 0 }) }
        if !matches!(s.get("background-image"), V::Kw("none")) { self.count("background-image (not painted)") }
        // (side width, x, y, length, horizontal?, style, colour)
        for (t, sx, sy, length, horiz, style_p, color_p) in [
            (bt, bx, by, w, true, "border-top-style", "border-top-color"),
            (bb, bx, by + hgt - bb, w, true, "border-bottom-style", "border-bottom-color"),
            (bl, bx, by + bt, hgt - bt - bb, false, "border-left-style", "border-left-color"),
            (br, bx + w - br, by + bt, hgt - bt - bb, false, "border-right-style", "border-right-color"),
        ] {
            if t <= 0 || length <= 0 { continue }
            let rgba = color_of(s, color_p);
            let seg = |at: U, n: U, radius: U| if horiz { Rect { x: sx + at, y: sy, w: n, h: t, rgba, radius } }
                                              else { Rect { x: sx, y: sy + at, w: t, h: n, rgba, radius } };
            match kw(s, style_p) {
                // Dashes 3t long with 2t gaps; dots t across (round: radius
                // t/2) with t gaps. Both start at the side's origin and are
                // clipped at its end — one stated rule, no fitting.
                style @ ("dashed" | "dotted") => {
                    let (on, off, r) = if style == "dashed" { (3 * t, 2 * t, 0) } else { (t, t, t / 2) };
                    let mut at = 0;
                    while at < length { paint.push(seg(at, on.min(length - at), r)); at += on + off }
                }
                _ => paint.push(seg(0, length, 0)),
            }
        }
        for (i, r) in paint.into_iter().enumerate() {
            self.scene.order.insert(bg_slot + i, (0, self.scene.rects.len()));
            self.scene.rects.push(r);
        }
        mt + hgt + mb
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
        let hgt = self.block(h, 0, 0, cb_w, cb_h, force);
        self.scene.order.truncate(o); self.scene.rects.truncate(r); self.scene.runs.truncate(n); self.scene.links.truncate(l);
        self.links.truncate(links); self.unimplemented = counts; self.marker = marker; self.report = report;
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
            if matches!(kw(cs, "position"), "absolute" | "fixed") { self.count("position: absolute/fixed (skipped)"); continue }
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
                          strike: (kw(s, "text-decoration-line") == "line-through").then_some(deco) };
        (FontStyle { family, bold: weight >= 600, em, size, rgba: color, link: None }, line)
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
                    if !l.is_empty() { out.push(Item::Word(l.replace('\t', "        "), st.clone(), line)) }
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
                        let sp = ch == ' ' || ch == '\t';
                        if sp != in_space && !run.is_empty() {
                            let r = std::mem::take(&mut run);
                            out.push(if in_space { Item::PreSpace(r, st.clone(), line) } else { Item::Word(r, st.clone(), line) });
                        }
                        in_space = sp;
                        if ch == '\t' { run.push_str("        ") } else { run.push(ch) }
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
        let (mut cx, mut pending): (U, Option<(FontStyle, Line)>) = (0, None);
        for it in items {
            match it {
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
            for (c, yline, x0, x1, t) in deco { self.scene.rect(Rect { x: x0, y: yline, w: x1 - x0, h: t, rgba: c, radius: 0 }) }
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
        let o = render(r#"<style>.f { display: grid } .p { position: absolute }</style><div class="f">a</div><div class="p">b</div>"#);
        assert!(o.unimplemented.contains_key("display: grid (laid out as block)"));
        assert!(o.unimplemented.contains_key("position: absolute/fixed (skipped)"));
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
