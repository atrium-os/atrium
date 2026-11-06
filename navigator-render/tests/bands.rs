#![cfg(feature = "raster")]
//! ★ BANDS, phase 1 (backend §4.8 step 3): the first screen laid out alone
//! draws what the full layout draws there, and a band's own scene draws its
//! rows as the full scene does — compared by pixels.

use navigator_render::{bands, fontset::FontSet, html::{premeasure_tables, render_html, render_html_first}, raster, Scene};
use navigator_style::cascade::Env;

const PX: i64 = 64;

fn page(paras: usize) -> String {
    let mut b = String::from(r#"<html><head><style>body { margin: 8px; background-color: #fafafa } p { margin: 0px 0px 10px }
        .card { display: flex; background-color: #eeeeff } .card div { flex-grow: 1; padding-left: 4px }
        h2 { margin: 12px 0px; color: #333366 }</style></head><body><h2>Title</h2>"#);
    for i in 0..paras {
        if i % 7 == 3 { b.push_str(&format!(r#"<div class="card"><div>Left {i}</div><div>Right side {i} with more words in it</div></div>"#)) }
        b.push_str(&format!("<p>Paragraph {i}: the quick brown fox jumps over the lazy dog, again and again, until the line wraps at least once here.</p>"));
    }
    b.push_str("</body></html>");
    b
}

/// The pixels of `scene` in rows `[y0, y1)` (px), painted on a canvas as
/// tall as `y1`.
fn rows(scene: &Scene, fonts: &FontSet, y0: usize, y1: usize) -> Vec<[f32; 4]> {
    // Paint a copy limited to the rows wanted.
    let mut s = bands::scene_of(scene, &(0..scene.order.len() as u32).collect::<Vec<_>>());
    s.height = y1 as i64 * PX;
    let cv = raster::paint(&s, fonts, &Default::default());
    cv.px[y0 * cv.w..y1 * cv.w].to_vec()
}

#[test]
fn the_first_screen_draws_what_the_full_layout_draws_there() {
    let fonts = FontSet::load().unwrap();
    let doc = page(120);
    let full = render_html(&doc, &fonts, &Env::default()).scene;
    let limit = 900; // px: a screen and a half
    let (first, truncated) = render_html_first(&doc, &fonts, &Env::default(), &Default::default(), limit * PX);
    let first = first.scene;
    assert!(truncated, "a long page is cut");
    assert!(first.order.len() * 3 < full.order.len(), "far less was laid out: {} of {} nodes", first.order.len(), full.order.len());
    assert_eq!(rows(&first, &fonts, 0, limit as usize), rows(&full, &fonts, 0, limit as usize), "the first screen's pixels are the full layout's");
    // The comparison can fail: one recoloured box in the first screen is caught.
    let mut planted = bands::scene_of(&first, &(0..first.order.len() as u32).collect::<Vec<_>>());
    let i = planted.rects.iter().position(|r| r.y < (limit as i64 - 50) * PX && r.h > 0 && r.w > 0).expect("a box on the first screen");
    planted.rects[i].rgba ^= 0xff000000;
    assert_ne!(rows(&planted, &fonts, 0, limit as usize), rows(&full, &fonts, 0, limit as usize), "a planted difference is caught");
}

#[test]
fn a_long_table_stops_measuring_rows_at_the_limit() {
    let fonts = FontSet::load().unwrap();
    let mut doc = String::from("<html><head><style>body { margin: 0px } td { padding-left: 2px }</style></head><body><p>Before</p><table>");
    for i in 0..1000 { doc.push_str(&format!("<tr><td>Row {i}</td><td>value {}</td></tr>", i * 7)) }
    doc.push_str("</table><p>After</p></body></html>");
    let (doc, _) = premeasure_tables(&doc, &fonts, &Env::default());
    let full = render_html(&doc, &fonts, &Env::default()).scene;
    let (first, truncated) = render_html_first(&doc, &fonts, &Env::default(), &Default::default(), 600 * PX);
    let first = first.scene;
    assert!(truncated);
    let rows_of = |s: &Scene| s.runs.iter().filter(|r| r.text == "Row").count();
    assert!(rows_of(&first) < 60 && rows_of(&full) == 1000, "rows laid out: first {} full {}", rows_of(&first), rows_of(&full));
    assert_eq!(rows(&first, &fonts, 0, 600), rows(&full, &fonts, 0, 600));
}

#[test]
fn a_page_that_fits_is_laid_out_whole_and_unchanged() {
    let fonts = FontSet::load().unwrap();
    let doc = page(3);
    let full = render_html(&doc, &fonts, &Env::default()).scene;
    let (first, truncated) = render_html_first(&doc, &fonts, &Env::default(), &Default::default(), 10_000 * PX);
    let first = first.scene;
    assert!(!truncated);
    assert_eq!(navigator_render::nsg::write(&first, &fonts), navigator_render::nsg::write(&full, &fonts));
}

#[test]
fn each_band_draws_its_rows_as_the_full_scene_does() {
    let fonts = FontSet::load().unwrap();
    let full = render_html(&page(40), &fonts, &Env::default()).scene;
    let idx = bands::index(&full, 300 * PX);
    assert!(idx.bands.len() >= 4, "{} bands", idx.bands.len());
    let total = (full.height / PX) as usize;
    let whole = rows(&full, &fonts, 0, total);
    let w = (full.width / PX) as usize;
    for b in 0..idx.bands.len() {
        let (y0, y1) = (b * 300, ((b + 1) * 300).min(total));
        if y0 >= y1 { continue }
        let mut band = bands::band_scene(&full, &idx, b);
        band.height = y1 as i64 * PX;
        let cv = raster::paint(&band, &fonts, &Default::default());
        assert_eq!(&cv.px[y0 * cv.w..y1 * cv.w], &whole[y0 * w..y1 * w], "band {b} draws rows {y0}..{y1} as the full scene");
        assert!(band.order.len() < full.order.len(), "band {b} holds part of the scene");
    }
}
