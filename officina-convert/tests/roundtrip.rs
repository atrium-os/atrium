//! `.xlsx` out and back in: values, formulas (fills as shared formulas, other sheets,
//! newer functions), sizes, awkward text.

use officina_convert::xlsx;
use officina_grid::addr::CellAddr;
use officina_grid::{ErrorKind, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Sums & more").unwrap();
    for r in 0..500u32 {
        wb.set_value(CellAddr::new(0, r, 0), Value::Num(r as f64 * 1.5));
        wb.set_value(CellAddr::new(0, r, 1), Value::str(&format!("item {r}")));
    }
    wb.fill_formula(0, 2, 0, 499, "=A1*2").unwrap();
    wb.set_value(CellAddr::new(0, 0, 3), Value::str("  leading and trailing  "));
    wb.set_value(CellAddr::new(0, 1, 3), Value::str("<tags> & \"quotes\" _x0041_ \u{1F600} tab\there"));
    wb.set_value(CellAddr::new(0, 2, 3), Value::Bool(true));
    wb.set_value(CellAddr::new(0, 3, 3), Value::Err(ErrorKind::NA));
    wb.set_value(CellAddr::new(0, 4, 3), Value::Num(1e-300));
    wb.set_value(CellAddr::new(0, 5, 3), Value::Num(123456789012345678.0));
    wb.set_value(CellAddr::new(0, 6, 3), Value::Num(-0.1));
    wb.set_formula(CellAddr::new(1, 0, 0), "=SUM(Data!A1:A500)").unwrap();
    wb.set_formula(CellAddr::new(1, 1, 0), "=XLOOKUP(\"item 7\",Data!B1:B500,Data!A1:A500)").unwrap();
    wb.set_formula(CellAddr::new(1, 2, 0), "=IF(A1>0,\"big\",\"small\")").unwrap();
    wb.set_formula(CellAddr::new(1, 3, 0), "=1/0").unwrap();
    wb.set_formula(CellAddr::new(1, 4, 0), "=Data!D99").unwrap(); // an empty result
    wb.sheet_mut(0).col_sizes.set(1, 140.0);
    wb.sheet_mut(0).row_sizes.set(3, 40.0);
    wb.recalc();
    wb
}

#[test]
fn xlsx_round_trip() {
    let wb = book();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert_eq!(back.sheet_count(), 2);
    assert_eq!(back.sheet_name(1), "Sums & more");
    for s in 0..2u16 {
        let (rows, cols) = wb.extent(s);
        for r in 0..rows {
            for c in 0..cols {
                let a = CellAddr::new(s, r, c);
                assert_eq!(back.get(a), wb.get(a), "value at sheet {s} {r},{c}");
                assert_eq!(back.formula_text(a), wb.formula_text(a), "formula at sheet {s} {r},{c}");
            }
        }
    }
    // The fill came back as one group.
    assert_eq!(back.groups().filter(|(_, g)| g.sheet == 0).count(), 1);
    assert_eq!(back.sheet(0).col_sizes.size(1), 140.0);
    assert_eq!(back.sheet(0).row_sizes.size(3), 40.0);
    assert_eq!(loaded.differ, 0, "{:?}", loaded.report.human());
    assert_eq!(loaded.formulas, 505);
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
}

#[test]
fn hostile_xlsx_never_panics() {
    let file = xlsx::write(&charted(with_table(filtered(book()))), Vec::new()).unwrap();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    // Fewer in a debug build (it is ~30x slower); release runs them all.
    let n = if cfg!(debug_assertions) { 300 } else { 3000 };
    for i in 0..n {
        let mut m = file.clone();
        for _ in 0..1 + i % 8 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = (x % m.len() as u64) as usize;
            m[at] = (x >> 32) as u8;
        }
        let _ = xlsx::read(&m);
    }
    for cut in (0..file.len()).step_by(if cfg!(debug_assertions) { 997 } else { 97 }) {
        let _ = xlsx::read(&file[..cut]);
    }
}

