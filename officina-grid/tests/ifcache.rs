//! The -IF functions' range index gives what walking the range gives, to the bit.
use officina_grid::eval::Ctx;
use officina_grid::{CellAddr, Value, Workbook};
use std::time::Instant;

fn book(rows: u32, seed: u64) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sheet1").unwrap();
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let words = ["apple", "APPLE", "Apple", "pear", "5", "5.0", "TRUE", "", "x*", "-0"];
    for r in 0..rows {
        for c in 0..3 {
            let v = match next() % 9 {
                0 => Value::Empty,
                1 => Value::Bool(next() % 2 == 0),
                2 | 3 => Value::Num((next() % 7) as f64),
                4 => Value::Num(-0.0),
                5 => Value::Num(0.1 + (next() % 3) as f64 * 0.2),
                6 => Value::Err(officina_grid::ErrorKind::NA),
                _ => Value::str(words[(next() % words.len() as u64) as usize]),
            };
            if v != Value::Empty {
                wb.set_value(CellAddr::new(0, r, c), v);
            }
        }
        wb.set_value(CellAddr::new(0, r, 3), Value::Num((next() % 1000) as f64 / 7.0));
        wb.set_value(CellAddr::new(0, r, 4), Value::Num((next() % 1000) as f64 - 500.0));
    }
    wb
}

#[test]
fn indexed_if_functions_match_the_walk() {
    let rows = 20_000;
    let forms = [
        "=SUMIF($A$1:$A$20000,A1,$D$1:$D$20000)",
        "=SUMIF(A:A,B1)",
        "=COUNTIF(A:A,A1)",
        "=COUNTIFS(A:A,A1,B:B,B1)",
        "=SUMIFS(D:D,A:A,A1,C:C,C1)",
        "=AVERAGEIF(B:B,B1,D:D)",
        "=COUNTIF(A:A,\"apple\")",
        "=SUMIF(C:C,\">2\",D:D)",
        "=COUNTIF(A:A,\"x*\")",
        "=SUMIF(A:A,5,D:D)",
        "=COUNTIF(A:A,TRUE)",
        "=COUNTIF(A:A,\"\")",
        "=SUMIF(A1:B20000,A1,C1:D20000)",
        "=COUNTIF(D:D,\">\"&D1)",
        "=COUNTIF(A:A,\"<=\"&C1)",
        "=SUMIF(C:C,\">=\"&C1,E:E)",
        "=SUMIF(C:C,\"<\"&C1,D:D)",
        "=AVERAGEIF(C:C,\">\"&C1,E:E)",
        "=COUNTIFS(D:D,\">\"&D1,D:D,\"<\"&(D1+50),A:A,A1)",
        "=SUMIFS(E:E,C:C,\">=\"&C1,C:C,\"<=\"&(C1+3),B:B,B1)",
        "=SUMIFS(D:D,A:A,\">\"&C1,B:B,B1)",
        "=COUNTIF(A1:C20000,\">=\"&C1)",
    ];
    for (i, f) in forms.iter().enumerate() {
        let mut wb = book(rows, 0x9E37_79B9 + i as u64);
        let g = wb.fill_formula(0, 6, 0, rows - 1, f).unwrap();
        let t = Instant::now();
        wb.recalc();
        let took = t.elapsed();
        let expr = wb.group(g).expr.clone();
        for r in (0..rows).step_by(97) {
            let want = Ctx::new(&wb, 0, r, 6).eval_cell(&expr);
            let got = wb.value(0, r, 6);
            let same = match (&want, &got) {
                (Value::Num(a), Value::Num(b)) => a.to_bits() == b.to_bits(),
                _ => want == got,
            };
            assert!(same, "{f} row {r}: indexed {got:?}, walked {want:?}");
        }
        eprintln!("{f}: {rows} rows in {took:?}");
    }
}

#[test]
fn conditional_indexes_preserve_matched_errors_after_edits() {
    let mut wb = Workbook::new();
    wb.add_sheet("Summary").unwrap();
    for row in 0..3 {
        wb.set_value(CellAddr::new(0, row, 0), Value::Num(f64::from(row + 1)));
        wb.set_value(CellAddr::new(0, row, 1), Value::Num(f64::from((row + 1) * 10)));
    }
    let input = CellAddr::new(0, 1, 1);
    wb.set_value(input, Value::Err(officina_grid::ErrorKind::Div0));
    let rows = 64;
    for r in 0..rows {
        wb.set_value(CellAddr::new(0, r, 2), Value::Num(if r % 2 == 0 { 1.0 } else { 2.0 }));
    }
    wb.fill_formula(0, 3, 0, rows - 1, "=SUMIF($A$1:$A$3,C1+1,$B$1)").unwrap();
    wb.fill_formula(0, 4, 0, rows - 1, r#"=AVERAGEIFS($B$1:$B$3,$A$1:$A$3,">"&C1)"#).unwrap();
    wb.recalc();
    let err = Value::Err(officina_grid::ErrorKind::Div0);
    for r in 0..rows {
        for c in [3, 4] {
            assert_eq!(wb.value(0, r, c), if r % 2 == 0 { err.clone() } else { Value::Num(30.0) });
        }
    }
    wb.set_value(input, Value::Num(40.0));
    wb.recalc();
    for r in 0..rows {
        assert_eq!(wb.value(0, r, 3), Value::Num(if r % 2 == 0 { 40.0 } else { 30.0 }));
        assert_eq!(wb.value(0, r, 4), Value::Num(if r % 2 == 0 { 35.0 } else { 30.0 }));
    }
}
