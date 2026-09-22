//! What a navigation costs the WORKER, in-process — no pipe, no broker.
//!
//! `jailed_corpus` reports navigate at the broker, which is this plus a pipe
//! round trip and two thread wakeups. Run both on one machine and the
//! difference is the transport; neither number alone says where time goes.
//!
//!   nav_cost <recordings-dir>

use navigator_backend::document::Document;
use navigator_backend::reverse::History;
use navigator_backend::{ingest, Limits};
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("recordings directory");
    let (mut go_ms, mut back_ms) = (vec![], vec![]);
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("readable").flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    for p in &files {
        let bytes = std::fs::read(p).expect("readable");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let Ok(doc) = Document::accept(&r.document) else { continue };
        let mut h = History::new(doc);
        for t in r.transitions.iter().filter(|t| t.anchored) {
            let s = Instant::now();
            if h.go(t).is_err() { continue }
            go_ms.push(s.elapsed().as_secs_f64() * 1e3);
            let s = Instant::now();
            h.back().expect("back after a successful go");
            back_ms.push(s.elapsed().as_secs_f64() * 1e3);
        }
    }
    for (name, v) in [("go", &mut go_ms), ("back", &mut back_ms)] {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() { println!("{name}_ms n=0"); continue }
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        println!("{name}_ms n={} p50={:.2} p90={:.2} p99={:.2} max={:.2} mean={:.2}",
                 v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1], v.iter().sum::<f64>() / v.len() as f64);
    }
}