/// Mutations of the XML itself (the archive's CRCs stop most byte flips before the
/// parser sees them): every part re-zipped with a few bytes changed.
#[test]
fn hostile_sheet_xml_never_panics() {
    use officina_convert::zip::{Archive, ZipWriter};
    let file = xlsx::write(&charted(with_table(filtered(book()))), Vec::new()).unwrap();
    let z = Archive::open(&file).unwrap();
    let parts: Vec<(String, Vec<u8>)> = z.entries.clone().into_iter().map(|e| (e.name.clone(), z.read(&e).unwrap())).collect();
    let mut x: u64 = 0xD1B5_4A32_D192_ED03;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    const TOKENS: &[&[u8]] = &[b"<", b">", b"/>", b"\"", b"&amp;", b"&#0;", b"r=\"A0\"", b"t=\"s\"", b"<v>99999999</v>", b"<row r=\"1\">", b"</row>", b"<c r=\"XFE1\">", b"si=\"7\"", b"t=\"shared\"", b"<f>", b"</f>", b"\xff\xfe", b"<c:ser>", b"</c:ser>", b"<xdr:col>99999999999</xdr:col>", b"<c:f>Data!$A$1:$C$9</c:f>", b"[#This Row]", b"<autoFilter ref=\"A1:XFD1048576\"/>"];
    let n = if cfg!(debug_assertions) { 150 } else { 1500 };
    for i in 0..n {
        let target = (next() % parts.len() as u64) as usize;
        let mut w = ZipWriter::new(Vec::new());
        w.level = 1;
        for (k, (name, data)) in parts.iter().enumerate() {
            let mut d = data.clone();
            if k == target && !d.is_empty() {
                for _ in 0..1 + i % 6 {
                    let at = (next() % d.len() as u64) as usize;
                    match next() % 3 {
                        0 => d[at] = next() as u8,
                        1 => {
                            let t = TOKENS[(next() % TOKENS.len() as u64) as usize];
                            d.splice(at..at, t.iter().copied());
                        }
                        _ => {
                            let n = ((next() % 40) as usize).min(d.len() - at);
                            d.drain(at..at + n);
                        }
                    }
                }
            }
            w.add(name, &d).unwrap();
        }
        let _ = xlsx::read(&w.finish().unwrap());
    }
}

#[test]
fn styles_round_trip() {
    use officina_grid::addr::Rect;
    use officina_grid::style::{Edge, HAlign, Line, Style};
    let mut wb = book();
    let mut money = Style::default().with_num_fmt("\"$\"#,##0.00");
    money.font.bold = true;
    money.fill = Some(0xE2EFDA);
    money.borders[3] = Edge { line: Line::Medium, color: Some(0x375623) };
    let date = Style::default().with_num_fmt("yyyy-mm-dd");
    let mut centred = Style::default();
    centred.h_align = HAlign::Center;
    centred.font.italic = true;
    // A whole column, part of one (past the data), one cell, and a column style with
    // unstyled cells inside it.
    wb.set_style(&Rect { sheet: 0, r0: 0, c0: 0, r1: 1_048_575, c1: 0 }, money.clone());
    wb.set_style(&Rect { sheet: 0, r0: 10, c0: 0, r1: 12, c1: 0 }, Style::default());
    wb.set_style(&Rect { sheet: 0, r0: 0, c0: 5, r1: 799, c1: 5 }, date.clone());
    wb.set_style(&Rect { sheet: 1, r0: 2, c0: 0, r1: 2, c1: 0 }, centred.clone());
    wb.set_value(CellAddr::new(0, 3, 5), Value::Num(46340.0));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    for (s, r, c) in [(0u16, 0u32, 0u32), (0, 9, 0), (0, 10, 0), (0, 12, 0), (0, 13, 0), (0, 600, 0), (0, 1_048_575, 0), (0, 3, 5), (0, 799, 5), (0, 800, 5), (1, 2, 0), (1, 3, 0), (0, 5, 1)] {
        let a = CellAddr::new(s, r, c);
        assert_eq!(back.style(a), wb.style(a), "style at {s} {r},{c}");
    }
    assert_eq!(back.formatted(CellAddr::new(0, 3, 5)).text, "2026-11-14");
    assert_eq!(back.formatted(CellAddr::new(0, 4, 0)).text, "$6.00");
    // A whole formatted column stays a few runs, and the file small.
    assert!(back.sheet(0).styles[0].runs().len() <= 3, "{:?}", back.sheet(0).styles[0].runs());
    assert!(file.len() < 200_000, "{} bytes", file.len());
}

