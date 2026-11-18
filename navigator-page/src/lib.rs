//! The doc-viewer child's work that needs no window (limen.md §2.1, L6):
//! read what the parent handed over, bounded, and turn a recording into
//! the normalized document `DocumentView` lays out.
//!
//! Everything here reads hostile bytes: the recording came from the
//! converter, whose input was a web page. The child is jailed with no
//! network and no files (only the descriptor it was handed and its
//! frescod connection), so what a bug here can reach is this one page.

pub mod print;

use std::collections::BTreeMap;
use std::io::Read;

use limen::doc_viewer::{error, DocKind, DOC_MAX, TITLE_MAX};
use navigator_render::Subresources;

/// A document ready to lay out.
pub struct Prepared {
    /// Normalized, pre-measured HTML.
    pub html: String,
    pub subs: Subresources,
    /// The page's `<title>`, cut to `TITLE_MAX` (empty when it has none).
    pub title: String,
    /// The `id`s the page's fragments can name (for `ScrollTo`), at most
    /// `ANCHORS_MAX`.
    pub anchors: Vec<String>,
    /// The headings a reader can jump to (navigator-ui.md §7 "Jump to
    /// section"): (anchor, heading text), at most `SECTIONS_MAX`.
    pub sections: Vec<(String, String)>,
    /// The document's plain forms ([`forms`]).
    pub forms: Vec<limen::doc_viewer::Form>,
}

/// Read all of `r`, refusing more than `DOC_MAX` bytes.
pub fn read_bounded(r: impl Read) -> Result<Vec<u8>, u16> {
    let mut b = Vec::new();
    r.take(DOC_MAX + 1).read_to_end(&mut b).map_err(|_| error::UNREADABLE)?;
    if b.len() as u64 > DOC_MAX {
        return Err(error::TOO_BIG);
    }
    Ok(b)
}

/// The largest picture decoded, per axis: more than a large screen shows.
pub const PICTURE_PX_MAX: u32 = 2048;
/// An SVG smaller than this on its longer side is drawn at this size, so a
/// small icon stays sharp when it is shown large.
pub const SVG_PX_MIN: u32 = 1024;

/// A picture (`DocKind::Picture`) decoded here, in the child's jail, under
/// the same decoders and ceilings as a page's images, reduced to at most
/// [`PICTURE_PX_MAX`] per axis.
pub fn picture(bytes: &[u8]) -> Result<navigator_image::image::Pixels, u16> {
    use navigator_image::image::{decode_unaddressed, sniff, Ceilings, DecodeError, Format};
    let format = sniff(bytes).map_err(|_| error::UNREADABLE)?;
    let at = |target| decode_unaddressed(bytes, Ceilings { target, ..Ceilings::default() });
    let px = if format == Format::Svg {
        // Vectors have no natural pixel size to reduce from: draw at its
        // own size, then again larger or smaller to fit the range.
        let scaled = |w: u32, h: u32, to: u32| {
            let (w, h) = (w as u64, h as u64);
            let m = w.max(h).max(1);
            (((w * to as u64) / m).max(1) as u32, ((h * to as u64) / m).max(1) as u32)
        };
        match at(None) {
            Ok(px) if px.width.max(px.height) < SVG_PX_MIN => at(Some(scaled(px.width, px.height, SVG_PX_MIN))),
            Ok(px) if px.width.max(px.height) > PICTURE_PX_MAX => at(Some(scaled(px.width, px.height, PICTURE_PX_MAX))),
            Err(DecodeError::TooLarge { width, height, .. }) => at(Some(scaled(width, height, PICTURE_PX_MAX))),
            other => other,
        }
    } else {
        // The decoders reduce to cover their target, so ask for the box
        // the picture fits in whole, not a square.
        let (_, w, h) = navigator_image::image::natural_size(bytes).map_err(|_| error::UNREADABLE)?;
        let m = w.max(h) as u64;
        let target = if m <= PICTURE_PX_MAX as u64 { None } else {
            let t = |v: u32| ((v as u64 * PICTURE_PX_MAX as u64).div_ceil(m)).max(1) as u32;
            Some((t(w), t(h)))
        };
        at(target)
    };
    px.map_err(|e| {
        eprintln!("navigator-page: picture not shown: {e:?}");
        error::UNREADABLE
    })
}

