//! ★ THE RENDERER REFUSES WHAT THE PROFILE DOES NOT BOUND.
//!
//! Guarantee G3 is boundedness. A document outside the Document Profile is
//! exactly the one for which no bound was ever established, so the backend
//! refuses it rather than starting work it cannot promise to finish.
//!
//! The converter takes the opposite posture on the same numbers — it reports
//! and continues, because naming which ceiling a real page broke is how three
//! of those ceilings got corrected. Both sides read the ceilings from
//! `navigator_dom::profile`; only the decision differs.
//!
//! ★★ Every refusal test here is paired with an ACCEPTANCE test. A validator
//! that refuses everything passes every refusal test ever written, and the
//! profile checker in the converter did once check nothing at all while two
//! whole corpora reported clean.

use navigator_backend::document::Document;
use navigator_dom::profile;

/// The ordinary case: a real document is accepted, and its tree is there.
#[test]
fn a_reasonable_document_is_accepted() {
    let html = "<html><body><h1>Title</h1><p>Some prose.</p></body></html>";
    let d = Document::accept(html).expect("a plain document must be accepted");
    assert!(d.dom.by_tag("h1").len() == 1, "the tree must actually be parsed");
}

/// Depth: nesting past the ceiling is refused, and the refusal names the
/// ceiling and both numbers — a bare `false` cannot be acted on.
#[test]
fn a_document_deeper_than_the_ceiling_is_refused() {
    let depth = profile::MAX_DEPTH + 10;
    let html = format!("<html><body>{}{}</body></html>",
        "<div>".repeat(depth), "</div>".repeat(depth));
    let v = Document::accept(&html).expect_err("must be refused");
    let d = v.iter().find(|v| v.ceiling == "tree depth").expect("names the ceiling");
    assert!(d.measured > d.allowed, "{d}");
    assert_eq!(d.allowed, profile::MAX_DEPTH);
}

/// ★ AND THE PAIRED ACCEPTANCE. Just under the ceiling must pass, or the test
/// above proves only that `accept` can fail.
#[test]
fn a_document_just_inside_the_depth_ceiling_is_accepted() {
    // -2 leaves room for the html and body elements the parser supplies.
    let depth = profile::MAX_DEPTH - 2;
    let html = format!("<html><body>{}{}</body></html>",
        "<div>".repeat(depth), "</div>".repeat(depth));
    Document::accept(&html).expect("just inside the ceiling must be accepted");
}

/// Stylesheets are counted from the DOM — `<style>` blocks plus `<link>`
/// elements whose rel names one — so markup tricks do not change the count.
#[test]
fn too_many_stylesheets_is_refused_and_a_reasonable_number_is_not() {
    let many = format!("<html><head>{}</head><body></body></html>",
        "<link rel=stylesheet href=a.css>".repeat(profile::MAX_STYLESHEETS + 1));
    let v = Document::accept(&many).expect_err("must be refused");
    assert!(v.iter().any(|v| v.ceiling == "stylesheets per document"), "{v:?}");

    let few = format!("<html><head>{}</head><body></body></html>",
        "<link rel=stylesheet href=a.css>".repeat(8));
    Document::accept(&few).expect("eight stylesheets is an ordinary page");
}

/// A `<link>` that is not a stylesheet does not count toward the ceiling —
/// preload and icon links are numerous on real pages and bound nothing.
#[test]
fn non_stylesheet_links_do_not_count() {
    let html = format!("<html><head>{}</head><body></body></html>",
        "<link rel=preload href=a.woff2>".repeat(profile::MAX_STYLESHEETS + 50));
    Document::accept(&html).expect("preload links are not stylesheets");
    let d = Document::parse(&html);
    assert_eq!(profile::stylesheet_count(&d.dom), 0);
}

/// The byte ceiling the backend's ingest limit refuses on is the profile's
/// own, read from it rather than copied — so the two cannot drift apart and
/// leave a gap a document fits through.
#[test]
fn the_ingest_byte_limit_is_the_profiles_byte_ceiling() {
    assert_eq!(navigator_backend::Limits::default().max_document_bytes,
               profile::MAX_DOCUMENT_BYTES);
}

/// ★ `parse` deliberately does NOT enforce the profile: a tool that needs to
/// look at an out-of-bounds document must still be able to. The distinction
/// is easy to erase by "tidying" the two into one, so it is pinned.
#[test]
fn parse_does_not_refuse_what_accept_does() {
    let depth = profile::MAX_DEPTH + 10;
    let html = format!("<html><body>{}{}</body></html>",
        "<div>".repeat(depth), "</div>".repeat(depth));
    assert!(Document::accept(&html).is_err());
    let d = Document::parse(&html); // must not panic, must produce the tree
    assert!(d.dom.max_depth() > profile::MAX_DEPTH);
}
