//! SVG as an image: a strict profile, then a rasteriser, in the worker
//! (threat model §2.4, the SVG rows).
//!
//! ★★ SVG IS A DOCUMENT FORMAT, NOT A PICTURE FORMAT. A browser's SVG can
//! run script, load other files, embed HTML (`foreignObject`), animate, and
//! expand a few hundred bytes into billions of nodes through `<use>`. None of
//! that is allowed here. The bytes are parsed once, by roxmltree, and the
//! tree is checked against the profile below; only a tree that passes is
//! handed (the same tree, not the bytes again) to usvg, and rendered by
//! resvg/tiny-skia into exactly the painted size.
//!
//! The profile, each limit set from the corpora's 279 distinct SVGs
//! (2026-11-06: bytes p50 2.4 kB, max 438 kB; elements p50 7, max 288; depth
//! max 9; path segments max 15,000; no script, foreignObject, filter or
//! external reference in any of them):
//! - at most `MAX_SVG_BYTES`, UTF-8, the root element `svg`;
//! - a bare DOCTYPE is dropped; an internal DTD subset or an entity is
//!   refused (roxmltree is run with DTDs off, so entity expansion cannot
//!   happen at all);
//! - SVG-namespace elements only from `ALLOWED`; `script`, `foreignObject`,
//!   `image`, text, filters, animation, `marker` and anything unlisted are
//!   refused BY NAME (a document drawn with a part silently missing is worse
//!   than one not drawn: v1 has no text, so an SVG with text is refused, not
//!   drawn without its words). Elements in other namespaces (editor
//!   metadata) are ignored by usvg and only counted;
//! - every reference is local: an `href` must be `#id`, every `url(…)` must
//!   be `url(#id)`, a `<style>` may not `@import` or `@font-face`, and no
//!   attribute may be an event handler;
//! - bounded: `MAX_ELEMENTS`, `MAX_DEPTH`, `MAX_PATH_SEGMENTS`,
//!   `MAX_STOPS`, the node count after `<use>` expansion
//!   (`MAX_EXPANDED_NODES`, computed with memoisation, a cycle refused), the
//!   nesting of layers that allocate (`MAX_LAYER_DEPTH`), and the work
//!   (expanded nodes × output pixels, `MAX_WORK`);
//! - pixels: the output is the painted size (vectors are drawn sharp at any
//!   size, so not reduced from a natural frame), checked against the axis
//!   cap and the document's decode budget BEFORE the pixmap is allocated.
//!   resvg's group layers allocate a pixmap each, so the transient frame is
//!   charged as output × (1 + layer depth).

use crate::image::{Ceilings, DecodeError, Format, Pixels, BYTES_PER_PIXEL};

pub const MAX_SVG_BYTES: usize = 1 << 20;
pub const MAX_ELEMENTS: usize = 10_000;
pub const MAX_DEPTH: usize = 64;
pub const MAX_PATH_SEGMENTS: usize = 200_000;
pub const MAX_STOPS: usize = 1_024;
pub const MAX_EXPANDED_NODES: u64 = 20_000;
pub const MAX_LAYER_DEPTH: usize = 8;
/// Expanded nodes × output pixels: a cap on the rasteriser's work, so a
/// thousand full-canvas shapes at 16k² is refused before it runs.
pub const MAX_WORK: u64 = 1 << 31;

const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// SVG-namespace elements the profile draws.
const ALLOWED: &[&str] = &["svg", "g", "defs", "symbol", "use", "path", "rect", "circle", "ellipse", "line",
    "polyline", "polygon", "linearGradient", "radialGradient", "stop", "clipPath", "mask", "pattern",
    "title", "desc", "metadata", "style", "a", "switch", "view"];

/// Refused by name, with the reason the measurement counts.
fn refused_element(name: &str) -> Option<&'static str> {
    Some(match name {
        "script" => "script",
        "foreignObject" => "foreignObject",
        "image" | "feImage" => "embedded image",
        "text" | "tspan" | "textPath" | "tref" | "font" | "font-face" | "glyph" | "altGlyph" => "text (v1)",
        "filter" => "filter",
        n if n.starts_with("fe") => "filter",
        "animate" | "animateMotion" | "animateTransform" | "animateColor" | "set" | "mpath" | "discard" => "animation",
        "marker" => "marker",
        "iframe" | "video" | "audio" | "canvas" | "object" | "embed" => "embedded content",
        "cursor" => "cursor",
        _ => return None,
    })
}