/// Where a `iw` x `ih` picture goes in a `w` x `h` window: whole, centred,
/// never drawn larger than its own pixels (an SVG was drawn large already).
pub fn fit_picture(iw: u32, ih: u32, w: f32, h: f32) -> [f32; 4] {
    let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
    let s = (w / iw).min(h / ih).min(1.0).max(0.0);
    let (pw, ph) = ((iw * s).round(), (ih * s).round());
    [((w - pw) / 2.0).floor(), ((h - ph) / 2.0).floor(), pw, ph]
}

/// A recording (or, for `DocKind::Html`, a page's own HTML) as a
/// normalized document.
pub fn prepare(kind: DocKind, bytes: &[u8]) -> Result<Prepared, u16> {
    prepare_with(kind, bytes, &BTreeMap::new())
}

/// The sizes the normalizer is told for an HTML document's pictures: what
/// each `<img>` declares (`width`, `height`, `natural-ratio`, as a mail
/// body comes from Mail's parse jail), or the real size of a picture the
/// parent provided. A picture with neither is dropped, as before.
fn html_image_sizes(doc: &str, provided: &BTreeMap<String, (u32, u32)>) -> BTreeMap<String, navigator_normalize::ImageSize> {
    use navigator_normalize::ImageSize;
    let dom = navigator_dom::parse(doc);
    let mut out = BTreeMap::new();
    for h in dom.by_tag_anywhere("img") {
        let Some(src) = dom.attr(h, "src").filter(|s| !s.is_empty()) else { continue };
        if let Some(&wh) = provided.get(src) {
            out.insert(src.to_string(), ImageSize::from(wh));
            continue;
        }
        let px = |a: &str| dom.attr(h, a).and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok()).filter(|v| *v <= 16_384);
        let (w, ht) = (px("width"), px("height"));
        let ratio = dom.attr(h, "natural-ratio").map(str::trim);
        let size = match (w, ht, ratio) {
            (Some(w), Some(ht), _) => ImageSize::from((w, ht)),
            (None, None, None) => continue,
            (w, ht, r) => ImageSize {
                width: w,
                height: ht,
                ratio: r.and_then(|r| r.split_once('/')).and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?))).filter(|&(a, b): &(u32, u32)| a > 0 && b > 0),
            },
        };
        out.insert(src.to_string(), size);
    }
    out
}

/// [`prepare`], with the natural sizes of the pictures provided for an
/// HTML document (`ToChild::Provide`).
pub fn prepare_with(kind: DocKind, bytes: &[u8], provided: &BTreeMap<String, (u32, u32)>) -> Result<Prepared, u16> {
    let text = std::str::from_utf8(bytes).map_err(|_| error::UNREADABLE)?;
    let fonts = navigator_render::fontset::FontSet::shared().map_err(|_| error::UNREADABLE)?;
    let env = navigator_style::cascade::Env::default();
    let (doc, inputs, subs) = match kind {
        DocKind::Recording => {
            let r = navigator_normalize::recording::parse(text).map_err(|_| error::UNREADABLE)?;
            let subs = navigator_render::conformance::subresources_from_recording_json(text);
            (r.document, r.inputs, subs)
        }
        DocKind::Html => {
            let inputs = navigator_normalize::Inputs { images: html_image_sizes(text, provided), ..Default::default() };
            (text.to_string(), inputs, Subresources::default())
        }
        DocKind::Picture => return Err(error::UNREADABLE),
    };
    let html = navigator_normalize::normalize_and_measure(&doc, &inputs, fonts, &env).0;
    let anchors = anchors(&doc);
    let sections = sections(&doc, &anchors);
    let forms = forms(&doc);
    Ok(Prepared { title: title(&doc), anchors, sections, forms, html, subs })
}

