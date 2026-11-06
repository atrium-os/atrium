//! ★ COLLAPSED TABLE BORDERS, compiled by the producer (CSS 2.1 §17.6.2):
//! each grid line is drawn once, by the border that wins the conflict.

use navigator_normalize::{normalize_and_measure, Inputs};
use navigator_render::{fontset::FontSet, html::render_html, Scene};
use navigator_style::cascade::Env;

fn run(html: &str) -> (Scene, navigator_normalize::Report) {
    let fonts = FontSet::load().unwrap();
    let (doc, report) = normalize_and_measure(html, &Inputs::default(), &fonts, &Env::default());
    let o = render_html(&doc, &fonts, &Env::default());
    assert!(o.diagnostics.is_empty(), "{:?}", o.diagnostics);
    (o.scene, report)
}

/// The colour painted at (x + 0.5, y + 0.5) px: the last rect covering it.
fn colour_at(s: &Scene, x: i64, y: i64) -> Option<u32> {
    let (px, py) = (x * 64 + 32, y * 64 + 32);
    let mut out = None;
    for &(kind, i) in &s.order {
        if kind != 0 { continue }
        let r = &s.rects[i];
        let inside = |x0: i64, y0: i64, x1: i64, y1: i64| px >= x0 && px < x1 && py >= y0 && py < y1;
        if !inside(r.x, r.y, r.x + r.w, r.y + r.h) { continue }
        if r.ring > 0 && inside(r.x + r.ring, r.y + r.ring, r.x + r.w - r.ring, r.y + r.h - r.ring) { continue }
        out = Some(r.rgba);
    }
    out
}

/// Runs of `rgba` along row y: (first x, width) in px.
fn runs(s: &Scene, y: i64, rgba: u32) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = vec![];
    for x in 0..800 {
        if colour_at(s, x, y) == Some(rgba) {
            match out.last_mut() { Some((x0, w)) if *x0 + *w == x => *w += 1, _ => out.push((x, 1)) }
        }
    }
    out
}

fn table(css: &str, dir: &str) -> String {
    format!(r#"<html><head><style>body {{ margin: 0px; direction: {dir} }}
        table {{ border-collapse: collapse }} td {{ width: 100px; height: 20px; padding: 0px }} {css}</style></head>
        <body><table><tr><td class="a">a</td><td class="b">b</td><td class="c">c</td></tr>
        <tr><td class="a">d</td><td class="b">e</td><td class="c">f</td></tr></table></body></html>"#)
}

const GREY: u32 = 0xa2a9b1ff;
const RED: u32 = 0xff0000ff;
const BLUE: u32 = 0x0000ffff;

#[test]
fn a_uniform_grid_draws_each_line_once() {
    // Wikipedia's wikitable: the table and every cell 1px solid.
    let (s, report) = run(&table("table, td { border: 1px solid #a2a9b1 }", "ltr"));
    assert!(report.dropped.keys().any(|k| k.starts_with("collapsed table borders compiled")), "{:?}", report.dropped);
    // Through the middle of the first row: four 1 px lines, no doubles, no gaps.
    let lines = runs(&s, 10, GREY);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines.iter().all(|(_, w)| *w == 1), "every line is 1 px: {lines:?}");
    // Down the middle of the first column: three horizontal lines, each 1 px.
    let mut ys = vec![];
    for y in 0..80 { if colour_at(&s, 50, y) == Some(GREY) { ys.push(y) } }
    assert_eq!(ys.len(), 3, "top, between the rows, bottom: {ys:?}");
}

#[test]
fn the_wider_border_wins_then_the_stronger_style() {
    let (s, _) = run(&table("td { border: 1px solid #0000ff } .a { border-right: 3px solid #ff0000 }", "ltr"));
    let red = runs(&s, 10, RED);
    assert_eq!(red.len(), 1, "{red:?}");
    assert_eq!(red[0].1, 3, "the 3 px border is the line between a and b");
    assert!(runs(&s, 10, BLUE).iter().all(|(x, _)| *x < red[0].0 || *x > red[0].0 + 2), "and no blue line beside it");
    // Equal widths: solid beats dashed.
    let (s, _) = run(&table("td { border: 2px dashed #0000ff } .b { border-left: 2px solid #ff0000 }", "ltr"));
    assert_eq!(runs(&s, 10, RED).len(), 1, "the solid border wins the a|b line");
}

#[test]
fn hidden_suppresses_the_line() {
    let (s, _) = run(&table("td { border: 2px solid #0000ff } .a { border-right-style: hidden }", "ltr"));
    let blue = runs(&s, 10, BLUE);
    // Lines at the start of a, between b|c and at the end of c: the a|b line is gone.
    assert_eq!(blue.len(), 3, "{blue:?}");
}

#[test]
fn a_cell_beats_the_table_and_the_start_cell_wins_a_tie() {
    // Same width and style: the cell's colour wins over the table's (rule 4)…
    let (s, _) = run(&table("table { border: 1px solid #ff0000 } td { border: 1px solid #0000ff }", "ltr"));
    assert!(runs(&s, 10, RED).is_empty(), "the outer lines are the cells' blue");
    // …and between two equal cells, the one at the start (rule 5).
    let (s, _) = run(&table("td { border: 1px solid #0000ff } .a { border-color: #ff0000 }", "ltr"));
    let red = runs(&s, 10, RED);
    assert_eq!(red.len(), 2, "a's own two lines, start and a|b: {red:?}");
}

#[test]
fn in_a_right_to_left_table_the_start_is_on_the_right() {
    let (s, _) = run(&table("td { border: 1px solid #0000ff } .a { border-color: #ff0000 }", "rtl"));
    let red = runs(&s, 10, RED);
    assert_eq!(red.len(), 2, "{red:?}");
    // Column a is on the right: its lines are the rightmost two.
    let blue = runs(&s, 10, BLUE);
    assert!(red.iter().all(|r| blue.iter().all(|b| b.0 < r.0)), "red right of blue: red {red:?} blue {blue:?}");
}

#[test]
fn a_separated_table_is_left_alone() {
    let html = table("td { border: 1px solid #a2a9b1 }", "ltr").replace("border-collapse: collapse", "border-collapse: separate");
    let (s, report) = run(&html);
    assert!(!report.dropped.keys().any(|k| k.starts_with("collapsed table borders compiled")), "{:?}", report.dropped);
    // Separated: each cell draws its own pair, spacing apart.
    assert_eq!(runs(&s, 10, GREY).len(), 6, "two lines per cell");
}
