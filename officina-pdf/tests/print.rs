//! Printing from Sheets and Slides: the PDFs the apps hand the print
//! sheet, read back and drawn by Folium (what Prelum's render jail does).

use std::sync::Arc;

use folium_content::{interpret, Shared};
use folium_render::{Renderer, View};
use officina_pdf::sheet::{self, Align, Cell, Setup, Sheet};
use officina_pdf::slides;
use officina_slides::model::{set_plain, Color, Deck, Fill, GradientStop, PhKind, Rect, Shape};
use officina_slides::ops::new_slide;
use officina_text::layout::PinnedFonts;

/// Each page's size (points) and pixels at 1 px to the point (RGBA), with
/// what Folium skipped.
fn draw(pdf: &[u8]) -> Vec<((f64, f64), u32, Vec<u8>)> {
    let doc = folium_doc::Doc::open(Box::new(folium_syntax::source::Bytes(Arc::from(pdf))), b"").expect("Folium opens it");
    let shared = Shared::new();
    let r = Renderer::new();
    (0..doc.page_count())
        .map(|i| {
            let page = doc.page(i).unwrap();
            let contents = doc.contents(&page).unwrap();
            let crop = folium_content::Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
            let dl = interpret(&doc.syntax, &shared, &contents, &page.resources, Some(crop), &|| false).unwrap();
            assert!(dl.skipped.is_empty(), "Folium skipped {:?}", dl.skipped);
            let view = View { crop, rotate: 0, scale: 1.0 };
            let (w, h) = view.size();
            let (pm, _) = r.render(&doc.syntax, &dl, &view, (0, 0, w.ceil() as u32, h.ceil() as u32), &|| false);
            ((crop.width(), crop.height()), pm.width(), pm.data().to_vec())
        })
        .collect()
}

fn px(p: &((f64, f64), u32, Vec<u8>), x: u32, y: u32) -> [u8; 3] {
    let i = ((y * p.1 + x) * 4) as usize;
    [p.2[i], p.2[i + 1], p.2[i + 2]]
}

fn reddish(c: [u8; 3]) -> bool {
    c[0] > 200 && c[1] < 80 && c[2] < 80
}

fn cells(r: u32, c: u32) -> Option<Cell> {
    match (r, c) {
        (0, 0) => Some(Cell { text: "Total".into(), bold: true, fill: Some(0xFF0000), borders: [None, None, None, Some((2.0, 0x000000))], ..Default::default() }),
        (0, 1) => Some(Cell { text: "123456789012345678901".into(), numeric: true, align: Align::Right, ..Default::default() }),
        (0, 2) => Some(Cell { text: "A long label that will not fit".into(), ..Default::default() }),
        _ => Some(Cell { text: format!("{r}:{c}"), numeric: true, align: Align::Right, ..Default::default() }),
    }
}

#[test]
fn a_sheet_prints_down_then_across_and_looks_as_it_does_on_screen() {
    let fonts = PinnedFonts::new().unwrap();
    // 40 columns of 64 px (48 pt) and 150 rows of 20 px (15 pt) on A4 with
    // 54 pt margins: 10 columns and 48 rows a page, 4 pages across, 4 down.
    let s = Sheet { name: "Q3", cols: vec![64.0; 40], rows: vec![20.0; 150], cell: &cells, gridlines: true, merges: Vec::new() };
    let (pdf, stats) = sheet::export(&s, &Setup::paper(595.0, 842.0), &fonts, &fonts, "", Vec::new(), &mut |_, _| {}).unwrap();
    assert_eq!(stats.pages, 16);
    assert_eq!(stats.fonts, 2, "the screen's face, regular and bold");
    let pages = draw(&pdf);
    assert_eq!(pages.len(), 16);
    assert_eq!(pages[0].0, (595.0, 842.0));
    // A1's fill, right of its text, and its thick bottom border.
    assert!(reddish(px(&pages[0], 98, 58)), "{:?}", px(&pages[0], 98, 58));
    assert!(px(&pages[0], 80, 68).iter().all(|&v| v < 60), "the bottom border");
    // Some text drawn in every page's first cell (numbers right-aligned).
    for (i, p) in pages.iter().enumerate().skip(1) {
        let dark = (54..102).flat_map(|x| (56..68).map(move |y| (x, y))).filter(|&(x, y)| px(p, x, y)[0] < 128).count();
        assert!(dark > 5, "page {i} shows its first cell's text");
    }
    // Page 2 is the next rows (down first): the margins stay white.
    assert_eq!(px(&pages[1], 20, 20), [255, 255, 255]);
}

