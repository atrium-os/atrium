//! Two-sheet review and three-way cell merge for foreign workbook formats.
use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const ROWS: u32 = 1200;
const INCOMING_TEXT: &str = "Incoming detail stays readable from its opening words through the separate detail area. This last line ends with the word Lantern.";

fn set(book: &mut Workbook, sheet: u16, row: u32, col: u32, value: f64) {
    book.set_value(CellAddr::new(sheet, row, col), Value::Num(value));
}

fn fixture(variant: &str) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Data").unwrap();
    book.add_sheet("Notes").unwrap();
    for row in 0..ROWS {
        set(&mut book, 0, row, 0, f64::from(row + 1));
        set(
            &mut book,
            0,
            row,
            1,
            if variant == "review" {
                1000.0 + f64::from(row)
            } else {
                10.0
            },
        );
        book.set_value(CellAddr::new(0, row, 2), Value::str("stable"));
    }
    book.set_value(CellAddr::new(1, 0, 0), Value::str("Review values"));
    let (b, c, d, e, f, note) = match variant {
        "local" => (10.0, 20.0, 30.0, 40.0, 51.0, 70.0),
        "base" => (1.0, 2.0, 3.0, 40.0, 50.0, 7.0),
        "incoming" => (11.0, 22.0, 3.0, 44.0, 51.0, 7.0),
        "review" => (10.0, 20.0, 30.0, 40.0, 51.0, 99.0),
        _ => panic!("unknown variant {variant}"),
    };
    for (col, value) in [(1, b), (2, c), (3, d), (4, e), (5, f)] {
        set(&mut book, 0, 0, col, value);
    }
    set(&mut book, 1, 0, 1, note);
    set(&mut book, 1, 0, 2, 0.0);
    if variant == "incoming" {
        book.set_formula(CellAddr::new(0, 0, 1), "=5+6").unwrap();
        book.set_formula(CellAddr::new(1, 0, 2), "=Data!B1*2")
            .unwrap();
    }
    if variant == "review" {
        book.set_value(CellAddr::new(0, 0, 1), Value::str(INCOMING_TEXT));
        book.set_style(
            &Rect {
                sheet: 0,
                r0: 1,
                c0: 3,
                r1: 1,
                c1: 3,
            },
            officina_grid::style::Style {
                fill: Some(0xDDEBF7),
                ..Default::default()
            },
        );
    }
    book.recalc();
    book
}

fn make(dir: &Path) {
    for format in ["ods", "xlsx"] {
        let target = dir.join(format);
        fs::create_dir_all(&target).unwrap();
        for (name, variant) in [
            ("01-local", "local"),
            ("02-review", "review"),
            ("03-base", "base"),
            ("04-incoming", "incoming"),
        ] {
            let book = fixture(variant);
            let file = fs::File::create(target.join(format!("{name}.{format}"))).unwrap();
            match format {
                "ods" => ods::write(&book, file).unwrap(),
                "xlsx" => xlsx::write(&book, file).unwrap(),
                _ => unreachable!(),
            };
        }
    }
}

fn check(path: &Path, state: &str) {
    let bytes = fs::read(path).unwrap();
    let book = match path.extension().unwrap().to_str().unwrap() {
        "ods" => ods::read(&bytes).unwrap().0,
        "xlsx" => xlsx::read(&bytes).unwrap().0,
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("unexpected extension"),
    };
    let (b1, e1, result, b1_formula, cross_formula) = match state {
        "local" => (10.0, 40.0, 0.0, None, None),
        "independent" => (10.0, 44.0, 20.0, None, Some("=Data!B1*2")),
        "resolved" => (11.0, 44.0, 22.0, Some("=5+6"), Some("=Data!B1*2")),
        _ => panic!("unknown checkpoint {state}"),
    };
    assert_eq!(book.sheet_count(), 2);
    assert_eq!(book.sheet_name(0), "Data");
    assert_eq!(book.sheet_name(1), "Notes");
    for (sheet, col, value) in [
        (0, 1, b1),
        (0, 2, 20.0),
        (0, 3, 30.0),
        (0, 4, e1),
        (0, 5, 51.0),
        (1, 1, 70.0),
        (1, 2, result),
    ] {
        assert_eq!(
            book.get(CellAddr::new(sheet, 0, col)),
            Value::Num(value),
            "sheet {sheet}, column {col}"
        );
    }
    assert_eq!(
        book.formula_text(CellAddr::new(0, 0, 1)).as_deref(),
        b1_formula
    );
    assert_eq!(
        book.formula_text(CellAddr::new(1, 0, 2)).as_deref(),
        cross_formula
    );
    for row in 1..ROWS {
        assert_eq!(
            book.get(CellAddr::new(0, row, 0)),
            Value::Num(f64::from(row + 1))
        );
        assert_eq!(book.get(CellAddr::new(0, row, 1)), Value::Num(10.0));
        assert_eq!(book.get(CellAddr::new(0, row, 2)), Value::str("stable"));
    }
    assert_eq!(book.style(CellAddr::new(0, 1, 3)).fill, None);
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => make(Path::new(dir)),
        [command, path, state] if command == "check" => check(Path::new(path), state),
        _ => panic!("usage: sheets_review_fixture make DIR | check FILE STATE"),
    }
}
