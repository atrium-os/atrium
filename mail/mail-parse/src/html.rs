//! HTML mail into an Atrium document (docs/spec/mail.md §7.3, §7.4).
//!
//! Three passes, all in the parse jail, none with any capability:
//!
//! 1. **Strip and rewrite** (here, on the raw DOM). Everything that could act
//!    or fetch goes: scripts, frames, objects, embeds, media, `<meta>`,
//!    `<base>`, `<link>`, forms and their controls, `srcset`. Every reference
//!    to a picture — `<img src>`, the `background` attribute, CSS `url()` in
//!    `<style>` and `style=` — becomes an opaque resource id, listed once with
//!    where it came from (a `cid:` part, a remote URL, a `data:` URL). Links
//!    keep only `https`, `http`, `mailto` and in-document `#` targets.
//! 2. **Normalize** with `navigator-normalize`: the cascade is resolved, the
//!    output is one flat class per declaration block and the document profile
//!    holds. A picture's natural size is declared from what the HTML says
//!    (`width`/`height`), or from the real size once it has been loaded, so
//!    the space is kept and nothing jumps when it arrives.
//! 3. **Find the hidden pictures** on the normalized document, where the
//!    cascade is already resolved, so a pixel hidden by a class in a `<style>`
//!    block is caught as surely as one hidden by `style=`. A remote picture
//!    that is hidden, tiny (≤ 3 px either way) or off the page is removed from
//!    the document and marked `hidden`: Mail never fetches it, even after
//!    "Load pictures".

use navigator_dom::{Dom, Handle};
use navigator_normalize::{Budget, ImageSize, Inputs};
pub use navigator_render::fontset::FontSet;
pub use navigator_style::cascade::Env;
use std::collections::BTreeMap;

/// Where a resource's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A MIME part of this message, by its `Content-ID` (without `<>`).
    Cid(String),
    /// A remote URL: fetched only through Nuntius, only when the person loads
    /// pictures, never when `hidden`.
    Remote(String),
    /// A `data:` URL carried in the HTML itself (no fetch, no tracking).
    Data(String),
}

/// What the document uses a resource for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// An `<img>`.
    Image,
    /// A background picture (`background=` or a CSS `background*`).
    Background,
    /// Any other CSS `url()` (fonts, cursors, list markers): never fetched,
    /// because layout uses the system fonts and the inline CSS only (§7.4).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// The opaque id the document refers to (`src`, `url()`), which the
    /// viewer's `Provide { id, blob }` answers.
    pub id: String,
    pub source: Source,
    pub used_as: Use,
    /// A tracking pixel or a hidden picture: never fetched.
    pub hidden: bool,
}

impl Resource {
    /// May the engine fetch this one when the person presses "Load pictures"?
    pub fn loadable(&self) -> bool {
        matches!(self.source, Source::Remote(_)) && !self.hidden && self.used_as != Use::Other
    }
}

#[derive(Debug, Default)]
pub struct Converted {
    /// The profile document, ready for a `doc-viewer` load.
    pub document: String,
    pub resources: Vec<Resource>,
    /// What could not be represented or was removed, by reason, with counts
    /// (the normalizer's report plus this module's own removals).
    pub dropped: BTreeMap<String, usize>,
    /// Each link's destination and visible text, in document order (§7.6).
    pub links: Vec<(String, String)>,
}

/// The most links listed, and the visible text kept of each.
pub const LINKS_MAX: usize = 1024;
pub const LINK_TEXT_MAX: usize = 200;

impl Converted {
    /// Remote pictures not loaded, for the bar's "12 pictures not loaded".
    pub fn remote_pictures(&self) -> usize { self.resources.iter().filter(|r| r.loadable()).count() }
}

/// Mail's work budget: a message is not a web app. Generous for the heaviest
/// newsletter, small enough that a hostile one costs little.
pub fn budget() -> Budget {
    Budget { css_bytes: 2 << 20, elements: 50_000, match_steps: 50_000_000, var_tokens: 4096, var_steps: 4_000_000 }
}

/// Resource ids look like this, so the hidden-picture pass can tell them
/// from anything else.
const ID_PREFIX: &str = "mail-res:";