#[test]
fn numbers_that_do_not_fit_show_hashes_and_text_is_cut() {
    let fonts = PinnedFonts::new().unwrap();
    let s = Sheet { name: "", cols: vec![64.0; 3], rows: vec![20.0], cell: &cells, gridlines: false, merges: Vec::new() };
    let (pdf, _) = sheet::export(&s, &Setup::paper(595.0, 842.0), &fonts, &fonts, "Cut", Vec::new(), &mut |_, _| {}).unwrap();
    // The text is in the ToUnicode maps: '#' and '…' were used, the long
    // number's digits past what fits were not.
    let raw = inflate_all(&pdf);
    assert!(raw.contains("<0023>"), "'#' drawn");
    assert!(raw.contains("<2026>"), "'…' drawn");
}

#[test]
fn fit_to_width_puts_every_column_on_one_page_across() {
    let fonts = PinnedFonts::new().unwrap();
    let s = Sheet { name: "", cols: vec![64.0; 15], rows: vec![20.0; 150], cell: &cells, gridlines: true, merges: Vec::new() };
    let mut setup = Setup::paper(595.0, 842.0);
    let (_, plain) = sheet::export(&s, &setup, &fonts, &fonts, "", Vec::new(), &mut |_, _| {}).unwrap();
    setup.fit_width = true;
    let (_, fit) = sheet::export(&s, &setup, &fonts, &fonts, "", Vec::new(), &mut |_, _| {}).unwrap();
    assert_eq!(plain.pages, 2 * 4);
    assert!(fit.pages <= 3, "one across, fewer down at the smaller scale ({})", fit.pages);
}

fn deck() -> Deck {
    let mut d = Deck::new();
    let title = d.layout_of_kind("title").unwrap().id;
    for i in 0..3 {
        let mut s = new_slide(&mut d, title);
        for sh in s.shapes.iter_mut() {
            if matches!(sh.ph.map(|p| p.kind), Some(PhKind::CtrTitle | PhKind::Title)) {
                set_plain(&mut sh.text_mut().unwrap().doc, &format!("Slide {}", i + 1));
            }
        }
        if i == 1 {
            s.hidden = true;
        }
        if i == 2 {
            let (a, b, c) = (d.new_id(), d.new_id(), d.new_id());
            // A red box at the top left, a black-to-white gradient along the
            // bottom, a picture at the top right.
            s.shapes.push(Shape::rect(a, Rect::new(0, 0, 2_540_000, 1_270_000), Fill::Solid(Color::rgb(0xFF0000))));
            let stops = vec![GradientStop { pos: 0, color: Color::rgb(0x000000) }, GradientStop { pos: 100_000, color: Color::rgb(0xFFFFFF) }];
            s.shapes.push(Shape::rect(b, Rect::new(0, 5_588_000, 12_192_000, 1_270_000), Fill::Gradient { stops, angle: 0, radial: false }));
            let m = d.add_media("image/png", &png(8, 8, [0, 0, 255]));
            s.shapes.push(Shape::new(c, Rect::new(9_652_000, 0, 2_540_000, 1_270_000), officina_slides::model::ShapeKind::Picture { media: m, crop: [0; 4], line: None }));
        }
        d.push_slide(s);
    }
    d
}

fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    let mut wr = e.write_header().unwrap();
    wr.write_image_data(&rgb.repeat((w * h) as usize)).unwrap();
    drop(wr);
    out
}

