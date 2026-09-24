//! What the converter must hand the normalizer (Profile v1 §6 and §3.13).

use navigator_prerender::fetch::MapFetcher;
use navigator_prerender::subresource::{collect, intrinsic_size};

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend((13u32).to_be_bytes());
    v.extend(b"IHDR");
    v.extend(w.to_be_bytes());
    v.extend(h.to_be_bytes());
    v.extend([8, 2, 0, 0, 0]);
    v
}

#[test]
fn every_format_the_corpus_contains_reports_its_own_size() {
    assert_eq!(intrinsic_size(&png(40, 20)), Some((40, 20)));

    let mut gif = b"GIF89a".to_vec();
    gif.extend(300u16.to_le_bytes());
    gif.extend(200u16.to_le_bytes());
    assert_eq!(intrinsic_size(&gif), Some((300, 200)));

    // JPEG: a COM segment first, so the walk has to skip something.
    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xFE, 0x00, 0x04, 0x41, 0x42];
    jpg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
    jpg.extend(480u16.to_be_bytes()); // height first, as JPEG writes it
    jpg.extend(640u16.to_be_bytes());
    assert_eq!(intrinsic_size(&jpg), Some((640, 480)));

    let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    webp.extend([0; 8]);
    webp.extend([99, 0, 0]);   // (width - 1), 24-bit LE
    webp.extend([49, 0, 0]);   // (height - 1)
    assert_eq!(intrinsic_size(&webp), Some((100, 50)));

    // SVG carries its size in the markup by attribute. A viewBox alone is a
    // RATIO, not a size (CSS Images 3 §5.1) — it used to be read as 24×24.
    assert_eq!(intrinsic_size(br#"<svg xmlns="..." width="16" height="9"></svg>"#), Some((16, 9)));
    assert_eq!(intrinsic_size(br#"<svg xmlns="..." viewBox="0 0 24 24"></svg>"#), None);
    assert_eq!(navigator_prerender::subresource::natural_size(br#"<svg xmlns="..." viewBox="0 0 24 24"></svg>"#).and_then(|n| n.ratio), Some((1, 1)));

    // ★ The control: a format with no header we understand is REPORTED as
    // unmeasured, never guessed at.
    assert_eq!(intrinsic_size(b"not an image at all"), None);
    assert_eq!(intrinsic_size(&[0xFF, 0xD8, 0xFF]), None, "a truncated JPEG is not a size");
}

#[test]
fn stylesheets_are_collected_and_imports_followed() {
    let dom = navigator_dom::parse(
        r#"<html><head><link rel="stylesheet" href="/a.css">
           <link rel="stylesheet" href="/dark.css" media="(prefers-color-scheme: dark)"></head><body></body></html>"#);
    let mut f = MapFetcher::default();
    f.0.insert("https://example.com/a.css".into(), "@import \"b.css\";\np { color: red }".into());
    f.0.insert("https://example.com/b.css".into(), "body { margin: 0 }".into());
    f.0.insert("https://example.com/dark.css".into(), "p { color: white }".into());
    let out = collect(&dom, Some("https://example.com/index.html"), &mut f);
    let hrefs: Vec<&str> = out.sheets.iter().map(|s| s.href.as_str()).collect();
    // The import is carried too, keyed as the importing sheet names it, and
    // it comes BEFORE the sheet that imported it — which is cascade order.
    assert_eq!(hrefs, vec!["/b.css", "/a.css", "/dark.css"], "{hrefs:?}");
    // ★ The media attribute travels with the sheet: without it a dark-mode
    // stylesheet is applied unconditionally.
    assert_eq!(out.sheets[2].media, "(prefers-color-scheme: dark)");
    assert_eq!(out.sheets[0].media, "");
}

#[test]
fn an_image_is_measured_and_addressed_and_a_failure_is_reported() {
    let dom = navigator_dom::parse(r#"<html><body><img src="/logo.svg"><img src="/missing.png"></body></html>"#);
    let mut f = MapFetcher::default();
    f.0.insert("https://example.com/logo.svg".into(), r#"<svg width="64" height="32"></svg>"#.into());
    let out = collect(&dom, Some("https://example.com/"), &mut f);
    assert_eq!(out.images.len(), 1);
    assert_eq!((out.images[0].natural.width, out.images[0].natural.height), (Some(64), Some(32)));
    assert!(out.images[0].address.starts_with("blake3:"), "the bytes are named by content: {:?}", out.images[0]);
    // The one that could not be fetched is REPORTED, not silently missing.
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.failed[0].0, "/missing.png");
}

/// An image the document already declares needs no fetch — it has arrived
/// measured, which is what §3.13 asks for.
#[test]
fn a_declared_size_survives_a_failed_fetch() {
    let dom = navigator_dom::parse(r#"<html><body><img src="/x.png" width="10" height="4"></body></html>"#);
    let mut f = MapFetcher::default();
    let out = collect(&dom, Some("https://example.com/"), &mut f);
    assert_eq!(out.images.len(), 1);
    assert_eq!((out.images[0].natural.width, out.images[0].natural.height), (Some(10), Some(4)));
    assert_eq!(out.images[0].address, "", "no bytes, so no address — the space is still reserved");
}

/// ★ An SVG's natural sizing comes from its ROOT element, each part
/// optional (CSS Images 3 §5.1): a width alone has NO ratio, a viewBox alone
/// is a ratio with no size, and nothing at all is measured-as-nothing.
#[test]
fn svg_natural_sizing_reads_the_root_and_keeps_absent_parts_absent() {
    use navigator_prerender::subresource::{natural_size, intrinsic_size, Natural};
    let n = |b: &[u8]| natural_size(b).expect("an image");
    // WPT's support/w100.svg: a width and nothing else.
    assert_eq!(n(br#"<svg style="background: green" xmlns="http://www.w3.org/2000/svg" width="100"></svg>"#),
               Natural { width: Some(100), height: None, ratio: None });
    // An icon: a viewBox only.
    assert_eq!(n(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 12"></svg>"#),
               Natural { width: None, height: None, ratio: Some((2, 1)) });
    // Both: their ratio. A nested element's size is not the image's.
    assert_eq!(n(br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="999" height="999"/></svg>"#),
               Natural { width: Some(20), height: Some(10), ratio: Some((2, 1)) });
    // A relative length is no natural size.
    assert_eq!(n(br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="2em"></svg>"#),
               Natural { width: None, height: None, ratio: None });
    // The Rust book's diagram: 1000×1300 is exactly 10/13, not 769/1000.
    assert_eq!(n(br#"<svg viewBox="0.00 0.00 1000.00 1300.00" xmlns="http://www.w3.org/2000/svg"></svg>"#).ratio, Some((10, 13)));
    assert_eq!(n(br#"<svg viewBox="0 0 1038 1342" xmlns="http://www.w3.org/2000/svg"></svg>"#).ratio, Some((519, 671)));
    // Nothing declared: measured, and natural size none.
    assert_eq!(n(br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="999" height="999"/></svg>"#),
               Natural { width: None, height: None, ratio: None });
    // The both-dimensions view is empty unless both are there.
    assert_eq!(intrinsic_size(br#"<svg xmlns="http://www.w3.org/2000/svg" width="100"></svg>"#), None);
    assert_eq!(intrinsic_size(br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"></svg>"#), Some((20, 10)));
}