/// Convert one HTML body. `loaded` gives the real natural size of pictures
/// already fetched (by resource id), for the conversion done after "Load
/// pictures"; on first conversion it is empty. Ids are assigned in document
/// order, so the same HTML gives the same ids every time.
///
/// Tables are measured offline with the pinned font set (§3.13 of the
/// profile), which is why it is an argument: the jail loads it once.
pub fn convert(html: &str, loaded: &BTreeMap<String, (u32, u32)>, fonts: &FontSet, env: &Env) -> Converted {
    let mut out = Converted::default();
    let mut dom = navigator_dom::parse(html);
    let mut res = Resources::default();
    strip(&mut dom, &mut out.dropped);
    rewrite(&mut dom, &mut res);

    // Declared sizes: the real one when loaded, else what the HTML says, else
    // nothing (the profile's 300 × 150 default object size, kept as space).
    let mut inputs = Inputs { budget: budget(), ..Inputs::default() };
    for (id, size) in &res.declared {
        let n = match loaded.get(id) {
            Some(&(w, h)) => ImageSize::from((w, h)),
            None => *size,
        };
        inputs.images.insert(id.clone(), n);
    }
    // A loaded picture with no declared size (a background): its real one.
    for (id, &(w, h)) in loaded {
        inputs.images.entry(id.clone()).or_insert_with(|| ImageSize::from((w, h)));
    }
    let (doc, report) = navigator_normalize::normalize_and_measure(&dom.serialize(), &inputs, fonts, env);
    for (k, v) in report.dropped { *out.dropped.entry(k).or_default() += v }

    // Pass 3, on the normalized document.
    let mut doc_dom = navigator_dom::parse(&doc);
    let classes = class_rules(&doc_dom);
    let mut hidden = Vec::new();
    for h in doc_dom.by_tag_anywhere("img") {
        let Some(src) = doc_dom.attr(h, "src").map(str::to_string) else { continue };
        if !src.starts_with(ID_PREFIX) { continue }
        if is_hidden(&doc_dom, h, &classes) { hidden.push((h, src)) }
    }
    for (h, src) in hidden {
        doc_dom.detach_for_move(h);
        if let Some(r) = res.list.iter_mut().find(|r| r.id == src) {
            if matches!(r.source, Source::Remote(_)) {
                r.hidden = true;
                *out.dropped.entry("hidden or tiny remote picture (tracking pixel; never fetched)".into()).or_default() += 1;
            }
        }
    }
    // A remote reference used again visibly elsewhere stays loadable only if
    // some visible use remains; ids are per reference, so nothing to merge.
    for a in doc_dom.by_tag_anywhere("a").into_iter().take(LINKS_MAX) {
        let Some(href) = doc_dom.attr(a, "href").map(str::to_string) else { continue };
        if href.len() > 8192 { continue }
        let text: String = doc_dom.text_content(a).split_whitespace().collect::<Vec<_>>().join(" ").chars().take(LINK_TEXT_MAX).collect();
        out.links.push((href, text));
    }
    out.document = doc_dom.serialize();
    // A picture the normalizer left out altogether (inside `display: none`,
    // in a dropped element) is not shown, so it is never fetched either. Not
    // a background: that is left out until its size is known, which only
    // loading it tells (the document is then converted again).
    for r in res.list.iter_mut() {
        if matches!(r.source, Source::Remote(_)) && r.used_as == Use::Image && !r.hidden && !mentions(&out.document, &r.id) {
            r.hidden = true;
            *out.dropped.entry("hidden or tiny remote picture (tracking pixel; never fetched)".into()).or_default() += 1;
        }
    }
    out.resources = res.list;
    out
}

// ---- pass 1: strip ----

