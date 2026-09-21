use navigator_backend::{document::Document, ingest, Limits};
fn main() {
    let path = std::env::args().nth(1).expect("recording path");
    let bytes = std::fs::read(&path).expect("readable");
    let r = ingest(&bytes, &Limits::default()).expect("ingests");
    let doc = Document::parse(&r.document);
    let t = &r.transitions[0].trigger;
    println!("trigger: {t}");
    let mut cur = doc.dom.root();
    for step in t.split('>') {
        let (tag, nth) = step.rsplit_once(':').unwrap();
        let nth: usize = nth.parse().unwrap();
        let kids: Vec<_> = doc.dom.children_of(cur);
        let matching: Vec<_> = kids.iter().copied()
            .filter(|&c| doc.dom.tag(c).map(|x| x.eq_ignore_ascii_case(tag)).unwrap_or(false))
            .collect();
        println!("  at {:?}: {} children, {} matching <{tag}>, want index {nth}",
            doc.dom.tag(cur).unwrap_or("#root"), kids.len(), matching.len());
        match matching.get(nth) {
            Some(&c) => cur = c,
            None => {
                println!("  ✗ FAILS HERE: no <{tag}> at index {nth}");
                for k in &kids {
                    println!("      actual child: <{}>  text={:?}",
                        doc.dom.tag(*k).unwrap_or("#text/comment"),
                        doc.dom.text_content(*k).chars().take(40).collect::<String>());
                }
                return
            }
        }
    }
    println!("  ✓ resolved");
}
