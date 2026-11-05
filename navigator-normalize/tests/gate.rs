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
    let src = r#"<html><head><style>body { padding: 1px } p { margin: 1px 2px 3px }</style></head><body><p>x</p></body></html>"#;
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
    inputs.images.insert("cat.png".into(), (40, 20).into());
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

/// ★ Logical properties are how modern CSS is written — `padding-block`
/// appears 5024 times in a 29-document corpus — and the profile's rows are
/// physical. Dropping them as "unknown" silently removes the layout.
#[test]
fn logical_properties_become_physical_ones() {
    let src = r#"<html><head><style>
      p { margin-inline-start: 10px; padding-block: 4px 8px; inline-size: 50px }
    </style></head><body><p>x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    for want in ["margin-left: 10px", "padding-top: 4px", "padding-bottom: 8px", "width: 50px"] {
        assert!(out.contains(want), "missing {want} in {out}");
    }
    assert!(refusals(&out).is_empty());
}

/// ★ A state belongs to the element it is written on. `#t:checked ~ .p` styles
/// the SIBLING, so emitting `:checked` on the sibling names a state it can
/// never have — and `:checked` is decidable from the DOM anyway.
#[test]
fn a_static_state_is_resolved_and_a_misplaced_one_is_refused() {
    let src = r#"<html><head><style>
      #t:checked ~ .p { margin-left: 30px }
      #u:checked ~ .q { margin-left: 40px }
      #v:hover ~ .r { margin-left: 50px }
    </style></head><body>
      <input id="t" type="checkbox" checked><div class="p">a</div>
      <input id="u" type="checkbox"><div class="q">b</div>
      <input id="v" type="checkbox"><div class="r">c</div>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("margin-left: 30px"), "a checked sibling applies: {out}");
    assert!(!out.contains("margin-left: 40px"), "an unchecked one does not: {out}");
    assert!(!out.contains("margin-left: 50px"), "a dynamic state on a sibling cannot be expressed: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("dynamic state")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty());
}

