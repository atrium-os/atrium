//! Fixtures for the installed Sheets XLSM and XLS desktop gate.
//! "make DIR SOURCE" takes Apache POI's SimpleMacro.xlsm as the VBA source.
use std::{env, fs, path::Path};

use officina_convert::{cfb, xls, xlsx};
use officina_grid::{CellAddr, Value, Workbook};

const MACRO_NOTICE: &str = "inert: macros (kept as they are for saving as .xlsm; never run)";
const MERGE_NOTICE: &str = "approximate: merged cells shown unmerged";

fn legacy_file() -> Vec<u8> {
    let sample = xls::sample::workbook(None);
    let mut stream = cfb::Cfb::open(&sample).unwrap().stream("Workbook").unwrap();
    let eof = [0x0a, 0, 0, 0];
    assert!(stream.ends_with(&eof));
    stream.truncate(stream.len() - eof.len());
    // One BIFF8 MergeCells record over E1:F1, immediately before the sheet EOF.
    stream.extend_from_slice(&[0xe5, 0, 10, 0, 1, 0, 0, 0, 0, 0, 4, 0, 5, 0]);
    stream.extend_from_slice(&eof);
    cfb::write(&[("Workbook", &stream)])
}

fn sample(path: &str) -> Workbook {
    let (book, _) = xlsx::read(&fs::read(path).unwrap()).unwrap();
    assert!(
        book.macros().is_some(),
        "the source must contain a VBA project"
    );
    assert_eq!(book.value(0, 0, 0), Value::str("This is a macro workbook"));
    book
}

fn check_macro(book: &Workbook, source: &Workbook, units: f64) {
    assert_eq!(
        book.macros(),
        source.macros(),
        "VBA parts, bytes and code names"
    );
    assert_eq!(book.sheet_count(), source.sheet_count());
    for sheet in 0..book.sheet_count() as u16 {
        assert_eq!(book.sheet_name(sheet), source.sheet_name(sheet));
        let (rows, columns) = book.extent(sheet);
        let (source_rows, source_columns) = source.extent(sheet);
        for row in 0..rows.max(source_rows) {
            for column in 0..columns.max(source_columns) {
                let expected = match (sheet, row, column) {
                    (0, 1, 0) => Value::Num(units),
                    (0, 1, 1) => Value::Num(units * 21.0),
                    _ => source.value(sheet, row, column),
                };
                assert_eq!(
                    book.value(sheet, row, column),
                    expected,
                    "cell {sheet}:{row}:{column}"
                );
            }
        }
    }
    assert_eq!(
        book.formula_text(CellAddr::new(0, 1, 1)).as_deref(),
        Some("=A2*21")
    );
}

fn check_legacy(book: &Workbook, units: f64) {
    assert_eq!(book.sheet_count(), 1);
    assert_eq!(book.sheet_name(0), "Data");
    assert_eq!(book.value(0, 0, 0), Value::Num(units));
    assert_eq!(book.value(0, 0, 1), Value::str("hello"));
    assert_eq!(book.value(0, 0, 2), Value::Num(units * 21.0));
    assert_eq!(
        book.formula_text(CellAddr::new(0, 0, 2)).as_deref(),
        Some("=A1*21")
    );
    assert!(book.merges(0).is_empty());
    assert!(book.macros().is_none());
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir, source] if action == "make" => {
            let mut book = sample(source);
            book.set_input(CellAddr::new(0, 1, 0), "2").unwrap();
            book.set_input(CellAddr::new(0, 1, 1), "=A2*21").unwrap();
            book.recalc();
            let dir = Path::new(dir);
            fs::write(
                dir.join("macro.xlsm"),
                xlsx::write_xlsm(&book, Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(dir.join("legacy.xls"), legacy_file()).unwrap();
        }
        [action, path, state, source] if action == "check" => {
            let bytes = fs::read(path).unwrap();
            let mut book = if path.ends_with(".xlsm") {
                let (book, loaded) = xlsx::read(&bytes).unwrap();
                assert_eq!(loaded.report.human(), [MACRO_NOTICE]);
                book
            } else if path.ends_with(".xls") {
                let (book, loaded) = xls::read(&bytes, None).unwrap();
                assert_eq!(loaded.report.human(), [MERGE_NOTICE]);
                book
            } else {
                officina_grid::native::load(&bytes).unwrap()
            };
            book.recalc();
            match state.as_str() {
                "macro-initial" => check_macro(&book, &sample(source), 2.0),
                "macro-edited" => check_macro(&book, &sample(source), 8.0),
                "xls-initial" => check_legacy(&book, 2.0),
                "native-copy" => check_legacy(&book, 8.0),
                _ => panic!("Unknown checkpoint {state}"),
            }
        }
        _ => panic!("Expected make DIR SOURCE or check FILE STATE SOURCE"),
    }
}
