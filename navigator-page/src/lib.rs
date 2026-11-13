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
    };
    let html = navigator_normalize::normalize_and_measure(&doc, &inputs, fonts, &env).0;
    Ok(Prepared { title: title(&doc), anchors: anchors(&doc), html, subs })
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
    fn a_recording_is_prepared_with_its_title_and_anchors() {
        let rec = br##"{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":2,"tier_reason":"","measurements":{},
          "transitions":[],"stylesheets":[{"href":"/s.css","media":"","text":"h1 { color: #224466 }"}],"images":[],
          "document":"<html><head><title>Atrium &amp; its\n court</title><link rel=\"stylesheet\" href=\"/s.css\"></head><body><h1 id=\"top\">Atrium</h1><p id=\"plan\">A <a href=\"/wiki/Impluvium\">pool</a>.</p></body></html>"}"##;
        let p = prepare(DocKind::Recording, rec).unwrap();
        assert_eq!(p.title, "Atrium & its court");
        assert_eq!(p.anchors, vec!["top", "plan"]);
        assert!(p.html.contains("Impluvium"), "{}", p.html);
        assert_eq!(prepare(DocKind::Recording, b"{\"format\":\"nope\"}").err(), Some(error::UNREADABLE));
        assert_eq!(prepare(DocKind::Recording, &[0xff, 0xfe]).err(), Some(error::UNREADABLE));
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
