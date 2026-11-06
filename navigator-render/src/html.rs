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
use std::collections::{BTreeMap, HashMap};

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
        // ★ TRUNCATED to the 1/64 px grid, as a browser's LayoutUnit does,
        // not rounded: two `flex-basis: 50%` items in a 701.5 px wrapping row
        // each rounded UP, summed to 1/64 px more than the row, and the
        // second wrapped (PostgreSQL's search box under its version list).
        V::Pct(p) => Some((basis as f64 * p / 100.0).trunc() as U),
        V::Calc(c) => calc_eval(c, basis).map(u),
        _ => None,
    }
}

/// A length that needs no basis: a plain one, or a `calc()` / `min()` /
/// `max()` / `clamp()` with no percentage in it. Measurement must treat
/// both alike — taking only the plain one measured GitHub Docs' search
/// box, `width: calc(8rem * 2 + 0.125rem)`, by its placeholder text, and
/// the Menu button beside it was pushed 22 px off the page.
fn abs_len(v: &V) -> Option<U> {
    match v {
        V::Len(l) => Some(u(l.v)),
        V::Calc(c) if !calc_has_pct(c) => calc_eval(c, 0).map(u),
        _ => None,
    }
}

/// A box's border-box size `size` as an INTRINSIC CONTRIBUTION: clamped by
/// its own `min-width`/`max-width` (CSS Sizing §5.2). A percentage is
/// against the indefinite size being measured, so it does not clamp.
/// A TABLE is never narrower than its content (CSS 2.1 §17.5.2), so its
/// max-width does not cap it.
/// ★ Unclamped, an ad slot's `min-width: 300px` measured its column at the
/// width of the word "Ad", and a fit-content box centred 35 px of it.
fn contribution(cs: &Style, size: U) -> U {
    let lim = |p: &str| abs_len(cs.get(p));
    let table = matches!(cs.get("display"), V::Kw("table"));
    let size = if table { size } else { lim("max-width").map_or(size, |mx| size.min(mx)) };
    lim("min-width").map_or(size, |mn| size.max(mn))
}

/// Whether a size property is a percentage, or a calc() that holds one.
fn pct_size(cs: &Style, p: &str) -> bool {
    match cs.get(p) { V::Pct(_) => true, V::Calc(c) => calc_has_pct(c), _ => false }
}

/// Whether a calc() depends on a percentage.
fn calc_has_pct(c: &navigator_style::values::Calc) -> bool {
    use navigator_style::values::Calc as C;
    match c {
        C::Pct(_) => true,
        C::Num(_) | C::Len(_) => false,
        C::Add(a, b) | C::Sub(a, b) | C::Mul(a, b) | C::Div(a, b) => calc_has_pct(a) || calc_has_pct(b),
        C::Min(v) | C::Max(v) => v.iter().any(calc_has_pct),
        C::Clamp(a, b, d) => calc_has_pct(a) || calc_has_pct(b) || calc_has_pct(d),
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
    styles: &'a [Option<std::rc::Rc<Style>>],
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
    /// ★ Directional isolates (UAX #9 LRI/RLI…PDI): each inline item
    /// carries the index of its PATH here — the isolates it sits in, outer
    /// first, as (id, rtl). Index 0 is no isolate. `iso_cur` is the path
    /// `collect_inline` is inside.
    iso_paths: Vec<Vec<(u32, bool)>>,
    iso_cur: u32,
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
    /// ★ ELEMENT GEOMETRY for the converter's layout reads (legacy-web
    /// §5.4.5.6e), off unless `layout_geometry` asks for it, so the scene
    /// path is untouched. A LOG, so a trial layout (`measure`) can roll back
    /// what it recorded: only the real placement of each box survives.
    geo: Option<GeoLog>,
    /// The chain of inline elements the item being collected sits in, as an
    /// index into `GeoLog::inl_paths` (0: none). Like `iso_cur`.
    inl_cur: u32,
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
    /// What is left of this document's share of vector lines
    /// (`path::DOCUMENT_SEGMENT_SHARE`); past it, SVGs keep the bitmap path.
    vector_budget: usize,
    /// Inside a scroll region: a table in it does not open another (the
    /// outer one already scrolls it, and the compositor's model is flat).
    in_scroll: bool,
    /// Tables already measured by `premeasure_tables`: their (min, max)
    /// content widths, spacing included, for the cells that contain them.
    measured_tables: HashMap<Handle, (U, U)>,
    /// The table whose box opened the scroll region now open, if any.
    scroll_table: Option<Handle>,
    /// ★ FIRST SCREEN ONLY (backend §4.8, bands phase 1): stop laying out
    /// at the first block in normal flow whose top is below this (1/64 px).
    stop_after: Option<U>,
    /// The stop applies here: plain block flow, not inside a flex or grid
    /// container, a table cell, or a trial measurement — whose sizes must
    /// stay exact for the layout above the limit to be the full layout's.
    stop_ok: bool,
    /// The limit handed to a table in normal flow, which stops measuring
    /// rows once they pass it.
    table_stop: Option<U>,
    /// Whether the stop cut anything.
    truncated: bool,
    /// Something skipped could paint above the cut (see the stop).
    stop_unsafe: bool,
    /// The region a box has opened and `block` will close: the attributes
    /// to restore, its clip, its transform, and the content's x, y, width.
    scroll_open: Option<(crate::Attrs, u32, u32, (U, U, U))>,
    /// ★ Absolutely positioned boxes whose placement needs the HEIGHT of a
    /// containing block that is still being laid out (`bottom` without
    /// `top`, a percentage `top` or `height`, both offsets with an auto
    /// height). CSS lays them out once that size is known (CSS 2 §10.6.4);
    /// placed at their static position instead, Spiegel's featured-teaser
    /// overlay (`bottom: 0` in an auto-height card) fell below the card and
    /// over the carousel. Queued with their static position; the positioned
    /// ancestor drains its share when its own height is final.
    deferred_abs: Vec<(Handle, U, U, U, Option<FlexStatic>)>,
    /// A positioned ancestor with an auto height is open: it will drain
    /// `deferred_abs` when it finishes.
    cb_open: bool,
    /// ★ The element whose background colour became the CANVAS's (CSS
    /// Backgrounds 3 §2.11.2): the root's, or — when the root has none —
    /// the body's. Its own box does not paint that colour again.
    canvas_src: Option<Handle>,
    /// Set by `abs_box`: the border-box origin `block()` must use instead of
    /// the one normal flow would give it.
    abs_origin: Option<(U, U)>,
    /// Set by `flex` for the one `abs_box` call it makes: the static position
    /// of an absolutely positioned child of a flex container.
    flex_static: Option<FlexStatic>,
    /// ★ What `measure` has already worked out, keyed by everything its
    /// answer depends on. A flex item is laid out twice — measured for its
    /// cross size, then placed — and so is every item inside it: nested
    /// flex containers cost 2^depth. Twenty of them (WPT's XHTML flexbox
    /// tests, whose `<div/>`s nest when parsed as HTML) never finished.
    /// The value is the height and the transient fields the trial left
    /// behind, so a hit leaves the context exactly as a miss would.
    /// A fresh baseline probe is part of the key and its result part of
    /// the value: an atomic inline is probed for its baseline, measured and
    /// placed, three layouts a level.
    measured: HashMap<(Handle, U, Option<U>, Option<U>, Option<U>, Option<U>, u32, bool, bool), (U, Option<U>, bool, U, Option<Option<U>>)>,
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
    render_inner(html, fonts, env, subs, false).0
}

/// ★ THE FIRST SCREEN (backend §4.8, bands phase 1): the layout of the
/// document down to `y_limit` (1/64 px), stopping at the first block in
/// normal flow whose top is below it, and a table's rows once they pass it.
/// Everything that is laid out is laid out as the full layout lays it out —
/// the stop is off inside flex and grid containers, table cells and trial
/// measurements — so the scene above the limit is the full scene's (checked
/// by `tests/bands.rs` and on the corpus). The second value says whether
/// anything was cut (when not, this IS the full layout).
pub fn render_html_first(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, y_limit: U) -> (HtmlOut, bool) {
    STOP_AFTER.with(|c| c.set(Some(y_limit)));
    let (o, _, _, truncated) = render_inner_t(html, fonts, env, subs, false);
    STOP_AFTER.with(|c| c.set(None));
    // Something skipped could paint above the cut: the page, whole.
    if STOP_UNSAFE.with(|c| c.replace(false)) {
        UNSAFE_FIRST.with(|c| c.set(c.get() + 1));
        return (render_inner_t(html, fonts, env, subs, false).0, false);
    }
    (o, truncated)
}

/// How many first screens on this thread were laid out whole because
/// something skipped could have painted above the cut (for measurement).
pub fn first_screens_laid_out_whole() -> u64 { UNSAFE_FIRST.with(|c| c.get()) }

thread_local! {
    /// The first-screen limit for the next `render_inner` (see `render_html_first`).
    static STOP_AFTER: std::cell::Cell<Option<U>> = const { std::cell::Cell::new(None) };
    static STOP_UNSAFE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static UNSAFE_FIRST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One element's geometry from a layout (legacy-web §5.4.5.6e). Page
/// coordinates at scroll 0, in 1/64 px, as the scene's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geom {
    /// The border box: a block-level or atomic box's own, or the union of an
    /// inline box's line fragments.
    pub x: U, pub y: U, pub w: U, pub h: U,
    /// Border widths and padding, top, right, bottom, left (zero for an
    /// inline box: its fragments already carry what is painted).
    pub border: [U; 4],
    pub padding: [U; 4],
    /// `position` is not `static` (an `offsetParent` candidate).
    pub positioned: bool,
    /// An inline box (its `clientWidth` and `clientHeight` are 0).
    pub inline: bool,
    /// What `getClientRects()` returns: one rect per line an inline box is
    /// on, or the border box of any other.
    pub fragments: Vec<(U, U, U, U)>,
}

/// A document's geometry, keyed by the value of a marker attribute the
/// caller put on its elements before handing the document over (the
/// normalizer keeps attributes, and the caller's own node ids survive
/// normalization that way where node positions do not).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geometry {
    /// The scene's extent: the document's scroll size.
    pub width: U,
    pub height: U,
    pub elements: BTreeMap<u64, Geom>,
}

#[derive(Default)]
struct GeoLog {
    /// (element, border box, border, padding, positioned) per block-level
    /// or atomic box, in the order laid out: the LAST entry for an element
    /// is its real placement.
    blocks: Vec<(Handle, [U; 4], [U; 4], [U; 4], bool)>,
    /// (inline element, x, y, w, h) per placed item it contains.
    frags: Vec<(Handle, U, U, U, U)>,
    /// Inline element chains, outermost first; index 0 is the empty chain.
    inl_paths: Vec<Vec<Handle>>,
    /// Inline elements that are positioned (`position: relative` on a span).
    inl_positioned: std::collections::BTreeSet<Handle>,
}

/// ★ LAYOUT GEOMETRY for the converter (legacy-web §5.4.5.6e): the same
/// layout `render_html_with` paints, with every element's box recorded and
/// keyed by its `marker` attribute. Elements that were not laid out
/// (`display: none`, or inside one) are absent — the caller answers them as
/// a browser does, with zeros. The scene path does not change: a render
/// with geometry off records nothing and paints the same bytes.
pub fn layout_geometry(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, marker: &str) -> Geometry {
    let (out, log, dom) = render_inner(html, fonts, env, subs, true);
    let log = log.expect("geometry was asked for");
    let key = |h: Handle| dom.attr(h, marker).and_then(|v| v.parse::<u64>().ok());
    let mut elements: BTreeMap<u64, Geom> = BTreeMap::new();
    for (h, b, border, padding, positioned) in &log.blocks {
        let Some(k) = key(*h) else { continue };
        elements.insert(k, Geom { x: b[0], y: b[1], w: b[2], h: b[3], border: *border, padding: *padding,
                                  positioned: *positioned, inline: false, fragments: vec![(b[0], b[1], b[2], b[3])] });
    }
    // Inline boxes: one fragment per line (items on one line share its y),
    // the bounding box their union. An element with a box of its own (an
    // inline-block) keeps that box.
    let mut lines: BTreeMap<u64, (Handle, BTreeMap<(U, U), (U, U)>)> = BTreeMap::new();
    for &(h, x, y, w, ht) in &log.frags {
        let Some(k) = key(h) else { continue };
        if elements.contains_key(&k) { continue }
        let e = lines.entry(k).or_insert((h, BTreeMap::new())).1.entry((y, ht)).or_insert((x, x + w));
        *e = (e.0.min(x), e.1.max(x + w));
    }
    for (k, (h, ls)) in lines {
        let fragments: Vec<(U, U, U, U)> = ls.iter().map(|(&(y, ht), &(x0, x1))| (x0, y, x1 - x0, ht)).collect();
        let x0 = fragments.iter().map(|f| f.0).min().unwrap_or(0);
        let y0 = fragments.iter().map(|f| f.1).min().unwrap_or(0);
        let x1 = fragments.iter().map(|f| f.0 + f.2).max().unwrap_or(0);
        let y1 = fragments.iter().map(|f| f.1 + f.3).max().unwrap_or(0);
        elements.insert(k, Geom { x: x0, y: y0, w: x1 - x0, h: y1 - y0, border: [0; 4], padding: [0; 4],
                                  positioned: log.inl_positioned.contains(&h), inline: true, fragments });
    }
    Geometry { width: out.scene.width, height: out.scene.height, elements }
}

fn render_inner(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, geo: bool) -> (HtmlOut, Option<GeoLog>, Dom) {
    let (o, g, d, _) = render_inner_t(html, fonts, env, subs, geo);
    (o, g, d)
}

fn render_inner_t(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, geo: bool) -> (HtmlOut, Option<GeoLog>, Dom, bool) {
    // ★ At another text size the pre-measured columns are measured AGAIN,
    // exactly (backend §4.9, `remeasure_tables`).
    let remeasured;
    let html = if env.text_scale_64 != 64 && html.contains("atrium-premeasured") {
        remeasured = remeasure_tables(html, fonts, env);
        remeasured.as_str()
    } else { html };
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
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None, baseline_probe: None, indent: None, iso_paths: vec![vec![]], iso_cur: 0, content_dy: 0, table_part: false, promoted: vec![], boxes: std::env::var_os("NSG_DUMP_BOXES").map(|_| vec![]), geo: None, inl_cur: 0,
                      viewport: (u(env.width_px), Some(u(env.height_px))), pos_cb: (0, 0, u(env.width_px), Some(u(env.height_px))),
                      subs, placing_abs: false, pending_abs: vec![], deferred_abs: vec![], cb_open: false, canvas_src: None, abs_origin: None, flex_static: None, measured: HashMap::new(), contexts: vec![], hoists: vec![], diagnostics: vec![], vector_budget: crate::path::DOCUMENT_SEGMENT_SHARE, in_scroll: false, scroll_open: None, measured_tables: HashMap::new(), scroll_table: None, stop_after: None, stop_ok: true, table_stop: None, truncated: false, stop_unsafe: false };
    if geo { cx.geo = Some(GeoLog { inl_paths: vec![vec![]], ..Default::default() }) }
    cx.stop_after = STOP_AFTER.with(|c| c.get());
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
    let log = cx.geo.take();
    let truncated = cx.truncated;
    STOP_UNSAFE.with(|c| c.set(cx.stop_unsafe));
    let out = HtmlOut { scene: cx.scene, report: cx.report, diagnostics, unimplemented: cx.unimplemented };
    (out, log, dom, truncated)
}

/// ★ §3.13's offline half: measure every table column's min-content and
/// max-content width and DECLARE them on the first row's cells, so the
/// renderer never has to. This lives here because the measurement must use
/// the same shaper and the same pinned font set the renderer uses — which is
/// also why the font set version belongs in the normalizer's cache key.
///
/// Returns the rewritten HTML and how many columns were measured.
pub fn premeasure_tables(html: &str, fonts: &FontSet, env: &Env) -> (String, usize) {
    // Always at text size 100%: the renderer scales the result (§4.9), so
    // the normalizer's output does not depend on the reader's text size.
    premeasure_tables_at(html, fonts, &Env { text_scale_64: 64, ..env.clone() })
}

