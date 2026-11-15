//! Sections and columns against LibreOffice, both ways and across formats:
//! we write .docx and .odt, LibreOffice opens each and saves it as .docx and
//! as .odt itself, and we read its files back and check each section's first
//! paragraph, how it starts, its columns, gap and line between.
//!
//! cargo run --example sections_lo [OUTDIR]

use std::path::PathBuf;
use std::process::Command;

use officina_convert::{docx, odt};

#[allow(dead_code)]
#[path = "../tests/sections.rs"]
mod s;

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("sections-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = s::sections_doc();
    let want = s::shape(&d);
    let (mut pass, mut fail) = (0, 0);
    for (ours, bytes) in [("docx", docx::write(&d, Vec::new()).unwrap()), ("odt", odt::write(&d, Vec::new()).unwrap())] {
        let src = dir.join(format!("ours-{ours}.{ours}"));
        std::fs::write(&src, &bytes).unwrap();
        for (theirs, filter) in [("docx", "docx:MS Word 2007 XML"), ("odt", "odt")] {
            let out = dir.join(format!("lo-{ours}"));
            std::fs::create_dir_all(&out).unwrap();
            let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", filter, "--outdir"]).arg(&out).arg(&src).env("HOME", &dir).output().expect("soffice");
            let path = out.join(src.with_extension(theirs).file_name().unwrap());
            let Ok(b) = std::fs::read(&path) else {
                eprintln!("LibreOffice did not convert {}:\n{}", src.display(), String::from_utf8_lossy(&st.stderr));
                std::process::exit(1)
            };
            let r = if theirs == "docx" { docx::read(&b).expect("our reader takes LibreOffice's .docx").0 } else { odt::read(&b).expect("our reader takes LibreOffice's .odt").0 };
            let got = s::shape(&r);
            let route = format!("{ours} → LibreOffice → {theirs}");
            if got == want {
                pass += 1;
                println!("ok   {route}: sections");
            } else {
                fail += 1;
                println!("FAIL {route}: sections: {got:?}\n     want {want:?}");
            }
            let texts = |d: &officina_text::model::TextDoc| d.blocks.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).filter(|t| !t.is_empty()).collect::<Vec<_>>();
            if texts(&r) == texts(&d) {
                pass += 1;
                println!("ok   {route}: text");
            } else {
                fail += 1;
                println!("FAIL {route}: text: {:?}", texts(&r));
            }
        }
    }
    println!("sections via LibreOffice: {pass} of {} checks pass", pass + fail);
    if fail > 0 {
        std::process::exit(1);
    }
}
