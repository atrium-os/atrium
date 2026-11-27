//! Tables: cells merged across and down, cell shading, rules per side,
//! padding and vertical alignment, row heights and header rows, a table
//! style's regions and the table's place (docs/spec/officina.md §16.172),
//! through .docx, .odt and the native file.

use officina_convert::{docx, odt};
use officina_text::model::{Border, BorderStyle, CellBorders, CellLook, CellPad, CellPaint, CellRef, HAlign, RegionLook, RowLook, TableBorders, TableLook, TableStyle, TextDoc, VAlign, LOOK_BANDED_ROWS, LOOK_HEADER};

pub fn tables_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    d.push_para("Before the table.", 0, vec![]);
    let red = Border { style: BorderStyle::Double, width: 6, color: Some(0xC00000), space: 0 };
    // Four rows of three columns (1440 each); row 0 a header; in row 1 the
    // first cell spans two columns; the last column's cell in rows 2 and 3
    // is one cell merged down.
    let rows: Vec<Vec<(&str, u32, u8, bool, CellLook)>> = vec![
        vec![("Name", 1440, 1, false, CellLook::default()), ("Kind", 1440, 1, false, CellLook::default()), ("Notes", 1440, 1, false, CellLook::default())],
        vec![("Wide cell", 2880, 2, false, CellLook { shading: Some(Some(0xFFF2CC)), valign: VAlign::Center, ..Default::default() }), ("Shaded", 1440, 1, false, CellLook { shading: Some(Some(0xDEEBF7)), ..Default::default() })],
        vec![("a", 1440, 1, false, CellLook { borders: CellBorders { bottom: Some(Some(red)), ..Default::default() }, ..Default::default() }), ("b", 1440, 1, false, CellLook { pad: Some(CellPad { top: 40, bottom: 40, start: 200, end: 200 }), ..Default::default() }), ("Merged down", 1440, 1, false, CellLook { valign: VAlign::Bottom, ..Default::default() })],
        vec![("c", 1440, 1, false, CellLook::default()), ("d", 1440, 1, false, CellLook::default()), ("", 1440, 1, true, CellLook { valign: VAlign::Bottom, ..Default::default() })],
    ];
    for (r, row) in rows.iter().enumerate() {
        let row_look = match r {
            0 => RowLook { header: true, cant_split: true, ..Default::default() },
            1 => RowLook { height: 720, ..Default::default() },
            2 => RowLook { height: 600, exact: true, ..Default::default() },
            _ => RowLook::default(),
        };
        for (c, (text, width, span, merged_up, look)) in row.iter().enumerate() {
            let k = d.push_para(text, 0, vec![]);
            let cell = CellRef { table: 1, row: r as u32, col: c as u16, width: *width, look: *look, row_look, merged_up: *merged_up, span: *span };
            d.blocks.update(k, |b| {
                let p = b.as_para_mut().unwrap();
                p.props.cell = Some(cell);
                p.props.space_after = Some(0);
            });
        }
    }
    d.push_para("After the table.", 0, vec![]);
    // ODF has no room between a cell's rule and its text: none here.
    let thin = Border { space: 0, ..Border::default() };
    let rule = Border { color: Some(0x4472C4), width: 8, ..thin };
    d.styles.tables.insert("Blue Grid".into(), TableStyle { borders: TableBorders { top: Some(rule), bottom: Some(rule), inside_h: Some(thin), ..Default::default() }, header: RegionLook { shading: Some(0x4472C4), bold: true, color: Some(0xFFFFFF), rule: Some(rule) }, band_row: Some(0xD9E2F3), ..Default::default() });
    d.tables.insert(1, TableLook { style: Some("Blue Grid".into()), align: HAlign::Center, look: LOOK_HEADER | LOOK_BANDED_ROWS, pad: Some(CellPad { top: 0, bottom: 0, start: 108, end: 108 }), ..Default::default() });
    d
}

/// What each cell shows: text, width, span, merged up, row look and how
/// it is drawn (resolved from its own look, its table's and the style's).
fn looks(d: &TextDoc) -> Vec<(String, u32, bool, RowLook, CellPaint, officina_text::model::CellPad)> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < d.blocks.len() {
        if d.cell(k).is_none() {
            out.push((d.para(k).unwrap().text.clone(), 0, false, RowLook::default(), CellPaint::default(), CellPad::default()));
            k += 1;
            continue;
        }
        let row = d.row_range(k);
        let paints = d.row_paints(row.clone(), 0);
        for (i, b) in row.clone().enumerate() {
            let c = d.cell(b).unwrap();
            out.push((d.para(b).unwrap().text.clone(), c.width, c.merged_up, c.row_look, paints[i], d.cell_pad(b)));
        }
        k = row.end;
    }
    out
}

