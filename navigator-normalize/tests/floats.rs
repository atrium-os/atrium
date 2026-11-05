//! ★ FLOATS BESIDE CONTENT, compiled by the producer (profile §7a C): the
//! renderer has no floats, so the normalizer turns a float with content
//! after it into a flex row, and cuts the row where the float ends.

use navigator_normalize::{normalize_and_measure, Inputs};
use navigator_render::{fontset::FontSet, html::render_html, Scene};
use navigator_style::cascade::Env;

fn run_r(html: &str) -> (String, Scene, navigator_normalize::Report) {
    let fonts = FontSet::load().unwrap();
    let (doc, report) = normalize_and_measure(html, &Inputs::default(), &fonts, &Env::default());
    let o = render_html(&doc, &fonts, &Env::default());
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    (doc, o.scene, report)
}

fn run(html: &str) -> (String, Scene) { let (d, s, _) = run_r(html); (d, s) }

/// The box of the first rect painted in `rgba`.
fn rect(s: &Scene, rgba: u32) -> (i64, i64, i64, i64) {
    let r = s.rects.iter().find(|r| r.rgba == rgba).unwrap_or_else(|| panic!("no rect {rgba:08x}"));
    (r.x, r.y, r.w, r.h)
}

/// (leftmost x, top y, rightmost x) of the runs of the first line holding `word`.
fn line_of(s: &Scene, word: &str) -> (i64, i64) {
    let r = s.runs.iter().find(|r| r.text.contains(word)).unwrap_or_else(|| panic!("no run {word}"));
    (r.x, r.y)
}

const WORDS: &str = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua";

