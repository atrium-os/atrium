//! lo_check: `.xlsx` against LibreOffice, an independent implementation.
//!
//!   cargo run --release --example lo_check
//!
//! 1. Builds officina-grid's conformance workbook (the fixture and its 152 formulas,
//!    each with the result Excel shows), writes it with our `.xlsx` writer.
//! 2. LibreOffice opens our file and saves it as `.xlsx` again (`soffice --headless
//!    --convert-to xlsx`), recalculating as it loads (our file names no Excel version).
//!    That it opens at all and keeps every value is the "reopens without repair" check.
//! 3. Our reader opens LibreOffice's file: a third-party writer's styles, shared strings
//!    and formulas; our engine recalculates, and every result is compared with what
//!    LibreOffice computed and with Excel's documented result.
//!
//! Needs `soffice` on PATH. Prints a table of the disagreements and a summary line.

use std::process::Command;

use officina_grid::{CellAddr, Value, Workbook};

const COL: u32 = 25; // Z, as in tests/conformance.rs

/// The conformance cases, read from officina-grid's test (one source of truth).
fn cases() -> Vec<(String, String)> {
    let src = include_str!("../../officina-grid/tests/conformance.rs");
    let unq = |s: &str| s.replace("\\\"", "\"").replace("\\\\", "\\");
    let mut out = Vec::new();
    for line in src.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("(\"") else { continue };
        // ("formula", "result"),
        let Some(mid) = find_sep(rest) else { continue };
        let f = &rest[..mid];
        let tail = &rest[mid + 4..];
        let Some(end) = tail.rfind("\")") else { continue };
        out.push((unq(f), unq(&tail[..end])));
    }
    out
}

/// The `", "` between the two literals, skipping escaped quotes.
fn find_sep(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if &b[i..i + 4] == b"\", \"" {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn fixture() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("Other sheet").unwrap();
    let set = |wb: &mut Workbook, a: &str, s: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    };
    for (i, v) in ["1", "2", "3", "4", "5"].iter().enumerate() {
        set(&mut wb, &format!("A{}", i + 1), v);
    }
    for (i, v) in ["apple", "Banana", "cherry", "apple pie", "date"].iter().enumerate() {
        set(&mut wb, &format!("B{}", i + 1), v);
    }
    set(&mut wb, "D1", "10");
    set(&mut wb, "D2", "x");
    set(&mut wb, "D4", "20");
    set(&mut wb, "E1", "=1/0");
    wb.set_input(CellAddr::new(1, 0, 0), "7").unwrap();
    wb
}

fn main() {
    let cases = cases();
    assert!(cases.len() >= 150, "read {} cases", cases.len());
    let mut wb = fixture();
    for (i, (f, _)) in cases.iter().enumerate() {
        wb.set_input(CellAddr::new(0, 100 + i as u32, COL), f).unwrap();
    }
    wb.recalc();
    let dir = std::env::temp_dir().join(format!("lo-check-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let ours = a.join("conformance.xlsx");
    std::fs::write(&ours, officina_convert::xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
    let st = Command::new("soffice")
        .args(["--headless", "--norestore", "--convert-to", "xlsx:Calc MS Excel 2007 XML", "--outdir"])
        .arg(&b)
        .arg(&ours)
        .env("HOME", &dir)
        .output()
        .expect("soffice");
    let lo_file = b.join("conformance.xlsx");
    if !lo_file.exists() {
        eprintln!("LibreOffice did not convert our file:\n{}\n{}", String::from_utf8_lossy(&st.stdout), String::from_utf8_lossy(&st.stderr));
        std::process::exit(1);
    }
    let data = std::fs::read(&lo_file).unwrap();
    let (back, loaded) = officina_convert::xlsx::read(&data).unwrap_or_else(|e| panic!("our reader on LibreOffice's file: {e}"));

    // The LibreOffice file's cached results, before our recalculation overwrote them:
    // read the values again with formulas left out, from a copy where <f> is removed.
    let lo_values = cached_values(&data);
    let mut agree_lo = 0;
    let mut lo_is_excel = 0;
    let mut rows = Vec::new();
    for (i, (f, excel)) in cases.iter().enumerate() {
        let r = 100 + i as u32;
        let ours = back.get(CellAddr::new(0, r, COL)).display();
        let lo = lo_values.get(&(r, COL)).cloned().unwrap_or_default();
        let same = ours == lo || close(&ours, &lo);
        agree_lo += same as u32;
        lo_is_excel += (lo == *excel || close(&lo, excel)) as u32;
        if !same {
            rows.push(format!("{f:<45} ours {ours:<22} LibreOffice {lo:<22} Excel {excel}"));
        }
        // Our own result must still be Excel's (the conformance gate, after a round trip
        // through LibreOffice's file).
        if ours != *excel {
            rows.push(format!("  !! after the round trip {f} shows {ours}, Excel {excel}"));
        }
    }
    // The fixture's values survived LibreOffice.
    let mut kept = true;
    for r in 0..5 {
        for c in [0u32, 1] {
            let a = CellAddr::new(0, r, c);
            kept &= back.get(a) == wb.get(a);
        }
    }
    println!("cases where our result differs from LibreOffice's:");
    for r in &rows {
        println!("  {r}");
    }
    println!("fidelity report on LibreOffice's file:");
    for l in loaded.report.human() {
        println!("  {l}");
    }
    println!(
        "summary: {} cases; ours = LibreOffice on {agree_lo}; LibreOffice = Excel on {lo_is_excel}; reader differential {} of {} formulas; fixture values kept {kept}",
        cases.len(),
        loaded.differ,
        loaded.formulas
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn close(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
        _ => false,
    }
}

/// (row, col) → the value the file stores, as shown, for sheet 1.
fn cached_values(xlsx: &[u8]) -> std::collections::HashMap<(u32, u32), String> {
    // Strip formulas so the reader keeps the cached values as plain values.
    let z = officina_convert::zip::Archive::open(xlsx).unwrap();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in z.entries.clone() {
        let mut d = z.read(&e).unwrap();
        if e.name.contains("worksheets/") {
            let s = String::from_utf8(d).unwrap();
            let mut o = String::with_capacity(s.len());
            let mut rest = s.as_str();
            while let Some(i) = rest.find("<f") {
                o.push_str(&rest[..i]);
                let t = &rest[i..];
                // <f .../> or <f ...>...</f>; not <fill> etc. (not in sheet parts).
                let close_tag = t.find('>').unwrap();
                if t.as_bytes()[close_tag - 1] == b'/' {
                    rest = &t[close_tag + 1..];
                } else {
                    let end = t.find("</f>").unwrap();
                    rest = &t[end + 4..];
                }
            }
            o.push_str(rest);
            d = o.into_bytes();
        }
        w.add(&e.name, &d).unwrap();
    }
    let plain = w.finish().unwrap();
    let (wb, _) = officina_convert::xlsx::read(&plain).unwrap();
    let mut out = std::collections::HashMap::new();
    let (rows, cols) = wb.extent(0);
    for r in 0..rows {
        for c in 0..cols {
            let v = wb.get(CellAddr::new(0, r, c));
            if !matches!(v, Value::Empty) {
                out.insert((r, c), v.display());
            }
        }
    }
    out
}
