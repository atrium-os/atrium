//! ★ INLINE `<svg>`, MADE STANDALONE (threat model §2.4, "inline SVG").
//!
//! An inline SVG is an icon whose paint comes from the PAGE: `fill:
//! currentColor`, a class the page's CSS colours, a `color` inherited from a
//! button around it. Drawn on its own it would be black. So this resolves,
//! for each OUTER `<svg>` of a document, the SVG paint properties the page's
//! cascade gives each of its elements, writes them onto the elements as
//! presentation attributes (CSS beats a presentation attribute, so the value
//! the page meant wins), replaces `currentColor` with the element's resolved
//! `color`, copies in any symbol a `<use href="#id">` names from elsewhere
//! in the document, and serializes the result as one standalone SVG.
//!
//! ONE function, called in two places that must agree byte for byte:
//! - the converter, which stores each result as a content-addressed blob and
//!   names it as an image of the recording;
//! - the normalizer, which, after resolving the `<svg>` box's own CSS like
//!   any element's (so `svg { width: 1em }` still matches), turns the box
//!   into a replaced element painting that blob.
//!
//! Both call it on the same recording (its document and its stylesheets),
//! so the addresses agree. Drawing is the worker's SVG path and its strict
//! profile; an icon the profile refuses (an external sprite file, text, a
//! filter) keeps its box and draws nothing, and is counted.
//!
//! What is resolved, deliberately narrow: rules with no `@media` condition
//! and no dynamic state, and the `style=` attribute, at the normalizer's
//! own priority order; custom properties through the same cascade the
//! normalizer uses.

use crate::{css, layer_rank, resolve_customs, sel, substitute, Inputs, Priority, Report};
use navigator_dom::{Dom, Handle, Kind};
use navigator_style::token::tokenize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// SVG properties the page's CSS may set, and that are written out.
pub const PAINT: &[&str] = &["fill", "fill-opacity", "fill-rule", "stroke", "stroke-width", "stroke-opacity",
    "stroke-linecap", "stroke-linejoin", "stroke-miterlimit", "stroke-dasharray", "stroke-dashoffset", "opacity",
    "display", "visibility", "color", "stop-color", "stop-opacity", "clip-rule", "paint-order"];
/// Of those, the ones that inherit into the SVG from the HTML around it.
const INHERITED: &[&str] = &["fill", "fill-opacity", "fill-rule", "stroke", "stroke-width", "stroke-opacity",
    "stroke-linecap", "stroke-linejoin", "stroke-miterlimit", "stroke-dasharray", "stroke-dashoffset",
    "visibility", "color", "clip-rule", "paint-order"];
/// `<use>` chains copied in from elsewhere in the document, at most.
const MAX_COPIED: usize = 256;

/// The outer `<svg>` elements of a document, in document order: those not
/// inside other foreign content.
pub fn outer_svgs(dom: &Dom) -> Vec<Handle> {
    dom.by_tag_anywhere("svg").into_iter()
        .filter(|&h| !dom.get(h).and_then(|n| n.parent).is_some_and(|p| dom.is_foreign(p)))
        .collect()
}

/// One standalone SVG per outer `<svg>` of `html`, in document order.
pub fn standalone(html: &str, inputs: &Inputs) -> Vec<String> {
    let dom = navigator_dom::parse(html);
    let outers = outer_svgs(&dom);
    if outers.is_empty() { return vec![] }
    standalone_in(&dom, &outers, inputs)
}

