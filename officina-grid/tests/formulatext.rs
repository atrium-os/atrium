use officina_grid::{
    protection::{ProtectionFormat, SheetProtection},
    style::Style,
    CellAddr, ErrorKind, Rect, Value, Workbook,
};

fn cell(r: u32, c: u32) -> CellAddr {
    CellAddr::new(0, r, c)
}

#[test]
fn source_dependencies_update_without_making_value_cycles() {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Tax").unwrap();
    wb.set_formula(cell(0, 0), "=FORMULATEXT(A1)").unwrap();
    wb.set_formula(cell(1, 0), "=FORMULATEXT(B2)").unwrap();
    wb.set_formula(cell(1, 1), "=LEN(A2)").unwrap();
    wb.set_formula(cell(2, 0), "=FORMULATEXT(A3)+A3").unwrap();
    wb.set_formula(cell(3, 0), "=FORMULATEXT(INDEX(A4:B4,2))")
        .unwrap();
    wb.set_formula(cell(3, 1), "=Tax!A1+5").unwrap();
    wb.define_name("Block", None, "=Data!$B$4:$B$5").unwrap();
    wb.set_formula(cell(4, 0), "=FORMULATEXT(Block)").unwrap();
    wb.set_formula(cell(5, 0), "=FORMULATEXT(1)").unwrap();
    wb.recalc();
    assert_eq!(wb.get(cell(0, 0)), Value::str("=FORMULATEXT(A1)"));
    assert_eq!(wb.get(cell(1, 0)), Value::str("=LEN(A2)"));
    assert_eq!(wb.get(cell(1, 1)), Value::Num(8.0));
    assert_eq!(wb.get(cell(2, 0)), Value::Err(ErrorKind::Circ));
    assert_eq!(wb.get(cell(3, 0)), Value::str("=Tax!A1+5"));
    assert_eq!(wb.get(cell(4, 0)), Value::str("=Tax!A1+5"));
    assert_eq!(wb.get(cell(5, 0)), Value::Err(ErrorKind::Value));

    wb.rename_sheet(1, "Tax Rates").unwrap();
    wb.recalc();
    assert_eq!(wb.get(cell(4, 0)), Value::str("='Tax Rates'!A1+5"));
    wb.set_formula(cell(3, 1), "=10/2").unwrap();
    wb.recalc();
    assert_eq!(wb.get(cell(4, 0)), Value::str("=10/2"));
    wb.set_value(cell(3, 1), Value::Num(5.0));
    wb.recalc();
    assert_eq!(wb.get(cell(4, 0)), Value::Err(ErrorKind::NA));
}

#[test]
fn reads_formula_bar_text_and_respects_visibility_and_length() {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.fill_formula(0, 0, 0, 1, "=B1/0").unwrap();
    wb.fill_formula(0, 2, 0, 1, "=FORMULATEXT(A1)").unwrap();
    wb.recalc();
    assert_eq!(wb.get(cell(1, 2)), Value::str("=B2/0"));
    wb.sheet_mut(0).protection = Some(SheetProtection::new(ProtectionFormat::Excel));
    wb.set_style(
        &Rect::cell(cell(1, 0)),
        Style {
            hide_formula: true,
            ..Default::default()
        },
    );
    wb.recalc();
    assert_eq!(wb.get(cell(0, 2)), Value::str("=B1/0"));
    assert_eq!(wb.get(cell(1, 2)), Value::Err(ErrorKind::NA));
    wb.set_style(&Rect::cell(cell(1, 0)), Style::default());
    wb.recalc();
    assert_eq!(wb.get(cell(1, 2)), Value::str("=B2/0"));
    // Fill keeps one AST. The row number grows by a digit in the last formula.
    let text = format!("=\"{}\"&B1", "a".repeat(8192 - 6));
    wb.fill_formula(0, 0, 0, 9, &text).unwrap();
    wb.fill_formula(0, 2, 0, 9, "=FORMULATEXT(A1)").unwrap();
    wb.recalc();
    assert_eq!(wb.get(cell(0, 2)), Value::str(&text));
    assert_eq!(wb.get(cell(9, 2)), Value::Err(ErrorKind::NA));
}
