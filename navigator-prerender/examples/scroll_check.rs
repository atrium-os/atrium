//! scroll_check <recording.json> <zoom_64>: the broker's scroll check on one
//! page at one page zoom, with its error.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let json = std::fs::read_to_string(&a[0]).unwrap();
    let z: u32 = a[1].parse().unwrap();
    let fonts = navigator_render::fontset::FontSet::load().unwrap();
    let r = navigator_normalize::recording::parse(&json).unwrap();
    let base = navigator_style::cascade::Env::default();
    let (n, _) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, &fonts, &base);
    let subs = navigator_render::conformance::subresources_from_recording_json(&json);
    let o = navigator_render::html::render_html_with(&n, &fonts, &navigator_style::cascade::Env::zoomed(800.0, 600.0, z), &subs);
    println!("{:?}", navigator_render::scroll::validate(&o.scene));
}
