//! ★ SERIALIZE -> REPARSE MUST BE A FIXED POINT.
//!
//! This is not an aesthetic property. The converter serializes its DOM into a
//! recording; the backend re-parses those bytes with THIS parser and walks the
//! tree positionally to resolve a recorded trigger. If parsing the serializer's
//! output produces a different tree — or if serializing it again produces
//! different bytes — the two sides disagree about the same document and the
//! disagreement surfaces as a trigger that silently fails to resolve.
//!
//! It did. Two separate causes, both found by comparing round 1 to round 2 on
//! real recordings rather than by reading the code:
//!
//!   1. `<script>` and `<style>` text was HTML-escaped on the way out, so
//!      `(()=>{` came back as `(()=&gt;{` and then `(()=&amp;gt;{`. Embedded
//!      JavaScript was corrupted and the document grew ~250 KB per round.
//!   2. `<noscript>`, which is raw text when scripting is enabled — and it is,
//!      here — had the same problem at a smaller scale.
//!
//! And one that ran the other way: html5ever lowercases attribute names, while
//! the converter's DOM kept whatever case a script assigned, so a
//! `tabIndex="-1"` written by the converter reparsed as `tabindex="-1"`.
//!
//! Each of these is cheap to reintroduce and expensive to notice, so each gets
//! a test that fails on the FIRST round rather than on a corpus.

use navigator_dom::{dom::Kind, parse};

fn stable(html: &str) -> (String, String) {
    let once = parse(html).serialize();
    let twice = parse(&once).serialize();
    (once, twice)
}

/// Script text is raw text: it must come out byte-for-byte, or the artifact
/// ships broken JavaScript.
#[test]
fn script_text_is_not_escaped_and_survives_a_round_trip() {
    let src = r#"<html><body><script>var f = (()=>{ return a < b && c > d; });</script></body></html>"#;
    let (once, twice) = stable(src);
    assert!(once.contains("(()=>{"), "script text was escaped: {once}");
    assert!(!once.contains("&gt;"), "script text was escaped: {once}");
    assert_eq!(once, twice, "serialization is not a fixed point");
}

#[test]
fn style_text_is_not_escaped_either() {
    let src = "<html><head><style>a > b { content: \"x\" }</style></head><body></body></html>";
    let (once, twice) = stable(src);
    assert!(once.contains("a > b"), "{once}");
    assert_eq!(once, twice);
}

/// noscript is raw text with scripting enabled, which is how this parser runs.
#[test]
fn noscript_content_does_not_grow_each_round() {
    let src = "<html><body><noscript><iframe src=\"x\" title=\"T\"></iframe></noscript></body></html>";
    let (once, twice) = stable(src);
    assert_eq!(once, twice, "noscript content re-escapes every round");
    assert!(!once.contains("&amp;lt;"), "double-escaped: {once}");
}

/// ★ THE COUNTERWEIGHT: textarea and title are ESCAPABLE raw text. Entities
/// are decoded there on parse, so they must be re-escaped on output. A fix for
/// the script case that swept these in with it would silently turn displayed
/// text into markup.
#[test]
fn textarea_and_title_stay_escaped() {
    let src = "<html><head><title>a &lt; b</title></head><body><textarea>x &lt; y</textarea></body></html>";
    let (once, twice) = stable(src);
    assert!(once.contains("a &lt; b"), "title lost its escaping: {once}");
    assert!(once.contains("x &lt; y"), "textarea lost its escaping: {once}");
    assert_eq!(once, twice);
}

