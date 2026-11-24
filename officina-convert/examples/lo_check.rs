//! lo_check: `.xlsx` against LibreOffice, an independent implementation.
//!
//!   cargo run --release --example lo_check
//!   cargo run --example lo_check -- --focused-make DIR
//!   cargo run --example lo_check -- --focused-check CALC_OUTPUT_DIR
//!
//! For the focused matrix, use an isolated Calc profile configured to always
//! recalculate Excel files on load (OOXMLRecalcMode=0), then open and save each
//! generated XLSX with `--headless --convert-to xlsx --outdir CALC_OUTPUT_DIR`.
//! The check validates both phases and requires Calc to replace a poisoned cache.
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
    let mut args = std::env::args_os().skip(1);
    if let Some(command) = args.next() {
        let dir = std::path::PathBuf::from(args.next().expect("expected a workbook directory"));
        assert!(args.next().is_none(), "unexpected argument");
        match command.to_str() {
            Some("--focused-make") => focused_make(&dir),
            Some("--focused-check") => focused_check(&dir),
            _ => panic!("unknown command: {command:?}"),
        }
        return;
    }
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
    let st = Command::new(std::env::var_os("SOFFICE").unwrap_or_else(|| "soffice".into()))
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

struct Check {
    cell: &'static str,
    before: &'static str,
    after: &'static str,
}

const CHECKS: &[Check] = &[
    Check { cell: "A1", before: "60", after: "#DIV/0!" },
    Check { cell: "A2", before: "20", after: "#DIV/0!" },
    Check { cell: "A3", before: "30", after: "#DIV/0!" },
    Check { cell: "A4", before: "15", after: "#DIV/0!" },
    Check { cell: "A5", before: "3", after: "2" },
    Check { cell: "A6", before: "30", after: "90" },
    Check { cell: "D1", before: "2", after: "2" },
    Check { cell: "D2", before: "4", after: "4" },
    Check { cell: "D3", before: "#N/A", after: "#N/A" },
    Check { cell: "E1", before: "4", after: "4" },
    Check { cell: "E2", before: "6", after: "6" },
    Check { cell: "E3", before: "0", after: "0" },
    Check { cell: "E4", before: "0", after: "20" },
    Check { cell: "F1", before: "1", after: "1" },
    Check { cell: "F2", before: "99", after: "99" },
    Check { cell: "F3", before: "99", after: "99" },
    Check { cell: "H1", before: "42", after: "42" },
    Check { cell: "H2", before: "42", after: "42" },
    Check { cell: "H3", before: "42", after: "42" },
];

fn focused_cell(name: &str) -> CellAddr {
    let (row, col) = officina_grid::addr::parse_a1(name).unwrap();
    CellAddr::new(0, row, col)
}

