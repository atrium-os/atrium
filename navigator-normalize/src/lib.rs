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
pub mod phases;
mod borders;
mod collapse;
pub mod expand;
pub mod recording;
pub mod sel;
pub mod svg_inline;
pub mod value;

use navigator_dom::{Dom, Handle, Kind};
use navigator_style::token::{tokenize, Tok, Token};
use std::collections::BTreeMap;
use std::rc::Rc;

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
    /// What one document may cost. See `Budget`.
    pub budget: Budget,
    /// ★ EXTERNAL SVG SPRITE FILES (2026-11-06): the file part of an inline
    /// `<use href="file.svg#icon">`, as the document writes it → the file's
    /// text, fetched by the converter. `svg_inline` copies the named symbol
    /// in, so the icon is a self-contained SVG and the strict profile still
    /// sees only local references.
    pub sprites: BTreeMap<String, String>,
}

/// At most this many sprite files per document, and this many bytes each
/// (the SVG profile's own ceiling on one SVG).
pub const MAX_SPRITE_FILES: usize = 32;
pub const MAX_SPRITE_BYTES: usize = 1 << 20;

/// ★ WORK BUDGETS. The normalizer reads hostile input, and its worst cases
/// are not exotic: GitHub's custom-property cascade once took more than
/// 14 GB. A jail's memory and CPU caps are the outer wall, but a kill loses
/// the whole page and says nothing. These budgets are the inner one: when a
/// document goes over, the normalizer stops doing THAT work, drops what it
/// could not finish, and says so in the report, and the rest of the page is
/// still normalized.
///
/// Every budget counts WORK, never time. The output is content-addressed, so
/// the same input must give the same document on a fast machine and a slow
/// one; a time limit would make the result depend on the hardware.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Total bytes of CSS read, across `<style>`, linked sheets and imports.
    /// Sheets past it are not read.
    pub css_bytes: usize,
    /// Elements in the document. Past it the document is passed through
    /// unstyled: its text survives, its layout does not.
    pub elements: usize,
    /// Selector-matching steps (one compound tested against one element).
    /// Rules past it are dropped.
    pub match_steps: u64,
    /// Tokens one `var()` substitution may produce. A value past it is
    /// invalid, as a cycle is (the "billion laughs" of custom properties).
    pub var_tokens: usize,
    /// Tokens produced by all `var()` substitution in the document.
    pub var_steps: u64,
}

impl Default for Budget {
    /// Sized from the corpus (2026-11-03): the most expensive page in each
    /// unit uses under a quarter of it. CSS 7.0 MB (edX), elements 48k (a
    /// long Wikipedia table), matching 65M steps (GitHub), var() 7.5M
    /// tokens (Notion).
    fn default() -> Self {
        Budget { css_bytes: 32 << 20, elements: 200_000, match_steps: 500_000_000, var_tokens: 8192, var_steps: 64_000_000 }
    }
}

thread_local! {
    /// (tokens left in the document's var() budget, per-value cap). Set at
    /// the start of each `normalize`, so `resolve_var` keeps its signature.
    static VAR_BUDGET: std::cell::Cell<(u64, usize)> = const { std::cell::Cell::new((u64::MAX, usize::MAX)) };
    static VAR_EXHAUSTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    /// What the document cost against its `Budget`.
    pub spent: Spent,
}

/// Work done, in the units `Budget` counts.
#[derive(Debug, Default, Clone, Copy)]
pub struct Spent { pub css_bytes: usize, pub elements: usize, pub match_steps: u64, pub var_steps: u64 }

impl Report {
    fn drop(&mut self, why: impl Into<String>) { *self.dropped.entry(why.into()).or_default() += 1 }
    fn drop_n(&mut self, why: impl Into<String>, n: usize) { *self.dropped.entry(why.into()).or_default() += n }
}

/// A declaration with no `var()`, expanded once per rule: its longhands
/// (and the dark side of a `light-dark()`, in the base context), and what
/// expanding it reported.
#[derive(Clone)]
struct Expanded { dark: Option<Vec<(String, String)>>, light: Vec<(String, String)>, drops: Vec<(String, usize)> }

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
pub(crate) type Priority = (bool, usize, (u32, u32, u32), usize);

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
    // ★ Tables first: a float that is a table (an infobox) has no height
    // until its columns are measured — fitted before them, ru Wikipedia's
    // infobox was 2 px tall and every paragraph went below it. Column
    // measures do not depend on the width a table is given, so moving
    // content afterwards leaves them true.
    let (measured, n) = navigator_render::html::premeasure_tables(&doc, fonts, env);
    phases::lap("tables");
    let (measured, fit) = fit_floats(&measured, fonts, env);
    phases::lap("floats");
    if fit.cut > fit.narrow { report.drop_n("content below a float moved out of its row (by layout)", fit.cut - fit.narrow) }
    if fit.narrow > 0 { report.drop_n("float too wide for content beside it (content moved below)", fit.narrow) }
    if n > 0 { report.columns_measured = n }
    (measured, report)
}

