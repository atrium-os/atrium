//! Excel date-system and ISO date fixtures for the installed Sheets desktop gate.

use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{cfb, xls, xlsx};
use officina_grid::eval::dates::DateSystem;
use officina_grid::style::Style;
use officina_grid::{CellAddr, Rect, Value, Workbook};

const TIMESTAMP: &str = "1904-01-03T12:30:00.125Z";
const MACRO_NOTICE: &str = "inert: macros (kept as they are for saving as .xlsm; never run)";

const FORMULAS: [(u32, &str); 6] = [
    (2, "=B2-A2"),
    (3, "=DATE(1904,2,29)"),
    (4, "=A2+1"),
    (5, "=YEAR(A2)*10000+MONTH(A2)*100+DAY(A2)"),
    (6, "=TEXT(A2,\"ddd yyyy-mm-dd\")"),
    (7, "=WEEKDAY(A2)"),
];

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn modern() -> Workbook {
    let mut wb = Workbook::with_date_system(DateSystem::Excel1904);
    wb.add_sheet("Dates").unwrap();
    for (col, width) in [
        (0, 88.0),
        (1, 184.0),
        (2, 124.0),
        (3, 88.0),
        (4, 88.0),
        (5, 86.0),
        (6, 136.0),
        (7, 75.0),
        (9, 88.0),
    ] {
        wb.sheet_mut(0).col_sizes.set(col, width);
    }
    for (col, title) in [
        "Start",
        "ISO timestamp",
        "Days apart",
        "Leap day",
        "Next day",
        "Parts",
        "Text",
        "Weekday",
        "Time",
        "ISO date",
    ]
    .into_iter()
    .enumerate()
    {
        wb.set_value(at(0, col as u32), Value::str(title));
    }
    wb.set_value(at(1, 0), Value::Num(0.0));
    wb.set_value(at(1, 1), Value::Num(2.0 + 45000.125 / 86400.0));
    wb.set_value(at(1, 8), Value::Num(0.5));
    wb.set_value(at(1, 9), Value::Num(60.0));
    for (col, formula) in FORMULAS {
        wb.set_formula(at(1, col), formula).unwrap();
    }
    for (col, code) in [
        (0, "yyyy-mm-dd"),
        (2, "0.000"),
        (3, "yyyy-mm-dd"),
        (4, "yyyy-mm-dd"),
        (8, "hh:mm"),
    ] {
        wb.set_style(&Rect::cell(at(1, col)), Style::default().with_num_fmt(code));
    }
    wb.recalc();
    wb
}