#[test]
fn tables_round_trip_docx() {
    let d = tables_doc();
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(looks(&r), looks(&d));
    let t = r.tables.values().next().unwrap();
    assert_eq!((t.style.as_deref(), t.align, t.look), (Some("Blue Grid"), HAlign::Center, LOOK_HEADER | LOOK_BANDED_ROWS));
    assert_eq!(r.styles.tables.get("Blue Grid"), d.styles.tables.get("Blue Grid"));
    assert_eq!(r.cell(4).unwrap().span, 2, "merged across");
}

#[test]
fn tables_round_trip_odt() {
    let d = tables_doc();
    let r = odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap().0;
    // ODF keeps each cell's look; the style's name doesn't survive.
    let strip = |v: Vec<(String, u32, bool, RowLook, CellPaint, CellPad)>| v.into_iter().map(|mut x| {
        (x.4.bold, x.4.color) = (false, None);
        x
    }).collect::<Vec<_>>();
    assert_eq!(strip(looks(&r)), strip(looks(&d)));
    assert_eq!(r.tables.values().next().unwrap().align, HAlign::Center);
}

#[test]
fn tables_round_trip_native() {
    let d = tables_doc();
    let r = officina_text::native::load(&officina_text::native::save_to_vec(&d, &[])).unwrap().doc;
    assert_eq!(looks(&r), looks(&d));
    assert_eq!(r.tables, d.tables);
    assert_eq!(r.styles.tables, d.styles.tables);
}

/// One outer row of two cells; the first holds its text, then a 2x2
/// table, then more text; the second text, then a table.
fn nested_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let outer = |col: u16| CellRef { table: 1, row: 0, col, width: 4320, ..Default::default() };
    let mut push = |d: &mut TextDoc, text: &str, cell: CellRef, inner: Option<CellRef>| {
        let k = d.push_para(text, 0, vec![]);
        d.blocks.update(k, |b| {
            let p = b.as_para_mut().unwrap();
            p.props.cell = Some(cell);
            p.props.inner = inner;
            p.props.space_after = Some(0);
        });
    };
    push(&mut d, "Outer text", outer(0), None);
    let shaded = CellLook { shading: Some(Some(0xE2EFDA)), ..Default::default() };
    for r in 0..2u32 {
        for c in 0..2u16 {
            let look = if (r, c) == (1, 1) { shaded } else { CellLook::default() };
            push(&mut d, &format!("in {r}{c}"), outer(0), Some(CellRef { table: 2, row: r, col: c, width: 1800, look, ..Default::default() }));
        }
    }
    push(&mut d, "More outer text", outer(0), None);
    push(&mut d, "Second cell", outer(1), None);
    // A table that ends its cell (Word adds a paragraph after it; reading
    // drops that again).
    push(&mut d, "last", outer(1), Some(CellRef { table: 3, row: 0, col: 0, width: 1800, ..Default::default() }));
    // A table, then an empty paragraph that is the cell's own.
    push(&mut d, "third", outer(2), Some(CellRef { table: 4, row: 0, col: 0, width: 1800, ..Default::default() }));
    push(&mut d, "", outer(2), None);
    d.tables.insert(1, TableLook::default());
    d.tables.insert(2, TableLook::default());
    d
}

/// Each paragraph's text, whether it is in a table in a cell, and
/// that cell's row, column, width and shading.
fn nesting(d: &TextDoc) -> Vec<(String, Option<u16>, Option<(u32, u16, u32, Option<Option<u32>>)>)> {
    (0..d.blocks.len())
        .filter_map(|k| d.para(k))
        .filter(|p| !(p.text.is_empty() && p.props.cell.is_none()))
        .map(|p| (p.text.clone(), p.props.cell.map(|c| c.col), p.props.inner.map(|c| (c.row, c.col, c.width, c.look.shading.filter(|s| s.is_some())))))
        .collect()
}

#[test]
fn nested_tables_round_trip() {
    let d = nested_doc();
    let want = nesting(&d);
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    // Word ends a cell with a paragraph; the cell here ends in text already.
    assert_eq!(nesting(&r), want, "docx");
    let r = odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(nesting(&r), want, "odt");
    let r = officina_text::native::load(&officina_text::native::save_to_vec(&d, &[])).unwrap().doc;
    assert_eq!(nesting(&r), want, "native");
}
