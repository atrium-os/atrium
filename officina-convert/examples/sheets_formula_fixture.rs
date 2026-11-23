//! Formula, date and error fidelity checkpoints for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::eval::dates::DateSystem;
use officina_grid::style::Style;
use officina_grid::tables::Table;
use officina_grid::value::ErrorKind;
use officina_grid::{CellAddr, Rect, Value, Workbook};

const DATE: f64 = 44883.0; // 2026-11-19 in the 1904 system.
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
    let mut book = Workbook::with_date_system(DateSystem::Excel1904);
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
    book.set_value(CellAddr::new(1, 0, 1), Value::str("Units"));
    book.add_table(Table {
        name: "InputTable".into(),
        rect: Rect {
            sheet: 1,
            r0: 0,
            c0: 1,
            r1: 1,
            c1: 1,
        },
        header: true,
        totals: false,
        columns: vec!["Units".into()],
        style: None,
        stripes: false,
        filter: None,
    })
    .unwrap();
    book.define_name("SpillTotal", None, "=SUM(Report!$D$2#)")
        .unwrap();
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
        (9, "=SUM(InputTable[Units])"),
        (10, "=SpillTotal"),
        (11, "=DATE(1904,1,1)"),
    ] {
        book.set_formula(at(1, col), formula).unwrap();
    }
    number_format(&mut book, at(1, 11), "yyyy-mm-dd");
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
    rewrite_xml(bytes, target, |xml| {
        assert_eq!(xml.matches(old).count(), 1, "C2 cache in {format}");
        xml.replace(old, new)
    })
}