fn strip(dom: &mut Dom, dropped: &mut BTreeMap<String, usize>) {
    let mut note = |why: &str| *dropped.entry(why.to_string()).or_default() += 1;
    // Gone with their content.
    for tag in ["script", "noscript", "iframe", "frame", "frameset", "object", "embed", "applet", "audio", "video",
                "source", "track", "meta", "base", "link", "title", "template", "portal", "input", "select",
                "textarea", "dialog"] {
        for h in dom.by_tag_anywhere(tag) {
            if dom.connected(h) { dom.detach_for_move(h); note(&format!("<{tag}> (not in mail)")) }
        }
    }
    // Gone, their content kept.
    for tag in ["form", "button", "picture", "label", "fieldset"] {
        for h in dom.by_tag_anywhere(tag) {
            if dom.connected(h) { unwrap(dom, h) }
        }
    }
    // SVG references outside the document.
    for tag in ["image", "use", "feImage"] {
        for h in dom.by_tag_anywhere(tag) {
            if !dom.is_foreign(h) { continue }
            let href = dom.attr(h, "href").or_else(|| dom.attr(h, "xlink:href")).unwrap_or("");
            if !href.starts_with('#') { dom.detach_for_move(h); note("SVG reference outside the message") }
        }
    }
    for h in all_elements(dom) {
        // Event handlers do nothing without a script engine, but they are
        // noise, and `srcset` is a second, unlisted picture source.
        let names: Vec<String> = dom.get(h).map(|n| n_attrs(n)).unwrap_or_default();
        for a in names {
            let l = a.to_ascii_lowercase();
            if l.starts_with("on") || matches!(l.as_str(), "srcset" | "imagesrcset" | "ping" | "formaction" | "action" | "poster" | "lowsrc" | "dynsrc" | "longdesc") {
                dom.remove_attr(h, &a);
            }
        }
        // Links: the link bar handles these (§7.6); other schemes go.
        if let Some(href) = dom.attr(h, "href").map(str::to_string) {
            if !dom.is_foreign(h) && !link_ok(&href) { dom.remove_attr(h, "href"); note("link with a scheme Mail does not open") }
        }
    }
}

fn link_ok(href: &str) -> bool {
    let t = href.trim();
    if t.starts_with('#') { return true }
    let lower = t.to_ascii_lowercase();
    ["https:", "http:", "mailto:"].iter().any(|s| lower.starts_with(s))
}

fn unwrap(dom: &mut Dom, h: Handle) {
    let Some(p) = dom.get(h).and_then(|n| n.parent) else { return };
    for c in dom.children_of(h) {
        dom.detach_for_move(c);
        dom.insert_before(p, c, Some(h));
    }
    dom.detach_for_move(h);
}

fn all_elements(dom: &Dom) -> Vec<Handle> {
    let mut out = Vec::new();
    let mut stack = vec![dom.root()];
    while let Some(h) = stack.pop() {
        for c in dom.element_children(h).into_iter().rev() { out.push(c); stack.push(c) }
    }
    out
}

fn n_attrs(n: &navigator_dom::Node) -> Vec<String> {
    n.attrs.iter().map(|(k, _)| k.to_string()).collect()
}

// ---- pass 1: rewrite references ----

#[derive(Default)]
struct Resources {
    list: Vec<Resource>,
    /// id → the natural size the HTML declares, for `<img>` only.
    declared: Vec<(String, ImageSize)>,
}

impl Resources {
    fn add(&mut self, url: &str, used_as: Use) -> Option<String> {
        let t = url.trim();
        if t.is_empty() || t.starts_with('#') { return None }
        let lower = t.to_ascii_lowercase();
        let source = if let Some(c) = lower.strip_prefix("cid:") {
            // Content-IDs compare as written, minus the scheme; percent-escapes decoded (RFC 2392).
            let _ = c;
            Source::Cid(percent_decode(&t[4..]).trim_matches(['<', '>']).to_string())
        } else if lower.starts_with("data:") {
            Source::Data(t.to_string())
        } else if lower.starts_with("https:") || lower.starts_with("http:") || lower.starts_with("//") {
            Source::Remote(if lower.starts_with("//") { format!("https:{t}") } else { t.to_string() })
        } else {
            return None; // relative or another scheme: nothing it could name
        };
        let id = format!("{ID_PREFIX}{}", self.list.len());
        self.list.push(Resource { id: id.clone(), source, used_as, hidden: false });
        Some(id)
    }
}

