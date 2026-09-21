use navigator_backend::{ingest, Limits};
use navigator_dom::{parse, Dom};
fn shape(d: &Dom) -> (usize, usize) { (d.element_count(), d.max_depth()) }
fn main() {
    let path = std::env::args().nth(1).expect("recording path");
    let bytes = std::fs::read(&path).expect("readable");
    let r = ingest(&bytes, &Limits::default()).expect("ingests");
    let once = parse(&r.document);
    let s1 = once.serialize();
    let twice = parse(&s1);
    let s2 = twice.serialize();
    println!("document bytes        {}", r.document.len());
    println!("parse→serialize bytes {}", s1.len());
    println!("second round bytes    {}", s2.len());
    println!("shape once  {:?}", shape(&once));
    println!("shape twice {:?}", shape(&twice));
    println!("serialize is idempotent: {}", s1 == s2);
    // ★ THE ONE THAT MATTERS. Converter-output-vs-reparse can differ for a
    // reason the converter is entitled to (it holds a DOM the parser would
    // never build from those bytes). round1-vs-round2 cannot: both sides came
    // out of the same parser, so any difference is the parser and the
    // serializer disagreeing about the same document.
    if s1 != s2 {
        let a = s1.as_bytes(); let b = s2.as_bytes();
        let i = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]).unwrap_or(a.len().min(b.len()));
        println!("\nround 1 vs round 2 diverge at byte {i}:");
        let lo = i.saturating_sub(90);
        println!("  once:  …{}…", String::from_utf8_lossy(&a[lo..(i+90).min(a.len())]));
        println!("  twice: …{}…", String::from_utf8_lossy(&b[lo..(i+90).min(b.len())]));
    }
    if r.document != s1 {
        // find first divergence between what the converter wrote and what a
        // reparse of it produces
        let a = r.document.as_bytes(); let b = s1.as_bytes();
        let i = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]).unwrap_or(a.len().min(b.len()));
        println!("\nconverter output vs reparse diverge at byte {i}:");
        let lo = i.saturating_sub(90);
        println!("  recorded: …{}…", String::from_utf8_lossy(&a[lo..(i+90).min(a.len())]));
        println!("  reparsed: …{}…", String::from_utf8_lossy(&b[lo..(i+90).min(b.len())]));
    }
}
