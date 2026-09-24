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
pub struct Image { pub src: String, pub natural: Natural, pub address: String }

/// ★ An image's NATURAL sizing (CSS Images 3 §5.1): a width, a height and
/// a ratio, EACH optional. A raster image has all three. An SVG may state a
/// width alone (no ratio: its height defaults at layout), or only a
/// viewBox (a ratio and no size), or nothing (measured — and natural size
/// none). Collapsing that to a (width, height) pair invented ratios CSS
/// says are not there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Natural { pub width: Option<u32>, pub height: Option<u32>, pub ratio: Option<(u32, u32)> }

impl Natural {
    pub fn sized(w: u32, h: u32) -> Self { Natural { width: Some(w), height: Some(h), ratio: Some((w, h)) } }
}

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
            Ok(bytes) => match natural_size(&bytes).or_else(|| declared("width").zip(declared("height")).map(|(w, h)| Natural::sized(w, h))) {
                Some(natural) => {
                    let address = format!("blake3:{}", blake3::hash(&bytes).to_hex());
                    store(&address, &bytes);
                    out.images.push(Image { src, natural, address });
                }
                None => out.failed.push((src, "no intrinsic size in the header".into())),
            },
            Err(e) => match declared("width").zip(declared("height")) {
                // The document declared it, so layout is safe even though the
                // bytes are not in hand: the renderer reserves the space.
                Some((w, ht)) => out.images.push(Image { src, natural: Natural::sized(w, ht), address: String::new() }),
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
/// Both natural dimensions, when the image has both — the raster case.
pub fn intrinsic_size(b: &[u8]) -> Option<(u32, u32)> {
    let n = natural_size(b)?;
    n.width.zip(n.height)
}

/// ★ The image's natural sizing, each part optional (see [`Natural`]).
/// `None` only when the bytes are not an image this can read.
pub fn natural_size(b: &[u8]) -> Option<Natural> {
    if let Some((w, h)) = raster_size(b) { return Some(Natural::sized(w, h)) }
    let head = &b[..b.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    let start = text.find("<svg")?;
    // ★ Only the ROOT element's attributes. Searching the whole file took
    // the first `width="` anywhere — a nested rect's, as often as not.
    let tag = &text[start..start + text[start..].find('>').unwrap_or(text.len() - start)];
    let raw = |name: &str| -> Option<String> {
        let at = [format!(" {name}=\""), format!(" {name}='")].iter().find_map(|k| tag.find(k.as_str()).map(|i| i + k.len()))?;
        let rest = &tag[at..];
        Some(rest[..rest.find(['"', '\''])?].trim().to_string())
    };
    // Only absolute px lengths are natural dimensions; `100%` or `2em`
    // depend on where the image is used, so they are no natural size.
    let px = |name: &str| -> Option<f64> {
        let v = raw(name)?;
        let v = v.strip_suffix("px").unwrap_or(&v);
        v.parse::<f64>().ok().filter(|n| *n > 0.0)
    };
    let (w, h) = (px("width"), px("height"));
    let vb: Option<(f64, f64)> = raw("viewBox").and_then(|v| {
        let n: Vec<f64> = v.split([' ', ',']).filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
        (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then(|| (n[2], n[3]))
    });
    // ★ A ratio is EXACT: the numbers themselves (to three decimals),
    // reduced by their gcd — 1000×1300 is 10/13. Scaling the larger side
    // to 1000 and rounding made it 769/1000, and the Rust book's diagrams
    // came out a fraction of a pixel short, shifting every line below.
    let ratio_of = |a: f64, b: f64| -> (u32, u32) {
        let (mut x, mut y) = (((a * 1000.0).round() as u64).max(1), ((b * 1000.0).round() as u64).max(1));
        let (mut p, mut q) = (x, y);
        while q != 0 { let r = p % q; p = q; q = r }
        x /= p; y /= p;
        // Keep it in u32 without losing the proportion.
        while x > u32::MAX as u64 || y > u32::MAX as u64 { x = (x + 1) / 2; y = (y + 1) / 2 }
        (x as u32, y as u32)
    };
    let ratio = match (w, h) { (Some(a), Some(b)) => Some(ratio_of(a, b)), _ => vb.map(|(a, b)| ratio_of(a, b)) };
    Some(Natural { width: w.map(|v| v.round() as u32), height: h.map(|v| v.round() as u32), ratio })
}

fn raster_size(b: &[u8]) -> Option<(u32, u32)> {
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
    None
}
