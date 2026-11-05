//! ★ The positive control for the link bound (threat model §2.10): every
//! `<a>`/`<area>` href in every corpus document, and every href a recorded
//! transition sets or inserts, must be FOUND by the broker's byte search of
//! the recording. A miss is a real link the broker would refuse.
//!
//!     cargo run --release --example link_bound_corpus -- RECORDINGS...

use navigator_backend::carried::Carried;
use navigator_backend::links::{document_carries, recording_spellings};
use navigator_backend::{ingest, Effect, Limits};

fn main() {
    for dir in std::env::args().skip(1) {
        let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        paths.sort();
        let (mut docs, mut hrefs, mut found, mut precise, mut effect_hrefs, mut effect_found) = (0, 0, 0, 0, 0, 0);
        let mut misses = vec![];
        for p in &paths {
            let bytes = std::fs::read(p).unwrap();
            let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
            docs += 1;
            let c = Carried::new(&bytes);
            let dom = navigator_dom::parse(&r.document);
            for t in ["a", "area"] {
                for h in dom.by_tag(t) {
                    let Some(href) = dom.attr(h, "href") else { continue };
                    hrefs += 1;
                    if recording_spellings(href).iter().any(|n| c.contains(n)) { found += 1 }
                    else if misses.len() < 10 { misses.push(format!("{}: {href:?}", p.file_name().unwrap().to_string_lossy())) }
                    if document_carries(&dom, href) { precise += 1 }
                }
            }
            for tr in &r.transitions {
                for e in &tr.effects {
                    let mut hs: Vec<String> = vec![];
                    match e {
                        Effect::Attribute { name, to: Some(v), .. } if name == "href" => hs.push(v.clone()),
                        Effect::Insert { html, .. } => {
                            let f = navigator_dom::parse_fragment(html);
                            for t in ["a", "area"] { for h in f.by_tag_anywhere(t) { if let Some(v) = f.attr(h, "href") { hs.push(v.to_string()) } } }
                        }
                        _ => {}
                    }
                    for href in hs {
                        effect_hrefs += 1;
                        if recording_spellings(&href).iter().any(|n| c.contains(n)) { effect_found += 1 }
                        else if misses.len() < 10 { misses.push(format!("{} (transition): {href:?}", p.file_name().unwrap().to_string_lossy())) }
                    }
                }
            }
        }
        // The cost the broker pays per click: a miss scans the whole
        // recording twice (both spellings), the worst case.
        let (mut worst_ms, mut worst_name) = (0f64, String::new());
        for p in &paths {
            let bytes = std::fs::read(p).unwrap();
            let c = Carried::new(&bytes);
            let t = std::time::Instant::now();
            let hit = recording_spellings("https://absent.test/never").iter().any(|n| c.contains(n));
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert!(!hit);
            if ms > worst_ms { worst_ms = ms; worst_name = format!("{} ({} bytes)", p.file_name().unwrap().to_string_lossy(), bytes.len()) }
        }
        println!("{dir}: worst-case bound (a miss) {worst_ms:.1} ms on {worst_name}");
        println!("{dir}: {docs} documents; document hrefs {hrefs}, found by the bound {found}, by the precise check {precise}; transition hrefs {effect_hrefs}, found {effect_found}");
        for m in misses { println!("  MISS {m}") }
    }
}