fn focused_book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Report").unwrap();
    wb.add_sheet("Source data").unwrap();
    wb.define_name("Amounts", None, "='Source data'!$C$1").unwrap();
    for (name, value) in [
        ("A1", "yes"), ("A2", "yes"), ("B1", "no"), ("B2", "yes"),
        ("C1", "10"), ("C2", "20"), ("D1", "=NA()"), ("D2", "30"),
        ("F1", "1"), ("F2", "2"), ("F3", "3"),
    ] {
        wb.set_input(CellAddr { sheet: 1, ..focused_cell(name) }, value).unwrap();
    }
    for (name, code) in [
        ("A1", r#"=SUMIF('Source data'!$A$1:$B$2,"yes",Amounts)"#),
        ("A2", r#"=AVERAGEIF('Source data'!$A$1:$B$2,"yes",Amounts)"#),
        ("A3", r#"=SUMIFS('Source data'!$C$1:$C$2,'Source data'!$A$1:$A$2,"yes")"#),
        ("A4", r#"=AVERAGEIFS('Source data'!$C$1:$C$2,'Source data'!$A$1:$A$2,"yes")"#),
        ("A5", r#"=COUNTIF('Source data'!$A$1:$A$5,"")"#),
        ("A6", r#"=SUMIF('Source data'!$B$1:$B$2,"yes",'Source data'!$D$1)"#),
        ("D1", "='Source data'!F1:F2+'Source data'!F1:F3"),
        ("E1", "='Source data'!F2:F5*2"),
        ("F1", "=IFERROR('Source data'!F1:F3/{1;0},99)"),
        ("H1", "=40+2"),
        ("H2", r#"=SUM("4",TRUE,37)"#),
        ("H3", r#"=VALUE("4")+38"#),
    ] {
        wb.set_formula(focused_cell(name), code).unwrap();
    }
    wb.recalc();
    wb
}

fn phase_check(after: bool, calc: bool, mut get: impl FnMut(CellAddr) -> String) -> bool {
    let mut ok = true;
    for check in CHECKS {
        let sheets = if after { check.after } else { check.before };
        // Calc 26.8 retains the short array operand's missing row through
        // IFERROR, and rejects the direct text argument to SUM.
        let want = match (calc, check.cell) {
            (true, "F3") => "#N/A",
            (true, "H2") => "#VALUE!",
            _ => sheets,
        };
        let got = get(focused_cell(check.cell));
        let matched = got == want || close(&got, want);
        ok &= matched;
        println!("{} {} expected {want:<8} actual {got:<8} {}",
            if after { "edited " } else { "initial" },
            check.cell,
            if !matched { "DIFF" } else if want != sheets { "documented Calc difference" } else { "ok" });
    }
    ok
}

fn focused_make(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let mut wb = focused_book();
    for (phase, after) in [("initial", false), ("edited", true)] {
        if after {
            for (cell, value) in [("C2", "=1/0"), ("D2", "90"), ("A5", "no"), ("F5", "10")] {
                wb.set_input(CellAddr { sheet: 1, ..focused_cell(cell) }, value).unwrap();
            }
            wb.recalc();
        }
        assert!(phase_check(after, false, |at| wb.get(at).display()));
        let xlsx = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
        let ods = officina_convert::ods::write(&wb, Vec::new()).unwrap();
        for (label, reopened) in [
            ("XLSX", officina_convert::xlsx::read(&xlsx).unwrap().0),
            ("ODS", officina_convert::ods::read(&ods).unwrap().0),
        ] {
            println!("Reopened {label}");
            assert!(phase_check(after, false, |at| reopened.get(at).display()));
        }
        // A deliberately stale scalar cache positively verifies Calc recalculated
        // the formulas rather than copying our caches into its output.
        let zip = officina_convert::zip::Archive::open(&xlsx).unwrap();
        let mut out = officina_convert::zip::ZipWriter::new(Vec::new());
        for entry in &zip.entries {
            let mut data = zip.read(entry).unwrap();
            if entry.name == "xl/worksheets/sheet1.xml" {
                let xml = String::from_utf8(data).unwrap();
                let cache = "40+2</f><v>42</v>";
                assert_eq!(xml.matches(cache).count(), 1);
                data = xml.replace(cache, "40+2</f><v>987654</v>").into_bytes();
            }
            out.add(&entry.name, &data).unwrap();
        }
        std::fs::write(dir.join(format!("{phase}.xlsx")), out.finish().unwrap()).unwrap();
    }
}

fn focused_check(dir: &std::path::Path) {
    let mut ok = true;
    for (phase, after) in [("initial", false), ("edited", true)] {
        let data = std::fs::read(dir.join(format!("{phase}.xlsx"))).unwrap();
        let vals = cached_values(&data);
        let get = |a: CellAddr| vals.get(&(a.row, a.col)).cloned().unwrap_or_default();
        assert_eq!(get(focused_cell("H1")), "42", "Calc must replace the stale cache");
        println!("Calc caches");
        ok &= phase_check(after, true, get);
        let (reopened, _) = officina_convert::xlsx::read(&data).unwrap();
        println!("Recalculated in Sheets after opening Calc’s XLSX");
        ok &= phase_check(after, false, |a| reopened.get(a).display());
    }
    assert!(ok, "Formula results differ");
}
