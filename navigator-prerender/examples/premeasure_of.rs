//! premeasure_of <file.html>: the table pre-measure's column declarations
//! for a document (review tooling).
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let html = std::fs::read_to_string(&a[0]).unwrap();
    let fonts = navigator_render::fontset::FontSet::load().unwrap();
    let (out, _) = navigator_render::html::premeasure_tables(&html, &fonts, &Default::default());
    for l in out.lines().filter(|l| l.contains(".tc")) { println!("{l}") }
}
