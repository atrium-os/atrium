//! band_probe <normalized.html>: for the last screen of the document, the
//! nodes a full walk finds that the band index does not (review tooling).
use navigator_render::{bands, fontset::FontSet, html::render_html};
use navigator_style::cascade::Env;
const PX: i64 = 64;
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let fonts = FontSet::load().unwrap();
    let s = render_html(&std::fs::read_to_string(&p).unwrap(), &fonts, &Env::default()).scene;
    let ix = bands::index(&s, 600 * PX);
    let y0 = s.height - 600 * PX;
    let y1 = s.height;
    let got: std::collections::BTreeSet<u32> = ix.nodes_in(y0, y1 + 1).into_iter().collect();
    println!("height {} px, bands {}, extent {} px, index finds {}", s.height / PX, ix.bands.len(), ix.extent / PX, got.len());
    let mut missing = 0;
    for (o, &(k, i)) in s.order.iter().enumerate() {
        if let Some((a, b)) = bands::extent(&s, k, i) {
            if b >= y0 && a <= y1 && !got.contains(&(o as u32)) {
                missing += 1;
                if missing <= 5 { println!("missing order {o} kind {k} extent {}..{} px", a / PX, b / PX) }
            }
        }
    }
    println!("missing {missing}");
}