fn rewrite(dom: &mut Dom, res: &mut Resources) {
    for h in all_elements(dom) {
        if dom.is_foreign(h) { continue }
        let tag = dom.tag(h).unwrap_or("").to_ascii_lowercase();
        if tag == "img" {
            let src = dom.attr(h, "src").unwrap_or("").to_string();
            match res.add(&src, Use::Image) {
                Some(id) => {
                    dom.set_attr(h, "src", &id);
                    res.declared.push((id, declared_size(dom, h)));
                }
                None => { dom.detach_for_move(h); }
            }
        }
        if let Some(bg) = dom.attr(h, "background").map(str::to_string) {
            dom.remove_attr(h, "background");
            if let Some(id) = res.add(&bg, Use::Background) {
                let style = dom.attr(h, "style").unwrap_or("").to_string();
                dom.set_attr(h, "style", &format!("background-image:url({id});{style}"));
            }
        }
        if let Some(style) = dom.attr(h, "style").map(str::to_string) {
            let new = rewrite_css(&style, res);
            if new != style { dom.set_attr(h, "style", &new) }
        }
    }
    for h in dom.by_tag_anywhere("style") {
        let css = dom.text_content(h);
        let new = rewrite_css(&css, res);
        if new != css { dom.set_text(h, &new) }
    }
}

/// The natural size an `<img>` declares with plain pixel attributes.
fn declared_size(dom: &Dom, h: Handle) -> ImageSize {
    let px = |a: &str| dom.attr(h, a).and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok()).filter(|v| *v <= 16_384);
    let (w, ht) = (px("width"), px("height"));
    match (w, ht) {
        (Some(w), Some(ht)) => ImageSize::from((w, ht)),
        _ => ImageSize { width: w, height: ht, ratio: None },
    }
}

/// Every `url(...)` (and `@import "..."`) in a piece of CSS becomes a
/// resource id; `@import` is removed, since remote CSS is never fetched.
fn rewrite_css(css: &str, res: &mut Resources) -> String {
    let mut out = String::with_capacity(css.len());
    let lower = css.to_ascii_lowercase();
    let mut i = 0;
    while i < css.len() {
        if lower[i..].starts_with("@import") {
            // To the end of the statement.
            let end = lower[i..].find(';').map(|e| i + e + 1).unwrap_or(css.len());
            i = end;
            continue;
        }
        if lower[i..].starts_with("url(") {
            let start = i + 4;
            let Some(close) = css[start..].find(')') else { out.push_str(&css[i..]); break };
            let inner = css[start..start + close].trim().trim_matches(['"', '\'']);
            let used_as = if property_before(&lower[..i]).starts_with("background") { Use::Background } else { Use::Other };
            match res.add(inner, used_as) {
                Some(id) => { out.push_str("url("); out.push_str(&id); out.push(')') }
                None => out.push_str("none"),
            }
            i = start + close + 1;
            continue;
        }
        let ch = css[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The property name a declaration value belongs to: the identifier before
/// the nearest `:` that is not past a `;`, `{` or `}`.
fn property_before(prefix: &str) -> &str {
    let stmt = prefix.rfind([';', '{', '}']).map(|p| &prefix[p + 1..]).unwrap_or(prefix);
    stmt.split(':').next().unwrap_or("").trim()
}

/// The bytes a `data:` URL carries (base64 or percent-encoded), when at
/// most `cap`.
pub fn data_url(s: &str, cap: usize) -> Option<Vec<u8>> {
    let rest = s.get(..5).filter(|p| p.eq_ignore_ascii_case("data:")).map(|_| &s[5..])?;
    let (meta, payload) = rest.split_once(',')?;
    let bytes = if meta.trim().to_ascii_lowercase().ends_with(";base64") {
        crate::mime::decode_body(payload.as_bytes(), "base64", cap.saturating_add(1))
    } else {
        let b = payload.as_bytes();
        let mut out = Vec::with_capacity(b.len().min(cap + 1));
        let mut i = 0;
        while i < b.len() && out.len() <= cap {
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            if b[i] == b'%' && i + 2 < b.len() {
                if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) { out.push(h << 4 | l); i += 3; continue }
            }
            out.push(b[i]);
            i += 1;
        }
        out
    };
    (bytes.len() <= cap).then_some(bytes)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Byte by byte: a `%` before a multi-byte character must not slice it.
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) { out.push(h << 4 | l); i += 3; continue }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- pass 3: hidden pictures ----

/// `.class{decls}` rules of the normalized document's one sheet. The
/// normalizer emits flat class rules; anything inside an at-rule (dark mode)
/// is skipped, since hiding a pixel only in dark mode is still hiding it in
/// light mode's absence and the light rules decide.
fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(at) = rest.find("/*") {
        out.push_str(&rest[..at]);
        out.push(' ');
        rest = match rest[at + 2..].find("*/") { Some(e) => &rest[at + 2 + e + 2..], None => "" };
    }
    out.push_str(rest);
    out
}

fn class_rules(dom: &Dom) -> BTreeMap<String, Vec<(String, String)>> {
    let mut map = BTreeMap::new();
    for h in dom.by_tag_anywhere("style") {
        // Comments out first: the normalizer opens its block with one, which
        // would otherwise hide the first rule's selector.
        let css = strip_comments(&dom.text_content(h));
        let mut depth = 0usize;
        let mut sel_start = 0usize;
        let bytes = css.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => {
                    let sel = css[sel_start..i].trim();
                    if depth == 0 && sel.starts_with('.') && !sel.contains([' ', ',', ':', '>', '[']) {
                        if let Some(end) = css[i + 1..].find('}') {
                            let body = &css[i + 1..i + 1 + end];
                            let decls = body.split(';').filter_map(|d| {
                                let (k, v) = d.split_once(':')?;
                                Some((k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()))
                            }).collect::<Vec<_>>();
                            map.entry(sel[1..].to_string()).or_insert_with(Vec::new).extend(decls);
                            i = i + 1 + end + 1;
                            sel_start = i;
                            continue;
                        }
                    }
                    depth += 1;
                    sel_start = i + 1;
                }
                b'}' => { depth = depth.saturating_sub(1); sel_start = i + 1 }
                b';' if depth == 0 => sel_start = i + 1,
                _ => {}
            }
            i += 1;
        }
    }
    map
}

