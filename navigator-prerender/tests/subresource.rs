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

/// ★ EXTERNAL SVG SPRITES (2026-11-06): the file an inline
/// `<use href="file.svg#id">` names is fetched once, carried keyed as the
/// document writes it, and the icon's standalone SVG — the blob the
/// recording names — has the symbol copied in.
#[test]
fn an_external_sprite_file_is_fetched_once_and_the_icon_carries_its_symbol() {
    let dom = navigator_dom::parse(r##"<html><body>
        <svg width="16" height="16"><use href="/img/s.svg#a"></use></svg>
        <svg width="16" height="16"><use xlink:href="/img/s.svg#b"></use></svg>
        <svg width="16" height="16"><use href="/img/gone.svg#c"></use></svg></body></html>"##);
    let mut f = MapFetcher::default();
    f.0.insert("https://example.com/img/s.svg".into(),
        r##"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="a" viewBox="0 0 16 16"><rect width="16" height="16" fill="#ff0000"/></symbol><symbol id="b" viewBox="0 0 16 16"><circle cx="8" cy="8" r="8" fill="#0000ff"/></symbol></svg>"##.into());
    navigator_prerender::subresource::install_blob_sink();
    let out = collect(&dom, Some("https://example.com/page"), &mut f);
    let blobs = navigator_prerender::subresource::take_blobs();
    assert_eq!(out.sprites.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(), vec!["/img/s.svg"], "one file, fetched once");
    assert!(out.failed.iter().any(|(h, why)| h == "/img/gone.svg" && why.starts_with("sprite:")), "{:?}", out.failed);
    // The inline icons' blobs (images named blake3:…): the two with a
    // carried file have their symbols.
    let svgs: Vec<&str> = out.images.iter().filter(|i| i.src.starts_with("blake3:")).map(|i| i.src.as_str()).collect();
    assert_eq!(svgs.len(), 3, "{:?}", out.images);
    let text = |a: &str| String::from_utf8(blobs.iter().find(|(x, _)| x == a).expect("stored").1.clone()).unwrap();
    let all: Vec<String> = svgs.iter().map(|a| text(a)).collect();
    assert!(all.iter().any(|t| t.contains("<symbol id=\"atrium-sprite-") && t.contains("#ff0000")), "icon a has its symbol: {all:#?}");
    assert!(all.iter().any(|t| t.contains("<symbol id=\"atrium-sprite-") && t.contains("#0000ff")), "icon b has its symbol");
    assert!(all.iter().any(|t| t.contains("/img/gone.svg#c")), "the missing file's icon is left as written");
}

/// The FIRST display does not wait for sprite files (it does not wait for
/// images either): its icons fill in with the readable recording.
#[test]
fn the_first_display_fetches_no_sprite_file() {
    let dom = navigator_dom::parse(r##"<html><body><svg width="16" height="16"><use href="/img/s.svg#a"></use></svg></body></html>"##);
    let mut f = MapFetcher::default();
    f.0.insert("https://example.com/img/s.svg".into(), r##"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="a"><rect width="16" height="16"/></symbol></svg>"##.into());
    let out = navigator_prerender::subresource::collect_first(&dom, Some("https://example.com/page"), &mut f);
    assert!(out.sprites.is_empty());
    let full = collect(&dom, Some("https://example.com/page"), &mut f);
    assert_eq!(full.sprites.len(), 1);
}

/// ★ `srcset` at one density for every reader (resolution-independence.md
/// §10): the 2x candidate is fetched whatever screen reads the page, kept
/// under the `src` the document names it by, and sized at its density, so it
/// takes the room its 1x `src` would.
#[test]
fn a_srcset_image_is_fetched_at_two_x_for_everyone_and_sized_in_css_px() {
    let dom = navigator_dom::parse(
        r#"<html><body><img src="/one.svg" srcset="/one.svg 1x, /two.svg 2x, /three.svg 3x"></body></html>"#);
    let mut f = MapFetcher::default();
    // Only the 2x candidate is there: fetching another would fail.
    f.0.insert("https://example.com/two.svg".into(), r#"<svg width="128" height="64"></svg>"#.into());
    let out = collect(&dom, Some("https://example.com/"), &mut f);
    assert!(out.failed.is_empty(), "{:?}", out.failed);
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].src, "/one.svg", "keyed as the document names it");
    assert_eq!((out.images[0].natural.width, out.images[0].natural.height), (Some(64), Some(32)), "128 px at 2x is 64 CSS px");
}

/// A `<picture>`'s first source that applies (a type we decode, a media
/// that holds at the conversion viewport) gives the candidates, at 2x.
#[test]
fn a_picture_source_is_picked_at_two_x_too() {
    let dom = navigator_dom::parse(r#"<html><body><picture>
        <source type="image/avif" srcset="/a.avif 2x">
        <source media="(max-width: 600px)" srcset="/narrow.svg 2x">
        <source srcset="/wide1.svg 1x, /wide2.svg 2x">
        <img src="/fallback.svg"></picture></body></html>"#);
    let mut f = MapFetcher::default();
    f.0.insert("https://example.com/wide2.svg".into(), r#"<svg width="200" height="100"></svg>"#.into());
    let out = collect(&dom, Some("https://example.com/"), &mut f);
    assert!(out.failed.is_empty(), "{:?}", out.failed);
    assert_eq!(out.images[0].src, "/fallback.svg");
    assert_eq!((out.images[0].natural.width, out.images[0].natural.height), (Some(100), Some(50)));
}
