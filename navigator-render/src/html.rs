//! M2.2 — profile HTML + CSS → NSG: block and inline layout, box paint.
//!
//! The profile's fixed rules (§3.1) are not options here: every box is
//! `border-box`, margins NEVER collapse, there are no floats. So a block's
//! outer height is margin-top + height + margin-bottom, always, and the
//! next block starts right after it.
//!
//! ★ WHAT IS NOT LAID OUT YET IS COUNTED, NOT FAKED. `unimplemented` is the
//! list of what the conformance number cannot yet claim — today: background
//! images, italic (no italic face ships), and the value-level gaps noted
//! where they occur. Every `display` value in the profile is laid out.

use crate::fontset::{Family, FontSet};
use crate::{scale, Grad, Link, Rect, Report, Run, Scene, Shadow, Shaper, Style as FontStyle, Subresources, Xform, PX, U, XF_ONE};
use navigator_dom::{Dom, Handle, Kind};
use navigator_style::cascade::{cascade, Env, Style};
use navigator_style::sheet::{parse_sheet, Diagnostic, Stylesheet};
use navigator_style::token::Pos;
use navigator_style::values::{Calc, Color, Rgba, Tf, V};
use std::collections::BTreeMap;

/// Rows (profile §3.3–§3.9) the layout READS. A non-initial value in any
/// other row is counted as unimplemented on every element that has one —
/// so a property the layout ignores can never be dropped silently (§5.1).
/// Value-level gaps inside a read row (flex as block, italic without an
/// italic face, …) are counted where they occur.
pub const READ_ROWS: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64];

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
        // Percentages are resolved here, so unlike the cascade's evaluator
        // every argument is answerable and the comparison is exact.
        Calc::Min(v) => v.iter().map(|x| calc_eval(x, basis)).collect::<Option<Vec<_>>>()?.into_iter().fold(f64::INFINITY, f64::min),
        Calc::Max(v) => v.iter().map(|x| calc_eval(x, basis)).collect::<Option<Vec<_>>>()?.into_iter().fold(f64::NEG_INFINITY, f64::max),
        Calc::Clamp(lo, val, hi) => {
            let (lo, hi) = (calc_eval(lo, basis)?, calc_eval(hi, basis)?);
            calc_eval(val, basis)?.clamp(lo, hi.max(lo))
        }
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
    /// Elements whose `display` is inline but which contain block-level
    /// content, computed once bottom-up (see `is_block_level`).
    promoted: Vec<bool>,
    /// ★ REVIEW TOOLING, off unless asked for: every block box, so a person
    /// can ask WHICH ELEMENT is where. The scene says what was painted; it
    /// cannot say which element painted it, and that is the question every
    /// layout investigation starts from.
    pub boxes: Option<Vec<(Handle, U, U, U, U)>>,
    subs: &'a Subresources,
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
    /// ★ Out-of-flow boxes met while gathering INLINE content. An absolutely
    /// positioned element is blockified (CSS Display 3 §2.7) and takes no
    /// room in the line, so it cannot be flattened into it — doing that
    /// poured Joel on Software's two `.screen-reader-text` labels, each
    /// `width: 1px; overflow: hidden`, across the page as visible text.
    /// They are placed after the line box that would have held them.
    pending_abs: Vec<Handle>,
    /// ★ The element whose background colour became the CANVAS's (CSS
    /// Backgrounds 3 §2.11.2): the root's, or — when the root has none —
    /// the body's. Its own box does not paint that colour again.
    canvas_src: Option<Handle>,
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
    render_html_with(html, fonts, env, &Subresources::new())
}

/// With the subresources the input supplies (profile §3.13): a document that
/// references one it does not declare gets a diagnostic, never a guess.
pub fn render_html_with(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources) -> HtmlOut {
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
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None, baseline_probe: None, indent: None, content_dy: 0, table_part: false, promoted: vec![], boxes: std::env::var_os("NSG_DUMP_BOXES").map(|_| vec![]),
                      viewport: (u(env.width_px), Some(u(env.height_px))), pos_cb: (0, 0, u(env.width_px), Some(u(env.height_px))),
                      subs, placing_abs: false, pending_abs: vec![], canvas_src: None, abs_origin: None, contexts: vec![], hoists: vec![], diagnostics: vec![] };
    cx.promoted = promote_block_in_inline(&dom, &styled.styles);
    cx.count_unread_rows();
    // The root element is the initial containing block's only child.
    let root = dom.element_children(dom.root()).into_iter().next();
    // ★ THE CANVAS. The root's background paints the whole canvas, not the
    // root box; when the root has none, the BODY's is used instead and the
    // body's own box paints nothing (CSS Backgrounds 3 §2.11.2). Without
    // this, example.com — `body { background: #eee; width: 60vw }` — came
    // out as a grey column on white instead of a grey page.
    let body = root.and_then(|r| dom.element_children(r).into_iter().find(|c| dom.tag(*c) == Some("body")));
    let bg_of = |h: Handle| styled.styles.get(h as usize).and_then(|s| s.as_ref()).map(|s| color_of(s, "background-color")).unwrap_or(0);
    cx.canvas_src = [root, body].into_iter().flatten().find(|h| bg_of(*h) & 0xff != 0);
    let canvas = cx.canvas_src.map(|src| {
        cx.scene.rect(Rect { x: 0, y: 0, w: u(env.width_px), h: 0, rgba: bg_of(src), radii: [0; 4], ring: 0 });
        cx.scene.rects.len() - 1
    });
    // The root's containing block is the viewport: definite in both axes.
    let h = match root { Some(r) => cx.block(r, 0, 0, u(env.width_px), Some(u(env.height_px)), (None, None)), None => 0 };
    // ★ The scene's extent is what a reader can scroll to, and that is the
    // CONTENT, not the root box. Wikipedia sets `html { height: 100% }`,
    // which makes the root box exactly one viewport tall while 14,000 nodes
    // sit below it — taking the root's height alone reported a 600 px
    // document and the review render showed only its first screen.
    let bottom = cx.scene.rects.iter().map(|r| r.y + r.h)
        .chain(cx.scene.runs.iter().map(|r| r.y))
        .chain(cx.scene.images.iter().map(|i| i.area.y + i.area.h))
        .chain(cx.scene.shadows.iter().map(|s| s.y + s.h))
        .chain(cx.scene.grads.iter().map(|g| g.area.y + g.area.h))
        .max().unwrap_or(0);
    cx.scene.height = h.max(u(env.height_px)).max(bottom);
    if let Some(i) = canvas { cx.scene.rects[i].h = cx.scene.height }
    cx.scene.order = restack(&cx.scene.order, &cx.contexts, &cx.hoists);
    cx.scene.resolve_xforms();
    diagnostics.extend(cx.diagnostics);
    if let Some(v) = &cx.boxes {
        for (h, x, y, w, ht) in v {
            let tag = dom.tag(*h).unwrap_or("?");
            let id = dom.attr(*h, "id").map(|i| format!("#{i}")).unwrap_or_default();
            let class = dom.attr(*h, "class").unwrap_or("");
            eprintln!("BOX {tag}{id} [{class}] x={} y={} w={} h={}", x / 64, y / 64, w / 64, ht / 64);
        }
    }
    HtmlOut { scene: cx.scene, report: cx.report, diagnostics, unimplemented: cx.unimplemented }
}

/// ★ §3.13's offline half: measure every table column's min-content and
/// max-content width and DECLARE them on the first row's cells, so the
/// renderer never has to. This lives here because the measurement must use
/// the same shaper and the same pinned font set the renderer uses — which is
/// also why the font set version belongs in the normalizer's cache key.
///
/// Returns the rewritten HTML and how many columns were measured.
pub fn premeasure_tables(html: &str, fonts: &FontSet, env: &Env) -> (String, usize) {
    let dom = navigator_dom::parse(html);
    let mut sheets: Vec<Stylesheet> = vec![];
    for h in dom.by_tag_anywhere("style") {
        let p = parse_sheet(&dom.text_content(h));
        if p.refused.is_none() { sheets.push(p.sheet) }
    }
    let styled = cascade(&dom, &sheets, env);
    let mut cx = Cx { dom: &dom, styles: &styled.styles, sh: Shaper::new(fonts), scene: Scene { width: u(env.width_px), ..Default::default() },
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None, baseline_probe: None,
                      indent: None, content_dy: 0, table_part: false, promoted: vec![], boxes: std::env::var_os("NSG_DUMP_BOXES").map(|_| vec![]), viewport: (u(env.width_px), Some(u(env.height_px))),
                      pos_cb: (0, 0, u(env.width_px), Some(u(env.height_px))), subs: &Subresources::new(),
                      placing_abs: false, pending_abs: vec![], canvas_src: None, abs_origin: None, contexts: vec![], hoists: vec![], diagnostics: vec![] };
    // (table, per-column (min, max), its first row's cells) for every table.
    let mut tables: Vec<(Handle, Vec<(U, U)>, Vec<Handle>)> = vec![];
    for t in 0..dom.nodes.len() as Handle {
        let Some(ts) = cx.st(t) else { continue };
        if kw(ts, "display") != "table" { continue }
        let sx = match ts.get("border-spacing") { V::Pair(a, _) => len(a, 0).unwrap_or(0), _ => 0 };
        // Rows, flattening any wrapper — the same walk the layout does.
        let mut rows = vec![];
        let mut stack: Vec<Handle> = dom.element_children(t).into_iter().rev().collect();
        while let Some(c) = stack.pop() {
            match cx.st(c).map(|cs| kw(cs, "display")) {
                Some("table-row") => rows.push(c),
                Some("none") => {}
                _ => for g in dom.element_children(c).into_iter().rev() { stack.push(g) },
            }
        }
        // Collected up front: the measuring loop needs `cx` mutably.
        let rows_cells: Vec<Vec<Handle>> = rows.iter().map(|r| dom.element_children(*r).into_iter()
            .filter(|c| cx.st(*c).map(|cs| kw(cs, "display")) == Some("table-cell")).collect()).collect();
        // ★ COLSPAN: a row has as many columns as its cells' spans add up
        // to, and the table as many as its widest row.
        let ncols = rows_cells.iter().map(|cells| cells.iter().map(|c| cx.colspan(*c)).sum::<usize>()).max().unwrap_or(0);
        if ncols == 0 { continue }
        let mut mins = vec![0 as U; ncols];
        let mut maxs = vec![0 as U; ncols];
        // Author `<col>` widths are floors, like a cell's (below).
        let author_cols = cx.column_decls(t).ok().unwrap_or_default().into_iter()
            .filter(|c| dom.tag(*c) == Some("col")).collect::<Vec<_>>();
        for (i, c) in author_cols.iter().enumerate().take(ncols) {
            if let Some(cw) = cx.st(*c).and_then(|cs| match cs.get("width") { V::Len(_) => len(cs.get("width"), 0), _ => None }) {
                mins[i] = mins[i].max(cw); maxs[i] = maxs[i].max(cw);
            }
        }
        // (first column, span, min, max) of every SPANNING cell, settled
        // after the single-column cells have set their columns.
        let mut spanning: Vec<(usize, usize, U, U)> = vec![];
        for cells in &rows_cells {
            let mut k = 0usize;
            for c in cells.iter().copied() {
                let n = cx.colspan(c).min(ncols.saturating_sub(k)).max(1);
                if k >= ncols { break }
                let (mn, mx) = cx.intrinsic(c);
                let pad = cx.st(c).map(|cs| {
                    let p = |n: &str| len(cs.get(n), 0).unwrap_or(0);
                    let b = |n: &str| if kw(cs, &format!("border-{n}-style")) == "none" { 0 } else { p(&format!("border-{n}-width")) };
                    p("padding-left") + p("padding-right") + b("left") + b("right")
                }).unwrap_or(0);
                // ★ A cell's own `width` is a FLOOR in automatic table layout
                // (CSS 2.1 §17.5.2.2), never a cap: the column is at least its
                // content's min-content. Taken as exact, the W3C specs'
                // `th { width: 3em }` held their property tables' header
                // column to 48 px and "Initial:" painted over its value.
                let floor = cx.st(c).and_then(|cs| match cs.get("width") { V::Len(_) => len(cs.get("width"), 0), _ => None }).unwrap_or(0);
                let (mn, mx) = ((mn + pad).max(floor), (mx + pad).max(floor));
                if n == 1 {
                    mins[k] = mins[k].max(mn);
                    maxs[k] = maxs[k].max(mx);
                } else {
                    spanning.push((k, n, mn, mx));
                }
                k += n;
            }
        }
        // ★ A spanning cell that needs more than its columns give (with the
        // spacing between them) spreads the shortfall over those columns
        // (CSS 2.1 §17.5.2.2), narrowest spans first so the wider ones see
        // the columns the narrower ones already grew.
        spanning.sort_by_key(|x| x.1);
        for (k, n, mn, mx) in spanning {
            let inner = sx * (n as U - 1);
            for (want, v) in [(mn, &mut mins), (mx, &mut maxs)] {
                let have: U = v[k..k + n].iter().sum::<U>() + inner;
                if want > have {
                    let short = want - have;
                    for (j, slot) in v[k..k + n].iter_mut().enumerate() {
                        *slot += short / n as U + if j == n - 1 { short % n as U } else { 0 };
                    }
                }
            }
        }
        for i in 0..ncols { maxs[i] = maxs[i].max(mins[i]) }
        tables.push((t, mins.into_iter().zip(maxs).collect(), rows_cells.first().cloned().unwrap_or_default()));
    }
    let n: usize = tables.iter().map(|(_, c, _)| c.len()).sum();
    if n == 0 { return (html.to_string(), 0) }
    // ★ Emitted as `<col>` DECLARATIONS, one per column, in a `<colgroup>`
    // that replaces any the author wrote: first-row cells cannot declare
    // the columns of a table whose first row spans. The measured values go
    // in a stylesheet placed AFTER the document's own (the profile has no
    // inline style attribute).
    let mut css = String::new();
    let mut dom2 = navigator_dom::parse(html);
    let mut k = 0usize;
    for (t, cols, first) in tables {
        // ★ `<col>` survives parsing only inside a real `<table>`: a CSS
        // table (a `div` or `ul` with `display: table`) loses it, and fell
        // back to undeclared columns — Wikipedia's portal box was refused.
        // Such a table declares on its first row's cells instead, which it
        // always can: a CSS table's cells never span.
        if dom2.tag(t) != Some("table") {
            for (c, (mn, mx)) in first.into_iter().zip(cols) {
                let existing = dom2.attr(c, "class").unwrap_or("").to_string();
                dom2.set_attr(c, "class", format!("{existing} tc{k}").trim());
                // `width: auto`: the author's width is already folded in as
                // a floor, and a first-row width would otherwise be EXACT.
                css.push_str(&format!(".tc{k} {{ width: auto; min-width: {}px; max-width: {}px }}\n", mn as f64 / PX as f64, mx as f64 / PX as f64));
                k += 1;
            }
            continue;
        }
        for c in dom2.element_children(t) {
            if matches!(dom2.tag(c), Some("col") | Some("colgroup")) {
                if let Some(p) = dom2.get_mut(t) { p.children.retain(|x| *x != c) }
            }
        }
        let group = dom2.create(Kind::Element("colgroup".into()));
        for (mn, mx) in cols {
            let col = dom2.create(Kind::Element("col".into()));
            dom2.set_attr(col, "class", &format!("tc{k}"));
            css.push_str(&format!(".tc{k} {{ min-width: {}px; max-width: {}px }}\n", mn as f64 / PX as f64, mx as f64 / PX as f64));
            dom2.append(group, col);
            k += 1;
        }
        // First, before any row: where HTML puts a colgroup.
        dom2.append(t, group);
        if let Some(p) = dom2.get_mut(t) { let g = p.children.pop().unwrap(); p.children.insert(0, g) }
    }
    let style = dom2.create(Kind::Element("style".into()));
    let text = dom2.create(Kind::Text(css));
    dom2.append(style, text);
    let head = dom2.by_tag_anywhere("head").into_iter().next()
        .or_else(|| dom2.by_tag_anywhere("body").into_iter().next())
        .unwrap_or(dom2.root());
    dom2.append(head, style);
    (dom2.serialize(), n)
}

