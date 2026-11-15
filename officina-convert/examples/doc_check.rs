//! Reads `.doc` files (a folder or one file) and prints what came out:
//! paragraphs, pictures, notes; with `-x`, the WordprocessingML it was
//! translated to; with `-t`, the text.
//!
//!   cargo run --release --example doc_check -- PATH [PASSWORD] [-x] [-t] [-v]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let rest: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let path = std::path::Path::new(rest.first().map(|s| s.as_str()).unwrap_or("."));
    let pw = rest.get(1).map(|s| s.as_str());
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        std::fs::read_dir(path).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("doc"))).collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    for f in files {
        let Ok(data) = std::fs::read(&f) else { continue };
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if flag("-x") {
            match officina_convert::doc::to_docx(&data, pw) {
                Ok(pkg) => {
                    let z = officina_convert::zip::Archive::open(&pkg).unwrap();
                    for e in z.entries.clone() {
                        if e.name.ends_with(".xml") || e.name.ends_with(".rels") {
                            println!("== {}\n{}", e.name, String::from_utf8_lossy(&z.read(&e).unwrap()));
                        }
                    }
                }
                Err(e) => println!("FAIL {name}: {e}"),
            }
            continue;
        }
        match std::panic::catch_unwind(|| officina_convert::doc::read(&data, pw)) {
            Ok(Ok((d, l))) => {
                println!("ok   {name}: {} paragraphs, {} pictures, {} notes", l.paragraphs, l.pictures, d.notes.len());
                if flag("-v") {
                    for n in l.report.lines() {
                        println!("       {n}");
                    }
                }
                if flag("-t") {
                    println!("{}", d.plain_text());
                }
            }
            Ok(Err(e)) => println!("FAIL {name}: {e}"),
            Err(_) => println!("PANIC {name}"),
        }
    }
}