/// The root's own attributes carry meaning — `lang` and `dir` decide language
/// and base direction, and the page's cascade keys on its classes.
#[test]
fn the_root_keeps_its_attributes() {
    let src = r#"<html lang="fr" dir="rtl" class="js dark"><body><p>x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains(r#"lang="fr""#), "{out}");
    assert!(out.contains(r#"dir="rtl""#), "{out}");
    assert!(out.contains("js dark"), "{out}");
}

/// ★ `display: contents` is a STRUCTURAL instruction, not a value: the
/// element generates no box and its children take its place. The normalizer
/// carries it out on the DOM, which is why the profile needs no such value.
#[test]
fn display_contents_splices_the_children_into_the_parent() {
    let src = r#"<html><head><style>
      .wrap { display: contents }
      .a { color: #ff0000 } .b { color: #00ff00 }
    </style></head><body><div class="outer"><div class="wrap"><p class="a">one</p><p class="b">two</p></div></div></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(!out.contains("display: contents"), "never emitted: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("children took its place")), "{:?}", report.dropped);
    // Both children survive, and the wrapper's own box is gone.
    assert!(out.contains("color: #ff0000") && out.contains("color: #00ff00"), "{out}");
    let wrap_at = out.find("class=\"wrap");
    assert!(wrap_at.is_none(), "the wrapper element is gone: {out}");
    assert!(refusals(&out).is_empty());
}

/// ★ Named grid areas resolve to NUMBERED lines, which the profile admits.
/// Without it every child lands in the same cell — rustdoc's breadcrumb
/// rendered on top of its search box.
#[test]
fn named_grid_areas_become_numbered_lines() {
    let src = r#"<html><head><style>
      .g { display: grid; grid-template-areas: "crumbs crumbs" "title toolbar"; grid-template-columns: 100px 100px }
      .c { grid-area: crumbs } .t { grid-area: title } .b { grid-area: toolbar }
      .x { grid-area: nowhere }
    </style></head><body><div class="g">
      <div class="c">a</div><div class="t">b</div><div class="b">c</div><div class="x">d</div>
    </div></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(!out.contains("grid-template-areas"), "never emitted: {out}");
    assert!(!out.contains("grid-area"), "never emitted: {out}");
    // crumbs spans both columns of row 1; toolbar is row 2, column 2.
    assert!(out.contains("grid-column-end: 3") && out.contains("grid-row-end: 2"), "crumbs spans the row: {out}");
    assert!(out.contains("grid-column-start: 2") && out.contains("grid-row-start: 2"), "toolbar is row 2 col 2: {out}");
    // A name no template defines is reported, not guessed.
    assert!(report.dropped.keys().any(|k| k.contains("nowhere")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty());
}

/// ★ Named grid lines are written by every real track list and the profile
/// places by number. Dropping the declaration over the names left MDN's page
/// with one implicit column and its sidebar on top of its content.
#[test]
fn named_grid_lines_are_stripped_not_refused() {
    let src = r#"<html><head><style>
      .g { display: grid; grid-template-columns: [full-start side-start] 200px [side-end body-start] minmax(0, 1fr) [body-end full-end] }
    </style></head><body><div class="g"><div>a</div><div>b</div></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("grid-template-columns: 200px minmax(0, 1fr)"), "tracks kept, names gone: {out}");
    assert!(refusals(&out).is_empty());
}

/// ★ A media feature written with `calc()` over absolute units is
/// computable, and dropping it renders the wrong layout entirely: MDN
/// switches to its mobile layout at
/// `(width < calc(1rem * 2 + (15rem + 2rem) * 2 + 31rem))` = 1072px.
#[test]
fn a_media_query_with_calc_is_folded() {
    let src = r#"<html><head><style>
      @media (width < calc(1rem * 2 + (15rem + 2rem) * 2 + 31rem)) { p { color: #ff0000 } }
      @media (width >= calc(50rem)) { p { color: #00ff00 } }
      @media (width < calc(50% + 10px)) { p { color: #0000ff } }
    </style></head><body><p>x</p></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("max-width: 1071.98px"), "1072px, strict: {out}");
    assert!(out.contains("min-width: 800px"), "50rem: {out}");
    // A percentage depends on what the query is deciding, so it is dropped.
    assert!(!out.contains("#0000ff"), "{out}");
    assert!(report.dropped.keys().any(|k| k.contains("@media")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty());
}

/// ★ A masked box is drawn THROUGH its mask: the colour is the ink, the
/// mask is the shape. With no mask in the profile, painting the fill
/// unmasked turns every icon into a solid square.
#[test]
fn a_masked_box_paints_neither_fill_nor_image() {
    let src = r#"<html><head><style>
      .icon { background-color: #404244; mask-image: url(chevron.svg); width: 10px; height: 10px }
      .plain { background-color: #404244; width: 10px; height: 10px }
    </style></head><body><span class="icon"></span><span class="plain"></span></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    // The plain box keeps its colour; the masked one does not.
    assert_eq!(out.matches("background-color: #404244").count(), 1, "only the unmasked box is filled:\n{out}");
    assert!(report.dropped.keys().any(|k| k.contains("masked box")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty());
}

/// ★ A ROW of same-direction floats is a horizontal strip and becomes one;
/// a float with unfloated company is not, and keeps being dropped. Without
/// the first half GitHub's Watch/Fork/Star buttons stacked one per line.
#[test]
fn a_float_row_becomes_a_strip_but_a_lone_float_does_not() {
    let row = r#"<html><head><style>
      li { float: left }
    </style></head><body><ul><li>a</li><li>b</li><li>c</li></ul></body></html>"#;
    let (out, report) = normalize(row, &Inputs::default());
    assert!(out.contains("display: inline-block"), "the strip is laid out in a line: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("float row")), "{:?}", report.dropped);

    // Counterweight: one floated image beside text is NOT a strip — the text
    // is meant to wrap around it, which inline-block would not do.
    let lone = r#"<html><head><style>
      .fig { float: left }
    </style></head><body><div><span class="fig">img</span><p>text</p></div></body></html>"#;
    let (out, report) = normalize(lone, &Inputs::default());
    assert!(!out.contains("display: inline-block"), "a lone float is not promoted: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("not a row")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty());
}

/// ★ CASCADE LAYERS. Primer, Bootstrap 5.3+ and Tailwind v4 put their whole
/// stylesheet inside `@layer`, so dropping the at-rule drops the stylesheet:
/// GitHub's `a { text-decoration: none }` never reached the cascade and
/// every button came out underlined.
#[test]
fn cascade_layers_are_honoured_not_dropped() {
    let src = r##"<html><head><style>
      @layer base, theme;
      @layer base { a { color: #ff0000 } p { color: #ff0000 } }
      @layer theme { a { color: #00ff00 } }
      a.later { color: #0000ff }
    </style></head><body><a class="later" href="#">x</a><p>y</p></body></html>"##;
    let (out, _) = normalize(src, &Inputs::default());
    // The rules inside the layers survive at all…
    assert!(out.contains("color: #ff0000"), "layered rules are kept: {out}");
    // …a later layer beats an earlier one…
    assert!(out.contains(".n1 { color: #0000ff }"), "the link's only colour is the unlayered one: {out}");
    // …and an UNLAYERED declaration beats both, however specific they are.
    assert!(out.contains("color: #0000ff"), "unlayered wins: {out}");
    assert!(refusals(&out).is_empty());
}

/// For `!important` the layer order reverses: unlayered important is the
/// weakest, and an earlier layer beats a later one.
#[test]
fn important_reverses_the_layer_order() {
    let src = r#"<html><head><style>
      @layer first, second;
      @layer first { p { color: #ff0000 !important } }
      @layer second { p { color: #00ff00 !important } }
      p { color: #0000ff !important }
    </style></head><body><p>x</p></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("color: #ff0000"), "the EARLIER layer wins for important: {out}");
    assert!(!out.contains("#0000ff"), "unlayered important is the weakest: {out}");
}

/// ★ A CUSTOM PROPERTY NAME IS CASE-SENSITIVE. Every modern design-token
/// sheet is camelCase (`--fgColor-default`, `--button-default-fgColor-rest`),
/// and lowercasing the name made all of them unresolvable — silently, since
/// a `var()` with no definition and no fallback left an EMPTY value that
/// read like a declaration nobody wrote. GitHub's buttons lost their colour,
/// their background and their border to this.
#[test]
fn a_camelcase_custom_property_resolves_and_a_missing_one_is_reported() {
    let src = r#"<html data-mode="light"><head><style>
      [data-mode=light] { --control-fgColor-rest: #25292e;
                          --button-default-fgColor-rest: var(--control-fgColor-rest) }
      .btn { color: var(--button-default-fgColor-rest, var(--color-btn-text)) }
    </style></head><body><a class="btn" href="/x">f</a></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("color: #25292e"), "the camelCase token resolves through its chain: {out}");

    // Counterweight: an undefined property with no fallback is DROPPED and
    // SAID SO, rather than leaving an empty declaration behind.
    let src = r#"<html><head><style>.btn { color: var(--nobody-defines-this) }</style></head>
                 <body><a class="btn" href="/x">f</a></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(!out.contains("color:"), "{out}");
    assert!(report.dropped.keys().any(|k| k.contains("unresolved custom property")), "{:?}", report.dropped);
}

/// ★ A table cell's `width` is a FLOOR in automatic table layout (CSS 2.1
/// §17.5.2.2), not a cap. The W3C specs declare `th { width: 3em }` on their
/// property tables; taken as exact, the header column was 48 px and
/// "Initial:" and "Inherited:" painted over the values beside them.
#[test]
fn a_cells_declared_width_is_a_floor_not_a_cap() {
    let fonts = FontSet::load().expect("pinned font set");
    let src = |w: &str| format!(r#"<html><head><style>
      table {{ border-collapse: collapse }} th {{ width: {w} }}
    </style></head><body><table><tr><th>Inheritedness:</th><td>no</td></tr></table></body></html>"#);
    let col0 = |out: &str| -> f64 {
        let rule = out.split(".tc0 {").nth(1).expect("column measured");
        rule.split("min-width: ").nth(1).unwrap().split("px").next().unwrap().parse().unwrap()
    };
    let (narrow, _) = normalize_and_measure(&src("10px"), &Inputs::default(), &fonts, &Env::default());
    assert!(col0(&narrow) > 60.0, "the word decides, not the 10px: {}", col0(&narrow));
    assert!(narrow.contains(r#"<col class="tc0">"#), "the columns are declared by <col>, not by the cells: {narrow}");
    // What the reader sees: the value starts AFTER the header's word ends.
    let o = render_html(&narrow, &fonts, &Env::default());
    let run = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.x).expect(t);
    assert!(run("no") > run("Inheritedness:") + 60 * 64, "the value is not painted under the header");
    // Control: an author width WIDER than the content still holds.
    let (wide, _) = normalize_and_measure(&src("300px"), &Inputs::default(), &fonts, &Env::default());
    // 302, not 300: the author wrote a CONTENT-box width, and the UA gives a
    // cell 1px of padding each side — the border box a browser draws.
    assert_eq!(col0(&wide), 302.0);
    assert!(refusals(&narrow).is_empty() && refusals(&wide).is_empty());
}

/// ★ COLSPAN, end to end. Every Wikipedia navbox opens with a title cell
/// across both columns; declared from the first row, the table had ONE
/// column, and every later row's list cell ran off the canvas. The
/// normalizer now declares columns with `<col>` and settles spanning cells.
#[test]
fn a_navbox_with_a_spanning_title_row_is_declared_and_fits() {
    let fonts = FontSet::load().expect("pinned font set");
    let words = "Bob Fabry, Keith Bostic, Bill Joy, Marshall Kirk McKusick, Kirk McKusick ".repeat(3);
    let src = format!(r#"<html><head><style>table {{ width: 100% }}</style></head><body><table>
        <tr><th colspan="2">Berkeley Software Distribution</th></tr>
        <tr><th>People</th><td>{words}</td></tr>
        <tr><th>Companies</th><td>Sleepycat</td></tr></table></body></html>"#);
    // Control: the raw document cannot be laid out by the profile.
    assert!(!refusals(&src).is_empty(), "the control must be refused");
    let (out, report) = normalize_and_measure(&src, &Inputs::default(), &fonts, &Env::default());
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    assert_eq!(report.columns_measured, 2, "two columns, though the first row has one cell");
    assert_eq!(out.matches("<col class=").count(), 2, "{out}");
    let o = render_html(&out, &fonts, &Env::default());
    let run = |t: &str| o.scene.runs.iter().find(|r| r.text.starts_with(t)).expect(t).clone();
    let (people, bob, companies) = (run("People"), run("Bob"), run("Companies"));
    // Beside, in the same row: the label is middle-aligned against the
    // wrapped list (HTML's UA `tr { vertical-align: middle }`), so its
    // baseline is at or below the list's first line, above the next row.
    assert!(bob.x > people.x && bob.y <= people.y && people.y < companies.y, "the list sits BESIDE its label");
    assert!(o.scene.runs.iter().all(|r| r.x < 800 * 64), "nothing starts past the canvas edge");

    // A spanning cell wider than its columns makes them grow to fit it.
    let wide = r#"<html><body><table><tr><td>a</td><td>b</td></tr>
        <tr><td colspan="2">averyveryverylongunbreakablewordthatneedsroom</td></tr></table></body></html>"#;
    let (out, _) = normalize_and_measure(wide, &Inputs::default(), &fonts, &Env::default());
    let min = |k: usize| -> f64 { out.split(&format!(".tc{k} {{")).nth(1).unwrap().split("min-width: ").nth(1).unwrap()
        .split("px").next().unwrap().parse().unwrap() };
    assert!(min(0) + min(1) > 250.0, "the two columns together hold the long word: {} + {}", min(0), min(1));
}

/// ★ A CSS table (`display: table` on a `div` or `ul`) cannot hold `<col>` —
/// the HTML parser drops it outside a real `<table>` — so its columns are
/// declared on its first row's cells. Inserting `<col>` there left
/// Wikipedia's portal box with undeclared columns, and it was refused. And
/// `colspan` means nothing on a `div`: only an HTML cell spans.
#[test]
fn a_css_table_declares_on_its_first_row_and_does_not_span() {
    let fonts = FontSet::load().expect("pinned font set");
    let src = r#"<html><head><style>
      .t { display: table } .r { display: table-row } .c { display: table-cell }
    </style></head><body><div class="t">
      <div class="r"><div class="c" colspan="2">left heading</div><div class="c">right</div></div>
      <div class="r"><div class="c">a</div><div class="c">b</div></div></div></body></html>"#;
    let (out, report) = normalize_and_measure(src, &Inputs::default(), &fonts, &Env::default());
    assert!(refusals(&out).is_empty(), "{:?}\n{out}", refusals(&out));
    assert_eq!(report.columns_measured, 2, "two columns: the div's colspan does not count");
    assert!(!out.contains("<col"), "no <col> outside a real table: {out}");
    let o = render_html(&out, &fonts, &Env::default());
    let x = |t: &str| o.scene.runs.iter().find(|r| r.text == t).expect(t).x;
    assert_eq!(x("right"), x("b"), "`right` is in column 2, above `b` — the div did not span");
}

/// ★ BOX-SIZING, compiled away. The profile is border-box everywhere; CSS's
/// default is content-box. Dropping `box-sizing` (51,846 times across the
/// corpus) made every padded content-box box narrower by its padding.
#[test]
fn content_box_sizes_are_compiled_to_border_box() {
    let fonts = FontSet::load().expect("pinned font set");
    let norm = |css: &str, body: &str| normalize(&format!("<html><head><style>{css}</style></head><body>{body}</body></html>"), &Inputs::default()).0;

    // The default: content-box. 300 + 20 + 20 + 2 (border) = 342.
    let out = norm(".b { width: 300px; padding-left: 20px; padding-right: 20px; border-left: 2px solid #000; background-color: #ff0000 }",
                   r#"<div class="b">x</div>"#);
    assert!(out.contains("width: 342px"), "{out}");
    let o = render_html(&out, &fonts, &Env::default());
    let red: Vec<_> = o.scene.rects.iter().filter(|r| r.rgba == 0xff0000ff).collect();
    assert_eq!(red[0].w, 342 * 64, "the box is the size the author meant");

    // Border-box, directly and through the `inherit` idiom: left as written.
    for css in ["* { box-sizing: border-box }", "html { box-sizing: border-box } *, *::before { box-sizing: inherit }"] {
        let out = norm(&format!("{css} .b {{ width: 300px; padding-left: 20px }}"), r#"<div class="b">x</div>"#);
        assert!(out.contains("width: 300px") && !out.contains("320px"), "{css}: {out}");
    }
    // A percentage with em padding becomes calc().
    let out = norm(".b { width: 50%; padding-left: 1em; padding-right: 1em }", r#"<div class="b">x</div>"#);
    assert!(out.contains("width: calc(50% + 1em + 1em)"), "{out}");
    // The UA's own padding counts: a <ul> has 40px on the left.
    let out = norm("ul { width: 300px }", "<ul><li>x</li></ul>");
    assert!(out.contains("width: 340px"), "{out}");
    // A breakpoint that changes only the padding still gets its own width.
    let out = norm(".b { width: 300px; padding-left: 10px } @media (max-width: 900px) { .b { padding-left: 30px } }",
                   r#"<div class="b">x</div>"#);
    assert!(out.contains("width: 310px") && out.contains("width: 330px"), "{out}");
    // Keywords are not sizes: auto stays auto.
    let out = norm(".b { width: auto; padding-left: 10px }", r#"<div class="b">x</div>"#);
    assert!(!out.contains("calc(auto"), "{out}");
    assert!(refusals(&out).is_empty());
}

/// `-webkit-box-sizing` is an alias in Chrome and Safari; a page that sets
/// only the prefixed form is border-box there.
#[test]
fn the_webkit_prefixed_box_sizing_is_an_alias() {
    let src = r#"<html><head><style>.b { -webkit-box-sizing: border-box; width: 300px; padding-left: 20px }</style></head>
        <body><div class="b">x</div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("width: 300px") && !out.contains("320px"), "{out}");
}

/// ★ An image declares only the natural sizing it HAS. A width-only SVG
/// arrives with `width` and `natural-ratio="none"`, no height, and lays out
/// the way a browser does: its width kept under `max-height`.
#[test]
fn an_image_declares_only_the_natural_sizing_it_has() {
    let fonts = FontSet::load().expect("pinned font set");
    let mut inputs = Inputs::default();
    inputs.images.insert("w100.svg".into(), navigator_normalize::ImageSize { width: Some(100), height: None, ratio: None });
    inputs.images.insert("icon.svg".into(), navigator_normalize::ImageSize { width: None, height: None, ratio: Some((1, 1)) });
    inputs.images.insert("photo.png".into(), (200, 100).into());
    let src = r#"<html><head><style>img { display: block; background-color: #ff0000 } .m { max-height: 70px }</style></head>
        <body><img class="m" src="w100.svg"><img src="icon.svg"><img src="photo.png"></body></html>"#;
    let (out, _) = normalize(src, &inputs);
    assert!(out.contains(r#"src="w100.svg" width="100" natural-ratio="none""#) || (out.contains(r#"width="100""#) && out.contains(r#"natural-ratio="none""#)), "{out}");
    assert!(out.contains(r#"natural-ratio="1/1""#), "{out}");
    assert!(out.contains(r#"width="200""#) && out.contains(r#"height="100""#), "{out}");
    let o = render_html(&out, &fonts, &Env::default());
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    let sizes: Vec<(i64, i64)> = o.scene.rects.iter().filter(|r| r.rgba == 0xff0000ff).map(|r| (r.w / 64, r.h / 64)).collect();
    assert_eq!(sizes, vec![(100, 70), (150, 150), (200, 100)], "width kept; ratio alone in 300×150; a raster image its own size");
}

/// ★ CSS Variables 1: a custom property of `initial` is the guaranteed-
/// invalid value, and a property referencing it is invalid too, so `var()`
/// takes its fallback. The postcss light-dark polyfill depends on it; taking
/// `initial` literally produced `color: initial #a4cefe` (15,972 drops).
#[test]
fn a_guaranteed_invalid_custom_property_falls_back() {
    let src = r#"<html><head><style>
      :root { --csstools-color-scheme--light: initial }
      @media (prefers-color-scheme: dark) { :root { --csstools-color-scheme--light: ; } }
      p { --toggle: var(--csstools-color-scheme--light) #000000; color: var(--toggle, #a4cefe) }
      em { --a: var(--b); --b: var(--a); color: var(--a, #00aa00) }
    </style></head><body><p>x <em>y</em></p></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    let base: Vec<&str> = out.lines().take_while(|l| !l.starts_with("@media")).collect();
    assert!(base.iter().any(|l| l.contains("color: #a4cefe")), "light: the toggle is invalid, so the fallback wins:\n{out}");
    assert!(out.contains("color: #000000"), "dark: the toggle is ` #000000`:\n{out}");
    assert!(base.iter().any(|l| l.contains("color: #00aa00")), "a cycle is invalid, so the fallback wins:\n{out}");
    assert!(!out.contains("initial #"), "{out}");
    assert!(!report.dropped.keys().any(|k| k.contains("initial")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty());
}

/// ★ A media query LIST is an OR, and the renderer evaluates lists:
/// `screen, print` applies everywhere; `print` leaves an OR; the output of
/// every form must still be accepted by the profile.
#[test]
fn media_query_lists_are_admitted_member_by_member() {
    let src = r#"<html><head><style>
      @media screen, print { p { color: #111111 } }
      @media print, (max-width: 2000px) { em { color: #222222 } }
      @media (max-width: 100px), (orientation: landscape) { b { color: #333333 } }
      @media only print, only all and (prefers-color-scheme: no-preference) { i { color: #444444 } }
    </style></head><body><p>a <em>b</em> <b>c</b> <i>d</i></p></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    let r = refusals(&out);
    assert!(r.is_empty(), "the profile accepts what the normalizer emits: {r:?}\n{out}");
    let base: Vec<&str> = out.lines().take_while(|l| !l.starts_with("@media")).collect();
    assert!(base.iter().any(|l| l.contains("#111111")), "screen, print: unconditional\n{out}");
    assert!(out.contains("#222222") && out.contains("#333333"), "{out}");
    assert!(!out.contains("#444444"), "an obsolete value never matches, so its rules go: {out}");
    assert!(!report.dropped.keys().any(|k| k.contains("screen, print")), "{:?}", report.dropped);
}

/// ★ Static pseudo-classes the normalizer can decide from the DOM:
/// `:lang()` (4,749 drops in the corpus), `:dir()`, `:nth-of-type()` (which
/// was counted as `:nth-child`), and `:where()`, whose specificity is ZERO.
#[test]
fn static_selectors_lang_dir_nth_of_type_and_where() {
    let src = r#"<html lang="en-GB"><head><style>
      :lang(en) .a { color: #111111 }
      :lang(fr) .a { color: #990000 }
      :lang("*-GB") .gb { color: #121212 }
      .r:dir(ltr) { color: #990000 }
      .r:dir(rtl) { color: #222222 }
      /* (`:dir(ltr) .r` would ALSO match here, via <html>: a descendant
         selector tests every ancestor, in a browser too.) */
      li:nth-of-type(2) { color: #333333 }
      :where(.w) { color: #990000 }
      p { color: #444444 }
      :-webkit-any(.k) { color: #555555 }
    </style></head><body>
      <p class="a">en</p><p class="gb">gb</p>
      <div dir="rtl"><span class="r">rtl</span></div>
      <ul><h2>not an li</h2><li>one</li><li class="second">two</li></ul>
      <p class="w">where</p><span class="k">k</span>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(!report.dropped.keys().any(|k| k.contains("not understood")), "{:?}", report.dropped);
    assert!(!out.contains("#990000"), "no rule for the wrong language or direction, and :where() loses to `p`:\n{out}");
    for c in ["#111111", "#121212", "#222222", "#333333", "#444444", "#555555"] { assert!(out.contains(c), "missing {c}:\n{out}") }
    // nth-of-type counts only <li>: the second <li> is `.second`, not the first.
    let o = render_html(&out, &FontSet::load().unwrap(), &Env::default());
    let color_of = |t: &str| o.scene.runs.iter().find(|r| r.text == t).map(|r| r.rgba).unwrap();
    assert_eq!(color_of("two"), 0x333333ff, "the second li");
    assert_ne!(color_of("one"), 0x333333ff, "the first li is not nth-of-type(2), though it is nth-child(2)");
    assert_eq!(color_of("where"), 0x444444ff, "`p` beats a zero-specificity :where(.w)");
    assert!(refusals(&out).is_empty());
}

/// ★ PRESENTATIONAL HINTS (HTML §15), below every author rule: a legacy
/// table's `width`/`bgcolor`/`cellpadding`/`border`/`align`/`valign`, and an
/// `<img>`'s own `width`/`height` — which a browser shows at that size.
#[test]
fn presentational_attributes_become_css_below_author_rules() {
    let fonts = FontSet::load().expect("pinned font set");
    let mut inputs = Inputs::default();
    inputs.images.insert("big.png".into(), (400, 200).into());
    let src = r##"<html><head><style>.author { background-color: #00ff00 }</style></head><body bgcolor="#ffffee">
      <table width="300" cellpadding="5" border="1" bgcolor="ff0000">
        <tr><td align="center" valign="top" width="100">a</td><td class="author" bgcolor="#0000ff">b</td></tr>
      </table>
      <img src="big.png" width="100" height="50">
    </body></html>"##;
    let (out, report) = normalize_and_measure(src, &inputs, &fonts, &Env::default());
    assert!(!report.dropped.keys().any(|k| k.starts_with("presentational attribute `")), "{:?}", report.dropped);
    for want in ["background-color: #ffffee", "background-color: #ff0000", "text-align: center", "vertical-align: top",
                 "padding-left: 5px", "border-left-width: 1px"] {
        assert!(out.contains(want), "missing {want}:\n{out}");
    }
    // An author rule beats the hint: the cell is green, not blue.
    assert!(out.contains("background-color: #00ff00") && !out.contains("#0000ff"), "{out}");
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    // The image is shown at the AUTHOR's size, not its natural 400×200.
    assert!(out.contains("width: 100px") && out.contains("height: 50px"), "{out}");
}

/// ★ Colour spellings the profile can say once expanded: `#rgba` (3,820
/// drops) and `light-dark()` (868) — light in the base, dark under
/// `prefers-color-scheme: dark`, exactly where a dark reader looks for it.
#[test]
fn rgba_hex_and_light_dark_colours_are_compiled() {
    let fonts = FontSet::load().expect("pinned font set");
    let src = r##"<html><head><style>
      p { color: light-dark(#0d0f12, #f2f5fa); border-top-color: #f008 }
      em { background-color: #0000 }
    </style></head><body><p>x <em>y</em></p></body></html>"##;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("border-top-color: #ff000088") && out.contains("background-color: #00000000"), "{out}");
    let base: Vec<&str> = out.lines().take_while(|l| !l.starts_with("@media")).collect();
    assert!(base.iter().any(|l| l.contains("color: #0d0f12")), "light in the base:\n{out}");
    assert!(!report.dropped.keys().any(|k| k.contains("light-dark") || k.contains("#f008") || k.contains("#0000`")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    // A DARK reader gets the dark value.
    let dark = Env { dark: true, ..Env::default() };
    let o = render_html(&out, &fonts, &dark);
    assert_eq!(o.scene.runs.iter().find(|r| r.text == "x").map(|r| r.rgba), Some(0xf2f5faff), "dark:\n{out}");
    let l = render_html(&out, &fonts, &Env::default());
    assert_eq!(l.scene.runs.iter().find(|r| r.text == "x").map(|r| r.rgba), Some(0x0d0f12ff));
}

/// ★ Inline SVG: the outer <svg>'s width/height size its box (≈1,900 icons
/// in the corpus had theirs dropped and collapsed to nothing), and every
/// attribute INSIDE it — geometry, not presentation — is kept.
#[test]
fn inline_svg_keeps_its_geometry_and_reserves_its_box() {
    let fonts = FontSet::load().expect("pinned font set");
    let src = r#"<html><head><style>svg { background-color: #ff0000 }</style></head><body>
      <p>icon <svg width="24" height="20" viewBox="0 0 24 20"><rect x="2" y="3" width="10" height="7"/></svg> after</p></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains(r#"width="10""#) && out.contains(r#"height="7""#), "the rect keeps its geometry: {out}");
    assert!(out.contains("width: 24px") && out.contains("height: 20px"), "the svg's box: {out}");
    assert!(!report.dropped.keys().any(|k| k.starts_with("presentational attribute `")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    let o = render_html(&out, &fonts, &Env::default());
    let red: Vec<(i64, i64)> = o.scene.rects.iter().filter(|r| r.rgba == 0xff0000ff).map(|r| (r.w / 64, r.h / 64)).collect();
    assert_eq!(red, vec![(24, 20)], "the icon reserves 24×20");
}

/// `unset` is `inherit` for an inherited property and `initial` otherwise.
#[test]
fn unset_compiles_to_inherit_or_initial() {
    let src = r#"<html><head><style>
      div { color: #ff0000; margin-left: 30px } p { color: unset; margin-left: unset }
    </style></head><body><div><p>x</p></div></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("color: inherit") && out.contains("margin-left: initial"), "{out}");
    assert!(!report.dropped.keys().any(|k| k.contains("unset")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// The `font` shorthand RESETS what it covers, and `background-position`
/// follows CSS's one- and two-value rules (1,388 declarations dropped before).
#[test]
fn font_and_background_position_shorthands_expand() {
    let src = r#"<html><head><style>
      div { font-weight: 700; font-style: italic }
      p { font: 16px/1.5 "IBM Plex Sans", sans-serif }
      em { font: italic bold 12px serif }
      b { font: inherit }
      .a { background-position: top }
      .b { background-position: right 10px }
      .c { background-position: bottom left }
    </style></head><body><div><p>x <em>y</em> <b>z</b></p></div>
      <span class="a">a</span><span class="b">b</span><span class="c">c</span></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(!report.dropped.keys().any(|k| k == "property `font`" || k == "property `background-position`"), "{:?}", report.dropped);
    // The shorthand reset the div's bold/italic on the <p>.
    let p_rule = out.lines().find(|l| l.contains("font-size: 16px")).expect("p's block");
    assert!(p_rule.contains("font-weight: 400") && p_rule.contains("font-style: normal") && p_rule.contains("line-height: 1.5"), "{p_rule}");
    assert!(out.contains("font-weight: 700") && out.contains("font-style: italic") && out.contains("font-size: 12px"), "{out}");
    for (x, y) in [("50%", "0%"), ("100%", "10px"), ("0%", "100%")] {
        assert!(out.contains(&format!("background-position-x: {x}; background-position-y: {y}")), "missing {x} {y}:\n{out}");
    }
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ Margins collapse (CSS 2.1 §8.3.1), compiled into literal margins: the
/// renderer never collapses, so each case is measured on the RENDERED
/// geometry — and a control renders the same page un-normalized, where the
/// margins add up.
#[test]
fn adjoining_margins_are_collapsed_into_literal_margins() {
    let fonts = FontSet::load().expect("pinned font set");
    let page = |css: &str, body: &str| format!("<html><head><style>body {{ margin: 0 }} .a {{ background-color: #ff0000; height: 10px }} .b {{ background-color: #0000ff; height: 10px }} {css}</style></head><body>{body}</body></html>");
    let y = |html: &str, rgba: u32| {
        let o = render_html(html, &fonts, &Env::default());
        o.scene.rects.iter().find(|r| r.rgba == rgba).map(|r| r.y / 64).expect("box painted")
    };
    let blue_y = |css: &str, body: &str| {
        let src = page(css, body);
        let (out, _) = normalize(&src, &Inputs::default());
        assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
        (y(&out, 0x0000ffff), y(&src, 0x0000ffff))
    };
    // Siblings: the larger margin wins. Control: literal margins add.
    let (got, control) = blue_y(".a { margin-bottom: 20px } .b { margin-top: 30px }", r#"<div class="a"></div><div class="b"></div>"#);
    assert_eq!(got, 40);
    assert!(control >= 60, "un-normalized, the margins add: {control}");
    // A negative margin subtracts from the largest positive.
    assert_eq!(blue_y(".a { margin-bottom: 20px } .b { margin-top: -5px }", r#"<div class="a"></div><div class="b"></div>"#).0, 25);
    // em is the element's OWN font size: 1em at 20px beside 1em at 10px.
    assert_eq!(blue_y(".a { margin-bottom: 1em; font-size: 20px } .b { margin-top: 1em; font-size: 10px }", r#"<div class="a"></div><div class="b"></div>"#).0, 30);
    // A first child's margin escapes a parent with no top border/padding…
    assert_eq!(blue_y(".p { margin-top: 10px } .b { margin-top: 25px }", r#"<div class="p"><div class="b"></div></div>"#).0, 25);
    // …and stays inside one that has padding (control for the escape rule).
    assert_eq!(blue_y(".p { margin-top: 10px; padding-top: 1px } .b { margin-top: 25px }", r#"<div class="p"><div class="b"></div></div>"#).0, 36);
    // A flex item's margin never collapses with its container's.
    assert_eq!(blue_y(".p { display: flex; margin-top: 10px } .b { margin-top: 25px; width: 10px }", r#"<div class="p"><div class="b"></div></div>"#).0, 35);
    // An empty box is transparent: its own margins join the chain.
    assert_eq!(blue_y(".a { margin-bottom: 10px } .e { margin: 40px 0 } .b { margin-top: 5px }", r#"<div class="a"></div><div class="e"></div><div class="b"></div>"#).0, 50);
    // Nested empty boxes: EVERY margin inside joins the chain. max(2, 14) + min(-4) = 10.
    assert_eq!(blue_y(".c1 { margin: 2px } .c2 { margin: -4px 0 } .c3 { margin: 0 0 14px }",
        r#"<div class="c1"><div class="c2"><div class="c3"></div></div></div><div class="b"></div>"#).0, 10);
    // A last child's bottom margin escapes too.
    assert_eq!(blue_y(".p .a { margin-bottom: 30px } .b { margin-top: 10px }", r#"<div class="p"><div class="a"></div></div><div class="b"></div>"#).0, 40);
    // A line of text separates: nothing collapses across it.
    let (got, _) = blue_y(".a { margin-bottom: 20px } .b { margin-top: 30px }", r#"<div class="a"></div>text<div class="b"></div>"#);
    assert!(got > 60, "text between: {got}");
}

/// Percentages cannot be added up in the normalizer: they stay symbolic, in
/// the one expression CSS defines for the collapsed value.
#[test]
fn a_percentage_margin_chain_stays_symbolic() {
    let src = r#"<html><head><style>.a { margin-bottom: 5% } .b { margin-top: 10px }</style></head>
        <body><div class="a">x</div><div class="b">y</div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("max(0px, 5%, 10px)"), "{out}");
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ A statically positioned box sits AFTER the margins before it have
/// collapsed; the compiled margins must keep it there. And a block inside an
/// inline joins the outer flow, margins and all.
#[test]
fn collapsing_keeps_static_positions_and_sees_blocks_in_inlines() {
    let fonts = FontSet::load().expect("pinned font set");
    let y = |html: &str, rgba: u32| render_html(html, &fonts, &Env::default()).scene.rects.iter().find(|r| r.rgba == rgba).map(|r| r.y / 64).expect("painted");
    // p (16px margins) · abs box · green box: both land 16px below p.
    let src = r#"<html><head><style>body { margin: 0 } p { margin: 16px 0; height: 20px }
        .abs { position: absolute; width: 10px; height: 10px; background-color: #ff0000 } .g { height: 10px; background-color: #00ff00 }</style></head>
        <body><p></p><div class="abs"></div><div class="g"></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert_eq!((y(&out, 0xff0000ff), y(&out, 0x00ff00ff)), (52, 52), "{out}");
    // The same with the abs box LAST: the escaping chain must not double.
    let src = r#"<html><head><style>body { margin: 0 } p { margin: 16px 0; height: 20px }
        .abs { position: absolute; width: 10px; height: 10px; background-color: #ff0000 }</style></head>
        <body><p></p><div class="abs"></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert_eq!(y(&out, 0xff0000ff), 52, "{out}");
    // <span><div 20px><div 100px>: one 100px gap, collapsed through the span.
    let src = r#"<html><head><style>body { margin: 0 } .g { height: 10px; background-color: #00ff00 }</style></head>
        <body><div><span><div style="margin-top: 20px"></div><div class="g" style="margin-top: 100px"></div>text</span></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert_eq!(y(&out, 0x00ff00ff), 100, "{out}");
    // max-height separates a parent's bottom from its last child's margin.
    let src = r#"<html><head><style>body { margin: 0 } .p { max-height: 50px } .c { height: 60px; margin-bottom: 10px } .g { height: 10px; background-color: #00ff00 }</style></head>
        <body><div class="p"><div class="c"></div></div><div class="g"></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert_eq!(y(&out, 0x00ff00ff), 50, "the margin stays inside the clamped parent: {out}");
}

/// An empty box holding an absolutely positioned child sits after the
/// margins before it (§8.3.1), and so does the child.
#[test]
fn an_empty_box_holding_an_abspos_child_keeps_its_place() {
    let fonts = FontSet::load().expect("pinned font set");
    let src = r#"<html><head><style>body { margin: 0 } p { margin: 16px 0; height: 20px } .r { position: relative }
        .a { position: absolute; width: 10px; height: 10px; background-color: #ff0000 }</style></head>
        <body><p></p><div class="r"><div class="a"></div></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let o = render_html(&out, &fonts, &Env::default());
    let y = o.scene.rects.iter().find(|r| r.rgba == 0xff0000ff).map(|r| r.y / 64);
    assert_eq!(y, Some(52), "{out}");
}

/// The visually-hidden idiom: `:not(:focus)` is about the element AT REST,
/// where nothing has focus, so the rule applies. It used to read the state
/// as "on" inside `:not()` and drop the rule, putting NHS's skip link on
/// top of its cookie banner.
#[test]
fn a_negated_dynamic_state_holds_at_rest() {
    let src = r#"<html><head><style>
      .skip { position: absolute; top: 16px; padding: 8px }
      .skip:not(:active):not(:focus) { width: 1px !important; height: 1px !important; overflow: hidden !important; padding: 0 !important }
      .menu:not(:hover) .tip { margin-left: 70px }
    </style></head><body>
      <a class="skip" href="/m">Skip to main content</a>
      <div class="menu"><span class="tip">t</span></div>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("width: 1px"), "the hiding rule applies at rest: {out}");
    assert!(out.contains("padding-top: 0"), "and beats the base padding: {out}");
    assert!(out.contains("margin-left: 70px"), "a negated state on an ancestor holds at rest too: {out}");
    assert!(refusals(&out).is_empty());
}

/// An EMPTY clip paints nothing, and the profile can say that even though
/// it has no clipping: Folha's hidden "Salvar artigos" drop-down was drawn
/// over its masthead. A real clip shape is still dropped, and `clip` on a
/// box that is not positioned does nothing, as in CSS.
#[test]
fn an_empty_clip_hides_and_a_real_one_is_dropped() {
    let src = r#"<html><head><style>
      .vh { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0) }
      .vh:focus { clip: auto }
      .cp { clip-path: inset(50%) }
      .flow { clip: rect(0, 0, 0, 0) }
      .shape { clip-path: circle(40%) }
    </style></head><body>
      <a class="vh" href="/x">hidden one</a>
      <p class="cp">hidden two</p>
      <p class="flow">shown, not positioned</p>
      <p class="shape">shown, a real shape</p>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    let hidden = out.matches("visibility: hidden").count();
    assert_eq!(hidden, 2, "the two empty clips hide, the others do not: {out}");
    assert!(out.contains("visibility: visible"), "focus lifts the clip: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("clip-path")), "the circle is still dropped: {:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// Named grid LINES become numbers, from the parent's template; the old
/// gap aliases are the gaps. Notion's page column (`content-start /
/// content-end`) landed in its 16px gutter without this.
#[test]
fn named_grid_lines_are_numbered_and_gap_aliases_hold() {
    let src = r#"<html><head><style>
      .layout { display: grid; grid-template-columns: [full-start] 16px [content-start] minmax(0, 1fr) [content-end] 16px [full-end]; grid-gap: 24px }
      .layout > * { grid-column: content-start / content-end }
      .wide { grid-column: full }
      .bad { grid-column: nowhere-start / nowhere-end }
    </style></head><body><div class="layout">
      <p>in the content column</p><p class="wide">full bleed</p><p class="bad">unplaced</p>
    </div></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("grid-column-start: 2") && out.contains("grid-column-end: 3"), "content-start/end -> 2/3: {out}");
    assert!(out.contains("grid-column-start: 1") && out.contains("grid-column-end: 4"), "`full` alone -> 1/4: {out}");
    assert!(out.contains("column-gap: 24px") && out.contains("row-gap: 24px"), "grid-gap is gap: {out}");
    assert!(!out.contains("[full-start]"), "the names themselves are gone: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("no parent template")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// The prefixed flexbox names are the standard properties: a page that
/// wrote only `-webkit-justify-content` meant `justify-content`, and when
/// both are written the later one wins, as for any one property. The
/// 2009 `-webkit-box-*` model is not an alias and stays dropped.
#[test]
fn prefixed_flexbox_aliases_are_the_standard_properties() {
    let src = r#"<html><head><style>
      .a { display: flex; -webkit-justify-content: space-between; -webkit-align-items: center; -webkit-flex-direction: column; -webkit-flex-wrap: wrap }
      .b { -webkit-flex: 1 1 0px; -webkit-order: 2; -webkit-align-self: flex-end }
      .c { display: flex; justify-content: flex-start; -webkit-justify-content: flex-end }
      .d { display: flex; -webkit-justify-content: flex-end; justify-content: center }
      .e { -webkit-box-flex: 1 }
    </style></head><body>
      <div class="a"><p class="b">one</p><p class="e">two</p></div>
      <div class="c"><p>three</p></div><div class="d"><p>four</p></div>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    for want in ["justify-content: space-between", "align-items: center", "flex-direction: column", "flex-wrap: wrap",
                 "flex-grow: 1", "flex-shrink: 1", "flex-basis: 0px", "align-self: flex-end",
                 "justify-content: flex-end", "justify-content: center"] {
        assert!(out.contains(want), "{want}: {out}");
    }
    assert!(!out.contains("justify-content: flex-start"), "the later -webkit- alias wins over the earlier standard name: {out}");
    assert_eq!(out.matches("justify-content: flex-end").count(), 1, "the later standard name wins over the earlier alias: {out}");
    assert!(!report.dropped.keys().any(|k| k.contains("-webkit-justify-content") || k.contains("-webkit-flex")), "{:?}", report.dropped);
    assert!(report.dropped.keys().any(|k| k == "property `order`"), "`order` is outside the profile either way: {:?}", report.dropped);
    assert!(report.dropped.keys().any(|k| k.contains("-webkit-box-flex")), "the old box model is not an alias: {:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// A minified compound query (`)and (`) is the same query.
#[test]
fn a_minified_compound_media_query_is_admitted() {
    let src = r#"<html><head><style>
      @media only screen and (min-width: 768px)and (max-width: 1023px) { p { margin-left: 33px } }
    </style></head><body><p>x</p></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("margin-left: 33px"), "{out}");
    assert!(!report.dropped.keys().any(|k| k.contains("not admitted")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// A ligature icon font draws a WORD as a glyph; without the font the word
/// is drawn. It becomes an empty icon-sized box, unless a later rule names
/// an ordinary family.
#[test]
fn a_ligature_icon_is_an_empty_icon_box() {
    let src = r#"<html><head><style>
      .material-icons { font-family: 'Material Icons'; font-size: 24px }
      .material-icons.plain { font-family: Arial, sans-serif }
    </style></head><body>
      <p>Back <span class="material-icons">keyboard_arrow_left</span></p>
      <p><span class="material-icons plain">ordinary words</span></p>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert_eq!(out.matches("visibility: hidden").count(), 1, "only the icon hides: {out}");
    assert!(out.contains("width: 1em") && out.contains("display: inline-block"), "{out}");
    assert!(report.dropped.keys().any(|k| k.contains("icon font")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// A media or state rule only wins where it outranks the base rule, as in
/// the source cascade: the output puts it later, so it must not be emitted
/// when it lost.
#[test]
fn a_conditional_rule_that_lost_is_not_emitted() {
    let src = r#"<html><head><style>
      .promo h3 { font-size: 16px; line-height: 16px }
      @media (max-width: 1200px) { h3 { font-size: 26px } }
      @media (max-width: 1200px) { .promo h3 { color: #ff0000 } }
      #x { color: #00ff00 }
      a:hover { color: #0000ff }
      #x:hover { text-decoration: underline }
    </style></head><body><div class="promo"><h3>t</h3></div><a id="x" href="/">l</a></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert!(!out.contains("font-size: 26px"), "the lower-specificity media rule lost: {out}");
    assert!(out.contains("#ff0000"), "a media rule that wins stays: {out}");
    assert!(!out.contains("#0000ff"), "the weaker :hover colour lost to the id rule: {out}");
    assert!(out.contains(":hover"), "the winning :hover declaration stays: {out}");
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// The declarations an element's generated classes carry in the base
/// context (top-level rules; media rules are indented).
fn base_props(out: &str, id: &str) -> std::collections::BTreeMap<String, String> {
    let at = out.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element #{id} in {out}"));
    let (open, close) = (out[..at].rfind('<').expect("tag"), at + out[at..].find('>').expect("tag"));
    let tag = &out[open..close];
    let classes = tag.split("class=\"").nth(1).and_then(|c| c.split('"').next()).unwrap_or("");
    let mut m = std::collections::BTreeMap::new();
    for c in classes.split_ascii_whitespace() {
        for line in out.lines().filter(|l| l.starts_with(&format!(".{c} {{"))) {
            let body = line.split('{').nth(1).and_then(|b| b.split('}').next()).unwrap_or("");
            for d in body.split(';') {
                if let Some((p, v)) = d.split_once(':') { m.insert(p.trim().to_string(), v.trim().to_string()); }
            }
        }
    }
    m
}

/// ★ `dir` is HTML's direction (the UA sheet's `[dir=rtl i] { direction:
/// rtl }`), below every author rule; `dir=auto` and `<bdi>` take the first
/// strong character's. Dropped, an Arabic page that says `<html dir="rtl">`
/// and sets no `direction` in CSS laid out left-to-right.
#[test]
fn the_dir_attribute_gives_the_direction() {
    let src = r#"<html dir="RTL" id="root"><head><style>.l { direction: ltr }</style></head><body>
      <p id="a">x</p><p dir="ltr" id="b">y</p><p dir="rtl" class="l" id="c">z</p>
      <p dir="auto" id="d">שלום world</p><p dir="auto" id="e">1. hello שלום</p><bdi id="f">עברית</bdi>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let dir = |id: &str| base_props(&out, id).get("direction").cloned();
    assert_eq!(dir("root").as_deref(), Some("rtl"), "{out}");
    assert_eq!(dir("a"), None, "a child inherits, it states nothing: {out}");
    assert_eq!(dir("b").as_deref(), Some("ltr"));
    assert_eq!(dir("c").as_deref(), Some("ltr"), "an author rule beats the attribute");
    assert_eq!(dir("d").as_deref(), Some("rtl"), "auto: the first strong character is Hebrew");
    assert_eq!(dir("e").as_deref(), Some("ltr"), "auto: digits are not strong; `h` is");
    assert_eq!(dir("f").as_deref(), Some("rtl"), "bdi is auto");
    assert!(refusals(&out).is_empty());
}

/// ★ An inline side is the element's OWN start or end: `margin-inline-start`
/// is `margin-right` in a right-to-left element. And it competes with the
/// physical property on that side by cascade priority, as CSS's logical
/// property groups do.
#[test]
fn logical_properties_follow_the_elements_direction() {
    let src = r#"<html><head><style>
      p { margin-inline-start: 10px; padding-inline: 1px 2px; inset-inline-end: 3px }
      #w { margin-left: 5px }
      #v { margin-right: 6px }
      #v { margin-inline-start: 7px }
    </style></head><body>
      <p id="l">x</p><div dir="rtl"><p id="r">y</p><p id="w">w</p><p id="v">v</p></div>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let (l, r, w, v) = (base_props(&out, "l"), base_props(&out, "r"), base_props(&out, "w"), base_props(&out, "v"));
    assert_eq!(l.get("margin-left").map(String::as_str), Some("10px"), "{out}");
    assert_eq!(r.get("margin-right").map(String::as_str), Some("10px"), "rtl start is the right: {out}");
    assert_eq!(r.get("margin-left"), None, "{out}");
    assert_eq!((r.get("padding-right").map(String::as_str), r.get("padding-left").map(String::as_str)), (Some("1px"), Some("2px")));
    assert_eq!(r.get("left").map(String::as_str), Some("3px"), "rtl inline-end is the left");
    assert_eq!((w.get("margin-left").map(String::as_str), w.get("margin-right").map(String::as_str)), (Some("5px"), Some("10px")),
        "on different sides they do not compete");
    assert_eq!(v.get("margin-right").map(String::as_str), Some("7px"), "the later, equally specific rule wins the right side: {out}");
    assert!(refusals(&out).is_empty());
}

/// ★ `text-align: left | right` are physical, the profile's values are
/// `start` / `end`; which one depends on the element's direction — and the
/// computed value inherits AS `left`, so a right-to-left child of a
/// `text-align: right` block is aligned to its START.
#[test]
fn physical_text_alignment_resolves_against_direction() {
    let src = r#"<html><head><style>
      #a { text-align: left } #b { text-align: right } #s { text-align: start }
    </style></head><body>
      <div id="a">x</div><div dir="rtl" id="ar"><div id="a2" style="text-align: left">y</div></div>
      <div id="b"><p dir="rtl" id="q">z</p><p id="same">same</p></div>
      <div id="s"><p dir="rtl" id="t">t</p></div>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let ta = |id: &str| base_props(&out, id).get("text-align").cloned();
    assert_eq!(ta("a").as_deref(), Some("start"), "{out}");
    assert_eq!(ta("a2").as_deref(), Some("end"), "left in rtl is the end: {out}");
    assert_eq!(ta("b").as_deref(), Some("end"));
    assert_eq!(ta("q").as_deref(), Some("start"), "an inherited `right` in an rtl child is its start: {out}");
    assert_eq!(ta("same"), None, "no change of direction, nothing to restate");
    assert_eq!(ta("t"), None, "`start` inherits as start");
    assert!(refusals(&out).is_empty());
}

/// ★ The UA's list and `dd` indents are on the START side (HTML §15:
/// `padding-inline-start: 40px`). In right-to-left content the bullets'
/// room is on the right.
#[test]
fn ua_list_indents_are_on_the_start_side_in_rtl() {
    let src = r#"<html dir="rtl"><head><style>ol { padding-inline-start: 20px }</style></head><body>
      <ul id="u"><li>a</li></ul><ol id="o"><li>b</li></ol><dl><dd id="d">c</dd></dl>
      <ul dir="ltr" id="l"><li>d</li></ul>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let (u, o, d, l) = (base_props(&out, "u"), base_props(&out, "o"), base_props(&out, "d"), base_props(&out, "l"));
    assert_eq!((u.get("padding-left").map(String::as_str), u.get("padding-right").map(String::as_str)), (Some("0px"), Some("40px")), "{out}");
    assert_eq!((o.get("padding-left").map(String::as_str), o.get("padding-right").map(String::as_str)), (Some("0px"), Some("20px")), "{out}");
    assert_eq!((d.get("margin-left").map(String::as_str), d.get("margin-right").map(String::as_str)), (Some("0px"), Some("40px")));
    assert_eq!(l.get("padding-right"), None, "an ltr list keeps the UA's left indent");
    assert!(refusals(&out).is_empty());
}

/// In a right-to-left parent a row of RIGHT floats is the strip (it runs
/// from the right edge, where an rtl inline-block row starts).
#[test]
fn a_row_of_right_floats_in_rtl_is_a_strip() {
    let src = r#"<html><head><style>li { float: right } .s { float: inline-start }</style></head><body>
      <ul dir="rtl"><li id="a">a</li><li>b</li></ul><ul><li id="b">c</li><li>d</li></ul>
      <ul dir="rtl"><li class="s" id="c">e</li><li class="s">f</li></ul>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    assert_eq!(base_props(&out, "a").get("display").map(String::as_str), Some("inline-block"), "{out}");
    assert_eq!(base_props(&out, "b").get("display"), None, "right floats in ltr are not a strip");
    assert_eq!(base_props(&out, "c").get("display").map(String::as_str), Some("inline-block"), "inline-start in rtl is right");
}

/// The inline-side border shorthands and the logical corners were dropped
/// as unknown — 314 and 2,054 declarations in the corpus: a blockquote's
/// `border-inline-start` rule simply vanished.
#[test]
fn inline_border_shorthands_and_logical_corners_expand() {
    let src = r#"<html><head><style>
      p { border-inline-start: 4px solid #ff0000; border-inline: 1px dotted; border-start-end-radius: 5px; border-end-start-radius: 6px }
      .e { border-inline-end: 2px dashed #00ff00 }
    </style></head><body><p id="l">x</p><div dir="rtl"><p id="r">y</p><p id="e" class="e">z</p></div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let (l, r, e) = (base_props(&out, "l"), base_props(&out, "r"), base_props(&out, "e"));
    let get = |m: &std::collections::BTreeMap<String, String>, k: &str| m.get(k).cloned().unwrap_or_default();
    assert_eq!((get(&l, "border-left-width"), get(&l, "border-left-style")), ("1px".into(), "dotted".into()), "the later `border-inline` wins: {out}");
    assert_eq!(get(&l, "border-top-right-radius"), "5px", "start-end is top-right in ltr: {out}");
    assert_eq!(get(&r, "border-top-left-radius"), "5px", "…and top-left in rtl");
    assert_eq!(get(&r, "border-bottom-right-radius"), "6px");
    assert_eq!((get(&e, "border-left-width"), get(&e, "border-left-style")), ("2px".into(), "dashed".into()), "rtl inline-end is the left: {out}");
    assert!(refusals(&out).is_empty());
}

/// ★ Box Alignment's `start` / `end` / `normal` are flex-start / flex-end in
/// a flex container that is not reversed (for one that is, see below); `left`
/// and `right` are physical in a row and resolve against its direction.
#[test]
fn justify_content_keywords_resolve_to_the_flex_ones() {
    let src = r#"<html><head><style>
      div { display: flex } #s { justify-content: start } #e { justify-content: end } #n { justify-content: normal }
      #l, #rl { justify-content: left } #r, #rr { justify-content: right } #c { flex-direction: column; justify-content: right }
    </style></head><body>
      <div id="s">a</div><div id="e">b</div><div id="n">c</div><div id="l">d</div><div id="r">e</div><div id="c">f</div>
      <section dir="rtl"><div id="rl">g</div><div id="rr">h</div></section>
    </body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let jc = |id: &str| base_props(&out, id).get("justify-content").cloned().unwrap_or_default();
    assert_eq!([jc("s"), jc("e"), jc("n")], ["flex-start", "flex-end", "flex-start"], "{out}");
    assert_eq!([jc("l"), jc("r")], ["flex-start", "flex-end"], "ltr row: left is the start");
    assert_eq!([jc("rl"), jc("rr")], ["flex-end", "flex-start"], "rtl row: left is the END");
    assert_eq!(jc("c"), "flex-start", "in a column, left and right behave as start");
    assert!(refusals(&out).is_empty());
}

/// The ids of an element's children, in output order.
fn child_ids(out: &str, id: &str) -> Vec<String> {
    let dom = navigator_dom::parse(out);
    let h = dom.by_tag_anywhere("*").into_iter().chain((0..dom.nodes.len() as u32).filter(|h| dom.attr(*h, "id").is_some()))
        .find(|h| dom.attr(*h, "id") == Some(id)).unwrap_or_else(|| panic!("no #{id}"));
    dom.children_of(h).into_iter().map(|c| dom.attr(c, "id").map(str::to_string)
        .unwrap_or_else(|| dom.text_content(c).trim().to_string())).filter(|s| !s.is_empty()).collect()
}

/// ★ `row-reverse` / `column-reverse` are COMPILED: the children are
/// reversed in the document and the container laid out forwards, so the
/// reading order is the visual one (G5). Laying out forwards moves
/// main-start, so `flex-start` ⇄ `flex-end` and `normal` (or nothing)
/// packs at the end; `start` / `end` / `left` / `right` name sides that
/// reversal does not move.
#[test]
fn a_reversed_flex_container_is_reversed_in_the_document() {
    let src = r#"<html><head><style>
      .r { display: flex; flex-direction: row-reverse }
      #c { display: flex; flex-flow: column-reverse }
      #fs { justify-content: flex-start } #fe { justify-content: flex-end } #st { justify-content: start }
      #en { justify-content: end } #nm { justify-content: normal } #ce { justify-content: center } #lf { justify-content: left }
    </style></head><body>
      <div id="r" class="r"><i id="a">a</i><i id="b">b</i>text<i id="d">d</i></div>
      <div id="c"><p id="c1">1</p><p id="c2">2</p></div>
      <div id="fs" class="r"></div><div id="fe" class="r"></div><div id="st" class="r"></div><div id="en" class="r"></div>
      <div id="nm" class="r"></div><div id="ce" class="r"></div><div id="lf" class="r"></div>
    </body></html>"#;
    let before = refusals(src);
    assert!(before.iter().any(|r| r.contains("flex-direction")), "control: the renderer refuses row-reverse: {before:?}");
    let (out, report) = normalize(src, &Inputs::default());
    assert_eq!(child_ids(&out, "r"), ["d", "text", "b", "a"], "children and text runs reversed: {out}");
    assert_eq!(child_ids(&out, "c"), ["c2", "c1"], "{out}");
    assert_eq!(base_props(&out, "r").get("flex-direction").map(String::as_str), Some("row"), "{out}");
    assert_eq!(base_props(&out, "c").get("flex-direction").map(String::as_str), Some("column"), "{out}");
    let jc = |id: &str| base_props(&out, id).get("justify-content").cloned().unwrap_or_default();
    assert_eq!(jc("r"), "flex-end", "no justify-content packs at main-start, now the END: {out}");
    assert_eq!([jc("fs"), jc("fe"), jc("st"), jc("en"), jc("nm"), jc("ce"), jc("lf")],
               ["flex-end", "flex-start", "flex-start", "flex-end", "flex-end", "center", "flex-start"], "{out}");
    assert_eq!(report.dropped.get("flex-direction reverse compiled into document order"), Some(&9), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// A reversal that is not the same in every context, or whose lines break
/// in layout, cannot be one document order: it is laid out on the same
/// axis, unreversed, and counted. Out-of-flow children are not flex items
/// and keep their place; an rtl row-reverse starts at the left, which is
/// an rtl row's END.
#[test]
fn only_an_unconditional_single_line_reverse_is_compiled() {
    let src = r#"<html><head><style>
      div { display: flex }
      #w { flex-direction: column-reverse; flex-wrap: wrap }
      #m { flex-direction: row-reverse } @media (min-width: 600px) { #m { flex-direction: column } }
      #q { flex-direction: row } @media (max-width: 100px) { #q { flex-direction: row-reverse } }
      #h:hover { flex-direction: column-reverse }
      #k { flex-direction: row-reverse } @media (min-width: 600px) { #k { flex-direction: column-reverse } }
      #b { display: block; flex-direction: row-reverse }
      #ab { flex-direction: row-reverse } #ab > .x { position: absolute }
      #rt { flex-direction: row-reverse }
    </style></head><body>
      <div id="w"><i id="w1">1</i><i id="w2">2</i></div>
      <div id="m"><i id="m1">1</i><i id="m2">2</i></div>
      <div id="q"><i id="q1">1</i><i id="q2">2</i></div>
      <div id="h"><i id="h1">1</i><i id="h2">2</i></div>
      <div id="k"><i id="k1">1</i><i id="k2">2</i></div>
      <div id="b"><i id="b1">1</i><i id="b2">2</i></div>
      <div id="ab"><i id="a1">1</i><i id="a2" class="x">2</i><i id="a3">3</i><i id="a4">4</i></div>
      <section dir="rtl"><div id="rt"><i id="r1">1</i><i id="r2">2</i></div></section>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    for id in ["w", "m", "q", "h", "b"] {
        assert_eq!(child_ids(&out, id), [format!("{id}1"), format!("{id}2")], "#{id} keeps its order: {out}");
    }
    assert_eq!(base_props(&out, "w").get("flex-direction").map(String::as_str), Some("column"), "same axis, unreversed: {out}");
    assert!(!out.contains("-reverse"), "{out}");
    assert_eq!(child_ids(&out, "k"), ["k2", "k1"], "reversed in every context: {out}");
    assert_eq!(child_ids(&out, "ab"), ["a4", "a2", "a3", "a1"], "the absolute child keeps its slot: {out}");
    assert_eq!(child_ids(&out, "rt"), ["r2", "r1"], "{out}");
    assert_eq!(base_props(&out, "rt").get("justify-content").map(String::as_str), Some("flex-end"), "{out}");
    let d = |k: &str| report.dropped.get(k).copied().unwrap_or(0);
    assert_eq!(d("flex-direction reverse in a wrapping container (laid out unreversed)"), 1, "{:?}", report.dropped);
    assert_eq!(d("flex-direction reverse only in some states or media (laid out unreversed)"), 3, "{:?}", report.dropped);
    assert_eq!(d("flex-direction reverse on a box that is not flex (no effect)"), 1, "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ `grid` and `grid-template` are shorthands for the TEMPLATES. Dropping
/// them left every grid with no tracks — 72 css-grid WPT tests write
/// `grid: 150px 100px / 200px 300px` and nothing else.
#[test]
fn grid_shorthands_become_templates() {
    let src = r#"<html><head><style>
      .a { display: grid; grid: 150px 100px / 200px 300px }
      .b { display: grid; grid-template: [top] "x y" 40px [mid] "z z" [bot] / 1fr 2fr }
      .bx { grid-area: z }
      .c { display: grid; grid: auto-flow 30px / 10px 20px }
      .d { display: grid; grid: 50px / auto-flow dense 25px }
      .e { grid-area: 2 / 3 }
    </style></head><body>
      <div class="a"><div>1</div></div>
      <div class="b"><div>x</div><div class="bx">z</div></div>
      <div class="c"><div>c</div></div><div class="d"><div>d</div></div>
      <div class="a"><div class="e">e</div></div>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("grid-template-rows: 150px 100px") && out.contains("grid-template-columns: 200px 300px"), "{out}");
    // The art form: one row per string, `auto` where no size follows, and
    // the names numbered; the area resolves against the shorthand's strings.
    assert!(out.contains("grid-template-rows: 40px auto") && out.contains("grid-template-columns: 1fr 2fr"), "{out}");
    assert!(out.contains("grid-row-start: 2") && out.contains("grid-row-end: 3") && out.contains("grid-column-end: 3"), "z spans row 2: {out}");
    // auto-flow before the slash: rows implicit at 30px.
    assert!(out.contains("grid-auto-rows: 30px") && out.contains("grid-template-columns: 10px 20px"), "{out}");
    // After the slash: columns implicit, and `dense` is reported, never emitted.
    assert!(out.contains("grid-auto-columns: 25px") && out.contains("grid-template-rows: 50px"), "{out}");
    assert!(out.contains("grid-auto-flow: column") || report.dropped.keys().any(|k| k.contains("dense")), "{:?}", report.dropped);
    assert!(!out.contains("dense"), "{out}");
    // `grid-area: 2 / 3` leaves both ends auto.
    assert!(out.contains("grid-column-start: 3"), "{out}");
    assert!(!report.dropped.keys().any(|k| k == "property `grid`" || k == "property `grid-template`"), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ A negative grid line is a number counted from the end, not a NAME:
/// `grid-column: 1 / -1` is the full-width idiom, and reading `-1` as an
/// ident dropped it as a name no template has.
#[test]
fn negative_grid_lines_are_numbers() {
    let src = r#"<html><head><style>
      .g { display: grid; grid-template-columns: 10px 10px 10px }
      .a { grid-column: 1 / -1 } .b { grid-row-start: -2 }
    </style></head><body><div class="g"><div class="a">a</div><div class="b">b</div></div></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("grid-column-end: -1") && out.contains("grid-row-start: -2"), "{out}");
    assert!(!report.dropped.keys().any(|k| k.contains("grid line name")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ An item's grid line NAME is numbered against each media context's
/// template, not only the base one: the Guardian's `grid-column: title`
/// is lines 2-6 on a phone and 2-14 at 800px, and numbering it once put
/// every section title in a third of the page.
#[test]
fn named_lines_are_numbered_in_every_media_context() {
    let src = r#"<html><head><style>
      .g { display: grid; grid-template-columns: [t-start] 10px [t-end] 10px 10px 10px }
      @media (min-width: 500px) { .g { grid-template-columns: 10px [t-start] 10px 10px [t-end] 10px } }
      .i { grid-column: t }
    </style></head><body><div class="g"><div class="i">x</div></div></body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    let media = &out[out.find("@media").expect("a media rule")..];
    let base = &out[..out.find("@media").unwrap()];
    assert!(base.contains("grid-column-start: 1") && base.contains("grid-column-end: 2"), "{out}");
    assert!(media.contains("grid-column-start: 2") && media.contains("grid-column-end: 4"), "the 500px template: {out}");
    assert!(!report.dropped.keys().any(|k| k.contains("no parent template")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ Self-alignment keywords outside the grammar that NAME an admitted one
/// (Box Alignment): `flex-*` on the inline axis are `start` / `end`; `left` /
/// `right` are the CONTAINER's line-left / line-right, so `start` or `end` by
/// its direction; `self-*` are the item's own start side — the container's
/// `start` while their directions agree; `normal` is `stretch` for a box with
/// no aspect ratio. Where the answer would differ item by item, or the value
/// is inherited into descendants (`legacy`), it stays dropped.
#[test]
fn self_alignment_keywords_map_to_the_ones_they_name() {
    let src = r#"<html><head><style>
      .g { display: grid } #fs { justify-self: flex-start } #fe { justify-self: flex-end }
      #l, #rl { justify-self: left } #r, #rr { justify-self: right }
      #ss, #rss, #xss { justify-self: self-start } #se { justify-self: self-end }
      #as { align-self: self-start } #ae { align-self: self-end } #n { justify-self: normal; align-self: normal }
      #fb { align-self: first baseline } #il { justify-items: left } #ir { justify-items: right }
      #ji { justify-items: self-end } #mix { justify-items: self-start }
      .col { display: flex; flex-direction: column } #cx { align-self: self-start } #lg { justify-items: legacy left }
    </style></head><body>
      <div class="g"><p id="fs">a</p><p id="fe">a</p><p id="l">a</p><p id="r">a</p><p id="ss">a</p><p id="se">a</p>
        <p id="as">a</p><p id="ae">a</p><p id="n">a</p><p id="fb">a</p><p id="xss" dir="rtl">a</p></div>
      <div class="g" dir="rtl"><p id="rl">a</p><p id="rr">a</p><p id="rss">a</p></div>
      <div class="g" id="il"><p>a</p></div><div class="g" id="ir" dir="rtl"><p>a</p></div>
      <div class="g" id="ji"><p>a</p></div><div class="g" id="mix"><p>a</p><p dir="rtl">b</p></div>
      <div class="col"><p id="cx" dir="rtl">a</p></div><div class="g" id="lg"><p>a</p></div>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    let p = |id: &str, prop: &str| base_props(&out, id).get(prop).cloned().unwrap_or_default();
    assert_eq!([p("fs", "justify-self"), p("fe", "justify-self")], ["start", "end"], "flex-* outside flex: {out}");
    assert_eq!([p("l", "justify-self"), p("r", "justify-self")], ["start", "end"], "ltr container: left is the start");
    assert_eq!([p("rl", "justify-self"), p("rr", "justify-self")], ["end", "start"], "rtl container: left is the END");
    assert_eq!([p("il", "justify-items"), p("ir", "justify-items")], ["start", "start"], "justify-items: left in ltr, right in rtl");
    assert_eq!([p("ss", "justify-self"), p("se", "justify-self"), p("rss", "justify-self")], ["start", "end", "start"], "same direction");
    assert_eq!(p("xss", "justify-self"), "end", "an rtl item in an ltr grid: its own start is the container's end");
    assert_eq!([p("as", "align-self"), p("ae", "align-self")], ["start", "end"], "block axis: every start is the top");
    assert_eq!([p("n", "justify-self"), p("n", "align-self")], ["stretch", "stretch"], "normal on a box with no ratio");
    assert_eq!(p("fb", "align-self"), "baseline");
    assert_eq!(p("ji", "justify-items"), "end");
    // Not exact: one class cannot be `start` for one item and `end` for the next.
    assert_eq!(p("mix", "justify-items"), "", "children of differing directions: dropped");
    assert_eq!(p("cx", "align-self"), "", "inline cross axis, item direction differs: dropped");
    assert_eq!(p("lg", "justify-items"), "", "legacy inherits into descendants: dropped");
    let said = |what: &str| report.dropped.keys().any(|k| k.contains(what));
    assert!(said("justify-items: self-start") && said("align-self: self-start") && said("legacy left"), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// ★ An image's `normal` self-alignment is `start` in a grid (Grid §6.2: a
/// replaced box with a ratio or a natural size is not stretched) — and so is
/// a non-replaced `aspect-ratio` box's in whichever axis the ratio derives:
/// the block axis unless its height is settled, the inline axis when it is
/// (Sizing 4; every case below measured in headless Chrome) — and that is
/// also what a page that says NOTHING gets — `auto` takes the container's
/// `normal`. The profile's initial is `stretch`, so it is written out, in
/// every media context where the container is a grid, and taken back
/// (`auto`) where it is a flex container, whose `normal` IS stretch. Boxes
/// without a ratio, and an explicit container alignment, are left alone.
#[test]
fn an_image_in_a_grid_keeps_its_size_as_in_css() {
    let src = r#"<html><head><style>
      .g { display: grid; grid-template-columns: 200px 200px } .f { display: flex }
      #m { display: grid } @media (min-width: 600px) { #m { display: flex } }
      #c { align-items: center } #n { justify-self: normal; align-self: normal }
    </style></head><body>
      <div class="g"><img id="a" src="cat.png"><p id="p">text</p><img id="n" src="cat.png"><p id="r" style="aspect-ratio: 2">r</p>
        <p id="w" style="aspect-ratio: 2; width: 100px">w</p><p id="j" style="aspect-ratio: 2; justify-self: stretch">j</p>
        <p id="h" style="aspect-ratio: 2; height: 50px">h</p><p id="s" style="aspect-ratio: 2; align-self: stretch">s</p>
        <p id="k" style="aspect-ratio: 2; justify-self: center">k</p><p id="q" style="aspect-ratio: 2; height: 50%">q</p></div>
      <div style="display: grid; grid-template: 100px / 200px"><p id="t" style="aspect-ratio: 2; height: 50%">t</p></div>
      <div class="f"><img id="b" src="cat.png"></div>
      <div id="m"><img id="d" src="cat.png"></div>
      <div class="g" id="c"><img id="e" src="cat.png"></div>
    </body></html>"#;
    let mut inputs = Inputs::default();
    inputs.images.insert("cat.png".into(), (40, 20).into());
    let (out, report) = normalize(src, &inputs);
    let p = |id: &str, prop: &str| base_props(&out, id).get(prop).cloned().unwrap_or_default();
    assert_eq!([p("a", "justify-self"), p("a", "align-self")], ["start", "start"], "{out}");
    assert_eq!([p("n", "justify-self"), p("n", "align-self")], ["start", "start"], "explicit normal, same answer");
    assert_eq!([p("p", "justify-self"), p("p", "align-self")], ["", ""], "a box with no ratio stretches already");
    // A 1/1 box in a 200px column and a 100px row is 200×200 in Chrome,
    // overflowing the row: the WIDTH stretches and the ratio gives the
    // height. (Round 17 left this `stretch`: 200×100.)
    assert_eq!([p("r", "justify-self"), p("r", "align-self")], ["", "start"], "a NON-replaced aspect-ratio box stretches its width, its height from the ratio");
    assert_eq!(p("w", "align-self"), "start", "…as it does with its width specified");
    assert_eq!([p("j", "justify-self"), p("j", "align-self")], ["stretch", "start"], "…or explicitly stretched");
    assert_eq!([p("h", "justify-self"), p("h", "align-self")], ["start", ""], "a SETTLED height makes the width the ratio's (Chrome 100×50, not 200×50)");
    assert_eq!([p("s", "justify-self"), p("s", "align-self")], ["start", "stretch"], "an explicit block stretch settles the height");
    assert_eq!([p("k", "justify-self"), p("k", "align-self")], ["center", ""], "a non-stretch justify leaves the width unsettled: the block axis stretches");
    assert_eq!([p("q", "justify-self"), p("q", "align-self")], ["", "start"], "a % height in an AUTO row is `auto`: nothing settled");
    assert_eq!(p("t", "justify-self"), "start", "…but in a 100px row it resolves, and settles the height");
    assert_eq!([p("b", "justify-self"), p("b", "align-self")], ["", ""], "a flex item's normal is stretch");
    assert_eq!([p("e", "justify-self"), p("e", "align-self")], ["start".to_string(), String::new()], "the container's center stands");
    assert_eq!(p("d", "align-self"), "start", "grid in the base context");
    let media = out.split("@media").skip(1).collect::<String>();
    assert!(media.contains("align-self: auto") && media.contains("justify-self: auto"), "and auto again where it is flex: {out}");
    assert!(report.dropped.keys().any(|k| k.contains("compiled to `start`")), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

/// `align-content: normal` is `stretch` in a flex container (Box Alignment
/// §5.4), and `start` / `end` are `flex-start` / `flex-end` there while no
/// `wrap-reverse` swaps the cross axis — which the profile excludes.
#[test]
fn align_content_keywords_resolve_to_the_flex_ones() {
    let src = r#"<html><head><style>
      div { display: flex; flex-wrap: wrap; height: 100px } #n { align-content: normal } #s { align-content: start } #e { align-content: end }
    </style></head><body><div id="n">a</div><div id="s">b</div><div id="e">c</div></body></html>"#;
    let (out, _) = normalize(src, &Inputs::default());
    let ac = |id: &str| base_props(&out, id).get("align-content").cloned().unwrap_or_default();
    assert_eq!([ac("n"), ac("s"), ac("e")], ["stretch", "flex-start", "flex-end"], "{out}");
    assert!(refusals(&out).is_empty());
}

/// ★ The white space between floats renders NOTHING (CSS 2.1 §9.5.1), but
/// between the inline-blocks a float row compiles to it is a word space —
/// two `width: 50%` floats wrapped onto two lines. A strip loses its
/// whitespace-only text, unless the space is preserved (`white-space: pre`)
/// or another context lays the children out inline, where it is real.
#[test]
fn white_space_between_floats_is_not_a_word_space() {
    let src = r#"<html><head><style>
      .s span { float: left; width: 50% } #p { white-space: pre } @media (max-width: 9000px) { #m span { display: inline } }
    </style></head><body>
      <div class="s" id="a">
        <span>one</span>
        <span>two</span>
      </div>
      <div class="s" id="p"> <span>one</span> <span>two</span> </div>
      <div class="s" id="m"> <span>one</span> <span>two</span> </div>
    </body></html>"#;
    let (out, report) = normalize(src, &Inputs::default());
    let inner = |id: &str| { let at = out.find(&format!("id=\"{id}\"")).expect("id"); let s = &out[at..]; s[s.find('>').unwrap() + 1..s.find("</div>").unwrap()].to_string() };
    assert!(!inner("a").contains("</span> ") && !inner("a").starts_with(char::is_whitespace), "{}", inner("a"));
    assert!(inner("p").contains("</span> <span"), "pre keeps its spaces: {}", inner("p"));
    assert!(inner("m").contains("</span> <span"), "laid out inline in some context: {}", inner("m"));
    assert_eq!(report.dropped.get("white space between floats removed (it renders nothing)"), Some(&3), "{:?}", report.dropped);
    // …and the two halves now share one line.
    let fonts = navigator_render::fontset::FontSet::load().expect("fonts");
    let o = navigator_render::html::render_html(&out, &fonts, &Env::default());
    let ys: Vec<i64> = o.scene.runs.iter().filter(|r| r.text == "one" || r.text == "two").take(2).map(|r| r.y as i64).collect();
    assert_eq!(ys.len(), 2);
    assert_eq!(ys[0], ys[1], "one line");
}
