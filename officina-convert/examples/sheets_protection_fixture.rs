//! Protected-sheet workbooks and checkpoints for the installed Sheets gate.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::{
    protection::{ProtectionFormat, SheetProtection},
    style::Style,
    CellAddr, Rect, Value, Workbook,
};

const PASSWORD_NOTE: &str =
    "sheet protection passwords kept for the source format. Sheets cannot remove these locks";
const OPTIONS_NOTE: &str = "custom sheet protection permissions kept for the source format. Sheets allows edits to unlocked cells only";

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn macros(path: &Path) -> officina_grid::macros::Macros {
    let (book, _) = xlsx::read(&fs::read(path).unwrap()).unwrap();
    let mut macros = book.macros().unwrap().clone();
    macros.sheet_codes.retain(|(sheet, _)| *sheet == 0);
    macros
}

fn fixture(format: ProtectionFormat) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Orders").unwrap();
    let mut protection = SheetProtection::new(format);
    match format {
        ProtectionFormat::Excel => {
            assert!(protection.keep("password", "ABCD"));
            assert!(protection.keep("formatCells", "0"));
        }
        ProtectionFormat::Ods => {
            assert!(protection.keep("table:protection-key", "AQIDBA=="));
            assert!(protection.keep(
                "table:protection-key-digest-algorithm",
                "http://www.w3.org/2000/09/xmldsig#sha1"
            ));
        }
        // The fixture protects sheets, never the workbook's structure.
        ProtectionFormat::ExcelWorkbook => unreachable!("a sheet fixture"),
    }
    book.sheet_mut(0).protection = Some(protection);
    for (col, title) in ["Locked", "Editable", "Total", "Hidden"]
        .into_iter()
        .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(title));
    }
    book.set_value(at(1, 0), Value::Num(2.0));
    book.set_value(at(1, 1), Value::Num(4.0));
    book.set_formula(at(1, 2), "=A2+B2").unwrap();
    book.set_style(
        &Rect {
            sheet: 0,
            r0: 1,
            c0: 1,
            r1: 2,
            c1: 1,
        },
        Style {
            unlocked: true,
            fill: Some(0xc6efce),
            ..Style::default()
        },
    );
    book.set_style(
        &Rect::cell(at(1, 2)),
        Style {
            hide_formula: true,
            ..Style::default()
        },
    );
    if format == ProtectionFormat::Ods {
        book.set_value(at(1, 3), Value::Num(12.0));
        book.set_style(
            &Rect::cell(at(1, 3)),
            Style {
                hide_value: true,
                hide_formula: true,
                ..Style::default()
            },
        );
    }
    book.recalc();
    book
}

fn check(path: &Path, state: &str, format: &str, source: &Path) {
    let bytes = fs::read(path).unwrap();
    let book = match path.extension().unwrap().to_str().unwrap() {
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        "ods" => {
            let (book, loaded) = ods::read(&bytes).unwrap();
            let notes = loaded.report.human();
            assert_eq!(notes, [format!("approximate: {PASSWORD_NOTE}")]);
            book
        }
        "xlsx" | "xlsm" => {
            let (book, loaded) = xlsx::read(&bytes).unwrap();
            let notes = loaded.report.human();
            assert_eq!(
                notes.len(),
                if format == "xlsm" { 3 } else { 2 },
                "{notes:?}"
            );
            for note in [PASSWORD_NOTE, OPTIONS_NOTE] {
                assert!(notes.contains(&format!("approximate: {note}")), "{notes:?}");
            }
            if format == "xlsm" {
                assert!(notes.iter().any(|note| note.starts_with("inert: macros")));
            }
            book
        }
        _ => panic!("unexpected file type"),
    };
    assert_eq!(
        book.macros().cloned(),
        (format == "xlsm").then(|| macros(source))
    );
    let expected = fixture(if format == "ods" {
        ProtectionFormat::Ods
    } else {
        ProtectionFormat::Excel
    });
    assert_eq!(book.sheet(0).protection, expected.sheet(0).protection);
    assert!(!book.can_edit_cells(Rect::cell(at(1, 0))));
    assert!(book.can_edit_cells(Rect::cell(at(1, 1))));
    assert!(book.can_edit_cells(Rect::cell(at(2, 1))));
    assert!(!book.can_edit_cells(Rect::cell(at(2, 0))));
    assert!(book.hides_formula(at(1, 2)));
    assert_eq!(book.hides_value(at(1, 3)), format == "ods");
    if format == "ods" {
        assert_eq!(book.get(at(1, 3)), Value::Num(12.0));
    }
    assert_eq!(book.style(at(2, 1)).fill, Some(0xc6efce));
    assert_eq!(book.get(at(1, 0)), Value::Num(2.0));
    assert_eq!(book.formula_text(at(1, 2)).as_deref(), Some("=A2+B2"));
    let (units, blank_input) = match state {
        "initial" => (4.0, Value::Empty),
        "edited" => (8.0, Value::Num(6.0)),
        "reopened-edit" => (7.0, Value::Num(6.0)),
        _ => panic!("unknown state {state}"),
    };
    assert_eq!(book.get(at(1, 1)), Value::Num(units));
    assert_eq!(book.get(at(2, 1)), blank_input);
    assert_eq!(book.get(at(1, 2)), Value::Num(2.0 + units));
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir, source] if action == "make" => {
            let dir = Path::new(dir);
            let mut book = fixture(ProtectionFormat::Excel);
            fs::write(dir.join("plain.xlsx"), xlsx::write(&book, Vec::new()).unwrap()).unwrap();
            book.set_macros(macros(Path::new(source))).unwrap();
            fs::write(dir.join("macro.xlsm"), xlsx::write_xlsm(&book, Vec::new()).unwrap()).unwrap();
            let book = fixture(ProtectionFormat::Ods);
            fs::write(dir.join("open.ods"), ods::write(&book, Vec::new()).unwrap()).unwrap();
        }
        [action, workbook, state, format, source] if action == "check" => {
            check(Path::new(workbook), state, format, Path::new(source));
        }
        _ => panic!("usage: sheets_protection_fixture make DIR VBA_SOURCE | check FILE STATE FORMAT VBA_SOURCE"),
    }
}
