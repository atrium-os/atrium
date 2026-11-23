//! Text filter fixtures and file checkpoints for the Sheets desktop gate.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::filter::{AutoFilter, Cond, Test, TextOp, TextTest};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const RANGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 6,
    c1: 1,
};
const MARKER: CellAddr = CellAddr {
    sheet: 0,
    row: 1,
    col: 3,
};

fn cell(row: u32) -> CellAddr {
    CellAddr::new(0, row, 1)
}

fn text(op: TextOp, value: &str, case_sensitive: bool) -> Test {
    Test::Text(TextTest {
        op,
        value: value.into(),
        case_sensitive,
    })
}

fn filter(format: &str) -> AutoFilter {
    use TextOp::*;
    let tests = match format {
        "ods" => vec![text(Regex, "A[0-9]+", true), text(NotEnds, "9", false)],
        "xlsx" => vec![
            text(Wildcard, "A?~**", false),
            text(NotWildcard, "A?~*9", false),
        ],
        _ => panic!("unexpected format {format}"),
    };
    AutoFilter {
        rect: RANGE,
        conds: vec![(1, Cond::Custom(tests, true))],
    }
}

fn values(format: &str) -> [&'static str; 6] {
    match format {
        "xlsx" => ["AX*1", "ay*2", "AZ21", "AZ*9", "BA*7", "AX"],
        "ods" => ["A101", "A12", "XA13", "A19", "a17", "A14Z"],
        _ => panic!("unexpected format {format}"),
    }
}

fn fixture(format: &str) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Text").unwrap();
    book.set_value(CellAddr::new(0, 0, 0), Value::str("Entry"));
    book.set_value(cell(0), Value::str("Code"));
    for (row, value) in (1..).zip(values(format)) {
        book.set_value(cell(row), Value::str(value));
    }
    book.set_value(cell(9), Value::str("Manually hidden"));
    book.sheet_mut(0).hidden = vec![(9, 9)];
    let state = book.filtered(0, Some(filter(format)));
    book.replace_filter(state);
    book
}

fn write(book: &Workbook, format: &str) -> Vec<u8> {
    match format {
        "xlsx" => xlsx::write(book, Vec::new()).unwrap(),
        "ods" => ods::write(book, Vec::new()).unwrap(),
        _ => panic!("unexpected format {format}"),
    }
}

fn read(bytes: &[u8], format: &str) -> Workbook {
    let (book, loaded) = match format {
        "xlsx" => xlsx::read(bytes).unwrap(),
        "ods" => ods::read(bytes).unwrap(),
        _ => panic!("unexpected format {format}"),
    };
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    book
}

fn check(book: &Workbook, state: &str, format: &str) {
    let (marker, edited, visible): (_, _, &[u32]) = match state {
        "initial" => (Value::Empty, false, &[1, 2]),
        "saved" => (Value::Num(8.0), false, &[1, 2]),
        "edited" => (Value::Num(8.0), true, &[1, 2]),
        "reapplied" => (Value::Num(8.0), true, &[2]),
        _ => panic!("unknown checkpoint {state}"),
    };
    let sheet = book.sheet(0);
    assert_eq!(sheet.filter, Some(filter(format)));
    let actual: Vec<_> = (1..=6).filter(|&row| !sheet.row_hidden(row)).collect();
    assert_eq!(actual, visible, "{format} {state}");
    assert!(!sheet.row_hidden(0));
    assert!(!sheet.row_hidden(8));
    assert!(sheet.row_hidden(9));
    assert_eq!(book.get(cell(9)), Value::str("Manually hidden"));
    assert_eq!(book.get(MARKER), marker);
    for (row, original) in (1..).zip(values(format)) {
        let expected = if row == 1 && edited { "bad" } else { original };
        assert_eq!(
            book.get(cell(row)),
            Value::str(expected),
            "{format} row {row}"
        );
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            for format in ["xlsx", "ods"] {
                fs::write(
                    Path::new(dir).join(format!("sales.{format}")),
                    write(&fixture(format), format),
                )
                .unwrap();
            }
        }
        [command, file, state, format] if command == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes, format)
            };
            check(&book, state, format);
        }
        _ => panic!("usage: sheets_text_filter_fixture make DIR | check FILE STATE FORMAT"),
    }
}

#[test]
fn saved_visibility_changes_only_on_reapply() {
    for format in ["xlsx", "ods"] {
        let mut book = read(&write(&fixture(format), format), format);
        check(&book, "initial", format);
        book.set_value(MARKER, Value::Num(8.0));
        book = read(&write(&book, format), format);
        check(&book, "saved", format);
        book.set_value(cell(1), Value::str("bad"));
        book = read(&write(&book, format), format);
        check(&book, "edited", format);
        use officina_core::history::Op;

        use officina_grid::{native, ops::GridOp};
        book = native::load(&native::save_to_vec(&book)).unwrap();
        let op = GridOp::Filter(book.filtered(0, book.sheet(0).filter.clone()));
        let undo = GridOp::decode(&op.encode()).unwrap().apply(&mut book);
        check(&book, "reapplied", format);
        let redo = undo.apply(&mut book);
        check(&book, "edited", format);
        redo.apply(&mut book);
        check(&read(&write(&book, format), format), "reapplied", format);
    }
}