/// What passed the profile: the tree's figures, for the work check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked { pub elements: usize, pub expanded: u64, pub layer_depth: usize, pub segments: usize }

/// The text with a bare DOCTYPE removed, or a refusal for an internal subset
/// or an entity.
fn without_doctype(text: &str) -> Result<std::borrow::Cow<'_, str>, &'static str> {
    let low_has = |n: &str| text.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n.as_bytes()));
    if low_has("<!ENTITY") { return Err("DTD entity") }
    let Some(at) = text.find("<!DOCTYPE").or_else(|| text.find("<!doctype")) else { return Ok(text.into()) };
    let rest = &text[at..];
    let end = rest.find('>').ok_or("unterminated DOCTYPE")?;
    if rest[..end].contains('[') { return Err("DTD internal subset") }
    let mut s = String::with_capacity(text.len());
    s.push_str(&text[..at]);
    s.push_str(&rest[end + 1..]);
    Ok(s.into())
}

fn local_refs_only(v: &str) -> bool {
    let mut rest = v;
    while let Some(i) = rest.find("url(") {
        let after = rest[i + 4..].trim_start_matches([' ', '"', '\'']);
        if !after.starts_with('#') { return false }
        rest = &rest[i + 4..];
    }
    true
}

/// Path segments: one per command letter (implicit repeats are bounded by
/// the bytes anyway, and usvg's own parser bounds them by length).
fn segments(d: &str) -> usize { d.bytes().filter(|b| b"MLHVCSQTAZmlhvcsqtaz".contains(b)).count() }

/// Check a parsed document against the profile.
pub fn check(doc: &roxmltree::Document) -> Result<Checked, &'static str> {
    let root = doc.root_element();
    if root.tag_name().name() != "svg" { return Err("root is not svg") }
    let mut ids: std::collections::HashMap<&str, roxmltree::Node> = Default::default();
    fn walk<'a, 'i>(n: roxmltree::Node<'a, 'i>, depth: usize, layer: usize, st: &mut (usize, usize, usize, usize),
                    ids: &mut std::collections::HashMap<&'a str, roxmltree::Node<'a, 'i>>) -> Result<(), &'static str> {
        if !n.is_element() { return Ok(()) }
        if depth > MAX_DEPTH { return Err("nested too deeply") }
        st.0 += 1;
        if st.0 > MAX_ELEMENTS { return Err("too many elements") }
        let name = n.tag_name().name();
        let in_svg = n.tag_name().namespace().is_none_or(|ns| ns == SVG_NS);
        if in_svg {
            if let Some(why) = refused_element(name) { return Err(why) }
            if !ALLOWED.contains(&name) { return Err("element not in the SVG profile") }
        }
        let mut layer = layer;
        for a in n.attributes() {
            let (an, v) = (a.name(), a.value());
            if an.starts_with("on") { return Err("event handler") }
            if an == "href" && !v.starts_with('#') { return Err("external reference") }
            if !local_refs_only(v) { return Err("external reference") }
            if an == "filter" && v.trim() != "none" { return Err("filter") }
            if an == "style" && (v.contains("@import") || v.contains("filter:") && !v.contains("filter:none")) { return Err("filter") }
            if an == "d" { st.1 += segments(v) }
            if an == "id" { ids.insert(v, n); }
        }
        if n.attribute("mask").is_some() || n.attribute("clip-path").is_some()
            || n.attribute("opacity").is_some_and(|o| o.trim() != "1") || name == "mask" || name == "pattern" {
            layer += 1;
            st.3 = st.3.max(layer);
            if layer > MAX_LAYER_DEPTH { return Err("layers nested too deeply") }
        }
        if st.1 > MAX_PATH_SEGMENTS { return Err("too many path segments") }
        if name == "stop" { st.2 += 1; if st.2 > MAX_STOPS { return Err("too many gradient stops") } }
        if name == "style" {
            let css: String = n.children().filter_map(|c| c.text()).collect();
            if css.contains("@import") { return Err("external reference") }
            if css.contains("@font-face") { return Err("text (v1)") }
            if !local_refs_only(&css) { return Err("external reference") }
            if css.contains("filter") { return Err("filter") }
        }
        for c in n.children() { walk(c, depth + 1, layer, st, ids)? }
        Ok(())
    }
    let mut st = (0usize, 0usize, 0usize, 0usize);
    walk(root, 1, 0, &mut st, &mut ids)?;
    let (elements, segs, _stops, max_layer) = st;
    // ★ `<use>` expansion, the billion-laughs of SVG: a `use` of a group of
    // ten `use`s of a group of ten… Computed bottom-up with memoisation, so
    // the check itself is linear however large the expansion would be; a
    // reference cycle is refused.
    fn expanded<'a, 'i>(n: roxmltree::Node<'a, 'i>, ids: &std::collections::HashMap<&'a str, roxmltree::Node<'a, 'i>>,
                        memo: &mut std::collections::HashMap<roxmltree::NodeId, u64>,
                        busy: &mut std::collections::HashSet<roxmltree::NodeId>) -> Result<u64, &'static str> {
        if let Some(v) = memo.get(&n.id()) { return Ok(*v) }
        if !busy.insert(n.id()) { return Err("use cycle") }
        let mut total: u64 = 1;
        if n.tag_name().name() == "use" {
            if let Some(target) = n.attributes().find(|a| a.name() == "href").and_then(|a| a.value().strip_prefix('#')).and_then(|id| ids.get(id)) {
                total = total.saturating_add(expanded(*target, ids, memo, busy)?);
            }
        }
        for c in n.children().filter(|c| c.is_element()) {
            total = total.saturating_add(expanded(c, ids, memo, busy)?);
            if total > MAX_EXPANDED_NODES { break }
        }
        busy.remove(&n.id());
        memo.insert(n.id(), total);
        Ok(total)
    }
    let mut memo = Default::default();
    let mut busy = Default::default();
    let expanded = expanded(root, &ids, &mut memo, &mut busy)?;
    if expanded > MAX_EXPANDED_NODES { return Err("too many nodes after use expansion") }
    Ok(Checked { elements, expanded, layer_depth: max_layer, segments: segs })
}

