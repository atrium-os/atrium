//! Every image of every recording, through the REAL path: `Navigatord` over
//! a `JailedHost` (a worker process per document; unconfined here, so this
//! measures the pipe and the decoder, not a jail's setup), the blob store as
//! the broker's capability, and `Request::Image` per address the recording
//! names.
//!
//!     cargo build --release --bin navigator-worker --example image_documents
//!     target/release/examples/image_documents target/release/navigator-worker BLOBS RECORDINGS
//!
//! Reports per document: images named, decoded, refused by cause, decoded
//! bytes against the 256 MiB budget, and wall time for all its images.

use navigator_backend::jailed::{JailedHost, WorkerConfig};
use navigator_backend::navigatord::{DocumentHost, Event, Navigatord, Request};
use navigator_backend::session::SessionLimits;
use navigator_backend::store::DirStore;
use std::collections::BTreeMap;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    assert!(a.len() == 3, "usage: image_documents WORKER BLOBS RECORDINGS");
    let (worker, blobs, recs) = (&a[0], &a[1], &a[2]);
    let mut names: Vec<_> = std::fs::read_dir(recs).unwrap().flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    names.sort();
    let mut causes: BTreeMap<String, usize> = BTreeMap::new();
    let (mut named, mut decoded, mut docs, mut opened) = (0usize, 0usize, 0usize, 0usize);
    let (mut per_doc_ms, mut per_img_ms, mut per_doc_bytes, mut per_doc_n) = (vec![], vec![], vec![], vec![]);
    let mut over_before = 0usize;
    for p in &names {
        docs += 1;
        let host = JailedHost::new(WorkerConfig::unconfined_for_testing(worker.clone()), SessionLimits::default());
        let mut n = Navigatord::unconfined_for_testing(host).with_blobs(Box::new(DirStore::new(blobs).unwrap()));
        let rec = std::fs::read(p).unwrap();
        let s = match n.handle(Request::OpenSession { recording: rec }).pop() {
            Some(Event::SessionOpened { session, .. }) => session,
            other => { *causes.entry(format!("open refused: {other:?}").chars().take(60).collect()).or_default() += 1; continue }
        };
        opened += 1;
        let images: Vec<String> = n.host().carried(s).unwrap().images().iter().cloned().collect();
        named += images.len();
        per_doc_n.push(images.len() as f64);
        let t = Instant::now();
        for a in &images {
            let ti = Instant::now();
            match n.handle(Request::Image { session: s, address: a.clone() }).pop() {
                Some(Event::Image { .. }) => { decoded += 1; per_img_ms.push(ti.elapsed().as_secs_f64() * 1e3) }
                Some(Event::Blocked { why, .. }) => {
                    let k = if why.contains("no object") { "not in the blob store (never fetched)".to_string() }
                            else { why.split(':').next().unwrap_or("").chars().take(48).collect::<String>()
                                   .split(|c: char| c.is_ascii_digit()).next().unwrap_or("").trim().to_string() };
                    *causes.entry(k).or_default() += 1;
                }
                other => panic!("{other:?}"),
            }
        }
        let over = causes.get("jpeg image needs").copied().unwrap_or(0) + causes.get("png image needs").copied().unwrap_or(0);
        if over > over_before {
            println!("  over budget: {} — {} of {} images refused, {:.1} MiB decoded",
                p.file_name().unwrap().to_string_lossy(), over - over_before, images.len(), n.decoded_bytes(s) as f64 / 1048576.0);
        }
        over_before = over;
        per_doc_ms.push(t.elapsed().as_secs_f64() * 1e3);
        per_doc_bytes.push(n.decoded_bytes(s) as f64);
        n.handle(Request::Close { session: s });
    }
    let q = |v: &Vec<f64>, p: f64| { let mut v = v.clone(); v.sort_by(|a, b| a.partial_cmp(b).unwrap()); if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * p).round() as usize] } };
    println!("{recs}");
    println!("documents {docs}, opened {opened}; images named {named}, decoded {decoded} ({:.1}%)", 100.0 * decoded as f64 / named.max(1) as f64);
    println!("refused by cause:");
    for (c, k) in &causes { println!("  {k:>5}  {c}") }
    println!("images per document: p50 {:.0} max {:.0}", q(&per_doc_n, 0.5), q(&per_doc_n, 1.0));
    println!("per image through the worker, ms: p50 {:.2} p95 {:.2} max {:.1}", q(&per_img_ms, 0.5), q(&per_img_ms, 0.95), q(&per_img_ms, 1.0));
    println!("all images of a document, ms: p50 {:.0} p95 {:.0} max {:.0}", q(&per_doc_ms, 0.5), q(&per_doc_ms, 0.95), q(&per_doc_ms, 1.0));
    println!("decoded bytes per document, MiB: p50 {:.1} p95 {:.1} max {:.1} (budget 256)",
        q(&per_doc_bytes, 0.5) / 1048576.0, q(&per_doc_bytes, 0.95) / 1048576.0, q(&per_doc_bytes, 1.0) / 1048576.0);
}