fn article(float_css: &str, extra: &str) -> String {
    let mut body = String::from(r#"<div class="box"></div>"#);
    for i in 0..12 { body.push_str(&format!("<p>Para{i} {WORDS}</p>")) }
    format!(r#"<html><head><style>body {{ margin: 0px }} p {{ margin: 0px 0px 8px }}
        .box {{ {float_css}; width: 200px; height: 150px; background-color: #ff0000 }} {extra}</style></head><body>{body}</body></html>"#)
}

#[test]
fn a_right_float_has_text_beside_it_then_below_it_full_width() {
    let (doc, s) = run(&article("float: right", ""));
    let (bx, by, bw, bh) = rect(&s, 0xff0000ff);
    assert_eq!((bx + bw, by), (800 * 64, 0), "the box sits at the right edge, at the top");
    let (x0, y0) = line_of(&s, "Para0");
    assert!(y0 < by + bh && x0 < bx, "Para0 is beside the box, to its left");
    // Every paragraph starting beside the box is narrower than the page; the
    // first one starting below it has the full width again.
    let below = (0..12).map(|i| format!("Para{i}")).find(|w| line_of(&s, w).1 >= by + bh).expect("text continues below the box");
    let n: usize = below.trim_start_matches("Para").parse().unwrap();
    assert!(n >= 2, "more than one paragraph beside a 150 px box (first below: {below})");
    // A line's right end: its last word's start plus that word's last glyph
    // offset (offsets are cumulative from the run's start).
    let line_end = |para: &str| {
        let (_, y) = line_of(&s, para);
        s.runs.iter().filter(|r| r.y == y).map(|r| r.x + r.glyphs.last().map(|g| g.1 as i64).unwrap_or(0)).max().unwrap()
    };
    assert!(line_end(&format!("Para{}", n - 1)) < bx, "beside the box: wraps before it");
    assert!(line_end(&below) > bx, "below the box: takes the full width");
    // ★ Visual order is reading order: the right float (the one empty div)
    // comes AFTER the text beside it in the document.
    let empty_div = doc.match_indices("<div class=").find(|(i, _)| doc[*i..].find('>').is_some_and(|e| doc[*i + e + 1..].starts_with("</div>"))).map(|(i, _)| i).expect("the box");
    assert!(doc.find("Para0").unwrap() < empty_div, "the text beside the right float precedes it");
    assert!(!doc.contains("atrium-float"), "the producer's markers are removed");
}

#[test]
fn a_left_float_comes_first_and_text_is_to_its_right() {
    let (_, s) = run(&article("float: left", ""));
    let (bx, by, bw, bh) = rect(&s, 0xff0000ff);
    assert_eq!((bx, by), (0, 0));
    let (x0, y0) = line_of(&s, "Para0");
    assert!(y0 < by + bh && x0 >= bx + bw, "Para0 is to the right of the box");
}

#[test]
fn clear_ends_what_goes_beside() {
    let (_, s) = run(&article("float: right", "p:nth-child(3) { clear: both }"));
    let (_, by, _, bh) = rect(&s, 0xff0000ff);
    // Para1 is the paragraph that clears (the box is child 1).
    let (_, y1) = line_of(&s, "Para1");
    assert!(y1 >= by + bh, "the clearing paragraph starts below the box");
}

#[test]
fn a_float_with_nothing_after_it_is_left_alone() {
    let html = r#"<html><head><style>.f { float: left; width: 100px; height: 20px; background-color: #ff0000 }</style></head>
        <body><div><p>Text</p><div class="f"></div></div></body></html>"#;
    let (_, _, report) = run_r(html);
    assert_eq!(report.dropped.get("property `float` (no content before the next float or clear; dropped)"), Some(&1), "{:?}", report.dropped);
    assert_eq!(report.dropped.get("float beside content compiled to a flex row"), None);
    // The control: the same float with content after it is compiled.
    let (_, _, report) = run_r(&html.replace("<p>Text</p><div class=\"f\"></div>", "<div class=\"f\"></div><p>Text</p>"));
    assert_eq!(report.dropped.get("float beside content compiled to a flex row"), Some(&1), "{:?}", report.dropped);
}

#[test]
fn a_floated_table_has_its_height_when_the_row_is_cut() {
    // ★ ru Wikipedia: the infobox is a TABLE; fitted before its columns
    // were measured it was 2 px tall and every paragraph went below it.
    let mut body = String::from(r#"<table class="ib"><tr><td>Name</td><td>Value</td></tr>"#);
    for i in 0..8 { body.push_str(&format!("<tr><td>Key{i}</td><td>Some value {i}</td></tr>")) }
    body.push_str("</table>");
    for i in 0..10 { body.push_str(&format!("<p>Para{i} {WORDS}</p>")) }
    let html = format!(r#"<html><head><style>body {{ margin: 0px }} p {{ margin: 0px 0px 8px }}
        .ib {{ float: right; background-color: #00ff00 }}</style></head><body>{body}</body></html>"#);
    let (_, s) = run(&html);
    let (bx, by, _, bh) = rect(&s, 0x00ff00ff);
    assert!(bh > 100 * 64, "the infobox has its rows: {} px", bh / 64);
    let (x0, y0) = line_of(&s, "Para0");
    assert!(y0 < by + bh && x0 < bx, "Para0 is beside the infobox");
    let (x1, y1) = line_of(&s, "Para1");
    assert!(y1 < by + bh && x1 < bx, "and so is Para1");
}

#[test]
fn a_float_too_wide_for_anything_beside_it_has_its_content_below() {
    let (_, s, report) = run_r(&article("float: left", ".box { width: 700px }"));
    let (_, by, _, bh) = rect(&s, 0xff0000ff);
    let (_, y0) = line_of(&s, "Para0");
    assert!(y0 >= by + bh, "Para0 is below a 700 px float in an 800 px page: {y0} vs {by}+{bh}, {:?}", report.dropped);
    assert_eq!(report.dropped.get("float too wide for content beside it (content moved below)"), Some(&1), "{:?}", report.dropped);
}

#[test]
fn in_a_right_to_left_page_text_beside_a_left_float_stays_in_its_column() {
    // ★ ar Wikipedia: the infobox floats left in an rtl page; the text
    // beside it must wrap at the column, not run under the infobox.
    let (_, s) = run(&article("float: left", "body { direction: rtl }"));
    let (bx, by, bw, bh) = rect(&s, 0xff0000ff);
    assert_eq!((bx, by), (0, 0), "the box is at the left");
    for r in s.runs.iter().filter(|r| r.y < by + bh) {
        assert!(r.x >= bx + bw, "a word beside the box is in the column: {:?} at x {} (box ends {})", r.text, r.x / 64, (bx + bw) / 64);
    }
}
