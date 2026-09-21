//! ★ VERIFY THE VERIFIER. A ceiling checker that reports zero having checked
//! nothing is worse than no checker: it converts an unexamined assumption
//! into apparent evidence. The first version of this one silently checked
//! nothing — the wiring had not been applied — and reported a clean corpus,
//! which is exactly what a working one reports. These tests exist so that
//! cannot happen again.

use navigator_prerender::{boa_impl::BoaEngine, convert, profile};

fn parse_only(html: &str) -> navigator_prerender::Conversion {
    convert(html, &mut BoaEngine::default())
}

#[test]
fn a_conforming_document_has_no_violations() {
    let c = parse_only("<html><head><link rel=stylesheet href=/a.css></head>\
        <body><p>An ordinary document.</p></body></html>");
    assert!(c.profile_violations.is_empty(), "{:?}", c.profile_violations);
}

#[test]
fn too_many_stylesheets_is_caught() {
    let links = "<link rel=stylesheet href=/a.css>".repeat(profile::MAX_STYLESHEETS + 44);
    let c = parse_only(&format!("<html><head>{links}</head><body><p>x</p></body></html>"));
    let v = c.profile_violations.iter()
        .find(|v| v.ceiling == "stylesheets per document")
        .unwrap_or_else(|| panic!("not caught: {:?}", c.profile_violations));
    assert_eq!(v.allowed, profile::MAX_STYLESHEETS);
    assert!(v.measured > profile::MAX_STYLESHEETS);
}

/// ★ The ceiling this test defends was 1,024 until it was measured: four
/// times looser than the project's own derivation rule produces. A document
/// 400 deep conforms under the old value and does not under the corrected
/// one, so this test would have passed vacuously before.
#[test]
fn too_deep_a_tree_is_caught() {
    let deep = "<div>".repeat(400) + "x" + &"</div>".repeat(400);
    let c = parse_only(&format!("<html><body>{deep}</body></html>"));
    let v = c.profile_violations.iter()
        .find(|v| v.ceiling == "tree depth")
        .unwrap_or_else(|| panic!("not caught: {:?}", c.profile_violations));
    assert_eq!(v.allowed, 256);
    assert!(v.measured >= 400, "{v:?}");
}

#[test]
fn an_oversized_document_is_caught() {
    // Padding inside a comment keeps the element and depth counts small, so
    // only the BYTE ceiling can fire — each violation is tested alone.
    let pad = "x".repeat(profile::MAX_DOCUMENT_BYTES + 1024);
    let c = parse_only(&format!("<html><body><p>hi</p><!--{pad}--></body></html>"));
    let names: Vec<&str> = c.profile_violations.iter().map(|v| v.ceiling).collect();
    assert_eq!(names, vec!["total document bytes"], "{:?}", c.profile_violations);
}

/// Stylesheets are counted from the DOM, so a `<link>` that is not a
/// stylesheet does not count — and neither does one the markup never closed,
/// because the parser decides what the document contains.
#[test]
fn only_actual_stylesheets_are_counted() {
    let c = parse_only("<html><head>\
        <link rel=icon href=/favicon.ico>\
        <link rel=preconnect href=https://x.test>\
        <link rel=stylesheet href=/a.css>\
        <style>p{}</style></head><body><p>x</p></body></html>");
    let dom = navigator_prerender::parse::parse(
        "<html><head>\
        <link rel=icon href=/favicon.ico>\
        <link rel=preconnect href=https://x.test>\
        <link rel=stylesheet href=/a.css>\
        <style>p{}</style></head><body><p>x</p></body></html>");
    assert_eq!(profile::stylesheet_count(&dom), 2, "icon and preconnect are not stylesheets");
    assert!(c.profile_violations.is_empty());
}

/// ★ A violation is a property of what was SERVED, not of what our
/// conversion made of it — so it is measured on the parsed document, before
/// any script runs. A page that builds 300 stylesheets at runtime has not
/// been served a non-conforming document.
#[test]
fn violations_describe_the_served_document_not_our_conversion() {
    let c = parse_only("<html><head></head><body><script>\
        for (var i = 0; i < 300; i++) {\
          var l = document.createElement('link');\
          l.rel = 'stylesheet'; l.setAttribute('href', '/x' + i + '.css');\
          document.head.appendChild(l);\
        }</script></body></html>");
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.profile_violations.is_empty(),
        "the SERVED document conformed: {:?}", c.profile_violations);
}