/// The document's plain HTML forms (limen `ToParent::Forms`; atrium-net.md
/// §8.1 P5): those a form panel can offer, each with a button to send it.
/// A form with a field the panel can't show (a file, a textarea is shown as
/// a line, an image button is a button) is left out; so is one with no
/// button. A `<select>` is sent as its chosen option, as a page would send it
/// untouched.
pub fn forms(doc: &str) -> Vec<limen::doc_viewer::Form> {
    use limen::doc_viewer::{cut, Field, FieldKind, Form, FIELDS_MAX, FIELD_LABEL_MAX, FIELD_NAME_MAX, FIELD_VALUE_MAX, FORMS_MAX, URL_MAX};
    if !doc.get(..doc.len().min(8 << 20)).unwrap_or(doc).to_ascii_lowercase().contains("<form") {
        return Vec::new();
    }
    let dom = navigator_dom::parse(doc);
    let fold = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    // `<label for=id>` texts, by id.
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for l in dom.by_tag("label") {
        if let Some(id) = dom.attr(l, "for") {
            labels.entry(id.to_string()).or_insert_with(|| fold(&dom.visible_text(l)));
        }
    }
    // A control's label: its `<label for>`, an enclosing `<label>`, its
    // placeholder or title, else its name.
    let label_of = |h: navigator_dom::dom::Handle| -> String {
        if let Some(t) = dom.attr(h, "id").and_then(|id| labels.get(id)).filter(|t| !t.is_empty()) {
            return t.clone();
        }
        let mut up = dom.get(h).and_then(|n| n.parent);
        while let Some(p) = up {
            if dom.tag(p).is_some_and(|t| t.eq_ignore_ascii_case("label")) {
                let t = fold(&dom.visible_text(p));
                if !t.is_empty() {
                    return t;
                }
            }
            up = dom.get(p).and_then(|n| n.parent);
        }
        ["aria-label", "placeholder", "title", "name"].iter().find_map(|a| dom.attr(h, a).map(fold).filter(|t| !t.is_empty())).unwrap_or_default()
    };
    let mut out = Vec::new();
    'forms: for f in dom.by_tag("form") {
        if out.len() == FORMS_MAX {
            break;
        }
        let post = dom.attr(f, "method").is_some_and(|m| m.trim().eq_ignore_ascii_case("post"));
        let enctype = dom.attr(f, "enctype").map(|e| e.trim().to_ascii_lowercase());
        if enctype.as_deref().is_some_and(|e| e != "application/x-www-form-urlencoded") {
            continue;
        }
        let action = dom.attr(f, "action").unwrap_or("").trim().to_string();
        if action.len() > URL_MAX || action.to_ascii_lowercase().starts_with("javascript:") {
            continue;
        }
        let mut fields = Vec::new();
        let mut stack: Vec<navigator_dom::dom::Handle> = dom.element_children(f).into_iter().rev().collect();
        while let Some(h) = stack.pop() {
            let tag = dom.tag(h).unwrap_or("").to_ascii_lowercase();
            if tag == "form" {
                continue;
            }
            if dom.attr(h, "disabled").is_some() && matches!(tag.as_str(), "input" | "button" | "select" | "textarea") {
                continue;
            }
            let name = dom.attr(h, "name").unwrap_or("").to_string();
            let value = |d: &str| dom.attr(h, "value").unwrap_or(d).to_string();
            let field = |kind, label: String, value: String, checked| Field {
                kind, name: cut(&name, FIELD_NAME_MAX), label: cut(&label, FIELD_LABEL_MAX), value: cut(&value, FIELD_VALUE_MAX), checked,
            };
            let got = match tag.as_str() {
                "input" => match dom.attr(h, "type").map(|t| t.trim().to_ascii_lowercase()).as_deref().unwrap_or("text") {
                    "hidden" => Some(field(FieldKind::Fixed, String::new(), value(""), false)),
                    "text" | "email" | "tel" | "number" | "search" | "url" | "" => Some(field(FieldKind::Text, label_of(h), value(""), false)),
                    "password" => Some(field(FieldKind::Password, label_of(h), value(""), false)),
                    "checkbox" => Some(field(FieldKind::Checkbox, label_of(h), value("on"), dom.attr(h, "checked").is_some())),
                    "radio" => Some(field(FieldKind::Radio, label_of(h), value("on"), dom.attr(h, "checked").is_some())),
                    "submit" | "image" => Some(field(FieldKind::Submit, dom.attr(h, "value").or(dom.attr(h, "alt")).map(fold).filter(|t| !t.is_empty()).unwrap_or_else(|| "Submit".into()), value(""), false)),
                    "button" | "reset" => None,
                    _ => continue 'forms,
                },
                "button" => match dom.attr(h, "type").map(|t| t.trim().to_ascii_lowercase()).as_deref().unwrap_or("submit") {
                    "submit" => {
                        let t = fold(&dom.visible_text(h));
                        Some(field(FieldKind::Submit, if t.is_empty() { "Submit".into() } else { t }, value(""), false))
                    }
                    _ => None,
                },
                "textarea" => Some(field(FieldKind::Text, label_of(h), fold(&dom.text_content(h)), false)),
                "select" => {
                    let opts = dom.by_tag_anywhere("option").into_iter().filter(|&o| dom.contains(h, o)).collect::<Vec<_>>();
                    let chosen = opts.iter().copied().find(|&o| dom.attr(o, "selected").is_some()).or(opts.first().copied());
                    let v = chosen.map(|o| dom.attr(o, "value").map(String::from).unwrap_or_else(|| fold(&dom.text_content(o)))).unwrap_or_default();
                    Some(field(FieldKind::Fixed, String::new(), v, false))
                }
                _ => {
                    stack.extend(dom.element_children(h).into_iter().rev());
                    None
                }
            };
            if let Some(g) = got {
                if fields.len() == FIELDS_MAX {
                    continue 'forms;
                }
                fields.push(g);
            }
        }
        let form = Form { action, post, fields };
        if form.ok() {
            out.push(form);
        }
    }
    out
}

