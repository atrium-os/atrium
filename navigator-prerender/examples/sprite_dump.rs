//! sprite_dump <cache-dir> <recording.json> <n>: the n-th inline icon with an
//! external `<use>`, made standalone with its sprite files (review tooling).
use navigator_prerender::fetch::{Fetcher, HttpFetcher};
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut f = HttpFetcher::new(std::path::PathBuf::from(&a[0]));
    let json = std::fs::read_to_string(&a[1]).unwrap();
    let mut r = navigator_normalize::recording::parse(&json).unwrap();
    let dom = navigator_dom::parse(&r.document);
    let base = url::Url::parse(&r.url).unwrap();
    for h in dom.by_tag_anywhere("use") {
        let Some(v) = dom.attr(h, "href").or_else(|| dom.attr(h, "xlink:href")) else { continue };
        let Some((file, _)) = v.split_once('#') else { continue };
        if file.is_empty() || r.inputs.sprites.contains_key(file) { continue }
        if let Ok(t) = f.get(base.join(file).unwrap().as_str()) { r.inputs.sprites.insert(file.to_string(), t); }
    }
    let n: usize = a[2].parse().unwrap();
    let out: Vec<String> = navigator_normalize::svg_inline::standalone(&r.document, &r.inputs).into_iter().filter(|s| s.contains("atrium-sprite-")).collect();
    println!("{}", out[n.min(out.len() - 1)]);
}