#[test]
fn names_round_trip() {
    let mut wb = book();
    wb.define_name("Items", None, "=Data!$A$1:$A$500").unwrap();
    wb.define_name("Half", Some(1), "=0.5").unwrap();
    wb.define_name("Twice", None, "=SUM(Items)*2").unwrap();
    wb.define_name("_xlnm.Print_Area", Some(0), "=Data!$A$1:$D$20").unwrap();
    wb.set_formula(CellAddr::new(1, 10, 0), "=SUM(Items)*Half").unwrap();
    wb.set_formula(CellAddr::new(1, 11, 0), "=Twice").unwrap();
    wb.recalc();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.names().len(), 4);
    for (s, r) in [(1u16, 10u32), (1, 11)] {
        let a = CellAddr::new(s, r, 0);
        assert_eq!(back.get(a), wb.get(a));
        assert_eq!(back.formula_text(a), wb.formula_text(a));
    }
    assert_eq!(back.name_text(back.names().get(0, "_XLNM.PRINT_AREA").unwrap()), "=Data!$A$1:$D$20");
    assert!(back.names().get(0, "Half").is_none());
}

fn filtered(mut wb: Workbook) -> Workbook {
    use officina_grid::addr::Rect;
    use officina_grid::filter::{AutoFilter, Cond};
    let f = AutoFilter {
        rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 20, c1: 1 },
        conds: vec![
            (0, Cond::Compare(vec![(">".into(), 3.0), ("<".into(), 20.0)], true)),
            (1, Cond::Values(vec!["item 3".into(), "ITEM 5".into(), "item 9".into()])),
        ],
    };
    let mut st = wb.filtered(0, Some(f));
    st.hidden.push((100, 102)); // hidden by hand, outside the filter
    st.hidden = officina_grid::filter::merge(st.hidden);
    wb.replace_filter(st);
    wb
}

#[test]
fn filter_round_trip() {
    let wb = filtered(book());
    let want = wb.filter_state(0);
    assert_eq!(want.hidden.iter().map(|(a, b)| b - a + 1).sum::<u32>(), 20 - 3 + 3);
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.filter_state(0), want);
    assert_eq!(back.filter_state(1).filter, None);
    // The hidden _FilterDatabase name Excel wants comes and goes with the filter.
    assert!(back.names().get(0, "_xlnm._FilterDatabase").is_some());
    let again = xlsx::write(&back, Vec::new()).unwrap();
    assert_eq!(xlsx::read(&again).unwrap().0.names().len(), back.names().len());
}

