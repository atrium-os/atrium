//! ★ The normalizer's acceptance test is the profile itself (§6: "its output
//! is checkable"). Every test here renders the OUTPUT and asserts the
//! renderer reports nothing — and each one carries a control showing the
//! same input is refused before normalization, so a test that stopped
//! normalizing anything could not pass quietly.

use navigator_normalize::{normalize, normalize_and_measure, Inputs};
use navigator_render::fontset::FontSet;
use navigator_render::html::render_html;
use navigator_style::cascade::Env;

fn refusals(html: &str) -> Vec<String> {
    let fonts = FontSet::load().expect("pinned font set");
    render_html(html, &fonts, &Env::default()).diagnostics.iter()
        .map(|d| format!("{}: {}", d.code, d.msg)).collect()
}

/// Everything the corpus refused, in one document.
const HOSTILE: &str = r#"<html><head><style>
  :root { --brand: #cf222e; --pad: 4px }
  @media print { body { color: #ff0000 } }
  @media (min-width: 600px) { .card p { margin: 8px } }
  @container (min-width: 10px) { p { color: #00ff00 } }
  article div p { margin: 1px 2px 3px; color: var(--brand) }
  .card > p:first-child { padding: var(--pad) }
  p:not(.skip) { text-align: left }
  a:hover { text-decoration: underline }
  p::before { content: "x" }
  h1 + p { font-weight: bold !important }
  td { float: left; width: 10pt }
  .card { background: #eaeef2 url(bg.png) no-repeat center / cover; border: 1px solid var(--brand) }
</style></head><body>
  <article><div class="card"><h1>Title</h1><p class="skip" style="color: #0969da; margin-top: 9px">one</p><p>two</p></div></article>
  <table><tr><td>a</td><td>a longer cell</td></tr><tr><td>b</td><td>c</td></tr></table>
</body></html>"#;

#[test]
fn the_normalizer_output_is_accepted_by_the_profile() {
    // Control: the input is refused, and refused a lot.
    let before = refusals(HOSTILE);
    assert!(before.len() > 10, "the control must actually be refused: {before:?}");
    let fonts = FontSet::load().expect("pinned font set");
    let (out, report) = normalize_and_measure(HOSTILE, &Inputs::default(), &fonts, &Env::default());
    let after = refusals(&out);
    assert!(after.is_empty(), "normalized output must be accepted:\n{after:#?}\n\n{out}");
    assert!(report.rules_out > 0, "something was emitted");
    assert_eq!(report.columns_measured, 2, "the table's columns were measured offline");
}

#[test]
fn the_cascade_is_resolved_the_way_css_says() {
    let src = r#"<html><head><style>
      p { color: #ff0000 }
      .c { color: #00ff00 }
      #i { color: #0000ff }
      p { color: #ffff00 !important }
    </style></head><body><p id="i" class="c">x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    // !important beats the id, however specific the id is.
    assert!(out.contains("color: #ffff00"), "{out}");
    assert!(!out.contains("color: #0000ff"), "the id must not win over !important: {out}");
}

#[test]
fn an_inline_style_beats_a_rule_and_survives_as_a_class() {
    let src = r#"<html><head><style>p { color: #ff0000 }</style></head>
                 <body><p style="color: #0969da">x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("color: #0969da"), "{out}");
    assert!(!out.contains("style="), "the inline attribute is gone: {out}");
    assert!(refusals(&out).is_empty());
}

#[test]
fn a_state_rule_keeps_its_state() {
    let src = r#"<html><head><style>a:hover { color: #cf222e }</style></head>
                 <body><a href="x">link</a></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains(":hover {"), "the state travels with the rule: {out}");
    assert!(refusals(&out).is_empty());
}

#[test]
fn shorthands_expand_to_the_right_sides() {
    let src = r#"<html><head><style>p { margin: 1px 2px 3px }</style></head><body><p>x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    for want in ["margin-top: 1px", "margin-right: 2px", "margin-bottom: 3px", "margin-left: 2px"] {
        assert!(out.contains(want), "missing {want} in {out}");
    }
}

/// ★ What cannot be represented is DROPPED AND REPORTED. Silence would be
/// the only real failure, because the reader would never know.
#[test]
fn what_cannot_be_represented_is_reported() {
    let src = r#"<html><head><style>p { float: left } p::after { content: "x" }</style></head>
                 <body><p>x</p><img src="cat.png"></body></html>"#;
    let (_, report) = normalize(src, &Inputs::default());
    let keys: Vec<&String> = report.dropped.keys().collect();
    assert!(keys.iter().any(|k| k.contains("float")), "{keys:?}");
    assert!(keys.iter().any(|k| k.contains("pseudo-element")), "{keys:?}");
    assert!(keys.iter().any(|k| k.contains("intrinsic size")), "{keys:?}");
}

/// An image whose size the input DOES supply is kept, with the size declared
/// — §3.13's rule is "arrive measured", not "be thrown away".
#[test]
fn a_measured_image_is_kept() {
    let src = r#"<html><body><img src="cat.png"></body></html>"#;
    let mut inputs = Inputs::default();
    inputs.images.insert("cat.png".into(), (40, 20));
    let (out, _) = normalize(src, &inputs);
    assert!(out.contains("width=\"40\""), "{out}");
    assert!(out.contains("height=\"20\""), "{out}");
}

/// ★ The bug the RENDER found, not the tests: custom properties taken as
/// "the last `--x` in the file" pick up whatever a dark-mode block set, and
/// every page comes out in its dark palette. They must go through the
/// cascade, per element and per media context.
#[test]
fn a_dark_mode_custom_property_stays_in_its_media_context() {
    let src = r#"<html><head><style>
      :root { --bg: #ffffff }
      @media (prefers-color-scheme: dark) { :root { --bg: #000000 } }
      body { background-color: var(--bg) }
    </style></head><body><p>x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    // The light value is the one outside any media block...
    let base: Vec<&str> = out.lines().take_while(|l| !l.starts_with("@media")).collect();
    assert!(base.iter().any(|l| l.contains("background-color: #ffffff")), "base must be light:
{out}");
    assert!(!base.iter().any(|l| l.contains("background-color: #000000")), "dark must not leak into the base:
{out}");
    // ...and the dark one is inside the media block, not lost.
    assert!(out.contains("@media") && out.contains("#000000"), "the dark value survives in its context:
{out}");
    assert!(refusals(&out).is_empty());
}

/// A `<link media="...">` conditions the WHOLE sheet. Ignoring it applies a
/// dark-mode or print stylesheet unconditionally.
#[test]
fn a_links_media_attribute_conditions_its_sheet() {
    let src = r#"<html><head><link rel="stylesheet" href="dark.css" media="(prefers-color-scheme: dark)"></head>
                 <body><p>x</p></body></html>"#;
    let mut inputs = Inputs::default();
    inputs.stylesheets.insert("dark.css".into(), "p { color: #000000 }".into());
    let (out, _) = normalize(src, &inputs);
    let base: Vec<&str> = out.lines().take_while(|l| !l.starts_with("@media")).collect();
    assert!(!base.iter().any(|l| l.contains("#000000")), "a conditioned sheet must not apply unconditionally:\n{out}");
    assert!(out.contains("@media (prefers-color-scheme: dark)"), "{out}");
}

/// ★ The real CSS is usually behind an `@import`: the W3C's stylesheet for a
/// spec is 123 bytes and one import. Dropping it renders the page as though
/// the sheet had never been fetched.
#[test]
fn an_import_is_followed() {
    let src = r#"<html><head><link rel="stylesheet" href="a.css"></head><body><p>x</p></body></html>"#;
    let mut inputs = Inputs::default();
    inputs.stylesheets.insert("a.css".into(), "@import \"b.css\";\n".into());
    inputs.stylesheets.insert("b.css".into(), "p { color: #0969da }".into());
    let (out, _) = normalize(src, &inputs);
    assert!(out.contains("color: #0969da"), "the imported rule must land:\n{out}");
}
