//! border_measure <dir>… [--ext readable|any]: collapsed table borders on
//! real documents (profile §7a C). One TSV line per document:
//! `stem  tables_collapsed  spanning_cells  approx_width_tables  group_borders_ignored`
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ext = args.iter().position(|a| a == "--ext").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "readable".into());
    for dir in args.iter().filter(|a| !a.starts_with("--") && **a != ext) {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("dir").flatten().map(|e| e.path())
            .filter(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string();
                          if ext == "any" { n.ends_with(".json") } else { n.ends_with(&format!(".{ext}.json")) } }).collect();
        files.sort();
        for p in files {
            let json = std::fs::read_to_string(&p).unwrap();
            let stem = p.file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
            let Ok(r) = navigator_normalize::recording::parse(&json) else { println!("{stem}\tunparsed"); continue };
            let (_, report) = navigator_normalize::normalize(&r.document, &r.inputs);
            let n = |k: &str| report.dropped.get(k).copied().unwrap_or(0);
            println!("{stem}\t{}\t{}\t{}\t{}", n("collapsed table borders compiled to separated ones (CSS 2.1 §17.6.2)"),
                     n("collapsed border of a spanning cell: one winner for the whole side"),
                     n("collapsed table with a border width read without layout (em/pt/unknown)"),
                     n("collapsed border on a column or row group (not in the conflict)"));
        }
    }
}
