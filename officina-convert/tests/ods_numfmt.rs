//! Excel number formats through `.ods` and back: the shown text stays the same
//! (accounting sections, blanks, fills, grouping, month names), and cell text
//! Excel keeps that ODF needs care with.

use officina_convert::ods;
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::Style;
use officina_grid::{Value, Workbook};

const CODES: &[&str] = &[
    r#"_(* #,##0_);_(* \(#,##0\);_(* "-"_);_(@_)"#,
    r#"_("$"* #,##0.00_);_("$"* \(#,##0.00\);_("$"* "-"??_);_(@_)"#,
    r#"_-* #,##0.00\ "€"_-;\-* #,##0.00\ "€"_-;_-* "-"??\ "€"_-;_-@_-"#,
    r#"#,##0;[Red]\-#,##0"#,
    r#"#,##0.00_ ;\-#,##0.00\ "#,
    "mmmm",
    "d mmmm yyyy",
    "# ???/???",
    "0.00",
    "#,##0",
    "[=0]?;0%",
];
const VALUES: &[f64] = &[12126000.0, -2882.82, 0.0, 46.4, 1.0];

#[test]
fn accounting_and_padded_formats_show_the_same_after_ods() {
    let mut wb = Workbook::new();
    wb.add_sheet("F").unwrap();
    for (c, code) in CODES.iter().enumerate() {
        wb.set_style(&Rect { sheet: 0, r0: 0, c0: c as u32, r1: VALUES.len() as u32, c1: c as u32 }, Style::default().with_num_fmt(code));
        for (r, v) in VALUES.iter().enumerate() {
            wb.set_value(CellAddr::new(0, r as u32, c as u32), Value::Num(*v));
        }
        wb.set_value(CellAddr::new(0, VALUES.len() as u32, c as u32), Value::str("Median"));
    }
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = ods::read(&file).unwrap();
    let mut bad = Vec::new();
    for c in 0..CODES.len() as u32 {
        for r in 0..=VALUES.len() as u32 {
            let a = CellAddr::new(0, r, c);
            let (want, got) = (wb.formatted(a).text.to_string(), back.formatted(a).text.to_string());
            if want != got {
                bad.push(format!("{} r{r}: {want:?} vs {got:?} (back as {:?})", CODES[c as usize], back.style(a).num_fmt));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn elapsed_seconds_locale_tags_empty_strings_and_crlf_survive_ods() {
    let mut wb = Workbook::new();
    wb.add_sheet("F").unwrap();
    for (c, code) in ["[ss].00", "[mm]:ss", "[DBNum1][$-804]General", "[h]:mm:ss"].iter().enumerate() {
        wb.set_style(&Rect { sheet: 0, r0: 0, c0: c as u32, r1: 0, c1: c as u32 }, Style::default().with_num_fmt(code));
        wb.set_value(CellAddr::new(0, 0, c as u32), Value::Num(3.141592653589793));
    }
    wb.set_value(CellAddr::new(0, 1, 0), Value::str(""));
    wb.set_formula(CellAddr::new(0, 1, 1), "=ISTEXT(A2)").unwrap();
    wb.set_value(CellAddr::new(0, 2, 0), Value::str("Line 1\r\nLine 2\r\n"));
    wb.set_formula(CellAddr::new(0, 2, 1), "=\"Select\r\n(one)\"").unwrap();
    wb.recalc();
    assert_eq!(wb.get(CellAddr::new(0, 2, 1)), Value::str("Select\r\n(one)"));
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (mut back, _) = ods::read(&file).unwrap();
    back.recalc();
    for c in 0..4 {
        let a = CellAddr::new(0, 0, c);
        assert_eq!(back.formatted(a).text, wb.formatted(a).text, "{:?}", back.style(a).num_fmt);
    }
    assert_eq!(back.get(CellAddr::new(0, 1, 0)), Value::str(""));
    assert_eq!(back.formatted(CellAddr::new(0, 1, 1)).text, "TRUE");
    assert_eq!(back.get(CellAddr::new(0, 2, 0)), Value::str("Line 1\r\nLine 2\r\n"));
    assert_eq!(back.get(CellAddr::new(0, 2, 1)), Value::str("Select\r\n(one)"));
}