/// An image's natural sizing (CSS Images 3 §5.1): each part optional.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Natural { w: Option<U>, h: Option<U>, ratio: Option<(U, U)> }

/// CSS 2.1 §10.4's table: min/max on a replaced element whose width AND
/// height are auto, resolved so the RATIO survives where it can.
fn constrain_ratio(w: U, h: U, (mnw, mxw): (U, U), (mnh, mxh): (U, U)) -> (U, U) {
    let mul = |a: U, b: U, c: U| (a as i128 * b as i128 / c.max(1) as i128) as U;
    match (w > mxw, w < mnw, h > mxh, h < mnh) {
        (true, _, true, _) => if mxw as i128 * h as i128 <= mxh as i128 * w as i128 { (mxw, mnh.max(mul(mxw, h, w))) } else { (mnw.max(mul(mxh, w, h)), mxh) },
        (_, true, _, true) => if mnw as i128 * h as i128 <= mnh as i128 * w as i128 { (mxw.min(mul(mnh, w, h)), mnh) } else { (mnw, mxh.min(mul(mnw, h, w))) },
        (_, true, true, _) => (mnw, mxh),
        (true, _, _, true) => (mxw, mnh),
        (true, ..) => (mxw, mnh.max(mul(mxw, h, w))),
        (_, true, ..) => (mnw, mxh.min(mul(mnw, h, w))),
        (_, _, true, _) => (mnw.max(mul(mxh, w, h)), mxh),
        (_, _, _, true) => (mxw.min(mul(mnh, w, h)), mnh),
        _ => (w, h),
    }
}