/// Attribute names on HTML elements are lowercased, as `setAttribute` does —
/// so the DOM holds what a reparse of its own output would hold.
#[test]
fn html_attribute_names_are_lowercased() {
    let mut d = parse("<html><body><div id=x></div></body></html>");
    let h = d.by_id("x").expect("the div");
    d.set_attr(h, "tabIndex", "-1");
    assert_eq!(d.attr(h, "tabindex"), Some("-1"), "stored under the lowercase name");
    assert_eq!(d.attr(h, "tabIndex"), Some("-1"), "and still found by the spelling used");
    let (once, twice) = stable(&d.serialize());
    assert!(once.contains(r#"tabindex="-1""#), "{once}");
    assert_eq!(once, twice);

    // Removal has to normalize identically or it misses what set_attr stored.
    d.remove_attr(h, "TABINDEX");
    assert_eq!(d.attr(h, "tabindex"), None, "removal missed the normalized name");
}

/// ★ AND NOT IN FOREIGN CONTENT. SVG attribute case is significant: lowercasing
/// `viewBox` leaves a graphic that no renderer will scale.
#[test]
fn svg_attribute_names_keep_their_case() {
    let d = parse(r#"<html><body><svg viewBox="0 0 8 8"><path d="M0 0"/></svg></body></html>"#);
    let svg = d.by_tag("svg").first().copied().expect("the svg element");
    assert!(d.is_foreign(svg), "svg must be marked foreign content");
    assert_eq!(d.attr(svg, "viewBox"), Some("0 0 8 8"), "case was destroyed on parse");
    let out = d.serialize();
    assert!(out.contains(r#"viewBox="0 0 8 8""#), "{out}");

    // A script-set attribute on that element keeps its case too.
    let mut d = d;
    d.set_attr(svg, "gradientUnits", "userSpaceOnUse");
    assert_eq!(d.attr(svg, "gradientUnits"), Some("userSpaceOnUse"));
}

/// A plain element is not foreign, so the exemption cannot leak sideways.
#[test]
fn a_div_is_not_foreign_content() {
    let d = parse("<html><body><div></div></body></html>");
    let div = d.by_tag("div").first().copied().expect("the div");
    assert!(!d.is_foreign(div));
    assert!(matches!(d.get(div).map(|n| &n.kind), Some(Kind::Element(t)) if t == "div"));
}

/// Grafting carries the namespace flag, not just the attributes.
#[test]
fn grafting_preserves_foreign_content() {
    let frag = navigator_dom::parse_fragment(r#"<svg viewBox="0 0 4 4"></svg>"#);
    let svg = frag.by_tag("svg").first().copied().expect("the svg");
    assert!(frag.is_foreign(svg), "fragment parsing must mark it");

    let mut host = navigator_dom::parse("<html><body><div id=h></div></body></html>");
    let target = host.by_id("h").unwrap();
    let g = host.graft(&frag, svg);
    host.append(target, g);
    assert!(host.is_foreign(g), "graft dropped the namespace");
    host.set_attr(g, "preserveAspectRatio", "none");
    assert_eq!(host.attr(g, "preserveAspectRatio"), Some("none"));
    assert!(host.serialize().contains(r#"viewBox="0 0 4 4""#), "{}", host.serialize());
}

/// ★ `serialized_len` is what the profile's byte ceiling is checked against
/// after every applied transition, so it must be the length of what
/// `serialize` would produce — never an estimate. Every branch that writes
/// differently is covered: escaped text and attributes (all four entities),
/// raw text left alone, void elements, comments dropped, multibyte text.
#[test]
fn serialized_len_is_exactly_the_serialized_length() {
    for html in [
        "",
        "<p>plain</p>",
        r#"<p title="a&b<c>d&quot;e">x &amp; y &lt; z &gt; w "q"</p>"#,
        "<script>if (a < b && c > d) { s = \"&amp;\" }</script>",
        "<style>a > b { content: \"&\" }</style><noscript><iframe></noscript>",
        "<br><img src=x alt='<>'><input value=\"&\">",
        "<!-- gone --><div>é — 日本 &nbsp;</div>",
        "<textarea>&lt;kept escaped&gt;</textarea><title>a & b</title>",
    ] {
        let d = navigator_dom::parse(html);
        assert_eq!(d.serialized_len(), d.serialize().len(), "{html:?}");
    }
}
