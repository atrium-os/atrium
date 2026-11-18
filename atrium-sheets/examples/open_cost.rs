//! How long opening a saved workbook holds the UI before its first frame.
use std::time::Instant;

use atrium_sheets::doc::Doc;

fn main() {
    let rows: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    for f in ["=SUMIF($A$1:$A$50000,\">\"&A1)", "=COUNTIF($A$1:$A$50000,A1)+RAND()*0", "=A1*2", "=SUMIFS($A$1:$A$50000,$A$1:$A$50000,\">\"&A1)"] {
        let mut d = Doc::new("x");
        let a: Vec<f64> = (0..rows).map(|r| (r % 997) as f64).collect();
        d.wb.set_numbers(0, 0, 0, &a);
        d.wb.fill_formula(0, 1, 0, rows - 1, f).unwrap();
        d.wb.recalc();
        let bytes = d.stored_native().unwrap();
        let t = Instant::now();
        let d2 = Doc::from_native("x", &bytes).unwrap();
        println!("{f} x {rows}: open {:?} ({} groups pending? {})", t.elapsed(), d2.wb.groups().count(), d2.wb.recalc_pending());
    }
}
