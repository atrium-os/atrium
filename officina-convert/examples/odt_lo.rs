//! `.odt` against LibreOffice: we write a document using everything the model
//! has, LibreOffice opens it and saves it as `.odt` itself, and we read
//! LibreOffice's file back and check what came through. A second pass takes
//! our `.docx` through LibreOffice to `.odt`, so our reader also meets
//! LibreOffice's rendering of a document it did not get from us as `.odt`.
//!
//! cargo run --example odt_lo [OUTDIR]
//!
//! Needs `soffice` on PATH. Prints one line per check and a summary.

#[allow(dead_code)]
#[path = "docx_lo.rs"]
mod common;

use std::path::PathBuf;

use common::{compare, sample, soffice, Checks};
use officina_convert::{docx, odt};

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("odt-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lo")).unwrap();
    std::fs::create_dir_all(dir.join("lo2")).unwrap();
    let d = sample();
    let mut c = Checks { pass: 0, fail: 0 };

    let ours = dir.join("ours.odt");
    std::fs::write(&ours, odt::write(&d, Vec::new()).unwrap()).unwrap();
    let theirs = soffice(&dir, "odt:writer8", &ours, &dir.join("lo"));
    let (back, loaded) = odt::read(&theirs).expect("our reader takes LibreOffice's .odt");
    println!("-- ours.odt through LibreOffice");
    compare(&mut c, &d, &back);
    for l in loaded.report.human() {
        println!("note {l}");
    }

    let ours = dir.join("ours.docx");
    std::fs::write(&ours, docx::write(&d, Vec::new()).unwrap()).unwrap();
    let theirs = soffice(&dir, "odt:writer8", &ours, &dir.join("lo2"));
    let (back, loaded) = odt::read(&theirs).expect("our reader takes LibreOffice's .odt");
    println!("-- ours.docx to .odt by LibreOffice");
    compare(&mut c, &d, &back);
    for l in loaded.report.human() {
        println!("note {l}");
    }

    println!("odt via LibreOffice: {} of {} checks pass", c.pass, c.pass + c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}