/// The same measurement at `env`'s own text scale: what an exact re-measure
/// at another text size gives (`remeasure_tables`).
pub fn premeasure_tables_at(html: &str, fonts: &FontSet, env: &Env) -> (String, usize) {
    let dom = navigator_dom::parse(html);
    let mut sheets: Vec<Stylesheet> = vec![];
    for h in dom.by_tag_anywhere("style") {
        let p = parse_sheet(&dom.text_content(h));
        if p.refused.is_none() { sheets.push(p.sheet) }
    }
    let styled = cascade(&dom, &sheets, env);
    let mut cx = Cx { dom: &dom, styles: &styled.styles, sh: Shaper::new(fonts), scene: Scene { width: u(env.width_px), ..Default::default() },
                      report: Report::default(), links: vec![], unimplemented: BTreeMap::new(), marker: None, baseline_probe: None,
                      indent: None, iso_paths: vec![vec![]], iso_cur: 0, content_dy: 0, table_part: false, promoted: vec![], boxes: std::env::var_os("NSG_DUMP_BOXES").map(|_| vec![]), geo: None, inl_cur: 0, viewport: (u(env.width_px), Some(u(env.height_px))),
                      pos_cb: (0, 0, u(env.width_px), Some(u(env.height_px))), subs: &Subresources::new(),
                      placing_abs: false, pending_abs: vec![], deferred_abs: vec![], cb_open: false, canvas_src: None, abs_origin: None, flex_static: None, measured: HashMap::new(), contexts: vec![], hoists: vec![], diagnostics: vec![], vector_budget: crate::path::DOCUMENT_SEGMENT_SHARE, in_scroll: false, scroll_open: None, measured_tables: HashMap::new(), scroll_table: None, stop_after: None, stop_ok: true, table_stop: None, truncated: false, stop_unsafe: false };
    // (table, per-column (min, max), its first row's cells) for every table.
    let mut tables: Vec<(Handle, Vec<(U, U)>, Vec<Handle>)> = vec![];
    // ★ INNER TABLES FIRST: a cell holding a table is as wide as that
    // table's columns, which are known only once it is measured. In
    // document order the outer cell met its nested table unmeasured and
    // came out 1 px short; the nested table overhung it at 200% zoom.
    for t in (0..dom.nodes.len() as Handle).rev() {
        let Some(ts) = cx.st(t) else { continue };
        if kw(ts, "display") != "table" { continue }
        let sx = match ts.get("border-spacing") { V::Pair(a, _) => len(a, 0).unwrap_or(0), _ => 0 };
        // Rows, flattening any wrapper — the same walk the layout does.
        let (rows, _) = cx.table_rows(t);
        // Collected up front: the measuring loop needs `cx` mutably.
        let rows_cells: Vec<Vec<Handle>> = rows.iter().map(|r| dom.element_children(*r).into_iter()
            .filter(|c| cx.st(*c).map(|cs| kw(cs, "display")) == Some("table-cell")).collect()).collect();
        // ★ COLSPAN: a row has as many columns as its cells' spans add up
        // to, and the table as many as its widest row.
        // ★ ROWSPAN: and a cell spanning down from above pushes the cells
        // below it to the right, so the slots decide, not a running sum.
        let slots = cx.slots(&rows_cells);
        let ncols = slots.iter().flatten().map(|(k, n, _)| k + n).max().unwrap_or(0);
        if ncols == 0 { continue }
        let mut mins = vec![0 as U; ncols];
        let mut maxs = vec![0 as U; ncols];
        // Author `<col>` widths are floors, like a cell's (below).
        let author_cols = cx.column_decls(t).ok().unwrap_or_default().into_iter()
            .filter(|c| dom.tag(*c) == Some("col")).collect::<Vec<_>>();
        for (i, c) in author_cols.iter().enumerate().take(ncols) {
            if let Some(cw) = cx.st(*c).and_then(|cs| abs_len(cs.get("width"))) {
                mins[i] = mins[i].max(cw); maxs[i] = maxs[i].max(cw);
            }
        }
        // (first column, span, min, max) of every SPANNING cell, settled
        // after the single-column cells have set their columns.
        let mut spanning: Vec<(usize, usize, U, U)> = vec![];
        for (cells, row_slots) in rows_cells.iter().zip(&slots) {
            for (c, &(k, n, _)) in cells.iter().copied().zip(row_slots) {
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
                let floor = cx.st(c).and_then(|cs| abs_len(cs.get("width"))).unwrap_or(0);
                let (mn, mx) = ((mn + pad).max(floor), (mx + pad).max(floor));
                // ★ A CAPTION (compiled to a cell by the normalizer) does not
                // widen its table past its own longest word: only its
                // min-content counts (CSS 2.1 §17.4).
                let mx = if dom.attr(c, "atrium-caption").is_some() { mn } else { mx };
                if n == 1 {
                    mins[k] = mins[k].max(mn);
                    maxs[k] = maxs[k].max(mx);
                } else {
                    spanning.push((k, n, mn, mx));
                }
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
        let around = sx * (ncols as U + 1);
        cx.measured_tables.insert(t, (mins.iter().sum::<U>() + around, maxs.iter().sum::<U>() + around));
        tables.push((t, mins.into_iter().zip(maxs).collect(), rows_cells.first().cloned().unwrap_or_default()));
    }
    // Declared in document order, as before.
    tables.reverse();
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
    // Marked, so the renderer can scale it at another text size (§4.9).
    dom2.set_attr(style, "atrium-premeasured", "");
    let text = dom2.create(Kind::Text(css));
    dom2.append(style, text);
    let head = dom2.by_tag_anywhere("head").into_iter().next()
        .or_else(|| dom2.by_tag_anywhere("body").into_iter().next())
        .unwrap_or(dom2.root());
    dom2.append(head, style);
    (dom2.serialize(), n)
}

/// ★ TEXT SIZE ON PRE-MEASURED TABLES (backend §4.9): the normalizer's
/// column widths are measured at text size 100%, and at any other size they
/// are measured again, exactly, at that size — the pre-measure sheet is
/// dropped and `premeasure_tables_at` runs on what is left.
///
/// ★★ NOT SCALED. Scaling the 100% widths by the text scale was the first
/// design, and the corpus refuted it (2026-11-06, 1,705 columns on 36 of the
/// 122 pages): at 200% only 31% of columns came within 1 px of an exact
/// re-measure. A column holds parts that do not scale — padding, borders,
/// images, an author's px floor — so most came out too WIDE, and 3.5% (4.8%
/// at 300%) came out more than 1 px too NARROW, which clips their text.
pub fn remeasure_tables(html: &str, fonts: &FontSet, env: &Env) -> String {
    let mut dom = navigator_dom::parse(html);
    for h in dom.by_tag_anywhere("style") {
        if dom.attr(h, "atrium-premeasured").is_some() {
            if let Some(p) = dom.nodes[h as usize].parent { dom.remove_child(p, h); }
        }
    }
    premeasure_tables_at(&dom.serialize(), fonts, env).0
}

/// ★ The static position of an absolutely positioned child of a flex
/// container (CSS Flexbox §4.1) is where it would be as the container's SOLE
/// flex item: the container's content box, with justify-content placing it
/// on the main axis and align-self on the cross axis. Held in physical terms
/// — the axes and an rtl main- or cross-start are resolved by `flex`.
#[derive(Clone, Copy)]
struct FlexStatic { x: U, y: U, w: U, h: U, horiz: Align, vert: Align }

#[derive(Clone, Copy, PartialEq)]
enum Align { Start, Center, End }

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
               tab: i64,
               /// white-space: pre or nowrap — no soft wrap here.
               nowrap: bool,
               /// The directional isolates this item sits in (`Cx::iso_paths`).
               iso: u32 }

impl<'a> Cx<'a> {
    fn st(&self, h: Handle) -> Option<&'a Style> { self.styles.get(h as usize).and_then(|s| s.as_deref()) }
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
        // ★ The out-of-flow boxes still WAITING for the enclosing line box
        // belong to it, not to this box. An inline-block on the same line —
        // or the first of two absolutely positioned siblings, laid out
        // while the second is still pending — placed them at ITS content
        // origin: WPT's align-content references put "one" on the line of
        // "three", 16 px right of its own `left`.
        let outer = std::mem::take(&mut self.pending_abs);
        let opened_here = self.scroll_open.is_none();
        let r = self.block_box(h, x, y, cb_w, cb_h, force);
        debug_assert!(self.pending_abs.is_empty(), "a block places its own out-of-flow boxes");
        self.pending_abs = outer;
        // ★ This box opened a scroll region (`block_box`, a wide table):
        // close it, now that its height is known.
        if opened_here {
            if let Some((saved, clip, xf, (cx, cy, cw))) = self.scroll_open.take() {
                let c = &mut self.scene.clips[clip as usize];
                c.3 = (cy + r).min(c.1.saturating_add(c.3)).saturating_sub(c.1).max(0);
                let mut inner = std::mem::take(&mut self.scene.inner_clips);
                inner.retain(|c| (*c as usize) < self.scene.clips.len());
                inner.sort_unstable(); inner.dedup();
                self.scene.scroll_clip = None;
                self.scene.scrolls.push(crate::scroll::Scroll { clip, xform: xf, content: (cx, cy, cw, r), inner });
                self.scene.cur = saved;
                self.in_scroll = false;
                self.scroll_table = None;
            }
        }
        r
    }

    fn block_box(&mut self, h: Handle, x: U, y: U, cb_w: U, cb_h: Option<U>, force: (Option<U>, Option<U>)) -> U {
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
            self.abs_box(h, x, x + cb_w, y);
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
        // ★ A width IMPOSED by the container (a stretched grid item, a
        // flexed flex item) is the replaced box's used width, and an auto
        // height follows it through the ratio (CSS Sizing 4 §5.1): a 50×50
        // image stretched across a 100 px grid area is 100 tall when its
        // block axis is not stretched too — it stayed 50.
        let imposed_w = force.0.filter(|_| force.1.is_none()).map(|fw| (fw - frame).max(0));
        let repl_used = repl.as_ref().map(|(_, nat)| Self::replaced_used(s, nat, cb_w, cb_h, frame, bt + bb + pt + pb, imposed_w));
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
        // An AUTO-width table is as wide as its columns want, up to what is
        // available: min(available, max-content) — never the whole line,
        // which painted a five-column table's background 780 px wide.
        if kw(s, "display") == "table" {
            if specified_w.is_none() && force.0.is_none() {
                if let Some(max) = self.table_content(h, s, cb_w, true) { w = w.min(max + frame) }
            }
            if let Some(min) = self.table_content(h, s, cb_w, false) { w = w.max(min + frame) }
        }
        // Auto margins take what is left; both auto centres. ★ Never less
        // than nothing (CSS 2.1 §10.3.3): a box WIDER than its containing
        // block treats auto margins as 0 and overflows on the right. Centring
        // it anyway pushed Asahi's 1024px min-width page half off the left
        // edge, where no scrolling reaches.
        let free = (cb_w - w - ml.unwrap_or(0) - mr.unwrap_or(0)).max(0);
        match (ml, mr) {
            (None, None) => ml = Some(free / 2),
            (None, Some(_)) => ml = Some(free),
            _ => {}
        }
        // ★ In a right-to-left containing block it is `margin-left` that
        // gives way when the margins are over-constrained (CSS 2.1 §10.3.3):
        // a sized box sits against the RIGHT edge, and a box wider than its
        // containing block overflows on the LEFT — where an RTL page's
        // scrolling does reach. Laid out left-to-right, an Arabic page's
        // 25-column distance table ran off the right edge from its first
        // column. Only for a box in normal flow: a flex or grid item, a
        // table cell, is placed by its container.
        let cb_rtl = self.dom.get(h).and_then(|n| n.parent).and_then(|p| self.st(p)).is_some_and(|ps| kw(ps, "direction") == "rtl");
        if cb_rtl && force.0.is_none() && (matches!((ml, mr), (Some(_), Some(_))) || cb_w - w - ml.unwrap_or(0) - mr.unwrap_or(0) < 0) {
            ml = Some(cb_w - w - mr.unwrap_or(0));
        }
        let (bx, by) = (x + ml.unwrap_or(0), y + mt);
        let (rel_dx, rel_dy) = if kw(s, "position") == "relative" {
            (len(s.get("left"), cb_w).or_else(|| len(s.get("right"), cb_w).map(|r| -r)).unwrap_or(0),
             vpct("top").or_else(|| vpct("bottom").map(|b| -b)).unwrap_or(0))
        } else { (0, 0) };
        let (bx, by) = (bx + rel_dx, by + rel_dy);
        // `abs_box` has already decided where this box goes.
        let (bx, by) = self.abs_origin.take().unwrap_or((bx, by));
        // ★ A TABLE WIDER THAN ITS CONTAINING BLOCK SCROLLS ON ITS OWN
        // (backend §4.9 item 4). Its layout does not change: the box, its
        // border and background and every cell are painted under a clip of
        // the containing block, moved by a translation the UI owns
        // (`scroll`), so the rest of the page still fits the window. A clip
        // made inside (the table's own `overflow`, a cell's) moves with the
        // content (`Scroll::inner`). Not inside another region, which
        // already moves it. `block` closes the region.
        if kw(s, "display") == "table" && !self.in_scroll && force.0.is_none() && !positioned
            // (an RTL containing block gives way with a NEGATIVE margin-left,
            // so the box is held against what its non-negative margins leave)
            && w > cb_w - ml.unwrap_or(0).max(0) - mr.unwrap_or(0).max(0) && !self.table_rows(h).0.is_empty() {
            {
                let saved = self.scene.cur;
                let clip = self.scene.clip(x, by, cb_w, U::MAX / 4);
                let xf = self.scene.xform([XF_ONE, 0, 0, XF_ONE, 0, 0]);
                self.scene.cur.0 = Some(clip);
                self.scene.cur.2 = Some(xf);
                // ★ Clips made inside move with the content (`Scroll::inner`).
                self.scene.scroll_clip = Some((clip, saved.0));
                self.scene.inner_clips.clear();
                self.in_scroll = true;
                self.scroll_open = Some((saved, clip, xf, (bx, by, w)));
                self.scroll_table = Some(h);
            }
        }
        // A definite height is known BEFORE the children, so they can resolve
        // their own percentages against it.
        let clamp = |v: U| {
            let v = match vpct("max-height") { Some(mx) if !matches!(s.get("max-height"), V::Kw(_)) => v.min(mx), _ => v };
            match vpct("min-height") { Some(mn) if !matches!(s.get("min-height"), V::Kw(_)) => v.max(mn), _ => v }
        };
        let mut spec_h = match s.get("height") { V::Kw(_) => None, V::Pct(_) => cb_h.and_then(|b| len(s.get("height"), b)), v => len(v, 0) }.map(clamp);
        // ★ A table cell's own `height` is a FLOOR, never exact (CSS 2.1
        // §17.5.3: the row is as tall as its tallest cell's content).
        // Taken as exact, Wikipedia's weather boxes — `th { height: calc(16px
        // + …) }` — held a three-line row header to one line, and
        // "(≥ 0.1 mm)" painted over the next row.
        // ★ So is a TABLE's (§17.5.3: "the 'height' property … specifies
        // a minimum"): taken as exact, Samsung's footer terms — a
        // `display: table` of links with `height: 4.44vw` — kept its 36 px
        // while its links wrapped to six lines, and the row after it
        // painted over them.
        let cell_floor = if (part && kw(s, "display") == "table-cell") || matches!(kw(s, "display"), "table" | "inline-table") { spec_h.take() } else { None };
        let definite = force.1.or(repl_used.map(|(_, hh)| hh)).or(spec_h)
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
        // numbered among its `li` siblings from `<ol start>`. Only a BLOCK
        // `li` stands in for a list item: `display: inline-block`, `flex` or
        // `grid` replaces list-item, and the marker goes with it.
        if self.dom.tag(h) == Some("li") && kw(s, "display") == "block" {
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
        let deferred_from = self.deferred_abs.len();
        let saved_open = self.cb_open;
        if positioned { self.cb_open = definite.is_none() }
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
            let so = std::mem::replace(&mut self.stop_ok, false);
            cy += self.grid(h, s, content_x, cy, content_w, child_cb_h);
            self.stop_ok = so;
        } else if kw(s, "display") == "table" && !self.table_rows(h).0.is_empty() {
            let so = std::mem::replace(&mut self.stop_ok, false);
            self.table_stop = if so { self.stop_after } else { None };
            cy += self.table(h, s, content_x, cy, content_w);
            self.table_stop = None;
            self.stop_ok = so;
        } else if kw(s, "display") == "flex" {
            let auto_h = (clamp(0).max(vframe) - vframe, clamp(U::MAX / 4).max(vframe) - vframe);
            let so = std::mem::replace(&mut self.stop_ok, false);
            cy += self.flex(h, s, content_x, cy, content_w, child_cb_h, auto_h);
            self.stop_ok = so;
        } else {
            let mut inline: Vec<Item> = vec![];
            let mut stopped = false;
            for c in self.dom.children_of(h) {
                // ★ First screen only: nothing below the limit is laid out —
                // stopping only BEFORE a block, never inside a paragraph
                // (total-fit line breaking makes a line depend on the words
                // after it), and still laying out what is OUT OF FLOW after
                // the stop: a fixed header written at the end of the
                // document paints at its top (doc28).
                if !stopped && self.stop_ok && inline.is_empty() && self.is_block_level(c) && self.stop_after.is_some_and(|l| cy > l) {
                    stopped = true;
                    self.truncated = true;
                }
                if stopped {
                    // A FIXED box is placed against the viewport, not the
                    // flow above it: laid out. Anything else skipped that
                    // could paint ABOVE the cut — a transform (ynet's
                    // carousel cards are translated up from below it), or
                    // an absolutely positioned box, whose containing block's
                    // height may come from what was skipped — makes this
                    // first screen unsafe, and the caller lays the page out
                    // whole instead.
                    if self.st(c).is_some_and(|cs| kw(cs, "position") == "fixed") {
                        cy += self.block(c, content_x, cy, content_w, child_cb_h, (None, None));
                    } else if self.subtree_moves(c) {
                        self.stop_unsafe = true;
                    }
                    continue;
                }
                if self.is_block_level(c) {
                    let ly = cy;
                    cy += self.lines(std::mem::take(&mut inline), content_x, cy, content_w, s);
                    self.place_pending_abs(content_x, content_x + content_w, ly);
                    cy += self.block(c, content_x, cy, content_w, child_cb_h, (None, None));
                } else {
                    self.collect_inline(c, &mut inline, s);
                }
            }
            let ly = cy;
            cy += self.lines(inline, content_x, cy, content_w, s);
            self.place_pending_abs(content_x, content_x + content_w, ly);
        }
        let content_h = cy - (by + bt + pt);
        let hgt = definite.unwrap_or_else(|| clamp(content_h + pt + pb + bt + bb).max(vframe).max(cell_floor.unwrap_or(0)));
        // The out-of-flow boxes that waited for this height, inside this
        // box's clip like the rest of its content.
        if positioned && self.deferred_abs.len() > deferred_from {
            self.pos_cb.3 = Some((hgt - bt - bb).max(0));
            self.cb_open = false;
            for (d, sx, sr, sy, fs) in self.deferred_abs.split_off(deferred_from) { self.flex_static = fs; self.abs_box(d, sx, sr, sy) }
        }
        self.cb_open = saved_open;
        // The box's own border and background are outside its own clip.
        self.scene.cur.0 = saved_attrs.0;
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
                // ★ An inline SVG as VECTOR SHAPES where all of it can be
                // (navigator-svg `vector`, Fresco's fill op, its limits at
                // this painted scale); otherwise its blob's bitmap, as below.
                let vector = self.dom.attr(h, "atrium-svg").map(|text| {
                    let mut budget = self.vector_budget;
                    let r = crate::path::from_svg(text, (tx, ty, tw.max(1), th.max(1)), &mut budget);
                    if r.is_ok() { self.vector_budget = budget }
                    r
                });
                match vector {
                    Some(Ok(paths)) => {
                        self.count("inline svg drawn as vector shapes");
                        let first = self.scene.paths.len();
                        let n = paths.len();
                        for p in paths { self.scene.insert_path(slot, p); slot += 1 }
                        self.scene.path_fallbacks.push((first, n, crate::Image {
                            area: crate::Tiling { x: content.0, y: content.1, w: content.2, h: content.3, tx, ty, tw: tw.max(1), th: th.max(1), repeat: 0 },
                            address: addr.clone(), fit }));
                    }
                    other => {
                        if let Some(Err(why)) = other { self.count(kept_as_bitmap(why)) }
                        self.scene.insert_image(slot, crate::Image {
                            area: crate::Tiling { x: content.0, y: content.1, w: content.2, h: content.3, tx, ty, tw: tw.max(1), th: th.max(1), repeat: 0 },
                            address: addr, fit });
                        slot += 1;
                    }
                }
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
        if let Some(g) = self.geo.as_mut() { g.blocks.push((h, [bx, by, w, hgt], [bt, br, bb, bl], [pt, pr, pb, pl], positioned)) }
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
    /// `static_x`..`static_r` is the line or content box the box would have
    /// been placed in: its static position is the left end of it, or in an
    /// rtl containing block the RIGHT end (CSS 2.1 §10.3.7).
    fn abs_box(&mut self, h: Handle, static_x: U, static_r: U, static_y: U) {
        let fs = self.flex_static.take();
        let Some(s) = self.st(h) else { return };
        let rtl = self.dom.get(h).and_then(|n| n.parent).and_then(|p| self.st(p)).is_some_and(|ps| kw(ps, "direction") == "rtl");
        let (cbx, cby, cbw, cbh) = if kw(s, "position") == "fixed" {
            (0, 0, self.viewport.0, self.viewport.1)
        } else { self.pos_cb };
        let h_off = |p: &str| len(s.get(p), cbw);
        let v_off = |p: &str| -> Option<U> { match s.get(p) { V::Pct(_) => cbh.and_then(|b| len(s.get(p), b)), v => len(v, 0) } };
        let (left, right) = (h_off("left"), h_off("right"));
        let (top, bottom) = (v_off("top"), v_off("bottom"));
        if cbh.is_none() && self.cb_open && kw(s, "position") != "fixed"
            && (bottom.is_some() || matches!(s.get("top"), V::Pct(_)) || matches!(s.get("height"), V::Pct(_))) {
            self.deferred_abs.push((h, static_x, static_r, static_y, fs));
            return;
        }
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
                // Otherwise shrink-to-fit against what is available:
                // min(max(min-content, available), max-content). ★ The
                // min-content floor is NOT clamped by the available width —
                // an offset like `right: calc(100% + 2px)` leaves none, and
                // a box 0 wide spilled its content over its neighbour.
                _ => {
                    let avail = (cbw - left.or(right).unwrap_or(0) - ml - mr - frame).max(0);
                    frame + mx.min(avail.max(mn))
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
                // Negative: the START margin stays 0 — the left one, or in
                // rtl the right one.
                (true, true) if rtl => ml = free,
                (true, true) => mr = free,
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
        // In rtl an over-constrained box ignores `left` (§10.3.7), and with
        // both offsets auto its RIGHT edge is at the static position.
        let x = match (left, right) {
            (Some(_), Some(r)) if rtl => cbx + cbw - r - mr - w,
            (Some(l), _) => cbx + l + ml,
            (None, Some(r)) => cbx + cbw - r - mr - w,
            _ => match fs {
                Some(f) => match f.horiz {
                    Align::Start => f.x + ml,
                    Align::End => f.x + f.w - mr - w,
                    Align::Center => f.x + (f.w - ml - w - mr) / 2 + ml,
                },
                None if rtl => static_r - mr - w,
                None => static_x + ml,
            },
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
            // ★ Flex end or centre: the box is measured first, like a
            // `bottom` one. Unsafe, as in CSS — an oversized box centred in
            // a smaller container starts above it.
            _ => match fs {
                Some(f) if f.vert != Align::Start => {
                    self.placing_abs = true;
                    let outer = self.measure(h, cbw, cbh, (Some(w), forced_h));
                    if f.vert == Align::End { f.y + f.h - outer + mt } else { f.y + (f.h - outer) / 2 + mt }
                }
                Some(f) => f.y + mt,
                None => static_y + mt,
            },
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
        let (w, hh) = Self::replaced_used(s, &nat, 0, None, fh, fv, None);
        Some(((w - fh).max(0), (hh - fv).max(0)))
    }

    /// ★ An image's NATURAL sizing (CSS Images 3 §5.1): a width, a height and
    /// a ratio, EACH of which may be absent. The document declares them on
    /// the element — `width`, `height`, and `natural-ratio` (`W/H` or `none`)
    /// — and the subresource map names the bytes. Both dimensions and no
    /// `natural-ratio` means the ratio is theirs; one dimension alone means
    /// none. An SVG that states only `width="100"` has no ratio, and giving
    /// it one shrank its width under `max-height` where a browser keeps it.
    /// A replaced element: an `<img>`, or an inline `<svg>` the normalizer
    /// made a standalone blob of (`navigator_normalize::svg_inline`): it
    /// carries `src="blake3:…"` and no children, and paints that blob.
    fn is_replaced(&self, h: Handle) -> bool {
        match self.dom.tag(h) {
            Some("img") => true,
            Some("svg") => self.dom.attr(h, "src").is_some_and(|v| v.starts_with("blake3:")),
            _ => false,
        }
    }

    fn natural(&self, h: Handle) -> Option<(String, Natural)> {
        if !self.is_replaced(h) { return None }
        let src = self.dom.attr(h, "src").unwrap_or("");
        // ★ ZERO IS A DIMENSION (CSS Images 3 §5.1): an image declared 0 wide
        // is 0 wide, not "no width", which would fall to the 300 × 150 default
        // object size. Negative or unparsable is no dimension.
        let dim = |n: &str| self.dom.attr(h, n).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v >= 0.0).map(u);
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
        if !self.is_replaced(h) { return None }
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
    fn replaced_used(s: &Style, nat: &Natural, cb_w: U, cb_h: Option<U>, fh: U, fv: U, imposed_w: Option<U>) -> (U, U) {
        let hv = |p: &str| -> Option<U> { match s.get(p) { V::Kw(_) => None, V::Pct(_) => cb_h.and_then(|b| len(s.get(p), b)), v => len(v, 0) } };
        // ★ With no containing block width (an intrinsic measure), a
        // percentage is not a size — nor is a calc() holding one: Wikipedia's
        // thumbnail image, `max-width: calc(100% - 8px + 2px)`, measured 0
        // wide and its 282 px picture shrank to fit the caption's longest word.
        let wv = |p: &str| -> Option<U> { match s.get(p) { V::Kw(_) => None, V::Pct(_) if cb_w <= 0 => None,
                                                            V::Calc(c) if cb_w <= 0 && calc_has_pct(c) => None, v => len(v, cb_w) } };
        let (sw, sh) = (imposed_w.or_else(|| wv("width").map(|v| (v - fh).max(0))), hv("height").map(|v| (v - fv).max(0)));
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

    /// A cell's `rowspan`: HTML's rules (a non-negative integer, at most
    /// 65534; 0 means "to the end of the table" here, since row groups are
    /// flattened), 1 when absent or invalid.
    fn rowspan(&self, c: Handle) -> usize {
        if !matches!(self.dom.tag(c), Some("td") | Some("th")) { return 1 }
        self.dom.attr(c, "rowspan").and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| if n == 0 { usize::MAX } else { n.min(65534) }).unwrap_or(1)
    }

    /// ★ ROWSPAN. Where each cell sits in the table's grid: (first column,
    /// column span, row span), the HTML table-forming algorithm's slots. A
    /// cell takes the first column not still covered by a cell spanning down
    /// from a row above — without this, every cell after a `rowspan` slid one
    /// column left and the row's last cell fell off the declared columns.
    /// Row spans are clamped to the rows that exist.
    fn slots(&self, rows_cells: &[Vec<Handle>]) -> Vec<Vec<(usize, usize, usize)>> {
        let mut busy: Vec<usize> = vec![];
        let nrows = rows_cells.len();
        rows_cells.iter().enumerate().map(|(r, cells)| {
            let mut k = 0usize;
            let out = cells.iter().map(|c| {
                while busy.get(k).is_some_and(|b| *b > 0) { k += 1 }
                let (n, rs) = (self.colspan(*c), self.rowspan(*c).min(nrows - r));
                if busy.len() < k + n { busy.resize(k + n, 0) }
                for b in &mut busy[k..k + n] { *b = rs }
                let slot = (k, n, rs);
                k += n;
                slot
            }).collect();
            for b in &mut busy { *b = b.saturating_sub(1) }
            out
        }).collect()
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
        let Some(first) = self.table_rows(h).0.first().copied() else { return Ok(vec![]) };
        let cells: Vec<Handle> = self.dom.element_children(first).into_iter()
            .filter(|x| self.st(*x).map(|cs| kw(cs, "display")) == Some("table-cell")).collect();
        if cells.iter().any(|x| self.colspan(*x) > 1) { return Err("spanning first row") }
        Ok(cells)
    }

    /// A table's rows, flattening any wrapper (`tbody`, or whatever the
    /// parser inserted), and the NESTED tables met on the way. ★ A nested
    /// table is its own table, never a wrapper (CSS 2.1 §17.2.1: it sits in
    /// an anonymous row and cell of the outer one). Walked into, its rows
    /// became the outer table's: Wikipedia's `div.navbox { display: table }`
    /// around a real navbox `<table>` adopted that table's spanning title
    /// row, could not declare its columns, and was refused.
    fn table_rows(&self, h: Handle) -> (Vec<Handle>, Vec<Handle>) {
        let (mut rows, mut nested) = (vec![], vec![]);
        let mut stack: Vec<Handle> = self.dom.element_children(h).into_iter().rev().collect();
        while let Some(c) = stack.pop() {
            match self.st(c).map(|cs| kw(cs, "display")) {
                Some("table-row") => rows.push(c),
                Some("table") => nested.push(c),
                Some("none") => {}
                _ => for g in self.dom.element_children(c).into_iter().rev() { stack.push(g) },
            }
        }
        (rows, nested)
    }

    /// The narrowest a table can be: its declared column minimums plus the
    /// spacing around and between them. `None` when the columns are not
    /// declared, in which case the table is refused anyway (§3.13).
    /// The table's min-content (or, with `max`, max-content) width from its
    /// declared columns, spacing included.
    fn table_content(&mut self, h: Handle, s: &Style, cb_w: U, max: bool) -> Option<U> {
        let (sx, _) = match s.get("border-spacing") {
            V::Pair(a, b) => (len(a, cb_w).unwrap_or(0), len(b, 0).unwrap_or(0)),
            _ => (0, 0),
        };
        let cells = self.column_decls(h).ok()?;
        if cells.is_empty() { return None }
        let mut total = sx * (cells.len() as U + 1);
        for c in &cells {
            let cs = self.st(*c)?;
            total += match (len(cs.get("width"), cb_w), len(cs.get("min-width"), cb_w), len(cs.get("max-width"), cb_w)) {
                (Some(w), _, _) => w,
                (None, Some(m), x) => if max { x.unwrap_or(m).max(m) } else { m },
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

    /// A trial layout. The first-screen stop never applies inside one: its
    /// size must be the full layout's.
    fn measure(&mut self, h: Handle, cb_w: U, cb_h: Option<U>, force: (Option<U>, Option<U>)) -> U {
        let so = std::mem::replace(&mut self.stop_ok, false);
        let r = self.measure_inner(h, cb_w, cb_h, force);
        self.stop_ok = so;
        r
    }

    fn measure_inner(&mut self, h: Handle, cb_w: U, cb_h: Option<U>, force: (Option<U>, Option<U>)) -> U {
        // A trial with a side channel open — a list marker waiting for its
        // line, an out-of-flow box being placed, a content offset, a static
        // position — is not memoized; nor read from it.
        let probing = self.baseline_probe == Some(None);
        let key = (h, cb_w, cb_h, force.0, force.1, self.indent, self.iso_cur, self.table_part, probing);
        let pure = self.marker.is_none() && !self.placing_abs
            && self.abs_origin.is_none() && self.content_dy == 0 && self.flex_static.is_none();
        if pure {
            if let Some(&(v, indent, part, dy, probe)) = self.measured.get(&key) {
                (self.indent, self.table_part, self.content_dy) = (indent, part, dy);
                if probing { self.baseline_probe = probe }
                return v;
            }
        }
        let (o, r, n, l) = (self.scene.order.len(), self.scene.rects.len(), self.scene.runs.len(), self.scene.links.len());
        // ★ Every node kind, not just the three that existed when this was
        // written. `order` alone being rolled back hides it — the scene
        // still renders correctly, and quietly carries orphan nodes that
        // nothing points at.
        let (sh, gr_n, im) = (self.scene.shadows.len(), self.scene.grads.len(), self.scene.images.len());
        let (pa, vb) = (self.scene.paths.len(), self.vector_budget);
        let (cl, gr, cur, xf) = (self.scene.clips.len(), self.scene.groups.len(), self.scene.cur, self.scene.xforms.len());
        let diags = self.diagnostics.len();
        let (links, counts, marker, report) = (self.links.len(), self.unimplemented.clone(), self.marker.clone(), self.report.clone());
        let (ctx, hoi) = (self.contexts.len(), self.hoists.len());
        // A trial layout collects and places its OWN out-of-flow boxes (and
        // then throws them away with the rest of the scene); it must not
        // consume the ones the real pass is still holding.
        let pend = std::mem::take(&mut self.pending_abs);
        let dfr = self.deferred_abs.len();
        let geo_len = self.geo.as_ref().map(|g| (g.blocks.len(), g.frags.len()));
        let hgt = self.block(h, 0, 0, cb_w, cb_h, force);
        // ★ A trial's boxes are not where anything is: only the real pass's.
        if let (Some(g), Some((b, f))) = (self.geo.as_mut(), geo_len) { g.blocks.truncate(b); g.frags.truncate(f) }
        self.pending_abs = pend;
        self.deferred_abs.truncate(dfr);
        self.scene.order.truncate(o); self.scene.rects.truncate(r); self.scene.runs.truncate(n); self.scene.links.truncate(l);
        self.scene.rect_attrs.truncate(r); self.scene.run_attrs.truncate(n); self.scene.link_attrs.truncate(l);
        self.scene.shadows.truncate(sh); self.scene.shadow_attrs.truncate(sh);
        self.scene.grads.truncate(gr_n); self.scene.grad_attrs.truncate(gr_n);
        self.scene.images.truncate(im); self.scene.image_attrs.truncate(im);
        self.scene.paths.truncate(pa); self.scene.path_attrs.truncate(pa); self.vector_budget = vb;
        self.scene.path_fallbacks.retain(|(first, _, _)| *first < pa);
        self.scene.clips.truncate(cl); self.scene.groups.truncate(gr); self.scene.cur = cur;
        self.scene.xforms.truncate(xf); self.scene.xform_parents.truncate(xf); self.scene.scrolls.retain(|sc| (sc.xform as usize) < xf); self.scene.inner_clips.retain(|c| (*c as usize) < cl);
        // A trial layout must leave no diagnostics behind: the real one
        // re-emits whatever it finds.
        self.diagnostics.truncate(diags);
        self.links.truncate(links); self.unimplemented = counts; self.marker = marker; self.report = report;
        self.contexts.truncate(ctx); self.hoists.truncate(hoi);
        if pure { self.measured.insert(key, (hgt, self.indent, self.table_part, self.content_dy, self.baseline_probe)); }
        hgt
    }

    /// Flex layout (profile §3.4; CSS Flexbox §9 without `order` or any
    /// `*-reverse` — the profile removes them so visual order is reading
    /// order). Lays the items out inside the container's content box at
    /// (x, y), `w` wide, and returns the content height used.
    ///
    /// Items are the element children and anonymous items around loose
    /// text. `auto` margins count as 0 while lines are built and lengths
    /// flexed, then take the free space (§9.5 step 12, §9.6 step 13).
    ///
    /// `auto_h` bounds the content height when the container's own height is
    /// auto (its min-/max-height, less padding and border): the final size,
    /// which an absolutely positioned child's static position needs.
    fn flex(&mut self, h: Handle, s: &Style, x: U, y: U, w: U, cb_h: Option<U>, auto_h: (U, U)) -> U {
        let row = kw(s, "flex-direction") != "column";
        // ★ The inline axis runs right to left in an rtl container: a ROW's
        // main-start is its right edge (flex-start, flex-end and the item
        // margins with it), and a COLUMN's cross-start is.
        let rtl = kw(s, "direction") == "rtl";
        let wrap = kw(s, "flex-wrap") == "wrap";
        let main_gap = len(s.get(if row { "column-gap" } else { "row-gap" }), if row { w } else { cb_h.unwrap_or(0) }).unwrap_or(0);
        let cross_gap = len(s.get(if row { "row-gap" } else { "column-gap" }), if row { cb_h.unwrap_or(0) } else { w }).unwrap_or(0);
        let main_avail = if row { Some(w) } else { cb_h };
        struct It { h: Handle, base: U, hypo: U, grow: f64, shrink: f64, min: U, max: U, main: U, cross: U,
                    m_start: U, m_end: U, c_start: U, c_end: U, align: &'static str, cross_auto: bool,
                    // Which margins are `auto`, in the same flow-relative order.
                    auto: [bool; 4] }
        let mut items: Vec<It> = vec![];
        // Absolutely positioned children, each with the number of items
        // before it: placed in document order among them, once the
        // container's size is known.
        let mut abs: Vec<(Handle, usize)> = vec![];
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
                                    cross_auto: true, auto: [false; 4] });
                    continue;
                }
                Some(Kind::Element(_)) => {}
                _ => continue,
            }
            let Some(cs) = self.st(c) else { continue };
            if kw(cs, "display") == "none" { continue }
            // Out of flow: not an item, placed against its containing block.
            if matches!(kw(cs, "position"), "absolute" | "fixed") { abs.push((c, items.len())); continue }
            let px = |p: &str, basis: U| len(cs.get(p), basis);
            let (mw, mh) = (if rtl { ["margin-right", "margin-left"] } else { ["margin-left", "margin-right"] }, ["margin-top", "margin-bottom"]);
            let (mm, cm) = if row { (mw, mh) } else { (mh, mw) };
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
            // ★ …unless the item is a SCROLL CONTAINER (Flexbox §4.5):
            // `overflow` other than visible (or clip) makes the automatic
            // minimum 0, which is the whole point of `overflow: hidden` on a
            // flex item. Without it TikTok's 38rem newsroom column would not
            // shrink and ran 270 words off the right edge.
            // ROWS only, for now: in a column the item's height is shared
            // out of a container height this renderer often has to guess
            // (a stretched item in an auto-height row), and a zero minimum
            // there let edX's clamped testimonials collapse to one line.
            // CSS says the same of columns; that waits on definite heights.
            let scroller = row && ["overflow-x", "overflow-y"].iter().any(|p| !matches!(kw(cs, p), "visible" | "clip" | ""));
            let min = match cs.get(minp) {
                V::Kw("auto") if scroller => 0,
                V::Kw("auto") => {
                    let content_min = if row {
                        let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { len(cs.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
                        self.intrinsic(c).0 + len(cs.get("padding-left"), w).unwrap_or(0) + len(cs.get("padding-right"), w).unwrap_or(0) + b("left") + b("right")
                    } else { self.content_main(c, false, w) };
                    // The content size suggestion is clamped by a definite
                    // MAX main size (§4.5): an image's natural 1536 px is no
                    // minimum for an item that says `max-width: 100%`.
                    let content_min = px(maxp, main_avail.unwrap_or(0)).filter(|_| main_avail.is_some() || !matches!(cs.get(maxp), V::Pct(_)))
                        .map_or(content_min, |mx| content_min.min(mx));
                    let specified = if row { len(cs.get("width"), w) } else { match cs.get("height") { V::Pct(_) => None, v => len(v, 0) } };
                    specified.map(|sp| sp.min(content_min)).unwrap_or(content_min)
                }
                _ => px(minp, main_avail.unwrap_or(0)).unwrap_or(0),
            };
            // ★ A TABLE is never narrower than its min-content (CSS 2.1
            // §17.5.2): `block` widens it to that whatever it is given, so a
            // row that shared out less put it OVER its neighbour — ar
            // Wikipedia's 327 px infobox was given 297 and covered the first
            // 30 px of the article beside it.
            let min = if row && kw(cs, "display") == "table" {
                let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { len(cs.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
                let frame = len(cs.get("padding-left"), w).unwrap_or(0) + len(cs.get("padding-right"), w).unwrap_or(0) + b("left") + b("right");
                match self.table_content(c, cs, w, false) { Some(tm) => min.max(tm + frame), None => min }
            } else { min };
            let max = px(maxp, main_avail.unwrap_or(0)).unwrap_or(U::MAX);
            let hypo = base.clamp(min, max.max(min));
            let num = |p: &str| match cs.get(p) { V::Num(n) => *n, _ => 0.0 };
            let align = match kw(cs, "align-self") { "auto" | "" => kw(s, "align-items"), a => a };
            let cross_prop = if row { "height" } else { "width" };
            items.push(It { h: c, base, hypo, grow: num("flex-grow"), shrink: num("flex-shrink"), min, max,
                            main: hypo, cross: 0, m_start: m(mm[0]), m_end: m(mm[1]), c_start: m(cm[0]), c_end: m(cm[1]),
                            align: match align { "flex-start" | "start" => "start", "flex-end" | "end" => "end", "center" => "center",
                                                 "baseline" => "baseline", _ => "stretch" },
                            cross_auto: matches!(cs.get(cross_prop), V::Kw("auto")),
                            auto: [mm[0], mm[1], cm[0], cm[1]].map(|p| matches!(cs.get(p), V::Kw("auto"))) });
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
        // Resolve flexible lengths per line (§9.7): share the free space by
        // the flex factors, clamp, FREEZE the items that violated their
        // min/max, and share again among the rest until nothing moves.
        // ★ One pass and a clamp left a `min-width: 500px` video at 500
        // while its neighbour kept the share computed as if the video had
        // shrunk: LinkedIn's line overflowed the page by 116 px.
        for r in &lines {
            let Some(avail) = main_avail else { continue };
            let line = &mut items[r.clone()];
            let fixed = (line.iter().map(|i| i.m_start + i.m_end).sum::<U>() + main_gap * (line.len() as U).saturating_sub(1)) as f64;
            let growing = line.iter().map(|i| i.hypo).sum::<U>() as f64 + fixed < avail as f64;
            let factor = |i: &It| if growing { i.grow } else { i.shrink };
            let mut frozen: Vec<bool> = line.iter().map(|i| factor(i) == 0.0
                || (growing && i.base > i.hypo) || (!growing && i.base < i.hypo)).collect();
            let mut target: Vec<f64> = line.iter().zip(&frozen).map(|(i, f)| if *f { i.hypo } else { i.base } as f64).collect();
            let free_of = |target: &[f64]| avail as f64 - fixed - target.iter().sum::<f64>();
            let initial = free_of(&target);
            for _ in 0..=line.len() {
                if frozen.iter().all(|f| *f) { break }
                for (k, i) in line.iter().enumerate() { if !frozen[k] { target[k] = i.base as f64 } }
                let mut free = free_of(&target);
                let sum_f: f64 = line.iter().zip(&frozen).filter(|(_, f)| !**f).map(|(i, _)| factor(i)).sum();
                if sum_f < 1.0 && (initial * sum_f).abs() < free.abs() { free = initial * sum_f }
                let scaled: f64 = line.iter().zip(&frozen).filter(|(_, f)| !**f).map(|(i, _)| i.shrink * i.base as f64).sum();
                let mut violation = vec![0.0; line.len()];
                for (k, i) in line.iter().enumerate() {
                    if frozen[k] { continue }
                    let t = if growing { i.base as f64 + free * i.grow / sum_f }
                            else if scaled > 0.0 { i.base as f64 + free * i.shrink * i.base as f64 / scaled }
                            else { i.base as f64 };
                    let c = t.clamp(i.min as f64, i.max.max(i.min) as f64);
                    violation[k] = c - t;
                    target[k] = c;
                }
                let total: f64 = violation.iter().sum();
                for k in 0..line.len() {
                    if frozen[k] { continue }
                    if total == 0.0 || (total > 0.0 && violation[k] > 0.0) || (total < 0.0 && violation[k] < 0.0) { frozen[k] = true }
                }
            }
            for (i, t) in line.iter_mut().zip(&target) { i.main = t.round() as U }
        }
        // Cross sizes: measured at the resolved main size.
        for i in &mut items {
            i.cross = if row { self.measure(i.h, i.main, None, (Some(i.main), None)) - i.c_start - i.c_end }
                      else {
                          // ★ Fit-content in the room the margins leave, and
                          // min/max-width still hold: `max-width: 60rem;
                          // margin: 0 auto` in a column is the centred page
                          // column, and unclamped it was as wide as its text.
                          // A definite width is that width, room or not.
                          let cs = self.st(i.h);
                          let definite = cs.is_some_and(|cs| len(cs.get("width"), w).is_some());
                          let fit = self.intrinsic_outer_w(i.h, w);
                          let fit = if definite { fit } else { fit.min((w - i.c_start - i.c_end).max(0)) };
                          let mx = cs.and_then(|cs| len(cs.get("max-width"), w)).unwrap_or(U::MAX);
                          let mn = cs.and_then(|cs| len(cs.get("min-width"), w)).unwrap_or(0);
                          fit.min(mx).max(mn)
                      };
        }
        let container_cross = if row { cb_h } else { Some(w) };
        let line_cross: Vec<U> = lines.iter().map(|r| {
            let m = items[r.clone()].iter().map(|i| i.cross + i.c_start + i.c_end).max().unwrap_or(0);
            // A single line in a container of definite cross size fills it.
            if lines.len() == 1 && !wrap { container_cross.unwrap_or(m) } else { m }
        }).collect();
        // align-content: distribute the lines in the cross axis.
        let total: U = line_cross.iter().sum::<U>() + cross_gap * (lines.len() as U).saturating_sub(1);
        // ★ `center` and `flex-end` are UNSAFE by default (Box Alignment
        // §4.3): lines taller than the container overflow it on both sides,
        // or on the start side, as the keyword says. Only stretch needs room.
        let (mut cpos, cstep, cstretch) = match (container_cross, lines.len() > 1 || wrap) {
            (Some(cc), true) if cc != total => {
                let free = cc - total;
                match kw(s, "align-content") { "flex-end" => (free, 0, 0), "center" => (free / 2, 0, 0),
                    "stretch" if free > 0 => (0, 0, free / lines.len() as U), _ => (0, 0, 0) }
            }
            _ => (0, 0, 0),
        };
        // align-content: baseline on a flex container that is not itself in
        // a baseline-sharing group falls back to `start` (CSS Box Alignment):
        // laying it out as flex-start IS the specified behaviour.
        let jc = kw(s, "justify-content");
        // ★ The static position of an absolutely positioned child (§4.1):
        // as the SOLE item of this container — justify-content on the main
        // axis (space-between is start, space-around/-evenly centre, as for
        // one item), its own align-self on the cross axis (stretch and
        // baseline are start). Not align-content, even when wrapping: that
        // is what Chrome does. Placed at the content box's start whatever
        // the alignment, The Hindu's 3D carousel — `justify-content:
        // center` around absolutely positioned cards — sat at the left edge.
        let main_extent: U = lines.iter().map(|r| items[r.clone()].iter().map(|i| i.main + i.m_start + i.m_end).sum::<U>()
            + main_gap * (r.len() as U).saturating_sub(1)).max().unwrap_or(0);
        let (box_w, box_h) = if row { (w, cb_h.unwrap_or(total.max(0).min(auto_h.1).max(auto_h.0))) }
                             else { (w, cb_h.unwrap_or(main_extent.min(auto_h.1).max(auto_h.0))) };
        let place_abs = |this: &mut Self, c: Handle| {
            let main = match jc { "flex-end" | "end" => Align::End, "center" | "space-around" | "space-evenly" => Align::Center,
                                  "right" if row => Align::End, _ => Align::Start };
            let al = this.st(c).map(|cs| match kw(cs, "align-self") { "auto" | "" => kw(s, "align-items"), a => a }).unwrap_or("");
            let cross = match al { "flex-end" | "end" | "self-end" => Align::End, "center" => Align::Center, _ => Align::Start };
            let flip = |a: Align| match a { Align::Start => Align::End, Align::End => Align::Start, c => c };
            let (horiz, vert) = if row { (if rtl { flip(main) } else { main }, cross) } else { (if rtl { flip(cross) } else { cross }, main) };
            this.flex_static = Some(FlexStatic { x, y, w: box_w, h: box_h, horiz, vert });
            this.abs_box(c, x, x + w, y);
        };
        let mut next_abs = 0;
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
            // ★ AUTO MARGINS on the main axis take the free space first,
            // shared equally (§9.5 step 12): `margin-left: auto` on the last
            // nav item pushes it to the far end, and justify-content then
            // has nothing left to distribute. On an overflowing line `free`
            // is 0 and so are they.
            let autos: U = items[r.clone()].iter().map(|i| i.auto[0] as U + i.auto[1] as U).sum();
            let per_auto = if autos > 0 { free / autos } else { 0 };
            let (mut mpos, between) = if autos > 0 { (0, 0) } else { match jc {
                "flex-end" => (free, 0), "center" => (free / 2, 0),
                "space-between" if n > 1 => (0, free / (n - 1)),
                "space-around" if n > 0 => (free / n / 2, free / n),
                "space-evenly" if n > 0 => (free / (n + 1), free / (n + 1)),
                _ => (0, 0),
            } };
            for idx in r.clone() {
                while next_abs < abs.len() && abs[next_abs].1 <= idx { place_abs(self, abs[next_abs].0); next_abs += 1 }
                let i = &items[idx];
                let slot = lc - i.c_start - i.c_end;
                // ★ AUTO MARGINS on the cross axis (§8.1, §9.6 step 13) beat
                // align-self and stop the item stretching: both auto centre
                // it, one pushes it to the other side. With no room the
                // start one is 0.
                let (cross, coff) = if i.auto[2] || i.auto[3] {
                    let room = (slot - i.cross).max(0);
                    (i.cross, match (i.auto[2], i.auto[3]) { (true, true) => room / 2, (true, false) => room, _ => 0 })
                } else { match i.align {
                    "stretch" if i.cross_auto => (slot, 0),
                    "end" => (i.cross, slot - i.cross),
                    "center" => (i.cross, (slot - i.cross) / 2),
                    "baseline" => (i.cross, bl.iter().find(|x| x.0 == idx).map(|x| max_b - x.1).unwrap_or(0)),
                    _ => (i.cross, 0),
                } };
                let m0 = mpos + i.m_start + if i.auto[0] { per_auto } else { 0 };
                let c0 = cpos + i.c_start + coff;
                let (ix, iy, fw, fh) = match (row, rtl) {
                    (true, false) => (x + m0, y + c0, i.main, cross),
                    (true, true) => (x + w - m0 - i.main, y + c0, i.main, cross),
                    (false, false) => (x + c0, y + m0, cross, i.main),
                    (false, true) => (x + w - c0 - cross, y + m0, cross, i.main),
                };
                // The item's own margins are applied here, so it is placed
                // with them zeroed: lay out at the margin box's corner minus
                // the margin the block would add.
                let (ml, mt) = self.st(i.h).map(|cs| (len(cs.get("margin-left"), w).unwrap_or(0), len(cs.get("margin-top"), w).unwrap_or(0))).unwrap_or((0, 0));
                self.block(i.h, ix - ml, iy - mt, fw + ml, None, (Some(fw), Some(fh)));
                mpos = m0 + i.main + i.m_end + (if i.auto[1] { per_auto } else { 0 }) + main_gap + between;
            }
            cpos += lc + cross_gap + cstep;
        }
        let _ = cstep;
        for &(c, _) in &abs[next_abs..] { place_abs(self, c) }
        // Content height: a row container is as tall as its lines stacked in
        // the cross axis; a column container as its longest line's main extent.
        if row { total.max(0) } else { main_extent }
    }

    /// Grid layout (profile §3.5; CSS Grid without named lines, `subgrid` or
    /// dense packing — the profile excludes all three). Lays the items out in
    /// the container's content box at (x, y), `w` wide; returns the height.
    /// A grid's items placed in its tracks, and each column's intrinsic
    /// contributions — what layout sizes the columns from, and what
    /// `intrinsic` sums to measure the grid itself. Percentages resolve
    /// against `w` (0 when measuring). Out-of-flow children are returned,
    /// not placed.
    fn grid_plan(&mut self, h: Handle, s: &Style, w: U) -> GridPlan {
        use navigator_style::values::{GridLine, Track};
        let tracks_of = |v: &V| -> Vec<Track> { match v { V::Tracks(t) => t.clone(), _ => vec![] } };
        let cols_tpl = tracks_of(s.get("grid-template-columns"));
        let rows_tpl = tracks_of(s.get("grid-template-rows"));
        let auto_col = match s.get("grid-auto-columns") { V::Track(t) => t.clone(), _ => Track::Auto };
        let auto_row = match s.get("grid-auto-rows") { V::Track(t) => t.clone(), _ => Track::Auto };
        let flow_row = kw(s, "grid-auto-flow") != "column";
        let cgap = len(s.get("column-gap"), w).unwrap_or(0);

        let line = |v: &V| -> GridLine { match v { V::Line(l) => *l, _ => GridLine::Auto } };
        let mut raw = vec![];
        let mut abs = vec![];
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
            // Out of flow: not an item; `grid` places it.
            if matches!(kw(cs, "position"), "absolute" | "fixed") { abs.push(c); continue }
            // ★ Outside a flex container `flex-start` / `flex-end` are
            // `start` / `end` (Box Alignment §6.1), and so are `self-*` while
            // an item's direction is its grid's.
            let pick = |own: &str, parent: &str| -> String {
                match match kw(cs, own) { "auto" | "" => match kw(s, parent) { "" => "stretch", a => a }, a => a } {
                    "flex-start" | "self-start" => "start", "flex-end" | "self-end" => "end", a => a,
                }.to_string()
            };
            raw.push((c, line(cs.get("grid-column-start")), line(cs.get("grid-column-end")),
                      line(cs.get("grid-row-start")), line(cs.get("grid-row-end")),
                      pick("justify-self", "justify-items"), pick("align-self", "align-items")));
        }
        // ★ A NEGATIVE line counts from the END of the explicit grid: -1 is
        // the last line of the template. It was read as "no end", so
        // `grid-column: 1 / -1` (the full-width idiom) spanned ONE column:
        // Notion's footer menus came out in a 24px strip, one word wide.
        // Lines before the first one clamp to it (the profile has no
        // implicit tracks before the explicit grid).
        let (ncols, nrows) = (cols_tpl.len() as i32, rows_tpl.len() as i32);
        let resolve = |l: GridLine, n: i32| match l {
            GridLine::Line(v) if v < 0 => GridLine::Line((n + 2 + v).max(1)),
            other => other,
        };
        for r in raw.iter_mut() {
            r.1 = resolve(r.1, ncols); r.2 = resolve(r.2, ncols);
            r.3 = resolve(r.3, nrows); r.4 = resolve(r.4, nrows);
        }
        // Placement. A definite line is 1-based; `span n` sizes the area.
        let span_of = |a: GridLine, b: GridLine| -> (Option<usize>, usize) {
            match (a, b) {
                (GridLine::Line(s0), GridLine::Line(e)) if e > s0 => (Some((s0 - 1).max(0) as usize), (e - s0) as usize),
                (GridLine::Line(s0), GridLine::Span(n)) => (Some((s0 - 1).max(0) as usize), n.max(1) as usize),
                (GridLine::Line(s0), _) => (Some((s0 - 1).max(0) as usize), 1),
                (GridLine::Auto, GridLine::Line(e)) if e > 1 => (Some((e - 2).max(0) as usize), 1),
                (GridLine::Auto, GridLine::Span(n)) => (None, n.max(1) as usize),
                // ★ The span can sit on EITHER side: `span 4 / auto` is four
                // auto-placed tracks, and `span 2 / 5` ends at line 5. Both
                // used to fall through to one track, and Goodreads' 4-of-6
                // book column came out one sixth wide.
                (GridLine::Span(n), GridLine::Line(e)) => {
                    let n = n.max(1);
                    (Some((e - n - 1).max(0) as usize), n as usize)
                }
                (GridLine::Span(n), _) => (None, n.max(1) as usize),
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
        // ★ Placement runs in the passes of CSS Grid §8.5, not in one sweep
        // in document order: (1) items with a definite position on both
        // axes, (2) items LOCKED to a row (definite major line, auto minor)
        // — each placed past the ones this step already put in that row,
        // (3) the implicit grid's minor extent, (4) everything else, behind
        // one cursor. A locked item was simply given column 1, so Google's
        // header row (every item `grid-row: 1`) stacked in one cell; and
        // a later explicit item must still claim its cell before an
        // earlier auto item can take it.
        let mut occupied: Vec<Vec<bool>> = vec![];
        let free = |occ: &Vec<Vec<bool>>, mj: usize, mn: usize, ms: usize, ns: usize| {
            (0..ms).all(|a| (0..ns).all(|b| !*occ.get(mj + a).and_then(|r: &Vec<bool>| r.get(mn + b)).unwrap_or(&false)))
        };
        let claim = |occ: &mut Vec<Vec<bool>>, mj: usize, mn: usize, ms: usize, ns: usize| {
            while occ.len() < mj + ms { occ.push(vec![]) }
            for a in 0..ms { for b in 0..ns {
                let row = &mut occ[mj + a];
                while row.len() <= mn + b { row.push(false) }
                row[mn + b] = true;
            } }
        };
        // Major axis = the flow axis; minor = the fixed one.
        let axes: Vec<_> = raw.iter().map(|(_, cs0, ce, rs, re, ..)| {
            let (cstart, cspan) = span_of(*cs0, *ce);
            let (rstart, rspan) = span_of(*rs, *re);
            if flow_row { (rstart, cstart, rspan, cspan) } else { (cstart, rstart, cspan, rspan) }
        }).collect();
        let mut at: Vec<Option<(usize, usize)>> = vec![None; raw.len()];
        for (i, &(mj, mn, ms, ns)) in axes.iter().enumerate() {
            if let (Some(mj), Some(mn)) = (mj, mn) { claim(&mut occupied, mj, mn, ms, ns); at[i] = Some((mj, mn)) }
        }
        let mut row_cursor: std::collections::HashMap<usize, usize> = Default::default();
        for (i, &(mj, mn, ms, ns)) in axes.iter().enumerate() {
            let (Some(mj), None) = (mj, mn) else { continue };
            let mut c = *row_cursor.get(&mj).unwrap_or(&0);
            while !free(&occupied, mj, c, ms, ns) { c += 1 }
            claim(&mut occupied, mj, c, ms, ns);
            row_cursor.insert(mj, c + ns);
            at[i] = Some((mj, c));
        }
        // Step 2 may have reached past the template.
        let fixed_len = at.iter().zip(&axes).filter_map(|(a, x)| a.map(|(_, mn)| mn + x.3)).max().unwrap_or(0).max(fixed_len);
        let (mut cursor_major, mut cursor_minor) = (0usize, 0usize);
        for (i, &(mj, mn, ms, ns)) in axes.iter().enumerate() {
            if at[i].is_some() { continue }
            debug_assert!(mj.is_none());
            let (mut mj, mut c) = (cursor_major, cursor_minor);
            match mn {
                // A definite minor line: move on a row if it lies behind
                // the cursor, then down until the area is free.
                Some(m) => {
                    if m < c { mj += 1 }
                    while !free(&occupied, mj, m, ms, ns) { mj += 1 }
                    c = m;
                }
                // Fully auto: along the row, wrapping to the next one.
                None => loop {
                    if c + ns > fixed_len { mj += 1; c = 0; continue }
                    if free(&occupied, mj, c, ms, ns) { break }
                    c += 1;
                },
            }
            claim(&mut occupied, mj, c, ms, ns);
            cursor_major = mj; cursor_minor = c + ns;
            at[i] = Some((mj, c));
        }
        let mut items: Vec<GItem> = vec![];
        for ((c, .., justify, align), (pos, &(_, _, mspan, nspan))) in raw.into_iter().zip(at.into_iter().zip(&axes)) {
            let (mj, mn) = pos.unwrap_or((0, 0));
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
        // ★ Three of them (Grid §12.5): the min- and max-content
        // contributions, and the MINIMUM contribution an `auto` minimum
        // takes — the item's used min-width, so `min-width: 0` (or a scroll
        // container) contributes nothing and only an `auto` min-width is
        // content-based (§6.6). Taking the min-content contribution for all
        // of them laid react.dev's article, `<main class="min-w-0">` around a
        // wide code block, 992 px wide in an 800 px grid.
        let mut cmin = vec![0 as U; coltracks.len()];
        let mut cmax = vec![0 as U; coltracks.len()];
        let mut cauto = vec![0 as U; coltracks.len()];
        // Items spanning several columns, one of them flexible: their
        // contributions go to the flex fraction (§12.7.1), not the tracks.
        let mut cspans: Vec<(usize, usize, U)> = vec![];
        for it in &items {
            let crosses_flex = it.col.1 > 1 && (it.col.0..it.col.0 + it.col.1).any(|i| coltracks.get(i).and_then(flex_of).is_some());
            if it.col.1 != 1 && !crosses_flex { continue }
            let (mn, mx) = self.intrinsic(it.h);
            let (a, b, m) = match self.st(it.h) {
                // Anonymous text: no box, no minimum of its own.
                None => (mn, mx, mn),
                Some(cs) => {
                    let px = |p: &str| len(cs.get(p), w).unwrap_or(0);
                    let bd = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { px(&format!("border-{side}-width")) };
                    let frame = px("padding-left") + px("padding-right") + bd("left") + bd("right");
                    let margins = px("margin-left") + px("margin-right");
                    // A percentage width is cyclic here and behaves as auto.
                    let definite = abs_len(cs.get("width"));
                    let (a, b) = definite.map_or((mn + frame, mx + frame), |d| (d, d));
                    let (a, b) = (contribution(cs, a).max(frame) + margins, contribution(cs, b).max(frame) + margins);
                    let scroller = ["overflow-x", "overflow-y"].iter().any(|p| !matches!(kw(cs, p), "visible" | "clip" | ""));
                    let m = match cs.get("min-width") {
                        _ if definite.is_some() => a,
                        V::Kw("auto") if scroller => frame + margins,
                        V::Kw("auto") => a,
                        // ★ A percentage here is of the grid AREA, which
                        // is what is being sized: cyclic, so it resolves
                        // against zero for the contribution (CSS Sizing
                        // §5.2.1). Taken against the grid's width, BBC
                        // Arabic's `min-width: 50%` footer item claimed
                        // half the grid, and its `repeat(3, 1fr)` came out
                        // 384 / 176 / 176 where Chrome has three of 245.
                        v => len(v, 0).unwrap_or(0).max(frame) + margins,
                    };
                    (a, b, m)
                }
            };
            if it.col.1 != 1 { cspans.push((it.col.0, it.col.1, b)); continue }
            cmin[it.col.0] = cmin[it.col.0].max(a);
            cmax[it.col.0] = cmax[it.col.0].max(b);
            cauto[it.col.0] = cauto[it.col.0].max(m);
        }
        GridPlan { items, abs, coltracks, rowtracks, cmin, cmax, cauto, cgap, cspans }
    }

    fn grid(&mut self, h: Handle, s: &Style, x: U, y: U, w: U, cb_h: Option<U>) -> U {
        use navigator_style::values::Track;
        // ★ In an rtl grid column line 1 is on the RIGHT (CSS Grid §7.1: the
        // column axis is the inline axis), and an item's start is its right.
        let rtl = kw(s, "direction") == "rtl";
        let rgap = len(s.get("row-gap"), cb_h.unwrap_or(0)).unwrap_or(0);
        let GridPlan { items, abs, coltracks, rowtracks, cmin, cmax, cauto, cgap, .. } = self.grid_plan(h, s, w);
        // Out of flow: not items, placed against their containing block —
        // which, when this grid is that block, is the item's GRID AREA (see
        // below, once the tracks are sized).
        let grid_is_cb = kw(s, "position") != "static" && !kw(s, "position").is_empty();
        let cols = size_tracks(&coltracks, Some(w), cgap, &cmin, &cmax, &cauto, &[]);
        // Row sizes: content heights measured at the item's column width.
        let mut rmin = vec![0 as U; rowtracks.len()];
        for it in &items {
            if it.row.1 != 1 { continue }
            let cw: U = (it.col.0..it.col.0 + it.col.1).map(|i| cols.get(i).copied().unwrap_or(0)).sum::<U>() + cgap * (it.col.1 as U - 1);
            let iw = self.grid_item_w(it.h, cw, &it.justify);
            let hgt = self.measure(it.h, cw, None, (Some(iw), None));
            rmin[it.row.0] = rmin[it.row.0].max(hgt);
        }
        // ★ Then items spanning several rows, fewest rows first (Grid
        // §12.5): what their rows and the gaps between them fall short of
        // the item's height is shared among the INTRINSIC rows it spans. Left
        // out, BBC Arabic's `span 2` cards gave their auto rows nothing and
        // the next row of cards drew over them.
        let intrinsic_track = |t: &Track| !matches!(t, Track::Len(_) | Track::Pct(_))
            && !matches!(t, Track::MinMax(a, b) if matches!(**a, Track::Len(_) | Track::Pct(_)) && matches!(**b, Track::Len(_) | Track::Pct(_)));
        let mut spanning: Vec<&GItem> = items.iter().filter(|it| it.row.1 > 1).collect();
        spanning.sort_by_key(|it| it.row.1);
        // With an indefinite height, an item crossing a FLEXIBLE row is not
        // shared out here: it sets the rows' flex fraction (§12.7.1).
        let mut rspans: Vec<(usize, usize, U)> = vec![];
        for it in spanning {
            let rs = it.row.0..(it.row.0 + it.row.1).min(rowtracks.len());
            if cb_h.is_none() && rs.clone().any(|i| flex_of(&rowtracks[i]).is_some()) {
                let cw: U = (it.col.0..it.col.0 + it.col.1).map(|i| cols.get(i).copied().unwrap_or(0)).sum::<U>() + cgap * (it.col.1 as U - 1);
                let iw = self.grid_item_w(it.h, cw, &it.justify);
                rspans.push((rs.start, rs.len(), self.measure(it.h, cw, None, (Some(iw), None))));
                continue;
            }
            let grow: Vec<usize> = rs.clone().filter(|i| intrinsic_track(&rowtracks[*i])).collect();
            if grow.is_empty() { continue }
            let cw: U = (it.col.0..it.col.0 + it.col.1).map(|i| cols.get(i).copied().unwrap_or(0)).sum::<U>() + cgap * (it.col.1 as U - 1);
            let iw = self.grid_item_w(it.h, cw, &it.justify);
            let hgt = self.measure(it.h, cw, None, (Some(iw), None));
            let have: U = rmin[rs.clone()].iter().sum::<U>() + rgap * (rs.len() as U - 1);
            if hgt > have {
                let short = hgt - have;
                for (j, i) in grow.iter().enumerate() {
                    rmin[*i] += short / grow.len() as U + if j + 1 == grow.len() { short % grow.len() as U } else { 0 };
                }
            }
        }
        let mut rows = size_tracks(&rowtracks, cb_h, rgap, &rmin, &rmin, &rmin, &rspans);
        // ★ An indefinite height with a definite MIN-height: the rows are
        // stretched (auto) or expanded (fr) into the min-height's content
        // box instead (Grid §12.7.1, §12.8). Asahi's tab box is
        // `min-height: 216px` over `auto 176px` rows: without it the auto
        // row was 0, and the list started under its own tab headers.
        if cb_h.is_none() {
            let bw = |side: &str| if kw(s, &format!("border-{side}-style")) == "none" { 0 } else { len(s.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
            let frame_v = len(s.get("padding-top"), w).unwrap_or(0) + len(s.get("padding-bottom"), w).unwrap_or(0) + bw("top") + bw("bottom");
            let used = rows.iter().sum::<U>() + rgap * (rows.len() as U).saturating_sub(1);
            if let Some(minh) = len(s.get("min-height"), 0).map(|m| (m - frame_v).max(0)).filter(|m| *m > used) {
                rows = size_tracks(&rowtracks, Some(minh), rgap, &rmin, &rmin, &rmin, &[]);
            }
        }
        let total_h = rows.iter().sum::<U>() + rgap * (rows.len() as U).saturating_sub(1);
        // ★ The STATIC position of an absolutely positioned child (CSS
        // Position 3 §4.2, Box Alignment §5.1): its own justify-self /
        // align-self within the static-position rectangle — the grid's
        // content box, or its grid area when the grid is its containing
        // block. stretch and normal are start; in rtl start is the right.
        let static_align = |this: &Self, c: Handle| -> (Align, Align) {
            let pick = |own: &str, parent: &str| this.st(c).map(|cs| match kw(cs, own) { "auto" | "" => kw(s, parent), a => a }).unwrap_or("").to_string();
            let logical = |v: &str| match v { "end" | "flex-end" | "self-end" => Align::End, "center" => Align::Center, _ => Align::Start };
            let j = pick("justify-self", "justify-items");
            let horiz = match j.as_str() { "left" => Align::Start, "right" => Align::End,
                v => match (logical(v), rtl) { (Align::Start, true) => Align::End, (Align::End, true) => Align::Start, (a, _) => a } };
            (horiz, logical(&pick("align-self", "align-items")))
        };
        let content_h = cb_h.unwrap_or(total_h);
        let mut abs_jobs: Vec<AbsJob> = vec![];
        // ★ An absolutely positioned child of a grid that is its containing
        // block takes its GRID AREA as that block (Grid §9.1): the
        // placement properties name lines, and `auto` (or a line the grid
        // does not have) is the container's padding edge. A START line is
        // its track's start; an END line, the end of the track before it,
        // so the gutter is outside. Abspos children create no tracks.
        {
            use navigator_style::values::GridLine;
            let (pcx, pcy, pcw, pch) = self.pos_cb;
            // An auto height is not known to the caller yet: it is the rows,
            // plus the frame, clamped by min-/max-height (border-box, §3.1).
            let pch = pch.unwrap_or_else(|| {
                let bw = |side: &str| if kw(s, &format!("border-{side}-style")) == "none" { 0 } else { len(s.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
                let (pt, pb) = (len(s.get("padding-top"), w).unwrap_or(0), len(s.get("padding-bottom"), w).unwrap_or(0));
                let mut bh = total_h + pt + pb + bw("top") + bw("bottom");
                if let Some(m) = len(s.get("max-height"), 0) { bh = bh.min(m) }
                if let Some(m) = len(s.get("min-height"), 0) { bh = bh.max(m) }
                (bh - bw("top") - bw("bottom")).max(0)
            });
            let ntpl = |p: &str| match s.get(p) { V::Tracks(t) => t.len() as i32, _ => 0 };
            let (ncol_tpl, nrow_tpl) = (ntpl("grid-template-columns"), ntpl("grid-template-rows"));
            // Line k (0-based) as an offset from the content edge.
            let start_at = |sz: &[U], gap: U, k: usize| sz[..k].iter().sum::<U>() + gap * k as U;
            let end_at = |sz: &[U], gap: U, k: usize| sz[..k].iter().sum::<U>() + gap * (k as U).saturating_sub(1).max(0);
            // (start, end) in content-edge offsets; None = the padding edge.
            let span = |a: GridLine, b: GridLine, sz: &[U], gap: U, ntpl: i32| -> (Option<U>, Option<U>) {
                let n = sz.len() as i32;
                let k = |l: i32| { let l = if l < 0 { ntpl + 2 + l } else { l }; (l >= 1 && l <= n + 1).then_some((l - 1) as usize) };
                let (sa, eb) = match (a, b) {
                    (GridLine::Line(i), GridLine::Line(j)) => (k(i), k(j)),
                    (GridLine::Line(i), GridLine::Span(m)) => (k(i), k(i).map(|v| v + m.max(1) as usize).filter(|v| *v as i32 <= n)),
                    (GridLine::Span(m), GridLine::Line(j)) => (k(j).and_then(|v| v.checked_sub(m.max(1) as usize)), k(j)),
                    (GridLine::Line(i), _) => (k(i), None),
                    (_, GridLine::Line(j)) => (None, k(j)),
                    _ => (None, None),
                };
                (sa.map(|v| start_at(sz, gap, v)), eb.map(|v| end_at(sz, gap, v)))
            };
            for c in abs {
                if !(grid_is_cb && self.st(c).is_some_and(|cs| kw(cs, "position") == "absolute")) {
                    let (horiz, vert) = static_align(self, c);
                    abs_jobs.push((c, None, FlexStatic { x, y, w, h: content_h, horiz, vert }, x, x + w, y));
                    continue;
                }
                let Some(cs) = self.st(c) else { continue };
                let line = |p: &str| match cs.get(p) { V::Line(l) => *l, _ => GridLine::Auto };
                let (c0, c1) = span(line("grid-column-start"), line("grid-column-end"), &cols, cgap, ncol_tpl);
                let (r0, r1) = span(line("grid-row-start"), line("grid-row-end"), &rows, rgap, nrow_tpl);
                let (mut ax0, mut ax1) = (c0.map_or(pcx, |v| x + v), c1.map_or(pcx + pcw, |v| x + v));
                if rtl {
                    // Mirrored: line 1 is the content box's RIGHT edge.
                    ax0 = c1.map_or(pcx, |v| x + w - v);
                    ax1 = c0.map_or(pcx + pcw, |v| x + w - v);
                }
                let (ay0, ay1) = (r0.map_or(pcy, |v| y + v), r1.map_or(pcy + pch, |v| y + v));
                let (aw, ah) = ((ax1 - ax0).max(0), (ay1 - ay0).max(0));
                let (horiz, vert) = static_align(self, c);
                abs_jobs.push((c, Some((ax0, ay0, aw, Some(ah))), FlexStatic { x: ax0, y: ay0, w: aw, h: ah, horiz, vert }, ax0, ax1.max(ax0), ay0));
            }
        }
        // ★ Positioned boxes paint in TREE order (CSS 2.1 Appendix E, layer
        // 6, which `restack` hoists above the in-flow content), so each is
        // laid out where it stands among the items: placed first, a later
        // positioned grid item's abspos child drew under an earlier one.
        let dom_at: HashMap<Handle, usize> = self.dom.children_of(h).into_iter().enumerate().map(|(i, c)| (c, i)).collect();
        let at = |c: &Handle| dom_at.get(c).copied().unwrap_or(usize::MAX);
        let mut abs_jobs = abs_jobs.into_iter().peekable();
        let run_abs = |this: &mut Self, (c, cb, fs, sx, sr, sy): AbsJob| {
            let saved = this.pos_cb;
            if let Some(cb) = cb { this.pos_cb = cb }
            this.flex_static = Some(fs);
            this.abs_box(c, sx, sr, sy);
            this.pos_cb = saved;
        };
        // Place.
        let pos = |sizes: &[U], gap: U, i: usize| -> U { sizes[..i.min(sizes.len())].iter().sum::<U>() + gap * i as U };
        for it in &items {
            while abs_jobs.peek().is_some_and(|j| at(&j.0) < at(&it.h)) { let j = abs_jobs.next().expect("peeked"); run_abs(self, j) }
            let aw = (it.col.0..it.col.0 + it.col.1).map(|i| cols.get(i).copied().unwrap_or(0)).sum::<U>() + cgap * (it.col.1 as U - 1);
            let ax = if rtl { x + w - pos(&cols, cgap, it.col.0) - aw } else { x + pos(&cols, cgap, it.col.0) };
            let ay = y + pos(&rows, rgap, it.row.0);
            let ah = (it.row.0..it.row.0 + it.row.1).map(|i| rows.get(i).copied().unwrap_or(0)).sum::<U>() + rgap * (it.row.1 as U - 1);
            // ★ The item's MARGIN box goes in the area (Grid §11): stretch
            // fills what the margins leave, and end/centre align the margin
            // box. `auto` margins take the free space first and stop the
            // stretch (§11.2), the `margin: 0 auto` idiom. Ignored, a
            // stretched item with margins overflowed its area by them.
            let cs = self.st(it.h);
            let mg = |p: &str| cs.and_then(|cs| len(cs.get(p), aw)).unwrap_or(0);
            let auto = |p: &str| cs.is_some_and(|cs| matches!(cs.get(p), V::Kw("auto")));
            let (ml, mr, mt, mb) = (mg("margin-left"), mg("margin-right"), mg("margin-top"), mg("margin-bottom"));
            let (al, ar, at, ab) = (auto("margin-left"), auto("margin-right"), auto("margin-top"), auto("margin-bottom"));
            let room_h = (ah - mt - mb).max(0);
            let iw = self.grid_item_w(it.h, aw, &it.justify);
            let clamp_by = |v: U, lo: &str, hi: &str, basis: U| {
                let v = cs.and_then(|cs| len(cs.get(hi), basis)).map_or(v, |m| v.min(m));
                cs.and_then(|cs| len(cs.get(lo), basis)).map_or(v, |m| v.max(m))
            };
            let specified_h = cs.and_then(|cs| match cs.get("height") { V::Kw(_) => None, V::Pct(_) => len(cs.get("height"), ah), v => len(v, 0) });
            let ih = match it.align.as_str() {
                "stretch" if !(at || ab) => specified_h.unwrap_or(room_h),
                // `measure` answers with the margins; the box is without them.
                // An auto height is the content's, and may overflow the area.
                _ => specified_h.unwrap_or_else(|| self.measure(it.h, iw, None, (Some(iw), None)) - mt - mb),
            };
            let ih = clamp_by(ih, "min-height", "max-height", ah);
            // ★ A replaced item whose HEIGHT is settled — stretched in the
            // block axis alone, or a height that resolves against the area —
            // takes its auto width from that height through its ratio (Grid
            // §6.2, csswg #5713): a 50×50 image with `align-self: stretch`
            // in a 100 px area is 100 wide, not its natural 50, and a 200×200
            // one at `height: 100%` is 100 wide, not 200.
            // So does any box with an `aspect-ratio`, whose ratio is the
            // BORDER box's here (the profile is border-box, CSS Sizing 4 §2).
            let ratio = self.natural(it.h).and_then(|(_, n)| n.ratio);
            let settled_h = (it.align == "stretch" && !(at || ab)) || specified_h.is_some();
            let box_ratio = match cs.map(|cs| cs.get("aspect-ratio")) { Some(V::Num(r)) if *r > 0.0 && ratio.is_none() => Some(*r), _ => None };
            let iw = match (ratio, cs) {
                (None, Some(cs)) if box_ratio.is_some() && settled_h && it.justify != "stretch" && matches!(cs.get("width"), V::Kw(_)) =>
                    clamp_by(u(ih as f64 / PX as f64 * box_ratio.unwrap_or(1.0)), "min-width", "max-width", aw),
                (Some((rw, rh)), Some(cs)) if settled_h && it.justify != "stretch" && matches!(cs.get("width"), V::Kw(_)) => {
                    let px = |p: &str| len(cs.get(p), aw).unwrap_or(0);
                    let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { px(&format!("border-{side}-width")) };
                    let (fh_, fv_) = (px("padding-left") + px("padding-right") + b("left") + b("right"), px("padding-top") + px("padding-bottom") + b("top") + b("bottom"));
                    let w = ((ih - fv_).max(0) as i128 * rw as i128 / rh.max(1) as i128) as U + fh_;
                    clamp_by(w, "min-width", "max-width", aw)
                }
                _ => iw,
            };
            // Offsets of the margin box; `block` adds the start margins.
            let (fw, fh) = (aw - ml - mr - iw, ah - mt - mb - ih);
            // ★ Auto margins take POSITIVE free space only (Grid §11.2):
            // with an item larger than its area they are 0 and the
            // alignment properties decide, so the overflow stays safe on
            // the end side instead of centring off the start edge.
            let (al, ar) = if fw < 0 { (false, false) } else { (al, ar) };
            let (at, ab) = if fh < 0 { (false, false) } else { (at, ab) };
            let dx = match (al, ar) {
                (true, true) => fw / 2, (true, false) => fw, (false, true) => 0,
                _ => match (it.justify.as_str(), rtl) { ("end", false) => fw, ("center", _) => fw / 2, ("end", true) => 0, (_, true) => fw, _ => 0 },
            };
            let dy = match (at, ab) {
                (true, true) => fh / 2, (true, false) => fh, (false, true) => 0,
                _ => match it.align.as_str() { "end" => fh, "center" => fh / 2, _ => 0 },
            };
            let forced_h = ((it.align == "stretch" && !(at || ab)) || specified_h.is_some()).then_some(ih);
            self.block(it.h, ax + dx, ay + dy, iw, Some(ah), (Some(iw), forced_h));
        }
        for j in abs_jobs { run_abs(self, j) }
        total_h
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
        let (rows, nested) = self.table_rows(h);
        for _ in nested { self.count("a nested table beside table rows (not laid out)") }
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
        let mut cols: Vec<U> = size_tracks(&tracks, Some(avail), sx, &mins, &maxs, &mins, &[]);
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
        // Columns with no max-content at all (empty cells) share it EQUALLY.
        if stretch && free > 0 && !targets.is_empty() {
            let mut given = 0;
            for (n, i) in targets.iter().enumerate() {
                let share = if total_max > 0 { free as i128 * maxs[*i] as i128 / total_max as i128 } else { free as i128 / targets.len() as i128 };
                let add = if n + 1 == targets.len() { free - given } else { share as U };
                cols[*i] += add;
                given += add;
            }
        }
        // ★ Each cell takes the next `colspan` columns not covered by a
        // `rowspan` from above, and its width is theirs together with the
        // spacing between them. A cell past the declared columns keeps the
        // old fallback (the last column's width) and is counted, not
        // silently squeezed.
        let mut rows_cells: Vec<Vec<Handle>> = rows.iter().map(|r| cells_of(self, *r)).collect();
        let mut slots = self.slots(&rows_cells);
        // First screen only: rows are measured until they pass the limit.
        let row_limit = self.table_stop.take();
        let width_of = |this: &mut Self, (k, n, _): (usize, usize, usize)| -> U {
            if k >= cols.len() {
                this.count("table cell past the declared columns");
                *cols.last().expect("non-empty")
            } else {
                let end = (k + n).min(cols.len());
                cols[k..end].iter().sum::<U>() + sx * (end - k - 1) as U
            }
        };
        // Natural heights first, every cell at its width.
        let mut nat: Vec<Vec<U>> = vec![];
        let mut widths: Vec<Vec<U>> = vec![];
        let mut acc = y + sy;
        for (cells, row_slots) in rows_cells.iter().zip(&slots) {
            let ws: Vec<U> = row_slots.iter().map(|sl| width_of(self, *sl)).collect();
            nat.push(cells.iter().zip(&ws).map(|(c, cw)| { self.table_part = true; self.measure(*c, *cw, None, (Some(*cw), None)) }).collect());
            widths.push(ws);
            if let Some(l) = row_limit {
                let r = nat.len() - 1;
                acc += row_slots.iter().zip(&nat[r]).filter(|((_, _, rs), _)| *rs == 1).map(|(_, h)| *h).max().unwrap_or(0) + sy;
                if acc > l { break }
            }
        }
        if nat.len() < rows_cells.len() {
            // The rows kept, and spans clamped to them.
            let n = nat.len();
            rows_cells.truncate(n);
            slots.truncate(n);
            for (r, row) in slots.iter_mut().enumerate() { for sl in row.iter_mut() { sl.2 = sl.2.min(n - r) } }
            self.truncated = true;
        }
        // Baselines, for cells aligned on one: the row's shared baseline is
        // the lowest of them (a cell spanning rows shares its FIRST row's),
        // and each such cell is pushed down to it.
        let mut shift: Vec<Vec<U>> = vec![];
        for (r, cells) in rows_cells.iter().enumerate() {
            let bases: Vec<Option<U>> = cells.iter().enumerate().map(|(i, c)| {
                let cs = self.st(*c).expect("styled");
                (kw(cs, "vertical-align") == "baseline").then(|| {
                    let cw = widths[r][i];
                    self.table_part = true;
                    self.baseline(*c, cw, (Some(cw), None))
                })
            }).collect();
            let max_b = bases.iter().flatten().copied().max().unwrap_or(0);
            shift.push(bases.iter().map(|b| b.map_or(0, |b| max_b - b)).collect());
        }
        // A row is as tall as its tallest one-row cell — shifted down by its
        // baseline, which the row must still hold (CSS 2.1 §17.5.3); then a
        // cell spanning rows that needs more than those rows (and the
        // spacing between them) give spreads the shortfall over them,
        // shortest spans first.
        let need = |r: usize, i: usize| nat[r][i] + shift[r][i];
        let mut row_h: Vec<U> = slots.iter().enumerate()
            .map(|(r, row_slots)| row_slots.iter().enumerate().filter(|(_, (_, _, rs))| *rs == 1).map(|(i, _)| need(r, i)).max().unwrap_or(0))
            .collect();
        let mut spans: Vec<(usize, usize, U)> = slots.iter().enumerate()
            .flat_map(|(r, row_slots)| row_slots.iter().enumerate().filter(|(_, (_, _, rs))| *rs > 1).map(move |(i, (_, _, rs))| (r, *rs, need(r, i))))
            .collect();
        spans.sort_by_key(|x| x.1);
        for (r, rs, want) in spans {
            let have: U = row_h[r..r + rs].iter().sum::<U>() + sy * (rs as U - 1);
            if want > have {
                let short = want - have;
                for (j, h) in row_h[r..r + rs].iter_mut().enumerate() {
                    *h += short / rs as U + if j == rs - 1 { short % rs as U } else { 0 };
                }
            }
        }
        let mut row_y: Vec<U> = vec![];
        let mut cy = y + sy;
        for h in &row_h { row_y.push(cy); cy += h + sy }
        // Column i's left edge, so a cell starts where its first column does.
        let col_x = |i: usize| -> U { x + sx + cols[..i.min(cols.len())].iter().sum::<U>() + sx * i.min(cols.len()) as U };
        // ★ A right-to-left table's columns run from its RIGHT edge (CSS 2.1
        // §17.2: column 1 is on the right): each cell is mirrored in the
        // table's box.
        let rtl = kw(s, "direction") == "rtl";
        // ★ THE FIRST COLUMN STAYS IN VIEW (backend §4.9 item 4): in a table
        // that scrolls on its own, the first column's cells keep the
        // transform OUTSIDE the region — the compositor does not move them —
        // and are painted after the rest, over the background behind them,
        // so the other columns slide under. Not when the first column would
        // take more than 40% of the region: pinned, it would leave too
        // little to scroll through.
        let pin = self.scroll_table == Some(h) && cols.len() >= 2 && self.scroll_open.as_ref().is_some_and(|(_, clip, ..)| {
            let vw = self.scene.clips[*clip as usize].2;
            (cols[0] + 2 * sx) * 5 <= vw * 2
        });
        let outer_xf = self.scroll_open.as_ref().map(|(saved, ..)| saved.2);
        let order_from = self.scene.order.len();
        let mut pinned: Vec<std::ops::Range<usize>> = vec![];
        for (r, (cells, row_slots)) in rows_cells.iter().zip(&slots).enumerate() {
            for (i, c) in cells.iter().enumerate() {
                let (k, _, rs) = row_slots[i];
                let pin_this = pin && k == 0;
                let (start, cur_xf) = (self.scene.order.len(), self.scene.cur.2);
                let (saved_scroll_clip, cur_clip) = (self.scene.scroll_clip, self.scene.cur.0);
                // A pinned cell is clipped to the region's VIEWPORT, which
                // does not move — not to a clip made inside the region (the
                // table's own `overflow`), which does: ru Wikipedia's
                // self-clipping tables were refused by the broker for it.
                let viewport = self.scroll_open.as_ref().map(|(_, clip, ..)| *clip);
                if pin_this { self.scene.cur.2 = outer_xf.flatten(); self.scene.scroll_clip = None; self.scene.cur.0 = viewport }
                let cw = widths[r][i];
                // The cell box fills every row it spans (separate-borders model).
                let ch: U = row_h[r..r + rs].iter().sum::<U>() + sy * (rs as U - 1);
                let cx = if k < cols.len() { col_x(k) } else { col_x(cols.len()) };
                let cx = if rtl { x + w - (cx - x) - cw } else { cx };
                let cs = self.st(*c).expect("styled");
                // vertical-align positions the CONTENT inside that height.
                self.content_dy = match kw(cs, "vertical-align") {
                    "middle" => (ch - nat[r][i]) / 2,
                    "bottom" => ch - nat[r][i],
                    "baseline" => shift[r][i],
                    _ => 0,
                };
                self.table_part = true;
                self.block(*c, cx, row_y[r], cw, Some(ch), (Some(cw), Some(ch)));
                self.content_dy = 0;
                if pin_this {
                    // What scrolls under it must not show through: the
                    // background behind the cell, under the cell and over
                    // the spacing around it (which the cell does not paint).
                    let bg = self.background_behind(*c);
                    self.scene.insert_rect(start, Rect { x: cx - sx, y: row_y[r] - sy, w: cw + 2 * sx, h: ch + 2 * sy, rgba: bg, radii: [0; 4], ring: 0 });
                    pinned.push(start..self.scene.order.len());
                    self.scene.cur.2 = cur_xf;
                    self.scene.cur.0 = cur_clip;
                    self.scene.scroll_clip = saved_scroll_clip;
                }
            }
        }
        if !pinned.is_empty() {
            let tail = self.scene.order.split_off(order_from);
            let is_pinned = |j: usize| pinned.iter().any(|rg| rg.contains(&(order_from + j)));
            let (mut front, mut back) = (vec![], vec![]);
            for (j, e) in tail.into_iter().enumerate() { if is_pinned(j) { back.push(e) } else { front.push(e) } }
            self.scene.order.extend(front);
            self.scene.order.extend(back);
        }
        cy - y
    }

    /// Whether `h` or anything inside it has a transform or is absolutely
    /// or fixed positioned: something that may paint away from where the
    /// flow would put it.
    fn subtree_moves(&self, h: Handle) -> bool {
        let mut st = vec![h];
        while let Some(x) = st.pop() {
            if let Some(cs) = self.st(x) {
                if kw(cs, "display") == "none" { continue }
                if matches!(kw(cs, "position"), "absolute" | "fixed") { return true }
                if matches!(cs.get("transform"), V::Transform(l) if !l.is_empty()) { return true }
            }
            st.extend(self.dom.element_children(x));
        }
        false
    }

    /// The nearest opaque background behind `h`: an ancestor's, else the
    /// page's white.
    fn background_behind(&self, h: Handle) -> u32 {
        let mut cur = self.dom.get(h).and_then(|n| n.parent);
        while let Some(p) = cur {
            if let Some(cs) = self.st(p) {
                if let V::Color(c) = cs.get("background-color") {
                    let v = rgba(c, 0x000000ff);
                    if v & 0xff == 0xff { return v }
                }
            }
            cur = self.dom.get(p).and_then(|n| n.parent);
        }
        0xffffffff
    }

    /// An item's content size along the main axis (its max-content width in
    /// a row, its laid-out height in a column), as a border-box size.
    ///
    /// ★ UNCLAMPED in a row (Flexbox §9.2 C/E: the flex base size is the
    /// max-content size, and fitting the line is the shrink step's job).
    /// Clamped to the container, as `intrinsic_outer_w` is for its
    /// shrink-to-fit callers, a `flex-shrink: 0` slide holding five 1000 px
    /// images was 800 wide, and its images 152 px each (Thomann's teaser
    /// carousel; Chrome 1008 each). It hid while images measured 0 wide.
    fn content_main(&mut self, h: Handle, row: bool, cb_w: U) -> U {
        if row { self.max_content_outer_w(h, cb_w) } else {
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
        let px = |p: &str| len(cs.get(p), 0).unwrap_or(0);
        let (ml, mr) = (px("margin-left"), px("margin-right"));
        let border = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { px(&format!("border-{side}-width")) };
        let frame = px("padding-left") + px("padding-right") + border("left") + border("right");
        // ★ A COMPRESSIBLE replaced box (CSS Sizing 3 §5.2.2: a percentage
        // `width` or `max-width`, also inside calc()) contributes nothing to
        // min-content — the same rule `intrinsic` applies to a block-level
        // one. Its max-content is still its size (`atomic_outer_w`).
        if self.replaced_size(h).is_some() {
            return (pct_size(cs, "width") || pct_size(cs, "max-width")).then_some(frame + ml + mr);
        }
        if !matches!(cs.get("width"), V::Kw(k) if *k != "max-content") { return None }
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
        // ★ With no width to resolve against (an intrinsic measure), a
        // percentage limit is no limit — nor a calc() holding one. Wikipedia's
        // thumbnail image, `max-width: calc(100% - 8px + 2px)`, was clamped
        // against 0 and its column measured as the caption's longest word.
        let limit = |p: &str| match cs.get(p) {
            V::Pct(_) if cb_w <= 0 => None,
            V::Calc(c) if cb_w <= 0 && calc_has_pct(c) => None,
            v => len(v, cb_w),
        };
        if let Some(m) = limit("max-width") { bw = bw.min(m) }
        if let Some(m) = limit("min-width") { bw = bw.max(m) }
        bw.max(frame) + ml + mr
    }

    /// The border-box width a grid item is laid out at in an area `aw`
    /// wide: its margin box goes in the area, so stretch fills what the
    /// margins leave and anything else is fit-content in it; an `auto`
    /// margin stops the stretch (Grid §11). A definite specified width wins
    /// over the intrinsic one (an empty `width: 40px` box is 40 wide, not
    /// 0), and min-/max-width hold either way — BBC News's `width: 100%;
    /// max-width: calc(100% - 32px); margin: 0 16px` grid, centred in its
    /// area, was 800 wide at x = 0, not 768 at 16. The rows are measured at
    /// this width too, or content that wraps more than it was measured
    /// for runs into the next row (Which?'s article list).
    fn grid_item_w(&mut self, h: Handle, aw: U, justify: &str) -> U {
        let cs = self.st(h);
        let mg = |p: &str| cs.and_then(|cs| len(cs.get(p), aw)).unwrap_or(0);
        let auto = |p: &str| cs.is_some_and(|cs| matches!(cs.get(p), V::Kw("auto")));
        let room_w = (aw - mg("margin-left") - mg("margin-right")).max(0);
        let specified_w = cs.and_then(|cs| len(cs.get("width"), aw));
        let iw = match justify {
            "stretch" if !(auto("margin-left") || auto("margin-right")) => specified_w.unwrap_or(room_w),
            // A SPECIFIED width is the width, larger than the area or not;
            // an auto one is FIT-CONTENT (Grid §11.1): the area caps the
            // max-content width but never below min-content, so an item
            // too wide for a 10px track overflows it.
            _ => specified_w.unwrap_or_else(|| {
                let fit = self.intrinsic_outer_w(h, room_w);
                let min = cs.map_or(0, |cs| {
                    let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { len(cs.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
                    len(cs.get("padding-left"), aw).unwrap_or(0) + len(cs.get("padding-right"), aw).unwrap_or(0) + b("left") + b("right")
                }) + self.intrinsic(h).0;
                fit.max(min)
            }),
        };
        let iw = cs.and_then(|cs| len(cs.get("max-width"), aw)).map_or(iw, |m| iw.min(m));
        cs.and_then(|cs| len(cs.get("min-width"), aw)).map_or(iw, |m| iw.max(m))
    }

    fn intrinsic_outer_w(&mut self, h: Handle, cb_w: U) -> U {
        // A definite width is that width, room or not; only the content's
        // max-content is fitted to the room.
        if let Some(wd) = self.st(h).and_then(|cs| len(cs.get("width"), cb_w)) { return wd }
        self.max_content_outer_w(h, cb_w).min(cb_w.max(0))
    }

    /// Max-content border-box width of `h` (its width if definite), NOT
    /// clamped to `cb_w`, which only resolves percentages.
    fn max_content_outer_w(&mut self, h: Handle, cb_w: U) -> U {
        // An anonymous block box around loose text has no style and no box
        // decoration: its outer width IS its content width.
        let Some(cs) = self.st(h) else { return self.intrinsic(h).1 };
        if let Some(wd) = len(cs.get("width"), cb_w) { return wd }
        let b = |side: &str| if kw(cs, &format!("border-{side}-style")) == "none" { 0 } else { len(cs.get(&format!("border-{side}-width")), 0).unwrap_or(0) };
        let frame = len(cs.get("padding-left"), cb_w).unwrap_or(0) + len(cs.get("padding-right"), cb_w).unwrap_or(0) + b("left") + b("right");
        self.intrinsic(h).1 + frame
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
        // ★ A REPLACED box's intrinsic sizes are its own size (CSS Sizing 3
        // §5.1) — it has no content to measure. Measured as an empty block
        // it was 0 wide, so an `<img>` flex item's content basis was 0: NASA's
        // image of the day laid out 0 px wide inside its 750 px figure.
        // ★ …except that one sized by a PERCENTAGE `width` or `max-width` is
        // COMPRESSIBLE (§5.2.2): the percentage resolves against zero for
        // its min-content contribution. Counted at its natural width, every
        // `max-width: 100%` photo made its column as wide as the photo —
        // El País ran 81 words off the page.
        if !anon {
            if let Some((w, _)) = self.replaced_size(h) {
                return (if pct_size(s, "width") || pct_size(s, "max-width") { 0 } else { w }, w);
            }
        }
        // ★ A TABLE's intrinsic widths are its declared columns' (§3.13),
        // spacing included — what `block` sizes it from. Measured as a
        // generic block it came out 1 px short of itself inside a cell at
        // 200% zoom, and a nested table overhung its outer cell.
        if !anon && kw(s, "display") == "table" {
            let measured = self.measured_tables.get(&h).copied();
            if let Some((a, b)) = measured.or_else(|| self.table_content(h, s, 0, false).zip(self.table_content(h, s, 0, true))) {
                self.links.truncate(links); self.unimplemented = counts; self.pending_abs.truncate(pend);
                return (a, b.max(a));
            }
        }
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
                let cw = self.st(*c).and_then(|cs| abs_len(cs.get("width")));
                let (a, b) = match cw { Some(w) => (w, w), None => self.intrinsic(*c) };
                let frame = self.st(*c).map(|cs| {
                    let px = |p: &str| len(cs.get(p), 0).unwrap_or(0);
                    px("margin-left") + px("margin-right") + if cw.is_some() { 0 } else {
                        px("padding-left") + px("padding-right")
                            + if kw(cs, "border-left-style") == "none" { 0 } else { px("border-left-width") }
                            + if kw(cs, "border-right-style") == "none" { 0 } else { px("border-right-width") }
                    }
                }).unwrap_or(0);
                let margins = self.st(*c).map(|cs| len(cs.get("margin-left"), 0).unwrap_or(0) + len(cs.get("margin-right"), 0).unwrap_or(0)).unwrap_or(0);
                let (a, b) = match self.st(*c) {
                    Some(cs) => (contribution(cs, a + frame - margins) + margins, contribution(cs, b + frame - margins) + margins),
                    None => (a + frame, b + frame),
                };
                smn += a;
                smx += b;
                mins.push(a);
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
        // ★ A GRID container is as wide as its COLUMNS, each sized under the
        // min- or max-content constraint, plus the gaps (CSS Grid §12).
        // Measured like a block — as its widest item — rustdoc's toolbar,
        // four auto columns in a `minmax(0, max-content)` track, was one
        // button wide and its labels broke between letters.
        if !anon && kw(s, "display") == "grid" {
            let p = self.grid_plan(h, s, 0);
            let gaps = p.cgap * (p.coltracks.len() as U).saturating_sub(1);
            // Under a MIN-content constraint the flex fraction is zero
            // (§12.7.1), so spanning items count toward the max only.
            let mn = size_tracks(&p.coltracks, None, p.cgap, &p.cmin, &p.cmin, &p.cmin, &[]).iter().sum::<U>() + gaps;
            let mx = size_tracks(&p.coltracks, None, p.cgap, &p.cmin, &p.cmax, &p.cmin, &p.cspans).iter().sum::<U>() + gaps;
            self.links.truncate(links);
            self.unimplemented = counts;
            self.pending_abs.truncate(pend);
            return (mn, mx.max(mn));
        }
        for c in if anon { vec![] } else { self.dom.children_of(h) } {
            if self.is_block_level(c) {
                let Some(cs) = self.st(c) else { continue };
                // ★ An out-of-flow box takes no part in its container's
                // intrinsic sizes (CSS Sizing 3 §5). Counted, the Guardian's
                // slideshow caption — `position: absolute; left: 0; right: 0`
                // over the photo — set the carousel half's min-content at
                // its one-line width, and a 50% flex half could not shrink.
                if matches!(kw(cs, "position"), "absolute" | "fixed") { continue }
                let px = |p: &str| len(cs.get(p), 0).unwrap_or(0);
                // Border-box: a DEFINITE width already holds the padding and
                // border; only the margins are outside it. Adding the frame
                // again measured a 220 px box as 240 once tables stopped
                // filling the line and started wrapping their columns.
                let definite = abs_len(cs.get("width")).is_some();
                let frame = px("margin-left") + px("margin-right") + if definite { 0 } else {
                    px("padding-left") + px("padding-right")
                    + if kw(cs, "border-left-style") == "none" { 0 } else { px("border-left-width") }
                    + if kw(cs, "border-right-style") == "none" { 0 } else { px("border-right-width") } };
                let (a, b) = match abs_len(cs.get("width")) { Some(w) => (w, w), None => self.intrinsic(c) };
                let margins = px("margin-left") + px("margin-right");
                let (a, b) = (contribution(cs, a + frame - margins) + margins, contribution(cs, b + frame - margins) + margins);
                mn = mn.max(a);
                mx = mx.max(b);
            } else {
                self.collect_inline(c, &mut items, s);
            }
        }
        let mut line = 0;
        // Under `pre`/`nowrap` nothing between items is a break, and words
        // touching with no UAX #14 break between them are glued: either way
        // the run is ONE min-content unit, as `lines` will lay it out.
        let mut chain: Option<U> = None;
        let mut prev: Option<&str> = None;
        for it in &items {
            match it {
                Item::Break => { mx = mx.max(line); line = 0; chain = None; prev = None }
                Item::Word(t, st, l) | Item::PreSpace(t, st, l) => {
                    let w: U = self.sh.shape(t, st, &mut self.report).iter().map(|p| p.width).sum();
                    let glued = l.nowrap || prev.is_some_and(|p| !uax14_break_between(p, t));
                    let c = match chain { Some(c) if glued => c + w, _ => w };
                    if matches!(it, Item::Word(..)) { mn = mn.max(c); prev = Some(t) } else { prev = None }
                    chain = Some(c);
                    line += w;
                }
                Item::Space(st, l) => {
                    let w = self.sh.shape(" ", st, &mut self.report).iter().map(|p| p.width).sum::<U>();
                    chain = chain.filter(|_| l.nowrap).map(|c| c + w);
                    prev = None;
                    line += w;
                }
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
                    chain = None;
                    prev = None;
                }
                Item::Tab(st, l) => {
                    chain = None;
                    prev = None;
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
                          tab: match s.get("tab-size") { V::Int(n) => (*n).clamp(0, 64), _ => 8 },
                          nowrap: matches!(kw(s, "white-space"), "pre" | "nowrap"), iso: self.iso_cur };
        let sp = |p: &str| match s.get(p) { V::Len(l) => u(l.v), V::Calc(c) => calc_eval(c, 0).map(u).unwrap_or(0), _ => 0 };
        (FontStyle { family, bold: weight >= 600, em, size, rgba: color, link: None, inl: self.inl_cur,
                     ls: sp("letter-spacing"), ws: sp("word-spacing"), tnum: kw(s, "font-variant-numeric") == "tabular-nums" }, line)
    }

    /// Turn an inline-level node (and its inline descendants) into items.
    /// Lay out the out-of-flow boxes `collect_inline` set aside, at the
    /// static position of the line box they were written in.
    /// ★ In DOCUMENT order: the later of two overlapping positioned boxes
    /// paints on top (CSS 2.1 Appendix E). Popped from the end, the first
    /// of `<span abs red><span abs green>` covered the second.
    fn place_pending_abs(&mut self, x: U, right: U, y: U) {
        for h in std::mem::take(&mut self.pending_abs) { self.abs_box(h, x, right, y) }
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
                if tag == "img" || self.is_replaced(h) { let (st, l) = self.font_style(s); out.push(Item::Atomic(h, st, l)); return }
                if tag == "br" { out.push(Item::Break); return }
                let link = (tag == "a").then(|| self.dom.attr(h, "href")).flatten().map(|href| { self.links.push(href.to_string()); self.links.len() - 1 });
                let before = out.len();
                // ★ An inline box whose direction differs from its parent's is
                // a directional ISOLATE (profile row 48: HTML gives every
                // `dir` element `unicode-bidi: isolate`): its content is
                // ordered on its own and moves as one unit in the line, so
                // an English name inside Arabic text keeps its words in
                // order and the punctuation after it stays outside.
                let saved_iso = self.iso_cur;
                let rtl = kw(s, "direction") == "rtl";
                if rtl != (kw(parent, "direction") == "rtl") {
                    let mut path = self.iso_paths[self.iso_cur as usize].clone();
                    path.push((self.iso_paths.len() as u32, rtl));
                    self.iso_cur = self.iso_paths.len() as u32;
                    self.iso_paths.push(path);
                }
                let saved_inl = self.inl_cur;
                if let Some(g) = self.geo.as_mut() {
                    let mut path = g.inl_paths[self.inl_cur as usize].clone();
                    path.push(h);
                    if kw(s, "position") != "static" { g.inl_positioned.insert(h); }
                    self.inl_cur = g.inl_paths.len() as u32;
                    g.inl_paths.push(path);
                }
                for c in self.dom.children_of(h) { self.collect_inline(c, out, s) }
                self.inl_cur = saved_inl;
                self.iso_cur = saved_iso;
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
                // ★ A space is not the only break opportunity: CJK breaks
                // between ideographs, and Thai, Lao, Khmer and Myanmar
                // between words they do not space (UAX #14). Split only at
                // spaces, a Chinese paragraph was ONE word and ran off the
                // canvas. Each segment is its own Word; adjacent Words wrap.
                let push = |out: &mut Vec<Item>, word: String| {
                    for seg in uax14_segments(&word) { out.push(Item::Word(seg.to_string(), st.clone(), line)) }
                };
                let mut word = String::new();
                for ch in t.chars() {
                    if ch.is_whitespace() {
                        if !word.is_empty() { push(out, std::mem::take(&mut word)) }
                        // Collapse: at most one space, never at a line start.
                        if !matches!(out.last(), Some(Item::Space(..)) | Some(Item::Break) | None) {
                            out.push(Item::Space(st.clone(), line))
                        }
                    } else { word.push(ch) }
                }
                if !word.is_empty() { push(out, word) }
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
                        /// The isolate path that space was in (`Line::iso`).
                        gap_iso: u32,
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
        // The texts of the words placed since the last space, atomic or
        // break: the chain a word with no break before it is glued to.
        let mut run_texts: Vec<String> = vec![];
        // Whether each of them is glued to the one before it.
        let mut run_glue: Vec<bool> = vec![];
        for it in items {
            match it {
                Item::Tab(st, l) => {
                    run_texts.clear(); run_glue.clear();
                    // Tab stops: multiples of tab-size × the space's advance,
                    // measured from the line's start (exact for any font).
                    let sp: U = self.sh.shape(" ", &st, &mut self.report).iter().map(|p| p.width).sum();
                    let stop = (sp * l.tab as U).max(1);
                    cx = (cx / stop + 1) * stop;
                    pending = None;
                }
                Item::Break => { *forced.last_mut().expect("line") = true; lines.push(vec![]); forced.push(false); cx = 0; pending = None; run_texts.clear(); run_glue.clear() }
                Item::Space(st, l) => { pending = Some((st, l)); run_texts.clear(); run_glue.clear() }
                Item::PreSpace(text, st, l) => {
                    run_texts.clear(); run_glue.clear();
                    let pieces = self.sh.shape(&text, &st, &mut self.report);
                    let ww: U = pieces.iter().map(|p| p.width).sum();
                    // ★ Preserved spaces HANG at a line end (pre-wrap): they
                    // never push the line past the edge, and never wrap.
                    if cx + ww > w && !lines.last().expect("line").is_empty() { pending = None; continue }
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces, gap: false, gap_w: 0, gap_iso: 0, level: 0, adv: ww, atomic: None });
                    cx += ww;
                    pending = None;
                }
                Item::Atomic(ah, st, l) => {
                    run_texts.clear(); run_glue.clear();
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
                    let wrap = cx + sw + adv > w && !lines.last().expect("line").is_empty() && !no_soft_wrap(&l, &pending);
                    if wrap { lines.push(vec![]); forced.push(false); cx = 0 } else { cx += sw }
                    let asc = self.baseline(ah, w, (Some(bw), None));
                    let outer_h = self.measure(ah, w, None, (Some(bw), None));
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces: vec![], gap: !wrap && sw > 0,
                                                                  gap_w: if wrap { 0 } else { sw }, gap_iso: pending.as_ref().map(|p| p.1.iso).unwrap_or(0), level: 0,
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
                    let over = cx + sw + ww > w && !lines.last().expect("line").is_empty();
                    let mut wrap = over && !no_soft_wrap(&l, &pending);
                    // ★ A word touching the one before it wraps only where
                    // UAX #14 allows (and never inside `nowrap`); otherwise
                    // the glued chain goes to the next line WITH it — or, if
                    // the chain is the whole line, overflows rather than
                    // splitting a word. Wikipedia's nowrap "[timeframe?]"
                    // ran off the line instead of moving down whole.
                    let glued = pending.is_none() && run_texts.last().is_some_and(|p| l.nowrap || !uax14_break_between(p, &text));
                    if over && glued {
                        let mut k = 1;
                        while k < run_texts.len() && run_glue[run_texts.len() - k] { k += 1 }
                        let line = lines.last_mut().expect("line");
                        if k < line.len() {
                            let mut moved = line.split_off(line.len() - k);
                            let x0 = moved[0].x;
                            for p in &mut moved { p.x -= x0 }
                            moved[0].gap = false;
                            moved[0].gap_w = 0;
                            cx = moved.last().map(|p| p.x + p.adv).unwrap_or(0);
                            lines.push(moved);
                            forced.push(false);
                            run_texts.drain(..run_texts.len() - k);
                            run_glue.drain(..run_glue.len() - k);
                        }
                        wrap = false;
                    }
                    if wrap { run_texts.clear(); run_glue.clear() }
                    run_glue.push(glued && !run_texts.is_empty());
                    run_texts.push(text.clone());
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
                            lines.last_mut().expect("line").push(Placed { x: cx, st: st.clone(), line: l, pieces, gap: false, gap_w: 0, gap_iso: 0, level: 0, adv: cw, atomic: None });
                            cx += cw;
                            i = j;
                            if i < chars.len() { lines.push(vec![]); forced.push(false); cx = 0 }
                        }
                        pending = None;
                        continue;
                    }
                    if ww > w { self.report.overflow_lines += 1 }
                    lines.last_mut().expect("line").push(Placed { x: cx, st, line: l, pieces, gap: !wrap && sw > 0, gap_w: if wrap { 0 } else { sw }, gap_iso: pending.as_ref().map(|p| p.1.iso).unwrap_or(0), level: 0, adv: ww, atomic: None });
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
            if base_rtl || line.iter().any(|p| p.line.iso != 0 || p.pieces.iter().any(|pc| pc.text.chars().any(is_rtl))) {
                // The line's text in logical order, with each folded space
                // made an item of its own: reordering moves what is BETWEEN
                // words, so a space cannot stay glued to one of them. Where
                // the isolate path changes between two pieces, the isolates
                // left are closed (PDI) and those entered opened (LRI/RLI);
                // a line starts by opening every isolate its first piece is
                // in, so an isolate broken across lines is one on each.
                let mut logical = String::new();
                let mut path: &[(u32, bool)] = &[];
                let paths = &self.iso_paths;
                let mut enter = |logical: &mut String, to: u32| {
                    let to: &[(u32, bool)] = &paths[to as usize];
                    let k = path.iter().zip(to).take_while(|(a, b)| a.0 == b.0).count();
                    for _ in k..path.len() { logical.push('\u{2069}') }
                    for (_, r) in &to[k..] { logical.push(if *r { '\u{2067}' } else { '\u{2066}' }) }
                    path = to;
                };
                let mut seq: Vec<(bool, usize)> = vec![]; // (is the space, start offset), one pair per placed
                for p in &line {
                    let sp = if p.gap_w > 0 { enter(&mut logical, p.gap_iso); let st = logical.len(); logical.push(' '); st } else { usize::MAX };
                    enter(&mut logical, p.line.iso);
                    seq.push((true, sp));
                    let st = logical.len();
                    for pc in &p.pieces { logical.push_str(&pc.text) }
                    if p.atomic.is_some() { logical.push('\u{fffc}') } // object replacement: neutral, like an image
                    seq.push((false, st));
                }
                let info = unicode_bidi::BidiInfo::new(&logical, Some(if base_rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() }));
                let level_at = |i: usize| info.levels.get(i).map(|l| l.number()).unwrap_or(0);
                // Rebuild the line with the spaces as items, each carrying
                // its own level. A word whose own text spans two levels is
                // split and re-shaped at the boundary — itemization, which a
                // real engine does before shaping.
                let mut rebuilt: Vec<Placed> = vec![];
                for (p, pair) in line.into_iter().zip(seq.chunks(2)) {
                    let (sp, st) = (pair[0].1, pair[1].1);
                    if p.gap_w > 0 {
                        let spc = self.sh.shape(" ", &p.st, &mut self.report);
                        rebuilt.push(Placed { x: 0, st: p.st.clone(), line: p.line, pieces: spc, gap: p.gap, gap_w: 0, gap_iso: 0,
                                              level: level_at(sp), adv: p.gap_w, atomic: None });
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
                            rebuilt.push(Placed { x: 0, st: p.st.clone(), line: p.line, pieces, gap: false, gap_w: 0, gap_iso: 0, level: l, adv, atomic: None });
                        }
                    }
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
                // Visual order fixed: lay the advances out left to right. The
                // first line's text-indent is at its START: on the left in an
                // ltr line, on the right — after everything — in an rtl one.
                let mut px = if li == 0 && !base_rtl { first_indent } else { 0 };
                for p in &mut rebuilt { p.x = px; px += p.adv }
                line = rebuilt;
            }
            let lh = line.iter().map(|p| if p.atomic.is_some() { 0 } else { p.line.lh }).max().unwrap_or(0)
                .max(base + metrics.iter().map(|m| m.1).max().unwrap_or(0));
            let last = line.last().expect("non-empty");
            let used = last.x + last.adv + if li == 0 && base_rtl { first_indent } else { 0 };
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
                // ★ The marker hangs off the START side: right of an rtl
                // line, where the list's indent is — and its text is ordered
                // right-to-left too, so `1.` reads `.1` (its full stop is a
                // neutral, and takes the rtl direction after the digits).
                let text = match text.strip_suffix('.') { Some(n) if base_rtl => format!(".{n}"), _ => text };
                let pieces = self.sh.shape(&text, &st, &mut self.report);
                let mw: U = pieces.iter().map(|p| p.width).sum();
                let mut mx = if base_rtl { x + w + st.size / 2 } else { x - mw - st.size / 2 };
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
                // Every inline element this item sits in covers it, hidden
                // or not (visibility: hidden still has geometry).
                if let Some(g) = self.geo.as_mut() {
                    for &e in &g.inl_paths[p.st.inl as usize] { g.frags.push((e, start, yy, p.adv, lh)) }
                }
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
                // ★ An ATOMIC inline is exempt: its own font-size is only the
                // strut's, and its content sets its own. `ul { font-size: 0 }`
                // around inline-block items — the idiom that removes the gaps
                // between them — skipped every item, and Samsung's footer
                // links vanished.
                if p.st.size <= 0 && p.atomic.is_none() { continue }
                if let Some((ah, bw, asc, _)) = p.atomic {
                    // Its margin-box origin: the baseline of this line, minus
                    // the box's own baseline.
                    self.block(ah, px, yy + base - asc, w, None, (Some(bw), None));
                    px += p.adv;
                }
                // ★ An item at an ODD level runs right to left, and so do its
                // pieces: a word whose glyphs come from two faces — Arabic
                // from one, the full stop after it from another — is two
                // pieces, and laid out in logical order the stop landed
                // between the word and the one before it ("العام" "." "الرأي").
                let order: Vec<&crate::Piece> = if p.level % 2 == 1 { p.pieces.iter().rev().collect() } else { p.pieces.iter().collect() };
                for piece in order {
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
    fn auto_margins_never_push_an_overwide_box_off_the_left() {
        let o = render(r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .c { min-width: 1024px; width: 100%; margin-left: auto; margin-right: auto; height: 10px }
            .d { width: 100px; height: 10px; background-color: #00ff00 }</style><div class="c"><div class="d"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 0, 100, 10)]);
    }

    #[test]
    fn a_scrolling_flex_item_shrinks_below_its_content() {
        let src = |ov: &str| format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }}
            .r {{ display: flex; width: 300px }}
            .a {{ flex-basis: 200px; flex-shrink: 0; height: 10px; background-color: #ff0000 }}
            .b {{ flex-basis: 400px; flex-shrink: 1; height: 10px; background-color: #00ff00; {ov} }}
            .w {{ width: 250px; height: 1px }}</style>
            <div class="r"><div class="a"></div><div class="b"><div class="w"></div></div></div>"#);
        let o = render(&src("overflow-x: hidden; overflow-y: hidden"));
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(200, 0, 100, 10)]);
        // Control: a visible item keeps its 250px content width.
        let c = render(&src(""));
        assert_eq!(boxes(&c, 0x00ff00ff), vec![(200, 0, 250, 10)]);
    }

    #[test]
    fn a_bottom_anchored_box_waits_for_an_auto_height_container() {
        // Spiegel's teaser: a relative card of auto height, an image that
        // gives it its height, an overlay pinned to its bottom edge.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .card { position: relative; width: 300px }
            .pic { height: 200px; background-color: #0000ff }
            .ov { position: absolute; bottom: 0px; left: 0px; width: 100px; height: 20px; background-color: #00ff00 }
            .pct { position: absolute; top: 50%; left: 200px; width: 10px; height: 10px; background-color: #ff0000 }</style>
            <div class="card"><div class="ov"></div><div class="pct"></div><div class="pic"></div></div><div class="pic"></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 180, 100, 20)]);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(200, 100, 10, 10)]);
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
    fn small_and_big_scale_the_parent_font_size_by_1_2() {
        let o = render(r#"<style>p { font-size: 18px }</style><p>a<small>b</small><big>c</big></p>"#);
        let size = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.size).expect(t);
        assert_eq!(size("a"), 18 * PX);
        assert_eq!(size("b"), 15 * PX, "smaller: 18 / 1.2");
        assert_eq!(size("c"), u(21.6), "larger: 18 × 1.2");
    }

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
    /// ★ A cell's `height` is a floor. The row header wraps to several
    /// lines in a 60 px column; its `height: 40px` must not hold the row to
    /// one line, or the text paints over the row below.
    #[test]
    fn a_cell_height_is_a_floor_not_exact() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            table { border-spacing: 0px 0px } .c0 { width: 60px } .c1 { width: 60px }
            th { height: 40px } .a { background-color: #ff0000 } .b { background-color: #0000ff }</style>
            <table><colgroup><col class="c0"><col class="c1"></colgroup>
            <tr><th class="a">Mean days of rain over a tenth of a millimetre</th><td>1</td></tr>
            <tr><th class="b">x</th><td>2</td></tr></table>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (a, b) = (boxes(&o, 0xff0000ff), boxes(&o, 0x0000ffff));
        assert!(a[0].3 > 80, "the wrapped header grows its row past `height: 40px`: {:?}", a[0]);
        assert_eq!(b[0].1, a[0].1 + a[0].3, "the next row starts below the grown one");
        assert_eq!(b[0].3, 40, "a row whose content is shorter keeps the floor");
    }

    /// ★ ROWSPAN. A cell spanning two rows covers its column in both, so
    /// the second row's cells start one column further right; and a tall
    /// spanning cell grows the rows it spans, not just the first.
    #[test]
    fn a_rowspan_covers_its_column_and_grows_its_rows() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            table { border-spacing: 10px 10px } .c0 { width: 100px } .c1 { width: 50px } .c2 { width: 50px }
            .s { background-color: #ff0000; height: 200px } .a { background-color: #00ff00 } .d { background-color: #0000ff }
            .e { background-color: #ffff00 }</style>
            <table><colgroup><col class="c0"><col class="c1"><col class="c2"></colgroup>
            <tr><td class="s" rowspan="2">S</td><td class="a">a</td><td>b</td></tr>
            <tr><td class="d">d</td><td class="e">e</td></tr>
            <tr><td>f</td><td>g</td><td>h</td></tr></table>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert!(!o.unimplemented.keys().any(|k| k.contains("past the declared")), "{:?}", o.unimplemented);
        let (sp, a, d, e) = (boxes(&o, 0xff0000ff)[0], boxes(&o, 0x00ff00ff)[0], boxes(&o, 0x0000ffff)[0], boxes(&o, 0xffff00ff)[0]);
        assert_eq!((d.0, e.0), (10 + 100 + 10, 10 + 100 + 10 + 50 + 10), "row 2 starts after the column the span covers");
        assert_eq!(d.1, a.1 + a.3 + 10, "row 2 sits below row 1");
        assert_eq!(sp.3, a.3 + 10 + d.3, "the spanning cell fills both rows and the spacing between");
        assert_eq!(sp.3, 200, "and they grew to fit its 200 px floor");
    }

    /// ★ A baseline-aligned cell is pushed down to the row's baseline, and
    /// the row must still HOLD it (CSS 2.1 §17.5.3). Sized before the shift,
    /// the short cell's text hung below its row and over the next one.
    #[test]
    fn a_row_holds_its_baseline_shifted_cells() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            table { border-spacing: 0px 0px } .c0 { width: 50px } .c1 { width: 200px }
            td { vertical-align: baseline } .tall { display: inline-block; width: 10px; height: 100px }
            .n { background-color: #0000ff }</style>
            <table><colgroup><col class="c0"><col class="c1"></colgroup>
            <tr><td><span class="tall"></span></td><td>one<br>two<br>three<br>four</td></tr>
            <tr><td class="n">x</td><td>y</td></tr></table>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let next = boxes(&o, 0x0000ffff)[0];
        // Column 2's baselines in order: one, two, three, four, then y.
        let mut ys: Vec<U> = o.scene.runs.iter().filter(|r| r.x >= 50 * PX).map(|r| r.y).collect();
        ys.sort();
        ys.dedup();
        assert_eq!(ys.len(), 5, "{ys:?}");
        assert!(ys[0] / PX >= 100, "the text cell was shifted to the 100 px baseline: {}", ys[0] / PX);
        assert!(next.1 * PX >= ys[3], "the next row starts below \"four\": next {} four {}", next.1, ys[3] / PX);
    }

    /// ★ HTML's UA sheet: cells inherit `vertical-align: middle` from their
    /// row, and an author rule on the ROW reaches them.
    #[test]
    fn cells_are_middle_aligned_by_default_and_inherit_their_rows() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            table { border-spacing: 0px 0px } .c0 { width: 50px } .c1 { width: 50px }
            .tall { display: inline-block; width: 10px; height: 100px } .top { vertical-align: top }</style>
            <table><colgroup><col class="c0"><col class="c1"></colgroup>
            <tr><td><span class="tall"></span></td><td>x</td></tr>
            <tr class="top"><td><span class="tall"></span></td><td>y</td></tr></table>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let mut ys: Vec<U> = o.scene.runs.iter().filter(|r| r.x >= 50 * PX).map(|r| r.y / PX).collect();
        ys.sort();
        assert_eq!(ys.len(), 2, "{ys:?}");
        assert!((35..70).contains(&ys[0]), "x is centred in its 100 px row: baseline {}", ys[0]);
        assert!(ys[1] < 100 + 30, "y sits at the TOP of row 2, from `tr {{ vertical-align: top }}`: baseline {}", ys[1]);
    }

    /// ★ RIGHT-TO-LEFT. Column 1 of an RTL table is on the right; a sized
    /// box in an RTL block sits against the right edge; and a table too
    /// wide for its RTL container overflows on the LEFT.
    #[test]
    fn rtl_tables_run_from_the_right_and_overflow_to_the_left() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px; margin-right: 0px; direction: rtl }
            .box { width: 400px; margin-left: 0px; margin-right: 0px } .sized { width: 100px; background-color: #00ff00 }
            table { border-spacing: 0px 0px; margin-left: 0px; margin-right: 0px } .c0 { width: 100px } .c1 { width: 50px } .w { width: 500px }
            .a { background-color: #ff0000 } .b { background-color: #0000ff } .o { background-color: #ffff00 }</style>
            <div class="box"><div class="sized">s</div>
            <table><colgroup><col class="c0"><col class="c1"></colgroup><tr><td class="a">1</td><td class="b">2</td></tr></table>
            <table><colgroup><col class="w"></colgroup><tr><td class="o">wide</td></tr></table></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (sized, a, b, wide) = (boxes(&o, 0x00ff00ff)[0], boxes(&o, 0xff0000ff)[0], boxes(&o, 0x0000ffff)[0], boxes(&o, 0xffff00ff)[0]);
        // The 400 px box is itself sized, so it sits at the right of the
        // 800 px body: its edges are 400 and 800.
        assert_eq!((sized.0, sized.2), (800 - 100, 100), "the sized box sits against the right edge");
        assert_eq!((a.0, a.2), (800 - 100, 100), "column 1 is the RIGHTMOST, in a table shrink-wrapped to the right");
        assert_eq!((b.0, b.2), (800 - 150, 50), "column 2 is to its left");
        assert_eq!((wide.0, wide.2), (800 - 500, 500), "the overwide table overflows on the left, past the box's 400");
    }

    /// ★ An inline box whose direction differs from its parent's is an
    /// ISOLATE: `C++` written `dir=ltr` inside Hebrew keeps its `++` after
    /// the C. Unisolated, the two neutrals between an L and an R take the
    /// paragraph's rtl direction and move to the LEFT of the C.
    #[test]
    fn a_differing_direction_inline_is_an_isolate() {
        let o = render(r#"<style>p { direction: rtl } .l { direction: ltr }</style><p><span class="l">C++</span> שלום</p><p>C++ שלום</p>"#);
        // Each line's text, left to right.
        let mut lines: BTreeMap<U, Vec<(U, &str)>> = BTreeMap::new();
        for r in &o.scene.runs { lines.entry(r.y).or_default().push((r.x, r.text.as_str())) }
        let visual: Vec<String> = lines.into_values().map(|mut l| { l.sort(); l.iter().map(|x| x.1).collect::<String>() }).collect();
        assert_eq!(visual[0], "שלום C++", "isolated: `C++` whole, at the start (right) of the rtl line");
        assert_eq!(visual[1], "שלום ++C", "the control: unisolated, the neutrals take the rtl direction");
    }

    /// An isolate broken across lines is an isolate on each: its numbers
    /// count up left to right on every line. Unisolated, digits separated
    /// by spaces in an rtl paragraph run right to left.
    #[test]
    fn an_isolate_broken_across_lines_stays_in_order_on_each() {
        let o = render(r#"<style>p { direction: rtl; width: 120px } .l { direction: ltr }</style><p>שלום <span class="l">10 20 30 40 50 60 70 80 90</span> עולם</p>"#);
        let mut lines: BTreeMap<U, Vec<(U, &str)>> = BTreeMap::new();
        for r in &o.scene.runs { lines.entry(r.y).or_default().push((r.x, r.text.as_str())) }
        assert!(lines.len() >= 3, "it wraps: {lines:?}");
        let order = ["10", "20", "30", "40", "50", "60", "70", "80", "90"];
        for (_, mut l) in lines {
            l.sort();
            let latin: Vec<&str> = l.iter().map(|x| x.1).filter(|t| order.contains(t)).collect();
            assert!(latin.len() != 1 || l.len() > 1, "{l:?}");
            let pos: Vec<usize> = latin.iter().map(|t| order.iter().position(|o| o == t).unwrap()).collect();
            assert!(pos.windows(2).all(|w| w[0] < w[1]), "left to right inside the isolate on every line: {l:?}");
        }
    }

    /// ★ text-indent is at the START of the first line: the right in rtl.
    #[test]
    fn text_indent_is_on_the_right_in_rtl() {
        let at = |css: &str| render(&format!("<style>body {{ margin-right: 0px }} p {{ direction: rtl; {css} }}</style><p>שלום</p>")).scene.runs[0].x;
        assert_eq!(at("text-indent: 0px") - at("text-indent: 40px"), 40 * PX, "the indent pushes the first line LEFT, from the right edge");
    }

    /// ★ A list marker hangs off the start side: right of an rtl list item,
    /// with `1.` read right to left as `.1`.
    #[test]
    fn list_markers_hang_on_the_right_in_rtl() {
        let o = render(r#"<style>body { margin-right: 8px } ul, ol { direction: rtl; padding-left: 0px; padding-right: 40px }</style>
            <ul><li>שלום</li></ul><ol><li>עולם</li></ol>"#);
        let r = |t: &str| o.scene.runs.iter().find(|r| r.text == t).unwrap_or_else(|| panic!("no {t}: {:?}", o.scene.runs.iter().map(|r| &r.text).collect::<Vec<_>>()));
        let content_right = (800 - 8 - 40) * PX;
        assert!(r("\u{2022}").x > content_right, "the bullet is right of the content box: {}", r("\u{2022}").x / PX);
        assert!(r(".1").x > content_right, "the number too, ordered rtl");
        assert!(r("שלום").x < content_right);
    }

    /// ★ An rtl flex ROW starts at the right: items run right to left,
    /// `flex-start` packs them right and `flex-end` left, and the margin on
    /// an item's start side is its right one. A COLUMN's cross-start is the
    /// right too.
    #[test]
    fn an_rtl_flex_row_runs_from_the_right() {
        let src = r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .f { display: flex; direction: rtl; width: 400px } .e { justify-content: flex-end } .c { flex-direction: column; align-items: flex-start }
            .a { width: 100px; height: 10px; background-color: #ff0000; margin-right: 20px } .b { width: 50px; height: 10px; background-color: #0000ff }
            .x { width: 60px; height: 10px; background-color: #00ff00 }</style>
            <div class="f"><div class="a"></div><div class="b"></div></div>
            <div class="f e"><div class="b"></div></div>
            <div class="f c"><div class="x"></div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (a, b, x) = (boxes(&o, 0xff0000ff)[0], boxes(&o, 0x0000ffff), boxes(&o, 0x00ff00ff)[0]);
        assert_eq!(a.0, 400 - 20 - 100, "the first item at the right, after its start (right) margin");
        assert_eq!(b[0].0, 400 - 20 - 100 - 50, "the second to its left");
        assert_eq!(b[1].0, 0, "flex-end packs to the LEFT in rtl");
        assert_eq!(x.0, 400 - 60, "a column's cross-start is the right edge");
    }

    /// ★ Flexible lengths FREEZE (Flexbox §9.7): an item clamped by its
    /// min or max size is frozen and the free space is shared again among
    /// the rest. Here the first item cannot shrink below 300 px, so the
    /// second gets what is left (100), not the half it would have had.
    /// Growing, an item capped by max-width gives its excess to the other.
    #[test]
    fn flexible_lengths_freeze_items_at_their_limits() {
        let src = r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .f { display: flex; width: 400px } .s > * { width: 100%; height: 10px }
            .a { min-width: 300px; background-color: #ff0000 } .b { background-color: #0000ff }
            .g > * { flex-grow: 1; height: 10px; width: 0px } .c { max-width: 50px; background-color: #00ff00 } .d { background-color: #ffff00 }</style>
            <div class="f s"><div class="a"></div><div class="b"></div></div>
            <div class="f g"><div class="c"></div><div class="d"></div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (a, b) = (boxes(&o, 0xff0000ff)[0], boxes(&o, 0x0000ffff)[0]);
        assert_eq!((a.2, b.0, b.2), (300, 300, 100), "the min-clamped item is frozen and the other takes the rest");
        let (c, d) = (boxes(&o, 0x00ff00ff)[0], boxes(&o, 0xffff00ff)[0]);
        assert_eq!((c.2, d.2), (50, 350), "the max-clamped item is frozen and its excess goes to the other");
    }

    /// ★ A child's `min-width`/`max-width` clamp its INTRINSIC contribution
    /// (CSS Sizing §5.2): a shrink-to-fit box around a `min-width: 300px`
    /// child is 300 wide, and around a long line capped at `max-width:
    /// 100px` it is 100. Both in block and in row-flex measurement.
    #[test]
    fn min_and_max_width_clamp_an_intrinsic_contribution() {
        let src = r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .ib { display: inline-block; background-color: #ff0000; height: 10px } .fx { display: inline-block; background-color: #0000ff; height: 10px }
            .row { display: flex } .lo { min-width: 300px; height: 10px } .hi { max-width: 100px; height: 10px }</style>
            <div><div class="ib"><div class="lo"></div></div></div>
            <div><div class="ib"><div class="hi">a long run of words that would be far wider than one hundred pixels</div></div></div>
            <div><div class="fx"><div class="row"><div class="lo"></div><div class="lo"></div></div></div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let red = boxes(&o, 0xff0000ff);
        assert_eq!(red[0].2, 300, "min-width raises the contribution");
        assert_eq!(red[1].2, 100, "max-width caps it");
        assert_eq!(boxes(&o, 0x0000ffff)[0].2, 600, "a row flex container sums the clamped contributions");
        let t = render(r#"<style>body { margin-left: 0px; margin-top: 0px } .ib { display: inline-block; background-color: #ff0000; height: 10px }
            .t { display: table; max-width: 30px } .c { width: 60px; height: 10px }</style>
            <div><div class="ib"><div class="t"><div class="c"></div></div></div></div>"#);
        assert_eq!(boxes(&t, 0xff0000ff)[0].2, 60, "a table's max-width never caps it below its content (CSS 2.1 §17.5.2)");
    }

    /// ★ Flex AUTO MARGINS (Flexbox §8.1). On the main axis they take the
    /// line's free space before `justify-content` sees any — `margin-left:
    /// auto` is THE idiom for pushing the last nav item to the right — and
    /// in an rtl row the auto margin on the start (right) side pushes the
    /// item left. On the cross axis they centre or push the item, beat
    /// `align-self` and stop it stretching. When the line overflows they are
    /// zero.
    #[test]
    fn flex_auto_margins_take_the_free_space() {
        let src = r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .f { display: flex; width: 400px; justify-content: center } .r { direction: rtl } .t { height: 100px; align-items: stretch }
            .c { flex-direction: column; justify-content: flex-start } .o { width: 100px; justify-content: flex-start }
            .a { width: 50px; height: 10px; background-color: #ff0000 }
            .b { width: 50px; height: 10px; background-color: #0000ff; margin-left: auto }
            .m { width: 60px; height: 10px; background-color: #00ff00; margin-left: auto; margin-right: auto }
            .s { width: 50px; height: 10px; background-color: #ffff00; margin-right: auto }
            .v { width: 20px; margin-top: auto; margin-bottom: auto } .e { margin-top: auto }
            .k { height: 10px; background-color: #ff00ff }
            .w { max-width: 200px; height: 10px; background-color: #00ffff; margin-left: auto; margin-right: auto }
            .q { width: 300px; height: 10px }
            .n { width: 80px; flex-shrink: 0; height: 10px; background-color: #808080 }
            .n2 { width: 80px; flex-shrink: 0; height: 10px; background-color: #800000; margin-left: auto }</style>
            <div class="f"><div class="a"></div><div class="b"></div></div>
            <div class="f"><div class="m"></div></div>
            <div class="f r"><div class="s"></div></div>
            <div class="f t"><div class="v"><div class="k"></div></div><div class="e"><div class="k"></div></div></div>
            <div class="f c"><div class="w"><div class="q"></div></div></div>
            <div class="f o"><div class="n"></div><div class="n2"></div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (a, b) = (boxes(&o, 0xff0000ff)[0], boxes(&o, 0x0000ffff)[0]);
        assert_eq!((a.0, b.0), (0, 350), "the auto margin takes all the free space; justify-content: center sees none");
        assert_eq!(boxes(&o, 0x00ff00ff)[0].0, 170, "two auto margins share it: centred");
        assert_eq!(boxes(&o, 0xffff00ff)[0].0, 0, "rtl: margin-right is the start side, and its auto pushes the item left");
        let k = boxes(&o, 0xff00ffff);
        assert_eq!(k[0].3, 10, "a cross-auto item is not stretched");
        let top = 10 + 10 + 10;
        assert_eq!(k[0].1, top + 45, "auto top and bottom centre it in the 100px line");
        assert_eq!(k[1].1, top + 90, "auto top alone pushes it to the bottom");
        let w = boxes(&o, 0x00ffffff)[0];
        assert_eq!((w.0, w.2), (100, 200), "the max-width + margin: auto idiom centres a column item");
        assert_eq!(boxes(&o, 0x800000ff)[0].0, 80, "an overflowing line gives its auto margins nothing");
        let d = render(r#"<style>body { margin-left: 0px; margin-top: 0px } .f { display: flex; flex-direction: column; width: 70px }
            .b { width: 200px; height: 10px; background-color: #0000ff }</style><div class="f"><div class="b"></div></div>"#);
        assert_eq!(boxes(&d, 0x0000ffff)[0].2, 200, "a definite width in a narrower column is that width, overflowing");
    }

    /// ★ In an rtl grid column line 1 is on the right, and an item's
    /// `start` alignment is its right edge; negative lines still count from
    /// the end (now the left).
    #[test]
    fn an_rtl_grid_numbers_its_columns_from_the_right() {
        let src = r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .g { display: grid; direction: rtl; width: 300px; grid-template-columns: 100px 100px 100px; column-gap: 0px }
            .a { background-color: #ff0000; height: 10px } .b { background-color: #0000ff; height: 10px; grid-column-start: 3 }
            .s { background-color: #00ff00; height: 10px; width: 40px; justify-self: start } .e { background-color: #ffff00; height: 10px; width: 40px; justify-self: end }
            .n { background-color: #00ffff; height: 10px; grid-column-start: -2; grid-column-end: -1 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>
            <div class="g"><div class="s"></div><div class="e"></div><div class="n"></div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let one = |c: u32| boxes(&o, c)[0];
        assert_eq!((one(0xff0000ff).0, one(0xff0000ff).2), (200, 100), "auto-placed into column 1: the RIGHTMOST");
        assert_eq!(one(0x0000ffff).0, 0, "column 3 is the leftmost");
        assert_eq!(one(0x00ff00ff).0, 300 - 40, "justify-self: start is the cell's right edge");
        assert_eq!(one(0xffff00ff).0, 100, "end is its left edge (cell 2 spans 100..200)");
        assert_eq!(one(0x00ffffff).0, 0, "-2 / -1 is the last column, on the left");
    }

    /// ★ A percentage resolves by TRUNCATION to the 1/64 px grid: two
    /// `flex-basis: 50%` items in an odd-width wrapping row fit on one line,
    /// as in a browser. Rounded, each was 1/64 px too wide together.
    #[test]
    fn percentages_truncate_so_halves_fit_one_line() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; flex-wrap: wrap; width: calc(700px + 1px / 64) }
            .h { flex-basis: 50%; flex-grow: 0; flex-shrink: 0; max-width: 50%; height: 10px }
            .a { background-color: #ff0000 } .b { background-color: #0000ff }</style>
            <div class="f"><div class="h a"></div><div class="h b"></div></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert_eq!(boxes(&o, 0xff0000ff)[0].1, boxes(&o, 0x0000ffff)[0].1, "one line");
        assert_eq!(boxes(&o, 0x0000ffff)[0].0, 350);
    }

    /// ★ A replaced box's intrinsic sizes are its OWN size: an `<img>` flex
    /// item's content basis is its natural width (clamped by `max-width`,
    /// as is its automatic minimum), and a block-level image makes a
    /// `min-content` box as wide as itself — unless a percentage sizes it,
    /// which makes it COMPRESSIBLE (CSS Sizing 3 §5.2.2). Sizes are
    /// headless Chrome's.
    #[test]
    fn a_replaced_box_measures_as_its_own_size() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; width: 750px; justify-content: center }
            .i { max-height: 557px; max-width: 100%; background-color: #00ff00 }
            .m { width: min-content; background-color: #0000ff } .b { display: block }
            .c { width: min-content; height: 5px; background-color: #ff0000 } .p { max-width: 100% }</style>
            <div class="f"><img class="i" src="a.png" width="1536" height="1024"></div>
            <div class="m"><img class="b" src="b.png" width="300" height="100"></div>
            <div class="c"><img class="b p" src="b.png" width="300" height="100"></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let wh = |c: u32| boxes(&o, c).first().map(|r| (r.2, r.3));
        assert_eq!(wh(0x00ff00ff), Some((750, 500)), "natural width, clamped by max-width; height by the ratio");
        assert_eq!(wh(0x0000ffff), Some((300, 100)), "a min-content box around a block image");
        assert_eq!(wh(0xff0000ff).map(|w| w.0), Some(0), "…but a `max-width: 100%` image is compressible: it contributes 0");
    }

    /// ★ A width IMPOSED on a replaced box — stretched across its grid area
    /// while its block axis is not — is its used width, and an auto height
    /// follows it through the ratio (WPT replaced-alignment-with-aspect-
    /// ratio-008; Chrome: 100 × 100). It kept the natural 50.
    #[test]
    fn an_imposed_width_gives_a_replaced_box_its_ratio_height() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; grid-template-columns: 100px; grid-template-rows: 100px; width: 100px; height: 100px }
            img { background-color: #00ff00; justify-self: stretch; align-self: start }</style>
            <div class="g"><img src="a.png" width="50" height="50"></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff).first().map(|r| (r.2, r.3)), Some((100, 100)));
    }

    /// ★ A grid item whose HEIGHT is settled — stretched in the block axis
    /// alone, or given as a height that resolves against its area — takes
    /// its auto width from it through its ratio: an image's, or an
    /// `aspect-ratio` (WPT replaced-alignment-with-aspect-ratio-009,
    /// grid-in-table-cell-with-img). Sizes are headless Chrome's.
    #[test]
    fn a_settled_height_gives_a_grid_item_its_ratio_width() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; grid-template-columns: 100px; grid-template-rows: 100px; width: 100px; height: 100px }
            .s { background-color: #00ff00; justify-self: start; align-self: stretch }
            .p { background-color: #0000ff; justify-self: start; align-self: start; height: 100% }
            .r { background-color: #ff0000; justify-self: start; align-self: start; height: 50px; aspect-ratio: 1 }</style>
            <div class="g"><img class="s" src="a.png" width="50" height="50"></div>
            <div class="g"><img class="p" src="b.png" width="200" height="200"></div>
            <div class="g"><div class="r"></div></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let wh = |c: u32| boxes(&o, c).first().map(|r| (r.2, r.3));
        assert_eq!(wh(0x00ff00ff), Some((100, 100)), "stretched height, width through the ratio");
        assert_eq!(wh(0x0000ffff), Some((100, 100)), "height: 100% of the area, not the natural 200 wide");
        assert_eq!(wh(0xff0000ff), Some((50, 50)), "aspect-ratio from a definite height");
    }

    /// ★ A flex item's content-based flex base size is its MAX-CONTENT
    /// size, not that size fitted to the container (Flexbox §9.2): fitting
    /// the line is the shrink step's job. Clamped, Thomann's teaser slide
    /// (`flex-shrink: 0`, five `max-width: 100%` images) was 800 wide and its
    /// images 152 px; and LinkedIn's photo, which may not shrink below its
    /// 400 px automatic minimum, took a share sized by the CLAMPED text
    /// base. Sizes are headless Chrome's.
    #[test]
    fn a_flex_base_size_is_the_unclamped_max_content() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .t { display: flex; width: 800px } .v { display: flex; flex-shrink: 0 }
            .a { display: inline-block; padding-left: 4px; padding-right: 4px }
            .i { display: block; max-width: 100%; background-color: #00ff00 }
            .l { display: flex; width: 700px; justify-content: space-between }
            .x { width: 564px; margin-right: 64px; max-width: 400px; height: 10px; background-color: #0000ff }
            .y { width: 450px; max-width: 400px; background-color: #ff0000 }
            .s { display: flex; width: 100px } .n { flex-shrink: 0; height: 10px; background-color: #ffff00 }</style>
            <div class="t"><div class="v"><a class="a"><img class="i" src="a.png" width="1000" height="406"></a><a class="a"><img class="i" src="a.png" width="1000" height="406"></a></div></div>
            <div class="l"><div class="x">some words of text that wrap and wrap again over several lines</div><img class="y" src="b.png" width="926" height="926"></div>
            <div class="s"><div class="n">unshrinkable words</div></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let g = boxes(&o, 0x00ff00ff);
        assert_eq!(g.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(), vec![(4, 1000), (1012, 1000)],
            "a flex-shrink: 0 slide is as wide as its images together (Chrome 1008 each), not the 800 px container");
        assert_eq!(boxes(&o, 0xff0000ff)[0].2, 400, "the photo holds its automatic minimum, 400");
        assert_eq!(boxes(&o, 0x0000ffff)[0].2, 236, "and the text takes what is left (700 - 64 - 400)");
        assert!(boxes(&o, 0xffff00ff)[0].2 > 100, "an unshrinkable item overflows a narrower container");
    }

    /// ★ `align-content: center | flex-end` are UNSAFE (Flexbox §8.4): lines
    /// taller than their container overflow it equally on both sides, or on
    /// the start side. With no positive free space they were left at the
    /// start. Expected offsets are headless Chrome's.
    #[test]
    fn align_content_overflows_unsafely() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px; margin-right: 0px }
            .f { display: flex; flex-wrap: wrap; width: 100px; height: 40px; margin-top: 100px } .i { width: 100px; height: 40px }
            .c { align-content: center } .e { align-content: flex-end }
            .a { background-color: #ff0000 } .b { background-color: #0000ff } .g { background-color: #00ff00 }</style>
            <div class="f c"><div class="i a"></div><div class="i b"></div></div>
            <div class="f e"><div class="i g"></div><div class="i b"></div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let one = |c: u32| boxes(&o, c)[0];
        assert_eq!(one(0xff0000ff).1, 80, "centred: 20 px above the container");
        assert_eq!(one(0x00ff00ff).1, 200, "flex-end: the overflow is at the start");
        assert_eq!(boxes(&o, 0x0000ffff).iter().map(|b| b.1).collect::<Vec<_>>(), [120, 240]);
    }

    /// ★ An absolutely positioned box is out of flow, and takes no part in
    /// its container's min-content (CSS Sizing 3 §5): a caption laid over a
    /// photo must not stop the photo's 50% flex half from shrinking to it.
    #[test]
    fn an_absolute_child_does_not_size_its_container() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px; margin-right: 0px }
            .f { display: flex; width: 400px } .h { flex-basis: 50%; height: 10px; position: relative }
            .a { background-color: #ff0000 } .b { background-color: #0000ff }
            .c { position: absolute; left: 0px; right: 0px; bottom: 0px }</style>
            <div class="f"><div class="h a"><div class="c">Pneumonoultramicroscopicsilicovolcanoconiosis-and-then-some-more</div></div><div class="h b">x</div></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let one = |c: u32| boxes(&o, c)[0];
        assert_eq!((one(0xff0000ff).0, one(0xff0000ff).2), (0, 200), "the half shrinks to its basis");
        assert_eq!((one(0x0000ffff).0, one(0x0000ffff).2), (200, 200));
    }

    /// ★ Out-of-flow boxes written inside a line wait for that line box —
    /// and belong to IT. Laid out while a sibling was still waiting, the
    /// first of two `position: absolute` spans placed the second at its own
    /// content origin (and itself on the second's line); an inline-block
    /// laid out on the same line did the same. Expected offsets are
    /// headless Chrome's.
    #[test]
    fn waiting_absolute_boxes_belong_to_their_own_line() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px; margin-right: 0px }
            .d { position: relative; width: 320px; height: 96px }
            .s { position: absolute; display: inline-block; width: 128px; height: 48px }
            .a { top: 0px; left: 16px; background-color: #ff0000 } .b { top: 48px; left: 176px; background-color: #0000ff }
            .i { display: inline-block; width: 30px; height: 10px; background-color: #00ff00 }
            .c { top: 4px; left: 200px; background-color: #ffff00 } .e { top: 4px; left: 200px; background-color: #00ffff }</style>
            <div class="d"><span class="s a">one</span> <span class="s b">two</span></div>
            <div class="d">x <span class="s c">c</span><span class="i">i</span><span class="s e">e</span></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let one = |c: u32| boxes(&o, c)[0];
        assert_eq!((one(0xff0000ff).0, one(0xff0000ff).1), (16, 0), "its own offsets, not the next box's");
        assert_eq!((one(0x0000ffff).0, one(0x0000ffff).1), (176, 48));
        assert_eq!((one(0xffff00ff).0, one(0xffff00ff).1), (200, 96 + 4), "not moved into the inline-block");
    }

    /// ★ …and they paint in DOCUMENT order (CSS 2.1 Appendix E): the later
    /// of two overlapping positioned boxes is on top. Taken from the end of
    /// the waiting list, the first covered the second.
    #[test]
    fn waiting_absolute_boxes_paint_in_document_order() {
        let src = r#"<style>.d { position: relative; width: 320px; height: 96px }
            .s { position: absolute; top: 0px; left: 0px; width: 50px; height: 50px }
            .a { background-color: #ff0000 } .b { background-color: #00ff00 }</style>
            <div class="d">x<span class="s a"></span><span class="s b"></span></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let order: Vec<u32> = rects(&o).iter().map(|r| r.rgba).filter(|c| [0xff0000ff, 0x00ff00ff].contains(c)).collect();
        assert_eq!(order, [0xff0000ff, 0x00ff00ff], "the later box paints on top");
    }

    /// ★ CSS Flexbox §4.1: an absolutely positioned child of a flex
    /// container takes its static position as the container's SOLE item —
    /// justify-content on the main axis, align-self on the cross axis, rtl
    /// flipping the inline start. Every expected offset is headless Chrome's.
    #[test]
    fn an_absolute_flex_child_is_placed_as_the_sole_flex_item() {
        let cases: &[(&str, &str, (U, U))] = &[
            ("justify-content: center; align-items: center", "", (175, 40)),
            ("justify-content: flex-end; align-items: flex-end", "", (345, 80)),
            ("justify-content: space-around", "", (175, 0)),
            ("justify-content: space-between; align-items: stretch", "", (5, 0)),
            ("flex-direction: column; justify-content: center; align-items: flex-end", "", (345, 40)),
            ("direction: rtl; justify-content: flex-start; align-items: center", "align-self: flex-start", (345, 0)),
            // align-content does not move it, even in a wrapping container.
            ("flex-wrap: wrap; align-content: center; align-items: flex-end", "", (5, 80)),
            ("flex-direction: column; direction: rtl; align-items: flex-start; justify-content: flex-end", "", (345, 80)),
            // The FINAL height: auto, raised to min-height.
            ("height: auto; min-height: 60px; align-items: flex-end; justify-content: center", "", (175, 40)),
            // Oversized and centred: unsafe, so it starts before the box.
            ("justify-content: center; align-items: center", "width: 500px; height: 150px", (-50, -25)),
        ];
        for (k, (c, a, want)) in cases.iter().enumerate() {
            let src = format!(r#"<style>body {{ margin-left: 0px; margin-right: 0px; margin-top: 0px }}
                .c {{ display: flex; position: relative; width: 400px; height: 100px; {c} }}
                .a {{ position: absolute; width: 50px; height: 20px; margin-left: 5px; margin-right: 5px; background-color: #ff0000; {a} }}
                .k {{ width: 10px; height: 30px }}</style>
                <div class="c"><div class="a"></div>{}</div>"#, if k == 8 { r#"<div class="k"></div>"# } else { "" });
            let o = render(&src);
            assert!(o.diagnostics.is_empty(), "{c}: {:?}", o.diagnostics);
            let b = boxes(&o, 0xff0000ff)[0];
            assert_eq!((b.0, b.1), *want, "{c} / {a}");
        }
    }

    /// ★ With both offsets auto, an absolutely positioned box in an rtl
    /// containing block puts its RIGHT edge at the static position; with
    /// both given and a width, `left` is the one ignored (CSS 2.1 §10.3.7).
    #[test]
    fn an_rtl_absolute_box_takes_its_static_position_from_the_right() {
        let src = r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .p { position: relative; direction: rtl; width: 400px; height: 100px }
            .a { position: absolute; width: 50px; height: 10px; background-color: #ff0000 }
            .o { position: absolute; left: 10px; right: 30px; width: 50px; height: 10px; background-color: #0000ff }
            .i { position: absolute; width: 20px; height: 10px; background-color: #00ff00 }</style>
            <div class="p"><div class="a"></div><div class="o"></div><p>שלום<span class="i"></span></p></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert_eq!(boxes(&o, 0xff0000ff)[0].0, 400 - 50, "a block-level static position, from the right");
        assert_eq!(boxes(&o, 0x0000ffff)[0].0, 400 - 30 - 50, "over-constrained: right wins in rtl");
        assert_eq!(boxes(&o, 0x00ff00ff)[0].0, 400 - 20, "an inline one's line starts at the right too");
    }

    /// ★ The pieces of a right-to-left item are laid out right to left: the
    /// full stop after an Arabic word (a different face, so another piece)
    /// is at the word's LEFT, the end of an rtl sentence.
    #[test]
    fn the_pieces_of_an_rtl_word_run_right_to_left() {
        let o = render(r#"<style>p { direction: rtl }</style><p>الرأي العام.</p>"#);
        let x = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.x).unwrap_or_else(|| panic!("no {t}: {:?}", o.scene.runs.iter().map(|r| &r.text).collect::<Vec<_>>()));
        assert!(x(".") < x("العام") && x("العام") < x("الرأي"), "visual order . العام الرأي: {} {} {}", x("."), x("العام"), x("الرأي"));
    }

    /// ★ A NESTED table is not a wrapper. A `display: table` div around a
    /// real table with a spanning title row must not adopt that row: the
    /// outer box is one anonymous cell, and the inner table lays itself out
    /// from its own `<col>`s.
    #[test]
    fn a_table_inside_a_css_table_keeps_its_own_rows() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .outer { display: table } table { border-spacing: 10px 0px } .c0 { width: 100px } .c1 { width: 300px }
            .t { background-color: #ff0000 } .l { background-color: #00ff00 }</style>
            <div class="outer"><table><colgroup><col class="c0"><col class="c1"></colgroup>
            <tr><th class="t" colspan="2">Title</th></tr>
            <tr><th class="l">People</th><td>Bob Fabry</td></tr></table></div>"#;
        let o = render(src);
        assert!(o.diagnostics.is_empty(), "the outer box is not refused: {:?}", o.diagnostics);
        let (t, l) = (boxes(&o, 0xff0000ff), boxes(&o, 0x00ff00ff));
        assert_eq!((t[0].0, t[0].2), (10, 100 + 10 + 300), "the inner title spans the inner table's columns");
        assert_eq!((l[0].0, l[0].2), (10, 100));
    }

    /// ★ A table's `height` is a minimum (CSS 2.1 §17.5.3): a table whose
    /// rows need more grows, and the box after it starts below them.
    #[test]
    fn a_table_grows_past_its_specified_height() {
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px; font-size: 12px; line-height: 20px }
            .t { display: table; height: 36px } .c { display: table-cell } .after { background-color: #ff0000; height: 10px }</style>
            <div class="t"><div class="c"><div>one</div><div>two</div><div>three</div><div>four</div></div></div>
            <div class="after"></div>"#;
        let o = render(src);
        let after = boxes(&o, 0xff0000ff);
        assert_eq!(after[0].1, 80, "four 20 px lines, not the 36 px the table asked for");
        // A short table still takes its height.
        let o = render(&src.replace("<div>two</div><div>three</div><div>four</div>", ""));
        assert_eq!(boxes(&o, 0xff0000ff)[0].1, 36);
    }

    /// ★ `pre` never soft-wraps: a code line made of several elements
    /// overflows its box (and its clip) instead of breaking between them.
    #[test]
    fn pre_does_not_wrap_between_elements() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px } pre { width: 120px }</style>
            <pre>    <span>let</span> s = <span>"hello"</span>;   <span>// s is valid from this point forward</span>
}</pre>"#);
        let y = |t: &str| o.scene.runs.iter().find(|r| r.text.contains(t)).map(|r| r.y).unwrap_or_else(|| panic!("{t}"));
        assert_eq!((y("let"), y("hello")), (y("valid"), y("valid")), "one source line, one line box");
        assert!(y("}") > y("let"));
        // And a shrink-to-fit box holds a `nowrap` line made of elements.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px } .b { display: inline-block; white-space: nowrap; background-color: #ff0000 }</style>
            <div style="width: 1px"><span class="b"><a>Home</a> <a>Docs</a> <a>Blog</a></span></div>"#);
        let ys: std::collections::BTreeSet<U> = o.scene.runs.iter().map(|r| r.y).collect();
        assert_eq!(ys.len(), 1, "one line");
        let b = boxes(&o, 0xff0000ff);
        let right = o.scene.runs.iter().map(|r| r.x).max().unwrap() / PX;
        assert!(b[0].2 > right, "the box ({} px) reaches past its last word ({right} px)", b[0].2);
    }

    /// ★ Words that touch across elements wrap only where UAX #14 allows:
    /// IKEA's price is `<span>399</span><span>.</span>00`, and a line that
    /// broke between them put ".00" on a line of its own. The glued chain
    /// moves to the next line together.
    #[test]
    fn touching_words_wrap_together() {
        // A box just wide enough for "Price 399", not for its ".00".
        let probe = render(r#"<style>body { margin-left: 0px; margin-top: 0px } div { font-size: 16px }</style>
            <div>Price <span>399</span><span>.</span>00</div>"#);
        let dot = probe.scene.runs.iter().find(|r| r.text == ".").expect("dot").x;
        let o = render(&format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }} div {{ width: {}px; font-size: 16px }}</style>
            <div>Price <span>399</span><span>.</span>00</div>"#, dot / PX + 2));
        let y = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.y).unwrap_or_else(|| panic!("{t}: {:?}", o.scene.runs.iter().map(|r| &r.text).collect::<Vec<_>>()));
        assert_eq!((y("399"), y(".")), (y("00"), y("00")), "399.00 stays on one line");
        assert!(y("Price") < y("399"), "and goes to the next line together");
        // Spaced words and CJK segments still break.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px } div { width: 40px }</style><div>北京四季分明</div>"#);
        assert!(o.scene.runs.iter().map(|r| r.y).collect::<std::collections::BTreeSet<_>>().len() > 1);
    }

    /// ★ A `nowrap` span of several words moves to the next line whole
    /// when it does not fit, like a glued word — Wikipedia's
    /// "[timeframe?]" ran off the end of its line.
    #[test]
    fn a_nowrap_span_moves_down_whole() {
        // UAX #14 would break after the hyphen; `nowrap` does not.
        let src = |w: &str| format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }} .n {{ white-space: nowrap }} div {{ width: {w} }}</style>
            <div>This is ongoing <span class="n"><b>well-</b>known</span></div>"#);
        let probe = render(&src("auto"));
        let k = probe.scene.runs.iter().find(|r| r.text == "known").expect("run").x;
        let o = render(&src(&format!("{}px", k / PX + 4)));
        let y = |s: &str| o.scene.runs.iter().find(|r| r.text == s).map(|r| r.y).unwrap_or_else(|| panic!("{s}"));
        assert_eq!(y("well-"), y("known"), "the span stays together");
        assert!(y("well-") > y("This"), "on the next line");
    }

    /// ★ UAX #14. CJK breaks between ideographs (punctuation stays with
    /// what it closes), Thai between dictionary words, and a Latin run
    /// after its hyphens and slashes — never inside a word or a number.
    #[test]
    fn unspaced_scripts_break_between_words() {
        assert_eq!(uax14_segments("北京四季分明，春秋"), ["北", "京", "四", "季", "分", "明，", "春", "秋"]);
        assert_eq!(uax14_segments("สวัสดีครับ"), ["สวัสดี", "ครับ"]);
        assert_eq!(uax14_segments("well-known/path"), ["well-", "known/", "path"]);
        assert_eq!(uax14_segments("don't"), ["don't"]);
        assert_eq!(uax14_segments("1,234.56"), ["1,234.56"]);
        assert_eq!(uax14_segments("(-5)"), ["(-5)"]);
        assert_eq!(uax14_segments("Com\u{ad}puted"), ["Com\u{ad}puted"]);
        assert_eq!(uax14_segments("2019年PM2.5"), ["2019", "年", "PM2.5"]);
        // And in layout: a paragraph with no spaces wraps INSIDE its box.
        let o = render(&format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }} div {{ width: 200px }}</style><div>{}</div>"#, "北京四季分明".repeat(10)));
        let mut ys: Vec<U> = o.scene.runs.iter().map(|r| r.y).collect();
        ys.sort();
        ys.dedup();
        assert!(ys.len() >= 3, "sixty ideographs in 200 px take several lines: {}", ys.len());
        assert!(o.scene.runs.iter().all(|r| r.x < 200 * PX), "nothing starts past the box");
        // A long URL wraps at its slashes instead of running off the line.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px } div { width: 200px }</style><div>https://www.example.edu/Documents/ADMINISTRATION/CENTERS/handouts/how-to-tie-a-tie.pdf</div>"#);
        assert!(o.scene.runs.len() >= 3, "{}", o.scene.runs.len());
        let fonts = FontSet::load().expect("pinned font set");
        let right = o.scene.runs.iter().map(|r| {
            let ff = fonts.faces[r.face].parse();
            r.x + r.glyphs.iter().map(|&(g, dx, _)| dx + ff.glyph_hor_advance(ttf_parser::GlyphId(g)).unwrap_or(0) as U * r.size / fonts.faces[r.face].upem).max().unwrap_or(0)
        }).max().unwrap();
        assert!(right <= 200 * PX, "every line ends inside the box: {}", right / PX);
        assert_eq!(o.report.overflow_lines, 0, "no line overflows");
    }

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
        assert!(text.concat().contains("author-wins"), "author display beats the UA [hidden] rule: {text:?}");
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

    #[test]
    fn an_absolute_box_pushed_past_its_containing_block_keeps_its_min_content_width() {
        // BBC News's seat-change badge: `right: calc(100% + 2px)` against a
        // 40 px containing block leaves NO room, and shrink-to-fit is
        // min(max(min-content, available), max-content) (CSS 2.1 §10.3.7) —
        // the min-content floor is not clamped by the available width. At 0
        // wide the badge's content spilled rightwards over the number it
        // sits beside.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .cb { position: relative; margin-left: 100px; width: 40px; height: 10px }
            .p { position: absolute; right: calc(100% + 2px); top: 0px; height: 10px; background-color: #0000ff }
            .i { width: 20px; height: 10px }</style>
            <div class="cb"><div class="p"><div class="i"></div></div></div>"#);
        // 20 wide, its right edge 2 px left of the containing block's left.
        assert_eq!(boxes(&o, 0x0000ffff), vec![(78, 0, 20, 10)]);
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

    /// ★ Premeasurement places cells by the same slots as layout: the
    /// second row's only cell sits in column 2, under the span's neighbour,
    /// so ITS width is what column 2 must hold — not column 1's.
    #[test]
    fn premeasure_measures_a_cell_below_a_rowspan_in_its_own_column() {
        let src = r#"<style>table { display: table } tr { display: table-row } td { display: table-cell }
            .s { background-color: #ff0000 } .l { background-color: #0000ff }</style>
            <table><tr><td class="s" rowspan="2">x</td><td>y</td></tr>
            <tr><td class="l">a much longer cell than the first</td></tr></table>"#;
        let fonts = FontSet::load().unwrap();
        let (fixed, n) = premeasure_tables(src, &fonts, &Env::default());
        assert_eq!(n, 2, "two columns measured");
        let o = render_html(&fixed, &fonts, &Env::default());
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (sp, l) = (boxes(&o, 0xff0000ff)[0], boxes(&o, 0x0000ffff)[0]);
        assert!(l.0 >= sp.0 + sp.2, "the long cell is in column 2: {sp:?} {l:?}");
        assert!(l.2 > 4 * sp.2, "and column 2 is the wide one: {sp:?} {l:?}");
    }

    #[test]
    fn text_size_remeasures_columns_exactly_not_by_scaling() {
        // A column with a fixed part (an image's 120 px) and text beside it:
        // scaling the 100% widths would double the image's share too.
        let src = r#"<style>td { padding-left: 6px; padding-right: 6px } .p { width: 120px; height: 10px; background-color: #ff0000 }
            .t { background-color: #0000ff }</style>
            <table><tr><td><div class="p"></div></td><td class="t">Supercalifragilistic expialidocious words</td></tr></table>"#;
        let fonts = FontSet::load().unwrap();
        let big = Env { text_scale_64: 128, ..Env::default() };
        let (at100, _) = premeasure_tables(src, &fonts, &big);
        assert!(at100.contains("atrium-premeasured"), "the normalizer's sheet is marked");
        let (exact, _) = premeasure_tables_at(src, &fonts, &big);
        let a = render_html(&at100, &fonts, &big);
        let b = render_html(&exact, &fonts, &big);
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        assert_eq!(crate::nsg::write(&a.scene, &fonts), crate::nsg::write(&b.scene, &fonts),
                   "the 100%-measured document at 200% lays out as one measured at 200%");
        assert_eq!(crate::nsg::write(&render_html(&at100, &fonts, &Env::default()).scene, &fonts),
                   crate::nsg::write(&render_html(&premeasure_tables(src, &fonts, &Env::default()).0, &fonts, &Env::default()).scene, &fonts),
                   "and at 100% nothing is measured again");
    }

    #[test]
    fn a_table_flex_item_is_never_narrower_than_its_min_content() {
        // ★ ar Wikipedia's infobox: `width` below the table's min-content,
        // beside a growing column. The row must give the table its
        // min-content, not its `width`, or the table covers the column.
        let src = r#"<style>body { margin-left: 0px; margin-top: 0px } .row { display: flex; width: 400px }
            .t { width: 50px; border-spacing: 0px 0px } td { padding-left: 0px; padding-right: 0px; background-color: #ff0000 }
            .c { flex-grow: 1; flex-shrink: 1; flex-basis: 0px; min-width: 0px; height: 10px; background-color: #0000ff }</style>
            <div class="row"><table class="t"><tr><td>Supercalifragilistic</td></tr></table><div class="c"></div></div>"#;
        let fonts = FontSet::load().unwrap();
        let (fixed, _) = premeasure_tables(src, &fonts, &Env::default());
        let o = render_html(&fixed, &fonts, &Env::default());
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        let (t, c) = (boxes(&o, 0xff0000ff)[0], boxes(&o, 0x0000ffff)[0]);
        assert!(t.2 > 50, "the table (its cell) is wider than its width (px): {t:?}");
        assert!(c.0 >= t.0 + t.2, "the column starts where the table ends: table {t:?} column {c:?}");
    }

    #[test]
    fn an_inline_image_with_a_percentage_limit_is_compressible_but_keeps_its_max_content() {
        // ★ CSS Sizing 3 §5.2.2: min-content 0 (El País), max-content its
        // size — also when the limit is a calc() holding a percentage
        // (Wikipedia's thumbnails). Clamped against "no width", both came
        // out 0, or both its full width.
        for limit in ["max-width: 100%", "max-width: calc((100% - 8px) + 2px)"] {
            let src = format!(r#"<style>td {{ padding-left: 0px; padding-right: 0px }} img {{ width: 400px; {limit} }}</style>
                <table><tr><td><a href="x"><img src="i.png" width="400" height="100"></a></td></tr></table>"#);
            let fonts = FontSet::load().unwrap();
            let (out, _) = premeasure_tables(&src, &fonts, &Env::default());
            let decl = out.lines().find(|l| l.contains(".tc0")).expect("declared").to_string();
            let px = |name: &str| decl.split(name).nth(1).and_then(|r| r.split("px").next()).and_then(|v| v.trim().parse::<f64>().ok()).unwrap();
            assert!(px("min-width:") < 50.0, "{limit}: compressible, min {decl}");
            assert!(px("max-width:") >= 400.0, "{limit}: but its max-content is its width: {decl}");
        }
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
                          baseline_probe: None, indent: None, iso_paths: vec![vec![]], iso_cur: 0, content_dy: 0, table_part: false, promoted: vec![], boxes: std::env::var_os("NSG_DUMP_BOXES").map(|_| vec![]), geo: None, inl_cur: 0,
                          viewport: (0, None), pos_cb: (0, 0, 0, None), subs: &Subresources::new(),
                          placing_abs: false, pending_abs: vec![], deferred_abs: vec![], cb_open: false, canvas_src: None, abs_origin: None, flex_static: None, measured: HashMap::new(), contexts: vec![], hoists: vec![], diagnostics: vec![], vector_budget: crate::path::DOCUMENT_SEGMENT_SHARE, in_scroll: false, scroll_open: None, measured_tables: HashMap::new(), scroll_table: None, stop_after: None, stop_ok: true, table_stop: None, truncated: false, stop_unsafe: false };
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
    fn grid_auto_placement_follows_the_section_8_5_passes() {
        let css = r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 300px; grid-template-columns: 100px 100px 100px; grid-auto-rows: 10px }
            .a { background-color: #ff0000 } .b { background-color: #00ff00 } .c { background-color: #0000ff }
            .d { background-color: #ffff00 }"#;
        // Step 2: items locked to a row go along it, each past the last one
        // this step put there — not all into column 1.
        let o = render(&format!(r#"{css} .g > div {{ grid-row-start: 2 }} .g > .d {{ grid-row-start: 1 }}</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="c"></div><div class="d"></div></div>"#));
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 10, 100, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 10, 100, 10)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(200, 10, 100, 10)]);
        assert_eq!(boxes(&o, 0xffff00ff), vec![(0, 0, 100, 10)]);
        // Step 1 before step 4: a LATER item with a definite cell claims it
        // before an earlier auto item is placed.
        let o = render(&format!(r#"{css} .b {{ grid-row-start: 1; grid-column-start: 1 }}</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#));
        assert_eq!(boxes(&o, 0xff0000ff), vec![(100, 0, 100, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 0, 100, 10)]);
        // Step 4, sparse: a definite column BEHIND the cursor moves to the
        // next row rather than back-filling the hole.
        let o = render(&format!(r#"{css} .a {{ grid-column-start: 3 }} .b {{ grid-column-start: 1 }}</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#));
        assert_eq!(boxes(&o, 0xff0000ff), vec![(200, 0, 100, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 10, 100, 10)]);
        // Spans: an auto item spanning two columns that does not fit in the
        // rest of the row wraps whole; the hole it leaves stays empty.
        let o = render(&format!(r#"{css} .b, .c {{ grid-column-end: span 2 }}</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="c"></div><div class="d"></div></div>"#));
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 0, 200, 10)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 10, 200, 10)]);
        assert_eq!(boxes(&o, 0xffff00ff), vec![(200, 10, 100, 10)]);
        // Column flow: items locked to a COLUMN go down it.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; grid-auto-flow: column; grid-template-rows: 10px 10px; grid-auto-columns: 50px }
            .g > div { grid-column-start: 2 } .a { background-color: #ff0000 } .b { background-color: #00ff00 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(50, 0, 50, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(50, 10, 50, 10)]);
    }

    #[test]
    fn a_bare_fr_track_keeps_its_items_minimum_and_goes_inflexible() {
        // The Guardian's "most viewed" list: `1fr 1fr` in a 241 px grid whose
        // items' min-content is 233. `1fr` is `minmax(auto, 1fr)`, so each
        // column is at least that — the grid overflows, as in Chrome — and
        // a column already wider than its share drops out of the sharing
        // (Grid §12.7.1), leaving the rest to the other one.
        let doc = |cols: &str, a: &str| format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }}
            .g {{ display: grid; width: 100px; grid-template-columns: {cols} }}
            .a {{ background-color: #ff0000; height: 10px }} .b {{ background-color: #00ff00; height: 10px }}
            .w {{ width: 80px; height: 10px }} .z {{ min-width: 0px }}</style>
            <div class="g"><div class="{a}"><div class="w"></div></div><div class="b"></div></div>"#);
        let o = render(&doc("1fr 1fr", "a"));
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 80, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(80, 0, 20, 10)]);
        let o = render(&doc("1fr 3fr", "a"));
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 80, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(80, 0, 20, 10)]);
        // Control: with `min-width: 0` the item has no minimum, so the
        // columns share the space evenly.
        let o = render(&doc("1fr 1fr", "a z"));
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 50, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(50, 0, 50, 10)]);
        // A flex sum below 1 shares only that fraction of the space.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; grid-template-columns: 0.25fr 0.25fr }
            .a { background-color: #ff0000; height: 10px } .b { background-color: #00ff00; height: 10px }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 25, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(25, 0, 25, 10)]);
    }

    #[test]
    fn a_percentage_min_width_does_not_widen_a_grid_track() {
        // The item's `min-width: 50%` is of its grid area, which is what is
        // being sized: it resolves against zero, so all four `1fr` columns
        // stay 50 px (as in Chrome) instead of the first claiming 100.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 200px; grid-template-columns: 1fr 1fr 1fr 1fr }
            .a { min-width: 50%; width: 100%; height: 10px; background-color: #ff0000 }
            .b { height: 10px } .c { min-width: calc(10px + 50%); height: 10px; background-color: #00ff00 }</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="c"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 50, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 0, 50, 10)]);
    }

    #[test]
    fn fr_tracks_in_indefinite_space_share_one_flex_fraction() {
        // Le Monde: `repeat(3, 1fr)` rows in an auto-height grid. With
        // indefinite space every flexible track takes the SAME fraction —
        // the largest its content needs per unit of flex (Grid §12.7.1) —
        // so all three rows are as tall as the tallest, as in Chrome.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 200px; grid-template-rows: repeat(3, 1fr); row-gap: 5px }
            .a { height: 10px; background-color: #ff0000 } .b { height: 30px; background-color: #00ff00 }
            .c { height: 20px; background-color: #0000ff }</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="c"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 200, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 35, 200, 30)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 70, 200, 20)]);
        // Columns of a shrink-wrapped grid, in proportion to their flex: the
        // 60 px item sets 1fr = 60, so 2fr = 120; a 0.5fr track keeps its
        // content's 40 (a factor below 1 counts as 1).
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .o { display: inline-block } .g { display: grid; grid-template-columns: 1fr 2fr 0.5fr }
            .a { width: 60px; height: 10px; background-color: #ff0000 } .b { width: 30px; height: 10px }
            .c { width: 40px; height: 10px } .i2 { background-color: #00ff00 } .i3 { background-color: #0000ff }</style>
            <div class="o"><div class="g"><div><div class="a"></div></div><div class="i2"><div class="b"></div></div><div class="i3"><div class="c"></div></div></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(60, 0, 120, 10)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(180, 0, 40, 10)]);
    }

    #[test]
    fn a_spanning_item_sets_the_flex_fraction_in_indefinite_space() {
        // Rows `1fr 3fr` in an auto-height grid, one 100px item over both:
        // the fraction that fills it is 100 / 4, so 25 and 75 (§12.7.1).
        // Shared out evenly first, it made 50 / 50, and then 50 / 150.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; grid-template-columns: 50px 50px; grid-template-rows: 1fr 3fr }
            .s { grid-row-start: 1; grid-row-end: 3; height: 100px }
            .a { background-color: #ff0000 } .b { grid-column-start: 2; grid-row-start: 2; background-color: #00ff00 }</style>
            <div class="g"><div class="s"></div><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(50, 0, 50, 25)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(50, 25, 50, 75)]);
        // Columns of a shrink-wrapped grid: a `span 2` 100px item over two
        // `1fr` columns makes each 45 (with a 10px gap), not its 10px items'.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .o { display: inline-block } .g { display: grid; grid-template-columns: 1fr 1fr; column-gap: 10px }
            .w { grid-column-start: 1; grid-column-end: 3; width: 100px; height: 10px }
            .a { width: 10px; height: 10px; background-color: #ff0000 } .b { width: 10px; height: 10px; background-color: #00ff00 }
            .x { background-color: #0000ff }</style>
            <div class="o"><div class="g"><div class="w"></div><div class="x"><div class="a"></div></div><div><div class="b"></div></div></div></div>"#);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 10, 45, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(55, 10, 10, 10)]);
    }

    #[test]
    fn a_minmax_fr_row_in_an_auto_height_grid_holds_its_content() {
        // BBC News: `grid-template-rows: min-content minmax(0, 1fr)` on a
        // grid with no height. With indefinite free space a flexible track
        // sizes to its content (Grid §12.7.1) — the 0 minimum is only its
        // floor. Taken as 0, the second row's card hung below the grid and
        // the next section painted over it.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; grid-template-rows: min-content minmax(0, 1fr) }
            .a { height: 10px; background-color: #ff0000 } .b { height: 30px; background-color: #00ff00 }
            .after { height: 5px; background-color: #0000ff }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div><div class="after"></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 10, 100, 30)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 40, 792, 5)], "the next block starts below the fr row");
        // Control: with a definite height the fr row takes the leftover.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; height: 100px; grid-template-rows: min-content minmax(0, 1fr) }
            .a { height: 10px; background-color: #ff0000 } .b { background-color: #00ff00 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 10, 100, 90)]);
    }

    #[test]
    fn an_item_spanning_auto_rows_gives_them_its_height() {
        // BBC Arabic: each first-row card has `grid-row: span 2`. Only
        // one-row items fed the row sizes, so both auto rows came out 0, the
        // cards 16 px (the gap), and the next row drew over them. A spanning
        // item's size goes to the intrinsic rows it covers (Grid §12.5).
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 200px; grid-template-columns: 100px 100px }
            .a { grid-row-start: span 2; height: 100px; background-color: #ff0000 }
            .b { height: 10px; background-color: #00ff00 }
            .after { height: 5px; background-color: #0000ff }</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="b"></div></div><div class="after"></div>"#);
        // Rows of 10 + 10 fall 80 short; the shortfall is shared, 50 + 50.
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 0, 100, 10), (100, 50, 100, 10)]);
        assert_eq!(boxes(&o, 0x0000ffff), vec![(0, 100, 792, 5)]);
        // Control: rows that already hold it are not grown.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 200px; grid-template-columns: 100px 100px }
            .a { grid-row-start: span 2; height: 20px; background-color: #ff0000 }
            .b { height: 30px; background-color: #00ff00 }</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 0, 100, 30), (100, 30, 100, 30)]);
    }

    #[test]
    fn an_inline_block_holding_a_flex_box_stays_inline() {
        // Samsung's footer: `li { display: inline-block }` around
        // `a { display: flex }`. An inline-block is an ATOMIC inline with a
        // formatting context of its own, so a block-level child does not
        // make it block-in-inline. Promoted, every link became a full-width
        // block and the row stacked one per line.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .i { display: inline-block } .f { display: flex; padding-right: 12px; background-color: #ff0000 }</style>
            <div><span class="i"><a class="f">Privacy</a></span><span class="i"><a class="f">Legal</a></span></div>"#);
        let b = boxes(&o, 0xff0000ff);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].1, b[1].1, "one line: {b:?}");
        assert!(b[0].2 < 200 && b[1].0 >= b[0].0 + b[0].2, "shrink-to-fit, side by side: {b:?}");
        // Control: a plain INLINE around a block IS block-in-inline.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .f { display: flex; background-color: #ff0000 }</style>
            <div><span><a class="f">Privacy</a></span><span><a class="f">Legal</a></span></div>"#);
        let b = boxes(&o, 0xff0000ff);
        assert_eq!((b[0].1 < b[1].1, b[0].2), (true, 792), "stacked blocks: {b:?}");
    }

    #[test]
    fn an_inline_block_in_a_zero_font_size_line_still_lays_out() {
        // Samsung's footer: `font-size: 0` on the list removes the spaces
        // between its inline-block items, and each item's link sets its own
        // size. The zero-size skip for TEXT also skipped the atomic items,
        // so none of the links was laid out at all.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            ul { font-size: 0; padding-left: 0px; margin-top: 0px; list-style-type: none }
            li { display: inline-block; background-color: #ff0000 } a { font-size: 12px }
            .f { display: flex }</style>
            <ul><li><a>Privacy</a></li> <li><a class="f">Legal</a></li></ul>"#);
        let words: Vec<&str> = o.scene.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(words, vec!["Privacy", "Legal"]);
        let b = boxes(&o, 0xff0000ff);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].1, b[1].1, "one row: {b:?}");
        assert_eq!(b[1].0, b[0].0 + b[0].2, "no gap: the space between them is 0 px: {b:?}");
        // Control: text that is itself 0 px still paints nothing.
        let o = render(r#"<style>span { font-size: 0 }</style><p>Seen <span>hidden</span></p>"#);
        assert_eq!(o.scene.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["Seen"]);
    }

    #[test]
    fn only_a_block_li_has_a_marker() {
        // `display: inline-block` (or flex, grid) on an `li` replaces
        // list-item: no bullet. Navigation rows built from inline-block
        // items grew a "•" before every link.
        let o = render(r#"<style>.i { display: inline-block } .f { display: flex }</style>
            <ul><li class="i">a</li><li class="f">b</li><li>c</li></ul>"#);
        let words: Vec<&str> = o.scene.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(words, vec!["a", "b", "\u{2022}", "c"]);
    }

    #[test]
    fn a_minmax_auto_row_in_an_auto_height_grid_grows_to_its_content() {
        // Tildes: `grid-template-rows: repeat(3, minmax(22px, auto))` on a
        // grid with no height. 22px is only the BASE; with indefinite free
        // space a track grows to its growth limit, here its content. Left at
        // the base, a title that wrapped to two lines ran under the metadata
        // row below it.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; grid-template-rows: minmax(22px, auto) minmax(22px, auto) }
            .a { height: 50px; background-color: #ff0000 } .b { height: 10px; background-color: #00ff00 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 50, 100, 10)], "row 1 holds the 50 px item");
        // Control: a row with less content keeps its 22 px floor.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; grid-template-rows: minmax(22px, auto) minmax(22px, auto) }
            .a { height: 5px; background-color: #ff0000 } .b { height: 10px; background-color: #00ff00 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 22, 100, 10)]);
    }

    #[test]
    fn a_negative_grid_line_counts_from_the_end() {
        // `1 / -1` is the full-width idiom: every column, whatever the count.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 320px; grid-template-columns: 100px 100px 100px; column-gap: 10px }
            .g > div { height: 10px }
            .a { background-color: #ff0000; grid-column-start: 1; grid-column-end: -1 }
            .b { background-color: #00ff00; grid-column-start: -2; grid-column-end: -1 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 320, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(220, 10, 100, 10)]);
    }

    #[test]
    fn a_span_on_the_start_side_spans() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 300px; grid-template-columns: 100px 100px 100px }
            .g > div { height: 10px }
            .a { background-color: #ff0000; grid-column-start: span 2; grid-column-end: auto }
            .b { background-color: #00ff00; grid-column-start: span 2; grid-column-end: 4 }</style>
            <div class="g"><div class="a"></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 0, 200, 10)]);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(100, 10, 200, 10)]);
    }

    /// ★ A flex item is laid out twice — measured, then placed — so
    /// without `measure` remembering its answers, flex items nested in flex
    /// items cost 2^depth: 22 levels took 14 s, and WPT's XHTML flexbox
    /// tests (27 levels once `<div/>` nests) never finished.
    #[test]
    fn nested_flex_items_are_not_laid_out_exponentially() {
        let n = 24;
        let src = format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }}
            .f {{ display: flex; width: 200px }} .a {{ height: 10px; flex-basis: 10px; padding-left: 1px }}
            .z {{ width: 5px; height: 5px; background-color: #ff0000 }}</style>{}<div class="z"></div>{}"#,
            r#"<div class="f"><div class="a">"#.repeat(n), "</div></div>".repeat(n));
        let t = std::time::Instant::now();
        let o = render(&src);
        let spent = t.elapsed();
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert_eq!(boxes(&o, 0xff0000ff)[0].0, n as U, "each level's padding moves it 1 px");
        assert!(spent < std::time::Duration::from_secs(2), "{n} nested flex items took {spent:?}");
    }

    /// ★ A `calc()` with no percentage in it is a DEFINITE length to
    /// measurement, like a plain one: as a child's width in a row flex
    /// container's max-content, and as the `min-width` that clamps a
    /// contribution. GitHub Docs' search box and Menu button. Chrome's
    /// numbers.
    #[test]
    fn a_percentage_free_calc_is_a_definite_length_when_measuring() {
        let o = render(r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .row { display: flex } .sp { margin-left: auto; display: flex; min-width: 0px }
            .s { width: calc(8rem * 2 + 0.125rem); height: 10px; background-color: #00ff00 }
            .b { width: 50px; height: 10px; background-color: #ff0000 }
            .m { width: max-content; min-width: calc(4rem + 2rem); height: 10px; background-color: #0000ff }</style>
            <div class="row"><div class="sp"><div class="s">x</div><div class="b"></div></div></div>
            <div class="row"><div class="sp"><div class="m"></div></div></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert_eq!(boxes(&o, 0x00ff00ff)[0].0, 492, "the calc() width is the box's, not its text's");
        assert_eq!(boxes(&o, 0xff0000ff)[0].0, 750, "so its neighbour ends at the edge");
        assert_eq!(boxes(&o, 0x0000ffff)[0].0, 800 - 96, "a calc() min-width clamps the contribution");
    }

    /// ★ A grid container's intrinsic width is its COLUMNS' — sized under
    /// the content constraint, plus the gaps — not its widest item's: a
    /// toolbar grid in a `minmax(0, max-content)` column (rustdoc's), and
    /// one in a `min-content` box and in a flex item. Headless Chrome's
    /// numbers.
    #[test]
    fn a_grid_container_is_as_wide_as_its_columns() {
        let o = render(r#"<style>body { margin-left: 0px; margin-right: 0px; margin-top: 0px }
            .o { display: grid; width: 540px; grid-template-columns: minmax(105px, 1fr) minmax(0, max-content) }
            .t { display: grid; column-gap: 10px } .x { height: 10px }
            .b { width: 80px; height: 10px; background-color: #ff0000 }
            .c1 { grid-column-start: 1 } .c2 { grid-column-start: 2 } .c3 { grid-column-start: 3 }</style>
            <div class="o"><div class="x"></div><div class="t"><div class="b c1"></div><div class="b c2"></div><div class="b c3"></div></div></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert_eq!(boxes(&o, 0xff0000ff).iter().map(|b| b.0).collect::<Vec<_>>(), vec![280, 370, 460], "a max-content column");
        for wrap in [r#"<div class="m">"#, r#"<div class="f"><div>"#] {
            let o = render(&format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }} .m {{ width: min-content }} .f {{ display: flex }}
                .t {{ display: grid; column-gap: 10px; grid-template-columns: auto auto; background-color: #0000ff; height: 10px }}
                .b {{ width: 80px; height: 10px }}</style>
                {wrap}<div class="t"><div class="b"></div><div class="b"></div></div></div></div>"#));
            assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
            assert_eq!(boxes(&o, 0x0000ffff)[0].2, 170, "{wrap}");
        }
    }

    /// ★ A grid item's MARGIN box is what goes in its area: stretch fills
    /// what the margins leave, end and centre align the margin box, and
    /// `auto` margins take the free space and stop the stretch; min- and
    /// max-width clamp the box. Areas of 200×100, margins 5/10/15/20;
    /// headless Chrome's numbers.
    #[test]
    fn a_grid_items_margins_are_inside_its_area() {
        let cases: &[(&str, &str, (U, U, U, U))] = &[
            ("", "", (20, 5, 170, 80)),
            ("justify-items: end; align-items: end", "w", (140, 55, 50, 30)),
            ("justify-items: center; align-items: center", "w", (80, 30, 50, 30)),
            ("", "w ax", (75, 5, 50, 30)),
            ("", "w al at", (140, 55, 50, 30)),
            // min/max-width clamp the stretched or specified size.
            ("justify-items: center", "x", (55, 5, 100, 80)),
        ];
        for (g, i, want) in cases {
            let src = format!(r#"<style>body {{ margin-left: 0px; margin-top: 0px }}
                .g {{ display: grid; grid-template-columns: 200px 200px; grid-template-rows: 100px; width: 400px; {g} }}
                .i {{ background-color: #ff0000; margin-top: 5px; margin-right: 10px; margin-bottom: 15px; margin-left: 20px }}
                .w {{ width: 50px; height: 30px }} .ax {{ margin-left: auto; margin-right: auto }}
                .al {{ margin-left: auto }} .at {{ margin-top: auto }} .x {{ width: 100%; max-width: calc(100% - 100px) }}</style>
                <div class="g"><div class="i {i}"></div></div>"#);
            let o = render(&src);
            assert!(o.diagnostics.is_empty(), "{g} {i}: {:?}", o.diagnostics);
            assert_eq!(boxes(&o, 0xff0000ff)[0], *want, "{g} / {i}");
        }
        // The ROW is measured at the width the item gets: four 40 px boxes
        // fit one line of 200 but wrap in the 150 a 50 px margin leaves,
        // so the next row starts at 20.
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; grid-template-columns: 200px; width: 200px }
            .i { margin-left: 50px } .s { display: inline-block; width: 40px; height: 10px; background-color: #0000ff }
            .n { height: 10px; background-color: #ff0000 }</style>
            <div class="g"><div class="i"><span class="s"></span><span class="s"></span><span class="s"></span><span class="s"></span></div><div class="n"></div></div>"#);
        assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
        assert_eq!(boxes(&o, 0xff0000ff)[0].1, 20, "the next row");
    }

    /// ★ An `auto` minimum takes the item's MINIMUM contribution (Grid
    /// §12.5, §6.6): `min-width: 0` or a scroll container contributes
    /// nothing, a length contributes itself, and only an `auto` min-width
    /// is the content's min-content size. A `min-content` minimum still
    /// takes the min-content contribution. Widths are headless Chrome's.
    #[test]
    fn an_auto_grid_track_takes_the_items_minimum_contribution() {
        let cases: &[(&str, &str, &str, &[(U, U)])] = &[
            ("auto", "min-width: 0px", "", &[(0, 400)]),
            ("auto", "", "", &[(0, 600)]),
            ("auto", "overflow-x: hidden; overflow-y: hidden", "", &[(0, 400)]),
            ("auto", "min-width: 100px", "", &[(0, 400)]),
            ("auto auto", "min-width: 0px", r#"<div class="i"><div class="n"></div></div>"#, &[(0, 300), (300, 100)]),
            ("min-content auto", "min-width: 0px", r#"<div class="i"></div>"#, &[(0, 600), (600, 0)]),
            ("auto", "min-width: 0px; padding-left: 10px", "", &[(0, 400)]),
            ("auto", "width: 500px", "", &[(0, 500)]),
        ];
        for (cols, a, b, want) in cases {
            let src = format!(r#"<style>body {{ margin-left: 0px; margin-right: 0px; margin-top: 0px }}
                .g {{ display: grid; width: 400px; grid-template-columns: {cols} }}
                .i {{ height: 5px; background-color: #ff0000 }} .f {{ {a} }}
                .w {{ width: 600px; height: 5px }} .n {{ width: 100px; height: 5px }}</style>
                <div class="g"><div class="i f"><div class="w"></div></div>{b}</div>"#);
            let o = render(&src);
            assert!(o.diagnostics.is_empty(), "{cols} {a}: {:?}", o.diagnostics);
            let got: Vec<(U, U)> = boxes(&o, 0xff0000ff).iter().map(|b| (b.0, b.2)).collect();
            assert_eq!(got, want.to_vec(), "{cols} / {a}");
        }
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

    /// Auto margins absorb only POSITIVE free space (Grid §11.2): an item
    /// larger than its area starts at the area's start (WPT
    /// grid-item-auto-margins-001: a 100px item in a 0px track).
    #[test]
    fn grid_auto_margins_with_negative_free_space_are_zero() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; height: 100px; grid-template-columns: 0px; grid-template-rows: 0px }
            .g > div { width: 100px; height: 100px; margin-left: auto; margin-right: auto; margin-top: auto; margin-bottom: auto; background-color: #00ff00 }
            .h { display: grid; width: 100px; grid-template-columns: 60px }
            .h > div { width: 20px; height: 10px; margin-left: auto; margin-right: auto; background-color: #0000ff }</style>
            <div class="g"><div></div></div><div class="h"><div></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 0, 100, 100)], "no room: the margins are 0");
        assert_eq!(boxes(&o, 0x0000ffff), vec![(20, 100, 20, 10)], "room: the margins centre it");
    }

    /// A non-stretched auto-sized grid item is FIT-CONTENT in its area
    /// (Grid §11.1): never narrower than its min-content, never shorter
    /// than its content (WPT grid-minimum-size-grid-items-014).
    #[test]
    fn a_start_aligned_grid_item_overflows_a_small_area() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; grid-template-columns: 10px; grid-template-rows: 10px }
            .i { justify-self: start; align-self: start; background-color: #00ff00 }
            .c { width: 100px; height: 100px }</style>
            <div class="g"><div class="i"><div class="c"></div></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 0, 100, 100)]);
    }

    /// An abspos child of a grid that is its containing block takes its
    /// GRID AREA as that block (Grid §9.1); `auto` lines are the padding
    /// edges, and an auto-height container's are clamped by max-height.
    #[test]
    fn an_abspos_grid_child_is_contained_by_its_grid_area() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; position: relative; padding-left: 10px; padding-top: 10px; padding-right: 10px; padding-bottom: 10px;
                 grid-template-columns: 100px 50px; grid-template-rows: 40px 60px; column-gap: 5px; max-height: 90px }
            .g > div { position: absolute; width: 100%; height: 100% }
            .a { grid-column-start: 2; grid-column-end: 3; grid-row-start: 2; grid-row-end: 3; background-color: #00ff00 }
            .b { grid-column-start: 1; grid-column-end: 2; background-color: #0000ff }
            .c { grid-column-end: -1; grid-row-start: 1; grid-row-end: 2; background-color: #ff0000 }</style>
            <div class="g"><div class="a"></div><div class="b"></div><div class="c"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(115, 50, 50, 60)], "column 2, row 2");
        // Rows auto: the padding box, top to bottom — 90px tall by max-height.
        assert_eq!(boxes(&o, 0x0000ffff), vec![(10, 0, 100, 90)], "column 1, all rows");
        // Start auto = the left padding edge; end = the last line.
        assert_eq!(boxes(&o, 0xff0000ff), vec![(0, 10, 165, 40)], "padding edge to line -1");
    }

    /// An abspos child's STATIC position in a grid: its own self-alignment
    /// in the grid's content box (CSS Position 3 §4.2) — and positioned
    /// boxes paint in tree order among the grid's items.
    #[test]
    fn an_abspos_grid_child_aligns_at_its_static_position_in_tree_order() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; height: 100px; grid-template-columns: 100px; grid-template-rows: 100px }
            .a { position: absolute; width: 50px; height: 20px; align-self: end; justify-self: center; background-color: #00ff00 }
            .p { position: relative; grid-column-start: 1; grid-row-start: 1 }
            .r { position: absolute; width: 10px; height: 10px; background-color: #ff0000 }
            .b { position: absolute; width: 10px; height: 10px; background-color: #0000ff }</style>
            <div class="g"><div class="a"></div><div class="p"><div class="r"></div></div><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(25, 80, 50, 20)], "end / centre in the content box");
        // The red box is inside the item before the blue one: blue on top.
        let order: Vec<u32> = o.scene.order.iter().filter(|(k, _)| *k == 0).map(|(_, i)| o.scene.rects[*i].rgba).collect();
        assert_eq!(order, vec![0x00ff00ff, 0xff0000ff, 0x0000ffff]);
    }

    /// Outside flex, `flex-end` is `end` (Box Alignment §6.1).
    #[test]
    fn grid_items_read_flex_end_as_end() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; height: 100px; grid-template-columns: 100px; grid-template-rows: 100px; align-items: flex-end }
            .g > div { width: 50px; height: 50px; background-color: #00ff00 }</style>
            <div class="g"><div></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 50, 50, 50)]);
    }

    /// With an auto height but a definite min-height, rows stretch into
    /// the min-height (Grid §12.8): Asahi's `auto 176px` tab box.
    #[test]
    fn grid_rows_stretch_into_a_min_height() {
        let o = render(r#"<style>body { margin-left: 0px; margin-top: 0px }
            .g { display: grid; width: 100px; min-height: 216px; grid-template-rows: auto 176px }
            .b { grid-row-start: 2; height: 176px; background-color: #00ff00 }</style>
            <div class="g"><div class="b"></div></div>"#);
        assert_eq!(boxes(&o, 0x00ff00ff), vec![(0, 40, 100, 176)]);
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

    /// CSS 2.1 §17.5.2.2: an AUTO-width table is as wide as its columns
    /// want (never the whole line), and a SPECIFIED width is shared out even
    /// when every column is empty — equally, since none has content to weigh.
    #[test]
    fn an_auto_table_shrink_wraps_and_empty_columns_share_a_specified_width() {
        let css = r#"<style>body { margin-left: 0px; margin-top: 0px } table { border-spacing: 0px 0px; background-color: #ff0000 }
            td { padding-top: 0px; padding-right: 0px; padding-bottom: 0px; padding-left: 0px; height: 20px }
            .d { background-color: #0000ff; height: 10px }
            .c { min-width: 50px; max-width: 50px } .w { width: 300px } .e { min-width: 0px; max-width: 0px }</style>"#;
        let o = render(&format!(r#"{css}<table><colgroup><col class="c"><col class="c"></colgroup><tr><td></td><td></td></tr></table>"#));
        assert_eq!(boxes(&o, 0xff0000ff)[0].2, 100, "two 50 px columns: a 100 px table, not 800");
        let o = render(&format!(r#"{css}<table class="w"><colgroup><col class="e"><col class="e"></colgroup><tr><td><div class="d"></div></td><td></td></tr></table>"#));
        assert_eq!(boxes(&o, 0x0000ffff)[0].2, 150, "300 px shared equally by two empty columns");
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
/// One bottom-up pass: an element is promoted when its own `display` is
/// `inline` but a child is block-level (or was itself promoted). Doing this
/// once is what keeps it O(n) — asking the question per element while
/// laying out would walk the same subtrees again and again.
fn promote_block_in_inline(dom: &Dom, styles: &[Option<std::rc::Rc<Style>>]) -> Vec<bool> {
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
        // ★ Only a plain INLINE is split. An inline-block is an atomic
        // inline with its own formatting context: its block children stay
        // inside it. Promoting it made Samsung's footer links — inline-block
        // items around `display: flex` anchors — one full-width block each.
        let display = styles.get(h as usize).and_then(|s| s.as_ref()).map(|s| kw(s, "display"));
        if matches!(display, Some("none" | "inline-block")) || blockish(h) { continue }
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
/// A space-free run split at its UAX #14 break opportunities: between CJK
/// ideographs, kana and Hangul; between the words of the Southeast Asian
/// scripts (from ICU4X's dictionaries, integer data, so a render stays
/// byte-identical across ISAs); and in spaced scripts after a hyphen, a
/// slash or a dash before a letter — a long URL or `well-known/path` wraps
/// there instead of running off the line.
fn uax14_segments(word: &str) -> Vec<&str> {
    let unspaced = |c: char| matches!(c as u32,
        0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1100..=0x11FF | 0x1780..=0x17FF
        | 0x2E80..=0x2FFF | 0x3000..=0x303F | 0x3040..=0x30FF | 0x3130..=0x318F | 0x31F0..=0x31FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF
        | 0x20000..=0x3FFFF);
    // Letters and digits alone never break inside a word (LB28-LB30): skip
    // the segmenter for the common case.
    if !word.chars().any(unspaced) && word.chars().all(|c| c.is_alphanumeric() || c == '\'' || c == '\u{2019}') { return vec![word] }
    let seg = line_segmenter();
    let mut out = vec![];
    let mut last = 0;
    for b in seg.segment_str(word) {
        // ★ Not after a SOFT HYPHEN: a break there must paint a hyphen,
        // which this layout does not do — broken silently, MDN's
        // "Com\u{ad}puted value" read "Com" / "puted value".
        if b > last && !word[..b].ends_with('\u{ad}') { out.push(&word[last..b]); last = b }
    }
    if last < word.len() { out.push(&word[last..]) }
    out
}

/// ★ No soft wrap before an item under `white-space: pre` or `nowrap`:
/// the space before it (if any) and the item itself must both allow one.
/// `pre` text arrives as one word per element, and wrapping between them
/// split a code line in two — `let s =` / `"hello";` — inside its block.
fn no_soft_wrap(l: &Line, pending: &Option<(FontStyle, Line)>) -> bool {
    l.nowrap && pending.as_ref().is_none_or(|(_, pl)| pl.nowrap)
}

fn line_segmenter() -> icu_segmenter::LineSegmenterBorrowed<'static> {
    let mut opts = icu_segmenter::options::LineBreakOptions::default();
    // CSS `line-break: auto` is the normal rule set, not ICU's strict default.
    opts.strictness = Some(icu_segmenter::options::LineBreakStrictness::Normal);
    icu_segmenter::LineSegmenter::new_dictionary(opts)
}

/// Whether a line may break between two words that touch with no space —
/// the end of one element's text and the start of the next, or two
/// segments of one run. UAX #14 decides on the joined text: `<span>399</span>
/// <span>.</span>00` is ONE word to a reader, and breaking it put ".00" on
/// a line of its own.
fn uax14_break_between(a: &str, b: &str) -> bool {
    let (Some(x), Some(y)) = (a.chars().last(), b.chars().next()) else { return true };
    let letter = |c: char| c.is_alphanumeric() && c < '\u{0e00}';
    if letter(x) && letter(y) { return false }
    if x == '\u{ad}' { return false }
    let joined = format!("{a}{b}");
    line_segmenter().segment_str(&joined).any(|i| i == a.len())
}

fn is_rtl(c: char) -> bool {
    matches!(unicode_bidi::bidi_class(c), unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL)
}

struct GItem { h: Handle, col: (usize, usize), row: (usize, usize), justify: String, align: String }

/// What `grid_plan` works out before any track is sized.
struct GridPlan { items: Vec<GItem>, abs: Vec<Handle>, coltracks: Vec<navigator_style::values::Track>,
                  rowtracks: Vec<navigator_style::values::Track>, cmin: Vec<U>, cmax: Vec<U>, cauto: Vec<U>, cgap: U,
                  /// Items spanning several columns, at least one flexible:
                  /// (first, count, max-content contribution).
                  cspans: Vec<(usize, usize, U)> }

/// An absolutely positioned grid child, ready to lay out: the handle, the
/// containing block it takes (its grid area) if not the inherited one, its
/// static-position rectangle and alignment, and the static x / right / y.
type AbsJob = (Handle, Option<(U, U, U, Option<U>)>, FlexStatic, U, U, U);

/// A track's flex factor, if its maximum is `fr` (a flexible track, §7.2.4).
fn flex_of(t: &navigator_style::values::Track) -> Option<f64> {
    use navigator_style::values::Track;
    match t {
        Track::Fr(f) => Some(*f),
        Track::MinMax(_, b) => match **b { Track::Fr(f) => Some(f), _ => None },
        _ => None,
    }
}

/// `mins`/`maxs` are the tracks' min- and max-content contributions, `autos`
/// the minimum contributions an `auto` minimum takes (§12.5).
///
/// `spans` are the items crossing a flexible track and more than one track:
/// (first track, count, contribution). They count only with INDEFINITE
/// space, where they help find the flex fraction (§12.7.1).
fn size_tracks(tracks: &[navigator_style::values::Track], avail: Option<U>, gap: U, mins: &[U], maxs: &[U], autos: &[U],
               spans: &[(usize, usize, U)]) -> Vec<U> {
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
            // `auto` as a MINIMUM is the items' minimum contribution.
            Track::Auto => get(autos, i),
            Track::MaxContent => get(maxs, i),
            // With indefinite free space a flexible track sizes to its
            // content (§12.7.1). With definite space it takes its share
            // below — but a bare `1fr` is `minmax(auto, 1fr)` (§7.2.4), so
            // its base is still the items' minimum contribution. Starting
            // it at zero split the Guardian's `1fr 1fr` list in half,
            // 120 px a column, where Chrome keeps each at its content's
            // min-content (233) and lets the grid overflow.
            Track::Fr(_) => if avail.is_none() { get(maxs, i) } else { get(autos, i) },
            Track::MinMax(a, b) => {
                let floor = match &**a {
                    Track::Len(l) => u(l.v),
                    Track::Pct(p) => pct(*p),
                    Track::MinContent => get(mins, i),
                    Track::Auto => get(autos, i),
                    Track::MaxContent => get(maxs, i),
                    _ => 0,
                };
                // ★ The same holds for `minmax(x, 1fr)`: the minimum is only
                // a floor. Taking it alone sized BBC News's
                // `minmax(0, 1fr)` row at 0 in an auto-height grid, and the
                // card in it painted over the next section.
                if matches!(**b, Track::Fr(_)) && avail.is_none() { floor.max(get(maxs, i)) } else { floor }
            }
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
    let mut sizes: Vec<U> = tracks.iter().enumerate().map(|(i, t)| base(t, i)).collect();
    let limits: Vec<U> = tracks.iter().enumerate().map(|(i, t)| limit(t, i).max(sizes[i])).collect();
    // ★ With INDEFINITE free space (an auto-height grid's rows) every track
    // with a finite growth limit grows to it (§12.5: the free space is
    // infinite). Returning the bases left Tildes's `minmax(22px, auto)` rows
    // at 22 px, and a title wrapped to two lines ran under the row below.
    let Some(a) = avail else {
        for (i, t) in tracks.iter().enumerate() { if flex_of(t).is_none() { sizes[i] = limits[i] } }
        // ★ §12.7.1 with indefinite space: ONE flex fraction for every
        // flexible track — the largest any track's content needs, per unit
        // of flex (a factor below 1 counts as 1) — so `repeat(3, 1fr)` rows
        // in an auto-height grid are all as tall as the tallest. Sized one
        // by one, Le Monde's "most read" rows came out 109 / 130 / 129
        // where Chrome has three of 130.
        let mut frac = tracks.iter().enumerate()
            .filter_map(|(i, t)| flex_of(t).map(|f| get(maxs, i) as f64 / f.max(1.0)))
            .fold(0.0, f64::max);
        // ★ An item crossing flexible tracks needs an fr large enough to
        // fill its contribution over all the tracks it crosses (§12.7.1,
        // "find the size of an fr"): what the inflexible ones and the gaps
        // leave, over the flex factors (a sum below 1 counts as 1). A
        // flexible track whose base already exceeds its share is treated as
        // inflexible and the share found again. Ignored, a `span 2` item
        // over two `1fr` columns of a shrink-wrapped grid overflowed them.
        for &(first, n, contrib) in spans {
            let crossed: Vec<usize> = (first..(first + n).min(tracks.len())).collect();
            let mut flex: Vec<usize> = crossed.iter().copied().filter(|&i| flex_of(&tracks[i]).is_some()).collect();
            loop {
                let fixed: U = crossed.iter().filter(|i| !flex.contains(i)).map(|&i| sizes[i]).sum();
                let left = (contrib - fixed - gap * (crossed.len() as U - 1)).max(0) as f64;
                let sum: f64 = flex.iter().map(|&i| flex_of(&tracks[i]).unwrap_or(0.0)).sum();
                let hyp = left / sum.max(1.0);
                let before = flex.len();
                flex.retain(|&i| sizes[i] as f64 <= hyp * flex_of(&tracks[i]).unwrap_or(0.0));
                if flex.len() == before || flex.is_empty() { if !flex.is_empty() { frac = frac.max(hyp) } break }
            }
        }
        for (i, t) in tracks.iter().enumerate() {
            if let Some(f) = flex_of(t) { sizes[i] = sizes[i].max((frac * f) as U) }
        }
        return sizes;
    };
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

    // §12.7 expand flexible tracks: what is left goes to the `fr` tracks, in
    // proportion. ★ §12.7.1 "find the size of an fr": a track whose base is
    // already MORE than its share is treated as inflexible — it keeps its
    // base, and the space it took is no longer shared by the others — and
    // the share is found again; a flex sum below 1 counts as 1.
    if tracks.iter().any(|t| flex_of(t).is_some()) {
        let mut flex: Vec<usize> = (0..tracks.len()).filter(|i| flex_of(&tracks[*i]).is_some()).collect();
        let hyp = loop {
            let fixed: U = (0..tracks.len()).filter(|i| !flex.contains(i)).map(|i| sizes[i]).sum();
            let leftover = (a - fixed - gaps).max(0) as f64 / PX as f64;
            let sum: f64 = flex.iter().map(|&i| flex_of(&tracks[i]).unwrap_or(0.0)).sum::<f64>().max(1.0);
            let hyp = leftover / sum;
            let n = flex.len();
            flex.retain(|&i| u(hyp * flex_of(&tracks[i]).unwrap_or(0.0)) >= sizes[i]);
            if flex.len() == n { break hyp }
        };
        for &i in &flex { sizes[i] = sizes[i].max(u(hyp * flex_of(&tracks[i]).unwrap_or(0.0))) }
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


/// "inline svg kept as a bitmap: <why>", as a `'static` counter key: the
/// reasons are a small fixed set (navigator-svg's refusals and the vector
/// route's), each interned once.
fn kept_as_bitmap(why: &'static str) -> &'static str {
    thread_local! { static KEYS: std::cell::RefCell<std::collections::HashMap<&'static str, &'static str>> = Default::default(); }
    KEYS.with(|k| *k.borrow_mut().entry(why).or_insert_with(|| Box::leak(format!("inline svg kept as a bitmap: {why}").into_boxed_str())))
}