/// The first `<title>`'s text, whitespace folded, the common entities
/// decoded, cut to `TITLE_MAX`.
pub fn title(doc: &str) -> String {
    let lower = doc.get(..doc.len().min(1 << 20)).unwrap_or(doc).to_ascii_lowercase();
    let Some(a) = lower.find("<title") else { return String::new() };
    let Some(open_end) = lower[a..].find('>').map(|i| a + i + 1) else { return String::new() };
    let Some(close) = lower[open_end..].find("</title").map(|i| open_end + i) else { return String::new() };
    let raw = &doc[open_end..close];
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ")
        .replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&");
    let t: String = t.chars().filter(|c| !c.is_control()).collect();
    limen::doc_viewer::cut(&t, TITLE_MAX)
}

/// The document's `h1`–`h3` headings that carry an anchor (an `id` on the
/// heading, or the first one inside it, as wikis put it), with their text:
/// "Jump to section". Only anchors in `anchors` are named, so each can be
/// scrolled to.
pub fn sections(doc: &str, anchors: &[String]) -> Vec<(String, String)> {
    use limen::doc_viewer::{SECTIONS_MAX, SECTION_TITLE_MAX};
    let lower = doc.to_ascii_lowercase();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut at = 0;
    while out.len() < SECTIONS_MAX {
        let Some(i) = lower[at..].find("<h").map(|i| at + i) else { break };
        at = i + 2;
        let level = lower.as_bytes().get(i + 2).copied();
        if !matches!(level, Some(b'1'..=b'3')) || !matches!(lower.as_bytes().get(i + 3), Some(b' ' | b'>')) {
            continue;
        }
        let Some(open_end) = lower[i..].find('>').map(|k| i + k + 1) else { break };
        let close_tag = format!("</h{}", level.unwrap() as char);
        let Some(close) = lower[open_end..].find(&close_tag).map(|k| open_end + k) else { break };
        let id_in = |s: &str| s.find(" id=\"").and_then(|k| s[k + 5..].find('"').map(|e| s[k + 5..k + 5 + e].to_string()));
        let anchor = id_in(&doc[i..open_end]).or_else(|| id_in(&doc[open_end..close]));
        at = close;
        let Some(anchor) = anchor.filter(|a| anchors.contains(a)) else { continue };
        // The heading's text: its tags left out, the common entities read.
        let mut text = String::new();
        let mut in_tag = false;
        for c in doc[open_end..close].chars() {
            match c {
                '<' => in_tag = true,
                '>' => { in_tag = false; text.push(' ') }
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        let t = text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ").replace("&amp;", "&");
        let t: String = t.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).collect();
        let t = limen::doc_viewer::cut(&t, SECTION_TITLE_MAX);
        if !t.is_empty() && !out.iter().any(|(a, _)| *a == anchor) {
            out.push((anchor, t));
        }
    }
    out
}

/// The document's `id="…"` values, in order, unique, bounded as the
/// message is.
pub fn anchors(doc: &str) -> Vec<String> {
    use limen::doc_viewer::{ANCHORS_MAX, ANCHOR_MAX};
    let mut out: Vec<String> = Vec::new();
    for (i, _) in doc.match_indices(" id=\"") {
        if out.len() >= ANCHORS_MAX {
            break;
        }
        let rest = &doc[i + 5..];
        let Some(end) = rest.find('"') else { break };
        let id = &rest[..end];
        if !id.is_empty() && id.len() <= ANCHOR_MAX && !id.chars().any(char::is_control) && !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Where a link goes, resolved against the page's `base` (absolute
/// `http(s)` only; anything else, `javascript:` included, is `None`).
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.chars().any(char::is_control) {
        return None;
    }
    let scheme_of = |s: &str| s.split_once(':').map(|(a, _)| a.to_ascii_lowercase()).filter(|a| a.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
    if let Some(s) = scheme_of(href) {
        return matches!(s.as_str(), "http" | "https").then(|| href.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    let (host, path) = rest.find(['/', '?', '#']).map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    if let Some(r) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{r}"));
    }
    if href.starts_with('/') {
        return Some(format!("{scheme}://{host}{href}"));
    }
    let path_only = path.split(['?', '#']).next().unwrap_or("/");
    if href.starts_with('#') || href.starts_with('?') {
        let keep = if href.starts_with('#') { path.split('#').next().unwrap_or(path) } else { path_only };
        return Some(format!("{scheme}://{host}{keep}{href}"));
    }
    let dir = &path_only[..path_only.rfind('/').map_or(0, |i| i + 1)];
    let mut segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let (rel, tail) = href.find(['?', '#']).map_or((href, ""), |i| (&href[..i], &href[i..]));
    let mut last_dir = rel.ends_with('/') || rel.ends_with("/.") || rel.ends_with("/..") || rel == "." || rel == "..";
    for s in rel.split('/') {
        match s {
            "" | "." => {}
            ".." => { segs.pop(); }
            s => { segs.push(s); last_dir = false; }
        }
    }
    if rel.ends_with('/') { last_dir = true; }
    let mut p = format!("/{}", segs.join("/"));
    if last_dir && p != "/" {
        p.push('/');
    }
    Some(format!("{scheme}://{host}{p}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_resolve_against_the_page() {
        let b = "https://atlas.example/wiki/Atrium?x=1#top";
        assert_eq!(resolve(b, "Impluvium").as_deref(), Some("https://atlas.example/wiki/Impluvium"));
        assert_eq!(resolve(b, "/about").as_deref(), Some("https://atlas.example/about"));
        assert_eq!(resolve(b, "../map/").as_deref(), Some("https://atlas.example/map/"));
        assert_eq!(resolve(b, "#plan").as_deref(), Some("https://atlas.example/wiki/Atrium?x=1#plan"));
        assert_eq!(resolve(b, "//other.example/a").as_deref(), Some("https://other.example/a"));
        assert_eq!(resolve(b, "http://old.example/").as_deref(), Some("http://old.example/"));
        assert_eq!(resolve(b, "javascript:alert(1)"), None);
        assert_eq!(resolve(b, "mailto:a@b"), None);
        assert_eq!(resolve("https://atlas.example", "a").as_deref(), Some("https://atlas.example/a"));
    }

    #[test]
    fn sections_are_headings_with_anchors() {
        let doc = r#"<body><h1 id="top">Atrium <small>(architecture)</small></h1><h2><span id="Plan">The plan &amp; court</span></h2>
            <h2>No anchor</h2><h3 id="x">Rain</h3><h4 id="deep">Too deep</h4><header id="hd">Not a heading</header><h2 id="gone">Unlisted</h2></body>"#;
        let anchors: Vec<String> = ["top", "Plan", "x", "deep", "hd"].map(String::from).to_vec();
        assert_eq!(sections(doc, &anchors), vec![
            ("top".into(), "Atrium (architecture)".into()),
            ("Plan".into(), "The plan & court".into()),
            ("x".into(), "Rain".into()),
        ]);
    }

    #[test]
    fn a_recording_is_prepared_with_its_title_and_anchors() {
        let rec = br##"{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":2,"tier_reason":"","measurements":{},
          "transitions":[],"stylesheets":[{"href":"/s.css","media":"","text":"h1 { color: #224466 }"}],"images":[],
          "document":"<html><head><title>Atrium &amp; its\n court</title><link rel=\"stylesheet\" href=\"/s.css\"></head><body><h1 id=\"top\">Atrium</h1><p id=\"plan\">A <a href=\"/wiki/Impluvium\">pool</a>.</p></body></html>"}"##;
        let p = prepare(DocKind::Recording, rec).unwrap();
        assert_eq!(p.title, "Atrium & its court");
        assert_eq!(p.anchors, vec!["top", "plan"]);
        assert_eq!(p.sections, vec![("top".to_string(), "Atrium".to_string())]);
        assert!(p.html.contains("Impluvium"), "{}", p.html);
        assert_eq!(prepare(DocKind::Recording, b"{\"format\":\"nope\"}").err(), Some(error::UNREADABLE));
        assert_eq!(prepare(DocKind::Recording, &[0xff, 0xfe]).err(), Some(error::UNREADABLE));
    }

    #[test]
    fn a_picture_is_decoded_reduced_and_fitted() {
        // A 4000 x 1000 PNG, one colour.
        let mut png = Vec::new();
        {
            let mut e = png::Encoder::new(&mut png, 4000, 1000);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&vec![200u8; 4000 * 1000 * 4]).unwrap();
        }
        let px = picture(&png).unwrap();
        assert!(px.width <= PICTURE_PX_MAX && px.height <= PICTURE_PX_MAX, "{}x{}", px.width, px.height);
        assert_eq!(px.rgba.len(), (px.width * px.height * 4) as usize);
        // A small SVG is drawn large.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="12"><rect width="24" height="12" fill="red"/></svg>"#;
        let s = picture(svg).unwrap();
        assert_eq!((s.width, s.height), (SVG_PX_MIN, SVG_PX_MIN / 2));
        // Not a picture.
        assert_eq!(picture(b"hello").err(), Some(error::UNREADABLE));
        assert_eq!(picture(b"\x89PNG\r\n\x1a\nbroken").err(), Some(error::UNREADABLE));
        // Fitted: whole, centred, never enlarged.
        assert_eq!(fit_picture(2000, 500, 400.0, 300.0), [0.0, 100.0, 400.0, 100.0]);
        assert_eq!(fit_picture(100, 100, 400.0, 300.0), [150.0, 100.0, 100.0, 100.0]);
    }

    #[test]
    fn reading_stops_past_the_cap() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                b.fill(b'x');
                Ok(b.len())
            }
        }
        assert_eq!(read_bounded(Endless).err(), Some(error::TOO_BIG));
        assert_eq!(read_bounded(&b"abc"[..]).unwrap(), b"abc");
    }
}

