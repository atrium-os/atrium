use folium_content::{interpret, ClipKind, Shared};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let doc = folium_doc::Doc::open(Box::new(std::fs::File::open(&a[1]).unwrap()), b"").unwrap();
    let page = doc.page(a[2].parse::<usize>().unwrap() - 1).unwrap();
    let c = doc.contents(&page).unwrap();
    let dl = interpret(&doc.syntax, &Shared::new(), &c, &page.resources, None, &|| false).unwrap();
    let mut kinds = std::collections::HashMap::new();
    for (i, cl) in dl.clips.iter().enumerate().take(12) {
        println!("{} parent {} {:?} bounds {:?}", i + 1, cl.parent, match &cl.kind { ClipKind::Rect(r) => format!("rect {:?}", r), ClipKind::Path { path, .. } => format!("path {} els {:?}", path.els.len(), &path.els[..path.els.len().min(6)]) }, cl.bounds);
    }
    for cl in &dl.clips { *kinds.entry(matches!(cl.kind, ClipKind::Rect(_))).or_insert(0) += 1; }
    println!("{kinds:?}");
}
