#![cfg(feature = "raster")]
//! ★ BANDS, phase 2 (backend §4.8 step 3): checkpoints. A document laid out
//! band by band — each band from the previous one's checkpoint, and again
//! from its own after eviction — draws every band's rows as the whole
//! layout draws them, compared by pixels; and a band laid out again is
//! identical to the first time.

use navigator_render::{bands::{self, Banded}, fontset::FontSet, html::render_html, nsg, raster, Scene};
use navigator_style::cascade::Env;

const PX: i64 = 64;

/// Sections with backgrounds and borders that span many bands, lists with
/// markers, flex cards, a relatively positioned box with a shadow, nested
/// wrappers, a fixed header written at the end, and an absolutely
/// positioned note.
fn page(n: usize) -> String {
    let mut b = String::from(r#"<html><head><style>
        body { margin: 8px; background-color: #fafafa }
        p { margin: 0px 0px 10px }
        section { background-color: #eef4ff; border-top: 3px solid #3366aa; border-bottom: 5px solid #aa3366; padding: 6px 10px; margin: 0px 0px 14px }
        .inner { background-color: #fff8e8; padding: 4px; margin: 0px 0px 8px }
        .card { display: flex; background-color: #eeeeff } .card div { flex-grow: 1; padding-left: 4px }
        .lift { position: relative; top: -6px; box-shadow: 0px 4px 8px 0px #00000066; background-color: #ffffff; padding: 4px }
        .note { position: absolute; top: 40px; right: 10px; width: 120px; background-color: #ffeeee }
        .wrap { position: relative }
        header { position: fixed; top: 0px; left: 0px; width: 800px; height: 24px; background-color: #222244; color: #ffffff }
        h2 { margin: 12px 0px; color: #333366 }
        ol { margin: 0px 0px 10px; padding-left: 30px }
        .anchor { height: 0px }
        </style></head><body><h2>Title</h2><div class="wrap"><div class="note">A positioned note</div>"#);
    for s in 0..n {
        b.push_str(&format!("<section><h2>Section {s}</h2>"));
        for i in 0..9 {
            match i {
                2 => b.push_str(&format!(r#"<div class="card"><div>Left {s}.{i}</div><div>Right side {s}.{i} with more words in it</div></div>"#)),
                4 => b.push_str(&format!(r#"<div class="lift">Lifted {s}.{i}: a box that sits above its place, with a shadow below it.</div>"#)),
                6 => b.push_str(&format!("<ol><li>First item {s}</li><li>Second item {s}, long enough to wrap onto a second line of text in the list.</li><li>Third</li><li><div class=\"anchor\"></div><p>Fourth {s}: a list item whose first child is an empty block, so its marker waits for this line.</p></li></ol>")),
                7 => b.push_str(&format!(r#"<div class="inner"><p>Inner {s}.{i} one.</p><p>Inner {s}.{i} two, a little longer than the first one is.</p></div>"#)),
                _ => b.push_str(&format!("<p>Paragraph {s}.{i}: the quick brown fox jumps over the lazy dog, again and again, until the line wraps at least once here.</p>")),
            }
        }
        b.push_str("</section>");
    }
    b.push_str("</div><header>A fixed header, written last</header></body></html>");
    b
}

/// Rows `[y0, y1)` (px) of `scene`, 8 bits per channel.
fn rows(scene: &Scene, fonts: &FontSet, y0: i64, y1: i64) -> Vec<[u8; 4]> {
    let mut s = bands::translated(scene, y0 * PX);
    s.height = (y1 - y0) * PX;
    raster::paint(&s, fonts, &Default::default()).px.iter().map(|p| p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)).collect()
}

/// The whole layout's nodes that paint in `[y0, y1)` px.
fn full_rows(full: &Scene, idx: &bands::BandIndex, fonts: &FontSet, y0: i64, y1: i64) -> Vec<[u8; 4]> {
    rows(&bands::scene_of(full, &idx.nodes_in(y0 * PX, y1 * PX)), fonts, y0, y1)
}

fn check_every_band(doc: &str, band_px: i64) -> (usize, bands::BandStats) {
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    let full = render_html(doc, &fonts, &env).scene;
    let idx = bands::index(&full, 64 * PX);
    let mut b = Banded::new(doc, &fonts, &env, &Default::default(), band_px * PX);
    b.settle();
    let (ext, settled) = b.extent();
    assert!(settled);
    assert_eq!(ext, full.height, "the settled extent is the whole layout's");
    let n = b.laid_out();
    for k in 0..n {
        b.ensure(k);
        let (y0, y1) = b.bands[k].range;
        // Whole pixel rows inside the band: a row a cut passes through is
        // drawn by both bands, each clipped to its own part.
        let (y0, y1) = ((y0.max(0) + PX - 1) / PX, y1.min(full.height) / PX);
        if y0 >= y1 { continue }
        let mine = rows(&b.composite(k).unwrap(), &fonts, y0, y1);
        let theirs = full_rows(&full, &idx, &fonts, y0, y1);
        if mine != theirs {
            let w = (full.width / PX) as usize;
            let first = mine.iter().zip(&theirs).position(|(a, c)| a != c).unwrap();
            panic!("band {k} ({y0}..{y1} px) differs from the whole layout first at x={} y={}: {:?} vs {:?}",
                   first % w, y0 + (first / w) as i64, mine[first], theirs[first]);
        }
        b.evict(k);
    }
    (n, b.stats)
}

#[test]
fn every_band_draws_what_the_whole_layout_draws() {
    let (n, st) = check_every_band(&page(12), 300);
    assert!(n >= 10, "{n} bands");
    assert!(!st.whole, "checkpoints held");
    assert!(st.reruns > 0, "bands were laid out again from their checkpoints");
}

#[test]
fn bands_cut_inside_nested_spanning_boxes() {
    // Small bands: most cuts fall inside a section, inside .wrap.
    let (n, st) = check_every_band(&page(4), 120);
    assert!(n >= 15, "{n} bands");
    assert!(st.suppressed > 0, "some band cut a box before its size was known");
}

#[test]
fn a_band_laid_out_again_from_its_checkpoint_is_identical() {
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    let doc = page(10);
    let mut b = Banded::new(&doc, &fonts, &env, &Default::default(), 300 * PX);
    b.settle();
    let n = b.laid_out();
    let mut first = vec![];
    for k in 0..n { b.ensure(k); first.push(nsg::write(b.scene(k).unwrap(), &fonts)); }
    for k in 0..n { assert!(b.evict(k)) }
    assert!((0..n).all(|k| b.scene(k).is_none()), "evicted");
    // In reverse: a far jump lays out from its checkpoint, not from the top.
    for k in (0..n).rev() {
        let before = b.stats.reruns;
        assert!(b.ensure(k));
        assert_eq!(b.stats.reruns, before + 1, "band {k}: one run, from its checkpoint");
        assert_eq!(nsg::write(b.scene(k).unwrap(), &fonts), first[k], "band {k} laid out again is identical");
    }
}

#[test]
fn a_planted_difference_is_caught() {
    // The comparison can fail: one recoloured leaf in a band is found.
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    let doc = page(6);
    let full = render_html(&doc, &fonts, &env).scene;
    let idx = bands::index(&full, 64 * PX);
    let mut b = Banded::new(&doc, &fonts, &env, &Default::default(), 300 * PX);
    b.settle();
    let k = 3;
    b.ensure(k);
    let (y0, y1) = b.bands[k].range;
    let (y0, y1) = (y0 / PX, y1 / PX);
    let mut c = b.composite(k).unwrap();
    let i = c.rects.iter().position(|r| r.y > y0 * PX + 20 * PX && r.y < y1 * PX - 20 * PX && r.w > 0 && r.h > 0).expect("a box in the band");
    c.rects[i].rgba ^= 0xff000000;
    assert_ne!(rows(&c, &fonts, y0, y1), full_rows(&full, &idx, &fonts, y0, y1));
}

#[test]
fn the_overlay_holds_what_paints_outside_its_band() {
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    let doc = page(8);
    let mut b = Banded::new(&doc, &fonts, &env, &Default::default(), 300 * PX);
    b.settle();
    // The fixed header is written last and paints at the top: the overlay
    // draws it over band 0.
    let ov = b.overlay();
    assert!(ov.runs.iter().any(|r| r.text.contains("fixed") || r.text == "A"), "the fixed header is in the overlay");
    assert!(b.stats.cross_leaves > 0);
}

#[test]
fn a_page_that_fits_is_one_band() {
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    let doc = "<html><body><p>Short.</p></body></html>";
    let mut b = Banded::new(doc, &fonts, &env, &Default::default(), 600 * PX);
    b.settle();
    assert_eq!(b.laid_out(), 1);
    b.ensure(0);
    let full = render_html(doc, &fonts, &env).scene;
    assert_eq!(nsg::write(&b.composite(0).unwrap(), &fonts), nsg::write(&full, &fonts));
}

#[test]
fn list_markers_survive_a_cut_before_the_first_line() {
    // whatwg's algorithm steps: `<li>` then white space, then the step's
    // `<p>`. A band cut before that `<p>` must leave the item's marker for
    // the line it begins — in the band's first run and when laid out again.
    let mut doc = String::from("<html><head><style>body { margin: 8px } p { margin: 0px 0px 6px } ol { padding-left: 40px } li { padding-top: 14px }</style></head><body><ol>");
    for i in 0..80 {
        doc.push_str(&format!("<li>\n  <p>Step {i}: if the input is a range, then take the first part of it and go on with the next step.</p>"));
        if i % 9 == 4 { doc.push_str("<ol><li><p>A nested step.</p></li><li><div></div><p>Another, after an empty block.</p></li></ol>") }
        doc.push_str("</li>");
    }
    doc.push_str("</ol></body></html>");
    for band in [60, 97, 150, 211, 333] {
        let (n, _) = check_every_band(&doc, band);
        assert!(n > 12, "{n} bands at {band} px");
    }
}
