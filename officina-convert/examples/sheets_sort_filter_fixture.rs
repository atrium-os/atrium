//! Sort order, visibility and formula checkpoints for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::filter::{AutoFilter, Cond};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const RANGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 6,
    c1: 5,
};
const ORDERS: [(&str, &str, u32, u32, &str); 6] = [
    ("South", "Open", 2, 5, "S2"),
    ("North", "Open", 1, 7, "N1"),
    ("South", "Closed", 3, 4, "S3"),
    ("North", "Closed", 2, 9, "N2A"),
    ("North", "Open", 3, 2, "N3"),
    ("North", "Open", 2, 6, "N2B"),
];

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Orders").unwrap();
    book.add_sheet("Checks").unwrap();
    for (col, name) in ["Region", "Status", "Rank", "Qty", "Total", "ID"]
        .into_iter()
        .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(name));
    }
    for (i, (region, status, rank, qty, id)) in ORDERS.into_iter().enumerate() {
        let row = i as u32 + 1;
        for (col, value) in [
            (0, Value::str(region)),
            (1, Value::str(status)),
            (2, Value::Num(f64::from(rank))),
            (3, Value::Num(f64::from(qty))),
            (5, Value::str(id)),
        ] {
            book.set_value(at(row, col), value);
        }
        book.set_formula(at(row, 4), &format!("=C{}*D{}", row + 1, row + 1))
            .unwrap();
    }
    book.set_value(at(0, 7), Value::str("Checks"));
    book.set_formula(at(1, 7), "=E2-E7").unwrap();
    book.set_formula(at(2, 7), "=SUM(E2:E7)").unwrap();
    book.set_formula(CellAddr::new(1, 0, 0), "=Orders!E2")
        .unwrap();
    book.set_value(at(8, 0), Value::str("Manual hidden"));
    book.sheet_mut(0).hidden = vec![(8, 8)];
    book.sheet_mut(0).filter = Some(AutoFilter {
        rect: RANGE,
        conds: vec![],
    });
    book.recalc();
    book
}

fn check(file: &Path, state: &str) {
    let bytes = fs::read(file).unwrap();
    let book = match file.extension().unwrap().to_str().unwrap() {
        "xlsx" => xlsx::read(&bytes).unwrap().0,
        "ods" => ods::read(&bytes).unwrap().0,
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("unexpected file type"),
    };
    let (order, delta, first_total) = match state {
        "initial" => ([0, 1, 2, 3, 4, 5], -2.0, 10.0),
        "secondary" => ([2, 4, 0, 3, 5, 1], 5.0, 12.0),
        "sorted" | "region" | "filtered" => ([4, 3, 5, 1, 2, 0], -4.0, 6.0),
        _ => panic!("unknown checkpoint {state}"),
    };
    let (conditions, hidden) = match state {
        "region" => (
            vec![(0, Cond::Values(vec!["North".into()]))],
            vec![(5, 6), (8, 8)],
        ),
        "filtered" => (
            vec![
                (0, Cond::Values(vec!["North".into()])),
                (1, Cond::Values(vec!["Open".into()])),
            ],
            vec![(2, 2), (5, 6), (8, 8)],
        ),
        _ => (vec![], vec![(8, 8)]),
    };
    assert_eq!(book.sheet_name(0), "Orders");
    assert_eq!(book.sheet_name(1), "Checks");
    for (col, header) in ["Region", "Status", "Rank", "Qty", "Total", "ID"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(book.get(at(0, col as u32)), Value::str(header));
    }
    for (i, src) in order.into_iter().enumerate() {
        let row = i as u32 + 1;
        let (region, status, rank, qty, id) = ORDERS[src];
        for (col, value) in [
            (0, Value::str(region)),
            (1, Value::str(status)),
            (2, Value::Num(f64::from(rank))),
            (3, Value::Num(f64::from(qty))),
            (4, Value::Num(f64::from(rank * qty))),
            (5, Value::str(id)),
        ] {
            assert_eq!(
                book.get(at(row, col)),
                value,
                "row {} column {}",
                row + 1,
                col + 1
            );
        }
        assert_eq!(
            book.formula_text(at(row, 4)).unwrap(),
            format!("=C{}*D{}", row + 1, row + 1)
        );
    }
    assert_eq!(
        book.sheet(0).filter,
        Some(AutoFilter {
            rect: RANGE,
            conds: conditions
        })
    );
    assert_eq!(book.sheet(0).hidden, hidden);
    assert_eq!(book.get(at(8, 0)), Value::str("Manual hidden"));
    assert_eq!(book.get(at(1, 7)), Value::Num(delta));
    assert_eq!(book.get(at(2, 7)), Value::Num(65.0));
    assert_eq!(book.get(CellAddr::new(1, 0, 0)), Value::Num(first_total));
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            let dir = Path::new(dir);
            let book = fixture();
            fs::write(
                dir.join("sales.ods"),
                ods::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(
                dir.join("sales.xlsx"),
                xlsx::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [command, file, state] if command == "check" => check(Path::new(file), state),
        _ => panic!("usage: sheets_sort_filter_fixture make DIR | check FILE STATE"),
    }
}
