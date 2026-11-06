//! Inline `<svg>` made standalone (src/svg_inline.rs): the page's cascade
//! resolved into the SVG's own attributes, and the normalizer turning the
//! box into a replaced element when the recording names the blob.
use navigator_normalize::{normalize, svg_inline::standalone, Inputs};

fn inputs_with(css: &str) -> Inputs {
    let mut i = Inputs::default();
    i.stylesheets.insert("/s.css".into(), css.into());
    i
}
const PAGE: &str = r##"<html><head><link rel=stylesheet href="/s.css"></head><body>
<button class=btn><svg class=ico width="16" height="16" viewBox="0 0 16 16"><path class=p d="M0 0h16v16z" fill="currentColor"/></svg></button>
<svg style="display:none"><symbol id=star viewBox="0 0 10 10"><path d="M5 0L10 10H0z"/></symbol></svg>
<p><svg class=ico width="10" height="10"><use href="#star"/></svg></p>
</body></html>"##;

#[test]
fn paint_comes_from_the_page_and_current_color_from_the_html_around() {
    let out = standalone(PAGE, &inputs_with(".btn { color: #ff0000 } .ico .p { stroke: blue; stroke-width: 2 }"));
    assert_eq!(out.len(), 3, "{out:#?}");
    let first = &out[0];
    assert!(first.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""), "{first}");
    assert!(first.contains("fill=\"#ff0000\""), "currentColor resolved through the button's color: {first}");
    assert!(first.contains("stroke=\"blue\"") && first.contains("stroke-width=\"2\""), "{first}");
    assert!(!first.contains("class=") && !first.contains("currentColor"), "{first}");
}

#[test]
fn a_symbol_used_from_elsewhere_in_the_document_is_copied_in() {
    let out = standalone(PAGE, &inputs_with(""));
    assert!(out[2].contains("<defs><symbol id=\"star\""), "{}", out[2]);
    assert!(out[2].contains("<use href=\"#star\">"), "{}", out[2]);
}

#[test]
fn the_normalizer_paints_the_blob_only_when_the_recording_names_it() {
    let mut i = inputs_with(".btn { color: #ff0000 }");
    let (plain, _) = normalize(PAGE, &i);
    assert!(plain.contains("<path"), "no blob named: the svg is left as it was");
    for t in standalone(PAGE, &i) {
        let a = format!("blake3:{}", blake3::hash(t.as_bytes()).to_hex());
        i.images.insert(a, navigator_normalize::ImageSize { width: Some(16), height: Some(16), ratio: Some((1, 1)) });
    }
    let (made, _) = normalize(PAGE, &i);
    assert!(made.contains("src=\"blake3:"), "{made}");
    assert!(!made.contains("<path d=\"M0 0h16v16z\""), "its children went into the blob: {made}");
}

