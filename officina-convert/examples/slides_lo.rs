//! Slides against LibreOffice, both ways (docs/spec/officina.md §9.2, O6c
//! gate): we write a deck using what the model has as `.pptx` and `.odp`,
//! LibreOffice opens each and saves it in the other format and in its own,
//! and we read LibreOffice's files back and check what came through.
//!
//! cargo run --example slides_lo [OUTDIR]
//!
//! Needs `soffice` on PATH. Prints one line per check and a summary.

#[allow(dead_code)]
#[path = "docx_lo.rs"]
mod common;
#[path = "../tests/support/slides_sample.rs"]
mod sample;

use std::path::PathBuf;

use common::{soffice, Checks};
use officina_convert::{odp, pptx};
use sample::{compare, sample};

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("slides-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["lo1", "lo2", "lo3", "lo4"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let mut deck = sample();
    let mut c = Checks { pass: 0, fail: 0 };
    let ours_pptx = dir.join("ours.pptx");
    let ours_odp = dir.join("ours.odp");
    std::fs::write(&ours_pptx, pptx::write(&mut deck, Vec::new()).unwrap()).unwrap();
    std::fs::write(&ours_odp, odp::write(&mut deck, Vec::new()).unwrap()).unwrap();

    // Ours read back by ourselves first.
    let (mut back, _) = pptx::read(&std::fs::read(&ours_pptx).unwrap()).unwrap();
    compare(&mut |w, ok, d| c.check(w, ok, || d), "our .pptx", &mut deck, &mut back);
    let (mut back, _) = odp::read(&std::fs::read(&ours_odp).unwrap()).unwrap();
    compare(&mut |w, ok, d| c.check(w, ok, || d), "our .odp", &mut deck, &mut back);

    for (input, filter, out, how) in [
        (&ours_pptx, "pptx:Impress MS PowerPoint 2007 XML", "lo1", "our .pptx through LibreOffice to .pptx"),
        (&ours_pptx, "odp:impress8", "lo2", "our .pptx through LibreOffice to .odp"),
        (&ours_odp, "odp:impress8", "lo3", "our .odp through LibreOffice to .odp"),
        (&ours_odp, "pptx:Impress MS PowerPoint 2007 XML", "lo4", "our .odp through LibreOffice to .pptx"),
    ] {
        let theirs = soffice(&dir, filter, input, &dir.join(out));
        let got = if filter.starts_with("pptx") { pptx::read(&theirs).map_err(|e| e.to_string()) } else { odp::read(&theirs).map_err(|e| e.to_string()) };
        match got {
            Ok((mut d, loaded)) => {
                compare(&mut |w, ok, d| c.check(w, ok, || d), how, &mut deck, &mut d);
                for l in loaded.report.human() {
                    println!("note ({how}) {l}");
                }
            }
            Err(e) => c.check(&format!("{how}: our reader takes LibreOffice's file"), false, || e),
        }
    }
    println!("slides via LibreOffice: {} of {} checks pass", c.pass, c.pass + c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}
