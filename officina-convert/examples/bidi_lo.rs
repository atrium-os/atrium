//! Paragraph direction against LibreOffice, both ways and across formats:
//! we write .docx and .odt with a right-to-left and a left-to-right
//! paragraph, LibreOffice saves each as .docx and as .odt itself, and we
//! read its files back and check each paragraph's direction.
//!
//! cargo run --example bidi_lo [OUTDIR]

use std::path::PathBuf;
use std::process::Command;

use officina_convert::{docx, odt};
use officina_text::model::TextDoc;

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("bidi-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut d = TextDoc::new();
    let a = d.push_para("שלום עולם (Atrium).", 0, vec![]);
    let b = d.push_para("Left to right: שלום.", 0, vec![]);
    d.blocks.update(a, |x| x.as_para_mut().unwrap().props.rtl = Some(true));
    d.blocks.update(b, |x| x.as_para_mut().unwrap().props.rtl = Some(false));
    let want = [Some(true), Some(false)];
    let (mut pass, mut fail) = (0, 0);
    for (ours, bytes) in [("docx", docx::write(&d, Vec::new()).unwrap()), ("odt", odt::write(&d, Vec::new()).unwrap())] {
        let src = dir.join(format!("ours-{ours}.{ours}"));
        std::fs::write(&src, &bytes).unwrap();
        for (theirs, filter) in [("docx", "docx:MS Word 2007 XML"), ("odt", "odt"), ("pdf", "pdf")] {
            let out = dir.join(format!("lo-{ours}"));
            std::fs::create_dir_all(&out).unwrap();
            let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", filter, "--outdir"]).arg(&out).arg(&src).env("HOME", &dir).output().expect("soffice");
            let path = out.join(src.with_extension(theirs).file_name().unwrap());
            let Ok(b) = std::fs::read(&path) else {
                eprintln!("LibreOffice did not convert {}:\n{}", src.display(), String::from_utf8_lossy(&st.stderr));
                std::process::exit(1)
            };
            if theirs == "pdf" {
                continue;
            }
            let r = if theirs == "docx" { docx::read(&b).expect("LibreOffice's .docx").0 } else { odt::read(&b).expect("LibreOffice's .odt").0 };
            // Effective direction: the paragraph's own, else its style's.
            let got: Vec<Option<bool>> = (0..r.blocks.len()).filter_map(|i| r.para(i)).filter(|p| !p.text.is_empty()).map(|p| r.styles.para_format(p.style, &p.props).0.rtl.or(Some(false))).collect();
            let ok = got == want;
            println!("{} ours .{ours} -> LibreOffice .{theirs}: {got:?}", if ok { "PASS" } else { "FAIL" });
            if ok { pass += 1 } else { fail += 1 }
        }
    }
    println!("{pass}/{} passed", pass + fail);
    if fail > 0 {
        std::process::exit(1);
    }
}
