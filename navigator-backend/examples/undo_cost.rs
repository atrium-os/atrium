//! What an undo actually costs, measured on real recordings.
//!
//! The history bound has to be a number, and a number picked for roundness is
//! not protection — it is a value that makes a reviewer feel better. This
//! prints the distribution so the default can be derived from it.
//!
//!   cargo run --example undo_cost -- <recordings-dir>

use navigator_backend::document::Document;
use navigator_backend::{ingest, Effect, Limits};

fn undo_bytes(t: &navigator_backend::Transition) -> usize {
    t.effects.iter().map(|e| match e {
        Effect::Insert { html, .. } => html.len(),
        Effect::Attribute { from, to, .. } =>
            from.as_ref().map_or(0, String::len) + to.as_ref().map_or(0, String::len),
        _ => 0,
    }).sum()
}

fn main() {
    let dir = std::env::args().nth(1).expect("recordings directory");
    let mut sizes: Vec<usize> = vec![];
    let mut docs = 0;
    for e in std::fs::read_dir(&dir).expect("readable").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let Ok(base) = Document::accept(&r.document) else { continue };
        docs += 1;
        for t in &r.transitions {
            if !t.anchored { continue }
            if let Ok((_, undo)) = base.applied_with_undo(t) {
                sizes.push(undo_bytes(&undo));
            }
        }
    }
    sizes.sort_unstable();
    if sizes.is_empty() { println!("no transitions in {dir}"); return }
    let at = |q: f64| sizes[((sizes.len() as f64 - 1.0) * q) as usize];
    let total: usize = sizes.iter().sum();
    println!("{docs} documents, {} undos", sizes.len());
    println!("  median {:>8}", at(0.50));
    println!("  p90    {:>8}", at(0.90));
    println!("  p99    {:>8}", at(0.99));
    println!("  max    {:>8}", sizes[sizes.len() - 1]);
    println!("  total  {:>8}  (every undo of every transition held at once)", total);
}
