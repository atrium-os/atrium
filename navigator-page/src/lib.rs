//! The doc-viewer child's work that needs no window (limen.md §2.1, L6):
//! read what the parent handed over, bounded, and turn a recording into
//! the normalized document `DocumentView` lays out.
//!
//! Everything here reads hostile bytes: the recording came from the
//! converter, whose input was a web page. The child is jailed with no
//! network and no files (only the descriptor it was handed and its
//! frescod connection), so what a bug here can reach is this one page.

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
    let text = std::str::from_utf8(bytes).map_err(|_| error::UNREADABLE)?;
    let fonts = navigator_render::fontset::FontSet::shared().map_err(|_| error::UNREADABLE)?;
    let env = navigator_style::cascade::Env::default();
    let (doc, inputs, subs) = match kind {
        DocKind::Recording => {
            let r = navigator_normalize::recording::parse(text).map_err(|_| error::UNREADABLE)?;
            let subs = navigator_render::conformance::subresources_from_recording_json(text);
            (r.document, r.inputs, subs)
        }
        DocKind::Html => (text.to_string(), Default::default(), Subresources::default()),
        DocKind::Picture => return Err(error::UNREADABLE),
    };
    let html = navigator_normalize::normalize_and_measure(&doc, &inputs, fonts, &env).0;
    let anchors = anchors(&doc);
    let sections = sections(&doc, &anchors);
    Ok(Prepared { title: title(&doc), anchors, sections, html, subs })
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
