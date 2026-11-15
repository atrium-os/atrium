//! Tables of contents, cross-references and tab stops against LibreOffice,
//! both ways and across formats: we write .docx and .odt, LibreOffice opens
//! each and saves it as .docx and as .odt itself, and we read its files back
//! and check that the contents (levels, entries, the headings their page
//! numbers point at), the references and the tab stops came through.
//!
//! cargo run --example toc_lo [OUTDIR]

use std::path::PathBuf;
use std::process::Command;

use officina_convert::{docx, odt};
use officina_text::fields;

#[allow(dead_code, unused_imports)]
#[path = "../tests/fields.rs"]
mod f;

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("toc-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lo")).unwrap();
    let d = f::toc_doc();
    let want = f::shape(&d);
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
            let mut r = if theirs == "docx" { docx::read(&b).expect("our reader takes LibreOffice's .docx").0 } else { odt::read(&b).expect("our reader takes LibreOffice's .odt").0 };
            let got = f::shape(&r);
            let route = format!("{ours} → LibreOffice → {theirs}");
            let mut check = |what: &str, ok: bool, detail: String| {
                if ok {
                    pass += 1;
                    println!("ok   {route}: {what}");
                } else {
                    fail += 1;
                    println!("FAIL {route}: {what}: {detail}");
                }
            };
            check("contents entries, levels, targets", got.0 == want.0, format!("{:?}\n     want {:?}", got.0, want.0));
            check("cross-references", got.1 == want.1, format!("{:?}\n     want {:?}", got.1, want.1));
            check("tab stops", got.2 == want.2, format!("{:?}\n     want {:?}", got.2, want.2));
            check("Update finds the contents current", fields::toc_ops(&mut r, 0, 3).is_empty(), String::new());
        }
    }
    println!("contents and references via LibreOffice: {pass} of {} checks pass", pass + fail);
    if fail > 0 {
        std::process::exit(1);
    }
}