pub fn normalize(html: &str, inputs: &Inputs) -> (String, Report) {
    phases::lap("before");
    let mut dom = navigator_dom::parse(html);
    // ★ Inline SVGs the converter made standalone (src/svg_inline.rs): the
    // same function, on the same untouched document and stylesheets, so the
    // bytes — and their address — are the converter's. Only a recording that
    // names a `blake3:` image can have any, so every other document skips it.
    let inline_svgs: Vec<(Handle, String, String)> = if inputs.images.keys().any(|k| k.starts_with("blake3:")) {
        let outers = svg_inline::outer_svgs(&dom);
        let texts = svg_inline::standalone_in(&dom, &outers, inputs);
        outers.into_iter().zip(texts)
            .map(|(h, t)| (h, format!("blake3:{}", blake3::hash(t.as_bytes()).to_hex()), t))
            .filter(|(_, a, _)| inputs.images.contains_key(a)).collect()
    } else { vec![] };
    phases::lap("parse");
    let mut report = Report::default();
    let budget = inputs.budget;
    VAR_BUDGET.with(|b| b.set((budget.var_steps, budget.var_tokens)));
    VAR_EXHAUSTED.with(|b| b.set(false));
    let mut css_left = budget.css_bytes;
    let mut take_css = |text: &str, report: &mut Report| -> bool {
        if text.len() > css_left { report.drop("stylesheet past the CSS budget (not read)"); return false }
        css_left -= text.len();
        true
    };

    // 1. Collect CSS: every <style>, every supplied <link>, in document order.
    let mut sheet = css::Sheet::default();
    let mut order = 0usize;
    for h in dom.by_tag_anywhere("style") {
        let text = expand_imports(&dom.text_content(h), "", inputs, 0, &mut report);
        if !take_css(&text, &mut report) { continue }
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
                if !take_css(&text, &mut report) { continue }
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
    drop(take_css);
    let css_left_final = css_left;
    for (what, _) in &sheet.dropped { report.drop(format!("@{what} not representable")) }
    report.rules_in = sheet.rules.len();

    phases::lap("css");
    // 2. The cascade, resolved by this program rather than by the renderer.
    let els: Vec<Handle> = (0..dom.nodes.len() as Handle)
        .filter(|h| matches!(dom.get(*h).map(|n| &n.kind), Some(Kind::Element(_)))).collect();
    let matcher_dom = navigator_dom::parse(html); // a stable copy to match against
    let m = sel::Matcher::new(&matcher_dom).with_budget(budget.match_steps);
    let index = sel::RuleIndex::new(&matcher_dom, &els);
    phases::lap("index");
    // Past the element budget the cascade is not attempted: every rule
    // would be matched against every element. The text is kept.
    if els.len() > budget.elements {
        report.drop_n("rule not applied: document past the element budget", sheet.rules.len());
        sheet.rules.clear();
    }

    // ★ Custom properties go through the CASCADE, per element and per media
    // context, and then inherit. Taking the last `--x` in the file instead —
    // which is the obvious shortcut — picks up whatever a
    // `@media (prefers-color-scheme: dark)` block set, and renders every
    // page in its dark palette no matter the environment. That is what the
    // first version did, and the render is what showed it.
    let custom = resolve_customs(&sheet, &index, &m, &dom);
    phases::lap("customs");

    // key: (element, state, media) -> property -> (priority, value)
    type Key = (Handle, String, String);
    let mut resolved: BTreeMap<Key, BTreeMap<String, (Priority, String)>> = BTreeMap::new();

    // Elements drawn through a mask (see below).
    let mut masked: std::collections::BTreeSet<Handle> = Default::default();

    // Media contexts that define custom properties: a base rule using
    // `var()` must be re-resolved in each of them.
    let var_contexts: Vec<String> = custom.keys().map(|(_, m)| m.clone())
        .filter(|m| !m.is_empty()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();

    for (ri, rule) in sheet.rules.iter().enumerate() {
        if m.exhausted() {
            report.drop_n("rule not applied: selector-matching budget spent", sheet.rules.len() - ri);
            break;
        }
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
        // ★ A declaration with no `var()` expands to the same longhands on
        // every element the rule matches, so it is expanded once per rule
        // (cut B, legacy-web §5.4.5.7): `to_longhands` was run per element
        // per declaration. Its report entries are kept with it and counted
        // again on every use, so the report is what it was.
        let mut expanded: Vec<Option<Expanded>> = vec![None; rule.decls.len()];
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
            for h in index.candidates(c) {
                if !m.matches(*h, c) { continue }
                let key = (*h, state.clone(), media.clone());
                let slot = resolved.entry(key).or_default();
                let empty: Rc<Customs> = Rc::default();
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
                for (di, d) in rule.decls.iter().enumerate() {
                    if !uses_var(&d.value) {
                        let e = expanded[di].get_or_insert_with(|| {
                            let mut scratch = Report::default();
                            let dark = (media.is_empty() && has_light_dark(&d.value))
                                .then(|| to_longhands(&d.name, &pick_light_dark(&d.value, true), inputs, &mut scratch));
                            let light = to_longhands(&d.name, &pick_light_dark(&d.value, false), inputs, &mut scratch);
                            Expanded { dark, light, drops: scratch.dropped.into_iter().collect() }
                        });
                        for (why, n) in &e.drops { report.drop_n(why.clone(), *n) }
                        if let Some(dark) = &e.dark {
                            let mut block = BTreeMap::new();
                            for (prop, v) in dark {
                                block.insert(prop.clone(), ((d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order), v.clone()));
                            }
                            if !block.is_empty() { pending.push(("(prefers-color-scheme: dark)".to_string(), block)) }
                        }
                        let pri: Priority = (d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order);
                        for (prop, v) in &e.light {
                            match slot.get(prop) {
                                Some((old, _)) if *old > pri => {}
                                _ => { slot.insert(prop.clone(), (pri, v.clone())); }
                            }
                        }
                        continue;
                    }
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

    phases::lap("cascade");
    // 4. Inline `style=` attributes: higher than any rule, as CSS says, and
    //    the single biggest refusal in the corpus (21 of 29 documents).
    for h in &els {
        let Some(style) = dom.attr(*h, "style").map(str::to_string) else { continue };
        let decls = css::declarations(&tokenize(&style));
        let empty: Rc<Customs> = Rc::default();
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

    clip_pass(&mut resolved, &mut report);
    icon_font_pass(&mut resolved, &mut report);

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
            // ★ `dir` (HTML §15.3.1, the UA sheet's `[dir=rtl i] { direction:
            // rtl }`): below every author rule, like the hints. `dir=auto`,
            // and a `<bdi>` without one, take the direction of their first
            // strong character (HTML's "auto directionality"), else ltr.
            let dir = a("dir").map(|v| v.trim().to_ascii_lowercase()).or_else(|| (t == "bdi").then(|| "auto".to_string()));
            match dir.as_deref() {
                Some(d @ ("ltr" | "rtl")) => hint("dir", "direction", Some(d.to_string())),
                Some("auto") => hint("dir", "direction", Some(if first_strong_rtl(&dom, *h).unwrap_or(false) { "rtl" } else { "ltr" }.to_string())),
                _ => {}
            }
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

    // ★ FLEX REVERSAL, compiled into the DOM (before the direction pass,
    // which must see `left` / `right` as written).
    reverse_pass(&mut resolved, &mut dom, &mut report);

    // ★ DIRECTION: logical sides, physical alignment, the UA's start-side
    // indents — all resolved against each element's own direction.
    let rtl = direction_pass(&mut resolved, &dom, &els, &mut report);

    // ★ SELF-ALIGNMENT: `flex-*`, `self-*`, `left` / `right` and `normal`
    // compiled to the keywords the grammar admits (needs the directions).
    alignment_pass(&mut resolved, &dom, &els, &rtl, inputs, &mut report);

    // ★ TABLE CAPTIONS (see `caption_pass`).
    caption_pass(&mut dom, &mut resolved, &mut report);

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
        if let Some((_, v)) = props.get("float-marker") {
            let r = rtl.get(h).copied().unwrap_or(false);
            let v = match v.as_str() { "inline-start" => if r { "right" } else { "left" }, "inline-end" => if r { "left" } else { "right" }, v => v };
            float_of.insert(*h, v.to_string());
        }
    }
    let mut rows = 0usize;

    let parents: Vec<Handle> = float_of.keys()
        .filter_map(|h| dom.get(*h).and_then(|n| n.parent))
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let mut strip: Vec<Handle> = vec![];
    let mut lone_floats: Vec<(Handle, String)> = vec![];
    let mut strip_parents: Vec<(Handle, Vec<Handle>)> = vec![];
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
        // In a right-to-left parent the strip is the RIGHT floats: they run
        // from the right edge, which is where an rtl inline-block row starts.
        let start = if rtl.get(&p).copied().unwrap_or(false) { "right" } else { "left" };
        let all_left = dirs.len() >= 2 && dirs.iter().all(|d| *d == start);
        let has_text = dom.children_of(p).iter().any(|c|
            dom.get(*c).is_some_and(|n| matches!(n.kind, Kind::Text(ref t) if !t.trim().is_empty())));
        if all_left && !has_text { rows += 1; strip_parents.push((p, inflow.clone())); strip.extend(inflow) } else {
            for (k, d) in inflow.iter().zip(&dirs) { if *d != "none" { lone_floats.push((*k, d.to_string())) } }
        }
    }
    // ★ The WHITE SPACE between floats generates nothing (CSS 2.1 §9.5.1:
    // it collapses, and a line holding only collapsible space is not
    // there), but between inline-blocks it is a space as wide as a word gap:
    // two `width: 50%` floats became two lines, and every WPT reference
    // drawing a grid of floated cells came out one column wide. So the
    // strip loses its whitespace-only text — unless the space is preserved
    // (`white-space: pre…`), or some context lays the children out inline
    // anyway, where the space is real.
    let mut stripped = 0usize;
    for (p, kids) in strip_parents {
        let mut cur = Some(p);
        let mut preserved = false;
        while let Some(x) = cur {
            match resolved.get(&(x, String::new(), String::new())).and_then(|pr| pr.get("white-space")).map(|(_, v)| v.as_str()) {
                Some("inherit") | None => {}
                Some(v) => { preserved = v.starts_with("pre") || v == "break-spaces"; break }
            }
            if matches!(dom.tag(x), Some("pre" | "textarea" | "listing" | "xmp")) { preserved = true; break }
            cur = dom.get(x).and_then(|n| n.parent).filter(|q| matches!(dom.get(*q).map(|n| &n.kind), Some(Kind::Element(_))));
        }
        let inline_elsewhere = resolved.iter().any(|((h, st, me), pr)| (!st.is_empty() || !me.is_empty()) && kids.contains(h)
            && pr.get("display").is_some_and(|(_, d)| d.starts_with("inline")));
        if preserved || inline_elsewhere { continue }
        let blank: Vec<Handle> = dom.children_of(p).into_iter()
            .filter(|c| matches!(dom.get(*c).map(|n| &n.kind), Some(Kind::Text(t)) if t.trim().is_empty())).collect();
        for c in blank { detach(&mut dom, c); stripped += 1 }
    }
    if stripped > 0 { report.drop_n("white space between floats removed (it renders nothing)", stripped) }
    for h in strip {
        let slot = resolved.entry((h, String::new(), String::new())).or_default();
        let keep = slot.get("display").map(|(_, v)| matches!(v.as_str(), "flex" | "grid" | "inline-flex" | "inline-grid" | "table")).unwrap_or(false);
        if !keep {
            let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
            slot.insert("display".to_string(), (pri, "inline-block".to_string()));
        }
    }
    if rows > 0 { report.drop_n("float row laid out as inline-block", rows) }
    // ★ FLOATS BESIDE CONTENT (profile §7a C, 2026-11-06). A float with
    // content after it is compiled into what the profile admits — a flex
    // ROW holding the float and a FLOW column with the content after it —
    // so the renderer stays float-free (G4). Which content goes beside is
    // decided twice: here, everything up to the next float or `clear`; then,
    // once the layout is known, only what starts above the float's bottom
    // (`normalize_and_measure`, `fit_floats`), the rest moved back below.
    // ★ VISUAL ORDER IS READING ORDER: the row's children are in the order
    // they are seen, start side first — a right float in a left-to-right
    // parent comes AFTER the content beside it, as a reversed row is
    // reversed in the document (§3.4).
    let clear_of: BTreeMap<Handle, String> = resolved.iter()
        .filter(|((_, st, me), _)| st.is_empty() && me.is_empty())
        .filter_map(|((h, _, _), pr)| pr.get("clear-marker").map(|(_, v)| (*h, v.clone())))
        .filter(|(_, v)| v != "none").collect();
    let (beside, alone) = compile_floats(&mut dom, &mut resolved, &lone_floats, &float_of, &clear_of, &rtl);
    let lone = alone;
    if beside > 0 { report.drop_n("float beside content compiled to a flex row", beside) }
    if lone > 0 { report.drop_n("property `float` (no content before the next float or clear; dropped)", lone) }
    for (_, props) in resolved.iter_mut() { props.remove("float-marker"); props.remove("clear-marker"); }

    // ★ COLLAPSED TABLE BORDERS, compiled into separated ones (`borders`).
    // Before box-sizing, which counts the borders it decides.
    borders::compile(&mut resolved, &dom, &rtl, &mut report);

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
        let ua = ua_values(&index, &m);
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

    // ★ MARGIN COLLAPSING, compiled into literal margins (see `collapse`).
    phases::lap("resolve");
    collapse::compile(&mut resolved, &dom, &ua_values(&index, &m), &mut report);
    phases::lap("collapse");

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
    named_line_pass(&mut resolved, &dom, &mut report);

    // ★ A conditional slot (a state, a media context) is emitted as a rule
    // that comes LATER in the output and is at least as specific, so it
    // always wins there. In the source it only wins if it outranks what the
    // element's unconditional declarations already say. MIT's promo
    // heading had `.promo h3 { font-size: 1rem }` beating Bootstrap's
    // `@media (max-width:1200px) { h3 { font-size: calc(…) } }`; emitted as
    // is, the media rule won, and 26px type sat on a 16px line.
    let outranked: Vec<((Handle, String, String), String)> = resolved.iter()
        .filter(|((_, st, me), _)| !st.is_empty() || !me.is_empty())
        .flat_map(|((h, st, me), props)| {
            let under: Vec<(Handle, String, String)> = [(*h, String::new(), String::new()), (*h, st.clone(), String::new()), (*h, String::new(), me.clone())]
                .into_iter().filter(|k| k.1 != *st || k.2 != *me).collect();
            props.iter().filter(|(p, (pri, _))| under.iter().any(|k| resolved.get(k).and_then(|x| x.get(*p)).is_some_and(|(q, _)| q > pri)))
                .map(|(p, _)| ((*h, st.clone(), me.clone()), p.clone())).collect::<Vec<_>>()
        }).collect();
    if !outranked.is_empty() { report.drop_n("conditional declaration outranked by an unconditional one", outranked.len()) }
    for (k, p) in outranked { if let Some(x) = resolved.get_mut(&k) { x.remove(&p); } }

    phases::lap("layout-rules");
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
    report.spent = Spent {
        css_bytes: budget.css_bytes - css_left_final,
        elements: els.len(),
        match_steps: m.steps(),
        var_steps: budget.var_steps - VAR_BUDGET.with(|b| b.get().0),
    };
    if VAR_EXHAUSTED.with(|b| b.get()) { report.drop("var() substitution past the document's budget (declarations dropped)") }
    report.elements_styled = per_element.len();

    phases::lap("emit");
    // ★ Embedded content, read before step 6 strips `width`/`height`: each
    // `<iframe>` and whether it is a recognised embed (see `embedded`).
    let frames: Vec<(Handle, Option<String>, Option<u32>, Option<u32>)> = dom.by_tag_anywhere("iframe").into_iter().map(|h| {
        let dim = |a: &str| dom.attr(h, a).and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok()).filter(|v| (1..=8192).contains(v));
        // ★ Real embeds mostly arrive LAZY: the URL sits in a data attribute
        // a script copies to `src` (LinkedIn's `data-delayed-url`, wikiHow's
        // `data-src`), and that script is another origin's, so it no longer
        // runs at conversion (legacy-web §5.4.1f). Read those too.
        let island = ["src", "data-src", "data-lazy-src", "data-delayed-url"].iter()
            .find_map(|a| dom.attr(h, a).and_then(navigator_dom::media::recognised_embed));
        (h, island, dim("width"), dim("height"))
    }).collect();
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
    embedded(&mut dom, &frames, &mut report);

    // 7. Declared intrinsic sizes (§3.13): an image the input measured gets
    //    `width`/`height` attributes; one it did not is DROPPED, because the
    //    profile admits no measurement path and a guess is worse than a gap.
    // An inline SVG made standalone: its box (styled above like any
    // element) becomes a replaced element painting that blob, and its
    // children go — they are in the blob.
    // ★ The standalone text travels with it (`atrium-svg`), so the renderer
    // can draw it as vector shapes (NSG `path` nodes, Fresco's fill op)
    // where it can, and as the blob's bitmap where it cannot.
    for (h, address, text) in &inline_svgs {
        for c in dom.children_of(*h) { detach(&mut dom, c) }
        dom.set_attr(*h, "src", address);
        dom.set_attr(*h, "atrium-svg", text);
    }
    let imgs: Vec<Handle> = dom.by_tag_anywhere("img").into_iter().chain(inline_svgs.iter().map(|(h, _, _)| *h)).collect();
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
    phases::lap("rewrite");
    let body = dom.serialize();
    let doc = format!("<html{root_attrs}><head><style>\n{out}</style></head>{body}</html>\n");
    phases::lap("serialize");
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

/// The attribute marking a table caption compiled to a cell: the table
/// pre-measure counts only its min-content (CSS 2.1 §17.4: a caption does
/// not make its table wider than the caption's longest word needs).
pub const CAPTION_CELL: &str = "atrium-caption";

/// ★ TABLE CAPTIONS. The profile has no `table-caption`; it used to become a
/// plain block. In a table that HAS rows that is close enough (the caption
/// sits above or below it, as wide as the container). But a `display:
/// table` with NO rows — Wikipedia's thumbnail `figure`: an image and a
/// `figcaption` — is a one-cell table around the image, which CSS makes as
/// wide as the image, with the caption wrapped to that width. As a block,
/// the figure took the caption's one-line width: a 684 px thumbnail.
///
/// Compiled: the table's other content goes into one explicit row and cell,
/// and each caption becomes a cell in a row of its own, in document order
/// (Wikipedia's `caption-side: bottom` captions come last), marked
/// `CAPTION_CELL` so the table pre-measure counts only its min-content.
fn caption_pass(dom: &mut Dom, resolved: &mut collapse::Resolved, report: &mut Report) {
    let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
    let base = |resolved: &collapse::Resolved, h: Handle, p: &str| resolved.get(&(h, String::new(), String::new())).and_then(|pr| pr.get(p)).map(|(_, v)| v.clone());
    let captions: Vec<Handle> = resolved.iter()
        .filter(|((_, st, me), pr)| st.is_empty() && me.is_empty() && pr.get("display").is_some_and(|(_, v)| v == "table-caption"))
        .map(|((h, _, _), _)| *h).collect();
    let mut tables: Vec<Handle> = captions.iter().filter_map(|c| dom.get(*c).and_then(|n| n.parent)).collect();
    tables.sort_unstable(); tables.dedup();
    let mut compiled = 0usize;
    for t in tables {
        let disp = base(resolved, t, "display").or_else(|| (dom.tag(t) == Some("table")).then(|| "table".to_string()));
        if disp.as_deref() != Some("table") { continue }
        let kids = dom.children_of(t);
        let has_rows = kids.iter().any(|k| {
            let d = base(resolved, *k, "display").or_else(|| dom.tag(*k).map(|tg| match tg { "tr" => "table-row", "td" | "th" => "table-cell", "thead" | "tbody" | "tfoot" => "table-row-group", _ => "" }.to_string()));
            matches!(d.as_deref(), Some("table-row" | "table-cell" | "table-row-group"))
        });
        if has_rows { continue }
        let is_caption = |h: Handle| captions.contains(&h);
        let set = |resolved: &mut collapse::Resolved, h: Handle, props: &[(&str, &str)]| {
            let slot = resolved.entry((h, String::new(), String::new())).or_default();
            for (k, v) in props { slot.insert(k.to_string(), (pri, v.to_string())); }
        };
        let (mut content_row, mut content_cell): (Option<Handle>, Option<Handle>) = (None, None);
        for k in kids {
            detach(dom, k);
            if is_caption(k) {
                let row = dom.create(Kind::Element("div".into()));
                set(resolved, row, &[("display", "table-row")]);
                dom.append(t, row);
                dom.append(row, k);
                set(resolved, k, &[("display", "table-cell")]);
                dom.set_attr(k, CAPTION_CELL, "");
            } else {
                let cell = match content_cell {
                    Some(c) => c,
                    None => {
                        let row = dom.create(Kind::Element("div".into()));
                        let cell = dom.create(Kind::Element("div".into()));
                        set(resolved, row, &[("display", "table-row")]);
                        set(resolved, cell, &[("display", "table-cell"), ("padding-top", "0px"), ("padding-right", "0px"), ("padding-bottom", "0px"), ("padding-left", "0px")]);
                        dom.append(t, row);
                        dom.append(row, cell);
                        content_row = Some(row);
                        content_cell = Some(cell);
                        cell
                    }
                };
                dom.append(cell, k);
            }
        }
        let _ = content_row;
        compiled += 1;
    }
    // Every other caption is a block, as before.
    for ((_, st, _), pr) in resolved.iter_mut() {
        if !st.is_empty() { continue }
        if let Some((_, v)) = pr.get_mut("display") { if v == "table-caption" { *v = "block".into() } }
    }
    for c in &captions {
        if dom.attr(*c, CAPTION_CELL).is_some() {
            resolved.entry((*c, String::new(), String::new())).or_default().insert("display".into(), (pri, "table-cell".into()));
        }
    }
    if compiled > 0 { report.drop_n("table with a caption and no rows: content and caption compiled to cells", compiled) }
}

/// The attributes marking a compiled float's row and its flow column, for
/// `fit_floats` (which removes them).
pub const FLOAT_ROW: &str = "atrium-float";
pub const FLOAT_FLOW: &str = "atrium-float-flow";

/// ★ WHAT GOES BESIDE A FLOAT, decided by layout (see the float pass). A row
/// first holds everything up to the next float or `clear`; laid out, the
/// content that starts at or below the float's bottom is moved back out,
/// after the row, where it takes the full width — as it would in a browser,
/// where text below a float is no longer beside it. A block that STARTS
/// beside the float stays beside it whole (a paragraph is not split), so the
/// column may run a little past the float's bottom.
///
/// Outermost rows first: a row nested in another's column was measured at
/// the column's width, and is measured again once it has its own place.
/// At most `FIT_PASSES` layouts; the reference width is `env`'s.
pub fn fit_floats(doc: &str, fonts: &navigator_render::fontset::FontSet, env: &navigator_style::cascade::Env) -> (String, FloatFit) {
    let mut fit = FloatFit::default();
    if !doc.contains(FLOAT_ROW) { return (doc.to_string(), fit) }
    let mut dom = navigator_dom::parse(doc);
    const MARK: &str = "atrium-fk";
    for pass in 0..FIT_PASSES {
        let rows: Vec<Handle> = (0..dom.nodes.len() as Handle).filter(|h| dom.connected(*h) && dom.attr(*h, FLOAT_ROW).is_some_and(|v| v != "done")).collect();
        if rows.is_empty() { break }
        let in_flow = |dom: &Dom, h: Handle| {
            let mut cur = dom.get(h).and_then(|n| n.parent);
            while let Some(c) = cur {
                if dom.attr(c, FLOAT_FLOW).is_some() && dom.get(c).and_then(|n| n.parent).is_some_and(|r| dom.attr(r, FLOAT_ROW).is_some_and(|v| v != "done")) { return true }
                cur = dom.get(c).and_then(|n| n.parent);
            }
            false
        };
        // The outermost undecided rows (all of them on the last pass).
        let last = pass + 1 == FIT_PASSES;
        let now: Vec<Handle> = rows.iter().copied().filter(|r| last || !in_flow(&dom, *r)).collect();
        let geo = navigator_render::html::layout_geometry(&dom.serialize_marked(MARK), fonts, env, &navigator_render::Subresources::new(), MARK);
        fit.layouts += 1;
        for row in now {
            dom.set_attr(row, FLOAT_ROW, "done");
            let kids = dom.element_children(row);
            let (Some(&flow), Some(&float)) = (kids.iter().find(|k| dom.attr(**k, FLOAT_FLOW).is_some()), kids.iter().find(|k| dom.attr(**k, FLOAT_FLOW).is_none())) else { continue };
            let Some(fg) = geo.elements.get(&(float as u64)) else { continue };
            let bottom = fg.y + fg.h;
            let items = dom.children_of(flow);
            // ★ A float that leaves the column too narrow has nothing beside
            // it: in CSS, content that does not fit beside a float goes below
            // it. Without this a 684 px figure left ru Wikipedia a 44 px
            // column, one word per line.
            let w = |h: Handle| geo.elements.get(&(h as u64)).map(|g| g.w).unwrap_or(0);
            let narrow = w(flow) * MIN_BESIDE_DIV < w(row);
            if narrow { fit.narrow += 1 }
            let cut = if narrow { Some(0) } else {
                items.iter().position(|c| geo.elements.get(&(*c as u64)).is_some_and(|g| !g.inline && g.y >= bottom))
            };
            fit.rows += 1;
            let Some(cut) = cut else { continue };
            let Some(parent) = dom.get(row).and_then(|n| n.parent) else { continue };
            let next = dom.children_of(parent).into_iter().skip_while(|c| *c != row).nth(1);
            for &c in &items[cut..] {
                detach(&mut dom, c);
                dom.insert_before(parent, c, next);
            }
            fit.moved += items[cut..].len();
            fit.cut += 1;
        }
    }
    for h in 0..dom.nodes.len() as Handle {
        if dom.attr(h, FLOAT_ROW).is_some() { dom.remove_attr(h, FLOAT_ROW) }
        if dom.attr(h, FLOAT_FLOW).is_some() { dom.remove_attr(h, FLOAT_FLOW) }
    }
    (dom.serialize(), fit)
}

/// At most this many layouts to fit a document's floats.
pub const FIT_PASSES: usize = 4;

/// A flow column narrower than 1/this of its row has nothing beside the float.
pub const MIN_BESIDE_DIV: i64 = 4;

/// What `fit_floats` did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FloatFit {
    /// Rows measured; rows cut (content moved back below the float); nodes moved.
    pub rows: usize,
    /// Rows whose float left too narrow a column (all their content moved below).
    pub narrow: usize,
    pub cut: usize,
    pub moved: usize,
    /// Layouts it took.
    pub layouts: usize,
}

/// Compile each lone float with content after it into a flex row (see the
/// float pass). Returns (compiled, left alone: nothing beside them).
fn compile_floats(dom: &mut Dom, resolved: &mut collapse::Resolved, floats: &[(Handle, String)],
                  float_of: &BTreeMap<Handle, String>, clear_of: &BTreeMap<Handle, String>,
                  rtl: &BTreeMap<Handle, bool>) -> (usize, usize) {
    let (mut beside, mut alone) = (0, 0);
    let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
    let set = |resolved: &mut collapse::Resolved, h: Handle, props: &[(&str, &str)]| {
        let slot = resolved.entry((h, String::new(), String::new())).or_default();
        for (k, v) in props { slot.insert(k.to_string(), (pri, v.to_string())); }
    };
    for (f, dir) in floats {
        let Some(p) = dom.get(*f).and_then(|n| n.parent) else { continue };
        let kids = dom.children_of(p);
        let Some(pos) = kids.iter().position(|c| c == f) else { continue };
        // What follows, up to the next float or anything that clears.
        let mut after: Vec<Handle> = vec![];
        for &c in &kids[pos + 1..] {
            if float_of.contains_key(&c) || clear_of.contains_key(&c) { break }
            after.push(c);
        }
        let content = after.iter().any(|c| match dom.get(*c).map(|n| &n.kind) {
            Some(Kind::Element(_)) => resolved.get(&(*c, String::new(), String::new()))
                .and_then(|pr| pr.get("display")).map(|(_, v)| v != "none").unwrap_or(true),
            Some(Kind::Text(t)) => !t.trim().is_empty(),
            _ => false,
        });
        if !content { alone += 1; continue }
        let row = dom.create(Kind::Element("div".into()));
        let flow = dom.create(Kind::Element("div".into()));
        dom.set_attr(row, FLOAT_ROW, &beside.to_string());
        dom.set_attr(flow, FLOAT_FLOW, "");
        dom.insert_before(p, row, Some(*f));
        detach(dom, *f);
        for &c in &after { detach(dom, c); dom.append(flow, c); }
        let start = if rtl.get(&p).copied().unwrap_or(false) { "right" } else { "left" };
        if dir == start { dom.append(row, *f); dom.append(row, flow); } else { dom.append(row, flow); dom.append(row, *f); }
        set(resolved, row, &[("display", "flex"), ("flex-direction", "row"), ("align-items", "flex-start")]);
        set(resolved, flow, &[("display", "block"), ("flex-grow", "1"), ("flex-shrink", "1"), ("flex-basis", "0px"), ("min-width", "0px")]);
        // A float is as wide as its content up to the space there is (CSS
        // 2.1 §10.3.5, shrink-to-fit): a flex item that does not grow.
        set(resolved, *f, &[("flex-grow", "0"), ("flex-shrink", "1")]);
        beside += 1;
    }
    (beside, alone)
}

/// ★★ NO IFRAMES IN THE DOCUMENT (profile §3: `<iframe>`, `<object>` and
/// `<embed>` are excluded; composition is Limen's). Measured 2026-11-13
/// (navigator-ui §6.6): 68 iframes survived normalization on the 119-page
/// corpus, 36 visible on 18 pages, and 7 pages kept an empty box for one.
///
/// - An `<iframe>` that is a RECOGNISED EMBED (`media::recognised_embed`:
///   a YouTube or Vimeo player) becomes a `<video>` box carrying only its
///   generated class, `id`, `title`, its declared size (or a 16/9 ratio)
///   and `atrium-island` = the canonical player URL. The renderer gives it
///   an `island` embed slot: a live part, started only by the reader.
/// - Every other `<iframe>`, and every `<frame>`, `<frameset>` and
///   `<embed>`, is dropped with its box, and counted.
/// - An `<object>` is replaced by its children, HTML's fallback content.
///
/// Runs after the cascade, so a structural selector that counted the
/// iframe as a sibling matched as it does in a browser. Any `atrium-island`
/// the PAGE wrote is removed first: only this function marks an island.
fn embedded(dom: &mut Dom, frames: &[(Handle, Option<String>, Option<u32>, Option<u32>)], report: &mut Report) {
    use navigator_dom::media::ISLAND_ATTR;
    for h in (0..dom.nodes.len() as Handle).filter(|h| dom.attr(*h, ISLAND_ATTR).is_some()).collect::<Vec<_>>() {
        dom.remove_attr(h, ISLAND_ATTR);
        report.drop(format!("`{ISLAND_ATTR}` written by the page"));
    }
    for (h, island, w, ht) in frames {
        if !dom.connected(*h) { continue }
        let Some(src) = island else {
            detach(dom, *h);
            report.drop("<iframe> (profile §3; not a recognised embed)");
            continue
        };
        let keep: Vec<(String, String)> = ["class", "id", "title"].iter()
            .filter_map(|a| dom.attr(*h, a).map(|v| (a.to_string(), v.to_string()))).collect();
        for c in dom.children_of(*h) { detach(dom, c) }
        if let Some(n) = dom.get_mut(*h) {
            n.kind = Kind::Element("video".into());
            n.attrs.clear();
        }
        for (a, v) in &keep { dom.set_attr(*h, a, v) }
        dom.set_attr(*h, ISLAND_ATTR, src);
        match (w, ht) {
            (Some(w), Some(ht)) => { dom.set_attr(*h, "width", &w.to_string()); dom.set_attr(*h, "height", &ht.to_string()) }
            _ => dom.set_attr(*h, "natural-ratio", "16/9"),
        }
        *report.dropped.entry("<iframe> recognised as an embed (now an island box)".into()).or_default() += 1;
    }
    for tag in ["frame", "frameset", "embed"] {
        for h in dom.by_tag_anywhere(tag) {
            if !dom.connected(h) { continue }
            detach(dom, h);
            report.drop(format!("<{tag}> (profile §3)"));
        }
    }
    for h in dom.by_tag_anywhere("object") {
        let Some(p) = dom.get(h).and_then(|n| n.parent) else { continue };
        for c in dom.children_of(h) {
            if dom.tag(c) == Some("param") { continue }
            dom.detach_for_move(c);
            dom.insert_before(p, c, Some(h));
        }
        detach(dom, h);
        report.drop("<object> (profile §3; its fallback content kept)");
    }
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
/// An element's custom properties: name -> value tokens, shared.
type Customs = BTreeMap<String, Rc<Vec<Token>>>;

fn resolve_customs(sheet: &css::Sheet, index: &sel::RuleIndex, m: &sel::Matcher, dom: &Dom)
    -> BTreeMap<(Handle, String), Rc<Customs>> {
    // Winners per (element, media), by the same priority rule as any property.
    let mut own: BTreeMap<(Handle, String), BTreeMap<String, (Priority, Rc<Vec<Token>>)>> = BTreeMap::new();
    let mut medias: Vec<String> = vec![String::new()];
    for rule in &sheet.rules {
        if !rule.decls.iter().any(|d| d.name.starts_with("--")) { continue }
        let Ok(list) = sel::parse_list(&rule.selector) else { continue };
        let media = match &rule.media { None => String::new(), Some(q) => match admitted_media(q) { Some(q) => q, None => continue } };
        if !medias.contains(&media) { medias.push(media.clone()) }
        for c in &list {
            if sel::has_pseudo_element(c) { continue }
            let spec = sel::specificity(c);
            for h in index.candidates(c) {
                if !m.matches(*h, c) { continue }
                let slot = own.entry((*h, media.clone())).or_default();
                for d in rule.decls.iter().filter(|d| d.name.starts_with("--")) {
                    let pri: Priority = (d.important, layer_rank(rule.layer, sheet.layers.len(), d.important), spec, rule.order);
                    match slot.get(&d.name) {
                        Some((old, _)) if *old > pri => {}
                        _ => { slot.insert(d.name.clone(), (pri, Rc::new(d.value.clone()))); }
                    }
                }
            }
        }
    }
    // Inherit: an element sees its ancestors' customs, its own winning.
    //
    // ★ The inherited map is SHARED, not copied. An element that declares no
    // custom property of its own sees exactly its parent's map, so it holds
    // the parent's `Rc`. Only an element with its own declarations pays for
    // a copy, and that copy shares every token list through `Rc` too.
    // Copying the whole chain into every element — which is what this did —
    // is elements × custom properties × media contexts: GitHub's page
    // (Primer defines thousands of `--` properties on `:root`) took 14 GB
    // and was OOM-killed on a 16 GB machine.
    let mut out: BTreeMap<(Handle, String), Rc<Customs>> = BTreeMap::new();
    let mut ctxs: Vec<String> = vec![];
    for media in &medias {
        ctxs.clear();
        ctxs.push(String::new());
        if !media.is_empty() { ctxs.push(media.clone()) }
        let mut memo: BTreeMap<Handle, Rc<Customs>> = BTreeMap::new();
        let empty: Rc<Customs> = Rc::new(BTreeMap::new());
        for h in index.all() {
            // Walk up to the nearest ancestor already resolved (or the root).
            let mut chain = vec![];
            let mut cur = Some(*h);
            let mut base = empty.clone();
            while let Some(c) = cur {
                if let Some(m) = memo.get(&c) { base = m.clone(); break }
                chain.push(c);
                cur = dom.get(c).and_then(|n| n.parent);
            }
            // Then resolve back down, root-most first.
            for a in chain.into_iter().rev() {
                let mut map = base.clone();
                // A media-conditioned custom overrides the base one here.
                for ctx in &ctxs {
                    if let Some(w) = own.get(&(a, ctx.clone())) {
                        let m = Rc::make_mut(&mut map);
                        for (k, (_, v)) in w { m.insert(k.clone(), v.clone()); }
                    }
                }
                memo.insert(a, map.clone());
                base = map;
            }
            let map = memo.get(h).cloned().unwrap_or_else(|| empty.clone());
            if !map.is_empty() { out.insert((*h, media.clone()), map); }
        }
    }
    out
}

/// Every UA-sheet declaration that applies to each element, by source order
/// (the UA sheet is simple enough that order alone decides).
fn ua_values(index: &sel::RuleIndex, m: &sel::Matcher) -> BTreeMap<Handle, BTreeMap<String, String>> {
    let mut ua_sheet = css::Sheet::default();
    let mut ua_order = 0usize;
    css::parse(navigator_style::cascade::UA_CSS, &mut ua_order, &mut ua_sheet);
    let mut ua: BTreeMap<Handle, BTreeMap<String, String>> = BTreeMap::new();
    for rule in &ua_sheet.rules {
        let Ok(list) = sel::parse_list(&rule.selector) else { continue };
        // An element matched by two of the list's selectors gets the same
        // declarations twice: the same values, so the same map.
        for c in &list {
            for h in index.candidates(c) {
                if !m.matches(*h, c) { continue }
                for d in &rule.decls {
                    ua.entry(*h).or_default().insert(d.name.clone(), css::write_tokens(&d.value).trim().to_string());
                }
            }
        }
    }
    ua
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
fn substitute(value: &[Token], custom: &Customs, depth: usize) -> Vec<Token> {
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
fn resolve_var(value: &[Token], custom: &Customs, depth: usize) -> Option<Vec<Token>> {
    if !value.iter().any(|t| matches!(&t.tok, Tok::Function(f) if f.eq_ignore_ascii_case("var"))) {
        return Some(value.to_vec());
    }
    if depth > 16 { return None }
    let (_, cap) = VAR_BUDGET.with(|b| b.get());
    let mut out = vec![];
    // Charge what this call produced against the document's budget, and
    // refuse a value past the per-value cap.
    let over = |out: &Vec<Token>| -> bool {
        if out.len() > cap { return true }
        false
    };
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
                let got = match (defined, comma) {
                    (Some(v), _) => v,
                    (None, Some(c)) => resolve_var(&inner[c + 1..], custom, depth + 1)?,
                    (None, None) => return None,
                };
                let spent = VAR_BUDGET.with(|b| {
                    let (left, cap) = b.get();
                    let n = got.len() as u64;
                    if n > left { return false }
                    b.set((left - n, cap));
                    true
                });
                if !spent { VAR_EXHAUSTED.with(|b| b.set(true)); return None }
                out.extend(got);
                if over(&out) { return None }
                i = j;
            }
            _ => { out.push(value[i].clone()); i += 1 }
        }
    }
    Some(out)
}

/// A declaration as longhands the profile admits, or nothing.
fn to_longhands(name: &str, value: &[Token], inputs: &Inputs, report: &mut Report) -> Vec<(String, String)> {
    to_longhands_inner(name, value, inputs, report, true)
}

fn to_longhands_plain(name: &str, value: &[Token], inputs: &Inputs, report: &mut Report) -> Vec<(String, String)> {
    to_longhands_inner(name, value, inputs, report, false)
}

fn to_longhands_inner(name: &str, value: &[Token], inputs: &Inputs, report: &mut Report, markers: bool) -> Vec<(String, String)> {
    if name.starts_with("--") { return vec![] }
    // ★ The pre-standard grid gap names are ALIASES in every engine (CSS
    // Box Alignment §8.3): `grid-gap: 24px` is `gap: 24px`, and dropping it
    // set every such grid's items edge to edge.
    let name = match name {
        "grid-gap" => "gap",
        "grid-column-gap" => "column-gap",
        "grid-row-gap" => "row-gap",
        // ★ The prefixed FLEXBOX names are aliases too (CSS Compat §3: every
        // engine maps them to the standard property at parse time), so a
        // page that wrote only `-webkit-justify-content` meant it — 693
        // corpus declarations were dropped and their rows packed at the
        // start. Same property, so the unprefixed one wins only by the
        // ordinary cascade. NOT the 2009 `-webkit-box-*` model: its names
        // (`-webkit-box-flex`, `-webkit-box-pack`) are a different grammar.
        "-webkit-justify-content" => "justify-content",
        "-webkit-align-items" => "align-items",
        "-webkit-align-self" => "align-self",
        "-webkit-align-content" => "align-content",
        "-webkit-flex" => "flex",
        "-webkit-flex-direction" => "flex-direction",
        "-webkit-flex-wrap" => "flex-wrap",
        "-webkit-flex-flow" => "flex-flow",
        "-webkit-flex-grow" => "flex-grow",
        "-webkit-flex-shrink" => "flex-shrink",
        "-webkit-flex-basis" => "flex-basis",
        "-webkit-order" => "order",
        n => n,
    };
    let text = css::write_tokens(value).trim().to_string();
    if text.is_empty() { return vec![] }
    // ★ `grid` and `grid-template` set the TEMPLATES, whose longhands need
    // this function's own handling (line names, areas), so each longhand
    // comes back through it. Dropping them left 72 css-grid WPT tests'
    // grids with no tracks at all — every item in one cell.
    if name == "grid" || name == "grid-template" {
        return match expand::expand(name, value) {
            Some(p) => p.into_iter().flat_map(|(k, v)| to_longhands_inner(&k, &tokenize(&v), inputs, report, markers)).collect(),
            None => { report.drop(format!("value `{name}: {text}`")); vec![] }
        };
    }
    // Already a longhand the profile knows?
    // `grid-area: <name>` is a name, not four lines; keep it whole.
    if name == "grid-area" && text.split(['/', ' ']).filter(|x| !x.trim().is_empty()).count() == 1
        && !text.trim().chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return vec![("grid-area-name".to_string(), text)];
    }
    if name == "grid-template-areas" { return vec![(name.to_string(), text)] }
    // ★ NAMED GRID LINES, half one: the template's names, as a map from
    // name to line number, carried beside the stripped track list (see the
    // named-line pass). Half two is below, on the items.
    let mut line_names = vec![];
    if (name == "grid-template-columns" || name == "grid-template-rows") && text.contains('[') {
        if let Some(map) = grid_line_names(&text) {
            line_names.push((format!("{name}-lines-marker"), map));
        }
    }
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
    if name == "float" && matches!(text.as_str(), "left" | "right" | "none" | "inline-start" | "inline-end") {
        return vec![("float-marker".to_string(), text)];
    }
    // `border-collapse` is compiled away (see `borders`).
    if name == "border-collapse" && matches!(text.as_str(), "collapse" | "separate" | "inherit" | "initial" | "unset" | "revert") {
        return vec![("border-collapse-marker".to_string(), text)];
    }
    // `clear` ends what goes beside a float (see the float pass).
    if name == "clear" && matches!(text.as_str(), "left" | "right" | "both" | "none" | "inline-start" | "inline-end") {
        return vec![("clear-marker".to_string(), text)];
    }
    // ★ The VISUALLY-HIDDEN idiom: a 1px box, `clip: rect(0 0 0 0)` and
    // `clip-path: inset(50%)`. The profile has no clipping, so both used to
    // be dropped and the 1px box drew its text anyway: Folha's "Salvar
    // artigos" drop-down was painted over its masthead. An EMPTY clip is not
    // a shape, though, it is "paint nothing", and that the profile can say.
    // Carried as a marker and decided once the cascade is known, because
    // `clip` only clips a positioned box (see the clip pass). Any other clip
    // is still a shape the profile cannot draw, and is still dropped.
    // ★ LIGATURE ICON FONTS. `<span class="material-icons">keyboard_arrow_left
    // </span>` is an icon only in its own font, which turns the WORD into a
    // glyph. The profile ships no such font, so the word itself was drawn,
    // at icon size: MIT's carousel arrows read "keyboard_arrow_left" in
    // 60px type across the page. Carried as a marker, so a later rule that
    // names an ordinary family still wins, and compiled in the icon pass to
    // an empty icon-sized box.
    if markers && name == "font-family" {
        let first = text.split(',').next().unwrap_or("").trim().trim_matches(['"', '\'']).to_ascii_lowercase();
        let icon = first.starts_with("material icons") || first.starts_with("material symbols");
        let mut v = to_longhands_plain(name, value, inputs, report);
        v.push(("icon-font-marker".to_string(), if icon { "icon" } else { "text" }.to_string()));
        return v;
    }
    if name == "clip" {
        match clip_rect_is_empty(&text) {
            Some(true) => return vec![("clip-marker".to_string(), "empty".to_string())],
            Some(false) => return vec![("clip-marker".to_string(), "auto".to_string())],
            None => {}
        }
    }
    if name == "clip-path" || name == "-webkit-clip-path" {
        match clip_path_is_empty(&text) {
            Some(true) => return vec![("clip-path-marker".to_string(), "empty".to_string())],
            Some(false) => return vec![("clip-path-marker".to_string(), "none".to_string())],
            None => {}
        }
    }
    let pairs: Vec<(String, String)> = if navigator_style::values::grammar(name).is_some() {
        vec![(name.to_string(), text)]
    } else {
        match expand::expand(name, value) {
            Some(p) => p.into_iter().filter(|(p, _)| navigator_style::values::grammar(&expand::physical(p, false)).is_some()).collect(),
            None => { report.drop(format!("property `{name}`")); return vec![] }
        }
    };
    // ★ Check the output against the profile's own grammar, repair what has
    // an honest mapping, and drop the rest WITH ITS VALUE. Nothing the
    // renderer can refuse leaves this function.
    let mut out: Vec<(String, String)> = line_names;
    out.extend(pairs.into_iter().filter_map(|(p, v)| {
        // ★ NAMED GRID LINES, half two: `grid-column: content-start /
        // content-end` names lines of the PARENT's template, so it is
        // carried as a marker and numbered once the cascade knows that
        // template. Notion's whole page sits in such a column; dropping the
        // names put it in the 16px gutter.
        if matches!(p.as_str(), "grid-column-start" | "grid-column-end" | "grid-row-start" | "grid-row-end")
            && is_line_name(&v) {
            return Some((format!("{p}-name-marker"), v));
        }
        // ★ `display: contents` is not a value the profile has — it is a
        // STRUCTURAL instruction: generate no box, and let the children take
        // this element's place in its parent. The normalizer can carry that
        // out literally (see `splice_contents`), so it is kept here as a
        // marker rather than dropped. 1510 elements in a 29-document corpus.
        if p == "display" && v == "contents" { return Some((p, v)) }
        // A table CAPTION is decided on the DOM (see the caption pass).
        if p == "display" && v.eq_ignore_ascii_case("table-caption") { return Some((p, "table-caption".to_string())) }
        // ★ NAMED GRID AREAS. The profile places items by NUMBER, and modern
        // layouts name them: `grid-template-areas` on the container plus
        // `grid-area: toolbar` on each child. The mapping from a name to a
        // rectangle of lines is static, so the normalizer can do it — but it
        // needs both halves, so they are carried here as markers and resolved
        // once the whole cascade is known. Without it every child lands in
        // the same cell: rustdoc's breadcrumb rendered on top of its search
        // box.
        if p == "grid-template-areas" || p == "grid-area-name" || p == "float-marker" || p == "clear-marker" || p == "border-collapse-marker" || p == "box-sizing-marker"
            || p == "clip-marker" || p == "clip-path-marker" || p.ends_with("-lines-marker") || p == "icon-font-marker" { return Some((p, v)) }
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
        // ★ `text-align: left | right` are PHYSICAL, and the profile's
        // values are direction-relative: which one `left` is depends on the
        // element's direction, and — because the computed value inherits AS
        // `left` — on the direction of every descendant that inherits it.
        // Kept as written and resolved in the direction pass.
        if matches!(p.as_str(), "text-align" | "justify-content") && matches!(v.to_ascii_lowercase().as_str(), "left" | "right") { return Some((p, v.to_ascii_lowercase())) }
        // ★ `*-reverse` and Box Alignment's `start` / `end` / `normal`: what
        // they come out as depends on whether the flex container ends up
        // REVERSED, which is decided once the cascade is known (see the
        // reverse pass). Kept as written until then.
        if p == "flex-direction" && matches!(v.to_ascii_lowercase().as_str(), "row-reverse" | "column-reverse") { return Some((p, v.to_ascii_lowercase())) }
        if p == "justify-content" && matches!(v.to_ascii_lowercase().as_str(), "start" | "end" | "normal") { return Some((p, v.to_ascii_lowercase())) }
        // ★ Self-alignment keywords the grammar lacks but that name an
        // admitted one EXACTLY once the element's direction, its container
        // and its box type are known — decided in the alignment pass.
        if alignment_carried(&p, &v.trim().to_ascii_lowercase()) { return Some((p, v.trim().to_ascii_lowercase())) }
        // An inline-side property (`margin-@start`) is checked as the
        // physical property it will become; both sides take the same values.
        let phys = expand::physical(&p, false);
        // `hidden` wins a collapsed-border conflict (CSS 2.1 §17.6.2.1), so
        // it is carried to the borders pass, which then draws it as `none`.
        if phys.starts_with("border-") && phys.ends_with("-style") && v.trim().eq_ignore_ascii_case("hidden") { return Some((p, "hidden".to_string())) }
        if value::admits(&phys, &v) { return Some((p, v)) }
        match value::repair(&phys, &v) {
            Some(fixed) => Some((p, fixed)),
            None => { report.drop(format!("value `{}: {v}`", p.replace('@', "inline-"))); None }
        }
    }));
    out
}

/// A grid line given by NAME: one custom identifier, not `auto`, `span …`
/// or a number.
fn is_line_name(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && !v.contains(' ') && v != "auto" && !matches!(v, "inherit" | "initial" | "unset" | "revert")
        && v.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        // ★ `-3` is a NUMBER, the third line from the end — an ident never
        // starts with `-` and a digit. Read as a name, every negative line
        // was dropped as "no parent template has it".
        && !(v.starts_with('-') && v[1..].starts_with(|c: char| c.is_ascii_digit()))
}

/// `[a b] 1fr [c] minmax(0, 2fr) [d]` -> `a=1 b=1 c=2 d=3`: each name and
/// the 1-based line it labels. `None` when a `repeat()` makes the count
/// depend on the box (`auto-fill`, `auto-fit`) or carries names inside it.
pub(crate) fn grid_line_names(v: &str) -> Option<String> {
    let mut line = 1usize;
    let mut names: Vec<(String, usize)> = vec![];
    let mut i = 0usize;
    let b: Vec<char> = v.chars().collect();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() { i += 1; continue }
        if c == '[' {
            let j = b[i..].iter().position(|x| *x == ']')? + i;
            for n in b[i + 1..j].iter().collect::<String>().split_whitespace() { names.push((n.to_string(), line)) }
            i = j + 1;
            continue;
        }
        // One track: up to the next whitespace at paren depth 0.
        let start = i;
        let mut depth = 0i32;
        while i < b.len() && !(depth == 0 && (b[i].is_whitespace() || b[i] == '[')) {
            match b[i] { '(' => depth += 1, ')' => depth -= 1, _ => {} }
            i += 1;
        }
        let track: String = b[start..i].iter().collect();
        if let Some(inner) = track.strip_prefix("repeat(").and_then(|x| x.strip_suffix(')')) {
            let (n, body) = inner.split_once(',')?;
            let n: usize = n.trim().parse().ok()?;
            if body.contains('[') { return None }
            line += n * count_tracks(body);
        } else {
            line += 1;
        }
    }
    if names.is_empty() { return None }
    Some(names.into_iter().map(|(n, l)| format!("{n}={l}")).collect::<Vec<_>>().join(" "))
}

fn count_tracks(body: &str) -> usize {
    let (mut n, mut depth, mut in_track) = (0usize, 0i32, false);
    for c in body.chars() {
        match c {
            '(' => { depth += 1; if !in_track { in_track = true; n += 1 } }
            ')' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => in_track = false,
            _ => if !in_track { in_track = true; n += 1 },
        }
    }
    n
}

/// Number the named lines (see `grid_line_names`). An item's name is looked
/// up in its PARENT's template, in the same media context or else the base
/// one. `foo` alone means `foo-start` for a start and `foo-end` for an end,
/// as CSS Grid §8.3 has it. A name no template has is dropped, and said so.
fn named_line_pass(resolved: &mut BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, dom: &Dom, report: &mut Report) {
    let maps: BTreeMap<(Handle, String, String), BTreeMap<String, usize>> = resolved.iter()
        .filter(|((_, st, _), _)| st.is_empty())
        .flat_map(|((h, _, media), props)| ["grid-template-columns", "grid-template-rows"].into_iter().filter_map(|axis| {
            let (_, v) = props.get(&format!("{axis}-lines-marker"))?;
            let m = v.split_whitespace().filter_map(|kv| { let (k, l) = kv.split_once('=')?; Some((k.to_string(), l.parse().ok()?)) }).collect();
            Some(((*h, media.clone(), axis.to_string()), m))
        }).collect::<Vec<_>>())
        .collect();
    // ★ A name set in the BASE context must also be numbered in every
    // media context where the parent's template names its lines
    // differently: the Guardian's `grid-column: title` is lines 2-6 of the
    // phone template and 2-14 of the 800px one, and numbering it once put
    // every section title in a third of the page.
    let mut markers: Vec<(Handle, String, &'static str, Priority, String)> = vec![];
    for ((h, st, media), props) in resolved.iter_mut() {
        if !st.is_empty() { continue }
        for p in ["grid-column-start", "grid-column-end", "grid-row-start", "grid-row-end"] {
            if let Some((pri, name)) = props.remove(&format!("{p}-name-marker")) { markers.push((*h, media.clone(), p, pri, name)) }
        }
    }
    let own: std::collections::BTreeSet<(Handle, String, &'static str)> = markers.iter().map(|(h, m, p, _, _)| (*h, m.clone(), *p)).collect();
    let mut want = vec![];
    for (h, media, p, pri, name) in markers {
        let axis = if p.starts_with("grid-column") { "grid-template-columns" } else { "grid-template-rows" };
        let parent = dom.get(h).and_then(|n| n.parent);
        if media.is_empty() {
            if let Some(q) = parent {
                for (qh, ctx, ax) in maps.keys() {
                    if *qh == q && ax == axis && !ctx.is_empty() && !own.contains(&(h, ctx.clone(), p)) {
                        want.push((h, ctx.clone(), p, pri, name.clone(), true));
                    }
                }
            }
        }
        want.push((h, media, p, pri, name, false));
    }
    let (mut placed, mut lost) = (0usize, 0usize);
    for (h, media, p, pri, name, extra) in want {
        let axis = if p.starts_with("grid-column") { "grid-template-columns" } else { "grid-template-rows" };
        let parent = dom.get(h).and_then(|n| n.parent);
        let map = parent.and_then(|q| maps.get(&(q, media.clone(), axis.to_string())).or_else(|| maps.get(&(q, String::new(), axis.to_string()))));
        let side = if p.ends_with("start") { "-start" } else { "-end" };
        let n = map.and_then(|m| m.get(&name).or_else(|| m.get(&format!("{name}{side}"))));
        match n {
            Some(n) => {
                let props = resolved.entry((h, String::new(), media)).or_default();
                if props.get(p).is_none_or(|(o, _)| *o <= pri) { props.insert(p.to_string(), (pri, n.to_string())); }
                if !extra { placed += 1 }
            }
            None if !extra => lost += 1,
            None => {}
        }
    }
    for (_, props) in resolved.iter_mut() { props.remove("grid-template-columns-lines-marker"); props.remove("grid-template-rows-lines-marker"); }
    if placed > 0 { report.drop_n("named grid lines resolved to numbers", placed) }
    if lost > 0 { report.drop_n("grid line name no parent template has", lost) }
}

/// A media query the profile admits, normalized to its own spelling — or
/// `None`, in which case the rules inside it are dropped.
fn admitted_media(q: &str) -> Option<String> {
    // ★ Minifiers drop the space after a `)`: `(min-width:768px)and
    // (max-width:1023px)` is the same query, and the browser reads it so.
    // Splitting on " and " alone refused 442 rules in the corpus, among
    // them Which?'s whole medium-width grid.
    let q = q.trim().to_ascii_lowercase().replace(")and", ") and").replace(" and(", " and (");
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

/// `clip: rect(t, r, b, l)` (commas optional): `Some(true)` when the rect
/// encloses nothing, `Some(false)` for `auto` or a non-empty rect, `None`
/// when it is not understood. `auto` in a side means that edge of the box,
/// which for this purpose is "not empty on that axis".
pub(crate) fn clip_rect_is_empty(v: &str) -> Option<bool> {
    let v = v.trim().to_ascii_lowercase();
    if v == "auto" || v == "initial" || v == "unset" { return Some(false) }
    let inner = v.strip_prefix("rect(")?.strip_suffix(')')?;
    let sides: Vec<Option<f64>> = inner.split([',', ' ']).filter(|x| !x.trim().is_empty())
        .map(|x| { let x = x.trim(); if x == "auto" { None } else { x.trim_end_matches("px").parse::<f64>().ok().map(Some).unwrap_or(Some(f64::NAN)) } })
        .collect();
    if sides.len() != 4 || sides.iter().flatten().any(|x| x.is_nan()) { return None }
    let (t, r, b, l) = (sides[0], sides[1], sides[2], sides[3]);
    let v_empty = matches!((t, b), (Some(t), Some(b)) if b <= t);
    let h_empty = matches!((l, r), (Some(l), Some(r)) if r <= l);
    Some(v_empty || h_empty)
}

/// `clip-path`: `Some(true)` for a region that encloses nothing (`inset()`
/// meeting in the middle, a zero circle), `Some(false)` for `none`, `None`
/// for any real shape.
pub(crate) fn clip_path_is_empty(v: &str) -> Option<bool> {
    let v = v.trim().to_ascii_lowercase();
    if v == "none" || v == "initial" || v == "unset" { return Some(false) }
    let pct = |x: &str| x.trim().strip_suffix('%').and_then(|n| n.trim().parse::<f64>().ok());
    if let Some(inner) = v.strip_prefix("inset(").and_then(|x| x.strip_suffix(')')) {
        let inner = inner.split(" round ").next().unwrap_or("");
        let p: Vec<f64> = inner.split_whitespace().map(pct).collect::<Option<Vec<_>>>()?;
        let (t, r, b, l) = match p.as_slice() {
            [a] => (*a, *a, *a, *a),
            [a, b] => (*a, *b, *a, *b),
            [a, b, c] => (*a, *b, *c, *b),
            [a, b, c, d] => (*a, *b, *c, *d),
            _ => return None,
        };
        return (t + b >= 100.0 || l + r >= 100.0).then_some(true);
    }
    if let Some(inner) = v.strip_prefix("circle(").and_then(|x| x.strip_suffix(')')) {
        let r = inner.split_whitespace().next().unwrap_or("");
        return matches!(r, "0" | "0px" | "0%").then_some(true);
    }
    None
}

/// Resolve the clip markers. An empty `clip` hides a box only when it is
/// absolutely positioned (CSS 2.1 §11.1.2); an empty `clip-path` hides any
/// box. "Hidden" here is `visibility: hidden`: the box keeps its place and
/// paints nothing, and its descendants inherit that. (A descendant that sets
/// `visibility: visible` would show, where the clip would still hide it; no
/// visually-hidden helper does that.) A state or media slot that turns the
/// clip back off (`:focus { clip: auto }`) turns visibility back on there.
/// HTML's "auto directionality": the first character of bidi class L, R or
/// AL in the element's text, skipping descendants that set their own `dir`
/// (and `bdi`, script, style, textarea). `None` when there is none.
fn first_strong_rtl(dom: &Dom, h: Handle) -> Option<bool> {
    for c in dom.children_of(h) {
        match dom.get(c).map(|n| &n.kind) {
            Some(Kind::Text(t)) => {
                for ch in t.chars() {
                    match unicode_bidi::bidi_class(ch) {
                        unicode_bidi::BidiClass::L => return Some(false),
                        unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL => return Some(true),
                        _ => {}
                    }
                }
            }
            Some(Kind::Element(_)) => {
                if dom.attr(c, "dir").is_some() || matches!(dom.tag(c), Some("bdi" | "script" | "style" | "textarea")) { continue }
                if let Some(r) = first_strong_rtl(dom, c) { return Some(r) }
            }
            _ => {}
        }
    }
    None
}

/// ★ THE DIRECTION PASS. `direction` inherits, and three things depend on
/// the direction an element ends up with, so they wait for the cascade:
///
/// - the inline-side properties carried as `@start` / `@end` (see
///   `expand::logical`) become left or right, and compete with the physical
///   property there by cascade priority — CSS's logical property groups;
/// - `text-align: left | right` become `start` or `end`. The computed value
///   inherits AS `left`, so an element whose direction differs from its
///   parent's and that inherits a physical alignment gets it written out;
/// - `justify-content: left | right`, the same way, in a row container;
/// - the UA sheet's list and `dd` indents are on the START side (HTML §15:
///   `padding-inline-start: 40px`), which the profile's UA writes as left —
///   in rtl they move to the right.
///
/// A media-context slot resolves with the element's base-context direction:
/// a page that changes `direction` inside a media query is not handled.
/// Returns each element's direction (true = rtl).
fn direction_pass(resolved: &mut BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, dom: &Dom, els: &[Handle],
                  report: &mut Report) -> BTreeMap<Handle, bool> {
    let base = |r: &BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, h: Handle, p: &str| -> Option<String> {
        r.get(&(h, String::new(), String::new())).and_then(|x| x.get(p)).map(|(_, v)| v.to_ascii_lowercase())
    };
    let parent_el = |h: Handle| dom.get(h).and_then(|n| n.parent).filter(|p| matches!(dom.get(*p).map(|n| &n.kind), Some(Kind::Element(_))));
    let mut rtl: BTreeMap<Handle, bool> = BTreeMap::new();
    for h in els {
        // Up to the nearest element whose direction is known or stated.
        let mut chain = vec![];
        let mut cur = Some(*h);
        let mut known = false;
        while let Some(x) = cur {
            if let Some(r) = rtl.get(&x) { known = *r; break }
            match base(resolved, x, "direction").as_deref() {
                Some("rtl") => { known = true; chain.push(x); break }
                Some("ltr") | Some("initial") => { known = false; chain.push(x); break }
                _ => { chain.push(x); cur = parent_el(x) }
            }
        }
        for x in chain { rtl.insert(x, known); }
    }
    let is_rtl = |h: Handle| rtl.get(&h).copied().unwrap_or(false);
    // A physical alignment inherited across a change of direction.
    let mut flips = vec![];
    for h in els {
        let Some(p) = parent_el(*h) else { continue };
        if is_rtl(*h) == is_rtl(p) || base(resolved, *h, "text-align").is_some_and(|v| v != "inherit") { continue }
        let mut cur = Some(p);
        while let Some(x) = cur {
            match base(resolved, x, "text-align").as_deref() {
                Some("inherit") | None => cur = parent_el(x),
                Some(v) => { if matches!(v, "left" | "right") { flips.push((*h, v.to_string())) } break }
            }
        }
    }
    for (h, v) in flips {
        resolved.entry((h, String::new(), String::new())).or_default().insert("text-align".into(), ((false, 0, (0, 0, 0), 0), v));
    }
    let mut logical = 0usize;
    for ((h, ..), props) in resolved.iter_mut() {
        let r = is_rtl(*h);
        if let Some((_, v)) = props.get_mut("text-align") {
            if matches!(v.as_str(), "left" | "right") { *v = if (v == "left") != r { "start" } else { "end" }.to_string() }
        }
        // `justify-content: left | right` (Box Alignment §5.1): physical in a
        // ROW container; in a column the main axis is not the inline one,
        // and they behave as `start`.
        let column = props.get("flex-direction").is_some_and(|(_, d)| d.starts_with("column"));
        if let Some((_, v)) = props.get_mut("justify-content") {
            if matches!(v.as_str(), "left" | "right") { *v = if column || (v == "left") != r { "flex-start" } else { "flex-end" }.to_string() }
        }
        let carried: Vec<String> = props.keys().filter(|k| k.contains('@')).cloned().collect();
        for k in carried {
            let (pri, v) = props.remove(&k).expect("listed");
            let phys = expand::physical(&k, r);
            logical += 1;
            match props.get(&phys) {
                Some((old, _)) if *old > pri => {}
                _ => { props.insert(phys, (pri, v)); }
            }
        }
    }
    if logical > 0 { report.drop_n("logical property resolved against the element's direction", logical) }
    let mut moved = 0usize;
    for h in els {
        if !is_rtl(*h) { continue }
        let (prop, by) = match dom.tag(*h) { Some("ul" | "ol") => ("padding", "40px"), Some("dd") => ("margin", "40px"), _ => continue };
        let slot = resolved.entry((*h, String::new(), String::new())).or_default();
        let (l, r) = (format!("{prop}-left"), format!("{prop}-right"));
        if !slot.contains_key(&l) { slot.insert(l, ((false, 0, (0, 0, 0), 0), "0px".into())); }
        if !slot.contains_key(&r) { slot.insert(r, ((false, 0, (0, 0, 0), 0), by.into())); }
        moved += 1;
    }
    if moved > 0 { report.drop_n("UA start-side indent moved to the right (rtl)", moved) }
    rtl
}

/// ★ THE REVERSE PASS. The profile has no `row-reverse` or `column-reverse`
/// (§3.4, G5: visual order is reading order), but a reversed flex container
/// can be COMPILED: reverse its children in the document and lay it out
/// forwards. The visual result is the same, and the reading order now
/// follows what the reader sees — which is what G5 asks for.
///
/// Laying the reversed children out forwards also moves the main-start
/// side, so the packing keywords that name it swap: `flex-start` ⇄
/// `flex-end`, and `normal` (and no `justify-content` at all), which pack
/// at main-start, become `flex-end`. `start` / `end` name the WRITING
/// mode's sides, which reversal does not move, so they become `flex-start`
/// / `flex-end` exactly as they do in a forward container; `left` / `right`
/// are physical and go to the direction pass as written. Auto margins and
/// gaps are physical or symmetric and need nothing.
///
/// It is compiled only when it is sound:
///
/// - **unconditionally** — the element is a reversed flex container in its
///   base context and in every state and media context that says anything
///   about `display` or `flex-direction`. One DOM cannot hold two orders.
/// - **single-line** — a wrapping container breaks lines in the reversed
///   order (`1 2 | 3 4` read from the right is `2 1 | 4 3`), and a reversed
///   document would break them from the other end (`4 3 | 2 1`). Where the
///   lines fall is only known after layout. `wrap-reverse` is excluded
///   outright for the same reason: it reverses LINES.
///
/// Otherwise the container is laid out on the same axis, unreversed, and
/// counted by reason — which keeps a `column-reverse` a column, where
/// dropping the value made it a row.
///
/// Out-of-flow (absolute, fixed) children are not flex items, and their
/// static position does not depend on their place among the items (Flexbox
/// §4.1), so they keep their positions in the child list. Text runs are
/// anonymous flex items and are reversed with the elements.
fn reverse_pass(resolved: &mut BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, dom: &mut Dom, report: &mut Report) {
    type Key = (Handle, String, String);
    let mut slots: BTreeMap<Handle, Vec<Key>> = BTreeMap::new();
    for k in resolved.keys() { slots.entry(k.0).or_default().push(k.clone()) }
    let val = |r: &BTreeMap<Key, BTreeMap<String, (Priority, String)>>, k: &Key, p: &str| r.get(k).and_then(|x| x.get(p)).map(|(_, v)| v.clone());
    let is_rev = |v: &str| v.ends_with("-reverse");
    let wraps = |v: &str| v != "nowrap" && v != "initial";
    let mut reversed: std::collections::BTreeSet<Handle> = Default::default();
    for (h, keys) in &slots {
        let n = keys.iter().filter(|k| val(resolved, k, "flex-direction").is_some_and(|v| is_rev(&v))).count();
        if n == 0 { continue }
        let base: Key = (*h, String::new(), String::new());
        let conditional = || keys.iter().filter(|k| **k != base);
        let base_flex = val(resolved, &base, "display").is_some_and(|d| d == "flex");
        let any_flex = keys.iter().any(|k| val(resolved, k, "display").is_some_and(|d| d == "flex"));
        let wrapping = keys.iter().any(|k| val(resolved, k, "flex-wrap").is_some_and(|w| wraps(&w)));
        let uniform = base_flex && val(resolved, &base, "flex-direction").is_some_and(|v| is_rev(&v))
            && conditional().all(|k| val(resolved, k, "flex-direction").is_none_or(|v| is_rev(&v))
                && val(resolved, k, "display").is_none_or(|d| d == "flex"));
        let why = if !any_flex { "flex-direction reverse on a box that is not flex (no effect)" }
            else if wrapping { "flex-direction reverse in a wrapping container (laid out unreversed)" }
            else if !uniform { "flex-direction reverse only in some states or media (laid out unreversed)" }
            else { reversed.insert(*h); report.drop_n("flex-direction reverse compiled into document order", n); continue };
        report.drop_n(why, n);
    }
    for (k, props) in resolved.iter_mut() {
        if let Some((_, v)) = props.get_mut("flex-direction") {
            if let Some(axis) = v.strip_suffix("-reverse") { *v = axis.to_string() }
        }
        let rev = reversed.contains(&k.0);
        if let Some((_, v)) = props.get_mut("justify-content") {
            let to = match (v.as_str(), rev) {
                ("start", _) | ("normal", false) | ("flex-end", true) => "flex-start",
                ("end", _) | ("normal", true) | ("flex-start", true) => "flex-end",
                _ => continue,
            };
            *v = to.to_string();
        }
    }
    for h in reversed {
        // No `justify-content` packs at main-start, which is now the end.
        resolved.entry((h, String::new(), String::new())).or_default()
            .entry("justify-content".to_string()).or_insert(((false, 0, (0, 0, 0), 0), "flex-end".to_string()));
        let kids = dom.children_of(h);
        let out_of_flow = |c: Handle| resolved.get(&(c, String::new(), String::new())).and_then(|p| p.get("position"))
            .is_some_and(|(_, v)| v == "absolute" || v == "fixed");
        let at: Vec<usize> = (0..kids.len()).filter(|i| !out_of_flow(kids[*i])).collect();
        let mut new = kids.clone();
        for (i, j) in at.iter().zip(at.iter().rev()) { new[*i] = kids[*j] }
        if let Some(n) = dom.get_mut(h) { n.children = new }
    }
}

fn clip_pass(resolved: &mut BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, report: &mut Report) {
    let keys: Vec<(Handle, String, String)> = resolved.iter()
        .filter(|(_, p)| p.contains_key("clip-marker") || p.contains_key("clip-path-marker"))
        .map(|(k, _)| k.clone()).collect();
    let mut hidden = 0usize;
    for key in keys {
        let base = (key.0, String::new(), String::new());
        let get = |r: &BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, p: &str| {
            r.get(&key).and_then(|x| x.get(p)).or_else(|| r.get(&base).and_then(|x| x.get(p))).cloned()
        };
        let positioned = get(resolved, "position").is_some_and(|(_, v)| v == "absolute" || v == "fixed");
        let clip = get(resolved, "clip-marker");
        let path = get(resolved, "clip-path-marker");
        let empty_clip = clip.as_ref().filter(|(_, v)| v == "empty" && positioned);
        let empty_path = path.as_ref().filter(|(_, v)| v == "empty");
        let base_hid = resolved.get(&base).is_some_and(|b| {
            let pos = b.get("position").is_some_and(|(_, v)| v == "absolute" || v == "fixed");
            b.get("clip-marker").is_some_and(|(_, v)| v == "empty" && pos) || b.get("clip-path-marker").is_some_and(|(_, v)| v == "empty")
        });
        let slot = resolved.get_mut(&key).expect("key from the map");
        let own_vis = slot.get("visibility").map(|(p, _)| *p);
        match empty_clip.or(empty_path) {
            Some((pri, _)) => {
                if own_vis.is_none_or(|o| o <= *pri) { slot.insert("visibility".into(), (*pri, "hidden".into())); hidden += 1 }
            }
            None if key != base && base_hid => {
                // This slot clears the clip the base slot set.
                let lifted = slot.get("clip-marker").or_else(|| slot.get("clip-path-marker")).map(|(p, _)| *p);
                if let (Some(pri), None) = (lifted, own_vis) { slot.insert("visibility".into(), (pri, "visible".into())); }
            }
            None => {}
        }
    }
    for (_, props) in resolved.iter_mut() { props.remove("clip-marker"); props.remove("clip-path-marker"); }
    if hidden > 0 { report.drop_n("empty clip compiled to `visibility: hidden`", hidden) }
}

/// Compile the icon-font marker: an element whose own winning family is a
/// ligature icon font paints nothing and takes an icon's room, a 1em square
/// (inline boxes become inline-block so the size applies). Its text is the
/// icon's NAME, not content, so hiding it loses nothing a reader would see.
fn icon_font_pass(resolved: &mut BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>, report: &mut Report) {
    let mut n = 0usize;
    for (_, props) in resolved.iter_mut() {
        let Some((pri, v)) = props.remove("icon-font-marker") else { continue };
        if v != "icon" { continue }
        let mut set = |k: &str, val: &str| {
            if props.get(k).is_none_or(|(o, _)| *o <= pri) { props.insert(k.to_string(), (pri, val.to_string())); }
        };
        set("visibility", "hidden");
        set("width", "1em");
        set("height", "1em");
        set("overflow-x", "hidden");
        set("overflow-y", "hidden");
        if props.get("display").is_none_or(|(_, d)| d == "inline") { props.insert("display".into(), (pri, "inline-block".into())); }
        n += 1;
    }
    if n > 0 { report.drop_n("ligature icon font compiled to an empty icon box", n) }
}

/// The self-alignment keywords carried, as written, to the alignment pass.
fn alignment_carried(p: &str, v: &str) -> bool {
    match p {
        "justify-self" | "justify-items" => matches!(v, "normal" | "self-start" | "self-end" | "flex-start" | "flex-end" | "left" | "right"),
        "align-self" | "align-items" => matches!(v, "normal" | "self-start" | "self-end"),
        _ => false,
    }
}

type Slots = BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>;

/// ★ THE ALIGNMENT PASS. The grammar's self-alignment keywords are `start`
/// / `end` / `center` / `stretch` (and `flex-*` / `baseline` on the block
/// axis). CSS Box Alignment has more, and most of them NAME one of those
/// once three things are known — which the cascade alone does not say:
///
/// - `flex-start` / `flex-end` on the inline axis: outside a flex container
///   they ARE `start` / `end` (§5.3), and `justify-self` does nothing inside
///   one;
/// - `left` / `right` (inline axis only): the alignment container's
///   line-left / line-right edge — `start` or `end` by the CONTAINER's
///   direction (the parent for `justify-self`, the element for
///   `justify-items`);
/// - `self-start` / `self-end`: the edge on the ITEM's own start side — the
///   container's `start` while the two directions agree, `end` where they
///   differ. On the block axis (horizontal writing only, §3.6) every box's
///   start is the top, so they are `start` / `end` — unless the axis is the
///   inline one, which is the cross axis of a COLUMN flex container. An
///   item whose direction differs from such a container's, and an `-items`
///   value over children whose directions differ from it, stay dropped:
///   one class cannot be `start` for one item and `end` for the next;
/// - `normal`: as `stretch` for a box with no preferred aspect ratio and no
///   natural size in the axis, and otherwise as `start` (Grid §6.2), or
///   `stretch` for any flex item (Flexbox §8.3). An `<img>` takes the
///   `start` reading; a NON-replaced box with an `aspect-ratio` takes it in
///   whichever axis the ratio derives — the inline axis when its height is
///   settled, the block axis when its width is (see `decide`).
///
/// That last rule is also CSS's INITIAL behaviour — `auto` takes the
/// container's `normal` — while the profile's initial is `stretch`. So an
/// image placed in a grid by a page that says nothing about alignment
/// stretched to fill its area in the profile, and kept its own size in a
/// browser. Each such image gets `start` written on it, in every media
/// context where its container is a grid (or it is absolutely positioned),
/// and `auto` back where the container is something else.
///
/// The `legacy` keywords stay dropped: `justify-items: legacy left` is
/// inherited by every descendant whose `justify-items` is not set, which a
/// non-inherited profile property cannot express.
fn alignment_pass(resolved: &mut Slots, dom: &Dom, els: &[Handle], rtl: &BTreeMap<Handle, bool>, inputs: &Inputs, report: &mut Report) {
    let is_rtl = |h: Handle| rtl.get(&h).copied().unwrap_or(false);
    let parent_el = |h: Handle| dom.get(h).and_then(|n| n.parent).filter(|p| matches!(dom.get(*p).map(|n| &n.kind), Some(Kind::Element(_))));
    let get = |r: &Slots, h: Handle, ctx: &str, p: &str| -> Option<String> {
        r.get(&(h, String::new(), ctx.to_string())).and_then(|x| x.get(p))
            .or_else(|| r.get(&(h, String::new(), String::new())).and_then(|x| x.get(p)))
            .map(|(_, v)| v.clone())
    };
    let mut ctxs: BTreeMap<Handle, std::collections::BTreeSet<String>> = BTreeMap::new();
    for (h, st, me) in resolved.keys() { if st.is_empty() { ctxs.entry(*h).or_default().insert(me.clone()); } }
    let ctxs_of = |h: Handle| -> std::collections::BTreeSet<String> {
        let mut s = ctxs.get(&h).cloned().unwrap_or_default();
        s.insert(String::new());
        s
    };
    let column_flex = |r: &Slots, h: Handle| ctxs_of(h).iter().any(|c|
        get(r, h, c, "display").as_deref() == Some("flex") && get(r, h, c, "flex-direction").is_some_and(|d| d.starts_with("column")));

    // 1. The keywords that need only directions.
    let mut mapped = 0usize;
    let mut dropped: Vec<((Handle, String, String), String, String, &'static str)> = vec![];
    let keys: Vec<(Handle, String, String)> = resolved.keys().cloned().collect();
    for k in keys {
        let h = k.0;
        for p in ["justify-self", "justify-items", "align-self", "align-items"] {
            let Some(v) = resolved.get(&k).and_then(|x| x.get(p)).map(|(_, v)| v.clone()) else { continue };
            let items = p.ends_with("-items");
            let inline = p.starts_with("justify");
            let to: Result<&str, &'static str> = match v.as_str() {
                "flex-start" if inline => Ok("start"),
                "flex-end" if inline => Ok("end"),
                "left" | "right" if inline => {
                    let container = if items { h } else { parent_el(h).unwrap_or(h) };
                    Ok(if (v == "left") != is_rtl(container) { "start" } else { "end" })
                }
                "self-start" | "self-end" => {
                    let (s, e) = if v == "self-start" { ("start", "end") } else { ("end", "start") };
                    if items {
                        let differ = dom.element_children(h).into_iter().any(|c| is_rtl(c) != is_rtl(h));
                        if !differ || (!inline && !column_flex(resolved, h)) { Ok(s) }
                        else { Err("an item's direction differs from its container's") }
                    } else {
                        let container = parent_el(h).unwrap_or(h);
                        let same = is_rtl(h) == is_rtl(container);
                        if same { Ok(s) }
                        else if inline { Ok(e) }
                        else if !column_flex(resolved, container) { Ok(s) }
                        else { Err("the item's direction differs from its column flex container's") }
                    }
                }
                _ => continue,
            };
            match to {
                Ok(t) => { if let Some((_, x)) = resolved.get_mut(&k).and_then(|x| x.get_mut(p)) { *x = t.to_string() } mapped += 1 }
                Err(why) => dropped.push((k.clone(), p.to_string(), v.clone(), why)),
            }
        }
    }
    for (k, p, v, why) in dropped {
        if let Some(x) = resolved.get_mut(&k) { x.remove(&p); }
        report.drop(format!("value `{p}: {v}` ({why})"));
    }

    // 2. `normal`, and the initial behaviour it shares, by box type.
    // ★ Only a REPLACED box. A non-replaced one with an `aspect-ratio` is
    // not laid out as `start` by browsers: Chrome stretches it in the block
    // axis against a definite row and derives the width from that, and
    // stretches the width where the row is indefinite — neither is `start`,
    // and the profile's `stretch` is the nearer of the two.
    let ratio_box = |_r: &Slots, h: Handle, _ctx: &str, inline: bool| -> bool {
        if dom.tag(h) != Some("img") { return false }
        let Some(n) = dom.attr(h, "src").and_then(|s| inputs.images.get(s)) else { return false };
        n.ratio.is_some() || (n.width.is_some() && n.height.is_some()) || if inline { n.width.is_some() } else { n.height.is_some() }
    };
    let auto_ish = |v: &Option<String>| v.as_deref().is_none_or(|v| matches!(v, "auto" | "initial"));
    let normal_ish = |v: &Option<String>| v.as_deref().is_none_or(|v| matches!(v, "normal" | "initial"));
    let low: Priority = (false, 0, (0, 0, 0), 0);
    let mut writes: Vec<((Handle, String, String), &'static str, &'static str)> = vec![];
    let mut compiled = 0usize;
    for h in els {
        let h = *h;
        let parent = parent_el(h);
        for (own_p, items_p) in [("justify-self", "justify-items"), ("align-self", "align-items")] {
            let inline = own_p == "justify-self";
            // What `h` must say in one context (None: what the profile does
            // with nothing said is already right).
            let decide = |r: &Slots, ctx: &str, own: &Option<String>| -> Option<&'static str> {
                let eff_normal = own.as_deref() == Some("normal")
                    || (auto_ish(own) && normal_ish(&parent.and_then(|p| get(r, p, ctx, items_p))));
                if !eff_normal { return None }
                let is_normal = own.as_deref() == Some("normal");
                let display = parent.and_then(|p| get(r, p, ctx, "display"));
                // ★★ A NON-replaced box with an `aspect-ratio` in a grid:
                // `normal` stretches ONE axis and the ratio gives the other
                // (Sizing 4 §5.1, Grid §6.2). Which one, measured in Chrome
                // (2026-11-05; WPT grid-aspect-ratio-007, -010, -028, -030,
                // -038):
                // - the HEIGHT is settled — specified, or an explicit
                //   `stretch` — so the width is the ratio-dependent axis and
                //   `justify-self: normal` is `start` (height 100px in a
                //   200px column: 100×100, not 200×100);
                // - otherwise the WIDTH is: specified, explicitly stretched,
                //   or stretched by its own `normal` — so `align-self:
                //   normal` is `start` (a 1/1 box in a 200px column and a
                //   100px row: 200×200, overflowing the row — round 17 left
                //   this one `stretch`, 200×100);
                // - an explicit non-stretch `justify-self` (center, start…)
                //   leaves the width unsettled, and then Chrome stretches the
                //   block axis instead (100px row: 100×100, centred), which
                //   is the profile's `stretch` — nothing to write.
                if display.as_deref() == Some("grid") && dom.tag(h) != Some("img")
                    && get(r, h, ctx, "aspect-ratio").is_some_and(|v| !matches!(v.as_str(), "auto" | "initial")) {
                    let sized = |p: &str| get(r, h, ctx, p).is_some_and(|v| !matches!(v.as_str(), "auto" | "initial" | "min-content" | "max-content" | "fit-content"));
                    // What an axis's own alignment says once `auto` takes
                    // the container's `-items` (None: nothing said, so
                    // `normal`).
                    let says = |own: &str, items: &str| -> Option<String> {
                        match get(r, h, ctx, own) {
                            Some(v) if !matches!(v.as_str(), "auto" | "initial") => Some(v),
                            _ => parent.and_then(|p| get(r, p, ctx, items)).filter(|v| !matches!(v.as_str(), "initial")),
                        }
                    };
                    let a_says = says("align-self", "align-items");
                    let j_says = says("justify-self", "justify-items");
                    // ★ A PERCENTAGE height settles nothing unless the row it
                    // resolves against is definite: in an auto row Chrome
                    // treats it as `auto` and stretches the width instead
                    // (`height: 100%` in a 100px column: 100×100, not 0×0;
                    // in a 100px row, `height: 50%` gives 50×50). Definite
                    // here means every row the item can land in is a fixed
                    // length — explicit px rows, with px implicit rows or no
                    // more children than explicit cells.
                    let rows_definite = || -> bool {
                        let Some(p) = parent else { return false };
                        let px_tracks = |v: &str| -> Option<usize> {
                            let t: Vec<&str> = v.split_whitespace().collect();
                            (!t.is_empty() && t.iter().all(|x| x.strip_suffix("px").is_some_and(|n| n.parse::<f64>().is_ok()))).then_some(t.len())
                        };
                        let Some(rows) = get(r, p, ctx, "grid-template-rows").as_deref().and_then(px_tracks) else { return false };
                        if get(r, p, ctx, "grid-auto-rows").as_deref().and_then(px_tracks).is_some() { return true }
                        let cols = get(r, p, ctx, "grid-template-columns").map(|v| v.split_whitespace().count().max(1)).unwrap_or(1);
                        dom.element_children(p).len() <= rows * cols
                    };
                    let h_set = sized("height") && (!get(r, h, ctx, "height").is_some_and(|v| v.contains('%')) || rows_definite());
                    let h_settled = h_set || a_says.as_deref() == Some("stretch");
                    if inline {
                        if !sized("width") && h_settled { return Some("start") }
                    } else {
                        let j_stretches = match j_says.as_deref() { None | Some("normal" | "stretch") => true, _ => false };
                        if !h_set && (sized("width") || j_stretches) { return Some("start") }
                    }
                }
                if !ratio_box(r, h, ctx, inline) { return is_normal.then_some("stretch") }
                // Only a grid's items need it written: a flex item's
                // `normal` is `stretch` like the profile's initial, and a
                // block's child or an out-of-flow box outside a grid is not
                // aligned by these properties at all.
                if display.as_deref() == Some("grid") { return Some("start") }
                let abs = get(r, h, ctx, "position").is_some_and(|v| v == "absolute" || v == "fixed");
                let flex_item = !abs && display.as_deref() == Some("flex");
                is_normal.then_some(if !inline && flex_item { "stretch" } else { "start" })
            };
            let mut all = ctxs_of(h);
            if let Some(p) = parent { all.extend(ctxs_of(p)) }
            let base_own = get(resolved, h, "", own_p);
            let base = decide(resolved, "", &base_own);
            if let Some(v) = base { writes.push(((h, String::new(), String::new()), own_p, v)) }
            for ctx in all.iter().filter(|c| !c.is_empty()) {
                let own = get(resolved, h, ctx, own_p);
                match (decide(resolved, ctx, &own), base) {
                    (Some(v), _) => if Some(v) != base || resolved.get(&(h, String::new(), ctx.clone())).is_some_and(|x| x.contains_key(own_p)) {
                        writes.push(((h, String::new(), ctx.clone()), own_p, v))
                    },
                    (None, Some(_)) if auto_ish(&own) => writes.push(((h, String::new(), ctx.clone()), own_p, "auto")),
                    _ => {}
                }
            }
            // A state slot that says `normal` itself.
            // ★ A RANGE, not a scan: the slots are ordered by handle first, so
            // `h`'s are contiguous. Filtering every key for every element was
            // quadratic — 27 s of a 2.1 MB manual's 27 s normalize, 48,322
            // elements (legacy-web §5.4.5.7). Same keys, same order.
            let states: Vec<(Handle, String, String)> = resolved.range((h, String::new(), String::new())..)
                .map(|(k, _)| k).take_while(|k| k.0 == h).filter(|k| !k.1.is_empty()).cloned().collect();
            for k in states {
                if resolved.get(&k).and_then(|x| x.get(own_p)).is_some_and(|(_, v)| v == "normal") {
                    let v = decide(resolved, &k.2, &Some("normal".into())).unwrap_or("stretch");
                    writes.push((k, own_p, v));
                }
            }
        }
    }
    for (k, p, v) in writes {
        let base_pri = resolved.get(&(k.0, String::new(), String::new())).and_then(|x| x.get(p)).map(|(q, _)| *q);
        let slot = resolved.entry(k.clone()).or_default();
        let was = slot.get(p).map(|(q, v)| (*q, v.clone()));
        if was.as_ref().is_none_or(|(_, w)| w != "normal") && v == "start" { compiled += 1 }
        let pri = [was.map(|(q, _)| q), base_pri, Some(low)].into_iter().flatten().max().expect("low");
        slot.insert(p.to_string(), (pri, v.to_string()));
    }

    // 3. A container's own `normal`: what its auto items do with it is now
    // written on the ones that differ, so for the rest it is `stretch`.
    for props in resolved.values_mut() {
        for p in ["justify-items", "align-items", "justify-self", "align-self"] {
            if let Some((_, v)) = props.get_mut(p) { if v == "normal" { *v = "stretch".into(); mapped += 1 } }
        }
    }
    if mapped > 0 { report.drop_n("self-alignment keyword mapped to the admitted one it names", mapped) }
    if compiled > 0 { report.drop_n("image / aspect-ratio grid item's `normal` alignment compiled to `start`", compiled) }
}