fn refuse(why: &'static str) -> DecodeError { DecodeError::Refused { format: Format::Svg, why } }

/// Rasterise an SVG at the painted size (`c.target`; else its own size;
/// else 300 × 150), under the profile and the ceilings.
pub fn rasterize(bytes: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    if bytes.len() > MAX_SVG_BYTES { return Err(refuse("over the SVG size limit")) }
    if bytes.starts_with(&[0x1f, 0x8b]) { return Err(refuse("compressed (svgz)")) }
    let text = std::str::from_utf8(bytes).map_err(|_| refuse("not UTF-8"))?;
    let text = without_doctype(text).map_err(refuse)?;
    let opt = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: (MAX_ELEMENTS as u32) * 8, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(&text, opt)
        .map_err(|e| DecodeError::Corrupt { format: Format::Svg, why: format!("XML: {e}") })?;
    let checked = check(&doc).map_err(refuse)?;
    // Nothing outside the document can be reached: no files, no data URLs.
    let options = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver { resolve_data: Box::new(|_, _, _| None), resolve_string: Box::new(|_, _| None) },
        default_size: usvg::Size::from_wh(300.0, 150.0).expect("size"),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_xmltree(&doc, &options)
        .map_err(|e| DecodeError::Corrupt { format: Format::Svg, why: format!("{e}") })?;
    let size = tree.size();
    let natural = (size.width().ceil().max(1.0) as u32, size.height().ceil().max(1.0) as u32);
    let (w, h) = c.target.unwrap_or(natural);
    if w == 0 || h == 0 { return Err(DecodeError::Empty { format: Format::Svg }) }
    if w > c.max_dimension || h > c.max_dimension { return Err(DecodeError::TooLarge { format: Format::Svg, width: w, height: h }) }
    let out = w as usize * h as usize * BYTES_PER_PIXEL;
    if out > c.budget { return Err(DecodeError::OverBudget { format: Format::Svg, needed: out, left: c.budget }) }
    let transient = out.saturating_mul(1 + checked.layer_depth);
    if transient > c.frame { return Err(DecodeError::FrameTooLarge { format: Format::Svg, needed: transient, limit: c.frame }) }
    if checked.expanded.saturating_mul(w as u64 * h as u64) > MAX_WORK { return Err(refuse("too much drawing work for its size")) }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).ok_or(DecodeError::Empty { format: Format::Svg })?;
    let t = resvg::tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height());
    resvg::render(&tree, t, &mut pixmap.as_mut());
    // tiny-skia is premultiplied; the worker's output is straight alpha.
    let mut rgba = Vec::with_capacity(out);
    for p in pixmap.pixels() {
        let d = p.demultiply();
        rgba.extend_from_slice(&[d.red(), d.green(), d.blue(), d.alpha()]);
    }
    Ok(Pixels { width: w, height: h, rgba })
}
