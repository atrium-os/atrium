//! Subresources the NORMALIZER needs — collected by the converter, because
//! the normalizer has no capabilities (Profile v1 §6) and the renderer has
//! no measurement path (§3.13).
//!
//! Two kinds, and they are needed for different reasons:
//! - **stylesheets**, because a document's design is in them and a
//!   normalizer that is not given them emits a page with no styling at all;
//! - **image intrinsic sizes**, because layout must know a replaced box's
//!   size BEFORE it lays out (§1.2, the anti-CLS rule), and nothing later in
//!   the lane is allowed to measure.
//!
//! ★ The size is read from the image's own header, and the bytes are named
//! by their content address — so the renderer can paint from CAS without
//! anyone re-fetching, and a byte that changes changes the address.

use crate::fetch::Fetcher;
use navigator_dom::Dom;

#[derive(Debug, Clone, PartialEq)]
pub struct Sheet { pub href: String, pub media: String, pub text: String }

#[derive(Debug, Clone, PartialEq)]
pub struct Image { pub src: String, pub width: u32, pub height: u32, pub address: String }

/// Where fetched bytes are kept, named by their content address. A
/// stand-in for Tessera's CAS with the same two properties that matter: the
/// name IS the hash, so a byte that changes changes the name, and a second
/// document referencing the same image costs nothing.
pub fn blob_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PRERENDER_BLOB_DIR").map(std::path::PathBuf::from)
}

