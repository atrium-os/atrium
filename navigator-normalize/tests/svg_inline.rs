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
