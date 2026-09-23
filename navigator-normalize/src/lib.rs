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
}

/// One element's resolved declarations, in cascade order.
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Block(Vec<(String, String)>);

/// The winning declaration for a property: (important, specificity, order).
type Priority = (bool, (u32, u32, u32), usize);

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
                    let value = substitute(&d.value, scope, 0);
                    for (prop, v) in to_longhands(&d.name, &value, inputs, &mut report) {
                        let pri: Priority = (d.important, spec, rule.order);
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
                                block.insert(prop, ((d.important, spec, rule.order), v));
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
                let pri: Priority = (d.important, (u32::MAX, 0, 0), usize::MAX);
                slot.insert(prop, (pri, v));
            }
        }
    }

    // 5. Emit: one class per distinct block, so elements that resolved to the
    //    same declarations share a rule instead of each getting their own.
    let mut blocks: BTreeMap<(String, String, Block), String> = BTreeMap::new();
    let mut per_element: BTreeMap<Handle, Vec<String>> = BTreeMap::new();
    for ((h, state, media), props) in resolved {
        let mut b: Vec<(String, String)> = props.into_iter().map(|(p, (_, v))| (p, v)).collect();
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
                    let pri: Priority = (d.important, spec, rule.order);
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
    let q = q.trim_start_matches("only ").trim().to_string();
    let q = q.strip_prefix("screen and ").unwrap_or(&q).trim().to_string();
    let q = q.strip_prefix("all and ").unwrap_or(&q).trim().to_string();
    if q == "screen" || q == "all" || q.is_empty() { return Some(String::new()) }
    if !q.starts_with('(') { return None }
    // Only the features the profile's own parser admits.
    let inner = q.trim_matches(|c| c == '(' || c == ')');
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
                None => Some(q),
            }
        }
        "prefers-color-scheme" | "prefers-reduced-motion" | "orientation" => Some(q),
        _ => None,
    }
}

/// `calc(640px - 1px)` and friends, in absolute px. Anything else — a
/// percentage, a font-relative unit, a division by a length — is not a
/// constant and returns `None`.
fn fold_px(v: &str) -> Option<f64> {
    let inner = v.trim().strip_prefix("calc(")?.strip_suffix(')')?;
    let mut total = 0.0f64;
    let mut sign = 1.0f64;
    for tok in inner.split_whitespace() {
        match tok {
            "+" => sign = 1.0,
            "-" => sign = -1.0,
            t => {
                let n: f64 = t.strip_suffix("px").or(Some(t).filter(|x| **x == *"0"))?.parse().ok()?;
                total += sign * n;
            }
        }
    }
    Some(total)
}
