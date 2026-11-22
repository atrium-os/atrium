//! Formula, date and error fidelity checkpoints for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::style::Style;
use officina_grid::value::ErrorKind;
use officina_grid::{CellAddr, Rect, Value, Workbook};

const DATE: f64 = 46345.0; // 2026-11-19 in the 1900 system.
const STALE_RESULT: &str = "formula results that differ from the file's saved results";

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn number_format(book: &mut Workbook, cell: CellAddr, format: &str) {
    let CellAddr { sheet, row, col } = cell;
    book.set_style(
        &Rect {
            sheet,
            r0: row,
            c0: col,
            r1: row,
            c1: col,
        },
        Style::default().with_num_fmt(format),
    );
}

fn workbook() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Report").unwrap();
    book.add_sheet("Input Data").unwrap();
    for (col, title) in [
        (1, "Rows"),
        (2, "Combined"),
        (3, "Spill"),
        (4, "Spill sum"),
        (5, "Division"),
        (6, "Error"),
        (7, "Dates and times"),
        (8, "Fallback"),
    ] {
        book.set_value(at(0, col), Value::str(title));
    }
    book.sheet_mut(0).col_sizes.set(7, 148.0);
    book.set_value(CellAddr::new(1, 1, 0), Value::Num(DATE));
    number_format(&mut book, CellAddr::new(1, 1, 0), "yyyy-mm-dd");
    book.set_value(CellAddr::new(1, 1, 1), Value::Num(5.0));
    book.define_name("Units", None, "='Input Data'!$B$2")
        .unwrap();
    book.set_value(at(1, 1), Value::Num(2.0));
    for (col, formula) in [
        (2, "=Units+'Input Data'!$B$2+B2"),
        (3, "=SEQUENCE(B2,1,10,1)"),
        (4, "=SUM(D2#)"),
        (5, "=1/(B2-3)"),
        (7, "='Input Data'!A2+B2"),
        (8, "=IFERROR(F2,42)"),
    ] {
        book.set_formula(at(1, col), formula).unwrap();
    }
    book.set_value(at(1, 6), Value::Err(ErrorKind::NA));
    book.set_formula(at(2, 6), "=SQRT(-1)").unwrap();
    book.set_value(at(2, 7), Value::Num(DATE + 0.625));
    book.set_value(at(3, 7), Value::Num(13.0 / 48.0));
    for (row, format) in [(1, "yyyy-mm-dd"), (2, "yyyy-mm-dd hh:mm"), (3, "hh:mm")] {
        number_format(&mut book, at(row, 7), format);
    }
    book.recalc();
    book
}

