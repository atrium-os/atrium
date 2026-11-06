//! Resolving a media URL against the document's (RFC 3986 §5.2, the cases a
//! document writes), for the Navigator's fetcher. Only `https` and `http`
//! are fetched; anything else is refused.

/// Longest URL handed to the fetcher.
pub const URL_MAX: usize = 4096;

/// `reference` against `base` (an absolute `http(s)` URL), fragment dropped.
pub fn resolve(base: &str, reference: &str) -> Option<String> {
    let r = reference.trim();
    let r = r.split('#').next().unwrap_or("");
    if r.is_empty() || r.chars().any(|c| c.is_control()) { return None }
    let (scheme, rest) = base.split_once("://")?;
    if !matches!(scheme, "https" | "http") { return None }
    let host_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = (&rest[..host_end], &rest[host_end..]);
    if authority.is_empty() { return None }
    let out = if let Some((s, _)) = r.split_once(':').filter(|(s, _)| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) && !r.starts_with('/')) {
        let s = s.to_ascii_lowercase();
        if s != "https" && s != "http" { return None }
        r.to_string()
    } else if let Some(net) = r.strip_prefix("//") {
        format!("{scheme}://{net}")
    } else if r.starts_with('/') {
        format!("{scheme}://{authority}{}", normalize(r))
    } else if r.starts_with('?') {
        let p = path.split('?').next().unwrap_or("");
        format!("{scheme}://{authority}{}{r}", if p.is_empty() { "/" } else { p })
    } else {
        let p = path.split('?').next().unwrap_or("");
        let dir = &p[..p.rfind('/').map(|i| i + 1).unwrap_or(0)];
        let dir = if dir.is_empty() { "/" } else { dir };
        format!("{scheme}://{authority}{}", normalize(&format!("{dir}{r}")))
    };
    (out.len() <= URL_MAX).then_some(out)
}

/// Remove `.` and `..` segments.
fn normalize(path: &str) -> String {
    let (p, q) = match path.split_once('?') { Some((p, q)) => (p, Some(q)), None => (path, None) };
    let mut out: Vec<&str> = vec![];
    let segs: Vec<&str> = p.split('/').collect();
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "." => { if i + 1 == segs.len() { out.push("") } }
            ".." => { if out.len() > 1 { out.pop(); } if i + 1 == segs.len() { out.push("") } }
            s => out.push(s),
        }
    }
    let mut s = out.join("/");
    if !s.starts_with('/') { s.insert(0, '/') }
    if let Some(q) = q { s.push('?'); s.push_str(q) }
    s
}

#[cfg(test)]
mod tests {
    use super::resolve;
    #[test]
    fn resolves_what_documents_write() {
        let b = "https://en.wikipedia.org/wiki/Page?x=1";
        assert_eq!(resolve(b, "//upload.wikimedia.org/a.webm").as_deref(), Some("https://upload.wikimedia.org/a.webm"));
        assert_eq!(resolve(b, "/static/v.mp4").as_deref(), Some("https://en.wikipedia.org/static/v.mp4"));
        assert_eq!(resolve(b, "v.mp4").as_deref(), Some("https://en.wikipedia.org/wiki/v.mp4"));
        assert_eq!(resolve(b, "../a/./b.webm#t=3").as_deref(), Some("https://en.wikipedia.org/a/b.webm"));
        assert_eq!(resolve(b, "https://cdn.test/x.mp4").as_deref(), Some("https://cdn.test/x.mp4"));
        assert_eq!(resolve(b, "javascript:alert(1)"), None);
        assert_eq!(resolve(b, "file:///etc/passwd"), None);
        assert_eq!(resolve(b, "data:video/mp4;base64,AAAA"), None);
        assert_eq!(resolve(b, ""), None);
        assert_eq!(resolve("about:blank", "a.mp4"), None);
    }
}