fn store(address: &str, bytes: &[u8]) {
    let Some(dir) = blob_dir() else { return };
    let Some(hex) = address.strip_prefix("blake3:") else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(hex);
    // Content-addressed: if it is there, it is already the right bytes.
    if path.exists() { return }
    let _ = std::fs::write(path, bytes);
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Subresources {
    pub sheets: Vec<Sheet>,
    pub images: Vec<Image>,
    /// What could not be fetched or measured, with the reason.
    pub failed: Vec<(String, String)>,
}

/// The profile bounds `@import` depth (§3.11); so does this.
const MAX_IMPORT_DEPTH: usize = 2;

pub fn collect(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher) -> Subresources {
    let mut out = Subresources::default();
    let resolve = |href: &str| -> Option<String> {
        match base.and_then(|b| url::Url::parse(b).ok()) {
            Some(b) => b.join(href).ok().map(|u| u.to_string()),
            None => href.starts_with("http").then(|| href.to_string()),
        }
    };

    for h in dom.by_tag_anywhere("link") {
        let is_sheet = dom.attr(h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet")));
        if !is_sheet { continue }
        let Some(href) = dom.attr(h, "href").map(str::to_string) else { continue };
        if out.sheets.iter().any(|s| s.href == href) { continue }
        let Some(abs) = resolve(&href) else { out.failed.push((href, "cannot resolve".into())); continue };
        match fetcher.get(&abs) {
            Ok(text) => {
                // ★ Follow `@import`: the W3C's stylesheet for a spec is 123
                // bytes and one import. A converter that stops at the link
                // hands the normalizer an empty design.
                let media = dom.attr(h, "media").unwrap_or("").to_string();
                imports(&text, &abs, &href, fetcher, 0, &mut out);
                out.sheets.push(Sheet { href, media, text });
            }
            Err(e) => out.failed.push((href, e)),
        }
    }

    for h in dom.by_tag_anywhere("img") {
        let Some(src) = dom.attr(h, "src").map(str::to_string) else { continue };
        if out.images.iter().any(|i| i.src == src) { continue }
        // An image whose size the DOCUMENT already declares needs no fetch:
        // it has already arrived measured.
        let declared = |n: &str| dom.attr(h, n).and_then(|v| v.trim().parse::<u32>().ok());
        let Some(abs) = resolve(&src) else { out.failed.push((src, "cannot resolve".into())); continue };
        match fetcher.get_bytes(&abs) {
            Ok(bytes) => match intrinsic_size(&bytes).or_else(|| declared("width").zip(declared("height"))) {
                Some((w, ht)) => {
                    let address = format!("blake3:{}", blake3::hash(&bytes).to_hex());
                    store(&address, &bytes);
                    out.images.push(Image { src, width: w, height: ht, address });
                }
                None => out.failed.push((src, "no intrinsic size in the header".into())),
            },
            Err(e) => match declared("width").zip(declared("height")) {
                // The document declared it, so layout is safe even though the
                // bytes are not in hand: the renderer reserves the space.
                Some((w, ht)) => out.images.push(Image { src, width: w, height: ht, address: String::new() }),
                None => out.failed.push((src, e)),
            },
        }
    }
    out
}

/// `abs` is where the sheet was fetched from; `key` is how the DOCUMENT
/// names it. ★ The two differ, and the import must be keyed the way the
/// normalizer will resolve it — relative to the importing sheet's own key —
/// or the lookup misses and the imported CSS is silently absent.
fn imports(css: &str, abs: &str, key: &str, fetcher: &mut dyn Fetcher, depth: usize, out: &mut Subresources) {
    if depth >= MAX_IMPORT_DEPTH || !css.contains("@import") { return }
    for stmt in css.split("@import").skip(1) {
        let head = &stmt[..stmt.find(';').unwrap_or(stmt.len().min(256))];
        let target = head.split_once('"').map(|(_, r)| r.split('"').next().unwrap_or("").to_string())
            .or_else(|| head.split_once("url(").map(|(_, r)| r.split(')').next().unwrap_or("").trim_matches(['"', '\'']).to_string()))
            .unwrap_or_default();
        if target.is_empty() { continue }
        let Ok(next_abs) = url::Url::parse(abs).and_then(|b| b.join(&target)) else { continue };
        let next_abs = next_abs.to_string();
        let next_key = match (target.starts_with("http"), key.rfind('/')) {
            (true, _) => target.clone(),
            (false, Some(i)) => format!("{}{}", &key[..i + 1], target),
            (false, None) => target.clone(),
        };
        if out.sheets.iter().any(|s| s.href == next_key) { continue }
        match fetcher.get(&next_abs) {
            Ok(text) => {
                imports(&text, &next_abs, &next_key, fetcher, depth + 1, out);
                out.sheets.push(Sheet { href: next_key, media: String::new(), text });
            }
            Err(e) => out.failed.push((next_key, e)),
        }
    }
}

/// Intrinsic size from an image's own header. Formats the corpus actually
/// contains; anything else is reported rather than guessed at.
pub fn intrinsic_size(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| -> Option<u32> { Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?])) };
    let be16 = |i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let le16 = |i: usize| -> Option<u32> { Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        // JPEG: walk the segments to the frame header.
        let mut i = 2usize;
        // ★ `i + 9 <= len`: the frame header's last byte read is `i + 8`,
        // and a `<` here made a JPEG whose frame is the LAST segment report
        // no size at all.
        while i + 9 <= b.len() {
            if b[i] != 0xFF { i += 1; continue }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            let seg = be16(i + 2)? as usize;
            if seg < 2 { return None }
            i += 2 + seg;
        }
        return None;
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { return Some((le16(6)?, le16(8)?)) }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16) {
            Some(b"VP8X") => {
                let d = |i: usize| -> Option<u32> { Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0]) + 1) };
                Some((d(24)?, d(27)?))
            }
            Some(b"VP8 ") => Some(((le16(26)? & 0x3FFF), (le16(28)? & 0x3FFF))),
            Some(b"VP8L") => {
                let bits = u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            _ => None,
        };
    }
    // SVG: its size is in the markup, as `width`/`height` or a `viewBox`.
    let head = &b[..b.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    if text.contains("<svg") {
        let attr = |name: &str| -> Option<f64> {
            let at = text.find(&format!("{name}=\""))? + name.len() + 2;
            let rest = &text[at..];
            let end = rest.find('"')?;
            rest[..end].trim().trim_end_matches("px").parse().ok()
        };
        if let (Some(w), Some(h)) = (attr("width"), attr("height")) {
            if w > 0.0 && h > 0.0 { return Some((w.round() as u32, h.round() as u32)) }
        }
        if let Some(at) = text.find("viewBox=\"") {
            let rest = &text[at + 9..];
            if let Some(end) = rest.find('"') {
                let n: Vec<f64> = rest[..end].split([' ', ',']).filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
                if n.len() == 4 && n[2] > 0.0 && n[3] > 0.0 { return Some((n[2].round() as u32, n[3].round() as u32)) }
            }
        }
    }
    None
}
