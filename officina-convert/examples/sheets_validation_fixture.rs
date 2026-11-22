//! Validation fixtures and persisted checkpoints for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::condfmt::CmpOp;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Inputs").unwrap();
    for (col, text) in [
        "Control",
        "Paste here",
        "Fill left",
        "Fill right",
        "Mixed",
        "Valid",
        "Check",
    ]
    .into_iter()
    .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(text));
    }
    for (row, col, value) in [
        (1, 1, 2.0),
        (2, 1, 3.0),
        (1, 2, 6.0),
        (1, 3, 9.0),
        (2, 2, 4.0),
        (2, 3, 5.0),
        (1, 4, 7.0),
        (2, 4, 9.0),
        (1, 5, 6.0),
        (2, 5, 8.0),
    ] {
        book.set_value(at(row, col), Value::Num(value));
    }
    book.set_formula(at(1, 6), "=SUM(B2:B3,C3:D3)").unwrap();
    for range in [
        Rect {
            sheet: 0,
            r0: 1,
            r1: 2,
            c0: 1,
            c1: 1,
        },
        Rect {
            sheet: 0,
            r0: 2,
            r1: 2,
            c0: 2,
            c1: 3,
        },
    ] {
        let mut rule = Validation::new(
            0,
            vec![range],
            Allow::Whole,
            CmpOp::Between,
            Some(
                book.validation_formula("1", 0, (range.r0, range.c0))
                    .unwrap(),
            ),
            Some(
                book.validation_formula("8", 0, (range.r0, range.c0))
                    .unwrap(),
            ),
        );
        rule.dropdown = false;
        rule.error = Message::new("Outside limit", "Use a whole number from 1 to 8.");
        book.add_validation(rule).unwrap();
    }
    book.recalc();
    book
}

fn check(file: &Path, state: &str) {
    let bytes = fs::read(file).unwrap();
    let book = match file.extension().unwrap().to_str().unwrap() {
        "xlsx" => xlsx::read(&bytes).unwrap().0,
        "ods" => ods::read(&bytes).unwrap().0,
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("unexpected extension"),
    };
    let (b2, b3, d2, c3, d3) = match state {
        "initial" => (2.0, 3.0, 9.0, 4.0, 5.0),
        "typed" => (8.0, 3.0, 9.0, 4.0, 5.0),
        "pasted" => (6.0, 8.0, 9.0, 4.0, 5.0),
        "ready" => (6.0, 8.0, 8.0, 4.0, 5.0),
        "filled" => (6.0, 8.0, 8.0, 6.0, 8.0),
        _ => panic!("unknown case {state}"),
    };
    for (row, col, value) in [
        (1, 1, b2),
        (2, 1, b3),
        (1, 2, 6.0),
        (1, 3, d2),
        (2, 2, c3),
        (2, 3, d3),
        (1, 4, 7.0),
        (2, 4, 9.0),
        (1, 5, 6.0),
        (2, 5, 8.0),
        (1, 6, b2 + b3 + c3 + d3),
    ] {
        assert_eq!(
            book.get(at(row, col)),
            Value::Num(value),
            "row {row}, column {col}"
        );
    }
    for cell in [at(1, 1), at(2, 1), at(2, 2), at(2, 3)] {
        let rule = book.validation_at(cell).unwrap();
        assert_eq!(rule.allow, Allow::Whole);
        assert_eq!(rule.style, ErrorStyle::Stop);
        assert_eq!(
            rule.error,
            Message::new("Outside limit", "Use a whole number from 1 to 8.")
        );
        assert!(rule.show_error);
        assert!(book.check_cell(cell).is_ok());
    }
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
        _ => panic!("usage: sheets_validation_fixture make DIR | check FILE STATE"),
    }
}