/// Pictures the parent provided (`ToChild::Provide`, mail.md §7.3), for the
/// document about to load and for the one shown.
#[derive(Default)]
pub struct Provided {
    pending: Vec<(String, Vec<u8>)>,
    current: Vec<(String, Vec<u8>)>,
}

impl Provided {
    /// Keep a picture: for the shown document when `shown`, else for the
    /// next `Load`. False (and dropped) past the limits.
    pub fn provide(&mut self, id: String, bytes: Vec<u8>, shown: bool) -> bool {
        let list = if shown { &mut self.current } else { &mut self.pending };
        let total: usize = list.iter().map(|(_, b)| b.len()).sum();
        if list.len() >= limen::doc_viewer::PROVIDED_MAX || total + bytes.len() > limen::doc_viewer::PROVIDED_BYTES_MAX
            || bytes.len() > navigator_image::image::MAX_ENCODED_IMAGE_BYTES {
            return false;
        }
        list.retain(|(i, _)| *i != id);
        list.push((id, bytes));
        true
    }

    /// A `Load` came: what was provided before it is now the shown document's.
    pub fn load(&mut self) {
        self.current = std::mem::take(&mut self.pending);
    }

    /// The natural size of each picture provided for the shown document.
    pub fn sizes(&self) -> BTreeMap<String, (u32, u32)> {
        self.current.iter().filter_map(|(id, b)| navigator_image::image::natural_size(b).ok().map(|(_, w, h)| (id.clone(), (w, h)))).collect()
    }

