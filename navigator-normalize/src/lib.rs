//! The normalizer — Atrium Document Profile v1 §6.
//!
//! Accepts real-world HTML and emits a **profile-conformant** document, or
//! fails. It is allowed to be as lenient as it likes, because its output is
//! CHECKABLE: the acceptance test is that the profile renderer reports ZERO
//! diagnostics on what comes out.
//!
//! ★ The design decision that makes it tractable: **it resolves the cascade
//! itself and emits one flat class per distinct declaration block.** It does
//! not try to rewrite each unadmitted construct into an admitted one — which
//! is impossible in general for a descendant combinator — it evaluates the
//! selector, keeps the result, and throws the selector away. Eleven kinds of
//! unadmitted selector, `!important`, shorthands, `var()`, `@media` and
//! inline `style=` attributes all collapse into that one move.
//!
//! What it cannot represent, it DROPS and reports: a pseudo-element's
//! generated content, a property outside the profile's 64 rows, a layered
//! background. Silence would be the only real failure.

pub mod css;
pub mod expand;
pub mod recording;
pub mod sel;
pub mod value;

use navigator_dom::{Dom, Handle, Kind};
use navigator_style::token::{tokenize, Tok, Token};
use std::collections::BTreeMap;

/// What the input supplies besides the HTML. ★ The normalizer has NO
/// capabilities (§6): stylesheets and intrinsic sizes are handed to it.
#[derive(Default, Debug)]
pub struct Inputs {
    /// `href` → stylesheet text, for every `<link rel=stylesheet>`.
    pub stylesheets: BTreeMap<String, String>,
    /// `src` → (intrinsic width px, height px), for every image.
    pub images: BTreeMap<String, ImageSize>,
    /// `href` → the `media` attribute of the `<link>` that named it. A sheet
    /// fetched under a condition must be APPLIED under it (§3.11), or a
    /// dark-mode stylesheet lands on every reader.
    pub stylesheet_media: BTreeMap<String, String>,
}

/// ★ An image's NATURAL sizing (CSS Images 3 §5.1), each part optional —
/// a width alone, a ratio alone, or nothing, as well as the usual pair.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ImageSize { pub width: Option<u32>, pub height: Option<u32>, pub ratio: Option<(u32, u32)> }

impl From<(u32, u32)> for ImageSize {
    fn from((w, h): (u32, u32)) -> Self { ImageSize { width: Some(w), height: Some(h), ratio: Some((w, h)) } }
}

#[derive(Debug, Default)]
pub struct Report {
    pub rules_in: usize,
    pub rules_out: usize,
    pub elements_styled: usize,
    /// Table columns measured offline (§3.13).
    pub columns_measured: usize,
    /// What could not be represented, by reason, with a count.
    pub dropped: BTreeMap<String, usize>,
}

impl Report {
    fn drop(&mut self, why: impl Into<String>) { *self.dropped.entry(why.into()).or_default() += 1 }
    fn drop_n(&mut self, why: impl Into<String>, n: usize) { *self.dropped.entry(why.into()).or_default() += n }
}

/// One element's resolved declarations, in cascade order.
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Block(Vec<(String, String)>);

/// The winning declaration for a property:
/// (important, LAYER RANK, specificity, source order).
///
/// ★ The layer rank sits ABOVE specificity, because a cascade layer is not
/// a tie-breaker — an unlayered declaration beats a layered one however
/// specific the layered one is (CSS Cascade 5 §6.4.4). And for `!important`
/// the order REVERSES: unlayered important is the weakest, and an earlier
/// layer beats a later one.
type Priority = (bool, usize, (u32, u32, u32), usize);

/// Where a rule's layer ranks, for normal and for important declarations.
fn layer_rank(layer: Option<usize>, layers: usize, important: bool) -> usize {
    match (layer, important) {
        (None, false) => usize::MAX,   // unlayered wins
        (None, true) => 0,             // …except when important: then weakest
        (Some(i), false) => i + 1,     // later layer wins
        (Some(i), true) => layers.saturating_sub(i), // earlier layer wins
    }
}

/// The whole job: normalize, then PRE-MEASURE (§3.13). Measurement needs the
/// pinned font set, which is why it takes one — and why the font set version
/// belongs in this function's cache key.
pub fn normalize_and_measure(html: &str, inputs: &Inputs, fonts: &navigator_render::fontset::FontSet,
                             env: &navigator_style::cascade::Env) -> (String, Report) {
    let (doc, mut report) = normalize(html, inputs);
    let (measured, n) = navigator_render::html::premeasure_tables(&doc, fonts, env);
    if n > 0 { report.columns_measured = n }
    (measured, report)
}

