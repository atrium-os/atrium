//! The letters gate with LibreOffice (docs/spec/officina.md §16.170).
//! `write DIR`: `letters.docx` and `letters.odt` (each run a look).
//! `read PATH...`: each word's capitals, hidden, double strike, spacing,
//! width and effects, one line per word, then the paragraphs' text.
//!
//! ```sh
//! cargo run --example letters_lo -- write /tmp/l
//! soffice --headless --convert-to odt --outdir /tmp/l/lo /tmp/l/letters.docx
//! soffice --headless --convert-to docx --outdir /tmp/l/lo /tmp/l/letters.odt
//! cargo run --example letters_lo -- read /tmp/l/letters.docx /tmp/l/lo/letters.odt /tmp/l/lo/letters.docx
//! ```
#[path = "../tests/letters.rs"]
#[allow(dead_code)]
mod letters;

use officina_convert::{docx, odt};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("write") {
        let dir = std::path::Path::new(&args[2]);
        std::fs::create_dir_all(dir).unwrap();
        let d = letters::letters_doc();
        std::fs::write(dir.join("letters.docx"), docx::write(&d, Vec::new()).unwrap()).unwrap();
        std::fs::write(dir.join("letters.odt"), odt::write(&d, Vec::new()).unwrap()).unwrap();
        return;
    }
    for p in &args[2..] {
        let b = std::fs::read(p).unwrap();
        let d = if p.ends_with(".odt") { odt::read(&b).unwrap().0 } else { docx::read(&b).unwrap().0 };
        println!("== {p}");
        for k in 0..d.blocks.len() {
            let q = d.para(k).unwrap();
            for (s, e, a) in q.spans() {
                let t = &q.text[s as usize..e as usize];
                if t.starts_with("word") {
                    let a = d.attrs.get(a);
                    println!("{} caps={:?} hidden={:?} dstrike={:?} spacing={:?} scale={:?} effects={:?}", t.trim(), a.caps, a.hidden, a.dstrike, a.spacing, a.scale, a.effects);
                }
            }
            println!("para {k}: {:?} break_before={:?}", q.text.chars().filter(|c| !c.is_ascii_digit()).collect::<String>().replace("word ", ""), q.props.page_break_before);
        }
    }
}