#[test]
fn a_deck_prints_a_page_per_slide_as_the_screen_draws_it() {
    let fonts = PinnedFonts::new().unwrap();
    let mut d = deck();
    let (pdf, p) = slides::export(&mut d, None, &fonts, &fonts, "Deck", Vec::new(), &mut |_, _| {}).unwrap();
    assert_eq!((p.stats.pages, p.hidden, p.damaged), (2, 1, 0), "the hidden slide left out");
    assert_eq!(p.stats.images, 1);
    assert!(p.stats.fonts >= 1);
    let pages = draw(&pdf);
    assert_eq!(pages[1].0, (960.0, 540.0), "16:9 at the slide's own size");
    let last = &pages[1];
    assert!(reddish(px(last, 20, 20)), "the red box {:?}", px(last, 20, 20));
    assert_eq!(px(last, 900, 20), [0, 0, 255], "the picture");
    let (l, r) = (px(last, 10, 520)[0], px(last, 950, 520)[0]);
    assert!(l < 30 && r > 225, "the gradient runs dark to light ({l} to {r})");
    let mid = px(last, 480, 520)[0];
    assert!((100..156).contains(&mid), "and blends in the middle ({mid})");
    // The title's text drawn in the middle of the first slide.
    let dark = (200..760).flat_map(|x| (150..300).map(move |y| (x, y))).filter(|&(x, y)| px(&pages[0], x, y)[0] < 128).count();
    assert!(dark > 200, "the title's words ({dark})");
}

#[test]
fn on_paper_slides_are_fitted_and_centred() {
    let fonts = PinnedFonts::new().unwrap();
    let mut d = deck();
    let (pdf, p) = slides::export(&mut d, Some((595.0, 842.0)), &fonts, &fonts, "", Vec::new(), &mut |_, _| {}).unwrap();
    assert_eq!(p.stats.pages, 2);
    let pages = draw(&pdf);
    assert_eq!(pages[1].0, (595.0, 842.0));
    // 960 × 540 at 595/960: 595 × 334.7, starting 253.6 pt down.
    assert_eq!(px(&pages[1], 5, 200), [255, 255, 255], "above the slide");
    assert!(reddish(px(&pages[1], 10, 262)), "the red box at the slide's corner {:?}", px(&pages[1], 10, 262));
}

/// Every Flate stream of `pdf`, inflated, as text (to look for what it holds).
fn inflate_all(pdf: &[u8]) -> String {
    let doc = folium_syntax::Document::from_bytes(pdf.to_vec()).unwrap();
    let mut out = String::new();
    for n in 1..doc.object_count() as u32 {
        let Ok(o) = doc.get(folium_syntax::Ref { num: n, gen: 0 }) else { continue };
        if let Some(s) = o.as_stream() {
            if let Ok(d) = doc.decode(s) {
                out.push_str(&String::from_utf8_lossy(&d.data));
            }
        }
    }
    out
}


#[test]
fn a_merged_area_prints_as_one_cell_even_across_pages() {
    let fonts = PinnedFonts::new().unwrap();
    // A1:C2 merged (red, "Total" in A1); J5:K5 runs over the page's edge
    // (10 columns a page).
    let s = Sheet { name: "", cols: vec![64.0; 12], rows: vec![20.0; 6], cell: &cells, gridlines: true, merges: vec![[0, 0, 1, 2], [4, 9, 4, 10]] };
    let (pdf, stats) = sheet::export(&s, &Setup::paper(595.0, 842.0), &fonts, &fonts, "", Vec::new(), &mut |_, _| {}).unwrap();
    assert_eq!(stats.pages, 2);
    let pages = draw(&pdf);
    // Red across all three columns and both rows: no gridline at B's or
    // C's left edge or between the rows, and B1's own text isn't drawn.
    for (x, y) in [(102, 62), (150, 62), (190, 69), (130, 80)] {
        assert!(reddish(px(&pages[0], x, y)), "({x}, {y}) {:?}", px(&pages[0], x, y));
    }
    // The cell after the area (D1) is an ordinary one, with its gridline.
    assert!(!reddish(px(&pages[0], 200, 62)));
    // J5:K5's piece on the second page: K5 has no text of its own drawn
    // ("4:10"), so the cell is blank there.
    let dark = (54..102).flat_map(|x| (115..129).map(move |y| (x, y))).filter(|&(x, y)| px(&pages[1], x, y)[0] < 128).count();
    assert_eq!(dark, 0, "the area's text stays on the page with its first cell");
}
