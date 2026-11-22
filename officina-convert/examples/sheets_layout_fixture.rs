//! Frozen panes and grid layout fixture for the installed Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::style::{Style, VAlign};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const TITLE: &str = "Merged layout heading";
const WRAPPED: &str = "This sentence wraps across several visual lines inside a sized cell.";
const MERGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 1,
    r1: 0,
    c1: 3,
};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Layout").unwrap();
    book.set_value(at(0, 0), Value::str("Frozen corner"));
    book.set_value(at(0, 1), Value::str(TITLE));
    book.set_merges(0, vec![MERGE]);
    book.set_value(at(1, 1), Value::str(WRAPPED));
    let wrap_cell = Rect {
        sheet: 0,
        r0: 1,
        c0: 1,
        r1: 1,
        c1: 1,
    };
    book.set_style(
        &wrap_cell,
        Style {
            wrap: true,
            v_align: VAlign::Top,
            fill: Some(0xDDEBF7),
            ..Style::default()
        },
    );
    book.set_value(at(2, 1), Value::str("Hidden row contents"));
    book.set_value(at(1, 2), Value::str("Hidden column contents"));
    book.set_value(at(3, 3), Value::Num(3.0));
    for row in 3..70 {
        book.set_value(at(row, 0), Value::str(&format!("Row {}", row + 1)));
    }
    for col in 4..45 {
        book.set_value(at(0, col), Value::str(&format!("Column {}", col + 1)));
        book.set_value(at(3, col), Value::Num(f64::from(col)));
    }
    let sheet = book.sheet_mut(0);
    sheet.freeze = (1, 1);
    sheet.hidden = vec![(2, 2)];
    sheet.hidden_widths = vec![(2, 118.0)];
    for (col, pixels) in [(0, 96.0), (1, 152.0), (2, 0.0), (3, 88.0)] {
        sheet.col_sizes.set(col, pixels);
    }
    for (row, pixels) in [(1, 72.0), (2, 44.0), (3, 24.0)] {
        sheet.row_sizes.set(row, pixels);
    }
    book
}

fn near(what: &str, actual: f64, expected: f64) {
    // XLSX stores column widths in character units and row heights in points.
    assert!(
        (actual - expected).abs() <= 2.0,
        "{what}: {actual} vs {expected}"
    );
}

fn check(book: &Workbook, state: &str) {
    let (value, column_width, row_height) = match state {
        "initial" => (3.0, 152.0, 24.0),
        "edited" => (8.0, 152.0, 24.0),
        "wider" => (8.0, 184.0, 24.0),
        "resized" => (8.0, 184.0, 48.0),
        _ => panic!("Unknown checkpoint {state}"),
    };
    assert_eq!(book.sheet_count(), 1);
    assert_eq!(book.sheet_name(0), "Layout");
    let sheet = book.sheet(0);
    assert_eq!(sheet.freeze, (1, 1));
    assert_eq!(sheet.freeze_from, (0, 0));
    assert_eq!(book.merges(0), &[MERGE]);
    assert_eq!(book.get(at(0, 1)), Value::str(TITLE));
    assert_eq!(book.get(at(1, 1)), Value::str(WRAPPED));
    let style = book.style(at(1, 1));
    assert!(style.wrap);
    assert_eq!(style.v_align, VAlign::Top);
    assert_eq!(style.fill, Some(0xDDEBF7));
    assert_eq!(sheet.hidden, [(2, 2)]);
    assert_eq!(sheet.view_row_sizes().size(2), 0.0);
    assert_eq!(sheet.col_sizes.size(2), 0.0);
    assert_eq!(book.get(at(2, 1)), Value::str("Hidden row contents"));
    assert_eq!(book.get(at(1, 2)), Value::str("Hidden column contents"));
    assert_eq!(sheet.hidden_widths.len(), 1);
    assert_eq!(sheet.hidden_widths[0].0, 2);
    near("hidden C width", sheet.hidden_widths[0].1, 118.0);
    near("A width", sheet.col_sizes.size(0), 96.0);
    near("B width", sheet.col_sizes.size(1), column_width);
    near("D width", sheet.col_sizes.size(3), 88.0);
    near("wrapped row height", sheet.row_sizes.size(1), 72.0);
    near("hidden row height", sheet.row_sizes.size(2), 44.0);
    near("row 4 height", sheet.row_sizes.size(3), row_height);
    assert_eq!(book.get(at(3, 3)), Value::Num(value));
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir] if action == "make" => {
            let book = fixture();
            let dir = Path::new(dir);
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
        [action, file, state] if action == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = if file.ends_with(".ods") {
                ods::read(&bytes).unwrap().0
            } else if file.ends_with(".xlsx") {
                xlsx::read(&bytes).unwrap().0
            } else {
                officina_grid::native::load(&bytes).unwrap()
            };
            check(&book, state);
        }
        _ => panic!("Expected make DIR or check FILE STATE"),
    }
}