    /// The shown document's pictures as the renderer takes them: each `id`
    /// (as the document spells it) mapped to its bytes' address and natural
    /// size, and the bytes by address. Bytes that aren't a picture are left out.
    pub fn resources(&self) -> (Subresources, Vec<(String, Vec<u8>)>) {
        let mut subs = Subresources::new();
        let mut images = Vec::new();
        for (id, b) in &self.current {
            let Ok((_, w, h)) = navigator_image::image::natural_size(b) else { continue };
            let address = format!("blake3:{}", blake3::hash(b).to_hex());
            subs.insert(id.clone(), (address.clone(), w as i64 * navigator_render::PX, h as i64 * navigator_render::PX));
            images.push((address, b.clone()));
        }
        (subs, images)
    }
}

#[cfg(test)]
mod provided_tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(&vec![200u8; (w * h * 4) as usize]).unwrap();
        out
    }

    /// Lay `doc` out with what `p` holds and paint it: how many pictures
    /// were drawn.
    fn pictures_drawn(doc: &[u8], p: &Provided) -> usize {
        use pergola::view::{Ctx, View};
        use std::cell::RefCell;
        use std::rc::Rc;
        let mut prepared = prepare_with(DocKind::Html, doc, &p.sizes()).unwrap();
        let (subs, images) = p.resources();
        prepared.subs.extend(subs);
        let images: std::collections::HashMap<String, Vec<u8>> = images.into_iter().collect();
        let d = pergola_document::Doc::html_scaled(prepared.html, prepared.subs, 600.0, 800.0, false, 64, move |a| {
            images.get(a).map(|_| pergola::node::Image::new(2, 2, vec![9; 16]))
        }).unwrap();
        struct V(pergola_document::DocumentView);
        impl View for V {
            fn render(&self, ctx: &mut Ctx) { self.0.emit(ctx); }
        }
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let scroll = pergola::Mutable::with_dirty(pergola::widgets::ScrollState::default(), dirty.clone());
        let more = scroll.sibling(0u64);
        let v = pergola_document::DocumentView::new(scroll, pergola::geom::Rect::new(0.0, 0.0, 600.0, 800.0), Rc::new(RefCell::new(d)), more);
        let mut app = pergola::App::new_with_flag(V(v), dirty);
        for _ in 0..3 {
            app.tick();
        }
        app.tree().iter().filter(|(_, n)| matches!(n, pergola::node::Node::Image { .. })).count()
    }

    #[test]
    fn a_mail_picture_is_drawn_once_provided() {
        // As Mail's parse jail writes it: the picture's size declared.
        let doc = br#"<html><body><p>Hello</p><img src="mail-res:0" width="40" height="300"><p>after</p></body></html>"#;
        let mut p = Provided::default();
        assert_eq!(pictures_drawn(doc, &p), 0, "a placeholder until provided");
        // Provided before Load: it belongs to the next document.
        assert!(p.provide("mail-res:0".into(), png(40, 300), false));
        assert_eq!(pictures_drawn(doc, &p), 0);
        p.load();
        assert_eq!(pictures_drawn(doc, &p), 1);
        // A picture whose size the document doesn't declare is laid out at
        // its own size once provided.
        let bare = br#"<html><body><img src="mail-res:0"></body></html>"#;
        assert_eq!(pictures_drawn(bare, &p), 1);
        // Another document (the next message) starts with nothing.
        p.load();
        assert_eq!(pictures_drawn(doc, &p), 0);
        // Not a picture: left out. Over the limits: refused.
        assert!(p.provide("mail-res:1".into(), b"<script>".to_vec(), true));
        assert!(p.resources().0.is_empty());
        assert!(!p.provide("big".into(), vec![0; navigator_image::image::MAX_ENCODED_IMAGE_BYTES + 1], true));
    }
}