#[derive(Clone)]
enum Item { Word(String, FontStyle, Line), Space(FontStyle, Line), Break,
            /// A tab in preserved white space: advance to the next tab stop.
            Tab(FontStyle, Line),
            /// Preserved spaces (pre-wrap): placed like a word, but they HANG
            /// at a line end instead of wrapping.
            PreSpace(String, FontStyle, Line),
            /// An atomic inline: an `inline-block`, which wraps like one word
            /// and is laid out as a block once its place on the line is known.
            Atomic(Handle, FontStyle, Line) }

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
    fn count_unread_rows(&mut self) { self.count_rows_outside(READ_ROWS) }

    /// ★ Every row is read today, so nothing calls this with a gap in real
    /// use — which is exactly why it takes the read set as an argument: the
    /// control can still hand it one, and the mechanism that would catch the
    /// NEXT unread row stays tested instead of silently passing.
    fn count_rows_outside(&mut self, read: &[u8]) {
        use navigator_style::values::{grammar, parse_value, Specified, ROWS};
        let props = navigator_style::cascade::longhands();
        let unread: Vec<(usize, &'static str, V)> = props.iter().enumerate().filter_map(|(i, p)| {
            let row = ROWS.iter().find(|r| r.props.contains(p))?;
            if read.contains(&row.id) { return None }
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
        let own = match self.st(h) { Some(s) => matches!(kw(s, "display"), "block" | "flex" | "grid" | "table" | "table-row" | "table-cell"), None => false };
        // ★ BLOCK-IN-INLINE. An inline box holding block-level content
        // cannot stay inline: CSS breaks it into anonymous blocks, and
        // flattening it instead pours the whole subtree into one line box.
        // Hacker News wraps its entire page in `<center>`; an unknown or
        // inline wrapper around a table is the general case, and it turned
        // every story into one run of text.
        own || self.promoted.get(h as usize).copied().unwrap_or(false)
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
        // ★ A TEXT handle here is an anonymous block box: a run of loose text
        // inside a flex or grid container, which CSS wraps in an anonymous
        // item. It has no style of its own — it inherits, and since it paints
        // no background or border, using the container's style is exactly
        // right (CSS 2 §9.2.1.1: anonymous boxes inherit, nothing else).
        if matches!(self.dom.get(h).map(|n| &n.kind), Some(Kind::Text(_))) {
            let Some(ps) = self.dom.get(h).and_then(|n| n.parent).and_then(|p| self.st(p)) else { return 0 };
            let mut items = vec![];
            self.collect_inline(h, &mut items, ps);
            return self.lines(items, x, y, force.0.unwrap_or(cb_w), ps);
        }
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
        // A transformed box is a stacking context (CSS Transforms 1 §3).
        let transformed = !matches!(s.get("transform"), V::Kw(_));
        let opens_context = z.is_some() || isolated || transformed;
        let ctx_start = self.scene.order.len();
        let side = |p: &str| len(s.get(p), cb_w);
        let repl = self.replaced(h);
        let Some(s) = self.st(h) else { return 0 };
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
        }
        ;
        // ★ A replaced box is sized JOINTLY — width, height, natural size,
        // ratio and min/max together (CSS 2.1 §10.3.2, §10.4, §10.6.2; CSS
        // Images 3 §5). One computation, used for both axes below.
        let repl_used = repl.as_ref().map(|(_, nat)| Self::replaced_used(s, nat, cb_w, cb_h, frame, bt + bb + pt + pb));
        let specified_w = if repl.is_some() && !matches!(s.get("width"), V::Kw("min-content") | V::Kw("max-content")) {
            repl_used.map(|(w, _)| w)
        } else { specified_w };
        let mut w = specified_w.unwrap_or_else(|| cb_w - ml.unwrap_or(0) - mr.unwrap_or(0));
        if let Some(mx) = len(s.get("max-width"), cb_w) { w = w.min(mx) }
        if let Some(mn) = len(s.get("min-width"), cb_w) { w = w.max(mn) }
        w = w.max(frame);
        if let Some(fw) = force.0 { w = fw.max(frame) }
        // ★ CSS 2.1 §17.5.2: a table's used width is the GREATER of its
        // specified width and its MINIMUM content width. Clamping the
        // columns to a narrower specified width instead is what left a
        // 330 px image hanging 28 px outside a 310 px infobox.
        if kw(s, "display") == "table" {
            if let Some(min) = self.table_min_content(h, s, cb_w) { w = w.max(min + frame) }
        }
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
        let definite = force.1.or(repl_used.map(|(_, hh)| hh)).or_else(|| match s.get("height") { V::Kw(_) => None, V::Pct(_) => cb_h.and_then(|b| len(s.get("height"), b)), v => len(v, 0) }.map(clamp))
            // aspect-ratio: with a definite width and an auto height, the
            // height follows the ratio (CSS Sizing 4).
            .or_else(|| match s.get("aspect-ratio") {
                V::Num(r) if *r > 0.0 => Some(clamp(u(w as f64 / PX as f64 / r))),
                _ => None,
            })
;
        // ★ A border box is never smaller than its padding and border: the
        // content box cannot go negative (CSS Box Sizing 3 §3). Width had
        // this floor; height did not, so WPT's box-sizing-026 — `height:
        // 10px` with 50 px borders — painted its borders over each other.
        let vframe = pt + pb + bt + bb;
        let definite = definite.map(|d| d.max(vframe));
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
        // ★ overflow: anything but `visible` clips this box's CONTENT and its
        // descendants to its PADDING box (CSS Overflow 3). The box's own
        // border and background are NOT clipped, so the clip is dropped again
        // before they are painted. A definite height is needed for the
        // vertical edge; without one there is nothing to clip against yet.
        let saved_attrs = self.scene.cur;
        // ★ Per CSS Overflow 3 §3, `visible` computes to `auto` when the
        // OTHER axis is not visible — so one non-visible axis clips both, and
        // there is no such thing as clipping in x alone.
        let clips = kw(s, "overflow-x") != "visible" || kw(s, "overflow-y") != "visible";
        // ★ The ROOT's overflow belongs to the VIEWPORT (CSS Overflow 3 §3.3),
        // and so does the body's when the root's is `visible`: the page
        // scrolls, it is not cut. Clipping the root box instead cut Nature's
        // whole article — `html { height: 100%; overflow-y: scroll }` — to
        // one screen, and Apple's store page the same way.
        let root = self.dom.element_children(self.dom.root()).into_iter().next();
        let to_viewport = Some(h) == root || (self.dom.tag(h) == Some("body") && root.is_some_and(|r|
            self.dom.get(h).and_then(|n| n.parent) == Some(r)
            && self.st(r).is_some_and(|rs| kw(rs, "overflow-x") == "visible" && kw(rs, "overflow-y") == "visible")));
        let clips = clips && !to_viewport;
        if clips {
            // ★ A `max-height` clips too. It is not a definite height — the
            // box may end up shorter — but it is an upper bound the content
            // cannot be painted past, and a clipping box is exactly where
            // that matters. Ignoring it let Wikipedia's table of contents,
            // held in `max-height: calc(100vh - 48px); overflow-y: auto`,
            // paint its whole 1390 px over the article title below it.
            let bound = match vpct("max-height") {
                Some(mx) if !matches!(s.get("max-height"), V::Kw(_)) => Some(mx),
                _ => None,
            };
            let ch = definite.or(bound).map(|d| (d - bt - bb).max(0)).unwrap_or(U::MAX / 4);
            let id = self.scene.clip(bx + bl, by + bt, (w - bl - br).max(0), ch);
            self.scene.cur.0 = Some(id);
            if ["border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius"]
                .iter().any(|p| len(s.get(p), w).unwrap_or(0) > 0) { self.count("overflow clip on a rounded box (clipped square)") }
        }
        // ★ opacity < 1 makes a GROUP: the box and everything in it are
        // painted together and then composited once, which is why the alpha
        // cannot simply be multiplied into each node (overlapping children
        // would show through each other).
        let alpha = match s.get("opacity") { V::Num(o) => (o.clamp(0.0, 1.0) * 255.0).round() as u32, _ => 255 };
        if alpha < 255 {
            let id = self.scene.group(alpha);
            self.scene.cur.1 = Some(id);
        }
        // ★ transform is PAINT-LEVEL ONLY (profile §3.8), so it is an
        // attribute of the painted nodes and changes no geometry — and a
        // transformed ancestor does NOT become the containing block of a
        // `fixed` descendant (the profile removes that rule deliberately).
        //
        // The matrix needs the box's HEIGHT for a percentage origin, which is
        // not known until the children have been laid out. The declaration is
        // by index, so it is pushed now, referenced by the subtree, and
        // filled in at the end.
        let xf_id = match s.get("transform") {
            V::Transform(list) if !list.is_empty() => {
                let id = self.scene.xform([XF_ONE, 0, 0, XF_ONE, 0, 0]);
                self.scene.cur.2 = Some(id);
                Some((id, list.clone()))
            }
            _ => None,
        };
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
                    let ly = cy;
                    cy += self.lines(std::mem::take(&mut inline), content_x, cy, content_w, s);
                    self.place_pending_abs(content_x, ly);
                    cy += self.block(c, content_x, cy, content_w, child_cb_h, (None, None));
                } else {
                    self.collect_inline(c, &mut inline, s);
                }
            }
            let ly = cy;
            cy += self.lines(inline, content_x, cy, content_w, s);
            self.place_pending_abs(content_x, ly);
        }
        let content_h = cy - (by + bt + pt);
        // The box's own border and background are outside its own clip.
        self.scene.cur.0 = saved_attrs.0;
        let hgt = definite.unwrap_or_else(|| clamp(content_h + pt + pb + bt + bb).max(vframe));
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
        if bg & 0xff != 0 && self.canvas_src != Some(h) { paint.push(Rect { x: bx, y: by, w, h: hgt, rgba: bg, radii, ring: 0 }) }
        // ★ The background image goes OVER the background colour and under
        // everything else, so it is inserted at the box's own slot, after the
        // colour. Its positioning area is the PADDING box (CSS's
        // background-origin default); it is painted over the whole border box
        // (the background-clip default). Neither property is in the profile,
        // so both are fixed rules here.
        let bg_image = match s.get("background-image") {
            V::Gradient { angle_deg, stops } => {
                let pad = (bx + bl, by + bt, (w - bl - br).max(0), (hgt - bt - bb).max(0));
                let area = self.tiling(s, (bx, by, w, hgt), pad, None);
                Some(Grad { area, angle: (angle_deg * 64.0).round() as i64, stops: resolve_stops(stops, s) })
            }
            V::Url(_) => None,
            _ => None,
        };
        // An image background: the same tiling, sized from the DECLARED
        // intrinsic size (§3.13 — there is no measurement path here).
        let bg_url = match s.get("background-image") {
            V::Url(u) => match self.subs.get(u.as_str()) {
                Some((addr, iw, ih)) => {
                    let (addr, iw, ih) = (addr.clone(), *iw, *ih);
                    let pad = (bx + bl, by + bt, (w - bl - br).max(0), (hgt - bt - bb).max(0));
                    let area = self.tiling(s, (bx, by, w, hgt), pad, Some((iw, ih)));
                    Some(crate::Image { area, address: addr, fit: "fill" })
                }
                None => {
                    let u = u.clone();
                    self.diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "input.subresource-not-supplied",
                        msg: format!("background-image: url({u}): no subresource declared; §3.13 admits no measurement path") });
                    None
                }
            },
            _ => None,
        };
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
        let n_paint = paint.len();
        // ★ The shadow goes BEHIND the box, so it is inserted at the slot
        // first and the box's own rects land after it. The spread inflates
        // the box on every side, and the corner radii with it (CSS Backgrounds
        // 3 §6.2: shadow radius = box radius + spread, floored at 0).
        let mut slot = bg_slot;
        if !hidden {
            if let V::Shadow { x: sx, y: sy, blur, spread, color } = s.get("box-shadow") {
                let sp = u(spread.v);
                let current = match s.get("color") { V::Color(c) => rgba(c, 0x000000ff), _ => 0x000000ff };
                let sh = Shadow { x: bx + u(sx.v) - sp, y: by + u(sy.v) - sp, w: w + 2 * sp, h: hgt + 2 * sp,
                                  rgba: rgba(color, current), blur: u(blur.v).max(0),
                                  radii: radii.map(|r| if r > 0 { (r + sp).max(0) } else { 0 }) };
                if sh.w > 0 && sh.h > 0 && sh.rgba & 0xff != 0 {
                    self.scene.insert_shadow(slot, sh);
                    slot += 1;
                }
            }
        }
        for (i, r) in paint.into_iter().enumerate() { self.scene.insert_rect(slot + i, r) }
        slot += n_paint;
        if let Some(g) = bg_image {
            if !hidden && g.area.w > 0 && g.area.h > 0 { self.scene.insert_grad(slot, g); slot += 1 }
        }
        if let Some(im) = bg_url {
            if !hidden && im.area.w > 0 && im.area.h > 0 { self.scene.insert_image(slot, im); slot += 1 }
        }
        // The replaced content itself, fitted into the content box.
        if let Some((addr, nat)) = repl.filter(|(a, ..)| !a.is_empty()) {
            let content = (bx + bl + pl, by + bt + pt, (w - frame).max(0), (hgt - pt - pb - bt - bb).max(0));
            // `object-fit` needs a natural size; an image without one (or
            // without a ratio) simply fills its box.
            let (iw, ih) = match (nat.w, nat.h, nat.ratio) {
                (Some(a), Some(b), Some(_)) => (a, b),
                (_, _, Some((rw, rh))) => (rw, rh),
                _ => (content.2, content.3),
            };
            let fit = match kw(s, "object-fit") { "contain" => "contain", "cover" => "cover", "none" => "none", "scale-down" => "scale-down", _ => "fill" };
            let (tx, ty, tw, th) = Self::object_tile(fit, content, (iw, ih));
            if !hidden && content.2 > 0 && content.3 > 0 {
                self.scene.insert_image(slot, crate::Image {
                    area: crate::Tiling { x: content.0, y: content.1, w: content.2, h: content.3, tx, ty, tw: tw.max(1), th: th.max(1), repeat: 0 },
                    address: addr, fit });
                slot += 1;
            }
        }
        let painted = slot - bg_slot;
        // ★ Inserting this box's background at the slot reserved before the
        // children SHIFTS every entry after it, so the ranges the children
        // recorded no longer point at what they painted. Move them.
        if painted > 0 {
            for c in self.contexts.iter_mut().filter(|c| c.1 >= bg_slot) { c.1 += painted; c.2 += painted }
            for h in self.hoists.iter_mut().filter(|h| h.0 >= bg_slot) { h.0 += painted; h.1 += painted }
        }
        if let Some((id, list)) = xf_id {
            let origin = match s.get("transform-origin") {
                V::Pair(a, b) => (len(a, w).unwrap_or(w / 2), len(b, hgt).unwrap_or(hgt / 2)),
                _ => (w / 2, hgt / 2),
            };
            self.scene.xforms[id as usize] = transform_matrix(&list, w, hgt, (bx + origin.0, by + origin.1));
        }
        self.scene.cur = saved_attrs;
        if let Some(v) = self.boxes.as_mut() { v.push((h, bx, by, w, hgt)) }
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
        // `None` is `auto`; resolved below, once the box's size is known.
        let (mla, mra) = (h_off("margin-left"), h_off("margin-right"));
        let (mta, mba) = (h_off("margin-top"), h_off("margin-bottom"));
        let (mut ml, mut mr) = (mla.unwrap_or(0), mra.unwrap_or(0));
        let (mut mt, mut mb) = (mta.unwrap_or(0), mba.unwrap_or(0));
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
        // ★ AUTO MARGINS (CSS 2.1 §10.3.7): with both offsets AND the width
        // given, an auto margin takes what is left — both auto split it,
        // which is the `left: 0; right: 0; margin: auto` centring idiom.
        // Taken as 0, WPT's box-sizing-003 put its square 25 px off.
        if let (Some(l), Some(r), false) = (left, right, matches!(s.get("width"), V::Kw("auto"))) {
            let free = cbw - l - r - w - ml - mr;
            match (mla.is_none(), mra.is_none()) {
                (true, true) if free >= 0 => { ml = free / 2; mr = free - free / 2 }
                (true, true) => mr = free, // negative: the start margin stays 0
                (true, false) => ml = free,
                (false, true) => mr = free,
                _ => {}
            }
        }
        // …and vertically (§10.6.4), when the height is given too.
        let spec_h = match s.get("height") { V::Pct(_) => cbh.and_then(|b| len(s.get("height"), b)), V::Kw(_) => None, v => len(v, 0) };
        if let (Some(t), Some(b), Some(hh), Some(ch)) = (top, bottom, spec_h, cbh) {
            let hh = hh.max(bt + bb + v_off("padding-top").unwrap_or(0) + v_off("padding-bottom").unwrap_or(0));
            let free = ch - t - b - hh - mt - mb;
            match (mta.is_none(), mba.is_none()) {
                (true, true) => { mt = free / 2; mb = free - free / 2 }
                (true, false) => mt = free,
                (false, true) => mb = free,
                _ => {}
            }
        }
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

    /// A replaced element: `<img src>` resolved through the input's
    /// subresources. ★ §3.13 again — an undeclared image is refused, never
    /// measured here.
    /// ★ The DECLARED size counts even when no bytes are in hand — the same
    /// rule `replaced` uses, and it must be the same rule, because this is
    /// what MEASURES the box. Consulting only the subresource map made an
    /// image 0 wide during offline table pre-measurement (which is run
    /// without one), so its column came out too narrow and the image
    /// overflowed the cell at render time.
    /// The CONTENT-box size of a replaced element laid out with nothing
    /// around it — its contribution to a line or an intrinsic size.
    fn replaced_size(&self, h: Handle) -> Option<(U, U)> {
        let (_, nat) = self.natural(h)?;
        let s = self.st(h)?;
        let px = |p: &str| len(s.get(p), 0).unwrap_or(0);
        let bd = |side: &str| if kw(s, &format!("border-{side}-style")) == "none" { 0 } else { px(&format!("border-{side}-width")) };
        let fh = px("padding-left") + px("padding-right") + bd("left") + bd("right");
        let fv = px("padding-top") + px("padding-bottom") + bd("top") + bd("bottom");
        let (w, hh) = Self::replaced_used(s, &nat, 0, None, fh, fv);
        Some(((w - fh).max(0), (hh - fv).max(0)))
    }

    /// ★ An image's NATURAL sizing (CSS Images 3 §5.1): a width, a height and
    /// a ratio, EACH of which may be absent. The document declares them on
    /// the element — `width`, `height`, and `natural-ratio` (`W/H` or `none`)
    /// — and the subresource map names the bytes. Both dimensions and no
    /// `natural-ratio` means the ratio is theirs; one dimension alone means
    /// none. An SVG that states only `width="100"` has no ratio, and giving
    /// it one shrank its width under `max-height` where a browser keeps it.
    fn natural(&self, h: Handle) -> Option<(String, Natural)> {
        if self.dom.tag(h) != Some("img") { return None }
        let src = self.dom.attr(h, "src").unwrap_or("");
        let dim = |n: &str| self.dom.attr(h, n).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v > 0.0).map(u);
        let (w, hh) = (dim("width"), dim("height"));
        let ratio_attr = self.dom.attr(h, "natural-ratio").map(str::trim);
        let sub = self.subs.get(src);
        let addr = sub.map(|(a, ..)| a.clone()).unwrap_or_default();
        let declared = w.is_some() || hh.is_some() || ratio_attr.is_some();
        let nat = if declared {
            let ratio = match ratio_attr {
                Some("none") => None,
                Some(r) => r.split_once('/').and_then(|(a, b)| Some((a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?)))
                    .filter(|(a, b)| *a > 0.0 && *b > 0.0).map(|(a, b)| (u(a), u(b))),
                None => w.zip(hh),
            };
            Natural { w, h: hh, ratio }
        } else {
            // Undeclared on the element: the subresource map's size, both
            // dimensions, hence a ratio.
            let (_, sw, sh) = sub?;
            let (sw, sh) = (Some(*sw).filter(|v| *v > 0), Some(*sh).filter(|v| *v > 0));
            Natural { w: sw, h: sh, ratio: sw.zip(sh) }
        };
        Some((addr, nat))
    }

    fn replaced(&mut self, h: Handle) -> Option<(String, Natural)> {
        if self.dom.tag(h) != Some("img") { return None }
        let src = self.dom.attr(h, "src").unwrap_or("").to_string();
        // ★ §1.2 says replaced content CARRIES its natural sizing, on the
        // element. The subresource map is a separate thing: it names the
        // BYTES. So a declared image lays out and holds its space — no layout
        // shift — even when the bytes were not supplied, and that is counted
        // rather than refused.
        match self.natural(h) {
            Some((addr, nat)) => {
                if addr.is_empty() { self.count("image bytes not supplied (space reserved)") }
                Some((addr, nat))
            }
            None => {
                self.diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "input.subresource-not-supplied",
                    msg: format!("<img src={src:?}>: no natural size declared and no subresource supplied; §3.13 admits no measurement path") });
                None
            }
        }
    }

    /// ★ The used BORDER-BOX size of a replaced element, width and height
    /// together: specified sizes first, then the ratio, then the natural
    /// size, then the default object size 300×150 (CSS 2.1 §10.3.2, §10.6.2;
    /// CSS Images 3 §5.3), with min/max applied by CSS 2.1 §10.4's
    /// ratio-preserving table when both sizes are auto and there is a ratio,
    /// and to each axis alone otherwise. All the profile's sizes are
    /// border-box, so the frame comes off first and goes back on last.
    fn replaced_used(s: &Style, nat: &Natural, cb_w: U, cb_h: Option<U>, fh: U, fv: U) -> (U, U) {
        let hv = |p: &str| -> Option<U> { match s.get(p) { V::Kw(_) => None, V::Pct(_) => cb_h.and_then(|b| len(s.get(p), b)), v => len(v, 0) } };
        let wv = |p: &str| -> Option<U> { match s.get(p) { V::Kw(_) => None, V::Pct(_) if cb_w <= 0 => None, v => len(v, cb_w) } };
        let (sw, sh) = (wv("width").map(|v| (v - fh).max(0)), hv("height").map(|v| (v - fv).max(0)));
        let mnw = wv("min-width").map(|v| (v - fh).max(0)).unwrap_or(0);
        let mxw = wv("max-width").map(|v| (v - fh).max(0)).unwrap_or(U::MAX / 4).max(mnw);
        let mnh = hv("min-height").map(|v| (v - fv).max(0)).unwrap_or(0);
        let mxh = hv("max-height").map(|v| (v - fv).max(0)).unwrap_or(U::MAX / 4).max(mnh);
        let r_h = |w: U| nat.ratio.map(|(rw, rh)| (w as i128 * rh as i128 / rw.max(1) as i128) as U);
        let r_w = |h: U| nat.ratio.map(|(rw, rh)| (h as i128 * rw as i128 / rh.max(1) as i128) as U);
        let (dw, dh) = (u(300.0), u(150.0));
        let (w, h) = match (sw, sh) {
            (Some(w), Some(h)) => (w.clamp(mnw, mxw), h.clamp(mnh, mxh)),
            // One given: the other follows the ratio from its USED value.
            (Some(w), None) => { let w = w.clamp(mnw, mxw); (w, r_h(w).or(nat.h).unwrap_or(dh).clamp(mnh, mxh)) }
            (None, Some(h)) => { let h = h.clamp(mnh, mxh); (r_w(h).or(nat.w).unwrap_or(dw).clamp(mnw, mxw), h) }
            (None, None) => {
                let (w, h) = match (nat.w, nat.h, nat.ratio) {
                    (Some(w), Some(h), _) => (w, h),
                    (Some(w), None, Some(_)) => (w, r_h(w).unwrap_or(dh)),
                    (None, Some(h), Some(_)) => (r_w(h).unwrap_or(dw), h),
                    (Some(w), None, None) => (w, dh),
                    (None, Some(h), None) => (dw, h),
                    // A ratio alone: the largest box of that ratio inside the
                    // default object size (CSS Images 3 §5.3, `contain`).
                    (None, None, Some((rw, rh))) => {
                        if dw as i128 * rh as i128 <= dh as i128 * rw as i128 { (dw, (dw as i128 * rh as i128 / rw.max(1) as i128) as U) }
                        else { ((dh as i128 * rw as i128 / rh.max(1) as i128) as U, dh) }
                    }
                    (None, None, None) => (dw, dh),
                };
                match nat.ratio {
                    Some(_) if w > 0 && h > 0 => constrain_ratio(w, h, (mnw, mxw), (mnh, mxh)),
                    _ => (w.clamp(mnw, mxw), h.clamp(mnh, mxh)),
                }
            }
        };
        (w + fh, h + fv)
    }

    /// `object-fit` (row 58): how the source fills the content box. Returns
    /// the tile, which the content box then clips — so `cover` needs no
    /// special case in a reader.
    fn object_tile(fit: &str, (cx, cy, cw, ch): (U, U, U, U), (iw, ih): (U, U)) -> (U, U, U, U) {
        if iw <= 0 || ih <= 0 { return (cx, cy, cw, ch) }
        let ratio = |target_w: U| (target_w as i128 * ih as i128 / iw as i128) as U;
        let (tw, th) = match fit {
            "fill" => (cw, ch),
            "none" => (iw, ih),
            "contain" | "scale-down" => {
                let (fw, fh) = if ratio(cw) <= ch { (cw, ratio(cw)) } else { ((ch as i128 * iw as i128 / ih as i128) as U, ch) };
                // scale-down never ENLARGES: it is the smaller of none and contain.
                if fit == "scale-down" && iw <= fw { (iw, ih) } else { (fw, fh) }
            }
            _ => {
                // cover: the smaller side reaches the box, the other overflows.
                if ratio(cw) >= ch { (cw, ratio(cw)) } else { ((ch as i128 * iw as i128 / ih as i128) as U, ch) }
            }
        };
        // Centred, which is `object-position: 50% 50%` — the profile has no
        // object-position row, so it is a fixed rule.
        (cx + (cw - tw) / 2, cy + (ch - th) / 2, tw, th)
    }

    /// A cell's `colspan`: HTML's parsing rules (a positive integer, at most
    /// 1000), 1 when absent or invalid.
    fn colspan(&self, c: Handle) -> usize {
        // Only an HTML table cell spans; a CSS table's cells cannot.
        if !matches!(self.dom.tag(c), Some("td") | Some("th")) { return 1 }
        self.dom.attr(c, "colspan").and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1).map(|n| n.min(1000)).unwrap_or(1)
    }

    /// ★ The elements that DECLARE a table's columns (§3.13): its `<col>`s,
    /// direct or inside a `<colgroup>`, one per column — or, when it has
    /// none, the cells of its first row. The second form cannot describe a
    /// first row that SPANS (every Wikipedia navbox opens with one title
    /// cell across both columns), so such a table must use `<col>`.
    fn column_decls(&self, h: Handle) -> Result<Vec<Handle>, &'static str> {
        let mut cols = vec![];
        for c in self.dom.element_children(h) {
            match self.dom.tag(c) {
                Some("col") => cols.push(c),
                Some("colgroup") => cols.extend(self.dom.element_children(c).into_iter().filter(|g| self.dom.tag(*g) == Some("col"))),
                _ => {}
            }
        }
        if !cols.is_empty() { return Ok(cols) }
        let mut stack: Vec<Handle> = self.dom.element_children(h).into_iter().rev().collect();
        while let Some(c) = stack.pop() {
            match self.st(c).map(|cs| kw(cs, "display")) {
                Some("table-row") => {
                    let cells: Vec<Handle> = self.dom.element_children(c).into_iter()
                        .filter(|x| self.st(*x).map(|cs| kw(cs, "display")) == Some("table-cell")).collect();
                    if cells.iter().any(|x| self.colspan(*x) > 1) { return Err("spanning first row") }
                    return Ok(cells);
                }
                Some("none") => {}
                _ => for g in self.dom.element_children(c).into_iter().rev() { stack.push(g) },
            }
        }
        Ok(vec![])
    }

    /// The narrowest a table can be: its declared column minimums plus the
    /// spacing around and between them. `None` when the columns are not
    /// declared, in which case the table is refused anyway (§3.13).
    fn table_min_content(&mut self, h: Handle, s: &Style, cb_w: U) -> Option<U> {
        let (sx, _) = match s.get("border-spacing") {
            V::Pair(a, b) => (len(a, cb_w).unwrap_or(0), len(b, 0).unwrap_or(0)),
            _ => (0, 0),
        };
        let cells = self.column_decls(h).ok()?;
        if cells.is_empty() { return None }
        let mut total = sx * (cells.len() as U + 1);
        for c in &cells {
            let cs = self.st(*c)?;
            total += match (len(cs.get("width"), cb_w), len(cs.get("min-width"), cb_w)) {
                (Some(w), _) => w,
                (None, Some(m)) => m,
                _ => return None,
            };
        }
        Some(total)
    }

    /// CSS background geometry: `background-size` gives the tile, then
    /// `background-position-x/y` place it in the POSITIONING area (the
    /// padding box), and `background-repeat` says how it tiles over the
    /// PAINTED area (the border box).
    ///
    /// `intrinsic` is the image's own size, or `None` for a gradient — which
    /// has no intrinsic size, so `auto` is the positioning area itself.
    fn tiling(&mut self, s: &Style, paint: (U, U, U, U), pos_area: (U, U, U, U), intrinsic: Option<(U, U)>) -> crate::Tiling {
        let (ax, ay, aw, ah) = pos_area;
        let (iw, ih) = intrinsic.unwrap_or((aw, ah));
        let (tw, th) = match s.get("background-size") {
            V::Kw("cover") | V::Kw("contain") if iw > 0 && ih > 0 => {
                // Scale to fit or to fill, keeping the ratio, in integers.
                let (nw, nh) = (aw as i128 * ih as i128, ah as i128 * iw as i128);
                let cover = kw(s, "background-size") == "cover";
                if (nw > nh) == cover { (aw, (aw as i128 * ih as i128 / iw as i128) as U) }
                else { ((ah as i128 * iw as i128 / ih as i128) as U, ah) }
            }
            V::Kw(_) => (iw, ih),
            // One value sizes the WIDTH; the height follows the ratio, and a
            // gradient has none, so it takes the area's height.
            v => {
                let tw = len(v, aw).unwrap_or(iw).max(0);
                let th = if intrinsic.is_some() && iw > 0 { (tw as i128 * ih as i128 / iw as i128) as U } else { ah };
                (tw, th)
            }
        };
        // P% puts the point P% across the tile at P% across the area.
        let place = |v: &V, area: U, tile: U, is_x: bool| -> U {
            match v {
                V::Kw(k) => match *k {
                    "right" | "bottom" => area - tile,
                    "center" => (area - tile) / 2,
                    _ => 0,
                },
                V::Pct(p) => scale(area - tile, (*p * 1024.0).round() as i64, 102400),
                v => len(v, area).unwrap_or(0) * if is_x { 1 } else { 1 },
            }
        };
        let repeat = match kw(s, "background-repeat") { "no-repeat" => 0, "repeat-x" => 1, "repeat-y" => 2, _ => 3 };
        crate::Tiling { x: paint.0, y: paint.1, w: paint.2, h: paint.3,
                        tx: ax + place(s.get("background-position-x"), aw, tw, true),
                        ty: ay + place(s.get("background-position-y"), ah, th, false),
                        tw: tw.max(1), th: th.max(1), repeat }
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
            outer - self.st(h).and_then(|cs| len(cs.get("margin-bottom"), cb_w)).unwrap_or(0)
        })
    }

    fn measure(&mut self, h: Handle, cb_w: U, cb_h: Option<U>, force: (Option<U>, Option<U>)) -> U {
        let (o, r, n, l) = (self.scene.order.len(), self.scene.rects.len(), self.scene.runs.len(), self.scene.links.len());
        // ★ Every node kind, not just the three that existed when this was
        // written. `order` alone being rolled back hides it — the scene
        // still renders correctly, and quietly carries orphan nodes that
        // nothing points at.
        let (sh, gr_n, im) = (self.scene.shadows.len(), self.scene.grads.len(), self.scene.images.len());
        let (cl, gr, cur, xf) = (self.scene.clips.len(), self.scene.groups.len(), self.scene.cur, self.scene.xforms.len());
        let diags = self.diagnostics.len();
        let (links, counts, marker, report) = (self.links.len(), self.unimplemented.clone(), self.marker.clone(), self.report.clone());
        let (ctx, hoi) = (self.contexts.len(), self.hoists.len());
        // A trial layout collects and places its OWN out-of-flow boxes (and
        // then throws them away with the rest of the scene); it must not
        // consume the ones the real pass is still holding.
        let pend = std::mem::take(&mut self.pending_abs);
        let hgt = self.block(h, 0, 0, cb_w, cb_h, force);
        self.pending_abs = pend;
        self.scene.order.truncate(o); self.scene.rects.truncate(r); self.scene.runs.truncate(n); self.scene.links.truncate(l);
        self.scene.rect_attrs.truncate(r); self.scene.run_attrs.truncate(n); self.scene.link_attrs.truncate(l);
        self.scene.shadows.truncate(sh); self.scene.shadow_attrs.truncate(sh);
        self.scene.grads.truncate(gr_n); self.scene.grad_attrs.truncate(gr_n);
        self.scene.images.truncate(im); self.scene.image_attrs.truncate(im);
        self.scene.clips.truncate(cl); self.scene.groups.truncate(gr); self.scene.cur = cur;
        self.scene.xforms.truncate(xf); self.scene.xform_parents.truncate(xf);
        // A trial layout must leave no diagnostics behind: the real one
        // re-emits whatever it finds.
        self.diagnostics.truncate(diags);
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
                // ★ CSS wraps a run of loose text in an ANONYMOUS item. It
                // has no style of its own, so every flex property takes its
                // initial value: no grow, shrink 1, basis from the content,
                // no margins, and the container's `align-items`.
                Some(Kind::Text(t)) if !t.trim().is_empty() => {
                    let base = self.content_main(c, row, w);
                    let min = if row { self.intrinsic(c).0 } else { base };
                    items.push(It { h: c, base, hypo: base.max(min), grow: 0.0, shrink: 1.0, min, max: U::MAX,
                                    main: base.max(min), cross: 0, m_start: 0, m_end: 0, c_start: 0, c_end: 0,
                                    align: match kw(s, "align-items") { "flex-start" | "start" => "start", "flex-end" | "end" => "end",
                                                                        "center" => "center", "baseline" => "baseline", _ => "stretch" },
                                    cross_auto: true });
                    continue;
                }
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
                let (ml, mt) = self.st(i.h).map(|cs| (len(cs.get("margin-left"), w).unwrap_or(0), len(cs.get("margin-top"), w).unwrap_or(0))).unwrap_or((0, 0));
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
                // ★ CSS wraps a run of loose text in an ANONYMOUS item: no
                // style, so it is auto-placed in one cell and aligned by the
                // container alone.
                Some(Kind::Text(t)) if !t.trim().is_empty() => {
                    let g = |p: &str| match kw(s, p) { "" => "stretch".to_string(), a => a.to_string() };
                    raw.push((c, GridLine::Auto, GridLine::Auto, GridLine::Auto, GridLine::Auto, g("justify-items"), g("align-items")));
                    continue;
                }
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
        // ★ The IMPLICIT grid first (CSS Grid §8.5): the fixed axis has as
        // many tracks as the template names OR as any item's definite
        // position and span reaches, whichever is more. Using the template's
        // count alone made the search below spin forever — an item placed
        // in column 2 of a one-column grid never fits, so the loop advanced
        // the row without end. FT's error page (`dt` in column 1, `dd` in
        // column 2, no template) hung the renderer outright.
        let tpl_len = if flow_row { cols_tpl.len() } else { rows_tpl.len() };
        let fixed_len = raw.iter().map(|(_, cs0, ce, rs, re, ..)| {
            let (start, span) = if flow_row { span_of(*cs0, *ce) } else { span_of(*rs, *re) };
            start.unwrap_or(0) + span
        }).max().unwrap_or(0).max(tpl_len).max(1);
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
            let frame = self.st(it.h).map(|cs| len(cs.get("padding-left"), w).unwrap_or(0) + len(cs.get("padding-right"), w).unwrap_or(0)).unwrap_or(0);
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
            let specified_w = self.st(it.h).and_then(|cs| len(cs.get("width"), aw));
            let iw = match it.justify.as_str() {
                "stretch" => specified_w.unwrap_or(aw),
                _ => specified_w.unwrap_or_else(|| self.intrinsic_outer_w(it.h, aw)).min(aw),
            };
            let specified_h = self.st(it.h).and_then(|cs| match cs.get("height") { V::Kw(_) => None, V::Pct(_) => len(cs.get("height"), ah), v => len(v, 0) });
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
        // ★ §3.13: the columns arrive MEASURED. A first-row cell declares
        // either an exact `width`, or the pair `min-width`/`max-width` — the
        // column's min-content and max-content widths, which the normalizer
        // measured offline. The pair is distributed into the available inline
        // size by the SAME track sizer grid uses, which is what §3.13 means
        // by "the same shape as resolving grid tracks of
        // minmax(min-content, max-content)": the expensive half is
        // precomputed, the viewport-dependent half stays here.
        if rows.is_empty() { return 0 }
        let first_cells = match self.column_decls(h) {
            Ok(c) if !c.is_empty() => c,
            Ok(_) => return 0,
            Err(_) => {
                self.diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "table.column-width-undeclared",
                    msg: "a table whose first row spans columns must declare its columns with <col> elements (§3.13); table refused".into() });
                return 0;
            }
        };
        let avail = (w - sx * (first_cells.len() as U + 1)).max(0);
        let mut tracks: Vec<navigator_style::values::Track> = vec![];
        let (mut mins, mut maxs): (Vec<U>, Vec<U>) = (vec![], vec![]);
        // Which columns may take leftover space: only the MEASURED ones. An
        // exact `width` is exact — growing it was the first bug here.
        let mut flexible: Vec<bool> = vec![];
        for c in &first_cells {
            let cs = self.st(*c).expect("styled");
            let exact = len(cs.get("width"), w);
            let (mn, mx) = (len(cs.get("min-width"), w), len(cs.get("max-width"), w));
            match (exact, mn, mx) {
                (Some(cw), _, _) => { tracks.push(navigator_style::values::Track::Len(navigator_style::values::Length { v: cw as f64 / PX as f64, unit: navigator_style::values::Unit::Px })); mins.push(cw); maxs.push(cw); flexible.push(false) }
                (None, Some(a), Some(b)) => {
                    tracks.push(navigator_style::values::Track::MinMax(
                        Box::new(navigator_style::values::Track::MinContent),
                        Box::new(navigator_style::values::Track::MaxContent)));
                    mins.push(a); maxs.push(b.max(a)); flexible.push(true);
                }
                _ => {
                    self.diagnostics.push(Diagnostic { pos: Pos { line: 0, col: 0 }, code: "table.column-width-undeclared",
                        msg: "a table's first row must declare every column width — an exact `width`, or `min-width`/`max-width` (§3.13: content that would need unbounded measurement arrives measured); table refused".into() });
                    return 0;
                }
            }
        }
        if tracks.is_empty() { return 0 }
        let mut cols: Vec<U> = size_tracks(&tracks, Some(avail), sx, &mins, &maxs);
        // Share what is left over among the columns that can still grow,
        // so a measured table fills its box instead of hugging its text.
        // ★ Leftover space is only taken when the TABLE's own width says so.
        // A `width: auto` table shrink-wraps its measured columns — spreading
        // the leftover equally made a one-character column 281 px wide. When
        // the width IS specified, the excess goes proportionally to each
        // column's max-content, which is how the wide column stays wide.
        let stretch = !matches!(s.get("width"), V::Kw(_));
        let used: U = cols.iter().sum::<U>();
        let free = avail - used;
        let targets: Vec<usize> = (0..cols.len()).filter(|i| flexible[*i]).collect();
        let total_max: U = targets.iter().map(|i| maxs[*i]).sum();
        if stretch && free > 0 && !targets.is_empty() && total_max > 0 {
            let mut given = 0;
            for (n, i) in targets.iter().enumerate() {
                let add = if n + 1 == targets.len() { free - given } else { (free as i128 * maxs[*i] as i128 / total_max as i128) as U };
                cols[*i] += add;
                given += add;
            }
        }
        let mut cy = y + sy;
        for r in rows {
            let cells = cells_of(self, r);
            // ★ Each cell takes the next `colspan` columns, and its width is
            // theirs together with the spacing between them. A cell past the
            // declared columns keeps the old fallback (the last column's
            // width) and is counted, not silently squeezed.
            let mut k = 0usize;
            let mut widths: Vec<U> = vec![];
            let mut starts: Vec<usize> = vec![];
            for c in &cells {
                if self.dom.attr(*c, "rowspan").and_then(|v| v.trim().parse::<usize>().ok()).is_some_and(|n| n > 1) {
                    self.count("rowspan (the cell occupies one row)");
                }
                let n = self.colspan(*c);
                starts.push(k);
                if k >= cols.len() {
                    self.count("table cell past the declared columns");
                    widths.push(*cols.last().expect("non-empty"));
                } else {
                    let end = (k + n).min(cols.len());
                    widths.push(cols[k..end].iter().sum::<U>() + sx * (end - k - 1) as U);
                }
                k += n;
            }
            // Natural heights first: the row is as tall as its tallest cell.
            let nat: Vec<U> = cells.iter().enumerate()
                .map(|(i, c)| { let cw = widths[i]; self.table_part = true; self.measure(*c, cw, None, (Some(cw), None)) })
                .collect();
            let row_h = nat.iter().copied().max().unwrap_or(0);
            // Baselines, for cells aligned on one (the row's shared baseline).
            let bases: Vec<Option<U>> = cells.iter().enumerate().map(|(i, c)| {
                let cs = self.st(*c).expect("styled");
                (kw(cs, "vertical-align") == "baseline").then(|| {
                    let cw = widths[i];
                    self.table_part = true;
                    self.baseline(*c, cw, (Some(cw), None))
                })
            }).collect();
            let max_b = bases.iter().flatten().copied().max().unwrap_or(0);
            // Column i's left edge, so a cell starts where its first column does.
            let col_x = |i: usize| -> U { x + sx + cols[..i.min(cols.len())].iter().sum::<U>() + sx * i.min(cols.len()) as U };
            for (i, c) in cells.iter().enumerate() {
                let cw = widths[i];
                let cx = if starts[i] < cols.len() { col_x(starts[i]) } else { col_x(cols.len()) };
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
            }
            cy += row_h + sy;
        }
        cy - y
    }

    /// An item's content size along the main axis (its max-content width in
    /// a row, its laid-out height in a column), as a border-box size.
    fn content_main(&mut self, h: Handle, row: bool, cb_w: U) -> U {
        if row { self.intrinsic_outer_w(h, cb_w) } else {
            let m = self.st(h).map(|cs| len(cs.get("margin-top"), cb_w).unwrap_or(0) + len(cs.get("margin-bottom"), cb_w).unwrap_or(0)).unwrap_or(0);
            self.measure(h, cb_w, None, (None, None)) - m
        }
    }

    /// Max-content border-box width of `h` (its width if definite).
    /// Margin-box width of an atomic inline: its border box (a specified
    /// `width` INCLUDES padding and border here — border-box is the profile's
    /// fixed rule) plus its horizontal margins. `cb_w` of 0 asks for the
    /// intrinsic answer, which is what `intrinsic` needs.
    /// The MIN-content outer width of an atomic inline, when that differs
    /// from its max-content one: an `auto`-width box that is not replaced,
    /// whose own content can wrap. `None` means its width is fixed (a length,
    /// `max-content`, or a replaced element's own size).
    fn atomic_min_outer_w(&mut self, h: Handle) -> Option<U> {
        let cs = self.st(h)?;
        if !matches!(cs.get("width"), V::Kw(k) if *k != "max-content") { return None }
        if self.replaced_size(h).is_some() { return None }
        let px = |p: &str| len(cs.get(p), 0).unwrap_or(0);
        let (ml, mr) = (px("margin-left"), px("margin-right"));
        let border = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { px(&format!("border-{side}-width")) };
        let frame = px("padding-left") + px("padding-right") + border("left") + border("right");
        let mut bw = frame + self.intrinsic(h).0;
        if let Some(m) = len(cs.get("max-width"), 0) { bw = bw.min(m) }
        if let Some(m) = len(cs.get("min-width"), 0) { bw = bw.max(m) }
        Some(bw.max(frame) + ml + mr)
    }

    fn atomic_outer_w(&mut self, h: Handle, cb_w: U) -> U {
        let Some(cs) = self.st(h) else { return 0 };
        let px = |p: &str| len(cs.get(p), cb_w).unwrap_or(0);
        let (ml, mr) = (px("margin-left"), px("margin-right"));
        let border = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { px(&format!("border-{side}-width")) };
        let frame = px("padding-left") + px("padding-right") + border("left") + border("right");
        let mut bw = match cs.get("width") {
            V::Kw("min-content") => self.intrinsic(h).0 + frame,
            V::Kw("max-content") => self.intrinsic(h).1 + frame,
            V::Kw(_) => match self.replaced_size(h) {
                // A replaced box is not shrink-to-fit: it is its own size.
                Some((iw, _)) => frame + iw,
                None => {
                    // Shrink-to-fit inside what the line has room for.
                    let avail = (cb_w - ml - mr - frame).max(0);
                    let (mn, mx) = self.intrinsic(h);
                    frame + if cb_w > 0 { mx.min(avail).max(mn.min(avail)) } else { mx }
                }
            },
            v => len(v, cb_w).unwrap_or(0),
        };
        if let Some(m) = len(cs.get("max-width"), cb_w) { bw = bw.min(m) }
        if let Some(m) = len(cs.get("min-width"), cb_w) { bw = bw.max(m) }
        bw.max(frame) + ml + mr
    }

    fn intrinsic_outer_w(&mut self, h: Handle, cb_w: U) -> U {
        // An anonymous block box around loose text has no style and no box
        // decoration: its outer width IS its content width.
        let Some(cs) = self.st(h) else { return self.intrinsic(h).1.min(cb_w.max(0)) };
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
        let (links, counts, pend) = (self.links.len(), self.unimplemented.clone(), self.pending_abs.len());
        let (mut mn, mut mx) = (0, 0);
        let mut items = vec![];
        // An anonymous block box around a run of loose text (see `block`).
        let anon = matches!(self.dom.get(h).map(|n| &n.kind), Some(Kind::Text(_)));
        let s = match if anon { self.dom.get(h).and_then(|n| n.parent).and_then(|p| self.st(p)) } else { self.st(h) } {
            Some(s) => s, None => return (0, 0),
        };
        if anon { self.collect_inline(h, &mut items, s) }
        // ★ A ROW FLEX CONTAINER is as wide as its items TOGETHER, not as
        // wide as its widest one: they sit side by side. Taking the max —
        // which is right for stacked blocks — measured MDN's breadcrumb as
        // one crumb wide, and the rest was clipped away.
        if !anon && kw(s, "display") == "flex" && kw(s, "flex-direction") != "column" {
            let gap = len(s.get("column-gap"), 0).unwrap_or(0);
            // ★ A run of text directly inside a flex container is an
            // ANONYMOUS flex item (CSS Flexbox §4) and counts like any other.
            // Summing element children only measured arXiv's header links —
            // `display: flex` anchors holding bare text — at zero, so each
            // was its 20 px of padding and the words piled onto each other.
            let kids: Vec<Handle> = self.dom.children_of(h).into_iter()
                .filter(|c| match self.dom.get(*c).map(|n| &n.kind) {
                    Some(Kind::Text(t)) => !t.trim().is_empty(),
                    Some(Kind::Element(_)) => self.st(*c).is_some_and(|cs| kw(cs, "display") != "none"
                        && !matches!(kw(cs, "position"), "absolute" | "fixed")),
                    _ => false,
                }).collect();
            let (mut smn, mut smx) = (0, 0);
            let mut mins: Vec<U> = vec![];
            for c in &kids {
                // ★ A child with a DEFINITE width is that wide — its content
                // does not get a vote. Recursing past it measured GitHub's
                // file-tree pane, whose own `width: 0` collapses it, at the
                // max-content width of the tree inside: 446 px of empty
                // space, and the README squeezed into 353 of the 800.
                // The profile is border-box, so only the margins are outside.
                let cw = self.st(*c).and_then(|cs| match cs.get("width") { V::Len(l) => Some(u(l.v)), _ => None });
                let (a, b) = match cw { Some(w) => (w, w), None => self.intrinsic(*c) };
                let frame = self.st(*c).map(|cs| {
                    let px = |p: &str| len(cs.get(p), 0).unwrap_or(0);
                    px("margin-left") + px("margin-right") + if cw.is_some() { 0 } else {
                        px("padding-left") + px("padding-right")
                            + if kw(cs, "border-left-style") == "none" { 0 } else { px("border-left-width") }
                            + if kw(cs, "border-right-style") == "none" { 0 } else { px("border-right-width") }
                    }
                }).unwrap_or(0);
                smn += a + frame;
                smx += b + frame;
                mins.push(a + frame);
            }
            let total_gap = gap * (kids.len() as U).saturating_sub(1);
            // Wrapping lets the line break, so the minimum is one item.
            let wrap = kw(s, "flex-wrap") == "wrap";
            let mn_out = if wrap { mins.into_iter().max().unwrap_or(0) } else { smn + total_gap };
            self.links.truncate(links);
            self.unimplemented = counts;
            self.pending_abs.truncate(pend);
            return (mn_out, (smx + total_gap).max(mn_out));
        }
        for c in if anon { vec![] } else { self.dom.children_of(h) } {
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
                Item::Atomic(ah, ..) => {
                    let w = self.atomic_outer_w(*ah, 0);
                    // ★ A shrink-to-fit inline-block wraps INSIDE itself, so
                    // its min-content contribution is its own min-content,
                    // not its max-content. Taking the max made every
                    // inline-block an unbreakable slab: MDN's table-of-contents
                    // link "Visual layout of table contents" set the whole
                    // column's minimum at 234 px, 26 more than it had.
                    mn = mn.max(self.atomic_min_outer_w(*ah).unwrap_or(w));
                    line += w;
                }
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
        self.pending_abs.truncate(pend);
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
                          // ★ `overflow-wrap` only applies WHERE BREAKING IS
                          // ALLOWED (CSS Text 3 §5.5). Under `nowrap` there is
                          // no such place, so break-word must not smuggle one
                          // in — MDN's nav buttons are `nowrap` inside a narrow
                          // box, and breaking them rendered one character per
                          // line, vertically, down the page.
                          break_word: kw(s, "overflow-wrap") == "break-word" && kw(s, "white-space") != "nowrap",
                          tab: match s.get("tab-size") { V::Int(n) => (*n).clamp(0, 64), _ => 8 } };
        let sp = |p: &str| match s.get(p) { V::Len(l) => u(l.v), V::Calc(c) => calc_eval(c, 0).map(u).unwrap_or(0), _ => 0 };
        (FontStyle { family, bold: weight >= 600, em, size, rgba: color, link: None,
                     ls: sp("letter-spacing"), ws: sp("word-spacing"), tnum: kw(s, "font-variant-numeric") == "tabular-nums" }, line)
    }

    /// Turn an inline-level node (and its inline descendants) into items.
    /// Lay out the out-of-flow boxes `collect_inline` set aside, at the
    /// static position of the line box they were written in.
    fn place_pending_abs(&mut self, x: U, y: U) {
        while let Some(h) = self.pending_abs.pop() { self.abs_box(h, x, y) }
    }

    fn collect_inline(&mut self, h: Handle, out: &mut Vec<Item>, parent: &Style) {
        match self.dom.get(h).map(|n| &n.kind) {
            Some(Kind::Text(t)) => {
                let t = t.clone();
                self.text_items(&t, parent, None, out);
            }
            Some(Kind::Element(tag)) => {
                let tag = tag.clone();
                let Some(s) = self.st(h) else { return };
                if kw(s, "display") == "none" { return }
                // ★ Out of flow: it is not part of this line. Placed once the
                // line box it was written in has been laid out, so its static
                // position is that line's origin.
                if matches!(kw(s, "position"), "absolute" | "fixed") { self.pending_abs.push(h); return }
                // An atomic inline: measured and placed by `lines`.
                if kw(s, "display") == "inline-block" { let (st, l) = self.font_style(s); out.push(Item::Atomic(h, st, l)); return }
                // ★ A replaced element is an atomic inline whatever its
                // `display` says (short of `none`): it has no inline content
                // to flow, only a box of its own declared size.
                if tag == "img" { let (st, l) = self.font_style(s); out.push(Item::Atomic(h, st, l)); return }
                if tag == "br" { out.push(Item::Break); return }
                let link = (tag == "a").then(|| self.dom.attr(h, "href")).flatten().map(|href| { self.links.push(href.to_string()); self.links.len() - 1 });
                let before = out.len();
                for c in self.dom.children_of(h) { self.collect_inline(c, out, s) }
                if let Some(li) = link {
                    for it in &mut out[before..] {
                        if let Item::Word(_, st, _) | Item::Space(st, _) | Item::Atomic(_, st, _) = it { st.link = Some(li) }
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
        struct Placed { x: U, st: FontStyle, line: Line, pieces: Vec<crate::Piece>, gap: bool,
                        /// Width of the collapsible space folded in BEFORE
                        /// this item. Bidi needs it as an item of its own,
                        /// because reordering moves what is between words.
                        gap_w: U,
                        /// Bidi embedding level (UAX#9); 0 until resolved.
                        level: u8,
                        /// How far this item advances the line: the shaped
                        /// width, or an atomic inline's margin-box width.
                        adv: U,
                        /// An atomic inline: (handle, border-box width,
                        /// baseline from its top margin edge, outer height).
                        atomic: Option<(Handle, U, U, U)> }
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
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces, gap: false, gap_w: 0, level: 0, adv: ww, atomic: None });
                    cx += ww;
                    pending = None;
                }
                Item::Atomic(ah, st, l) => {
                    // An atomic inline wraps as ONE unbreakable unit; it is
                    // sized against the line, then laid out for real only
                    // once alignment has fixed where the line starts.
                    let adv = self.atomic_outer_w(ah, w);
                    let cs = self.st(ah).expect("styled");
                    let (ml, mr) = (len(cs.get("margin-left"), w).unwrap_or(0), len(cs.get("margin-right"), w).unwrap_or(0));
                    let bw = (adv - ml - mr).max(0);
                    let sw = match (&pending, lines.last().map(|l| l.is_empty())) {
                        (Some((ps, _)), Some(false)) => self.sh.shape(" ", ps, &mut self.report).iter().map(|p| p.width).sum(),
                        _ => 0,
                    };
                    let wrap = cx + sw + adv > w && !lines.last().expect("line").is_empty();
                    if wrap { lines.push(vec![]); forced.push(false); cx = 0 } else { cx += sw }
                    let asc = self.baseline(ah, w, (Some(bw), None));
                    let outer_h = self.measure(ah, w, None, (Some(bw), None));
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces: vec![], gap: !wrap && sw > 0,
                                                                  gap_w: if wrap { 0 } else { sw }, level: 0,
                                                                  adv, atomic: Some((ah, bw, asc, outer_h)) });
                    cx += adv;
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
                            lines.last_mut().expect("line").push(Placed { x: cx, st: st.clone(), line: l, pieces, gap: false, gap_w: 0, level: 0, adv: cw, atomic: None });
                            cx += cw;
                            i = j;
                            if i < chars.len() { lines.push(vec![]); forced.push(false); cx = 0 }
                        }
                        pending = None;
                        continue;
                    }
                    if ww > w { self.report.overflow_lines += 1 }
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces, gap: !wrap && sw > 0, gap_w: if wrap { 0 } else { sw }, level: 0, adv: ww, atomic: None });
                    cx += ww;
                    pending = None;
                }
            }
        }
        // `direction` gives the line's base level and maps start/end to an
        // edge (profile row 48; CSS Writing Modes).
        let base_rtl = kw(block, "direction") == "rtl";
        let align = match (kw(block, "text-align"), base_rtl) {
            ("start", true) | ("end", false) => "end",
            ("start", false) | ("end", true) => "start",
            (a, _) => a,
        };
        let n_lines = lines.len();
        let mut yy = y;
        for (li, mut line) in lines.into_iter().enumerate() {
            if line.is_empty() {
                let (st, l) = self.font_style(block);
                let _ = st; yy += l.lh; continue
            }
            // Ascent above the baseline and descent below it, per item: for
            // text, half-leading around the font's own metrics; for an atomic
            // inline, its baseline and what is under it (CSS 2 §10.8).
            let metrics: Vec<(U, U)> = line.iter().map(|p| match p.atomic {
                Some((_, _, asc, outer)) => (asc, outer - asc),
                None => { let (a, d) = self.sh.asc_desc(&p.st); let asc = (p.line.lh - a - d) / 2 + a; (asc, p.line.lh - asc) }
            }).collect();
            let base = metrics.iter().map(|m| m.0).max().unwrap_or(0);
            // The line box holds every item: at least each item's own line
            // height, and always enough for the tallest ascent plus the
            // deepest descent.
            // ★ Bidi (UAX#9). Only a line that needs it is touched, so an
            // LTR document's goldens cannot move — and that is the control:
            // every existing golden must stay byte-identical.
            if base_rtl || line.iter().any(|p| p.pieces.iter().any(|pc| pc.text.chars().any(is_rtl))) {
                // The line's text in logical order, with each folded space
                // made an item of its own: reordering moves what is BETWEEN
                // words, so a space cannot stay glued to one of them.
                let mut logical = String::new();
                let mut spans: Vec<(bool, usize, usize)> = vec![]; // (is the space, start, end)
                for p in &line {
                    if p.gap_w > 0 { let st = logical.len(); logical.push(' '); spans.push((true, st, logical.len())) }
                    let st = logical.len();
                    for pc in &p.pieces { logical.push_str(&pc.text) }
                    if p.atomic.is_some() { logical.push('\u{fffc}') } // object replacement: neutral, like an image
                    spans.push((false, st, logical.len()));
                }
                let info = unicode_bidi::BidiInfo::new(&logical, Some(if base_rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() }));
                let level_at = |i: usize| info.levels.get(i).map(|l| l.number()).unwrap_or(0);
                // Rebuild the line with the spaces as items, each carrying
                // its own level. A word whose own text spans two levels is
                // split and re-shaped at the boundary — itemization, which a
                // real engine does before shaping.
                let mut rebuilt: Vec<Placed> = vec![];
                let mut src = line.into_iter();
                let mut iter = spans.into_iter();
                while let Some((is_space, st, en)) = iter.next() {
                    // A space span is handled with the word it preceded,
                    // which carries its width.
                    if is_space { continue }
                    let p = src.next().expect("one span per placed");
                    if p.gap_w > 0 {
                        let sp = self.sh.shape(" ", &p.st, &mut self.report);
                        rebuilt.push(Placed { x: 0, st: p.st.clone(), line: p.line, pieces: sp, gap: p.gap, gap_w: 0,
                                              level: level_at(st.saturating_sub(1)), adv: p.gap_w, atomic: None });
                    }
                    // Level runs inside this item's own text.
                    let text: String = p.pieces.iter().map(|pc| pc.text.clone()).collect();
                    let mut runs: Vec<(u8, String)> = vec![];
                    for (off, ch) in text.char_indices() {
                        let l = level_at(st + off);
                        match runs.last_mut() {
                            Some((rl, s0)) if *rl == l => s0.push(ch),
                            _ => runs.push((l, ch.to_string())),
                        }
                    }
                    if runs.len() <= 1 || p.atomic.is_some() {
                        rebuilt.push(Placed { level: level_at(st), gap: false, gap_w: 0, ..p });
                    } else {
                        for (l, t) in runs {
                            let pieces = self.sh.shape(&t, &p.st, &mut self.report);
                            let adv = pieces.iter().map(|pc| pc.width).sum();
                            rebuilt.push(Placed { x: 0, st: p.st.clone(), line: p.line, pieces, gap: false, gap_w: 0, level: l, adv, atomic: None });
                        }
                    }
                    let _ = en;
                }
                // L2: from the highest level down to the lowest odd one,
                // reverse every contiguous run at or above it.
                let max = rebuilt.iter().map(|p| p.level).max().unwrap_or(0);
                let min_odd = rebuilt.iter().map(|p| p.level).filter(|l| l % 2 == 1).min().unwrap_or(max + 1);
                let mut lvl = max;
                while lvl >= min_odd && lvl > 0 {
                    let mut i = 0;
                    while i < rebuilt.len() {
                        if rebuilt[i].level >= lvl {
                            let mut j = i;
                            while j < rebuilt.len() && rebuilt[j].level >= lvl { j += 1 }
                            rebuilt[i..j].reverse();
                            i = j;
                        } else { i += 1 }
                    }
                    lvl -= 1;
                }
                // Visual order fixed: lay the advances out left to right.
                let mut px = if li == 0 { first_indent } else { 0 };
                for p in &mut rebuilt { p.x = px; px += p.adv }
                line = rebuilt;
            }
            let lh = line.iter().map(|p| if p.atomic.is_some() { 0 } else { p.line.lh }).max().unwrap_or(0)
                .max(base + metrics.iter().map(|m| m.1).max().unwrap_or(0));
            let last = line.last().expect("non-empty");
            let used = last.x + last.adv;
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
            // ★ Decorations extend only over CONTIGUOUS decorated items. The
            // index of the item each open decoration last covered: an item
            // without that decoration in between ends it. Matching on colour
            // alone bridged two links on one line into one underline running
            // through the plain text between them — MDN's "CSS property sets
            // how the content of a replaced element", all underlined.
            let mut deco_last: Vec<usize> = vec![];
            for (pi, p) in line.iter().enumerate() {
                let mut px = x + shift + p.x;
                let start = px;
                // visibility: hidden — the space is taken (positions are
                // already fixed), nothing is painted, and hidden text is not
                // hit-testable, so it contributes no link region either.
                if p.line.hidden { continue }
                // ★ `font-size: 0` paints nothing and takes no space — it is
                // how pages hide text from sight while keeping it for a
                // screen reader. Emitting the run anyway put rustdoc's
                // hidden "Copy item path" into the scene, and a consumer
                // scaling glyphs by zero drew them at the font's own units:
                // a grey blob 450 px across.
                if p.st.size <= 0 { continue }
                if let Some((ah, bw, asc, _)) = p.atomic {
                    // Its margin-box origin: the baseline of this line, minus
                    // the box's own baseline.
                    self.block(ah, px, yy + base - asc, w, None, (Some(bw), None));
                    px += p.adv;
                }
                for piece in &p.pieces {
                    self.scene.run(Run { face: piece.face, size: p.st.size, rgba: p.st.rgba, x: px, y: yy + base, em: p.st.em,
                                         glyphs: piece.glyphs.clone(), text: piece.text.clone() });
                    px += piece.width;
                }
                let thick = (p.st.size / 16).max(PX);
                // ★ A decoration is NOT drawn across an atomic inline (CSS
                // Text Decoration 3 §2.1): an inline-block's own text is
                // decorated by its own line boxes, inside it. Drawing the line
                // across the whole margin box as well underlined MDN's table
                // of contents through each link's padding — "_Try it_".
                let (under, strike) = if p.atomic.is_some() { (None, None) } else { (p.line.underline, p.line.strike) };
                for (c, dy) in [(under, thick), (strike, -scale(p.st.size, 1, 3))] {
                    let Some(c) = c else { continue };
                    let yline = yy + base + dy;
                    let open = deco.iter().zip(&deco_last)
                        .position(|(d, last)| d.0 == c && d.1 == yline && d.4 == thick && last + 1 == pi);
                    match open {
                        Some(k) => { deco[k].3 = px; deco_last[k] = pi }
                        None => { deco.push((c, yline, start, px, thick)); deco_last.push(pi) }
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
    /// ★ `font-size: 0` is how a page hides text from sight while keeping it
    /// for a screen reader. It paints nothing and takes no space.
    #[test]
    fn zero_size_text_is_not_painted() {
        let o = render(r#"<style>.h { font-size: 0 }</style><p>seen<span class="h">hidden</span></p>"#);
        let texts: Vec<&str> = o.scene.runs.iter().map(|r| r.text.as_str()).collect();
        assert!(texts.contains(&"seen"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("hidden")), "zero-size text is not in the scene: {texts:?}");
    }

    /// ★ `overflow-wrap: break-word` applies only where breaking is allowed.
    /// Under `nowrap` there is nowhere, so a long word in a narrow box
    /// OVERFLOWS — it does not break into one character per line.
    #[test]
    fn nowrap_beats_break_word() {
        let o = render(r#"<style>p { width: 12px; white-space: nowrap; overflow-wrap: break-word }</style><p>HTML</p>"#);
        let ys: Vec<U> = o.scene.runs.iter().map(|r| r.y).collect();
        assert_eq!(ys.len(), 1, "one run, one line: {ys:?}");
        // Control: the same box WITHOUT nowrap does break, so the test is
        // about nowrap and not about the width.
        let c = render(r#"<style>p { width: 12px; overflow-wrap: break-word }</style><p>HTML</p>"#);
        assert!(c.scene.runs.len() > 1, "break-word alone still breaks: {}", c.scene.runs.len());
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

    /// ★ NATURAL SIZING with optional parts (CSS Images 3 §5). A width with
    /// no height and no ratio keeps its width under `max-height`; a ratio
    /// alone fills the default object size; a ratio survives min/max by
    /// CSS 2.1 §10.4; and an image declaring nothing is still refused.
    #[test]
    fn images_size_from_whatever_natural_sizing_they_declare() {
        let img = |attrs: &str, css: &str| {
            let o = render(&format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }} img {{ display: block; background-color: #ff0000; {css} }}</style>
                <img src="a.png" {attrs}>"#));
            let b = boxes(&o, 0xff0000ff).first().map(|r| (r.2, r.3)).unwrap_or((0, 0));
            (b, o.diagnostics.len())
        };
        // WPT's w100.svg: width 100, no height, no ratio. Under max-height
        // 70 a browser keeps the width; a (100, 150) ratio would shrink it.
        assert_eq!(img(r#"width="100""#, "max-height: 70px").0, (100, 70));
        // No constraint: the missing height is the default 150.
        assert_eq!(img(r#"width="100""#, "").0, (100, 150));
        // A ratio alone: the largest 2:1 box inside 300×150.
        assert_eq!(img(r#"natural-ratio="2/1""#, "").0, (300, 150));
        assert_eq!(img(r#"natural-ratio="1/1""#, "").0, (150, 150));
        // Both dimensions: their ratio, kept by §10.4 under max-width.
        assert_eq!(img(r#"width="200" height="100""#, "max-width: 100px").0, (100, 50));
        // …unless the declaration says there is none.
        assert_eq!(img(r#"width="200" height="100" natural-ratio="none""#, "max-width: 100px").0, (100, 100));
        // Declaring nothing is still refused (§3.13).
        assert_eq!(img("", "").1, 1);
    }

    /// ★ Absolutely positioned auto margins (CSS 2.1 §10.3.7, §10.6.4): with
    /// both offsets and the size given, they take what is left — the
    /// centring idiom. Found by WPT's box-sizing-003.
    #[test]
    fn absolute_auto_margins_take_the_remaining_space() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .cb { position: relative; width: 400px; height: 200px }
            .c { position: absolute; left: 0px; right: 0px; top: 0px; bottom: 0px; width: 100px; height: 50px;
                 margin-left: auto; margin-right: auto; margin-top: auto; margin-bottom: auto; background-color: #ff0000 }</style>
            <div class="cb"><div class="c"></div></div>"#;
        let o = render(src);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(150, 75, 100, 50)], "centred both ways");
        // Control: with zero margins it sits at the corner.
        let c = render(&src.replace("margin-left: auto; margin-right: auto; margin-top: auto; margin-bottom: auto;", ""));
        assert_eq!(boxes(&c, 0xff0000ff), vec![(0, 0, 100, 50)]);
    }

    /// ★ A border box is never smaller than its padding and border — in
    /// height as well as width. Found by WPT's box-sizing-026.
    #[test]
    fn a_border_box_is_at_least_its_frame_vertically() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .b { width: 10px; height: 10px; border-top-width: 50px; border-bottom-width: 50px; border-left-width: 50px; border-right-width: 50px;
                 border-top-style: solid; border-bottom-style: solid; border-left-style: solid; border-right-style: solid;
                 border-top-color: #00ff00; border-bottom-color: #00ff00; border-left-color: #00ff00; border-right-color: #00ff00 }
            .after { height: 10px; background-color: #ff0000 }</style><div class="b"></div><div class="after"></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff)[0].1, 100, "the next box starts below a 100px-tall border box, not 10px");
    }

    /// ★ COLSPAN. A navbox: one title cell across both columns, then label
    /// and list side by side. Columns are declared by `<col>`, because a
    /// first row that spans cannot declare them.
    #[test]
    fn a_spanning_title_row_and_col_declared_columns() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            table { border-spacing: 10px 0px } .c0 { width: 100px } .c1 { width: 300px }
            .t { background-color: #ff0000 } .l { background-color: #00ff00 } .v { background-color: #0000ff }</style>
            <table><colgroup><col class="c0"><col class="c1"></colgroup>
            <tr><th class="t" colspan="2">Title</th></tr>
            <tr><th class="l">People</th><td class="v">Bob Fabry, Keith Bostic</td></tr></table>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (t, l, v) = (boxes(&o, 0xff0000ff), boxes(&o, 0x00ff00ff), boxes(&o, 0x0000ffff));
        assert_eq!((t[0].0, t[0].2), (10, 100 + 10 + 300), "the title spans both columns AND the spacing between them");
        assert_eq!((l[0].0, l[0].2), (10, 100));
        assert_eq!((v[0].0, v[0].2), (10 + 100 + 10, 300), "the list sits beside its label, in column 2");
        // Control: the same table without <col> cannot declare its columns
        // from a first row that spans, and is refused — not guessed at.
        let c = render(&src.replace(r#"<colgroup><col class="c0"><col class="c1"></colgroup>"#, ""));
        assert!(c.diagnostics.iter().any(|d| d.code == "table.column-width-undeclared" && d.msg.contains("<col>")), "{:?}", c.diagnostics);
    }

    /// ★ Bare text inside a flex container is an anonymous flex item and
    /// counts toward the container's intrinsic width.
    #[test]
    fn text_in_a_flex_item_counts_toward_its_width() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            nav { display: flex } a { display: flex; padding-left: 10px; padding-right: 10px; white-space: nowrap }</style>
            <nav><a href="x">Search</a><a href="y">Submit</a><a href="z">Donate</a></nav>"#);
        let x = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.x).unwrap();
        assert!(x("Submit") - x("Search") > 50 * PX, "each link is as wide as its word plus padding");
        assert!(x("Donate") - x("Submit") > 50 * PX);
    }

    /// ★ The root's (and body's) overflow applies to the VIEWPORT: a page
    /// with `html { height: 100%; overflow-y: scroll }` scrolls; it is not
    /// cut to one screen. A div with the same style IS clipped (control).
    #[test]
    fn root_overflow_scrolls_the_viewport_instead_of_clipping() {
        let long = "<p>line</p>".repeat(80);
        let o = render(&format!("<style>html {{ height: 100%; overflow-y: scroll }}</style>{long}<p>LAST</p>"));
        let last = o.scene.runs.iter().position(|r| r.text == "LAST").expect("text");
        assert!(o.scene.run_attrs[last].0.is_none(), "the last line is not clipped away by the root");
        let c = render(&format!("<style>div {{ height: 300px; overflow-y: scroll }}</style><div>{long}<p>LAST</p></div>"));
        let last = c.scene.runs.iter().position(|r| r.text == "LAST").expect("text");
        assert!(c.scene.run_attrs[last].0.is_some(), "an ordinary box with the same style DOES clip");
    }

    /// ★ HTML's own hiding: `[hidden]` and a closed `<dialog>` are not
    /// painted, and author CSS still overrides them (as in a browser).
    #[test]
    fn hidden_elements_and_closed_dialogs_are_not_painted() {
        let o = render(r#"<p>shown</p><div hidden="true">modal</div><dialog>closed</dialog><dialog open>opened</dialog>
            <style>.force { display: block }</style><div class="force" hidden>author-wins</div>"#);
        let text: Vec<&str> = o.scene.runs.iter().map(|r| r.text.as_str()).collect();
        assert!(text.contains(&"shown") && text.contains(&"opened"), "{text:?}");
        assert!(!text.contains(&"modal") && !text.contains(&"closed"), "{text:?}");
        assert!(text.contains(&"author-wins"), "author display beats the UA [hidden] rule: {text:?}");
    }

    /// ★ An item placed past the explicit grid GROWS the grid (CSS Grid
    /// §8.5). Sizing the fixed axis from the template alone left an item in
    /// column 2 of a one-column grid with no slot, and the placement search
    /// spun forever — FT's error page hung the renderer. Run on a thread so a
    /// regression fails this test instead of hanging the suite.
    #[test]
    fn an_item_past_the_explicit_grid_grows_it_and_does_not_hang() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
                .g { display: grid; column-gap: 10px }
                .a { grid-column-start: 1 } .b { grid-column-start: 2 }</style>
                <div class="g"><div class="a">key</div><div class="b">value</div><div class="a">k2</div><div class="b">v2</div></div>"#);
            let x = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| (r.x, r.y)).unwrap();
            let _ = tx.send((x("key"), x("value"), x("k2"), x("v2")));
        });
        let (key, value, k2, v2) = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("placement terminates");
        assert!(value.0 > key.0 && value.1 == key.1, "value sits BESIDE its key, in column 2");
        assert!(k2.1 > key.1 && v2.1 == k2.1 && v2.0 == value.0, "the second pair is the next row");
    }

    /// ★ THE CANVAS: with no root background, the BODY's paints the whole
    /// canvas and its own box paints nothing (CSS Backgrounds 3 §2.11.2).
    /// example.com is `body { background: #eee; width: 60vw }` — a grey
    /// page, not a grey column on white.
    #[test]
    fn the_body_background_paints_the_canvas() {
        let src = r#"<style>body { background-color: #eeeeee; width: 300px; margin-left: 100px }</style>
            <body><p>x</p></body>"#;
        let o = render(src);
        let grey = boxes(&o, 0xeeeeeeff);
        assert_eq!(grey.len(), 1, "one canvas fill, not the canvas AND the body box: {grey:?}");
        assert_eq!((grey[0].0, grey[0].1, grey[0].2), (0, 0, 800), "it covers the canvas, not the 300 px body");
        assert!(grey[0].3 >= 600, "at least one viewport tall");
        // Control: the same colour on an ordinary element stays on its box.
        let c = render(&src.replace("body { background-color: #eeeeee;", "p { background-color: #eeeeee } body {"));
        let p = boxes(&c, 0xeeeeeeff);
        assert_eq!((p.len(), p[0].0, p[0].2), (1, 100, 300), "a <p> keeps its own box: {p:?}");
    }

    /// ★ Two links on one line get two underlines: the plain text between
    /// them is not decorated, however alike the links are.
    #[test]
    fn plain_text_between_two_links_is_not_underlined() {
        let o = render(r##"<style>body { margin-left: 0px; margin-top: 0px }</style>
            <p>The <a href="#a">first</a> plain words between <a href="#b">second link</a> end.</p>"##);
        let under: Vec<_> = rects(&o).into_iter().filter(|r| r.rgba == 0x0969daff).collect();
        assert_eq!(under.len(), 2, "one underline per link, none across the gap: {under:?}");
        let plain = o.scene.runs.iter().find(|r| r.text.contains("plain")).expect("plain text").x;
        assert!(under.iter().all(|u| u.x + u.w <= plain || u.x > plain), "no underline crosses the plain text");
    }

    /// ★ An auto-width inline-block wraps inside itself, so its min-content
    /// contribution is its OWN min-content. Treating it as an unbreakable
    /// slab at its max-content width pushed MDN's table of contents 26 px
    /// out of its column.
    #[test]
    fn an_inline_block_contributes_its_min_content_not_its_max() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .row { display: flex; width: 120px }
            .nav { background-color: #ff0000; height: 10px }
            .ib { display: inline-block }</style>
            <div class="row"><div class="nav"><span class="ib">several short words that wrap</span></div></div>"#;
        let o = render(src);
        let nav = boxes(&o, 0xff0000ff);
        assert_eq!(nav.len(), 1);
        assert!(nav[0].2 <= 120, "the item shrinks into its 120 px line: {}", nav[0].2);
        // Control: an unbreakable word of the same length does NOT fit, and
        // the item keeps its min-content width, overflowing.
        let c = render(&src.replace("several short words that wrap", "severalshortwordsthatcannotwrap"));
        assert!(boxes(&c, 0xff0000ff)[0].2 > 120, "the control really is wider than the line");
    }

    /// ★ An inline-block's underline is under its TEXT, drawn by its own
    /// line boxes — never across its padding by the line that holds it.
    #[test]
    fn an_inline_block_link_is_underlined_under_its_text_only() {
        let src = r##"<style>body { margin-left: 0px; margin-top: 0px }
            a { display: inline-block; padding-left: 20px; padding-right: 20px; color: #0969da }</style>
            <div><a href="#x">Try it</a></div>"##;
        let o = render(src);
        let under: Vec<_> = rects(&o).into_iter().filter(|r| r.rgba == 0x0969daff).collect();
        let run = o.scene.runs.iter().find(|r| r.text.contains("Try")).expect("the link text");
        assert_eq!(under.len(), 1, "exactly one underline: {under:?}");
        assert_eq!(under[0].x, run.x, "it starts where the text does, inside the padding");
        // Control: the same link INLINE (padding is still there) is underlined
        // once as well — the change is about atomic inlines only.
        let c = render(&src.replace("display: inline-block; ", ""));
        assert_eq!(rects(&c).into_iter().filter(|r| r.rgba == 0x0969daff).count(), 1);
    }

    /// ★ A `max-height` on a clipping box is an upper bound the content
    /// cannot be painted past, even though the box is not definite-height.
    /// Ignoring it let Wikipedia's table of contents — held in
    /// `max-height: calc(100vh - 48px); overflow-y: auto` — paint its whole
    /// 1390 px straight over the article title below it.
    #[test]
    fn a_max_height_clips_a_scrolling_box() {
        let tall = "<p>x</p>".repeat(40);
        let src = format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }}
            .box {{ max-height: 100px; overflow-x: hidden; overflow-y: auto }}</style>
            <div class="box">{tall}</div>"#);
        let o = render(&src);
        let clip = o.scene.clips.iter().map(|c| c.3).max().expect("the box clips");
        assert!(clip <= 100 * PX, "the clip stops at max-height, not at the content: {}", clip / PX);
        // Control: without `overflow` there is no clip at all, and the same
        // content is painted in full.
        let c = render(&src.replace("overflow-x: hidden; overflow-y: auto", "color: #000000"));
        assert!(c.scene.clips.is_empty(), "the control must not clip");
        assert!(c.scene.runs.len() >= o.scene.runs.len(), "the control keeps at least as much");
    }

    /// ★ An absolutely positioned INLINE element is out of flow: it is
    /// blockified (CSS Display 3 §2.7) and contributes nothing to the line.
    /// Flattening it into the line instead ignored its own width and
    /// `overflow`, which is the whole `.screen-reader-text` idiom — Joel on
    /// Software's two `width: 1px; overflow: hidden` labels came out as
    /// "View menu" and "View sidebar" written across the page.
    #[test]
    fn an_absolutely_positioned_inline_leaves_the_line() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .hidden { position: absolute; width: 1px; height: 1px;
                      overflow-x: hidden; overflow-y: hidden }</style>
            <div>visible<span class="hidden">SECRET</span></div>"#;
        let x_of = |o: &HtmlOut, t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.x);
        let o = render(src);
        // Out of flow: it starts at the line's ORIGIN, not after the text —
        // and its own 1px box, which is what hides it, now applies.
        assert_eq!(x_of(&o, "SECRET"), Some(0), "the out-of-flow box left the line");
        // Control: in flow, the same span sits after "visible" on the line.
        let c = render(&src.replace("position: absolute;", ""));
        assert!(x_of(&c, "SECRET").unwrap() > 0, "the control must place it in the line");
    }

    /// ★ A flex item's intrinsic width stops at a child's DEFINITE width.
    /// Measuring past one sized GitHub's collapsed file-tree pane
    /// (`width: 0`) at the max-content of the tree inside it: 446 px of
    /// empty space, with the README squeezed into 353 of the 800.
    #[test]
    fn a_definite_width_caps_a_flex_items_intrinsic_width() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px; margin-right: 0px }
            .row { display: flex; width: 800px }
            .main { flex-basis: 0; flex-grow: 1; height: 20px; background-color: #ff0000 }
            .wrap { display: flex; width: auto; height: 20px; background-color: #00ff00 }
            .pane { width: 0px }</style>
            <div class="row"><div class="main"></div>
            <div class="wrap"><div class="pane">a very long piece of text indeed</div></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 800, 20)], "the sized pane leaves all the room to the content");
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(800, 0, 0, 20)]);
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
        // A property the layout reads but cannot paint is still counted.
        let o = render(r#"<style>div { border-top-left-radius: 8px; overflow-x: hidden; overflow-y: hidden; width: 30px; height: 30px }</style><div>x</div>"#);
        assert!(o.unimplemented.contains_key("overflow clip on a rounded box (clipped square)"), "{:?}", o.unimplemented);
    }

    /// ★ §3.13: there is no measurement path. An image the input does not
    /// declare is REFUSED with a diagnostic, never guessed at.
    #[test]
    fn an_undeclared_subresource_is_refused() {
        let o = render(r#"<style>div { width: 50px; height: 50px; background-image: url(cat.png) }</style><div>x</div>"#);
        assert!(o.diagnostics.iter().any(|d| d.code == "input.subresource-not-supplied"), "{:?}", o.diagnostics);
        assert!(o.scene.images.is_empty(), "nothing is painted for it");
    }

    #[test]
    fn loose_text_becomes_an_anonymous_flex_item() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; width: 400px; column-gap: 10px }
            .i { width: 50px; height: 12px; background-color: #ff0000 }</style>
            <div class="f">loose<div class="i"></div></div>"#);
        // The control: the same text wrapped in an explicit item. An
        // anonymous item must place the red box in exactly the same spot.
        let c = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; width: 400px; column-gap: 10px }
            .i { width: 50px; height: 12px; background-color: #ff0000 }</style>
            <div class="f"><div>loose</div><div class="i"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), boxes(&c, 0xff0000ff));
        assert_eq!(o.scene.runs[0].x, c.scene.runs[0].x);
        assert_eq!(boxes(&o, 0xff0000ff).len(), 1);
        assert!(boxes(&o, 0xff0000ff)[0].0 > 0, "the box follows the text, it does not start the line");
    }

    #[test]
    fn an_inline_block_sits_on_the_baseline_and_raises_the_line() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            p { font-size: 16px; line-height: 20px; margin-top: 0px }
            .ib { display: inline-block; width: 60px; height: 30px; background-color: #ff0000 }
            .after { background-color: #00ff00; width: 40px; height: 8px }</style>
            <p>a<span class="ib"></span>b</p><div class="after"></div>"#);
        let ib = boxes(&o, 0xff0000ff);
        assert_eq!(ib.len(), 1);
        assert_eq!((ib[0].2, ib[0].3), (60, 30), "it keeps its own size");
        // An empty inline-block has no line box of its own, so its baseline
        // is its bottom margin edge: it sits ON the text baseline.
        let baseline = o.scene.runs[0].y;
        assert_eq!(ib[0].1 * 64 + 30 * 64, baseline, "bottom edge on the baseline");
        // The text after it is on the same line, to its right.
        assert!(o.scene.runs[1].x >= (ib[0].0 + 60) * 64, "the text after follows the box");
        // The line box grew to hold it: the next block starts below.
        assert!(boxes(&o, 0x00ff00ff)[0].1 >= 30, "line grew past the 20px line-height");
    }

    #[test]
    fn an_inline_block_wraps_as_one_unit() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            p { width: 100px; font-size: 16px }
            .ib { display: inline-block; width: 80px; height: 10px; background-color: #ff0000 }</style>
            <p>wordy<span class="ib"></span></p>"#);
        let ib = boxes(&o, 0xff0000ff);
        // It does not fit beside the word, and it does not break: it wraps
        // whole onto the next line.
        assert_eq!(ib.len(), 1);
        assert_eq!(ib[0].0, 0, "at the start of the second line");
        assert!(ib[0].1 * 64 > o.scene.runs[0].y, "below the first line");
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

    /// Clipping and group opacity are ATTRIBUTES of a node, so they survive
    /// the re-sort that painting order does.
    #[test]
    fn overflow_clips_to_the_padding_box_and_opacity_groups() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .box { width: 100px; height: 40px; padding-left: 10px; border-left-width: 5px; border-left-style: solid;
                   border-left-color: #000000; overflow-x: hidden; overflow-y: hidden; opacity: 0.5 }
            .big { width: 300px; height: 90px; background-color: #ff0000 }</style>
            <div class="box"><div class="big"></div></div>"#);
        // The clip is the padding box: inside the border, NOT inside padding.
        assert_eq!(o.scene.clips, vec![(5 * 64, 0, 95 * 64, 40 * 64)]);
        assert_eq!(o.scene.groups, vec![(128, None)]);
        // The child is clipped and grouped; the box's own border is grouped
        // but NOT clipped by its own clip.
        let red = o.scene.rects.iter().position(|r| r.rgba == 0xff0000ff).expect("child");
        assert_eq!(o.scene.rect_attrs[red], (Some(0), Some(0), None));
        let border = o.scene.rects.iter().position(|r| r.rgba == 0x000000ff).expect("border");
        assert_eq!(o.scene.rect_attrs[border], (None, Some(0), None));
        // The geometry is untouched: clipping is the consumer's job, so the
        // child keeps its full size and the reader can see what was cut.
        assert_eq!(boxes(&o, 0xff0000ff), vec![(15, 0, 300, 90)]);
    }

    #[test]
    fn nested_clips_are_intersected_at_build_time() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .a { width: 200px; height: 100px; overflow-x: hidden; overflow-y: hidden }
            .b { margin-left: 50px; width: 200px; height: 30px; overflow-x: hidden; overflow-y: hidden }</style>
            <div class="a"><div class="b"><div></div></div></div>"#);
        // The inner clip is already intersected with the outer one, so a
        // reader needs no stack: 50..200, not 50..250.
        assert_eq!(o.scene.clips, vec![(0, 0, 200 * 64, 100 * 64), (50 * 64, 0, 150 * 64, 30 * 64)]);
    }

    /// transform is paint-level: the geometry does not move, the node gets a
    /// matrix. Checked against values that must be exact.
    #[test]
    fn transform_is_a_matrix_on_the_node_not_a_move() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .t { width: 100px; height: 40px; background-color: #ff0000;
                 transform-origin: 0px 0px; transform: translate(10px, 5px) }</style>
            <div class="t"></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 100, 40)], "the box itself does not move");
        assert_eq!(o.scene.xforms, vec![[XF_ONE, 0, 0, XF_ONE, 10 * 64, 5 * 64]]);
        let red = o.scene.rects.iter().position(|r| r.rgba == 0xff0000ff).expect("box");
        assert_eq!(o.scene.rect_attrs[red].2, Some(0));

        // A quarter turn about the box's centre: the matrix is exact.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .t { width: 100px; height: 100px; background-color: #ff0000; transform: rotate(90deg) }</style>
            <div class="t"></div>"#);
        // rotate 90 about (50, 50): (x, y) -> (100 - y, x).
        assert_eq!(o.scene.xforms, vec![[0, XF_ONE, -XF_ONE, 0, 100 * 64, 0]]);

        // transform-origin as a percentage of the box.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .t { width: 100px; height: 40px; background-color: #ff0000; transform-origin: 100% 100%; transform: scale(2, 2) }</style>
            <div class="t"></div>"#);
        // Scaling by 2 about (100, 40): (x, y) -> (2x - 100, 2y - 40).
        assert_eq!(o.scene.xforms, vec![[2 * XF_ONE, 0, 0, 2 * XF_ONE, -100 * 64, -40 * 64]]);
    }

    /// A nested transform is composed with its ancestor's, so a reader needs
    /// no stack — the same rule as clips.
    #[test]
    fn nested_transforms_are_composed() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .a { width: 100px; height: 100px; transform-origin: 0px 0px; transform: translate(10px, 0px) }
            .b { width: 50px; height: 50px; background-color: #ff0000; transform-origin: 0px 0px; transform: translate(5px, 0px) }</style>
            <div class="a"><div class="b"></div></div>"#);
        assert_eq!(o.scene.xforms, vec![[XF_ONE, 0, 0, XF_ONE, 10 * 64, 0], [XF_ONE, 0, 0, XF_ONE, 15 * 64, 0]]);
        let red = o.scene.rects.iter().position(|r| r.rgba == 0xff0000ff).expect("box");
        assert_eq!(o.scene.rect_attrs[red].2, Some(1));
    }

    /// A shadow is a node of its own, BEHIND the box, inflated by the
    /// spread on every side — corner radii included.
    #[test]
    fn box_shadow_is_a_node_behind_the_box() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .s { width: 100px; height: 40px; background-color: #ffffff; border-top-left-radius: 8px;
                 box-shadow: 4px 6px 10px 2px #0000007f }</style>
            <div class="s"></div>"#);
        assert_eq!(o.scene.shadows.len(), 1);
        let sh = &o.scene.shadows[0];
        // Offset by (4, 6), inflated by the 2px spread on every side.
        assert_eq!((sh.x, sh.y, sh.w, sh.h), (2 * 64, 4 * 64, 104 * 64, 44 * 64));
        assert_eq!((sh.blur, sh.rgba), (10 * 64, 0x0000007f));
        // radius + spread, and a square corner stays square.
        assert_eq!(sh.radii, [10 * 64, 0, 0, 0]);
        // It paints before the box's own background.
        let first = o.scene.order.first().copied();
        assert_eq!(first, Some((3, 0)), "the shadow is painted first: {:?}", o.scene.order);
        // visibility: hidden paints neither the box nor its shadow.
        let h = render(r#"<style>.s { width: 100px; height: 40px; visibility: hidden; box-shadow: 4px 6px 10px 2px #000000 }</style><div class="s"></div>"#);
        assert!(h.scene.shadows.is_empty());
    }

    /// A gradient is a tiled paint source: the tile comes from
    /// `background-size`, its place from `background-position-*`, and the
    /// painted area is the border box while the POSITIONING area is the
    /// padding box.
    #[test]
    fn background_gradient_geometry() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { width: 200px; height: 100px; border-left-width: 10px; border-left-style: solid; border-left-color: #000000;
                 background-image: linear-gradient(to right, #ff0000, #0000ff) }</style>
            <div class="g"></div>"#);
        let g = &o.scene.grads[0];
        // Painted over the whole border box; the tile fills the padding box,
        // which starts 10px in and is 190 wide.
        assert_eq!((g.area.x, g.area.y, g.area.w, g.area.h), (0, 0, 200 * 64, 100 * 64));
        assert_eq!((g.area.tx, g.area.ty, g.area.tw, g.area.th), (10 * 64, 0, 190 * 64, 100 * 64));
        assert_eq!(g.angle, 90 * 64, "to right");
        assert_eq!(g.stops, vec![(0xff0000ff, 0), (0x0000ffff, 1024)]);

        // size, position and repeat.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { width: 200px; height: 100px; background-size: 50px; background-position-x: 100%; background-position-y: center;
                 background-repeat: no-repeat; background-image: linear-gradient(#ff0000, #00ff00, #0000ff) }</style>
            <div class="g"></div>"#);
        let g = &o.scene.grads[0];
        // A gradient has no intrinsic ratio, so one value sizes the width and
        // the height stays the area's.
        assert_eq!((g.area.tw, g.area.th), (50 * 64, 100 * 64));
        // 100% across: the tile's right edge on the area's right edge.
        assert_eq!((g.area.tx, g.area.ty), (150 * 64, 0));
        assert_eq!(g.area.repeat, 0);
        // The middle stop with no position gets the even share.
        assert_eq!(g.stops, vec![(0xff0000ff, 0), (0x00ff00ff, 512), (0x0000ffff, 1024)]);
    }

    /// §3.13's two halves, together: `premeasure_tables` declares the
    /// columns offline, and the layout then distributes them — so a table
    /// that would have been REFUSED renders, and its columns follow the
    /// content rather than being equal.
    #[test]
    fn premeasure_makes_a_refused_table_render() {
        let src = r#"<style>table { display: table } tr { display: table-row } td { display: table-cell }</style>
            <table><tr><td>x</td><td>a much longer cell than the first</td></tr></table>"#;
        let fonts = FontSet::load().unwrap();
        // Control: undeclared columns are refused, and nothing is painted.
        let before = render(src);
        assert!(before.diagnostics.iter().any(|d| d.code == "table.column-width-undeclared"));
        let (fixed, n) = premeasure_tables(src, &fonts, &Env::default());
        assert_eq!(n, 2, "two columns measured");
        let after = render_html(&fixed, &fonts, &Env::default());
        assert!(after.diagnostics.is_empty(), "{:?}", after.diagnostics);
        // The measured columns are not equal: the second holds more text.
        let runs: Vec<&crate::Run> = after.scene.runs.iter().collect();
        assert!(runs.len() >= 2);
        let first_x = runs[0].x;
        let second_x = runs.iter().map(|r| r.x).filter(|x| *x > first_x).min().expect("second column");
        assert!(second_x - first_x < 200 * 64, "the narrow column stays narrow: {}", (second_x - first_x) / 64);
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
        // Every row is read now, so the control supplies its own read set
        // with one row (39, text-align) held out. Without this the test
        // would pass while testing nothing.
        let dom = navigator_dom::parse(r#"<style>.a { text-align: center } .b { text-align: start }</style><div class="a">x</div><div class="b">y</div>"#);
        let sheets: Vec<_> = dom.by_tag_anywhere("style").into_iter()
            .map(|h| navigator_style::sheet::parse_sheet(&dom.text_content(h)).sheet).collect();
        let styled = cascade(&dom, &sheets, &Env::default());
        let fonts = FontSet::load().unwrap();
        let held_out: Vec<u8> = READ_ROWS.iter().copied().filter(|r| *r != 39).collect();
        let mut cx = Cx { dom: &dom, styles: &styled.styles, sh: Shaper::new(&fonts), scene: Scene::default(),
                          report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None,
                          baseline_probe: None, indent: None, content_dy: 0, table_part: false, promoted: vec![], boxes: std::env::var_os("NSG_DUMP_BOXES").map(|_| vec![]),
                          viewport: (0, None), pos_cb: (0, 0, 0, None), subs: &Subresources::new(),
                          placing_abs: false, pending_abs: vec![], canvas_src: None, abs_origin: None, contexts: vec![], hoists: vec![], diagnostics: vec![] };
        cx.count_rows_outside(&held_out);
        // The non-initial value is counted once; the initial one is not.
        assert_eq!(cx.unimplemented.get("row text-align… (not implemented)"), Some(&1), "{:?}", cx.unimplemented);
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
/// One bottom-up pass: an element is promoted when its own `display` is not
/// block-level but a child is (or was itself promoted). Doing this once is
/// what keeps it O(n) — asking the question per element while laying out
/// would walk the same subtrees again and again.
fn promote_block_in_inline(dom: &Dom, styles: &[Option<Style>]) -> Vec<bool> {
    let n = dom.nodes.len();
    let mut promoted = vec![false; n];
    let blockish = |h: Handle| -> bool {
        styles.get(h as usize).and_then(|s| s.as_ref())
            .is_some_and(|s| matches!(kw(s, "display"), "block" | "flex" | "grid" | "table" | "table-row" | "table-cell"))
    };
    // Children always have a higher index than their parent (the arena is
    // filled as the document is parsed), so one reverse pass suffices.
    for h in (0..n as Handle).rev() {
        let Some(node) = dom.get(h) else { continue };
        if !matches!(node.kind, Kind::Element(_)) { continue }
        let display_none = styles.get(h as usize).and_then(|s| s.as_ref()).is_some_and(|s| kw(s, "display") == "none");
        if display_none || blockish(h) { continue }
        if dom.element_children(h).into_iter().any(|c| blockish(c) || promoted[c as usize]) {
            promoted[h as usize] = true;
        }
    }
    promoted
}

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

/// The matrix for a `transform` list, about `origin` (absolute, 1/64 px).
///
/// The functions apply left to right, so the leftmost is the OUTERMOST:
/// `translate(…) rotate(…)` rotates first and then translates the result.
/// Percentages in `translate()` are of the box's own size (CSS Transforms 1).
fn transform_matrix(list: &[Tf], w: U, h: U, origin: (U, U)) -> Xform {
    let mut m: Xform = [XF_ONE, 0, 0, XF_ONE, 0, 0];
    for tf in list {
        let step: Xform = match tf {
            Tf::Translate(a, b) => [XF_ONE, 0, 0, XF_ONE, len(a, w).unwrap_or(0), len(b, h).unwrap_or(0)],
            Tf::Scale(sx, sy) => [(sx * XF_ONE as f64).round() as i64, 0, 0, (sy * XF_ONE as f64).round() as i64, 0, 0],
            Tf::Rotate(deg) => {
                let (sin, cos) = crate::sin_cos_deg(*deg);
                let (s, c) = ((sin * XF_ONE as f64).round() as i64, (cos * XF_ONE as f64).round() as i64);
                [c, s, -s, c, 0, 0]
            }
        };
        m = crate::compose(m, step);
    }
    // About the origin: translate there, transform, translate back.
    let to = [XF_ONE, 0, 0, XF_ONE, origin.0, origin.1];
    let back = [XF_ONE, 0, 0, XF_ONE, -origin.0, -origin.1];
    crate::compose(crate::compose(to, m), back)
}

/// CSS gradient stops with every position made explicit: a stop with no
/// position takes 0% (first), 100% (last) or an even share of the gap
/// between its positioned neighbours. Positions are in 1/1024.
fn resolve_stops(stops: &[navigator_style::values::Stop], s: &Style) -> Vec<(u32, i64)> {
    let current = match s.get("color") { V::Color(c) => rgba(c, 0x000000ff), _ => 0x000000ff };
    let n = stops.len();
    let mut at: Vec<Option<i64>> = stops.iter().map(|st| st.at.map(|p| (p * 1024.0 / 100.0).round() as i64)).collect();
    if n > 0 {
        if at[0].is_none() { at[0] = Some(0) }
        if at[n - 1].is_none() { at[n - 1] = Some(1024) }
    }
    let mut i = 0;
    while i < n {
        if at[i].is_some() { i += 1; continue }
        // The run of unpositioned stops between two positioned ones.
        let (start, mut end) = (i, i);
        while end < n && at[end].is_none() { end += 1 }
        let (a, b) = (at[start - 1].expect("positioned"), at[end].expect("positioned"));
        for k in start..end { at[k] = Some(a + (b - a) * (k - start + 1) as i64 / (end - start + 1) as i64) }
        i = end;
    }
    // A position never goes backwards (CSS clamps to the previous one).
    let mut last = i64::MIN;
    stops.iter().zip(at).map(|(st, p)| {
        let p = p.expect("resolved").max(last);
        last = p;
        (rgba(&st.color, current), p)
    }).collect()
}

/// Right-to-left by Unicode bidi class: what makes a line need reordering.
fn is_rtl(c: char) -> bool {
    matches!(unicode_bidi::bidi_class(c), unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL)
}

fn size_tracks(tracks: &[navigator_style::values::Track], avail: Option<U>, gap: U, mins: &[U], maxs: &[U]) -> Vec<U> {
    use navigator_style::values::Track;
    // ★ A track has TWO sizing functions, and they are not interchangeable:
    // the minimum gives its BASE size, the maximum gives a GROWTH LIMIT it
    // may expand to if there is room. Taking the maximum as the base — which
    // this did — makes `minmax(0, 48rem)` claim 768px of an 800px grid
    // before anything else is sized, and the other columns collapse to
    // nothing. (CSS Grid §12.4-12.6.)
    let pct = |p: f64| avail.map(|a| u(a as f64 / PX as f64 * p / 100.0)).unwrap_or(0);
    fn get(v: &[U], i: usize) -> U { v.get(i).copied().unwrap_or(0) }
    let base = |t: &Track, i: usize| -> U {
        match t {
            Track::Len(l) => u(l.v),
            Track::Pct(p) => pct(*p),
            Track::MinContent => get(mins, i),
            // `auto` as a MINIMUM is the min-content size.
            Track::Auto => get(mins, i),
            Track::MaxContent => get(maxs, i),
            // With indefinite free space a flexible track sizes to its
            // content (§12.7.1); with definite space it starts at zero and
            // takes its share below.
            Track::Fr(_) => if avail.is_none() { get(maxs, i) } else { 0 },
            Track::MinMax(a, _) => match &**a {
                Track::Len(l) => u(l.v),
                Track::Pct(p) => pct(*p),
                Track::MinContent | Track::Auto => get(mins, i),
                Track::MaxContent => get(maxs, i),
                _ => 0,
            },
        }
    };
    let limit = |t: &Track, i: usize| -> U {
        match t {
            Track::Len(l) => u(l.v),
            Track::Pct(p) => pct(*p),
            Track::MinContent => get(mins, i),
            Track::MaxContent | Track::Auto => get(maxs, i),
            Track::Fr(_) => U::MAX / 4,
            Track::MinMax(_, b) => match &**b {
                Track::Len(l) => u(l.v),
                Track::Pct(p) => pct(*p),
                Track::MinContent => get(mins, i),
                Track::MaxContent | Track::Auto => get(maxs, i),
                Track::Fr(_) => U::MAX / 4,
                _ => get(maxs, i),
            },
        }
    };
    let flex_of = |t: &Track| -> Option<f64> {
        match t {
            Track::Fr(f) => Some(*f),
            Track::MinMax(_, b) => match **b { Track::Fr(f) => Some(f), _ => None },
            _ => None,
        }
    };

    let mut sizes: Vec<U> = tracks.iter().enumerate().map(|(i, t)| base(t, i)).collect();
    let limits: Vec<U> = tracks.iter().enumerate().map(|(i, t)| limit(t, i).max(sizes[i])).collect();
    let Some(a) = avail else { return sizes };
    let gaps = gap * (tracks.len() as U).saturating_sub(1);

    // §12.5 maximize tracks: grow bases toward their growth limits, equally,
    // until the space runs out or every track has reached its limit. Only
    // tracks with a FINITE limit take part — a flexible one is handled next.
    let mut free = a - sizes.iter().sum::<U>() - gaps;
    let finite: Vec<usize> = (0..tracks.len()).filter(|i| flex_of(&tracks[*i]).is_none()).collect();
    while free > 0 {
        let growable: Vec<usize> = finite.iter().copied().filter(|i| limits[*i] > sizes[*i]).collect();
        if growable.is_empty() { break }
        let share = (free / growable.len() as U).max(1);
        let mut spent = 0;
        for i in growable {
            let step = share.min(limits[i] - sizes[i]).min(free - spent);
            sizes[i] += step;
            spent += step;
            if spent >= free { break }
        }
        if spent == 0 { break }
        free -= spent;
    }

    // §12.6 expand flexible tracks: what is left goes to the `fr` tracks, in
    // proportion, and never below the base each already has.
    let fr: f64 = tracks.iter().filter_map(flex_of).sum();
    if fr > 0.0 {
        let fixed: U = (0..tracks.len()).filter(|i| flex_of(&tracks[*i]).is_none()).map(|i| sizes[i]).sum();
        let for_flex = (a - fixed - gaps).max(0);
        for (i, t) in tracks.iter().enumerate() {
            if let Some(f) = flex_of(t) {
                sizes[i] = sizes[i].max(u(for_flex as f64 / PX as f64 * f / fr));
            }
        }
    } else {
        // §12.8 stretch auto tracks: with no flexible track to absorb it,
        // whatever is left is shared by the `auto` ones. The profile admits
        // no content-distribution property for a grid (§3.5), so the
        // distribution is always the initial `normal` and this always
        // applies.
        let is_auto = |t: &Track| match t { Track::Auto => true, Track::MinMax(_, b) => matches!(**b, Track::Auto), _ => false };
        let auto: Vec<usize> = tracks.iter().enumerate().filter(|(_, t)| is_auto(t)).map(|(i, _)| i).collect();
        let left = (a - sizes.iter().sum::<U>() - gaps).max(0);
        if !auto.is_empty() && left > 0 {
            let share = left / auto.len() as U;
            for (n, &i) in auto.iter().enumerate() {
                sizes[i] += if n + 1 == auto.len() { left - share * (auto.len() as U - 1) } else { share };
            }
        }
    }
    sizes
}

