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
fn a_clip_inside_keeps_the_table_overflowing_as_before() {
    let (scene, _) = render(&page("ltr", ".g { overflow-x: hidden }"));
    assert!(scene.scrolls.is_empty(), "an inner clip could not move with the content");
    // ★ The table's OWN clip counts too (hi and ru Wikipedia: the broker
    // refused the region, because the cells carried the table's clip).
    let (scene, _) = render(&page("ltr", "table { overflow-x: hidden }"));
    assert!(scene.scrolls.is_empty());
    scroll::validate(&scene).unwrap();
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