/// `id` appears in `doc` as a whole id (`mail-res:1` is not in `mail-res:12`).
fn mentions(doc: &str, id: &str) -> bool {
    doc.match_indices(id).any(|(at, _)| !doc[at + id.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

fn decls_of<'a>(dom: &Dom, h: Handle, classes: &'a BTreeMap<String, Vec<(String, String)>>) -> Vec<&'a (String, String)> {
    let c = dom.attr(h, "class").unwrap_or("");
    c.split_ascii_whitespace().filter_map(|n| classes.get(n)).flatten().collect()
}

fn px(v: &str) -> Option<f64> {
    let v = v.trim();
    let n = v.strip_suffix("px").unwrap_or(v);
    if n.is_empty() { return None }
    n.parse::<f64>().ok().filter(|_| v.ends_with("px") || v.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-'))
}

fn is_hidden(dom: &Dom, img: Handle, classes: &BTreeMap<String, Vec<(String, String)>>) -> bool {
    // The picture's own size: attributes (its declared natural size) and CSS.
    let attr = |a: &str| dom.attr(img, a).and_then(|v| v.trim().parse::<f64>().ok());
    if attr("width").is_some_and(|w| w <= 3.0) || attr("height").is_some_and(|h| h <= 3.0) { return true }
    for (k, v) in decls_of(dom, img, classes) {
        let small = matches!(k.as_str(), "width" | "height" | "max-width" | "max-height") && px(v).is_some_and(|n| n <= 3.0);
        if small { return true }
    }
    // It and every ancestor: hidden, transparent, or placed off the page.
    let mut h = Some(img);
    while let Some(e) = h {
        for (k, v) in decls_of(dom, e, classes) {
            let hide = match k.as_str() {
                "display" => v == "none",
                "visibility" => v == "hidden" || v == "collapse",
                "opacity" => v.trim_end_matches('%').parse::<f64>().is_ok_and(|o| o <= 0.01),
                "left" | "top" | "margin-left" | "margin-top" | "text-indent" => px(v).is_some_and(|n| n <= -1000.0),
                "max-height" | "max-width" | "height" | "width" if e != img => px(v).is_some_and(|n| n <= 0.0)
                    && decls_of(dom, e, classes).iter().any(|(k, v)| k == "overflow" && v == "hidden"),
                _ => false,
            };
            if hide { return true }
        }
        if dom.attr(e, "hidden").is_some() { return true }
        h = dom.get(e).and_then(|n| n.parent).filter(|p| *p != dom.root());
    }
    false
}
