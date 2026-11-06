#![cfg(feature = "raster")]
//! ★ A table wider than its box scrolls on its own (backend §4.9 item 4):
//! laid out once, moved by the compositor, checked by the broker.

use navigator_render::{fontset::FontSet, html::{premeasure_tables, render_html}, nsg, raster, scroll, Scene};
use navigator_style::cascade::Env;

const RED: u32 = 0xff0000ff;
const GREEN: u32 = 0x00ff00ff;
const BLUE: u32 = 0x0000ffff;

fn page(dir: &str, extra: &str) -> String {
    let word = |c: char| std::iter::repeat_n(c, 40).collect::<String>();
    format!(r#"<html><head><style>body {{ margin-left: 0px; margin-top: 0px }} .box {{ width: 300px; margin-left: 50px; direction: {dir} }}
        td {{ padding-left: 0px; padding-right: 0px }}
        .r {{ background-color: #ff0000 }} .g {{ background-color: #00ff00 }} .b {{ background-color: #0000ff }} {extra}</style></head>
        <body><div class="box"><table><tr><td class="r">{}</td><td class="g">{}</td><td class="b">{}</td></tr></table></div>
        <p>After the table.</p></body></html>"#, word('a'), word('b'), word('c'))
}

fn render(doc: &str) -> (Scene, FontSet) {
    let fonts = FontSet::load().unwrap();
    let (measured, _) = premeasure_tables(doc, &fonts, &Env::default());
    let o = render_html(&measured, &fonts, &Env::default());
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    (o.scene, fonts)
}

/// Columns of `rgba` pixels painted, and whether any is outside [x0, x1).
fn columns(cv: &raster::Canvas, rgba: u32) -> Vec<usize> {
    let want = [(rgba >> 24) as f32 / 255.0, ((rgba >> 16) & 255) as f32 / 255.0, ((rgba >> 8) & 255) as f32 / 255.0];
    let mut xs = std::collections::BTreeSet::new();
    for y in 0..cv.h { for x in 0..cv.w {
        let p = cv.px[y * cv.w + x];
        if (p[0] - want[0]).abs() < 0.02 && (p[1] - want[1]).abs() < 0.02 && (p[2] - want[2]).abs() < 0.02 { xs.insert(x); }
    } }
    xs.into_iter().collect()
}

#[test]
fn a_wide_table_scrolls_inside_its_box_and_nothing_paints_outside_it() {
    let (mut scene, fonts) = render(&page("ltr", ""));
    assert_eq!(scene.scrolls.len(), 1, "one scroll region");
    scroll::validate(&scene).expect("the broker's check passes");
    let s = scene.scrolls[0].clone();
    let (vx, _, vw, _) = scene.clips[s.clip as usize];
    assert_eq!((vx, vw), (50 * 64, 300 * 64), "the region is the table's box");
    assert!(s.content.2 > 3 * vw / 2, "the content is wider than the box: {}", s.content.2);
    let text = nsg::write(&scene, &fonts);
    assert!(text.lines().any(|l| l.starts_with(&format!("scroll c{} x{} ", s.clip, s.xform))), "declared in NSG");
    let (lo, hi, ylo, yhi) = scroll::range(&scene, &s);
    assert_eq!((hi, ylo, yhi), (0, 0, 0), "LTR content only moves left; no vertical range");
    assert_eq!(lo, vx + vw - (s.content.0 + s.content.2));
    let inside = |xs: &[usize]| xs.iter().all(|&x| (50..350).contains(&x));
    let none = std::collections::BTreeMap::new();
    // The review raster paints a transformed node into a canvas-sized layer
    // before moving it, so its canvas must hold the whole content: widened
    // here past the table's right edge. The viewport clip still bounds what
    // shows, which is what this checks.
    scene.width = 1200 * 64;
    let cv = raster::paint(&scene, &fonts, &none);
    let (r, b) = (columns(&cv, RED), columns(&cv, BLUE));
    assert!(!r.is_empty() && inside(&r), "red at offset 0, inside the box");
    assert!(b.is_empty(), "blue is past the box at offset 0");
    // The reader scrolls all the way (and further: clamped).
    assert_eq!(scroll::offset_scene(&mut scene, 0, -1 << 40, 0), (lo, 0));
    let cv = raster::paint(&scene, &fonts, &none);
    let (r, b) = (columns(&cv, RED), columns(&cv, BLUE));
    assert!(r.is_empty(), "red has scrolled out");
    assert!(!b.is_empty() && inside(&b), "blue has scrolled in, inside the box");
    // The table's right edge is the box's (349); its last cell ends one
    // border-spacing (2 px) inside it.
    assert_eq!(*b.last().unwrap(), 347, "and the last column ends at the box's right edge");
    // The paragraph after the table did not move.
    scroll::validate(&scene).expect("still valid after scrolling");
}

#[test]
fn a_right_to_left_table_scrolls_the_other_way() {
    let (scene, _) = render(&page("rtl", ""));
    let s = &scene.scrolls[0];
    let (lo, hi, _, _) = scroll::range(&scene, s);
    assert_eq!(lo, 0);
    assert!(hi > 0, "RTL content is to the left: it moves right");
}

#[test]
fn a_table_that_fits_writes_no_scroll_and_is_unchanged() {
    let doc = page("ltr", "").replace(&"a".repeat(40), "a").replace(&"b".repeat(40), "b").replace(&"c".repeat(40), "c");
    let (scene, fonts) = render(&doc);
    assert!(scene.scrolls.is_empty());
    assert!(!nsg::write(&scene, &fonts).contains("\nscroll "));
}

#[test]
fn a_clip_inside_scrolls_with_the_content_and_stays_inside_the_viewport() {
    // ★ A cell that clips (`overflow: hidden`) and a table that clips
    // itself: their clips are INNER clips of the region, moved with it and
    // intersected with the viewport when painted. (They used to keep the
    // table overflowing: 18 of 169 wide tables at 300%.)
    for extra in [".g { overflow-x: hidden }", "table { overflow-x: hidden }"] {
        let (mut scene, fonts) = render(&page("ltr", extra));
        assert_eq!(scene.scrolls.len(), 1, "{extra}: a region");
        assert!(!scene.scrolls[0].inner.is_empty(), "{extra}: with an inner clip");
        scroll::validate(&scene).unwrap_or_else(|e| panic!("{extra}: {e}"));
        scene.width = 1200 * 64;
        let none = std::collections::BTreeMap::new();
        let inside = |xs: &[usize]| xs.iter().all(|&x| (50..350).contains(&x));
        // Green (the clipping cell) scrolls into view, whole, and stays inside.
        let mut seen_green = false;
        for step in 0..12 {
            let cv = raster::paint(&scene, &fonts, &none);
            for c in [RED, GREEN, BLUE] { assert!(inside(&columns(&cv, c)), "{extra}, step {step}: {c:08x} outside the box") }
            seen_green |= !columns(&cv, GREEN).is_empty();
            scroll::offset_scene(&mut scene, 0, -64 * 64, 0);
        }
        assert!(seen_green, "{extra}: the clipped cell scrolled into view");
    }
}

#[test]
fn the_broker_refuses_a_forged_region() {
    let (scene, _) = render(&page("ltr", ""));
    let s = scene.scrolls[0].clone();
    // A node moved by the region but not clipped to it: it would paint over the page.
    let mut bad = clone(&scene);
    let n = bad.rect_attrs.iter().position(|a| a.2 == Some(s.xform)).unwrap();
    bad.rect_attrs[n].0 = None;
    assert!(scroll::validate(&bad).unwrap_err().contains("not clipped to its viewport"));
    // A region whose own transform scales.
    let mut bad = clone(&scene);
    bad.xforms[s.xform as usize][0] *= 2;
    assert!(scroll::validate(&bad).unwrap_err().contains("not a translation"));
    // The same transform claimed twice.
    let mut bad = clone(&scene);
    bad.scrolls.push(s.clone());
    assert!(scroll::validate(&bad).unwrap_err().contains("already moved"));
    // Indices that name nothing.
    let mut bad = clone(&scene);
    bad.scrolls[0].clip = 9999;
    assert!(scroll::validate(&bad).unwrap_err().contains("no clip"));
    // An inner clip that is the viewport itself.
    let mut bad = clone(&scene);
    bad.scrolls[0].inner = vec![s.clip];
    assert!(scroll::validate(&bad).unwrap_err().contains("not a clip of its own"));
    let mut bad = clone(&scene);
    bad.scrolls[0].content.2 = -1;
    assert!(scroll::validate(&bad).unwrap_err().contains("negative"));
}

/// The scene fields the checks read (Scene is not Clone: it is never copied
/// on the production path).
fn clone(s: &Scene) -> Scene {
    Scene { width: s.width, height: s.height, rects: s.rects.clone(), runs: s.runs.clone(), clips: s.clips.clone(), xforms: s.xforms.clone(),
            xform_parents: s.xform_parents.clone(), rect_attrs: s.rect_attrs.clone(), run_attrs: s.run_attrs.clone(), scrolls: s.scrolls.clone(),
            order: s.order.clone(), ..Default::default() }
}

#[test]
fn a_narrow_first_column_stays_in_view_while_the_rest_scrolls() {
    let word = |c: char| std::iter::repeat_n(c, 40).collect::<String>();
    let doc = format!(r#"<html><head><style>body {{ margin-left: 0px; margin-top: 0px }} .box {{ width: 300px; margin-left: 50px }}
        td {{ padding-left: 0px; padding-right: 0px }} .k {{ background-color: #00ff00 }}
        .r {{ background-color: #ff0000 }} .b {{ background-color: #0000ff }}</style></head>
        <body><div class="box"><table><tr><td class="k">Key</td><td class="r">{}</td><td class="b">{}</td></tr></table></div></body></html>"#, word('a'), word('c'));
    let (mut scene, fonts) = render(&doc);
    assert_eq!(scene.scrolls.len(), 1);
    scroll::validate(&scene).expect("pinned cells keep the viewport clip");
    scene.width = 1200 * 64;
    let none = std::collections::BTreeMap::new();
    let cv = raster::paint(&scene, &fonts, &none);
    let k0 = columns(&cv, GREEN);
    assert!(!k0.is_empty() && *k0.first().unwrap() >= 50, "the key column, at the box's start: {k0:?}");
    assert!(columns(&cv, BLUE).is_empty(), "blue is past the box at offset 0");
    scroll::offset_scene(&mut scene, 0, -1 << 40, 0);
    let cv = raster::paint(&scene, &fonts, &none);
    assert_eq!(columns(&cv, GREEN), k0, "the key column has not moved");
    let b = columns(&cv, BLUE);
    assert!(!b.is_empty(), "blue scrolled in");
    assert!(b.iter().all(|x| *x > *k0.last().unwrap()), "and slides UNDER the key column, never over it: key {k0:?} blue {b:?}");
    // ★ A table that clips itself AND pins its first column (ru Wikipedia):
    // the pinned cells are clipped to the viewport, not to the table's own
    // clip, which moves — or the broker refuses the region.
    let (scene, _) = render(&doc.replace(".k {{", "table { overflow-x: hidden } .k {{").replace("</style>", " table { overflow-x: hidden }</style>"));
    assert_eq!(scene.scrolls.len(), 1);
    assert!(!scene.scrolls[0].inner.is_empty(), "the table's own clip is an inner clip");
    scroll::validate(&scene).expect("pinned cells of a self-clipping table");
}
