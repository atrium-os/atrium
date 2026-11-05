//! lines_in <normalized.html> <y0> <y1>: per line (run y) in [y0, y1) px, the
//! leftmost run start and the rightmost run end (review tooling).
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let html = std::fs::read_to_string(&a[0]).unwrap();
    let (y0, y1): (i64, i64) = (a[1].parse().unwrap(), a[2].parse().unwrap());
    let fonts = navigator_render::fontset::FontSet::load().unwrap();
    let o = navigator_render::html::render_html(&html, &fonts, &Default::default());
    let mut lines: std::collections::BTreeMap<i64, (i64, i64, String)> = Default::default();
    for r in &o.scene.runs {
        let y = r.y / 64;
        if y < y0 || y >= y1 { continue }
        let end = r.x + r.glyphs.last().map(|g| g.1 as i64).unwrap_or(0);
        let e = lines.entry(y).or_insert((i64::MAX, i64::MIN, String::new()));
        e.0 = e.0.min(r.x / 64); e.1 = e.1.max(end / 64);
        if e.2.len() < 40 { e.2.push_str(&r.text); e.2.push(' ') }
    }
    for (y, (l, r, t)) in lines { println!("y={y} left={l} right={r} {t}") }
}
