//! Reads every `.xls` in a folder (or one file) and prints what came out:
//! cells, formulas, formula results that differ from the file's, notes.
//!
//!   cargo run --release --example xls_check -- DIR [PASSWORD] [-v]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v");
    let args: Vec<&String> = args.iter().filter(|a| *a != "-v").collect();
    let path = std::path::Path::new(args.first().map(|s| s.as_str()).unwrap_or("."));
    let pw = args.get(1).map(|s| s.as_str());
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        std::fs::read_dir(path).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xls"))).collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    let (mut ok, mut failed, mut differ) = (0, 0, 0);
    for f in files {
        let data = std::fs::read(&f).unwrap();
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let r = std::panic::catch_unwind(|| officina_convert::xls::read(&data, pw));
        match r {
            Ok(Ok((wb, l))) => {
                ok += 1;
                if l.differ > 0 {
                    differ += 1;
                }
                println!("ok   {name}: {} sheets, {} cells, {} formulas, {} differ", wb.sheet_count(), l.cells, l.formulas, l.differ);
                if verbose {
                    for n in l.report.lines() {
                        println!("       {n}");
                        // The formulas behind differing results.
                        if n.contains("differ from the file") {
                            for ex in n.rsplit('\t').next().unwrap_or("").split(", ") {
                                let Some((cell, _)) = ex.split_once(':') else { continue };
                                if let Some((r, c)) = officina_grid::addr::parse_a1(cell) {
                                    for s in 0..wb.sheet_count() as u16 {
                                        if let Some(t) = wb.formula_text(officina_grid::addr::CellAddr::new(s, r, c)) {
                                            println!("         sheet {s} {cell}: {t}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                failed += 1;
                println!("FAIL {name}: {e}");
            }
            Err(_) => {
                failed += 1;
                println!("PANIC {name}");
            }
        }
    }
    println!("{ok} read, {failed} failed, {differ} with differing results");
}
