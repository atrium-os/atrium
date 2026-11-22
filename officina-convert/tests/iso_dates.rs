//! Import typed dates as numbers and preserve unsupported dates as text.

use officina_convert::xlsx;
use officina_convert::zip::{Archive, ZipWriter};
use officina_grid::eval::dates::DateSystem;
use officina_grid::style::Style;
use officina_grid::{CellAddr, Rect, Value, Workbook};

#[test]
fn typed_dates_use_the_workbook_epoch_and_keep_explicit_formats() {
    let at = |col| CellAddr::new(0, 0, col);
    for system in [DateSystem::Excel1900, DateSystem::Excel1904] {
        let mut wb = Workbook::with_date_system(system);
        wb.add_sheet("Sheet").unwrap();
        wb.set_value(at(0), Value::Num(1.0));
        wb.set_value(at(1), Value::Num(1.0));
        wb.set_value(at(2), Value::Num(1.0));
        wb.set_style(
            &Rect::cell(at(0)),
            Style::default().with_num_fmt("dd/mm/yyyy"),
        );
        let bytes = xlsx::write(&wb, Vec::new()).unwrap();
        let archive = Archive::open(&bytes).unwrap();
        let mut out = ZipWriter::new(Vec::new());
        for part in &archive.entries {
            let mut data = archive.read(part).unwrap();
            if part.name == "xl/worksheets/sheet1.xml" {
                let xml = String::from_utf8(data).unwrap();
                let start = xml.find("<row ").unwrap();
                let end = start + xml[start..].find("</row>").unwrap() + "</row>".len();
                let style = &xml[xml.find("<c r=\"A1\"").unwrap()..];
                let id = style
                    .split(r#" s=""#)
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap();
                let row = format!(
                    concat!(
                        r#"<row r="1"><c r="A1" s="{id}" t="d"><v>1904-02-29</v></c>"#,
                        r#"<c r="B1" t="d"><v>1904-01-02T00:30:00+01:00</v></c>"#,
                        r#"<c r="C1" t="d"><v>1904-02-30</v></c></row>"#,
                    ),
                    id = id
                );
                let mut patched = xml;
                patched.replace_range(start..end, &row);
                data = patched.into_bytes();
            }
            out.add(&part.name, &data).unwrap();
        }
        let (back, loaded) = xlsx::read(&out.finish().unwrap()).unwrap();
        assert_eq!(back.date_system(), system);
        let offset = if system == DateSystem::Excel1900 {
            1462.0
        } else {
            0.0
        };
        assert_eq!(back.get(at(0)), Value::Num(offset + 59.0));
        assert_eq!(back.formatted(at(0)).text, "29/02/1904");
        assert_eq!(back.get(at(1)), Value::Num(offset + 23.5 / 24.0));
        assert_eq!(back.formatted(at(1)).text, "1904-01-01 23:30:00");
        assert_eq!(back.get(at(2)), Value::str("1904-02-30"));
        assert_eq!(
            loaded.report.human(),
            ["approximate: invalid or out-of-range ISO date cells kept as text (e.g. C1)"]
        );
    }
}
