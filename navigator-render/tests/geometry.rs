//! Layout geometry for the converter (legacy-web §5.4.5.6e): every element's
//! box from the same layout that paints the scene, keyed by a marker
//! attribute. What is checked here is that the boxes are the REAL
//! placements — not a trial layout's, not a box at the origin.

use navigator_render::html::{layout_geometry, render_html_with, Geom, Geometry};
use navigator_render::fontset::FontSet;
use navigator_render::Subresources;
use navigator_style::cascade::Env;

const PX: i64 = 64;

fn geo(html: &str) -> Geometry {
    let env = Env { width_px: 1280.0, height_px: 600.0, ..Env::default() };
    layout_geometry(html, &FontSet::load().expect("fonts"), &env, &Subresources::new(), "g")
}

fn el(g: &Geometry, k: u64) -> &Geom { g.elements.get(&k).unwrap_or_else(|| panic!("no box for g={k}: {:?}", g.elements.keys())) }

#[test]
fn block_boxes_are_where_the_layout_put_them() {
    let g = geo(r#"<html g="0"><body g="1"><style>body{margin-top:0;margin-right:0;margin-bottom:0;margin-left:0} .a{margin-top:10px;margin-right:10px;margin-bottom:10px;margin-left:10px;padding-top:5px;padding-right:5px;padding-bottom:5px;padding-left:5px;border-top-style:solid;border-right-style:solid;border-bottom-style:solid;border-left-style:solid;border-top-width:2px;border-right-width:2px;border-bottom-width:2px;border-left-width:2px;height:50px}</style>
        <div g="2" class="a">x</div><div g="3" class="a">y</div></body></html>"#);
    let a = el(&g, 2);
    assert_eq!((a.x, a.y, a.w, a.h), (10 * PX, 10 * PX, (1280 - 20) * PX, 50 * PX), "{a:?}");
    assert_eq!(a.border, [2 * PX; 4]);
    assert_eq!(a.padding, [5 * PX; 4]);
    // The profile does not collapse margins (the normalizer compiles
    // collapsing into literal margins before this): 10 + 50 + 10 + 10.
    assert_eq!(el(&g, 3).y, (10 + 50 + 10 + 10) * PX);
    assert_eq!(el(&g, 0).w, 1280 * PX);
}

/// ★ A flex item is MEASURED (a trial layout at the origin) before it is
/// placed. The trial's boxes must not survive, or every item reads as if it
/// sat at x = 0.
#[test]
fn a_flex_item_reports_its_placement_not_its_trial() {
    let g = geo(r#"<html><body><style>body{margin-top:0;margin-right:0;margin-bottom:0;margin-left:0} .f{display:flex} .i{width:100px;height:40px;flex-shrink:0}</style>
        <div class="f" g="1"><div class="i" g="2"><p g="4">a</p></div><div class="i" g="3"><p g="5">b</p></div></div></body></html>"#);
    assert_eq!((el(&g, 2).x, el(&g, 3).x), (0, 100 * PX));
    // The descendant of the second item is inside it too.
    assert!(el(&g, 5).x >= 100 * PX, "{:?}", el(&g, 5));
}

#[test]
fn an_inline_box_has_a_fragment_per_line_and_their_union() {
    let g = geo(r#"<html><body><style>body{margin-top:0;margin-right:0;margin-bottom:0;margin-left:0} div{width:60px}</style>
        <div g="1">aa <span g="2">bbbbb ccccc ddddd</span></div></body></html>"#);
    let s = el(&g, 2);
    assert!(s.fragments.len() >= 2, "a span wrapped across lines has a rect per line: {s:?}");
    let bottom = s.fragments.iter().map(|f| f.1 + f.3).max().unwrap();
    assert_eq!(s.y + s.h, bottom);
    assert!(s.w <= 60 * PX && s.w > 0);
}

#[test]
fn an_element_that_is_not_laid_out_has_no_box() {
    let g = geo(r#"<html><body><div g="1"><p g="2">x</p></div><style>div{display:none}</style><p g="3">y</p></body></html>"#);
    assert!(!g.elements.contains_key(&1) && !g.elements.contains_key(&2));
    assert!(g.elements.contains_key(&3));
}

#[test]
fn a_replaced_box_and_a_positioned_ancestor() {
    let g = geo(r#"<html><body><style>body{margin-top:0;margin-right:0;margin-bottom:0;margin-left:0} .r{position:relative;padding-top:30px}</style>
        <div class="r" g="1"><img g="2" src="/i.png" width="40" height="20"></div></body></html>"#);
    assert!(el(&g, 1).positioned && !el(&g, 2).positioned);
    let i = el(&g, 2);
    assert_eq!((i.y, i.w, i.h), (30 * PX, 40 * PX, 20 * PX));
}

/// ★ Geometry is opt-in and records into its own log: the scene a render
/// paints is the same whether or not geometry was asked for elsewhere.
#[test]
fn the_document_extent_is_the_scenes() {
    let html = r#"<html><body><div>x</div><style>div{height:2000px}</style></body></html>"#;
    let env = Env { width_px: 1280.0, height_px: 600.0, ..Env::default() };
    let g = geo(html);
    let out = render_html_with(html, &FontSet::load().unwrap(), &env, &Subresources::new());
    assert_eq!((g.width, g.height), (out.scene.width, out.scene.height));
    assert!(g.height >= 2000 * PX);
}

/// ★ A natural dimension of ZERO is a dimension (CSS Images 3 §5.1), not
/// an absent one: an image declared 0 × 0 with no ratio is 0 tall, not the
/// 300 × 150 default object size. (Chrome lays out a source-less image so.)
#[test]
fn a_zero_natural_size_is_a_size() {
    let g = geo(r#"<html><body><style>body{margin-top:0;margin-right:0;margin-bottom:0;margin-left:0} .w{width:300px} .full{width:100%}</style>
        <div class="w"><img g="1" class="full" src="" width="0" height="0" natural-ratio="none"></div><img g="2" src="" width="0" height="0" natural-ratio="none"></body></html>"#);
    assert_eq!((el(&g, 1).w, el(&g, 1).h), (300 * PX, 0));
    assert_eq!((el(&g, 2).w, el(&g, 2).h), (0, 0));
}
