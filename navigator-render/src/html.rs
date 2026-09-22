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
    let styled = cascade(&dom, &sheets, env);
    diagnostics.extend(styled.diagnostics);
    let mut cx = Cx { dom: &dom, styles: &styled.styles, sh: Shaper::new(fonts), scene: Scene { width: u(env.width_px), ..Default::default() },
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new() };
    // The root element is the initial containing block's only child.
    let root = dom.element_children(dom.root()).into_iter().next();
    let h = match root { Some(r) => cx.block(r, 0, 0, u(env.width_px)), None => 0 };
    cx.scene.height = h.max(u(env.height_px));
    HtmlOut { scene: cx.scene, report: cx.report, diagnostics, unimplemented: cx.unimplemented }
}

#[derive(Clone)]
enum Item { Word(String, FontStyle, Line), Space(FontStyle, Line), Break }

/// Per-item line metrics and decoration.
#[derive(Clone, Copy)]
struct Line { lh: U, underline: Option<u32>, strike: Option<u32> }

impl<'a> Cx<'a> {
    fn st(&self, h: Handle) -> Option<&'a Style> { self.styles.get(h as usize).and_then(|s| s.as_ref()) }
    fn count(&mut self, what: &'static str) { *self.unimplemented.entry(what).or_default() += 1 }

    fn is_block_level(&self, h: Handle) -> bool {
        match self.st(h) { Some(s) => matches!(kw(s, "display"), "block" | "flex" | "grid" | "table" | "table-row" | "table-cell"), None => false }
    }

    /// Lay out a block-level box at (x, y) in a containing block `cb_w` wide.
    /// Returns its OUTER height (margins included, never collapsed).
    fn block(&mut self, h: Handle, x: U, y: U, cb_w: U) -> U {
        let Some(s) = self.st(h) else { return 0 };
        match kw(s, "display") {
            "none" => return 0,
            "flex" => self.count("display: flex (laid out as block)"),
            "grid" => self.count("display: grid (laid out as block)"),
            "table" | "table-row" | "table-cell" => self.count("display: table* (laid out as block)"),
            _ => {}
        }
        match kw(s, "position") {
            "absolute" | "fixed" => { self.count("position: absolute/fixed (skipped)"); return 0 }
            _ => {}
        }
        let side = |p: &str| len(s.get(p), cb_w);
        let (bt, br, bb, bl) = ["border-top", "border-right", "border-bottom", "border-left"].map(|b| {
            if kw(s, &format!("{b}-style")) == "none" { 0 } else { len(s.get(&format!("{b}-width")), cb_w).unwrap_or(0) }
        }).into();
        let (pt, pr, pb, pl) = (side("padding-top").unwrap_or(0), side("padding-right").unwrap_or(0),
                                side("padding-bottom").unwrap_or(0), side("padding-left").unwrap_or(0));
        let (mut ml, mut mr) = (side("margin-left"), side("margin-right"));
        let (mt, mb) = (side("margin-top").unwrap_or(0), side("margin-bottom").unwrap_or(0));
        // ★ border-box: `width` INCLUDES padding and border (§3.1).
        let specified_w = match s.get("width") {
            V::Kw("min-content") | V::Kw("max-content") => { self.count("width: min/max-content (treated as auto)"); None }
            v => len(v, cb_w),
        };
        let mut w = specified_w.unwrap_or_else(|| cb_w - ml.unwrap_or(0) - mr.unwrap_or(0));
        if let Some(mx) = len(s.get("max-width"), cb_w) { w = w.min(mx) }
        if let Some(mn) = len(s.get("min-width"), cb_w) { w = w.max(mn) }
        w = w.max(bl + br + pl + pr);
        // Auto margins take what is left; both auto centres.
        let free = cb_w - w - ml.unwrap_or(0) - mr.unwrap_or(0);
        match (ml, mr) {
            (None, None) => { ml = Some(free / 2); mr = Some(free - free / 2) }
            (None, Some(_)) => ml = Some(free),
            (Some(_), None) => mr = Some(free),
            _ => {}
        }
        let _ = mr;
        let (bx, by) = (x + ml.unwrap_or(0), y + mt);
        let (rel_dx, rel_dy) = if kw(s, "position") == "relative" {
            (len(s.get("left"), cb_w).or_else(|| len(s.get("right"), cb_w).map(|r| -r)).unwrap_or(0),
             len(s.get("top"), 0).or_else(|| len(s.get("bottom"), 0).map(|b| -b)).unwrap_or(0))
        } else { (0, 0) };
        let (bx, by) = (bx + rel_dx, by + rel_dy);
        // Paint the background BEFORE the children: reserve its slot now.
        let bg_slot = self.scene.order.len();
        let content_x = bx + bl + pl;
        let content_w = (w - bl - br - pl - pr).max(0);
        let mut cy = by + bt + pt;
        // Children: block-level children stack; runs of inline content
        // between them form anonymous block boxes of line boxes.
        let mut inline: Vec<Item> = vec![];
        for c in self.dom.children_of(h) {
            if self.is_block_level(c) {
                cy += self.lines(std::mem::take(&mut inline), content_x, cy, content_w, s);
                cy += self.block(c, content_x, cy, content_w);
            } else {
                self.collect_inline(c, &mut inline, s);
            }
        }
        cy += self.lines(inline, content_x, cy, content_w, s);
        let content_h = cy - (by + bt + pt);
        let auto_h = content_h + pt + pb + bt + bb;
        let mut hgt = match s.get("height") {
            V::Kw(_) => auto_h,
            // A percentage height needs a definite containing block height,
            // which block flow does not have (CSS resolves it to auto).
            V::Pct(_) => { self.count("height: % (no definite containing height; auto)"); auto_h }
            v => len(v, 0).unwrap_or(auto_h),
        };
        if let Some(mx) = len(s.get("max-height"), 0).filter(|_| !matches!(s.get("max-height"), V::Pct(_))) { hgt = hgt.min(mx) }
        if let Some(mn) = len(s.get("min-height"), 0).filter(|_| !matches!(s.get("min-height"), V::Pct(_))) { hgt = hgt.max(mn) }
        // Paint: background over the border box, then borders.
        let mut paint = vec![];
        let bg = color_of(s, "background-color");
        if bg & 0xff != 0 { paint.push(Rect { x: bx, y: by, w, h: hgt, rgba: bg }) }
        if !matches!(s.get("background-image"), V::Kw("none")) { self.count("background-image (not painted)") }
        for (side_w, rect, style_p, color_p) in [
            (bt, Rect { x: bx, y: by, w, h: bt, rgba: 0 }, "border-top-style", "border-top-color"),
            (bb, Rect { x: bx, y: by + hgt - bb, w, h: bb, rgba: 0 }, "border-bottom-style", "border-bottom-color"),
            (bl, Rect { x: bx, y: by + bt, w: bl, h: hgt - bt - bb, rgba: 0 }, "border-left-style", "border-left-color"),
            (br, Rect { x: bx + w - br, y: by + bt, w: br, h: hgt - bt - bb, rgba: 0 }, "border-right-style", "border-right-color"),
        ] {
            if side_w <= 0 { continue }
            if kw(s, style_p) != "solid" { self.count("border-style dashed/dotted (drawn solid)") }
            paint.push(Rect { rgba: color_of(s, color_p), ..rect });
        }
        for (i, r) in paint.into_iter().enumerate() {
            self.scene.order.insert(bg_slot + i, (0, self.scene.rects.len()));
            self.scene.rects.push(r);
        }
        mt + hgt + mb
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
            "pre" | "pre-wrap" => {
                if kw(s, "white-space") == "pre-wrap" { self.count("white-space: pre-wrap (not wrapped)") }
                for (i, l) in t.split('\n').enumerate() {
                    if i > 0 { out.push(Item::Break) }
                    if !l.is_empty() { out.push(Item::Word(l.replace('\t', "        "), st.clone(), line)) }
                }
            }
            ws => {
                let nowrap = ws == "nowrap";
                let mut word = String::new();
                for ch in t.chars() {
                    if ch.is_whitespace() {
                        if !word.is_empty() { out.push(Item::Word(std::mem::take(&mut word), st.clone(), line)) }
                        // Collapse: at most one space, never at a line start.
                        if !matches!(out.last(), Some(Item::Space(..)) | Some(Item::Break) | None) {
                            if nowrap { word.push(' ') } else { out.push(Item::Space(st.clone(), line)) }
                        }
                    } else { word.push(ch) }
                }
                if !word.is_empty() { out.push(Item::Word(word, st, line)) }
            }
        }
    }

