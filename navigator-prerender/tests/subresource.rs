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

    // SVG carries its size in the markup, by attribute or by viewBox.
    assert_eq!(intrinsic_size(br#"<svg xmlns="..." width="16" height="9"></svg>"#), Some((16, 9)));
    assert_eq!(intrinsic_size(br#"<svg xmlns="..." viewBox="0 0 24 24"></svg>"#), Some((24, 24)));

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
    assert_eq!((out.images[0].width, out.images[0].height), (64, 32));
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
    assert_eq!((out.images[0].width, out.images[0].height), (10, 4));
    assert_eq!(out.images[0].address, "", "no bytes, so no address — the space is still reserved");
}