/// ★ EXTERNAL SPRITES: `<use href="/sprite.svg#icon">`, with the file
/// carried in the recording (`Inputs::sprites`), becomes a self-contained
/// SVG the worker's strict profile accepts and draws as vector shapes —
/// the symbol, and what it references inside the file, copied in.
const SPRITE_PAGE: &str = r##"<html><head><link rel=stylesheet href="/s.css"></head><body><a class=go><svg class=ico width="24" height="24"><use xlink:href="/img/sprite.svg#arrow"></use></svg></a>
<svg width="24" height="24"><use href="/img/sprite.svg#grad-dot"/></svg>
<svg width="24" height="24"><use href="/img/missing.svg#x"/></svg></body></html>"##;
const SPRITE: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
  <symbol id="arrow" viewBox="0 0 24 24"><path d="M2 12h16l-6-6M18 12l-6 6" fill="none" stroke="currentColor" stroke-width="2"/></symbol>
  <linearGradient id="g"><stop offset="0" stop-color="#ff0000"/><stop offset="1" stop-color="#0000ff"/></linearGradient>
  <symbol id="grad-dot" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10" fill="url(#g)"/></symbol>
  <symbol id="unused" viewBox="0 0 24 24"><rect width="24" height="24"/></symbol>
</svg>"##;

#[test]
fn an_external_sprite_is_copied_in_and_drawn_as_vectors() {
    let mut i = inputs_with(".go { color: #00aa00 }");
    i.sprites.insert("/img/sprite.svg".into(), SPRITE.into());
    let out = standalone(SPRITE_PAGE, &i);
    assert_eq!(out.len(), 3, "{out:#?}");
    // The arrow: its symbol copied under a renamed id, the use pointing at it,
    // currentColor resolved through the link's colour.
    let a = &out[0];
    assert!(a.contains("<defs><symbol id=\"atrium-sprite-"), "{a}");
    assert!(a.contains("href=\"#atrium-sprite-") && !a.contains("sprite.svg"), "the use points at the copy: {a}");
    assert!(a.contains("stroke=\"#00aa00\""), "currentColor from the page around the use: {a}");
    assert!(!a.contains("unused"), "only what is named is copied: {a}");
    // The dot: its gradient, referenced inside the file, comes with it.
    let d = &out[1];
    assert!(d.contains("<linearGradient id=\"atrium-sprite-") && d.contains("fill=\"url(#atrium-sprite-"), "{d}");
    // Both pass the worker's profile and draw.
    for svg in [a, d] {
        let (tree, _) = navigator_svg::load(svg.as_bytes()).unwrap_or_else(|e| panic!("refused: {e:?}\n{svg}"));
        let fills = navigator_svg::vector::fills(&tree).expect("vector");
        assert!(!fills.is_empty(), "drawn as shapes: {svg}");
    }
    // A file the recording does not carry is left as written: refused, as before.
    assert!(out[2].contains("href=\"/img/missing.svg#x\""), "{}", out[2]);
    assert!(navigator_svg::load(out[2].as_bytes()).is_err(), "still refused");
    // Without the file, the arrow is refused too (the control).
    let plain = standalone(SPRITE_PAGE, &inputs_with(".go { color: #00aa00 }"));
    assert!(navigator_svg::load(plain[0].as_bytes()).is_err());
}

#[test]
fn sprite_ids_do_not_collide_across_files_and_the_output_is_reproducible() {
    let page = r##"<html><body><svg width="8" height="8"><use href="/a.svg#i"/></svg><svg width="8" height="8"><use href="/b.svg#i"/></svg></body></html>"##;
    let mut i = Inputs::default();
    i.sprites.insert("/a.svg".into(), r##"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="i" viewBox="0 0 8 8"><rect width="8" height="8" fill="#ff0000"/></symbol></svg>"##.into());
    i.sprites.insert("/b.svg".into(), r##"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="i" viewBox="0 0 8 8"><rect width="8" height="8" fill="#0000ff"/></symbol></svg>"##.into());
    let out = standalone(page, &i);
    assert!(out[0].contains("#ff0000") && out[1].contains("#0000ff"), "{out:#?}");
    let id = |s: &str| s.split("<symbol id=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    assert_ne!(id(&out[0]), id(&out[1]), "two files' #i are different symbols");
    assert_eq!(out, standalone(page, &i), "the converter and the normalizer write the same bytes");
}

#[test]
fn a_recording_carries_its_sprites() {
    let json = format!("{{\n  \"format\": \"atrium-navigator-recording/3\",\n  \"url\": \"https://x.test/\",\n  \"stylesheets\": [],\n  \"sprites\": [\n    {{ \"href\": \"/img/sprite.svg\", \"text\": {:?} }}\n  ],\n  \"images\": [],\n  \"document\": \"<p>x</p>\"\n}}\n", SPRITE);
    let r = navigator_normalize::recording::parse(&json).unwrap();
    assert_eq!(r.inputs.sprites.get("/img/sprite.svg").map(String::as_str), Some(SPRITE));
}