pub fn normalize(html: &str, inputs: &Inputs) -> (String, Report) {
    let mut dom = navigator_dom::parse(html);
    let mut report = Report::default();

    // 1. Collect CSS: every <style>, every supplied <link>, in document order.
    let mut sheet = css::Sheet::default();
    let mut order = 0usize;
    for h in dom.by_tag_anywhere("style") {
        let text = expand_imports(&dom.text_content(h), "", inputs, 0, &mut report);
        css::parse(&text, &mut order, &mut sheet);
    }
    for h in dom.by_tag_anywhere("link") {
        let is_sheet = dom.attr(h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet")));
        if !is_sheet { continue }
        let href = dom.attr(h, "href").unwrap_or("").to_string();
        match inputs.stylesheets.get(&href) {
            Some(text) => {
                let text = text.clone();
                let text = expand_imports(&text, &href, inputs, 0, &mut report);
                // The document's own attribute, or the one the converter
                // recorded when it fetched the sheet.
                let media = dom.attr(h, "media").map(str::to_string)
                    .filter(|m| !m.is_empty())
                    .or_else(|| inputs.stylesheet_media.get(&href).cloned())
                    .unwrap_or_default();
                css::parse_in_media(&text, &media, &mut order, &mut sheet)
            }
            None => report.drop("stylesheet not supplied by the input"),
        }
    }
    for (what, _) in &sheet.dropped { report.drop(format!("@{what} not representable")) }
    report.rules_in = sheet.rules.len();

    // 2. The cascade, resolved by this program rather than by the renderer.
    let els: Vec<Handle> = (0..dom.nodes.len() as Handle)
        .filter(|h| matches!(dom.get(*h).map(|n| &n.kind), Some(Kind::Element(_)))).collect();
    let matcher_dom = navigator_dom::parse(html); // a stable copy to match against
    let m = sel::Matcher { dom: &matcher_dom };

    // ★ Custom properties go through the CASCADE, per element and per media
    // context, and then inherit. Taking the last `--x` in the file instead —
    // which is the obvious shortcut — picks up whatever a
    // `@media (prefers-color-scheme: dark)` block set, and renders every
    // page in its dark palette no matter the environment. That is what the
    // first version did, and the render is what showed it.
    let custom = resolve_customs(&sheet, &els, &m, &dom);

    // key: (element, state, media) -> property -> (priority, value)
    type Key = (Handle, String, String);
    let mut resolved: BTreeMap<Key, BTreeMap<String, (Priority, String)>> = BTreeMap::new();

    // Elements drawn through a mask (see below).
    let mut masked: std::collections::BTreeSet<Handle> = Default::default();

    // Media contexts that define custom properties: a base rule using
    // `var()` must be re-resolved in each of them.
    let var_contexts: Vec<String> = custom.keys().map(|(_, m)| m.clone())
        .filter(|m| !m.is_empty()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();

    for rule in &sheet.rules {
        let list = match sel::parse_list(&rule.selector) {
            Ok(l) => l,
            Err(e) => { report.drop(format!("selector not understood: {e}")); continue }
        };
        let media = match &rule.media {
            None => String::new(),
            Some(q) => match admitted_media(q) {
                Some(q) => q,
                None => { report.drop(format!("@media {q} not admitted")); continue }
            },
        };
        for c in &list {
            if sel::has_pseudo_element(c) { report.drop("pseudo-element (no generated content in the profile)"); continue }
            let spec = sel::specificity(c);
            let states = sel::subject_states(c);
            if let Some(bad) = sel::misplaced_state(c) {
                report.drop(format!("dynamic state `:{bad}` on something other than the styled element"));
                continue;
            }
            // ★ A state the profile does not admit cannot be emitted: the
            // matcher treats every unknown pseudo-class as a state, so
            // `:-moz-placeholder` would otherwise travel into the output and
            // be refused there. The rule is dropped, and said so.
            if let Some(bad) = states.iter().find(|s| !navigator_style::selector::STATES.contains(&s.as_str())) {
                report.drop(format!("pseudo-class `:{bad}` (not an admitted state)"));
                continue;
            }
            let state = states.join(":");
            for h in &els {
                if !m.matches(*h, c) { continue }
                let key = (*h, state.clone(), media.clone());
                let slot = resolved.entry(key).or_default();
                let empty = BTreeMap::new();
                let scope = custom.get(&(*h, media.clone())).or_else(|| custom.get(&(*h, String::new()))).unwrap_or(&empty);
                let mut pending: Vec<(String, BTreeMap<String, (Priority, String)>)> = vec![];
                for d in &rule.decls {
                    // ★ A MASKED box is drawn THROUGH its mask: the colour is
                    // the ink, the mask is the shape. The profile admits no
                    // mask, and painting the fill unmasked turns every icon
                    // into a solid square — Wikipedia's logo and search icon
                    // came out as two black blocks. Better to paint nothing.
                    if d.name.contains("mask") && !css::write_tokens(&d.value).trim().eq_ignore_ascii_case("none") {
                        masked.insert(*h);
                    }
                }
                for d in &rule.decls {
                    let value = substitute(&d.value, scope, 0);
                    // ★ `light-dark(L, D)` (CSS Color 5): the LIGHT value here,
                    // and the dark one in a `prefers-color-scheme: dark`
                    // block below — the same shape the dark-mode custom
                    // properties already compile to.
                    let dark_value = has_light_dark(&value).then(|| pick_light_dark(&value, true));
                    let value = pick_light_dark(&value, false);
                    if let (Some(dv), true) = (&dark_value, media.is_empty()) {
                        let mut block = BTreeMap::new();
                        for (prop, v) in to_longhands(&d.name, dv, inputs, &mut report) {
                            block.insert(prop, ((d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order), v));
                        }
                        if !block.is_empty() { pending.push(("(prefers-color-scheme: dark)".to_string(), block)) }
                    }
                    for (prop, v) in to_longhands(&d.name, &value, inputs, &mut report) {
                        let pri: Priority = (d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order);
                        match slot.get(&prop) {
                            Some((old, _)) if *old > pri => {}
                            _ => { slot.insert(prop, (pri, v)); }
                        }
                    }
                    // ★ A rule that uses `var()` has a DIFFERENT value in
                    // every media context whose customs differ — that is how
                    // `body { background: var(--bg) }` goes dark under
                    // `prefers-color-scheme`. One resolution is not enough:
                    // the rule is re-resolved into each such context.
                    if media.is_empty() && uses_var(&d.value) {
                        for ctx in &var_contexts {
                            let Some(cs) = custom.get(&(*h, ctx.clone())) else { continue };
                            // light-dark() picks its side by THIS context.
                            let alt = pick_light_dark(&substitute(&d.value, cs, 0), ctx.contains("prefers-color-scheme: dark"));
                            if alt == value { continue }
                            let mut block = BTreeMap::new();
                            for (prop, v) in to_longhands(&d.name, &alt, inputs, &mut report) {
                                block.insert(prop, ((d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order), v));
                            }
                            if !block.is_empty() { pending.push((ctx.clone(), block)) }
                        }
                    }
                }
                for (ctx, block) in pending {
                    let s2 = resolved.entry((*h, state.clone(), ctx)).or_default();
                    for (prop, val) in block {
                        match s2.get(&prop) { Some((old, _)) if *old > val.0 => {} _ => { s2.insert(prop, val); } }
                    }
                }
            }
        }
    }

    // 4. Inline `style=` attributes: higher than any rule, as CSS says, and
    //    the single biggest refusal in the corpus (21 of 29 documents).
    for h in &els {
        let Some(style) = dom.attr(*h, "style").map(str::to_string) else { continue };
        let decls = css::declarations(&tokenize(&style));
        let empty = BTreeMap::new();
        let scope = custom.get(&(*h, String::new())).unwrap_or(&empty);
        let scope = scope.clone();
        let slot = resolved.entry((*h, String::new(), String::new())).or_default();
        for d in &decls {
            let value = pick_light_dark(&substitute(&d.value, &scope, 0), false);
            for (prop, v) in to_longhands(&d.name, &value, inputs, &mut report) {
                let pri: Priority = (d.important, usize::MAX, (u32::MAX, 0, 0), usize::MAX);
                slot.insert(prop, (pri, v));
            }
        }
    }

    // A masked element paints neither its background colour nor its image:
    // without the mask both are the wrong shape.
    let mut unmasked = 0usize;
    for ((h, ..), props) in resolved.iter_mut() {
        if !masked.contains(h) { continue }
        let had = props.remove("background-color").is_some() | props.remove("background-image").is_some();
        if had { unmasked += 1 }
    }
    if unmasked > 0 { report.drop_n("background of a masked box (no mask in the profile)", unmasked) }

    // ★ `display: contents` elements generate no box: their children take
    // their place. Doing it here, on the DOM, is exactly what the value
    // means — and it is why the profile needs no such value.
    let mut contents: Vec<Handle> = resolved.iter()
        .filter(|((_, state, media), props)| state.is_empty() && media.is_empty()
            && props.get("display").is_some_and(|(_, v)| v == "contents"))
        .map(|((h, ..), _)| *h).collect();
    contents.sort_unstable();
    for h in &contents {
        // Its own declarations describe a box that does not exist.
        resolved.retain(|(e, ..), _| e != h);
    }
    let spliced = contents.len();
    if spliced > 0 { report.drop_n("display: contents (children took its place)", spliced) }
    for h in contents.into_iter().rev() {
        let Some(parent) = dom.get(h).and_then(|n| n.parent) else { continue };
        let kids = dom.get(h).map(|n| n.children.clone()).unwrap_or_default();
        if let Some(p) = dom.get_mut(parent) {
            if let Some(at) = p.children.iter().position(|c| *c == h) {
                p.children.splice(at..=at, kids.iter().copied());
            }
        }
        for k in kids { if let Some(n) = dom.get_mut(k) { n.parent = Some(parent) } }
    }

    // ★ PRESENTATIONAL HINTS (HTML §15 "Rendering"; CSS Cascade 5 puts them
    // in their own origin, BELOW every author rule). `<td width=100>`,
    // `bgcolor`, `align`, `valign`, `cellpadding`, `<table border>`, an
    // `<img>`'s own width and height: each maps to a CSS declaration, and
    // was dropped instead — 4,288 of them in the corpus, plus every `<img
    // width height>`, which the natural-size declaration overwrote. A hint
    // goes in only where no author rule set that property.
    let mut translated: std::collections::BTreeSet<(Handle, &'static str)> = Default::default();
    {
        let len = |v: &str| -> Option<String> {
            let v = v.trim();
            if let Some(p) = v.strip_suffix('%') { return p.trim().parse::<f64>().ok().filter(|n| *n >= 0.0).map(|n| format!("{n}%")) }
            let num: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            num.parse::<f64>().ok().filter(|n| *n >= 0.0).map(|n| format!("{n}px"))
        };
        let color = |v: &str| -> Option<String> {
            let v = v.trim();
            if v.starts_with('#') && v.len() > 1 { return Some(v.to_ascii_lowercase()) }
            if (v.len() == 6 || v.len() == 3) && v.chars().all(|c| c.is_ascii_hexdigit()) { return Some(format!("#{}", v.to_ascii_lowercase())) }
            v.chars().all(|c| c.is_ascii_alphabetic()).then(|| v.to_ascii_lowercase()).filter(|x| !x.is_empty())
        };
        // The nearest <table> above a cell, for the attributes a table
        // passes down (cellpadding, border).
        let table_of = |h: Handle| -> Option<Handle> {
            let mut cur = dom.get(h).and_then(|n| n.parent);
            while let Some(x) = cur {
                if dom.tag(x) == Some("table") { return Some(x) }
                cur = dom.get(x).and_then(|n| n.parent);
            }
            None
        };
        let mut hints: Vec<(Handle, &str, String)> = vec![];
        for h in &els {
            let tag = dom.tag(*h).unwrap_or("").to_ascii_lowercase();
            let a = |n: &str| dom.attr(*h, n).map(str::to_string);
            let mut hint = |attr: &'static str, prop: &'static str, v: Option<String>| {
                if let Some(v) = v { hints.push((*h, prop, v)); translated.insert((*h, attr)); }
            };
            let t = tag.as_str();
            // An inline <svg>'s width/height are presentation attributes too:
            // the icon's box. Only the OUTER svg — shapes inside it keep their
            // own geometry, untouched (below).
            let outer_svg = t == "svg" && !dom.get(*h).and_then(|n| n.parent).is_some_and(|p| dom.is_foreign(p));
            if matches!(t, "table" | "td" | "th" | "col" | "img" | "hr") || outer_svg { hint("width", "width", a("width").and_then(|v| len(&v))) }
            if matches!(t, "table" | "td" | "th" | "tr" | "img") || outer_svg { hint("height", "height", a("height").and_then(|v| len(&v))) }
            if matches!(t, "body" | "table" | "tr" | "td" | "th") { hint("bgcolor", "background-color", a("bgcolor").and_then(|v| color(&v))) }
            if t == "body" { hint("text", "color", a("text").and_then(|v| color(&v))) }
            if let Some(al) = a("align").map(|v| v.trim().to_ascii_lowercase()) {
                match (t, al.as_str()) {
                    ("td" | "th" | "tr" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "thead" | "tbody" | "tfoot" | "caption",
                     "left" | "right" | "center" | "justify") => hint("align", "text-align", Some(al.clone())),
                    ("table" | "img", "left" | "right") => hint("align", "float-marker", Some(al.clone())),
                    ("table" | "hr", "center") => { hint("align", "margin-left", Some("auto".into())); hint("align", "margin-right", Some("auto".into())) }
                    ("img", "top" | "middle" | "bottom" | "baseline") => hint("align", "vertical-align", Some(al.clone())),
                    ("img", "absmiddle") => hint("align", "vertical-align", Some("middle".into())),
                    _ => {}
                }
            }
            if matches!(t, "td" | "th" | "tr") {
                hint("valign", "vertical-align", a("valign").map(|v| v.trim().to_ascii_lowercase()).filter(|v| matches!(v.as_str(), "top" | "middle" | "bottom" | "baseline")));
            }
            if matches!(t, "td" | "th") && a("nowrap").is_some() { hint("nowrap", "white-space", Some("nowrap".into())) }
            if t == "table" { hint("cellspacing", "border-spacing", a("cellspacing").and_then(|v| len(&v)).map(|v| format!("{v} {v}"))) }
            if t == "img" {
                let px = |n: &str| a(n).and_then(|v| len(&v)).filter(|v| v.ends_with("px"));
                if let Some(b) = px("border") { for side in ["top", "right", "bottom", "left"] {
                    hints.push((*h, ["border-top-width", "border-right-width", "border-bottom-width", "border-left-width"][["top", "right", "bottom", "left"].iter().position(|x| *x == side).unwrap()], b.clone()));
                    hints.push((*h, ["border-top-style", "border-right-style", "border-bottom-style", "border-left-style"][["top", "right", "bottom", "left"].iter().position(|x| *x == side).unwrap()], "solid".into()));
                } translated.insert((*h, "border")); }
                if let Some(v) = px("hspace") { hints.push((*h, "margin-left", v.clone())); hints.push((*h, "margin-right", v)); translated.insert((*h, "hspace")); }
                if let Some(v) = px("vspace") { hints.push((*h, "margin-top", v.clone())); hints.push((*h, "margin-bottom", v)); translated.insert((*h, "vspace")); }
            }
            if t == "table" {
                // `border` with no value is `border=1`; 0 draws nothing.
                let b = a("border").map(|v| if v.trim().is_empty() { "1px".to_string() } else { len(&v).unwrap_or_default() });
                if let Some(b) = b.filter(|b| b.ends_with("px") && b != "0px") {
                    for (w, st, c) in [("border-top-width", "border-top-style", "border-top-color"), ("border-right-width", "border-right-style", "border-right-color"),
                                       ("border-bottom-width", "border-bottom-style", "border-bottom-color"), ("border-left-width", "border-left-style", "border-left-color")] {
                        hints.push((*h, w, b.clone())); hints.push((*h, st, "solid".into())); hints.push((*h, c, "#808080".into()));
                    }
                    translated.insert((*h, "border"));
                }
                if a("cellpadding").is_some() || a("border").is_some() { translated.insert((*h, "cellpadding")); }
            }
            // What a table passes to its own cells: `cellpadding`, and a 1px
            // border on every cell when the table has one.
            if matches!(t, "td" | "th") {
                if let Some(tb) = table_of(*h) {
                    if let Some(p) = dom.attr(tb, "cellpadding").and_then(|v| len(v)) {
                        for side in ["padding-top", "padding-right", "padding-bottom", "padding-left"] { hints.push((*h, side, p.clone())) }
                    }
                    let tb_border = dom.attr(tb, "border").map(|v| v.trim().is_empty() || len(v).is_some_and(|b| b != "0px")).unwrap_or(false);
                    if tb_border {
                        for (w, st, c) in [("border-top-width", "border-top-style", "border-top-color"), ("border-right-width", "border-right-style", "border-right-color"),
                                           ("border-bottom-width", "border-bottom-style", "border-bottom-color"), ("border-left-width", "border-left-style", "border-left-color")] {
                            hints.push((*h, w, "1px".into())); hints.push((*h, st, "solid".into())); hints.push((*h, c, "#808080".into()));
                        }
                    }
                }
            }
        }
        let mut n = 0usize;
        for (h, prop, v) in hints {
            let slot = resolved.entry((h, String::new(), String::new())).or_default();
            // Below EVERY author declaration: only where none exists.
            if slot.contains_key(prop) { continue }
            slot.insert(prop.to_string(), ((false, 0, (0, 0, 0), 0), v));
            n += 1;
        }
        if n > 0 { report.drop_n("presentational attribute translated to CSS", n) }
    }

    // ★ FLOAT ROWS. The profile has no floats, and dropping `float` left
    // GitHub's three repository-action buttons stacked one per line: they are
    // `<li float:left>` inside a `<ul>`. When EVERY in-flow element child of a
    // parent floats the same way and the parent holds no text of its own,
    // those children are a horizontal strip, and `inline-block` lays them out
    // in the same places. That condition is the whole point: a single image
    // floated into a paragraph is NOT a strip — text is meant to wrap around
    // it, which inline-block would not do — so it keeps being dropped.
    let mut float_of: BTreeMap<Handle, String> = BTreeMap::new();
    for ((h, st, media), props) in resolved.iter() {
        if !st.is_empty() || !media.is_empty() { continue }
        if let Some((_, v)) = props.get("float-marker") { float_of.insert(*h, v.clone()); }
    }
    let mut rows = 0usize;
    let mut lone = 0usize;
    let parents: Vec<Handle> = float_of.keys()
        .filter_map(|h| dom.get(*h).and_then(|n| n.parent))
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let mut strip: Vec<Handle> = vec![];
    for p in parents {
        let kids = dom.element_children(p);
        // Only the children that generate a box in flow count.
        let inflow: Vec<Handle> = kids.into_iter()
            .filter(|k| resolved.get(&(*k, String::new(), String::new()))
                .and_then(|pr| pr.get("display")).map(|(_, v)| v != "none").unwrap_or(true))
            .collect();
        // CSS 2.1 §9.7: float does not apply to an internal table element.
        // A stylesheet that floats `td` is describing nothing, and taking it
        // for a strip would dismantle the table.
        const TABLE: [&str; 9] = ["td", "th", "tr", "thead", "tbody", "tfoot", "caption", "col", "colgroup"];
        if inflow.iter().any(|k| dom.tag(*k).is_some_and(|t| TABLE.contains(&t))) { continue }
        let dirs: Vec<&str> = inflow.iter().map(|k| float_of.get(k).map(String::as_str).unwrap_or("none")).collect();
        let all_left = dirs.len() >= 2 && dirs.iter().all(|d| *d == "left");
        let has_text = dom.children_of(p).iter().any(|c|
            dom.get(*c).is_some_and(|n| matches!(n.kind, Kind::Text(ref t) if !t.trim().is_empty())));
        if all_left && !has_text { rows += 1; strip.extend(inflow) } else { lone += dirs.iter().filter(|d| **d != "none").count() }
    }
    for h in strip {
        let slot = resolved.entry((h, String::new(), String::new())).or_default();
        let keep = slot.get("display").map(|(_, v)| matches!(v.as_str(), "flex" | "grid" | "inline-flex" | "inline-grid" | "table")).unwrap_or(false);
        if !keep {
            let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
            slot.insert("display".to_string(), (pri, "inline-block".to_string()));
        }
    }
    if rows > 0 { report.drop_n("float row laid out as inline-block", rows) }
    if lone > 0 { report.drop_n("property `float` (not a row; dropped)", lone) }
    for (_, props) in resolved.iter_mut() { props.remove("float-marker"); }

    // ★ BOX-SIZING, compiled away. The profile's boxes are all border-box;
    // CSS's default is content-box, where `width` excludes padding and
    // border. So for every element whose EFFECTIVE box-sizing is content-box
    // (following `inherit` up the tree, for the `* { box-sizing: inherit }`
    // idiom), each size is rewritten to the border-box size that means the
    // same thing: `width: 300px; padding: 0 20px` becomes `width: 340px`,
    // and `width: 50%` with em padding becomes `calc(50% + 2em)`. Per media
    // context, because padding and width change at breakpoints. The UA's own
    // padding counts too — a `<ul>`'s 40 px is padding like any other.
    {
        let mut ua_sheet = css::Sheet::default();
        let mut ua_order = 0usize;
        css::parse(navigator_style::cascade::UA_CSS, &mut ua_order, &mut ua_sheet);
        const FRAME: [&str; 12] = ["padding-left", "padding-right", "padding-top", "padding-bottom",
            "border-left-width", "border-right-width", "border-top-width", "border-bottom-width",
            "border-left-style", "border-right-style", "border-top-style", "border-bottom-style"];
        let mut ua: BTreeMap<Handle, BTreeMap<String, String>> = BTreeMap::new();
        for rule in &ua_sheet.rules {
            let Ok(list) = sel::parse_list(&rule.selector) else { continue };
            for h in &els {
                if !list.iter().any(|c| m.matches(*h, c)) { continue }
                for d in rule.decls.iter().filter(|d| FRAME.contains(&d.name.as_str())) {
                    ua.entry(*h).or_default().insert(d.name.clone(), css::write_tokens(&d.value).trim().to_string());
                }
            }
        }
        let get = |h: Handle, ctx: &str, p: &str| -> Option<String> {
            resolved.get(&(h, String::new(), ctx.to_string())).and_then(|x| x.get(p)).map(|(_, v)| v.clone())
                .or_else(|| resolved.get(&(h, String::new(), String::new())).and_then(|x| x.get(p)).map(|(_, v)| v.clone()))
                .or_else(|| if p == "box-sizing-marker" { None } else { ua.get(&h).and_then(|x| x.get(p)).cloned() })
        };
        let parent_el = |h: Handle| dom.get(h).and_then(|n| n.parent)
            .filter(|p| matches!(dom.get(*p).map(|n| &n.kind), Some(Kind::Element(_))));
        let content_box = |h: Handle, ctx: &str| -> bool {
            let mut cur = Some(h);
            while let Some(x) = cur {
                match get(x, ctx, "box-sizing-marker").as_deref() {
                    Some("border-box") => return false,
                    Some("inherit") => cur = parent_el(x),
                    _ => return true, // content-box, initial, unset, revert, or nothing
                }
            }
            true
        };
        let is_zero = |v: &str| matches!(v.trim(), "0" | "0px" | "0%" | "0em" | "0rem");
        let border_px = |v: &str| match v.trim() { "thin" => "1px".to_string(), "medium" => "3px".to_string(), "thick" => "5px".to_string(), x => x.to_string() };
        let mut updates: Vec<(Handle, String, String, String)> = vec![];
        let mut skipped = 0usize;
        let mut contexts: BTreeMap<Handle, Vec<String>> = BTreeMap::new();
        for (h, st, media) in resolved.keys() {
            if st.is_empty() { contexts.entry(*h).or_default().push(media.clone()) }
        }
        for (h, ctxs) in &contexts {
            for ctx in ctxs {
                if !content_box(*h, ctx) { continue }
                let frame = |sides: [&str; 2]| -> Vec<String> {
                    let mut t = vec![];
                    for side in sides {
                        if let Some(p) = get(*h, ctx, &format!("padding-{side}")) { if !is_zero(&p) { t.push(p) } }
                        let style = get(*h, ctx, &format!("border-{side}-style")).unwrap_or_else(|| "none".into());
                        if style != "none" && style != "hidden" {
                            if let Some(b) = get(*h, ctx, &format!("border-{side}-width")) { let b = border_px(&b); if !is_zero(&b) { t.push(b) } }
                        }
                    }
                    t
                };
                let (hf, vf) = (frame(["left", "right"]), frame(["top", "bottom"]));
                if hf.is_empty() && vf.is_empty() { continue }
                let column_parent = parent_el(*h).and_then(|p| get(p, ctx, "flex-direction")).is_some_and(|d| d.starts_with("column"));
                let props: [(&str, bool); 7] = [("width", false), ("min-width", false), ("max-width", false),
                    ("height", true), ("min-height", true), ("max-height", true), ("flex-basis", column_parent)];
                for (prop, vertical) in props {
                    let terms = if vertical { &vf } else { &hf };
                    if terms.is_empty() { continue }
                    // Only a size THIS context states or inherits from the base.
                    let Some(v) = get(*h, ctx, prop) else { continue };
                    let v = v.trim().to_string();
                    let sized = v.starts_with(|c: char| c.is_ascii_digit() || c == '.')
                        || ["calc(", "min(", "max(", "clamp("].iter().any(|f| v.starts_with(f));
                    if !sized { continue } // auto, none, fit-content, …
                    // A vertical percentage padding resolves against the WIDTH;
                    // added to a height percentage it would resolve against the
                    // height instead. Left as written, and counted.
                    if vertical && terms.iter().any(|t| t.contains('%')) { skipped += 1; continue }
                    let base = if v == "0" { "0px".to_string() } else { v.clone() };
                    let px = |x: &str| x.strip_suffix("px").and_then(|n| n.parse::<f64>().ok());
                    let new = match (px(&base), terms.iter().map(|t| px(t)).collect::<Option<Vec<f64>>>()) {
                        (Some(a), Some(ts)) => format!("{}px", a + ts.iter().sum::<f64>()),
                        _ => {
                            let inner = base.strip_prefix("calc(").and_then(|x| x.strip_suffix(')')).map(|x| format!("({x})")).unwrap_or(base.clone());
                            format!("calc({inner} + {})", terms.join(" + "))
                        }
                    };
                    updates.push((*h, ctx.clone(), prop.to_string(), new));
                }
            }
        }
        let converted = updates.len();
        for (h, ctx, prop, v) in updates {
            let slot = resolved.entry((h, String::new(), ctx)).or_default();
            let pri: Priority = slot.get(&prop).map(|(p, _)| *p).unwrap_or((false, usize::MAX, (0, 1, 0), usize::MAX - 1));
            slot.insert(prop, (pri, v));
        }
        if converted > 0 { report.drop_n("content-box size compiled to border-box", converted) }
        if skipped > 0 { report.drop_n("content-box height with a percentage vertical padding (left as written)", skipped) }
        for (_, props) in resolved.iter_mut() { props.remove("box-sizing-marker"); }
    }

    // ★ Resolve named areas into the numeric lines the profile admits —
    // PER MEDIA CONTEXT. A responsive page keeps its whole layout in the
    // media queries: MDN's mobile template lives in
    // `@media (width < 1072px)`, and resolving only the base context left an
    // 800px viewport rendering the DESKTOP layout, sidebars and all.
    let mut templates: BTreeMap<(Handle, String), Vec<Vec<String>>> = BTreeMap::new();
    let mut named: Vec<(Handle, String, String)> = vec![];
    for ((h, st, media), props) in resolved.iter() {
        if !st.is_empty() { continue }
        if let Some((_, v)) = props.get("grid-template-areas") { templates.insert((*h, media.clone()), parse_areas(v)); }
        if let Some((_, v)) = props.get("grid-area-name") { named.push((*h, media.clone(), v.trim().to_string())); }
    }
    // A child placed in the base context also needs placing in every context
    // where its container's template DIFFERS.
    let contexts: Vec<String> = templates.keys().map(|(_, m)| m.clone())
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let mut want: Vec<(Handle, String, String)> = vec![];
    for (h, media, name) in &named {
        want.push((*h, media.clone(), name.clone()));
        if media.is_empty() {
            let parent = dom.get(*h).and_then(|n| n.parent);
            for ctx in &contexts {
                if ctx.is_empty() { continue }
                if parent.is_some_and(|p| templates.contains_key(&(p, ctx.clone()))) {
                    want.push((*h, ctx.clone(), name.clone()));
                }
            }
        }
    }
    let mut placed = 0usize;
    let mut unplaced: Vec<String> = vec![];
    for (h, media, name) in want {
        let parent = dom.get(h).and_then(|n| n.parent);
        let rect = parent.and_then(|p| templates.get(&(p, media.clone())).or_else(|| templates.get(&(p, String::new()))))
            .and_then(|t| area_rect(t, &name));
        let slot = resolved.entry((h, String::new(), media)).or_default();
        slot.remove("grid-area-name");
        match rect {
            Some((r0, r1, c0, c1)) => {
                let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
                for (k, v) in [("grid-row-start", r0), ("grid-row-end", r1), ("grid-column-start", c0), ("grid-column-end", c1)] {
                    slot.insert(k.to_string(), (pri, v.to_string()));
                }
                placed += 1;
            }
            None => unplaced.push(format!("grid-area `{name}` (no template names it)")),
        }
    }
    for u in unplaced { report.drop(u) }
    if placed > 0 { report.drop_n("named grid areas resolved to numbered lines", placed) }
    for (_, props) in resolved.iter_mut() { props.remove("grid-template-areas"); }

    // 5. Emit: one class per distinct block, so elements that resolved to the
    //    same declarations share a rule instead of each getting their own.
    let mut blocks: BTreeMap<(String, String, Block), String> = BTreeMap::new();
    let mut dropped_late: Vec<String> = vec![];
    let mut per_element: BTreeMap<Handle, Vec<String>> = BTreeMap::new();
    for ((h, state, media), props) in resolved {
        // ★ THE LAST GATE. Nothing leaves here that the profile would refuse,
        // whatever path it took to get this far — a `display: contents` that
        // survived because it was inside a media query and so could not be
        // spliced, or a value some expansion produced. One check at the exit
        // is worth more than trusting every entrance.
        let mut b: Vec<(String, String)> = props.into_iter().map(|(p, (_, v))| (p, v))
            .filter(|(p, v)| {
                if value::admits(p, v) { return true }
                dropped_late.push(format!("value `{p}: {v}` (refused at the exit)"));
                false
            }).collect();
        b.sort();
        if b.is_empty() { continue }
        let n = blocks.len();
        let name = blocks.entry((state.clone(), media.clone(), Block(b))).or_insert_with(|| format!("n{n}")).clone();
        per_element.entry(h).or_default().push(name);
    }

    let mut out = String::from("/* normalized: Atrium Document Profile v1 */\n");
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for ((state, media, block), name) in &blocks {
        let decls: String = block.0.iter().map(|(p, v)| format!("{p}: {v}")).collect::<Vec<_>>().join("; ");
        let sel = if state.is_empty() { format!(".{name}") } else { format!(".{name}:{state}") };
        groups.entry(media.clone()).or_default().push(format!("{sel} {{ {decls} }}"));
    }
    for (media, rules) in &groups {
        if media.is_empty() { for r in rules { out.push_str(r); out.push('\n') } }
    }
    for (media, rules) in &groups {
        if !media.is_empty() {
            out.push_str(&format!("@media {media} {{\n"));
            for r in rules { out.push_str("  "); out.push_str(r); out.push('\n') }
            out.push_str("}\n");
        }
    }
    for d in dropped_late { report.drop(d) }
    report.rules_out = blocks.len();
    report.elements_styled = per_element.len();

    // 6. Rewrite the document: generated classes on, `style=` off, `<link>`
    //    and the old `<style>` blocks gone, one new sheet in the head.
    for (h, names) in &per_element {
        let existing = dom.attr(*h, "class").unwrap_or("").to_string();
        let mut v: Vec<String> = existing.split_ascii_whitespace().map(str::to_string).collect();
        v.extend(names.iter().cloned());
        dom.set_attr(*h, "class", &v.join(" "));
    }
    for h in &els {
        dom.remove_attr(*h, "style");
        // Presentational attributes the profile does not admit.
        // ★ SVG (foreign content) keeps EVERY attribute: `width` on a <rect>
        // is geometry, not a presentational hint, and stripping it destroyed
        // the drawing.
        if dom.is_foreign(*h) { continue }
        for a in ["align", "valign", "bgcolor", "border", "cellpadding", "cellspacing", "width", "height", "hspace", "vspace", "nowrap", "text"] {
            // An <img>'s width/height are replaced by its NATURAL declaration below.
            if dom.tag(*h) == Some("img") && (a == "width" || a == "height") { continue }
            if a == "text" && dom.tag(*h) != Some("body") { continue }
            if dom.attr(*h, a).is_some() {
                dom.remove_attr(*h, a);
                // Reported only when nothing carried its meaning forward.
                if !translated.contains(&(*h, a)) { report.drop(format!("presentational attribute `{a}`")) }
            }
        }
    }
    detach_all(&mut dom, "style");
    let links: Vec<Handle> = dom.by_tag_anywhere("link").into_iter()
        .filter(|h| dom.attr(*h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet")))).collect();
    for h in links { detach(&mut dom, h) }
    // Scripts never reach the renderer: the converter has already run them.
    detach_all(&mut dom, "script");

    // 7. Declared intrinsic sizes (§3.13): an image the input measured gets
    //    `width`/`height` attributes; one it did not is DROPPED, because the
    //    profile admits no measurement path and a guess is worse than a gap.
    let imgs: Vec<Handle> = dom.by_tag_anywhere("img");
    for h in imgs {
        let src = dom.attr(h, "src").unwrap_or("").to_string();
        match inputs.images.get(&src) {
            // ★ Only the parts the image HAS are declared. Both dimensions
            // imply their ratio; anything else says its ratio outright —
            // `natural-ratio="W/H"`, or `none` — so the renderer never
            // invents one (an SVG with `width="100"` alone has none).
            Some(n) => {
                dom.remove_attr(h, "width");
                dom.remove_attr(h, "height");
                if let Some(w) = n.width { dom.set_attr(h, "width", &w.to_string()) }
                if let Some(ht) = n.height { dom.set_attr(h, "height", &ht.to_string()) }
                let implied = n.width.zip(n.height);
                let same_ratio = match (implied, n.ratio) {
                    (Some((w, ht)), Some((a, b))) => w as u64 * b as u64 == ht as u64 * a as u64,
                    (None, None) => false,
                    _ => false,
                };
                if !same_ratio {
                    let r = n.ratio.map(|(a, b)| format!("{a}/{b}")).unwrap_or_else(|| "none".into());
                    dom.set_attr(h, "natural-ratio", &r);
                }
            }
            None => { detach(&mut dom, h); report.drop("image without a declared intrinsic size"); }
        }
    }

    // ★ The ROOT's own attributes survive. Wrapping the serialized body in a
    // hardcoded `<html>` threw away `lang` and `dir` — which decide language
    // and base direction — and the classes the page's own cascade keys on.
    let root_attrs = dom.element_children(dom.root()).first().map(|h| {
        ["lang", "dir", "class", "id"].iter().filter_map(|a| dom.attr(*h, a).map(|v| format!(" {a}=\"{}\"", v.replace('"', "&quot;"))))
            .collect::<String>()
    }).unwrap_or_default();
    let body = dom.serialize();
    let doc = format!("<html{root_attrs}><head><style>\n{out}</style></head>{body}</html>\n");
    (doc, report)
}

/// ★ `@import` is where the real CSS usually is. The W3C's own stylesheet
/// for a spec is 123 bytes: one `@import "base.css"`. Dropping the at-rule —
/// which is what a strict parser does — silently discards the entire design
/// of the page, and the render looks like the sheet was never fetched at
/// all. The profile bounds the depth (§3.11), so this follows it to that
/// depth and reports what it could not resolve.
const MAX_IMPORT_DEPTH: usize = 2;

fn expand_imports(css: &str, base: &str, inputs: &Inputs, depth: usize, report: &mut Report) -> String {
    if !css.contains("@import") { return css.to_string() }
    if depth >= MAX_IMPORT_DEPTH { report.drop("@import deeper than the profile's limit"); return css.to_string() }
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(at) = rest.find("@import") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail.find(';').map(|i| i + 1).unwrap_or(tail.len());
        let stmt = &tail[..end];
        // `@import "x";` or `@import url(x);`, with optional media after it.
        let target = stmt.split_once('"').map(|(_, r)| r.split('"').next().unwrap_or("").to_string())
            .or_else(|| stmt.split_once("url(").map(|(_, r)| r.split(')').next().unwrap_or("").trim_matches(['"', '\'']).to_string()))
            .unwrap_or_default();
        match resolve(base, &target).and_then(|k| inputs.stylesheets.get(&k).cloned()) {
            Some(text) => {
                let resolved = resolve(base, &target).unwrap_or(target.clone());
                out.push_str(&expand_imports(&text, &resolved, inputs, depth + 1, report));
            }
            None => report.drop(format!("@import `{target}` not supplied by the input")),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Resolve an import against the importing sheet's own key, which is how the
/// input names it. Only what the input actually has can be resolved.
fn resolve(base: &str, target: &str) -> Option<String> {
    if target.is_empty() { return None }
    if target.starts_with("http") || base.is_empty() { return Some(target.to_string()) }
    match base.rfind('/') {
        Some(i) => Some(format!("{}{}", &base[..i + 1], target)),
        None => Some(target.to_string()),
    }
}

/// `"a a" "b c"` → rows of cell names. `.` is an empty cell.
fn parse_areas(v: &str) -> Vec<Vec<String>> {
    let mut rows = vec![];
    for row in v.split('"').skip(1).step_by(2) {
        let cells: Vec<String> = row.split_whitespace().map(str::to_string).collect();
        if !cells.is_empty() { rows.push(cells) }
    }
    rows
}

/// The 1-based line rectangle a name covers: (row start, row end, column
/// start, column end). CSS requires the area to be rectangular; a name that
/// is not gets the bounding box, which is what a browser resolves it to
/// after its own error handling.
fn area_rect(rows: &[Vec<String>], name: &str) -> Option<(usize, usize, usize, usize)> {
    let (mut r0, mut r1, mut c0, mut c1) = (usize::MAX, 0usize, usize::MAX, 0usize);
    for (r, cells) in rows.iter().enumerate() {
        for (c, cell) in cells.iter().enumerate() {
            if cell == name {
                r0 = r0.min(r); r1 = r1.max(r);
                c0 = c0.min(c); c1 = c1.max(c);
            }
        }
    }
    (r0 != usize::MAX).then(|| (r0 + 1, r1 + 2, c0 + 1, c1 + 2))
}

fn uses_var(v: &[Token]) -> bool {
    v.iter().any(|t| matches!(&t.tok, Tok::Function(f) if f.eq_ignore_ascii_case("var")))
}

fn detach_all(dom: &mut Dom, tag: &str) {
    for h in dom.by_tag_anywhere(tag) { detach(dom, h) }
}

fn detach(dom: &mut Dom, h: Handle) {
    let Some(p) = dom.get(h).and_then(|n| n.parent) else { return };
    if let Some(node) = dom.get_mut(p) { node.children.retain(|c| *c != h) }
    if let Some(node) = dom.get_mut(h) { node.parent = None }
}

/// Custom properties, cascaded per (element, media context) and then
/// INHERITED down the tree — which is what makes a `:root { --bg }` reach
/// the elements that use it, and what keeps a dark-mode override inside its
/// own media context.
fn resolve_customs(sheet: &css::Sheet, els: &[Handle], m: &sel::Matcher, dom: &Dom)
    -> BTreeMap<(Handle, String), BTreeMap<String, Vec<Token>>> {
    // Winners per (element, media), by the same priority rule as any property.
    let mut own: BTreeMap<(Handle, String), BTreeMap<String, (Priority, Vec<Token>)>> = BTreeMap::new();
    let mut medias: Vec<String> = vec![String::new()];
    for rule in &sheet.rules {
        if !rule.decls.iter().any(|d| d.name.starts_with("--")) { continue }
        let Ok(list) = sel::parse_list(&rule.selector) else { continue };
        let media = match &rule.media { None => String::new(), Some(q) => match admitted_media(q) { Some(q) => q, None => continue } };
        if !medias.contains(&media) { medias.push(media.clone()) }
        for c in &list {
            if sel::has_pseudo_element(c) { continue }
            let spec = sel::specificity(c);
            for h in els {
                if !m.matches(*h, c) { continue }
                let slot = own.entry((*h, media.clone())).or_default();
                for d in rule.decls.iter().filter(|d| d.name.starts_with("--")) {
                    let pri: Priority = (d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order);
                    match slot.get(&d.name) {
                        Some((old, _)) if *old > pri => {}
                        _ => { slot.insert(d.name.clone(), (pri, d.value.clone())); }
                    }
                }
            }
        }
    }
    // Inherit: an element sees its ancestors' customs, its own winning.
    let mut out: BTreeMap<(Handle, String), BTreeMap<String, Vec<Token>>> = BTreeMap::new();
    for media in &medias {
        for h in els {
            // The chain from the root down to this element.
            let mut chain = vec![*h];
            let mut cur = *h;
            while let Some(p) = dom.get(cur).and_then(|n| n.parent) { chain.push(p); cur = p }
            chain.reverse();
            let mut map: BTreeMap<String, Vec<Token>> = BTreeMap::new();
            for a in chain {
                // A media-conditioned custom overrides the base one here.
                for ctx in [String::new(), media.clone()] {
                    if let Some(w) = own.get(&(a, ctx)) {
                        for (k, (_, v)) in w { map.insert(k.clone(), v.clone()); }
                    }
                }
            }
            if !map.is_empty() { out.insert((*h, media.clone()), map); }
        }
    }
    out
}

fn has_light_dark(v: &[Token]) -> bool {
    v.iter().any(|t| matches!(&t.tok, Tok::Function(f) if f.eq_ignore_ascii_case("light-dark")))
}

/// Every `light-dark(L, D)` replaced by L (or by D when `dark`).
fn pick_light_dark(v: &[Token], dark: bool) -> Vec<Token> {
    let mut out = vec![];
    let mut i = 0;
    while i < v.len() {
        match &v[i].tok {
            Tok::Function(f) if f.eq_ignore_ascii_case("light-dark") => {
                let (mut d, mut j) = (1usize, i + 1);
                let mut args: Vec<Vec<Token>> = vec![vec![]];
                while j < v.len() && d > 0 {
                    match &v[j].tok {
                        Tok::LParen | Tok::Function(_) => { d += 1; args.last_mut().unwrap().push(v[j].clone()) }
                        Tok::RParen => { d -= 1; if d > 0 { args.last_mut().unwrap().push(v[j].clone()) } }
                        Tok::Comma if d == 1 => args.push(vec![]),
                        _ => args.last_mut().unwrap().push(v[j].clone()),
                    }
                    j += 1;
                }
                let pick = args.get(if dark { 1 } else { 0 }).cloned().unwrap_or_default();
                let pick: Vec<Token> = pick.into_iter().skip_while(|t| t.tok == Tok::Whitespace).collect();
                out.extend(pick_light_dark(&pick, dark));
                i = j;
            }
            _ => { out.push(v[i].clone()); i += 1 }
        }
    }
    out
}

/// Replace every `var(--x, fallback)` with its value, to a bounded depth.
/// An INVALID result (see `resolve_var`) is returned as the original tokens,
/// so the declaration is dropped AND reported downstream.
fn substitute(value: &[Token], custom: &BTreeMap<String, Vec<Token>>, depth: usize) -> Vec<Token> {
    resolve_var(value, custom, depth).unwrap_or_else(|| value.to_vec())
}

/// ★ CSS Variables 1, exactly. A custom property is treated as UNDEFINED
/// — so `var()` takes its fallback — when its value is `initial` (the
/// guaranteed-invalid value, §2.2) or when it is itself invalid at
/// computed-value time: it references an undefined property with no
/// fallback, or a cycle, or nests past the depth bound (§3). `None` means
/// the whole value is invalid.
///
/// The postcss light-dark polyfill is built on this: it sets
/// `--csstools-color-scheme--light: initial`, so a toggle property that
/// references it becomes invalid and `var(--toggle, <light>)` falls back to
/// the light colour. Substituting the word `initial` literally produced
/// `color: initial #a4cefe` — 15,972 dropped declarations in the corpus.
fn resolve_var(value: &[Token], custom: &BTreeMap<String, Vec<Token>>, depth: usize) -> Option<Vec<Token>> {
    if !value.iter().any(|t| matches!(&t.tok, Tok::Function(f) if f.eq_ignore_ascii_case("var"))) {
        return Some(value.to_vec());
    }
    if depth > 16 { return None }
    let mut out = vec![];
    let mut i = 0;
    while i < value.len() {
        match &value[i].tok {
            Tok::Function(f) if f.eq_ignore_ascii_case("var") => {
                let mut d = 1usize;
                let mut inner = vec![];
                let mut j = i + 1;
                while j < value.len() && d > 0 {
                    match &value[j].tok {
                        Tok::LParen | Tok::Function(_) => { d += 1; inner.push(value[j].clone()) }
                        Tok::RParen => { d -= 1; if d > 0 { inner.push(value[j].clone()) } }
                        _ => inner.push(value[j].clone()),
                    }
                    j += 1;
                }
                let sig: Vec<&Token> = inner.iter().filter(|t| t.tok != Tok::Whitespace).collect();
                let name = match sig.first().map(|t| &t.tok) { Some(Tok::Ident(n)) => n.clone(), _ => String::new() };
                let comma = inner.iter().position(|t| t.tok == Tok::Comma);
                // The referenced property's value, if it is defined AND valid.
                let defined = custom.get(&name)
                    .filter(|v| !css::write_tokens(v).trim().eq_ignore_ascii_case("initial"))
                    .and_then(|v| resolve_var(v, custom, depth + 1));
                match (defined, comma) {
                    (Some(v), _) => out.extend(v),
                    (None, Some(c)) => out.extend(resolve_var(&inner[c + 1..], custom, depth + 1)?),
                    (None, None) => return None,
                }
                i = j;
            }
            _ => { out.push(value[i].clone()); i += 1 }
        }
    }
    Some(out)
}

/// A declaration as longhands the profile admits, or nothing.
fn to_longhands(name: &str, value: &[Token], inputs: &Inputs, report: &mut Report) -> Vec<(String, String)> {
    if name.starts_with("--") { return vec![] }
    let text = css::write_tokens(value).trim().to_string();
    if text.is_empty() { return vec![] }
    // Already a longhand the profile knows?
    // `grid-area: <name>` is a name, not four lines; keep it whole.
    if name == "grid-area" && text.split(['/', ' ']).filter(|x| !x.trim().is_empty()).count() == 1
        && !text.trim().chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return vec![("grid-area-name".to_string(), text)];
    }
    if name == "grid-template-areas" { return vec![(name.to_string(), text)] }
    // ★ `float` is not in the profile, but a ROW of same-direction floats is
    // the pre-flexbox idiom for a horizontal strip, and block-stacking it is
    // simply wrong: GitHub's Watch/Fork/Star buttons came out one per line.
    // Carried as a marker and decided on the DOM, where the siblings are
    // visible (see the float row repair). `none` is carried too, or a later
    // `float: none` could not cancel an earlier `float: left`.
    // ★ `box-sizing` is not in the profile — every box there is border-box —
    // but the AUTHOR's content-box sizes must still come out the size the
    // author meant. Carried as a marker and compiled below (see the
    // box-sizing pass), never dropped: dropping it made every padded
    // content-box box narrower by its padding, silently, on most pages.
    // `-webkit-box-sizing` is still an ALIAS in Chrome and Safari: a page
    // that sets only the prefixed form means it.
    if (name == "box-sizing" || name == "-webkit-box-sizing") && matches!(text.as_str(), "content-box" | "border-box" | "inherit" | "initial" | "unset" | "revert") {
        return vec![("box-sizing-marker".to_string(), text)];
    }
    if name == "float" && matches!(text.as_str(), "left" | "right" | "none") {
        return vec![("float-marker".to_string(), text)];
    }
    let pairs: Vec<(String, String)> = if navigator_style::values::grammar(name).is_some() {
        vec![(name.to_string(), text)]
    } else {
        match expand::expand(name, value) {
            Some(p) => p.into_iter().filter(|(p, _)| navigator_style::values::grammar(p).is_some()).collect(),
            None => { report.drop(format!("property `{name}`")); return vec![] }
        }
    };
    // ★ Check the output against the profile's own grammar, repair what has
    // an honest mapping, and drop the rest WITH ITS VALUE. Nothing the
    // renderer can refuse leaves this function.
    pairs.into_iter().filter_map(|(p, v)| {
        // ★ `display: contents` is not a value the profile has — it is a
        // STRUCTURAL instruction: generate no box, and let the children take
        // this element's place in its parent. The normalizer can carry that
        // out literally (see `splice_contents`), so it is kept here as a
        // marker rather than dropped. 1510 elements in a 29-document corpus.
        if p == "display" && v == "contents" { return Some((p, v)) }
        // ★ NAMED GRID AREAS. The profile places items by NUMBER, and modern
        // layouts name them: `grid-template-areas` on the container plus
        // `grid-area: toolbar` on each child. The mapping from a name to a
        // rectangle of lines is static, so the normalizer can do it — but it
        // needs both halves, so they are carried here as markers and resolved
        // once the whole cascade is known. Without it every child lands in
        // the same cell: rustdoc's breadcrumb rendered on top of its search
        // box.
        if p == "grid-template-areas" || p == "grid-area-name" || p == "float-marker" || p == "box-sizing-marker" { return Some((p, v)) }
        // ★ An image the input has not measured cannot be painted (§3.13),
        // and the renderer says so. The normalizer must decide it HERE, the
        // same way it decides for an `<img>`, or it ships a document that
        // refuses.
        if p == "background-image" {
            if let Some(url) = v.strip_prefix("url(").and_then(|x| x.strip_suffix(')')) {
                let url = url.trim().trim_matches(['"', '\'']);
                // Tiling needs both natural dimensions.
                if !inputs.images.get(url).is_some_and(|n| n.width.is_some() && n.height.is_some()) {
                    report.drop("background image without a declared intrinsic size");
                    return Some((p, "none".to_string()));
                }
            }
        }
        if v.contains("var(") {
            report.drop("unresolved custom property (a cycle, or deeper than the limit)");
            return None;
        }
        if value::admits(&p, &v) { return Some((p, v)) }
        match value::repair(&p, &v) {
            Some(fixed) => Some((p, fixed)),
            None => { report.drop(format!("value `{p}: {v}`")); None }
        }
    }).collect()
}

/// A media query the profile admits, normalized to its own spelling — or
/// `None`, in which case the rules inside it are dropped.
fn admitted_media(q: &str) -> Option<String> {
    let q = q.trim().to_ascii_lowercase();
    // ★ A comma LIST is an OR, and the renderer evaluates lists. Each member
    // is admitted on its own: one that can never match on a screen (`print`)
    // simply leaves the OR, and one that always matches makes the whole
    // query unconditional. Refusing every list dropped ~7,800 declarations —
    // `@media screen, print { … }` is how a stylesheet says "everywhere".
    let members = depth0_split(&q, ",");
    if members.len() > 1 {
        let admitted: Vec<String> = members.iter().filter_map(|m| admitted_media(m)).collect();
        if admitted.is_empty() { return None }
        if admitted.iter().any(|m| m.is_empty()) { return Some(String::new()) }
        return Some(admitted.join(", "));
    }
    // `screen`, `all` and a bare feature query are fine; `print` is not.
    if q.contains("print") || q.contains("speech") { return None }
    // ★ A query is media types and features joined by `and`, and each part is
    // admitted ON ITS OWN. Stripping one `screen and ` prefix and then reading
    // the rest as ONE feature dropped every compound breakpoint —
    // `(min-width: …) and (max-width: …)` — and every doubled prefix a
    // concatenated stylesheet leaves behind (`screen and all and (…)`).
    let mut feats: Vec<String> = vec![];
    for part in depth0_split(&q, " and ") {
        let part = part.trim();
        let part = part.strip_prefix("only ").unwrap_or(part).trim();
        if part == "screen" || part == "all" || part.is_empty() { continue }
        let f = admitted_feature(part)?;
        if !f.is_empty() { feats.push(f) }
    }
    Some(feats.join(" and "))
}

/// A single non-negative length the profile's media parser reads: a number
/// and one of `px`, `em`, `rem`.
fn plain_length(v: &str) -> bool {
    let v = v.trim();
    let unit_at = v.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(v.len());
    let (num, unit) = v.split_at(unit_at);
    matches!(unit, "px" | "em" | "rem") && num.parse::<f64>().is_ok_and(|n| n >= 0.0)
}

/// Split at `sep` where no parenthesis is open.
fn depth0_split<'a>(q: &'a str, sep: &str) -> Vec<&'a str> {
    let (mut out, mut depth, mut last, b) = (vec![], 0i32, 0usize, q.as_bytes());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            // ★ Compare BYTES. Slicing `q[i..]` panicked on the first real
            // query holding a multi-byte character (`screen\u{FFFD}…`, from
            // a mis-decoded sheet); an ASCII separator can only match at a
            // character boundary, so the slices below stay valid.
            _ if depth == 0 && b[i..].starts_with(sep.as_bytes()) => { out.push(&q[last..i]); i += sep.len(); last = i; continue }
            _ => {}
        }
        i += 1;
    }
    out.push(&q[last..]);
    out
}

/// One parenthesized media feature, in the form the profile admits.
fn admitted_feature(q: &str) -> Option<String> {
    if !q.starts_with('(') || !q.ends_with(')') { return None }
    // ★ RANGE SYNTAX (Media Queries 4): `(width <= 1044px)` is how modern
    // sheets are written, and the profile's parser only knows `max-width`.
    // Dropping it drops every rule inside — MDN hides its mobile menu in
    // `@media (width <= 1044px)`, so the menu rendered, at one character per
    // line, on every page.
    if let Some(range) = from_range(q) { return Some(range) }
    // ★ EXACTLY ONE paren each side. `trim_matches` is greedy and ate the
    // closing paren of a `calc(…)` value — `(max-width:calc(1120px - 1px))`
    // became `max-width:calc(1120px - 1px`, failed to fold, and 18 of
    // Wikipedia's narrow-layout blocks were dropped: the ones that hide the
    // sidebar contents below 1120 px. The same greedy trim had already been
    // fixed once, in the range path.
    let inner = &q[1..q.len() - 1];
    let name = inner.split(':').next().unwrap_or("").trim();
    match name {
        "min-width" | "max-width" | "min-height" | "max-height" => {
            // ★ A media feature takes a plain length; real sheets write
            // `calc(640px - 1px)`. It is constant-foldable, so fold it —
            // dropping the query would drop a whole responsive breakpoint.
            let value = inner.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
            match fold_px(value) {
                Some(px) if px >= 0.0 => Some(format!("({name}: {px}px)")),
                Some(_) => None,
                // ★ Only a plain non-negative length passes through as written.
                // "Anything that is not a calc()" used to pass: ScienceDirect
                // ships `(max-width: getbreakpointdownvalue(48em))`, an
                // unexpanded Sass function, and it reached the renderer —
                // which refused the whole document. A browser treats such a
                // query as never matching; dropping it (reported) is the same.
                None if plain_length(value) => Some(format!("({name}: {value})")),
                None => None,
            }
        }
        // ★ The BOOLEAN form `(prefers-reduced-motion)` means "anything but
        // the feature's 'none' value" (Media Queries 4 §2.4.4); the profile's
        // parser takes only `name: value`, and Figma's sheet was refused for
        // it. Rewritten to the value it stands for — or, where every value is
        // true (a colour scheme, an orientation), to no condition at all.
        "prefers-reduced-motion" if !inner.contains(':') => Some("(prefers-reduced-motion: reduce)".into()),
        "prefers-color-scheme" | "orientation" if !inner.contains(':') => Some(String::new()),
        // ★ The VALUE is checked too, not only the name: an unknown value
        // makes the query never match in a browser, and here it would reach
        // the renderer and be refused. `prefers-color-scheme: no-preference`
        // was removed from the spec, and a real sheet still ships it.
        "prefers-color-scheme" | "prefers-reduced-motion" | "orientation" => {
            let v = inner.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
            let ok = match name {
                "prefers-color-scheme" => matches!(v, "light" | "dark"),
                "prefers-reduced-motion" => matches!(v, "no-preference" | "reduce"),
                _ => matches!(v, "portrait" | "landscape"),
            };
            ok.then(|| format!("({name}: {v})"))
        }
        _ => None,
    }
}

/// `(width <= 1044px)`, `(400px < height < 900px)` and the rest, rewritten
/// into the profile's `min-`/`max-` features.
///
/// A STRICT comparison is not the same as the profile's inclusive one, so
/// it is nudged by 0.02px — smaller than any device pixel, and honest about
/// which side of the boundary the rule falls on.
fn from_range(q: &str) -> Option<String> {
    // ★ Exactly ONE paren each side. `trim_end_matches(')')` is greedy and
    // ate the `calc(…)`'s own closing paren, which left the expression
    // unbalanced and the whole query unparsed.
    let t = q.trim();
    let inner = t.strip_prefix('(').unwrap_or(t);
    let inner = inner.strip_suffix(')').unwrap_or(inner).trim();
    if !inner.contains('<') && !inner.contains('>') { return None }
    // ★ Split on the operators at paren depth 0 — a `calc()` on either side
    // contains spaces and parentheses of its own, so splitting on whitespace
    // (which is what this did first) takes the query apart in the wrong
    // place and drops it.
    let (mut parts, mut ops, mut cur, mut depth) = (vec![], vec![], String::new(), 0usize);
    let ch: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < ch.len() {
        match ch[i] {
            '(' => { depth += 1; cur.push('(') }
            ')' => { depth = depth.saturating_sub(1); cur.push(')') }
            '<' | '>' | '=' if depth == 0 => {
                let mut op = ch[i].to_string();
                if ch.get(i + 1) == Some(&'=') { op.push('='); i += 1 }
                parts.push(cur.trim().to_string());
                ops.push(op);
                cur = String::new();
            }
            c => cur.push(c),
        }
        i += 1;
    }
    parts.push(cur.trim().to_string());

    let feature = |s: &str| matches!(s, "width" | "height");
    let eps = 0.02;
    match (parts.as_slice(), ops.as_slice()) {
        // `width <= 1044px`, or the same written backwards.
        ([a, b], [op]) => {
            let (f, n, op) = if feature(a) { (a.as_str(), len_px(b)?, op.clone()) }
                             else if feature(b) { (b.as_str(), len_px(a)?, flip(op)) }
                             else { return None };
            Some(match op.as_str() {
                "<=" => format!("(max-{f}: {n}px)"),
                "<" => format!("(max-{f}: {}px)", n - eps),
                ">=" => format!("(min-{f}: {n}px)"),
                ">" => format!("(min-{f}: {}px)", n + eps),
                "=" => format!("(min-{f}: {n}px) and (max-{f}: {n}px)"),
                _ => return None,
            })
        }
        // `400px <= width <= 900px`
        ([lo, f, hi], [op1, op2]) if feature(f) => {
            let (lo, hi) = (len_px(lo)?, len_px(hi)?);
            let min = match op1.as_str() { "<=" => lo, "<" => lo + eps, _ => return None };
            let max = match op2.as_str() { "<=" => hi, "<" => hi - eps, _ => return None };
            Some(format!("(min-{f}: {min}px) and (max-{f}: {max}px)"))
        }
        _ => None,
    }
}

/// `1044px`, `50rem`, or a `calc()` over them.
fn len_px(v: &str) -> Option<f64> {
    let v = v.trim();
    if v.starts_with("calc(") { return fold_px(v) }
    if let Some(n) = v.strip_suffix("px") { return n.trim().parse().ok() }
    if let Some(n) = v.strip_suffix("rem") { return n.trim().parse::<f64>().ok().map(|x| x * 16.0) }
    None
}

/// `a < b` read from the other side is `b > a`.
fn flip(op: &str) -> String {
    match op { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", other => other }.to_string()
}

/// `calc(…)` in a MEDIA FEATURE, folded to px.
///
/// ★ A media query is evaluated against the viewport, and the only units it
/// can hold that this can answer are ABSOLUTE ones: `px`, and the
/// root-relative `rem`, which is 16px because the profile's root font size
/// is (§3.6). A percentage or a viewport unit would make the answer depend
/// on what the query is deciding, so those return `None` and the query is
/// dropped rather than guessed.
///
/// MDN switches to its mobile layout at
/// `(width < calc(1rem * 2 + (15rem + 2rem) * 2 + 31rem))` — 1072px. Without
/// an evaluator that query is dropped, and an 800px viewport renders the
/// DESKTOP layout: two sidebars and a 48rem column in 800px.
fn fold_px(v: &str) -> Option<f64> {
    let inner = v.trim().strip_prefix("calc(")?.strip_suffix(')')?;
    let toks = calc_tokens(inner)?;
    let mut pos = 0usize;
    let out = calc_sum(&toks, &mut pos)?;
    (pos == toks.len()).then_some(out)
}

fn calc_tokens(s: &str) -> Option<Vec<String>> {
    let mut out = vec![];
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' | ')' | '+' | '*' | '/' => {
                if !cur.trim().is_empty() { out.push(cur.trim().to_string()) }
                cur.clear();
                out.push(c.to_string());
            }
            // `-` is a subtraction only when it stands alone; `-5px` is a
            // number, and CSS requires the spaces that make this decidable.
            '-' if cur.trim().is_empty() => { out.push("-".into()); cur.clear() }
            ' ' => { if !cur.trim().is_empty() { out.push(cur.trim().to_string()) } cur.clear() }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() { out.push(cur.trim().to_string()) }
    Some(out)
}

fn calc_sum(t: &[String], p: &mut usize) -> Option<f64> {
    let mut v = calc_prod(t, p)?;
    while let Some(op) = t.get(*p) {
        match op.as_str() {
            "+" => { *p += 1; v += calc_prod(t, p)? }
            "-" => { *p += 1; v -= calc_prod(t, p)? }
            _ => break,
        }
    }
    Some(v)
}

fn calc_prod(t: &[String], p: &mut usize) -> Option<f64> {
    let mut v = calc_atom(t, p)?;
    while let Some(op) = t.get(*p) {
        match op.as_str() {
            "*" => { *p += 1; v *= calc_atom(t, p)? }
            "/" => { *p += 1; let d = calc_atom(t, p)?; if d == 0.0 { return None } v /= d }
            _ => break,
        }
    }
    Some(v)
}

fn calc_atom(t: &[String], p: &mut usize) -> Option<f64> {
    let tok = t.get(*p)?.clone();
    *p += 1;
    if tok == "(" {
        let v = calc_sum(t, p)?;
        if t.get(*p)? != ")" { return None }
        *p += 1;
        return Some(v);
    }
    if tok == "-" { return Some(-calc_atom(t, p)?) }
    if let Some(n) = tok.strip_suffix("px") { return n.parse().ok() }
    if let Some(n) = tok.strip_suffix("rem") { return n.parse::<f64>().ok().map(|v| v * 16.0) }
    tok.parse().ok()
}

#[cfg(test)]
mod media_tests {
    /// Media Queries 4 range syntax, including `calc()` over absolute units.
    #[test]
    fn range_and_calc() {
        let m = |q| super::admitted_media(q);
        assert_eq!(m("(width <= 1044px)").as_deref(), Some("(max-width: 1044px)"));
        assert_eq!(m("(width >= calc(50rem))").as_deref(), Some("(min-width: 800px)"));
        assert_eq!(m("(400px <= width <= 900px)").as_deref(), Some("(min-width: 400px) and (max-width: 900px)"));
        // ★ The one that mattered: MDN's mobile switch.
        assert_eq!(m("(width < calc(1rem * 2 + (15rem + 2rem) * 2 + 31rem))").as_deref(),
                   Some("(max-width: 1071.98px)"), "strict `<`, nudged off the boundary");
        // A viewport-relative or percentage bound cannot be answered here.
        assert_eq!(m("(width < calc(50% + 10px))"), None);
        assert_eq!(m("(min-resolution: 2dppx)"), None);
    }

    /// ★ Wikipedia's own shapes. The first had its `calc()`'s closing paren
    /// eaten by a greedy trim; the others were read as ONE feature.
    #[test]
    fn compound_queries_and_calc_values() {
        let m = |q| super::admitted_media(q);
        assert_eq!(m("screen and (max-width:calc(1120px - 1px))").as_deref(), Some("(max-width: 1119px)"));
        assert_eq!(m("screen and (min-width:calc(640px - 1px)) and (max-width:calc(1680px - 1px))").as_deref(),
                   Some("(min-width: 639px) and (max-width: 1679px)"));
        // Doubled prefixes, from concatenated sheets.
        assert_eq!(m("screen and all and (max-width:calc(640px - 1px))").as_deref(), Some("(max-width: 639px)"));
        assert_eq!(m("screen and screen and (prefers-color-scheme:dark)").as_deref(), Some("(prefers-color-scheme: dark)"));
        // Counterweights: one refused part refuses the whole query; so do
        // print, a comma list, and a negation.
        assert_eq!(m("screen and (max-width: 600px) and (hover: hover)"), None);
        assert_eq!(m("print and (max-width: 600px)"), None);
        // A LIST is an OR: members admitted one by one.
        assert_eq!(m("screen, print").as_deref(), Some(""), "`screen` alone matches: unconditional");
        assert_eq!(m("print, (max-width: 600px)").as_deref(), Some("(max-width: 600px)"), "print leaves the OR");
        assert_eq!(m("(max-width: 600px), (orientation: portrait)").as_deref(), Some("(max-width: 600px), (orientation: portrait)"));
        assert_eq!(m("print, speech"), None, "nothing left: refused");
        assert_eq!(m("only print, only all and (prefers-color-scheme: no-preference)"), None, "an obsolete value never matches");
        assert_eq!(m("(prefers-color-scheme: light), print").as_deref(), Some("(prefers-color-scheme: light)"));
        assert_eq!(m("not all and (max-width: 600px)"), None);
        // ★ From the 96-document run: an unexpanded Sass function and a
        // negative bound are dropped, never passed through to be refused…
        assert_eq!(m("(max-width: getbreakpointdownvalue(48em))"), None);
        assert_eq!(m("(max-width: calc(0px - 1px))"), None);
        assert_eq!(m("(min-width: 48em)").as_deref(), Some("(min-width: 48em)"), "a plain em length passes");
        // …and the boolean form becomes the value it stands for.
        assert_eq!(m("(prefers-reduced-motion)").as_deref(), Some("(prefers-reduced-motion: reduce)"));
        assert_eq!(m("screen and (orientation)").as_deref(), Some(""), "always true: no condition");
        assert_eq!(m("(prefers-color-scheme) and (max-width: 600px)").as_deref(), Some("(max-width: 600px)"));
        // A mis-decoded sheet: refused, never a panic.
        assert_eq!(m("screen\u{FFFD}\u{FFFD} and (max-width: 600px)"), None);
        assert_eq!(m("screen and (max-width: 600px) and (\u{e9}t\u{e9}: 1)"), None);
    }
}