fn with_table(mut wb: Workbook) -> Workbook {
    use officina_grid::addr::Rect;
    use officina_grid::tables::Table;
    // Data!F1:H6: header, four data rows, totals.
    for (c, h) in ["Item", "Qty", "Line total"].iter().enumerate() {
        wb.set_value(CellAddr::new(0, 0, 5 + c as u32), Value::str(h));
    }
    for r in 1..5u32 {
        wb.set_value(CellAddr::new(0, r, 5), Value::str(&format!("thing {r}")));
        wb.set_value(CellAddr::new(0, r, 6), Value::Num(r as f64));
    }
    wb.set_value(CellAddr::new(0, 5, 5), Value::str("Total"));
    wb.add_table(Table {
        name: "Orders".into(),
        rect: Rect { sheet: 0, r0: 0, c0: 5, r1: 5, c1: 7 },
        header: true,
        totals: true,
        columns: vec!["Item".into(), "Qty".into(), "Line total".into()],
        style: Some("TableStyleMedium2".into()),
        stripes: true,
    })
    .unwrap();
    wb.fill_formula(0, 7, 1, 4, "=[@Qty]*Data!A1").unwrap();
    wb.set_formula(CellAddr::new(0, 5, 7), "=SUM(Orders[Line total])").unwrap();
    wb.set_formula(CellAddr::new(1, 20, 0), "=SUM(Orders[Qty])+ROWS(Orders[#All])").unwrap();
    wb.recalc();
    wb
}

#[test]
fn table_round_trip() {
    let wb = with_table(book());
    assert_eq!(wb.get(CellAddr::new(1, 20, 0)), Value::Num(10.0 + 6.0));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert_eq!(loaded.report.human(), vec!["approximate: table styles kept but not drawn".to_string()]);
    assert_eq!(back.tables(), wb.tables());
    for a in [CellAddr::new(0, 2, 7), CellAddr::new(0, 5, 7), CellAddr::new(1, 20, 0)] {
        assert_eq!(back.get(a), wb.get(a), "{a:?}");
        assert_eq!(back.formula_text(a), wb.formula_text(a), "{a:?}");
    }
    assert_eq!(back.formula_text(CellAddr::new(0, 3, 7)).unwrap(), "=[@Qty]*Data!A3");
}

fn charted(mut wb: Workbook) -> Workbook {
    use officina_grid::addr::Rect;
    use officina_grid::charts::*;
    let col = |c: u32, r0: u32, r1: u32| Rect { sheet: 0, r0, c0: c, r1, c1: c };
    let anchor = |r: u32| Anchor { sheet: 1, from: (r, 2, 0.0, 0.0), to: (r + 15, 9, 6.0, 2.5) };
    wb.add_chart(Chart {
        kind: ChartKind::Column,
        grouping: Grouping::Standard,
        title: Some(ChartText::Lit("Items & <values>".into())),
        series: vec![
            Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, 3))), cats: Some(col(1, 0, 9)), vals: col(0, 0, 9), kind: None },
            Series { name: Some(ChartText::Lit("Doubled".into())), cats: Some(col(1, 0, 9)), vals: col(2, 0, 9), kind: Some(ChartKind::Line) },
        ],
        legend: Some(LegendPos::Bottom),
        x_title: Some("Item".into()),
        y_title: Some("Amount".into()),
        anchor: anchor(0),
    })
    .unwrap();
    for (i, (kind, grouping)) in [(ChartKind::Bar, Grouping::Stacked), (ChartKind::Area, Grouping::Percent), (ChartKind::Scatter, Grouping::Standard), (ChartKind::Pie, Grouping::Standard), (ChartKind::Doughnut, Grouping::Standard)].into_iter().enumerate() {
        wb.add_chart(Chart {
            kind,
            grouping,
            title: Some(ChartText::Lit(format!("{kind:?}").into())),
            series: vec![Series { name: None, cats: Some(col(0, 0, 4)), vals: col(2, 0, 4), kind: None }],
            legend: if kind.round() { Some(LegendPos::Right) } else { None },
            x_title: None,
            y_title: None,
            anchor: anchor(20 * (i as u32 + 1)),
        })
        .unwrap();
    }
    wb
}

#[test]
fn chart_round_trip() {
    let wb = charted(book());
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.charts().len(), wb.charts().len());
    for (a, b) in wb.charts().iter().zip(back.charts()) {
        assert_eq!(b, a);
        assert_eq!(b.data(&back, 100), a.data(&wb, 100));
    }
}
