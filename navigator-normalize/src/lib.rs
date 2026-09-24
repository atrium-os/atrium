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
    pub images: BTreeMap<String, (u32, u32)>,
    /// `href` → the `media` attribute of the `<link>` that named it. A sheet
    /// fetched under a condition must be APPLIED under it (§3.11), or a
    /// dark-mode stylesheet lands on every reader.
    pub stylesheet_media: BTreeMap<String, String>,
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
                            let alt = substitute(&d.value, cs, 0);
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
            let value = substitute(&d.value, &scope, 0);
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
        for a in ["align", "valign", "bgcolor", "border", "cellpadding", "cellspacing", "width", "height", "hspace", "vspace"] {
            if dom.tag(*h) == Some("img") && (a == "width" || a == "height") { continue }
            if dom.attr(*h, a).is_some() { dom.remove_attr(*h, a); report.drop(format!("presentational attribute `{a}`")); }
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
            Some((w, ht)) => { dom.set_attr(h, "width", &w.to_string()); dom.set_attr(h, "height", &ht.to_string()); }
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

/// Replace every `var(--x, fallback)` with its value, to a bounded depth.
fn substitute(value: &[Token], custom: &BTreeMap<String, Vec<Token>>, depth: usize) -> Vec<Token> {
    if depth > 16 || !value.iter().any(|t| matches!(&t.tok, Tok::Function(f) if f.eq_ignore_ascii_case("var"))) {
        return value.to_vec();
    }
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
                let fallback: Vec<Token> = comma.map(|c| inner[c + 1..].to_vec()).unwrap_or_default();
                match custom.get(&name) {
                    Some(v) => out.extend(substitute(v, custom, depth + 1)),
                    // ★ No definition AND no fallback is not "the empty
                    // value": it makes the declaration invalid. Substituting
                    // nothing left a declaration that had simply vanished,
                    // with no diagnostic — the failure read as a rule nobody
                    // wrote. Keep the `var()` so the drop is REPORTED.
                    None if comma.is_none() => out.extend(value[i..j].iter().cloned()),
                    None => out.extend(substitute(&fallback, custom, depth + 1)),
                }
                i = j;
            }
            _ => { out.push(value[i].clone()); i += 1 }
        }
    }
    out
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
        if p == "grid-template-areas" || p == "grid-area-name" || p == "float-marker" { return Some((p, v)) }
        // ★ An image the input has not measured cannot be painted (§3.13),
        // and the renderer says so. The normalizer must decide it HERE, the
        // same way it decides for an `<img>`, or it ships a document that
        // refuses.
        if p == "background-image" {
            if let Some(url) = v.strip_prefix("url(").and_then(|x| x.strip_suffix(')')) {
                let url = url.trim().trim_matches(['"', '\'']);
                if !inputs.images.contains_key(url) {
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
    // `screen`, `all` and a bare feature query are fine; `print` is not.
    if q.contains("print") || q.contains("speech") { return None }
    // A comma list is an OR the profile's queries cannot say; `not` negates.
    if depth0_split(&q, ",").len() > 1 { return None }
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
        feats.push(admitted_feature(part)?);
    }
    Some(feats.join(" and "))
}

/// Split at `sep` where no parenthesis is open.
fn depth0_split<'a>(q: &'a str, sep: &str) -> Vec<&'a str> {
    let (mut out, mut depth, mut last, b) = (vec![], 0i32, 0usize, q.as_bytes());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && q[i..].starts_with(sep) => { out.push(&q[last..i]); i += sep.len(); last = i; continue }
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
                Some(px) => Some(format!("({name}: {px}px)")),
                None if value.contains("calc(") => None,
                None => Some(q.to_string()),
            }
        }
        "prefers-color-scheme" | "prefers-reduced-motion" | "orientation" => Some(q.to_string()),
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
        assert_eq!(m("screen and screen and (prefers-color-scheme:dark)").as_deref(), Some("(prefers-color-scheme:dark)"));
        // Counterweights: one refused part refuses the whole query; so do
        // print, a comma list, and a negation.
        assert_eq!(m("screen and (max-width: 600px) and (hover: hover)"), None);
        assert_eq!(m("print and (max-width: 600px)"), None);
        assert_eq!(m("(max-width: 600px), (orientation: portrait)"), None);
        assert_eq!(m("not all and (max-width: 600px)"), None);
    }
}
