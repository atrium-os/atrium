//! float_measure <dir>… [--ext readable|any]: floats beside content on real
//! documents (profile §7a C). One TSV line per document:
//! `stem  compiled  alone  rows_cut  nodes_moved  fit_layouts  fit_ms  normalize_and_tables_ms`
//! - `compiled`: floats with content after them, compiled to a flex row;
//! - `alone`: floats with nothing after them (dropped, as before);
//! - `rows_cut`, `nodes_moved`, `fit_layouts`, `fit_ms`: `fit_floats`, which
//!   moves content starting below a float back out of its row.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ext = args.iter().position(|a| a == "--ext").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "readable".into());
    let fonts = navigator_render::fontset::FontSet::load().expect("fonts");
    let env = navigator_style::cascade::Env::default();
    for dir in args.iter().filter(|a| !a.starts_with("--") && **a != ext) {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("dir").flatten().map(|e| e.path())
            .filter(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string();
                          if ext == "any" { n.ends_with(".json") } else { n.ends_with(&format!(".{ext}.json")) } }).collect();
        files.sort();
        for p in files {
            let json = std::fs::read_to_string(&p).unwrap();
            let stem = p.file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
            let Ok(r) = navigator_normalize::recording::parse(&json) else { println!("{stem}\tunparsed"); continue };
            let t0 = std::time::Instant::now();
            let (doc, report) = navigator_normalize::normalize(&r.document, &r.inputs);
            // As `normalize_and_measure` does: tables measured, then floats fitted.
            let (doc, _) = navigator_render::html::premeasure_tables(&doc, &fonts, &env);
            let t1 = std::time::Instant::now();
            let (_, fit) = navigator_normalize::fit_floats(&doc, &fonts, &env);
            let fit_ms = t1.elapsed().as_secs_f64() * 1e3;
            let n = |k: &str| report.dropped.get(k).copied().unwrap_or(0);
            println!("{stem}\t{}\t{}\t{}\t{}\t{}\t{fit_ms:.1}\t{:.1}", n("float beside content compiled to a flex row"),
                     n("property `float` (no content before the next float or clear; dropped)"), fit.cut, fit.moved, fit.layouts, (t1 - t0).as_secs_f64() * 1e3); // normalize + table pre-measure
        }
    }
}