fn stale_cache(bytes: &[u8], format: &str) -> Vec<u8> {
    let (target, old, new) = match format {
        "xlsx" => (
            "xl/worksheets/sheet1.xml",
            "</f><v>12</v>",
            "</f><v>999</v>",
        ),
        "ods" => (
            "content.xml",
            r#"office:value="12""#,
            r#"office:value="999""#,
        ),
        _ => unreachable!(),
    };
    let archive = Archive::open(bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for part in &archive.entries {
        let data = archive.read(part).unwrap();
        if part.name == target {
            let xml = String::from_utf8(data).unwrap();
            assert_eq!(xml.matches(old).count(), 1, "C2 cache in {format}");
            out.add(&part.name, xml.replace(old, new).as_bytes())
                .unwrap();
        } else if part.name == "mimetype" {
            out.add_stored(&part.name, &data).unwrap();
        } else {
            out.add(&part.name, &data).unwrap();
        }
    }
    out.finish().unwrap()
}

fn make(dir: &Path) {
    let book = workbook();
    for (format, bytes) in [
        ("ods", ods::write(&book, Vec::new()).unwrap()),
        ("xlsx", xlsx::write(&book, Vec::new()).unwrap()),
    ] {
        fs::write(
            dir.join(format!("sales.{format}")),
            stale_cache(&bytes, format),
        )
        .unwrap();
    }
}

fn check(file: &Path, state: &str, format: &str) {
    let bytes = fs::read(file).unwrap();
    let (book, report) = match file.extension().unwrap().to_str().unwrap() {
        "ods" => {
            let (book, loaded) = ods::read(&bytes).unwrap();
            (book, Some(loaded.report))
        }
        "xlsx" => {
            let (book, loaded) = xlsx::read(&bytes).unwrap();
            (book, Some(loaded.report))
        }
        "osheet" => {
            // Native autosave snapshots rebuild spill followers when Sheets opens them.
            let mut book = officina_grid::native::load(&bytes).unwrap();
            book.recalc();
            (book, None)
        }
        _ => panic!("unexpected file type"),
    };
    if let Some(report) = report {
        let notes = report.human();
        if state == "initial" {
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(
                notes[0].contains(STALE_RESULT) && notes[0].contains("C2: 12 vs 999"),
                "{notes:?}"
            );
        } else {
            assert!(notes.is_empty(), "{notes:?}");
        }
    }
    let rows: u32 = match state {
        "initial" => 2,
        "edited" => 3,
        "resized" => 4,
        _ => panic!("unknown checkpoint {state}"),
    };
    assert_eq!(book.sheet_count(), 2);
    assert_eq!(book.sheet_name(1), "Input Data");
    assert!(book.names().get(0, "Units").is_some());
    assert_eq!(
        book.formula_text(at(1, 2)).as_deref(),
        Some("=Units+'Input Data'!$B$2+B2")
    );
    assert_eq!(book.get(at(1, 1)), Value::Num(f64::from(rows)));
    assert_eq!(book.get(at(1, 2)), Value::Num(10.0 + f64::from(rows)));
    assert_eq!(book.spill_area(at(1, 3)).unwrap().rows(), u64::from(rows));
    for i in 0..rows {
        assert_eq!(book.get(at(i + 1, 3)), Value::Num(10.0 + f64::from(i)));
    }
    assert_eq!(book.get(at(rows + 1, 3)), Value::Empty);
    match (format, state) {
        ("ods", "initial") => assert_eq!(
            book.formula_text(at(1, 4)).as_deref(),
            Some("=SUM($D$2:$D$3)")
        ),
        ("ods", _) => assert_eq!(
            book.formula_text(at(1, 4)).as_deref(),
            Some("=SUM($D$2:$D$4)")
        ),
        _ => assert_eq!(book.formula_text(at(1, 4)).as_deref(), Some("=SUM(D2#)")),
    }
    let sum = match (format, rows) {
        (_, 2) => 21.0,
        ("ods", _) | (_, 3) => 33.0,
        _ => 46.0,
    };
    assert_eq!(book.get(at(1, 4)), Value::Num(sum));
    let division = match rows {
        2 => Value::Num(-1.0),
        3 => Value::Err(ErrorKind::Div0),
        _ => Value::Num(1.0),
    };
    assert_eq!(book.get(at(1, 5)), division);
    assert_eq!(book.get(at(1, 6)), Value::Err(ErrorKind::NA));
    assert_eq!(book.get(at(2, 6)), Value::Err(ErrorKind::Num));
    assert_eq!(
        book.get(at(1, 8)),
        Value::Num(match rows {
            2 => -1.0,
            3 => 42.0,
            _ => 1.0,
        })
    );
    assert_eq!(book.get(CellAddr::new(1, 1, 0)), Value::Num(DATE));
    assert_eq!(book.get(at(1, 7)), Value::Num(DATE + f64::from(rows)));
    assert_eq!(
        book.formatted(at(1, 7)).text,
        format!("2026-11-{}", 19 + rows)
    );
    assert_eq!(book.formatted(at(2, 7)).text, "2026-11-19 15:00");
    assert_eq!(book.formatted(at(3, 7)).text, "06:30");
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => make(Path::new(dir)),
        [command, file, state, format] if command == "check" => {
            check(Path::new(file), state, format)
        }
        _ => panic!("usage: sheets_formula_fixture make DIR | check FILE STATE FORMAT"),
    }
}