fn iso_cells(bytes: &[u8]) -> Vec<u8> {
    let src = Archive::open(bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for part in &src.entries {
        let mut data = src.read(part).unwrap();
        if part.name == "xl/worksheets/sheet1.xml" {
            let mut xml = String::from_utf8(data).unwrap();
            for (address, iso) in [("B2", TIMESTAMP), ("J2", "1904-03-01")] {
                let start = xml.find(&format!("<c r=\"{address}\"")).unwrap();
                let end = start + xml[start..].find("</c>").unwrap() + "</c>".len();
                xml.replace_range(
                    start..end,
                    &format!("<c r=\"{address}\" t=\"d\"><v>{iso}</v></c>"),
                );
            }
            data = xml.into_bytes();
        }
        out.add(&part.name, &data).unwrap();
    }
    out.finish().unwrap()
}

fn legacy() -> Vec<u8> {
    let sample = xls::sample::workbook(None);
    let mut stream = cfb::Cfb::open(&sample).unwrap().stream("Workbook").unwrap();
    let mut pos = 0;
    let mut xf = 0;
    let mut sheet_offset = None;
    while pos + 4 <= stream.len() {
        let tag = u16::from_le_bytes(stream[pos..pos + 2].try_into().unwrap());
        let size = u16::from_le_bytes(stream[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let data = &mut stream[pos + 4..pos + 4 + size];
        match tag {
            0x00e0 => {
                if xf == 14 {
                    data[2..4].copy_from_slice(&14u16.to_le_bytes()); // Built-in date.
                }
                xf += 1;
            }
            0x0085 => sheet_offset = Some(pos + 4),
            0x0203 => {
                assert_eq!(data[..4], [0, 0, 0, 0]);
                data[4..6].copy_from_slice(&14u16.to_le_bytes());
                data[6..14].copy_from_slice(&0.0f64.to_le_bytes());
            }
            0x0006 => {
                data[6..14].copy_from_slice(&21.0f64.to_le_bytes());
                *data.last_mut().unwrap() = 0x03; // ptgAdd, in A1+21.
            }
            _ => {}
        }
        pos += 4 + size;
    }
    let offset = sheet_offset.unwrap();
    let original = u32::from_le_bytes(stream[offset..offset + 4].try_into().unwrap());
    let epoch = [0x22, 0, 2, 0, 1, 0]; // DATE1904.
    stream[offset..offset + 4].copy_from_slice(&(original + epoch.len() as u32).to_le_bytes());
    stream.splice(offset - 4..offset - 4, epoch);
    let eof = [0x0a, 0, 0, 0];
    assert!(stream.ends_with(&eof));
    let end = stream.len() - eof.len();
    // COLINFO sets column A to 16 characters so its date is visible.
    stream.splice(
        end..end,
        [0x7d, 0, 12, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0],
    );
    cfb::write(&[("Workbook", &stream)])
}

fn macros(source: &Path) -> officina_grid::macros::Macros {
    let (wb, _) = xlsx::read(&fs::read(source).unwrap()).unwrap();
    let mut macros = wb.macros().unwrap().clone();
    macros.sheet_codes.retain(|(sheet, _)| *sheet == 0);
    macros
}

fn check(wb: &Workbook, legacy: bool, edited: bool) {
    assert_eq!(wb.date_system(), DateSystem::Excel1904);
    let start = if edited { 8.0 } else { 0.0 };
    if legacy {
        assert_eq!(wb.get(at(0, 0)), Value::Num(start));
        assert_eq!(
            wb.formatted(at(0, 0)).text,
            if edited { "1/9/1904" } else { "1/1/1904" }
        );
        assert_eq!(wb.formula_text(at(0, 2)).as_deref(), Some("=A1+21"));
        assert_eq!(wb.get(at(0, 2)), Value::Num(start + 21.0));
        return;
    }
    for (col, formula) in FORMULAS {
        assert_eq!(wb.formula_text(at(1, col)).as_deref(), Some(formula));
    }
    assert_eq!(wb.get(at(1, 0)), Value::Num(start));
    assert_eq!(
        wb.formatted(at(1, 0)).text,
        if edited { "1904-01-09" } else { "1904-01-01" }
    );
    assert_eq!(wb.formatted(at(1, 1)).text, "1904-01-03 12:30:00.125");
    let Value::Num(days) = wb.get(at(1, 2)) else {
        panic!("date difference must be numeric")
    };
    assert!((days - (2.0 + 45000.125 / 86400.0 - start)).abs() < 1e-12);
    assert_eq!(
        wb.formatted(at(1, 2)).text,
        if edited { "-5.479" } else { "2.521" }
    );
    assert_eq!(wb.get(at(1, 3)), Value::Num(59.0));
    assert_eq!(wb.formatted(at(1, 3)).text, "1904-02-29");
    assert_eq!(wb.get(at(1, 4)), Value::Num(start + 1.0));
    assert_eq!(
        wb.formatted(at(1, 4)).text,
        if edited { "1904-01-10" } else { "1904-01-02" }
    );
    assert_eq!(wb.get(at(1, 5)), Value::Num(19_040_101.0 + start));
    assert_eq!(
        wb.get(at(1, 6)),
        Value::str(if edited {
            "Sat 1904-01-09"
        } else {
            "Fri 1904-01-01"
        })
    );
    assert_eq!(wb.get(at(1, 7)), Value::Num(if edited { 7.0 } else { 6.0 }));
    assert_eq!(wb.get(at(1, 8)), Value::Num(0.5));
    assert_eq!(wb.formatted(at(1, 8)).text, "12:00");
    assert_eq!(wb.get(at(1, 9)), Value::Num(60.0));
    assert_eq!(wb.formatted(at(1, 9)).text, "1904-03-01");
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir, source] if action == "make" => {
            let path = Path::new(dir);
            let mut wb = modern();
            fs::write(
                path.join("dates.xlsx"),
                iso_cells(&xlsx::write(&wb, Vec::new()).unwrap()),
            )
            .unwrap();
            wb.set_macros(macros(Path::new(source))).unwrap();
            fs::write(
                path.join("macro.xlsm"),
                iso_cells(&xlsx::write_xlsm(&wb, Vec::new()).unwrap()),
            )
            .unwrap();
            fs::write(path.join("legacy.xls"), legacy()).unwrap();
        }
        [action, file, state, format, source] if action == "check" => {
            let bytes = fs::read(file).unwrap();
            let wb = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else if file.ends_with(".xls") {
                let (wb, loaded) = xls::read(&bytes, None).unwrap();
                assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
                assert!(loaded.date1904);
                wb
            } else {
                let (wb, loaded) = xlsx::read(&bytes).unwrap();
                assert!(loaded.date1904);
                if format == "xlsm" {
                    assert_eq!(loaded.report.human(), [MACRO_NOTICE]);
                    assert_eq!(wb.macros(), Some(&macros(Path::new(source))));
                } else {
                    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
                }
                wb
            };
            assert!(matches!(state.as_str(), "initial" | "edited"));
            check(&wb, format == "xls", state == "edited");
        }
        _ => panic!(
            "usage: sheets_date_fixture make DIR VBA_SOURCE | check FILE STATE FORMAT VBA_SOURCE"
        ),
    }
}