#[cfg(test)]
mod print_tests {
    use super::*;

    #[test]
    fn a_long_page_prints_on_several_pages_between_lines() {
        let mut body = String::from("<html><head><title>Atrium</title></head><body><h1>The atrium</h1>");
        for i in 0..120 {
            body.push_str(&format!("<p>Paragraph {i}: the impluvium is the sunken part of the atrium, designed to carry away the rainwater coming through the compluvium of the roof.</p>"));
        }
        body.push_str("<p><a href=\"https://atlas.example/wiki/Impluvium\">Impluvium</a></p></body></html>");
        let p = prepare(DocKind::Html, body.as_bytes()).expect("prepared");
        let a4 = print::Paper::new(595, 842);
        let out = print::pdf(p.html.clone(), p.subs.clone(), &p.title, a4, &|_| None).expect("printed");
        assert!(out.pdf.starts_with(b"%PDF-"), "a PDF");
        assert!(out.pages >= 3 && out.pages < 40, "{} pages", out.pages);
        assert!(!out.cut);
        if let Ok(d) = std::env::var("PRINT_OUT") {
            std::fs::write(d, &out.pdf).unwrap();
        }
        let text = String::from_utf8_lossy(&out.pdf);
        assert!(text.contains("/FontFile2") && text.contains("/URI"), "a face embedded and the link kept");
        // Letter is wider: fewer pages.
        let letter = print::pdf(p.html.clone(), p.subs.clone(), &p.title, print::Paper::new(612, 792), &|_| None).unwrap();
        assert!(letter.pages <= out.pages + 1);
        assert!(print::pdf(p.html, p.subs, "", print::Paper::new(100, 100), &|_| None).is_err(), "too small");
    }

