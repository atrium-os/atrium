//! Themes, table styles, shapes and Camera pictures (docs/spec/officina.md §16.165).

use officina_core::history::Op;
use officina_core::wire::Reader;
use officina_grid::addr::Rect;
use officina_grid::charts::Anchor;
use officina_grid::look::{builtin_theme, Camera, Look, Shape, TablePart, TableStyleDef, HEADER, ROW_STRIPE};
use officina_grid::ops::GridOp;
use officina_grid::shift::{shift_op, Axis};
use officina_grid::style::Style;
use officina_grid::tables::{Table, TableBands};
use officina_grid::{CellAddr, Value, Workbook};

fn anchor(sheet: u16, r: u32, c: u32) -> Anchor {
    Anchor { sheet, from: (r, c, 0.0, 0.0), to: (r + 4, c + 3, 0.0, 0.0) }
}

fn shape(name: &str, sheet: u16) -> Shape {
    Shape { name: name.into(), anchor: anchor(sheet, 2, 2), geom: "roundRect".into(), fill: Some(0x4472C4), line: None, text: "Note".into(),
        text_color: Some(0xFFFFFF), text_size: 220, bold: true, alt: None }
}

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Other").unwrap();
    for r in 0..6 {
        wb.set_value(CellAddr::new(0, r, 0), Value::Num(r as f64));
    }
    wb
}

#[test]
fn a_theme_change_recolours_what_was_the_old_themes_and_undoes() {
    let mut wb = book();
    let accent1 = wb.theme().colors[4];
    let mut st = Style::default();
    st.fill = Some(accent1);
    st.font.color = Some(0x123456);
    st.font.name = Some("Calibri".into());
    wb.set_style(&Rect { sheet: 0, r0: 1, c0: 0, r1: 2, c1: 0 }, st);
    let new = builtin_theme("office 2007").unwrap();
    let op = wb.theme_op(new.clone());
    let undo = op.apply(&mut wb);
    let got = wb.style(CellAddr::new(0, 1, 0));
    assert_eq!(got.fill, Some(new.colors[4]));
    // Colours not from the theme stay; the body font follows the theme's.
    assert_eq!(got.font.color, Some(0x123456));
    assert_eq!(got.font.name.as_deref(), Some("Calibri"));
    assert_eq!(wb.theme().name.as_ref(), "Office 2007");
    assert_eq!(wb.style(CellAddr::new(0, 3, 0)).fill, None);
    undo.apply(&mut wb);
    assert_eq!(wb.style(CellAddr::new(0, 1, 0)).fill, Some(accent1));
    assert_eq!(wb.theme().name.as_ref(), "Office");

    // Fonts: a theme with other fonts renames the cells in the old ones.
    let grey = builtin_theme("Grayscale").unwrap();
    wb.theme_op(grey).apply(&mut wb);
    assert_eq!(wb.style(CellAddr::new(0, 1, 0)).font.name.as_deref(), Some("Arial"));
}

#[test]
fn table_styles_by_name_options_and_custom() {
    let mut wb = book();
    let t = Table { name: "T".into(), rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 2 }, header: true, totals: true,
        columns: vec!["A".into(), "B".into(), "C".into()], style: Some("TableStyleMedium2".into()), stripes: true, bands: TableBands::default(), filter: None };
    wb.add_table(t.clone()).unwrap();
    let accent1 = wb.theme().colors[4];
    let header = wb.table_cell_look(&t, 0, 1).unwrap();
    assert_eq!((header.fill, header.text, header.bold), (Some(accent1), Some(0xFFFFFF), true));
    // The first body row is a stripe, the second not.
    let (a, b) = (wb.table_cell_look(&t, 1, 1).unwrap(), wb.table_cell_look(&t, 2, 1).unwrap());
    assert!(a.fill.is_some() && b.fill.is_none(), "{a:?} {b:?}");
    let totals = wb.table_cell_look(&t, 5, 1).unwrap();
    assert!(totals.bold && totals.border.is_some());
    // First column set apart in a style that has one.
    let dark = Table { style: Some("TableStyleDark1".into()), bands: TableBands { first: true, ..Default::default() }, ..t.clone() };
    assert!(wb.table_cell_look(&dark, 2, 0).unwrap().bold && !wb.table_cell_look(&dark, 2, 1).unwrap().bold);
    assert!(wb.table_cell_look(&t, 6, 1).is_none());

    // A custom style wins over nothing else and must not reuse a built-in name.
    let mut look = wb.workbook_look().clone();
    let mut parts = [None; 7];
    parts[HEADER] = Some(TablePart { fill: Some(0x112233), text: None, bold: true, border: None });
    parts[ROW_STRIPE] = Some(TablePart { fill: Some(0xEEEEEE), ..Default::default() });
    look.table_styles.push(TableStyleDef { name: "Mine".into(), parts });
    GridOp::Look(Box::new(look.clone())).apply(&mut wb);
    let mine = Table { style: Some("mine".into()), ..t.clone() };
    assert_eq!(wb.table_cell_look(&mine, 0, 0).unwrap().fill, Some(0x112233));
    assert_eq!(wb.table_cell_look(&mine, 1, 0).unwrap().fill, Some(0xEEEEEE));
    look.table_styles.push(TableStyleDef { name: "TableStyleLight1".into(), parts });
    assert!(look.valid(2).is_err());
}

#[test]
fn shapes_and_cameras_ride_the_wire_follow_edits_and_go_with_their_sheet() {
    let mut wb = book();
    let mut look = wb.workbook_look().clone();
    look.shapes.push(shape("Box 1", 0));
    look.shapes.push(Shape { geom: "rect".into(), fill: None, line: None, ..shape("Text 1", 1) });
    look.cameras.push(Camera { name: "Camera 1".into(), anchor: anchor(1, 8, 1), source: Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 0 } });
    look.valid(2).unwrap();
    assert!(look.shapes[1].is_text_box());
    let op = GridOp::Look(Box::new(look.clone()));
    let bytes = op.encode();
    assert_eq!(GridOp::decode(&bytes).unwrap(), op);
    for cut in 0..bytes.len() {
        assert!(GridOp::decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    op.apply(&mut wb);

    // Saved and loaded.
    let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&wb)).unwrap();
    assert_eq!(back.workbook_look(), &look);

    // Rows inserted above: the shape moves down, the camera's source too.
    shift_op(&wb, 0, Axis::Rows, 0, 2).unwrap().apply(&mut wb);
    assert_eq!(wb.workbook_look().shapes[0].anchor.from.0, 4);
    assert_eq!(wb.workbook_look().cameras[0].source, Rect { sheet: 0, r0: 2, c0: 0, r1: 7, c1: 0 });
    assert_eq!(wb.workbook_look().cameras[0].anchor.from.0, 8);

    // Deleting the source sheet takes its shape and the camera reading it.
    officina_grid::sheetops::delete_sheet_op(&wb, 0).unwrap().apply(&mut wb);
    assert_eq!(wb.workbook_look().shapes.len(), 1);
    assert_eq!(wb.workbook_look().shapes[0].anchor.sheet, 0);
    assert!(wb.workbook_look().cameras.is_empty());

    // Out-of-bounds parts are refused.
    let mut bad = Look::default();
    bad.shapes.push(Shape { geom: "noSuchShape".into(), ..shape("X", 0) });
    assert!(bad.valid(1).is_err());
    let mut r = Reader::new(&[]);
    assert!(Look::decode(&mut r).is_err());
}
