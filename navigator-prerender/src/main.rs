//! Corpus driver: convert documents and report the numbers the profile needs.
//!
//! Usage: prerender <file-or-dir>...
//! The output is a measurement, not a rendering — see spec §11.4.

use navigator_prerender::{boa_impl::BoaEngine, convert};
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

fn collect(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() { collect(&e.path(), out); }
        }
    } else if p.extension().map(|e| e == "html" || e == "htm").unwrap_or(false) {
        out.push(p.to_path_buf());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() { eprintln!("usage: prerender <file-or-dir>..."); std::process::exit(2); }
    let mut files = vec![];
    for a in &args { collect(Path::new(a), &mut files); }
    files.sort();

    let (mut with_js, mut js_ok, mut js_fail, mut mutated) = (0usize, 0usize, 0usize, 0usize);
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut elems = vec![];

    for f in &files {
        let Ok(src) = fs::read_to_string(f) else { continue };
        let c = convert(&src, &mut BoaEngine);
        elems.push(c.elements_after);
        if c.scripts_total > 0 {
            with_js += 1;
            if c.scripts_failed == 0 { js_ok += 1 } else { js_fail += 1 }
            if c.script_mutations > 0 { mutated += 1 }
            for e in &c.errors {
                // bucket by the leading phrase so the report names the gap
                let key: String = e.split(':').take(2).collect::<Vec<_>>().join(":");
                *errors.entry(key.chars().take(90).collect()).or_default() += 1;
            }
        }
    }

    elems.sort();
    let pct = |q: f64| elems.get(((elems.len() as f64) * q) as usize).copied().unwrap_or(0);
    println!("documents            {}", files.len());
    println!("  with inline script {with_js}");
    println!("  scripts all ran    {js_ok}");
    println!("  some script failed {js_fail}");
    println!("  DOM actually changed by script {mutated}");
    if !elems.is_empty() {
        println!("elements  median={} p95={} max={}", pct(0.5), pct(0.95), elems.last().unwrap());
    }
    if !errors.is_empty() {
        println!("top script failures (the missing-API report):");
        let mut v: Vec<_> = errors.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.into_iter().take(12) { println!("  {n:5}  {k}"); }
    }
}
