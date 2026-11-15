//! One `.ppt` through our reader: `ppt_check PATH [PASSWORD] [-x] [-t]`.
//! `-x` prints the PresentationML parts it turns into, `-t` each slide's text.

use officina_slides::model::{Shape, ShapeKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let pw = args.get(1).filter(|a| !a.starts_with('-')).map(String::as_str);
    let data = std::fs::read(path).expect("read");
    let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
    if args.iter().any(|a| a == "-x") {
        match officina_convert::ppt::to_pptx_parts(&data, pw) {
            Ok(parts) => {
                for (n, b) in parts {
                    if n.ends_with(".xml") || n.ends_with(".rels") {
                        println!("== {n}\n{}", String::from_utf8_lossy(&b));
                    } else {
                        println!("== {n} ({} bytes)", b.len());
                    }
                }
            }
            Err(e) => println!("FAIL {name}: {e}"),
        }
    }
    match officina_convert::ppt::read(&data, pw) {
        Ok((mut deck, loaded)) => {
            println!("ok   {name}: {} slides, {} shapes, {} pictures", deck.slide_count(), loaded.shapes, loaded.pictures);
            if args.iter().any(|a| a == "-t") {
                for i in 0..deck.slide_count() {
                    let Ok(s) = deck.slide(i) else { continue };
                    let mut t = Vec::new();
                    fn walk(s: &Shape, t: &mut Vec<String>) {
                        if let Some(tx) = s.text() {
                            t.push(tx.text().replace('\n', " / "));
                        }
                        if let ShapeKind::Group { shapes, .. } = &s.kind {
                            for k in shapes {
                                walk(k, t);
                            }
                        }
                    }
                    for sh in &s.shapes {
                        walk(sh, &mut t);
                    }
                    println!("-- slide {}: {}", i + 1, t.join(" | "));
                    if let Some(n) = &s.notes {
                        println!("   notes: {}", n.text().replace('\n', " / "));
                    }
                }
            }
            for l in loaded.report.human() {
                println!("note {l}");
            }
        }
        Err(e) => println!("FAIL {name}: {e}"),
    }
}
