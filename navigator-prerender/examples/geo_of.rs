//! geo_of <normalized.html> <text>: the boxes of the element holding `text`
//! and its ancestors (review tooling).
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let html = std::fs::read_to_string(&a[0]).unwrap();
    let fonts = navigator_render::fontset::FontSet::load().unwrap();
    let dom = navigator_dom::parse(&html);
    let geo = navigator_render::html::layout_geometry(&dom.serialize_marked("k"), &fonts, &Default::default(), &Default::default(), "k");
    if a.len() > 2 && a[2] == "--kids-of-class" {
        for h in 0..dom.nodes.len() as u32 {
            if dom.attr(h, "class").is_some_and(|c| c.split_whitespace().any(|x| x == a[1])) {
                for k in dom.element_children(h) {
                    let g = geo.elements.get(&(k as u64));
                    println!("<{}> class={:?} {:?}", dom.tag(k).unwrap_or("?"), dom.attr(k, "class").unwrap_or(""), g.map(|g| (g.x / 64, g.w / 64, g.y / 64, g.h / 64)));
                }
                return;
            }
        }
    }
    let mut h = (0..dom.nodes.len() as u32).find(|h| matches!(dom.get(*h).map(|n| &n.kind), Some(navigator_dom::Kind::Text(t)) if t.contains(a[1].as_str()))).expect("text");
    while let Some(p) = dom.get(h).and_then(|n| n.parent) {
        if let Some(g) = geo.elements.get(&(p as u64)) {
            println!("<{}> class={:?} x={} w={} y={} h={} kids={}", dom.tag(p).unwrap_or("?"), dom.attr(p, "class").unwrap_or(""), g.x / 64, g.w / 64, g.y / 64, g.h / 64, dom.element_children(p).len());
        }
        h = p;
    }
}
