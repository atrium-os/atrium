//! macros_lo: macros kept inert, checked against LibreOffice.
//!
//! 1. Our `.ods` with a Basic module → LibreOffice saves `.ods` → our reader: the
//!    module's code is still there.
//! 2. For each real `.xlsm` named on the command line: our reader keeps its VBA project,
//!    our `.xlsm` writer writes it back; LibreOffice opening the original and opening
//!    ours finds the same VBA modules (it shows them as Basic), so the project carried
//!    byte for byte is intact and bound.
//! Needs `soffice` on PATH. `KEEP=1` keeps the files.

use std::path::Path;
use std::process::Command;

use officina_grid::addr::CellAddr;
use officina_grid::macros::*;
use officina_grid::{Value, Workbook};

const CODE: &str = "Sub OfficinaKeepsThis()\n  Dim x As Integer\n  x = 42\nEnd Sub";

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.set_value(CellAddr::new(0, 0, 0), Value::Num(1.0));
    let x = |p: &str, t: String| MacroPart { path: p.into(), media_type: "text/xml".into(), data: t.into_bytes().into() };
    wb.set_macros(Macros {
        family: MacroFamily::Odf,
        parts: vec![
            x("Basic/script-lc.xml", r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE library:libraries PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "libraries.dtd"><library:libraries xmlns:library="http://openoffice.org/2000/library" xmlns:xlink="http://www.w3.org/1999/xlink"><library:library library:name="Standard" library:link="false"/></library:libraries>"#.into()),
            x("Basic/Standard/script-lb.xml", r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE library:library PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "library.dtd"><library:library xmlns:library="http://openoffice.org/2000/library" library:name="Standard" library:readonly="false" library:passwordprotected="false"><library:element library:name="Module1"/></library:library>"#.into()),
            x("Basic/Standard/Module1.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE script:module PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "module.dtd"><script:module xmlns:script="http://openoffice.org/2000/script" script:name="Module1" script:language="StarBasic" script:moduleType="normal">{CODE}</script:module>"#)),
        ],
        workbook_code: None,
        sheet_codes: vec![],
    })
    .unwrap();
    wb
}

fn soffice(dir: &Path, filter: &str, input: &Path, out: &Path) -> Vec<u8> {
    let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", filter, "--outdir"]).arg(out).arg(input).env("HOME", dir).output().expect("soffice");
    let ext = filter.split(':').next().unwrap();
    let p = out.join(input.file_stem().unwrap()).with_extension(ext);
    std::fs::read(&p).unwrap_or_else(|e| panic!("soffice gave no {}: {e}\n{}", p.display(), String::from_utf8_lossy(&st.stderr)))
}

fn has_code(wb: &Workbook) -> bool {
    wb.macros().is_some_and(|m| m.parts.iter().any(|p| String::from_utf8_lossy(&p.data).contains("OfficinaKeepsThis")))
}

fn main() {
    let dir = std::env::temp_dir().join(format!("macros-lo-{}", std::process::id()));
    let (a, b, c) = (dir.join("ours"), dir.join("lo"), dir.join("lo2"));
    for d in [&a, &b, &c] {
        std::fs::create_dir_all(d).unwrap();
    }
    let wb = book();
    let ods = a.join("macros.ods");
    std::fs::write(&ods, officina_convert::ods::write(&wb, Vec::new()).unwrap()).unwrap();
    let (back, loaded) = officina_convert::ods::read(&soffice(&dir, "ods:calc8", &ods, &b)).unwrap();
    println!("1. ods → LibreOffice → ods: code kept {}; report {:?}", has_code(&back), loaded.report.human());

    // 2. Real .xlsm files (paths as arguments): LibreOffice's reading of the original
    // and of our re-written copy should find the same VBA modules.
    for (k, path) in std::env::args().skip(1).enumerate() {
        let orig = std::fs::read(&path).unwrap();
        let (x, loaded) = officina_convert::xlsx::read(&orig).unwrap();
        let m = x.macros();
        println!("2. {path}: VBA kept {} ({} parts, {} bytes, code names {:?} {:?}); report {:?}", m.is_some(), m.map_or(0, |m| m.parts.len()), m.map_or(0, |m| m.total_bytes()), m.and_then(|m| m.workbook_code.clone()), m.map(|m| m.sheet_codes.clone()), loaded.report.human());
        let (d1, d2) = (dir.join(format!("x{k}a")), dir.join(format!("x{k}b")));
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        let src = d1.join("orig.xlsm");
        std::fs::write(&src, &orig).unwrap();
        let ours = d2.join("orig.xlsm");
        std::fs::write(&ours, officina_convert::xlsx::write_xlsm(&x, Vec::new()).unwrap()).unwrap();
        let lo_orig = officina_convert::ods::read(&soffice(&dir, "ods:calc8", &src, &d1)).unwrap().0;
        let lo_ours = officina_convert::ods::read(&soffice(&dir, "ods:calc8", &ours, &d2)).unwrap().0;
        let modules = |wb: &Workbook| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = wb.macros().map_or(Vec::new(), |m| m.parts.iter().filter(|p| p.path.starts_with("Basic/") && !p.path.ends_with("script-lb.xml") && !p.path.ends_with("script-lc.xml")).map(|p| (p.path.to_string(), String::from_utf8_lossy(&p.data).into_owned())).collect());
            v.sort();
            v
        };
        let (a1, a2) = (modules(&lo_orig), modules(&lo_ours));
        println!("   LibreOffice finds {} VBA modules in the original, {} in ours; same code: {}", a1.len(), a2.len(), !a1.is_empty() && a1 == a2);
    }
    if std::env::var_os("KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("kept {}", dir.display());
    }
}