pub(crate) fn standalone_in(dom: &Dom, outers: &[Handle], inputs: &Inputs) -> Vec<String> {
    // The page's CSS, collected exactly as `normalize` collects it.
    let mut report = Report::default();
    let mut sheet = css::Sheet::default();
    let mut order = 0usize;
    let mut css_left = inputs.budget.css_bytes;
    for h in dom.by_tag_anywhere("style") {
        let text = crate::expand_imports(&dom.text_content(h), "", inputs, 0, &mut report);
        if text.len() > css_left { continue }
        css_left -= text.len();
        css::parse(&text, &mut order, &mut sheet);
    }
    for h in dom.by_tag_anywhere("link") {
        let is_sheet = dom.attr(h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet")));
        if !is_sheet { continue }
        let href = dom.attr(h, "href").unwrap_or("").to_string();
        let Some(text) = inputs.stylesheets.get(&href) else { continue };
        // A sheet applied under a media condition is not resolved here.
        if dom.attr(h, "media").or(inputs.stylesheet_media.get(&href).map(String::as_str))
            .is_some_and(|m| !m.trim().is_empty() && !m.trim().eq_ignore_ascii_case("all") && !m.trim().eq_ignore_ascii_case("screen")) { continue }
        let text = crate::expand_imports(text, &href, inputs, 0, &mut report);
        if text.len() > css_left { continue }
        css_left -= text.len();
        css::parse(&text, &mut order, &mut sheet);
    }
    let els: Vec<Handle> = (0..dom.nodes.len() as Handle).filter(|h| matches!(dom.get(*h).map(|n| &n.kind), Some(Kind::Element(_)))).collect();
    let m = sel::Matcher::new(dom).with_budget(inputs.budget.match_steps);
    let index = sel::RuleIndex::new(dom, &els);
    // Who is asked about: every element of every outer svg, and the svg's
    // ancestors (for what inherits in from the HTML around it).
    let mut targets: HashSet<Handle> = HashSet::new();
    for &o in outers {
        let mut stack = vec![o];
        while let Some(h) = stack.pop() {
            if dom.tag(h).is_some() { targets.insert(h); }
            stack.extend(dom.children_of(h));
        }
        let mut p = dom.get(o).and_then(|n| n.parent);
        while let Some(a) = p { if dom.tag(a).is_some() { targets.insert(a); } p = dom.get(a).and_then(|n| n.parent) }
    }
    let customs = resolve_customs(&sheet, &index, &m, dom);
    let empty = std::rc::Rc::<BTreeMap<String, std::rc::Rc<Vec<navigator_style::token::Token>>>>::default();
    // (element, property) -> (priority, value)
    let mut won: HashMap<(Handle, &'static str), (Priority, String)> = HashMap::new();
    for rule in &sheet.rules {
        if rule.media.is_some() { continue }
        if !rule.decls.iter().any(|d| PAINT.contains(&d.name.as_str())) { continue }
        let Ok(list) = sel::parse_list(&rule.selector) else { continue };
        for c in &list {
            if sel::has_pseudo_element(c) || !sel::subject_states(c).is_empty() { continue }
            let spec = sel::specificity(c);
            for &h in index.candidates(c) {
                if !targets.contains(&h) || !m.matches(h, c) { continue }
                let scope = customs.get(&(h, String::new())).unwrap_or(&empty);
                for d in &rule.decls {
                    let Some(&p) = PAINT.iter().find(|p| **p == d.name) else { continue };
                    let pri: Priority = (d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order);
                    let v = css::write_tokens(&substitute(&d.value, scope, 0)).trim().to_string();
                    match won.get(&(h, p)) { Some((old, _)) if *old > pri => {} _ => { won.insert((h, p), (pri, v)); } }
                }
            }
        }
    }
    // `style=` beats every rule but an important one.
    for &h in &targets {
        let Some(style) = dom.attr(h, "style") else { continue };
        let scope = customs.get(&(h, String::new())).unwrap_or(&empty);
        for d in css::declarations(&tokenize(style)) {
            let Some(&p) = PAINT.iter().find(|p| **p == d.name) else { continue };
            let pri: Priority = (d.important, usize::MAX, (u32::MAX, 0, 0), usize::MAX);
            let v = css::write_tokens(&substitute(&d.value, scope, 0)).trim().to_string();
            match won.get(&(h, p)) { Some((old, _)) if old.0 && !d.important => {} _ => { won.insert((h, p), (pri, v)); } }
        }
    }
    let own = |h: Handle, p: &str| -> Option<String> {
        let p = PAINT.iter().find(|x| **x == p)?;
        won.get(&(h, *p)).map(|(_, v)| v.clone()).filter(|v| !v.eq_ignore_ascii_case("inherit") && !v.is_empty())
            .or_else(|| (dom.is_foreign(h)).then(|| dom.attr(h, p).map(str::to_string)).flatten())
    };
    outers.iter().map(|&o| serialize(dom, o, &own)).collect()
}

fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

fn serialize(dom: &Dom, outer: Handle, own: &dyn Fn(Handle, &str) -> Option<String>) -> String {
    // What inherits in from the HTML around the svg.
    let mut inherited: BTreeMap<&str, String> = BTreeMap::new();
    let mut p = dom.get(outer).and_then(|n| n.parent);
    while let Some(a) = p {
        for prop in INHERITED { if !inherited.contains_key(prop) { if let Some(v) = own(a, prop) { inherited.insert(prop, v); } } }
        p = dom.get(a).and_then(|n| n.parent);
    }
    let root_color = own(outer, "color").or_else(|| inherited.get("color").cloned()).unwrap_or_else(|| "#000000".into());
    // Symbols named by `<use href="#id">` and not inside this svg: copied in.
    let mut ids_inside = HashSet::new();
    let mut stack = vec![outer];
    while let Some(h) = stack.pop() {
        if let Some(id) = dom.attr(h, "id") { ids_inside.insert(id.to_string()); }
        stack.extend(dom.children_of(h));
    }
    let mut copies: Vec<Handle> = vec![];
    let mut want: Vec<Handle> = vec![outer];
    while let Some(h) = want.pop() {
        let mut st = vec![h];
        while let Some(x) = st.pop() {
            if let Some(r) = dom.attr(x, "href").or_else(|| dom.attr(x, "xlink:href")).and_then(|v| v.strip_prefix('#')) {
                if !ids_inside.contains(r) && copies.len() < MAX_COPIED {
                    if let Some(t) = dom.by_id(r) { ids_inside.insert(r.to_string()); copies.push(t); want.push(t); }
                }
            }
            st.extend(dom.children_of(x));
        }
    }
    let mut out = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"");
    write_attrs(dom, outer, own, &root_color, Some(&inherited), &mut out);
    out.push('>');
    if !copies.is_empty() {
        out.push_str("<defs>");
        for &c in &copies { node(dom, c, own, &root_color, &mut out) }
        out.push_str("</defs>");
    }
    for c in dom.children_of(outer) { node(dom, c, own, &root_color, &mut out) }
    out.push_str("</svg>");
    out
}

/// One element's attributes: its own, less `class`, `style` and the paint
/// the cascade resolved, then the resolved paint (and, on the root, what
/// inherits in), with `currentColor` replaced by its `color`.
fn write_attrs(dom: &Dom, h: Handle, own: &dyn Fn(Handle, &str) -> Option<String>, color: &str,
               inherited: Option<&BTreeMap<&str, String>>, out: &mut String) {
    // `currentColor`, any case, replaced; the rest of the value untouched
    // (a `url(#Grad1)` keeps its case).
    let cur = |v: &str| -> String {
        let (low, mut out, mut i) = (v.to_ascii_lowercase(), String::new(), 0usize);
        while let Some(j) = low[i..].find("currentcolor") {
            out.push_str(&v[i..i + j]);
            out.push_str(color);
            i += j + "currentcolor".len();
        }
        out.push_str(&v[i..]);
        out
    };
    let n = dom.get(h).expect("node");
    let mut written: HashSet<&str> = HashSet::new();
    let mut paint: Vec<(&str, String)> = vec![];
    for p in PAINT {
        if *p == "color" { continue }
        let v = own(h, p).or_else(|| inherited.and_then(|i| i.get(p).cloned()));
        if let Some(v) = v { paint.push((p, cur(&v))); written.insert(p); }
    }
    for (k, v) in &n.attrs {
        if k == "class" || k == "style" || k == "xmlns" || k.starts_with("xmlns:") || written.contains(k.as_str()) || k == "color" { continue }
        out.push_str(&format!(" {}=\"{}\"", k, esc(&cur(v))));
    }
    for (k, v) in paint { out.push_str(&format!(" {k}=\"{}\"", esc(&v))) }
}

fn node(dom: &Dom, h: Handle, own: &dyn Fn(Handle, &str) -> Option<String>, parent_color: &str, out: &mut String) {
    let Some(n) = dom.get(h) else { return };
    match &n.kind {
        Kind::Text(t) => out.push_str(&esc(t)),
        Kind::Element(_) => {
            let tag = dom.tag(h).unwrap_or("g");
            let color = own(h, "color").unwrap_or_else(|| parent_color.to_string());
            out.push('<');
            out.push_str(tag);
            write_attrs(dom, h, own, &color, None, out);
            out.push('>');
            for c in dom.children_of(h) { node(dom, c, own, &color, out) }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        _ => {}
    }
}
