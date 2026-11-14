//! numfmt_lo: our number formats against LibreOffice's, through `TEXT()`.
//!
//!   cargo run --release --example numfmt_lo
//!
//! Every (value, format code) pair goes in a cell as `=TEXT(value, "code")`; our
//! `.xlsx` goes through LibreOffice (`soffice --headless --convert-to xlsx`), which
//! recalculates; its cached results are compared with ours. LibreOffice is not Excel:
//! the disagreements are printed for a person to judge, with a summary line.
//!
//! 2026-11-14: 801 of 1,000 agree. Every other case is a known LibreOffice departure
//! from Excel, where ours shows what Excel shows: dates before 1900-03-01 (LibreOffice
//! counts from 1899-12-30 and has no 29 February 1900), negative and past-9999 dates
//! (Excel shows `#`, its `TEXT` `#VALUE!`), seconds truncated instead of rounded,
//! `#.##` without its trailing point (`1.`), and General's `1E-10`.

use std::process::Command;

use officina_grid::{CellAddr, Value, Workbook};

const VALUES: &[f64] = &[0.0, 1.0, -1.0, 0.5, -0.25, 1234.5678, -1234.5678, 0.000123, 12345678.9, 2.675, 0.125, 1.5, 3.14159, 46340.6463541667, 60.0, 61.0, 0.75, 999.999, 1e-10, 36.5];
const CODES: &[&str] = &[
    "General", "0", "0.00", "#,##0", "#,##0.00", "0%", "0.00%", "0.00E+00", "##0.0E+0", "# ?/?", "# ??/??", "?/8",
    "m/d/yyyy", "d-mmm-yy", "d-mmm", "mmm-yy", "h:mm AM/PM", "h:mm:ss AM/PM", "h:mm", "h:mm:ss", "m/d/yyyy h:mm",
    "#,##0 ;(#,##0)", "#,##0.00;(#,##0.00)", "mm:ss", "[h]:mm:ss", "mmss.0", "yyyy-mm-dd", "dddd, mmmm d, yyyy", "ddd", "mmmmm",
    "hh:mm:ss.00", "[mm]:ss", "#.##", "0.0#", "000-0000", "00000", "#,##0,", "0.0,,\"M\"", "\"$\"#,##0.00", "0;(0)", "0;-0;\"zero\"",
    "[<1000]0;0.0,\"k\"", "0.0\"x\"", "@", "\"n=\"0", "#,##0.000", "0.000E+0", "yy", "h AM/PM", "[h]:mm",
];

fn main() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let mut cases = Vec::new();
    for v in VALUES {
        for c in CODES {
            let r = cases.len() as u32;
            let code = c.replace('"', "\"\"");
            wb.set_input(CellAddr::new(0, r, 0), &format!("=TEXT({v},\"{code}\")")).unwrap();
            cases.push((*v, *c));
        }
    }
    wb.recalc();
    let dir = std::env::temp_dir().join(format!("numfmt-lo-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let ours = a.join("t.xlsx");
    std::fs::write(&ours, officina_convert::xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
    let st = Command::new("soffice")
        .args(["--headless", "--norestore", "--convert-to", "xlsx:Calc MS Excel 2007 XML", "--outdir"])
        .arg(&b)
        .arg(&ours)
        .env("HOME", &dir)
        .output()
        .expect("soffice");
    let lo_file = b.join("t.xlsx");
    if !lo_file.exists() {
        eprintln!("LibreOffice did not convert:\n{}", String::from_utf8_lossy(&st.stderr));
        std::process::exit(1);
    }
    let lo = cached_values(&std::fs::read(&lo_file).unwrap());
    let mut same = 0;
    for (i, (v, c)) in cases.iter().enumerate() {
        let r = i as u32;
        let o = wb.get(CellAddr::new(0, r, 0)).display();
        let l = lo.get(&(r, 0)).cloned().unwrap_or_default();
        if o == l {
            same += 1;
        } else {
            println!("{v:<18} {c:<24} ours {:<26} LibreOffice {:?}", format!("{o:?}"), l);
        }
    }
    println!("summary: {} cases; ours = LibreOffice on {same}", cases.len());
    let _ = std::fs::remove_dir_all(&dir);
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