fn rewrite_xml(bytes: &[u8], target: &str, rewrite: impl FnOnce(&str) -> String) -> Vec<u8> {
    let mut rewrite = Some(rewrite);
    let archive = Archive::open(bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for part in &archive.entries {
        let data = archive.read(part).unwrap();
        if part.name == target {
            let xml = String::from_utf8(data).unwrap();
            out.add(&part.name, rewrite.take().unwrap()(&xml).as_bytes())
                .unwrap();
        } else if part.name == "mimetype" {
            out.add_stored(&part.name, &data).unwrap();
        } else {
            out.add(&part.name, &data).unwrap();
        }
    }
    assert!(rewrite.is_none(), "missing {target}");
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
    assert_eq!(
        book.date_system(),
        DateSystem::Excel1904,
        "{format} {state}"
    );
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
    assert_eq!(book.formula_text(at(1, 4)).as_deref(), Some("=SUM(D2#)"));
    let sum = match rows {
        2 => 21.0,
        3 => 33.0,
        _ => 46.0,
    };
    assert_eq!(book.get(at(1, 4)), Value::Num(sum));
    assert_eq!(book.get(at(1, 9)), Value::Num(5.0));
    assert_eq!(
        book.formula_text(at(1, 9)).as_deref(),
        Some("=SUM(InputTable[Units])")
    );
    assert_eq!(book.get(at(1, 10)), Value::Num(sum));
    let name = book.names().get(0, "SpillTotal").unwrap();
    assert_eq!(book.name_text(name), "=SUM(Report!$D$2#)");
    assert_eq!(book.get(at(1, 11)), Value::Num(0.0));
    assert_eq!(book.formatted(at(1, 11)).text, "1904-01-01");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_resize_and_table_growth_recalculate_after_ods_reopen() {
        let mut book = ods::read(&ods::write(&workbook(), Vec::new()).unwrap())
            .unwrap()
            .0;
        let mut tables = book.tables().to_vec();
        tables[0].rect.r1 += 1;
        book.replace_tables(tables);
        book.set_value(CellAddr::new(1, 2, 1), Value::Num(7.0));
        book.set_value(at(1, 1), Value::Num(4.0));
        book.recalc();
        let (book, loaded) = ods::read(&ods::write(&book, Vec::new()).unwrap()).unwrap();
        assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
        assert_eq!(book.get(at(1, 4)), Value::Num(46.0));
        assert_eq!(book.get(at(1, 9)), Value::Num(12.0));
        assert_eq!(book.get(at(1, 10)), Value::Num(46.0));
    }

    #[test]
    fn edited_standard_formula_takes_precedence_over_retained_metadata() {
        let file = ods::write(&workbook(), Vec::new()).unwrap();
        let file = rewrite_xml(&file, "content.xml", |xml| {
            let expected = r#"table:formula="of:=SUM([.$D$2:.$D$3])""#;
            assert!(xml.contains(expected));
            xml.replacen(expected, r#"table:formula="of:=42""#, 1)
        });
        let (book, loaded) = ods::read(&file).unwrap();
        assert_eq!(book.formula_text(at(1, 4)).as_deref(), Some("=42"));
        assert_eq!(book.get(at(1, 4)), Value::Num(42.0));
        assert!(loaded
            .report
            .human()
            .iter()
            .any(|line| line.contains(STALE_RESULT)));
        assert_eq!(book.get(at(1, 10)), Value::Num(21.0));
    }

    #[test]
    fn unavailable_direct_functions_keep_caches_and_error_text_stays_text() {
        let mut source = Workbook::new();
        source.add_sheet("Results").unwrap();
        for (col, formula) in [
            (0, "=123"),
            (1, "=A1+1"),
            (2, r##"="#VALUE!""##),
            (3, "=NA()"),
        ] {
            source.set_formula(at(0, col), formula).unwrap();
        }
        source.recalc();
        for (format, file, part, original, other) in [
            (
                "ods",
                ods::write(&source, Vec::new()).unwrap(),
                "content.xml",
                r#"table:formula="of:=123""#,
                r#"table:formula="of:=VENDOR.RUN()""#,
            ),
            (
                "xlsx",
                xlsx::write(&source, Vec::new()).unwrap(),
                "xl/worksheets/sheet1.xml",
                "<f>123</f>",
                "<f>VENDOR.RUN()</f>",
            ),
        ] {
            let file = rewrite_xml(&file, part, |xml| {
                assert!(xml.contains(original));
                xml.replacen(original, other, 1)
            });
            let (book, loaded) = if format == "ods" {
                ods::read(&file).unwrap()
            } else {
                xlsx::read(&file).unwrap()
            };
            assert_eq!(book.get(at(0, 0)), Value::Num(123.0));
            assert_eq!(book.formula_text(at(0, 0)), None);
            assert_eq!(book.get(at(0, 1)), Value::Num(124.0));
            assert_eq!(book.get(at(0, 2)), Value::str("#VALUE!"));
            assert_eq!(book.get(at(0, 3)), Value::Err(ErrorKind::NA));
            let notes = loaded.report.human();
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains("kept as their values"), "{notes:?}");
        }
    }

    #[test]
    fn unavailable_array_formula_keeps_the_saved_followers() {
        let mut book = Workbook::new();
        book.add_sheet("Results").unwrap();
        book.set_formula(at(0, 0), "=SEQUENCE(2)").unwrap();
        book.set_formula(at(0, 1), "=A2+1").unwrap();
        book.recalc();
        for (format, file, xml_part) in [
            ("ods", ods::write(&book, Vec::new()).unwrap(), "content.xml"),
            (
                "xlsx",
                xlsx::write(&book, Vec::new()).unwrap(),
                "xl/worksheets/sheet1.xml",
            ),
        ] {
            let file = rewrite_xml(&file, xml_part, |xml| {
                assert!(xml.contains("SEQUENCE(2)"));
                xml.replace("SEQUENCE(2)", "VENDOR.RUN(2)")
            });
            let (reopened, loaded) = if format == "ods" {
                ods::read(&file).unwrap()
            } else {
                xlsx::read(&file).unwrap()
            };
            assert!(loaded
                .report
                .human()
                .iter()
                .any(|line| line.contains("kept as their values")));
            assert_eq!(reopened.get(at(0, 0)), Value::Num(1.0));
            assert_eq!(reopened.formula_text(at(0, 0)), None);
            assert_eq!(reopened.get(at(1, 0)), Value::Num(2.0));
            assert_eq!(reopened.get(at(0, 1)), Value::Num(3.0));
        }
    }

    #[test]
    fn typed_date_near_midnight_round_trips_without_a_24_hour_timestamp() {
        let mut book = Workbook::with_date_system(DateSystem::Excel1904);
        book.add_sheet("Times").unwrap();
        let timestamp = DATE + (86_400.0 - 0.0001) / 86_400.0;
        book.set_value(at(0, 0), Value::Num(timestamp));
        number_format(&mut book, at(0, 0), "yyyy-mm-dd hh:mm:ss.000");
        let file = ods::write(&book, Vec::new()).unwrap();
        let (book, loaded) = ods::read(&file).unwrap();
        assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
        assert_eq!(book.get(at(0, 0)), Value::Num(timestamp));
        let xml = Archive::open(&file)
            .unwrap()
            .read_name("content.xml")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(xml)
            .unwrap()
            .contains("2026-11-19T23:59:59."));
        let unstyled = rewrite_xml(&file, "content.xml", |xml| {
            let start = xml
                .find(r#"office:date-value="2026-11-19T23:59:59."#)
                .unwrap();
            let open = xml[..start].rfind("<table:table-cell").unwrap();
            let close = xml[open..start].find(" table:style-name=\"").unwrap() + open;
            let end = xml[close..].find('\"').unwrap() + close + 1;
            let end = xml[end..].find('\"').unwrap() + end + 1;
            format!("{}{}", &xml[..close], &xml[end..])
        });
        let (book, _) = ods::read(&unstyled).unwrap();
        assert_eq!(book.get(at(0, 0)), Value::Num(timestamp));
        assert_eq!(
            book.style(at(0, 0)).num_fmt.as_ref(),
            "yyyy-mm-dd hh:mm:ss.000"
        );
    }

    #[test]
    fn odf_timestamps_keep_zone_offsets_and_reject_invalid_calendar_days() {
        let file = ods::write(&workbook(), Vec::new()).unwrap();
        let file = rewrite_xml(&file, "content.xml", |xml| {
            assert!(xml.contains("2026-11-19T15:00:00"));
            assert!(xml.contains("2026-11-19T00:00:00"));
            xml.replacen("2026-11-19T15:00:00", "2026-11-19T17:15:00+02:15", 1)
                .replacen("2026-11-19T00:00:00", "2026-11-31T00:00:00", 1)
        });
        let (book, loaded) = ods::read(&file).unwrap();
        assert_eq!(book.get(at(2, 7)), Value::Num(DATE + 0.625));
        // The typed invalid date is preserved as the cell's readable text.
        assert!(matches!(book.get(CellAddr::new(1, 1, 0)), Value::Str(_)));
        assert!(loaded
            .report
            .human()
            .iter()
            .any(|s| s.contains("dates kept as text")));
    }
}
