//! The bytes' half of §3.13: the document declares the SIZE, the recording
//! names the BYTES, and only with both does an image paint.

use navigator_render::conformance::subresources_from_recording;
use navigator_render::fontset::FontSet;
use navigator_render::html::{render_html, render_html_with};
use navigator_style::cascade::Env;

const DOC: &str = r#"<html><body><img src="/logo.png" width="40" height="20"></body></html>"#;

fn recording(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("rec.json");
    std::fs::write(&p, r#"{
  "format": "atrium-navigator-recording/3",
  "images": [
    { "src": "/logo.png", "width": 40, "height": 20, "address": "blake3:abc123" },
    { "src": "/nobytes.png", "width": 8, "height": 8, "address": "" }
  ],
  "document": "x"
}
"#).expect("write");
    p
}

#[test]
fn an_image_paints_only_when_the_recording_names_its_bytes() {
    let dir = std::env::temp_dir().join(format!("nsg-subs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();

    // Control: no subresources. The box is laid out from the DECLARED size —
    // no layout shift — but there is nothing to paint, and that is counted.
    let bare = render_html(DOC, &fonts, &env);
    assert!(bare.diagnostics.is_empty(), "a declared size is enough to lay out: {:?}", bare.diagnostics);
    assert!(bare.scene.images.is_empty(), "nothing to paint without an address");
    assert!(bare.unimplemented.keys().any(|k| k.starts_with("image bytes not supplied")));

    // With the recording: the node carries the address the bytes live under.
    let subs = subresources_from_recording(&recording(&dir));
    assert_eq!(subs.get("/logo.png"), Some(&("blake3:abc123".to_string(), 40 * 64, 20 * 64)));
    assert!(!subs.contains_key("/nobytes.png"), "an empty address is not an address");

    let o = render_html_with(DOC, &fonts, &env, &subs);
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    assert_eq!(o.scene.images.len(), 1);
    assert_eq!(o.scene.images[0].address, "blake3:abc123");
    let a = &o.scene.images[0].area;
    assert_eq!((a.w, a.h), (40 * 64, 20 * 64), "painted at its declared intrinsic size");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ★ The scene's extent is the CONTENT, not the root box. Wikipedia sets
/// `html, body { height: 100% }`, which makes the root exactly one viewport
/// tall while the article runs far below it.
#[test]
fn the_extent_covers_content_below_a_full_height_root() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let tall: String = (0..80).map(|i| format!("<p>line {i}</p>")).collect();
    let o = render_html(&format!("<html><head><style>body {{ height: 100% }}</style></head><body>{tall}</body></html>"), &fonts, &env);
    let lowest = o.scene.runs.iter().map(|r| r.y).max().expect("runs");
    assert!(o.scene.height >= lowest, "the extent must reach the last line: {} vs {}", o.scene.height, lowest);
    assert!(o.scene.height > (env.height_px as i64) * 64, "and it must exceed one viewport");
}

/// ★ Measurement and layout must agree about a replaced box's size, or the
/// image overflows the cell its column was measured for. Offline table
/// pre-measurement runs WITHOUT a subresource map, so the declared size has
/// to be enough on its own.
#[test]
fn a_declared_image_measures_the_same_with_or_without_bytes() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let doc = r#"<html><head><style>table { display: table } tr { display: table-row } td { display: table-cell }</style></head>
        <body><table><tr><td><img src="/logo.png" width="250" height="51"></td><td>caption</td></tr></table></body></html>"#;
    // Pre-measured offline, with no bytes anywhere.
    let (measured, n) = navigator_render::html::premeasure_tables(doc, &fonts, &env);
    assert_eq!(n, 2);
    let o = render_html(&measured, &fonts, &env);
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    // The column the image sits in is at least as wide as the image.
    let widths: Vec<i64> = measured.lines().filter(|l| l.contains("min-width"))
        .filter_map(|l| l.split("min-width: ").nth(1)?.split("px").next()?.trim().parse::<f64>().ok())
        .map(|v| v as i64).collect();
    assert!(widths.iter().any(|w| *w >= 250), "the image's column must measure at least its width: {widths:?}");
}

/// ★ CSS 2.1 §17.5.2: a table's used width is the GREATER of its specified
/// width and its minimum content width. Clamping the columns to a narrower
/// specified width instead leaves the content hanging outside the box —
/// which is what a 330 px image in a 310 px infobox looked like.
#[test]
fn a_table_grows_past_a_specified_width_rather_than_overflowing() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let doc = r#"<html><head><style>
        table { display: table; width: 100px; background-color: #eeeeee }
        tr { display: table-row } td { display: table-cell }</style></head>
        <body><table><tr><td><img src="/wide.png" width="300" height="20"></td></tr></table></body></html>"#;
    let (measured, _) = navigator_render::html::premeasure_tables(doc, &fonts, &env);
    let o = render_html(&measured, &fonts, &env);
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    // The table's own background reaches at least as far as its content.
    let table = o.scene.rects.iter().find(|r| r.rgba == 0xeeeeeeff).expect("table background");
    assert!(table.w >= 300 * 64, "the table grew to its content: {} px", table.w / 64);
    // Control: with narrow content the specified width is respected.
    let narrow = doc.replace(r#"<img src="/wide.png" width="300" height="20">"#, "x");
    let (m2, _) = navigator_render::html::premeasure_tables(&narrow, &fonts, &env);
    let o2 = render_html(&m2, &fonts, &env);
    let t2 = o2.scene.rects.iter().find(|r| r.rgba == 0xeeeeeeff).expect("table background");
    assert_eq!(t2.w, 100 * 64, "a table that fits keeps its specified width");
}

/// ★ BLOCK-IN-INLINE. An inline box holding block-level content cannot stay
/// inline — flattening it pours the whole subtree into one line box. Hacker
/// News wraps its entire page in `<center>`; a `<span>` around a `<div>` is
/// the same shape.
#[test]
fn an_inline_wrapper_around_block_content_does_not_flatten_it() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let o = render_html(r#"<html><body><span><div>one</div><div>two</div></span></body></html>"#, &fonts, &env);
    let ys: Vec<i64> = o.scene.runs.iter().map(|r| r.y).collect();
    assert_eq!(ys.len(), 2);
    assert!(ys[0] != ys[1], "the two blocks must be on separate lines: {ys:?}");
    // Control: genuinely inline content stays on one line.
    let c = render_html(r#"<html><body><span><span>one</span><span>two</span></span></body></html>"#, &fonts, &env);
    let cys: Vec<i64> = c.scene.runs.iter().map(|r| r.y).collect();
    assert_eq!(cys[0], cys[1], "inline content shares a line: {cys:?}");
}

/// `<noscript>` is RAW TEXT when scripting is enabled, so its markup parses
/// as a text node — and the converter ran the scripts, so showing it puts
/// literal angle brackets on the page.
#[test]
fn noscript_is_not_shown() {
    let fonts = FontSet::load().expect("pinned font set");
    let o = render_html(r#"<html><body><noscript><iframe src="toc.html"></iframe></noscript><p>real</p></body></html>"#,
                        &fonts, &Env::default());
    let text: String = o.scene.runs.iter().map(|r| r.text.clone()).collect();
    assert!(!text.contains("iframe"), "the fallback must not be painted: {text:?}");
    assert!(text.contains("real"));
}

/// ★ An `fr` inside `minmax()` is still flexible. `minmax(0, 1fr)` is the
/// most common way to write "one flexible column", and treating only a
/// top-level `fr` as flexible made it a ZERO-WIDE track — which is how a
/// whole page came out overlapping in an 800 px viewport.
#[test]
fn an_fr_inside_minmax_takes_the_free_space() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let one = |css: &str| -> i64 {
        let o = render_html(&format!(r#"<html><head><style>body {{ margin: 0 }}
            .g {{ display: grid; width: 400px; grid-template-columns: {css} }}
            .i {{ background-color: #ff0000; height: 10px }}</style></head>
            <body><div class="g"><div class="i">x</div></div></body></html>"#), &fonts, &env);
        o.scene.rects.iter().find(|r| r.rgba == 0xff0000ff).expect("item").w / 64
    };
    assert_eq!(one("minmax(0, 1fr)"), 400, "a flexible column fills the grid");
    assert_eq!(one("1fr"), 400, "and so does a bare fr");
    // The minimum is respected: a column cannot shrink below it.
    assert_eq!(one("minmax(500px, 1fr)"), 500, "the minimum wins over the share");
    // Control: a fixed column does NOT take the free space.
    assert_eq!(one("100px"), 100, "a fixed column stays fixed");
}

/// ★ A flexible ROW in a container with no definite height has no free
/// space to take a fraction of, so it sizes to its content (CSS Grid
/// §12.7.1). Returning zero gave a page-tall row a height of zero, and
/// every section below it was painted on top of the one above.
#[test]
fn a_flexible_row_with_no_definite_height_sizes_to_content() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let o = render_html(r#"<html><head><style>body { margin-top: 0px; margin-left: 0px }
        .g { display: grid; grid-template-rows: min-content 1fr min-content }
        .a { height: 20px; background-color: #ff0000 }
        .b { height: 300px; background-color: #00ff00 }
        .c { height: 20px; background-color: #0000ff }</style></head>
        <body><div class="g"><div class="a"></div><div class="b"></div><div class="c"></div></div></body></html>"#,
        &fonts, &env);
    let at = |rgba: u32| o.scene.rects.iter().find(|r| r.rgba == rgba).map(|r| (r.y / 64, r.h / 64)).expect("box");
    assert_eq!(at(0xff0000ff), (0, 20));
    assert_eq!(at(0x00ff00ff), (20, 300), "the flexible row is as tall as its content");
    assert_eq!(at(0x0000ffff), (320, 20), "and what follows is BELOW it, not on top");
}

/// ★ A track list holds lengths, and they are converted like any other
/// length. Missing that read `minmax(15rem, 1fr)` as fifteen PIXELS — every
/// em/rem grid on the web sized at a sixteenth of its intended value.
#[test]
fn track_lengths_are_converted_to_px() {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let o = render_html(r#"<html><head><style>body { margin-top: 0px; margin-left: 0px }
        .g { display: grid; width: 600px; grid-template-columns: 10rem minmax(5rem, 1fr) }
        .a { background-color: #ff0000; height: 10px } .b { background-color: #00ff00; height: 10px }</style></head>
        <body><div class="g"><div class="a"></div><div class="b"></div></div></body></html>"#, &fonts, &env);
    let w = |rgba: u32| o.scene.rects.iter().find(|r| r.rgba == rgba).expect("box").w / 64;
    assert_eq!(w(0xff0000ff), 160, "10rem is 160px, not 10px");
    assert_eq!(w(0x00ff00ff), 440, "and the flexible column takes the rest");
}
