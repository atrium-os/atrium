//! nsg-conformance [--bless] [--verbose]   (fixtures in navigator-render/conformance/)
//!
//! Prints the M2 conformance number: rows exercised and rows matched, of 64.
//! `--bless` writes goldens for CLEAN fixtures only — and a golden is only
//! trustworthy once someone has LOOKED at it (nsg-raster), so bless, review,
//! then commit.

use navigator_render::{conformance, fontset::FontSet};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let bless = a.iter().any(|x| x == "--bless");
    let verbose = a.iter().any(|x| x == "--verbose");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance");
    let fonts = FontSet::load().expect("pinned font set");
    let rows = conformance::run(&dir, &fonts, bless);
    for r in &rows {
        let state = if r.matched { "MATCHED  " } else if r.exercised { "exercised" } else if r.fixtures.is_empty() { "—        " } else { "partial  " };
        println!("{:>2} {state} {:<26} {}", r.id, r.name, if r.fixtures.is_empty() { String::new() } else { r.fixtures.join(" ") });
        if verbose || (!r.fixtures.is_empty() && !r.matched) {
            if !r.missing.is_empty() && !r.fixtures.is_empty() { println!("     missing coverage: {}", r.missing.join(", ")) }
            for p in r.problems.iter().take(6) { println!("     {p}") }
        }
    }
    let ex = rows.iter().filter(|r| r.exercised).count();
    let m = rows.iter().filter(|r| r.matched).count();
    println!("\nCONFORMANCE: exercised {ex}/64, matched {m}/64");
}