    #[test]
    fn a_sign_in_pages_forms_are_read_as_spelled() {
        use limen::doc_viewer::FieldKind::*;
        let doc = r#"<html><body><form method="POST" action="/login">
            <input type="hidden" name="t" value="9f">
            <label for="room">Room number</label><input id="room" name="room">
            <label><input type="checkbox" name="terms" value="yes"> I accept the terms</label>
            <select name="plan"><option value="free">Free</option><option value="day" selected>Day</option></select>
            <button>Connect</button></form>
            <form action="/up" enctype="multipart/form-data"><input type="file" name="f"><button>Up</button></form>
            <form action="/q"><input name="q" placeholder="Search"></form></body></html>"#;
        let f = forms(doc);
        assert_eq!(f.len(), 1, "multipart and buttonless forms are left out: {f:?}");
        assert_eq!((f[0].action.as_str(), f[0].post), ("/login", true));
        let k: Vec<_> = f[0].fields.iter().map(|x| (x.kind, x.name.as_str(), x.label.as_str(), x.value.as_str())).collect();
        assert_eq!(k, vec![(Fixed, "t", "", "9f"), (Text, "room", "Room number", ""), (Checkbox, "terms", "I accept the terms", "yes"),
                           (Fixed, "plan", "", "day"), (Submit, "", "Connect", "")]);
        assert!(forms("<p>no forms</p>").is_empty());
    }
}