    /// Greedy line breaking of `items` into line boxes; returns their height.
    fn lines(&mut self, items: Vec<Item>, x: U, y: U, w: U, block: &Style) -> U {
        if items.iter().all(|i| matches!(i, Item::Space(..))) { return 0 }
        struct Placed { x: U, st: FontStyle, line: Line, pieces: Vec<crate::Piece> }
        let mut lines: Vec<Vec<Placed>> = vec![vec![]];
        let (mut cx, mut pending): (U, Option<(FontStyle, Line)>) = (0, None);
        for it in items {
            match it {
                Item::Break => { lines.push(vec![]); cx = 0; pending = None }
                Item::Space(st, l) => pending = Some((st, l)),
                Item::Word(text, st, l) => {
                    let pieces = self.sh.shape(&text, &st, &mut self.report);
                    let ww: U = pieces.iter().map(|p| p.width).sum();
                    let sw = match (&pending, lines.last().map(|l| l.is_empty())) {
                        (Some((ps, _)), Some(false)) => self.sh.shape(" ", ps, &mut self.report).iter().map(|p| p.width).sum(),
                        _ => 0,
                    };
                    if cx + sw + ww > w && !lines.last().expect("line").is_empty() { lines.push(vec![]); cx = 0 } else { cx += sw }
                    if ww > w { self.report.overflow_lines += 1 }
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces });
                    cx += ww;
                    pending = None;
                }
            }
        }
        let align = kw(block, "text-align");
        if align == "justify" { self.count("text-align: justify (laid out as start)") }
        let mut yy = y;
        for line in lines {
            if line.is_empty() {
                let (st, l) = self.font_style(block);
                let _ = st; yy += l.lh; continue
            }
            let lh = line.iter().map(|p| p.line.lh).max().unwrap_or(0);
            let base = line.iter().map(|p| { let (a, d) = self.sh.asc_desc(&p.st); (p.line.lh - a - d) / 2 + a }).max().unwrap_or(0);
            let last = line.last().expect("non-empty");
            let used = last.x + last.pieces.iter().map(|p| p.width).sum::<U>();
            let shift = match align { "center" => (w - used) / 2, "end" => w - used, _ => 0 }.max(0);
            let mut link_span: Option<(usize, U, U)> = None;
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
                if let Some(c) = p.line.underline { self.scene.rect(Rect { x: start, y: yy + base + thick, w: px - start, h: thick, rgba: c }) }
                if let Some(c) = p.line.strike { self.scene.rect(Rect { x: start, y: yy + base - scale(p.st.size, 1, 3), w: px - start, h: thick, rgba: c }) }
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
    fn inline_styles_links_and_decoration() {
        let o = render(r#"<p>plain <strong>bold</strong> <a href="https://x.example/">link</a></p>"#);
        assert!(o.scene.runs.iter().any(|r| r.text.contains("bold") && o.scene.runs.iter().any(|q| q.face != r.face)), "bold switches face");
        assert_eq!(o.scene.links.len(), 1);
        assert_eq!(o.scene.links[0].href, "https://x.example/");
        assert!(rects(&o).iter().any(|r| r.rgba == 0x0969daff), "the UA underlines links");
    }

    #[test]
    fn unimplemented_layout_is_counted_not_faked() {
        let o = render(r#"<style>.f { display: flex } .p { position: absolute }</style><div class="f">a</div><div class="p">b</div>"#);
        assert!(o.unimplemented.contains_key("display: flex (laid out as block)"));
        assert!(o.unimplemented.contains_key("position: absolute/fixed (skipped)"));
    }

    #[test]
    fn diagnostics_travel_with_the_render() {
        let o = render(r#"<style>nav a { color: red } p { margin: 0 }</style><p>x</p>"#);
        let codes: Vec<&str> = o.diagnostics.iter().map(|d| d.code).collect();
        assert!(codes.contains(&"selector.unadmitted") && codes.contains(&"property.shorthand"), "{codes:?}");
    }
}
