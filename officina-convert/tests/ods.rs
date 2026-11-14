//! `.ods` out and back in (values, formulas, sizes, awkward text, styles), and hostile
//! `.ods` input.

use officina_convert::ods;
use officina_convert::zip::{Archive, ZipWriter};
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{Edge, HAlign, Line, Style};
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
    wb.set_value(CellAddr::new(0, 1, 3), Value::str("<tags> & \"quotes\" \u{1F600} tab\there\nsecond  line"));
    wb.set_value(CellAddr::new(0, 2, 3), Value::Bool(true));
    wb.set_value(CellAddr::new(0, 3, 3), Value::Err(ErrorKind::NA));
    wb.set_value(CellAddr::new(0, 4, 3), Value::Num(1e-300));
    wb.set_value(CellAddr::new(0, 5, 3), Value::Num(123456789012345678.0));
    wb.set_value(CellAddr::new(0, 6, 3), Value::Num(-0.1));
    wb.set_formula(CellAddr::new(1, 0, 0), "=SUM(Data!A1:A500)").unwrap();
    wb.set_formula(CellAddr::new(1, 1, 0), "=XLOOKUP(\"item 7\",Data!B1:B500,Data!A1:A500)").unwrap();
    wb.set_formula(CellAddr::new(1, 2, 0), "=IF(A1>0,\"big;\",\"small\")").unwrap();
    wb.set_formula(CellAddr::new(1, 3, 0), "=1/0").unwrap();
    wb.set_formula(CellAddr::new(1, 4, 0), "=Data!D99").unwrap(); // an empty result
    wb.set_formula(CellAddr::new(1, 5, 0), "=SUM({1,2;3,4})+'Sums & more'!A1").unwrap();
    wb.set_formula(CellAddr::new(1, 6, 0), "=AND(TRUE,ISNA(#N/A))").unwrap();
    wb.sheet_mut(0).col_sizes.set(1, 140.0);
    wb.sheet_mut(0).row_sizes.set(3, 40.0);
    wb.recalc();
    wb
}

#[test]
fn ods_round_trip() {
    let wb = book();
    let file = ods::write(&wb, Vec::new()).unwrap();
    assert_eq!(&file[30..38], b"mimetype");
    let (back, loaded) = ods::read(&file).unwrap();
    assert_eq!(back.sheet_count(), 2);
    assert_eq!(back.sheet_name(1), "Sums & more");
    for s in 0..2u16 {
        let (rows, cols) = wb.extent(s);
        for r in 0..rows {
            for c in 0..cols {
                let a = CellAddr::new(s, r, c);
                assert_eq!(back.get(a), wb.get(a), "value at sheet {s} {r},{c}");
                // A constant error comes back as a formula of just the error.
                if !matches!(wb.get(a), Value::Err(_)) || wb.formula_text(a).is_some() {
                    assert_eq!(back.formula_text(a), wb.formula_text(a), "formula at sheet {s} {r},{c}");
                }
            }
        }
    }
    assert_eq!(back.sheet(0).col_sizes.size(1), 140.0);
    assert_eq!(back.sheet(0).row_sizes.size(3), 40.0);
    assert_eq!(loaded.differ, 0, "{:?}", loaded.report.human());
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
}

#[test]
fn ods_styles_round_trip() {
    let mut wb = book();
    let mut money = Style::default().with_num_fmt("\"$\"#,##0.00;[Red]-\"$\"#,##0.00");
    money.font.bold = true;
    money.fill = Some(0xE2EFDA);
    money.borders[3] = Edge { line: Line::Medium, color: Some(0x375623) };
    let date = Style::default().with_num_fmt("yyyy-mm-dd");
    let mut centred = Style::default().with_num_fmt("0.0%");
    centred.h_align = HAlign::Center;
    centred.font.italic = true;
    centred.font.size = Some(14 * 20);
    centred.font.name = Some("DejaVu Serif".into());
    wb.set_style(&Rect { sheet: 0, r0: 0, c0: 0, r1: 1_048_575, c1: 0 }, money.clone());
    wb.set_style(&Rect { sheet: 0, r0: 10, c0: 0, r1: 12, c1: 0 }, Style::default());
    wb.set_style(&Rect { sheet: 0, r0: 0, c0: 5, r1: 799, c1: 5 }, date.clone());
    wb.set_style(&Rect { sheet: 1, r0: 2, c0: 0, r1: 2, c1: 0 }, centred.clone());
    wb.set_value(CellAddr::new(0, 3, 5), Value::Num(46340.0));
    wb.set_value(CellAddr::new(0, 4, 5), Value::Num(46340.75));
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    for (s, r, c) in [(0u16, 0u32, 0u32), (0, 9, 0), (0, 10, 0), (0, 12, 0), (0, 13, 0), (0, 600, 0), (0, 1_048_575, 0), (0, 3, 5), (0, 799, 5), (0, 800, 5), (1, 2, 0), (1, 3, 0), (0, 5, 1)] {
        let a = CellAddr::new(s, r, c);
        let (mut got, want) = (back.style(a).clone(), wb.style(a));
        assert_eq!(back.formatted(a).text, wb.formatted(a).text, "shown at {s} {r},{c}");
        got.num_fmt = want.num_fmt.clone();
        assert_eq!(&got, want, "style at {s} {r},{c}");
    }
    assert_eq!(back.formatted(CellAddr::new(0, 3, 5)).text, "2026-11-14");
    assert_eq!(back.get(CellAddr::new(0, 4, 5)), Value::Num(46340.75));
    assert_eq!(back.formatted(CellAddr::new(0, 4, 0)).text, "$6.00");
    // (The column's style ends at .ods's last row.)
    assert!(back.sheet(0).styles[0].runs().len() <= 4, "{:?}", back.sheet(0).styles[0].runs());
    assert!(file.len() < 200_000, "{} bytes", file.len());
}

fn rezip(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut w = ZipWriter::new(Vec::new());
    for (name, d) in parts {
        w.add(name, d).unwrap();
    }
    w.finish().unwrap()
}

#[test]
fn repeats_are_bounded() {
    // A cell with a value repeated across every column of every row: refused, not
    // expanded.
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="A"><table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="16384" office:value-type="float" office:value="1"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let file = rezip(&[("mimetype".into(), b"application/vnd.oasis.opendocument.spreadsheet".to_vec()), ("content.xml".into(), content.as_bytes().to_vec())]);
    let t = std::time::Instant::now();
    assert!(ods::read(&file).is_err());
    assert!(t.elapsed().as_secs() < 60);
    // Huge repeats of nothing are fine and cheap.
    let content = content.replace(r#" office:value-type="float" office:value="1""#, "");
    let file = rezip(&[("content.xml".into(), content.into_bytes())]);
    let (wb, _) = ods::read(&file).unwrap();
    assert_eq!(wb.extent(0), (0, 0));
}

#[test]
fn hostile_ods_never_panics() {
    let file = ods::write(&book(), Vec::new()).unwrap();
    let z = Archive::open(&file).unwrap();
    let parts: Vec<(String, Vec<u8>)> = z.entries.clone().into_iter().map(|e| (e.name.clone(), z.read(&e).unwrap())).collect();
    let mut x: u64 = 0xD1B5_4A32_D192_ED03;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    const TOKENS: &[&[u8]] = &[b"<", b">", b"/>", b"\"", b"&amp;", b"&#0;", b"table:number-columns-repeated=\"99999999\" ", b"table:number-rows-repeated=\"4000000000\" ", b"office:value-type=\"date\" office:date-value=\"-9999-99-99\" ", b"office:time-value=\"PT99999999999H\" ", b"table:formula=\"of:=[.A1\" ", b"<table:table-row>", b"</table:table-row>", b"<table:table-cell>", b"style:parent-style-name=\"ce1\" ", b"\xff\xfe"];
    let n = if cfg!(debug_assertions) { 150 } else { 1500 };
    for i in 0..n {
        let mut ps = parts.clone();
        let target = (next() % ps.len() as u64) as usize;
        let d = &mut ps[target].1;
        if d.is_empty() {
            continue;
        }
        for _ in 0..1 + i % 6 {
            if d.is_empty() {
                break;
            }
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
        let _ = ods::read(&rezip(&ps));
    }
    for cut in (0..file.len()).step_by(if cfg!(debug_assertions) { 997 } else { 97 }) {
        let _ = ods::read(&file[..cut]);
    }
}

#[test]
fn ods_names_round_trip() {
    let mut wb = book();
    wb.define_name("Items", None, "=Data!$A$1:$A$500").unwrap();
    wb.define_name("Half", Some(1), "=0.5").unwrap();
    wb.define_name("Twice", None, "=SUM(Items)*2").unwrap();
    wb.define_name("Next", None, "=B1").unwrap(); // relative: the cell to the right
    wb.set_formula(CellAddr::new(1, 10, 0), "=SUM(Items)*Half").unwrap();
    wb.set_formula(CellAddr::new(1, 11, 0), "=Twice").unwrap();
    wb.set_value(CellAddr::new(1, 12, 1), Value::Num(9.0));
    wb.set_formula(CellAddr::new(1, 12, 0), "=Next").unwrap();
    wb.recalc();
    assert_eq!(wb.get(CellAddr::new(1, 12, 0)), Value::Num(9.0));
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.names().len(), 4);
    for r in [10u32, 11, 12] {
        let a = CellAddr::new(1, r, 0);
        assert_eq!(back.get(a), wb.get(a));
        assert_eq!(back.formula_text(a), wb.formula_text(a));
    }
    assert!(back.names().get(0, "Half").is_none());
}

#[test]
fn ods_filter_round_trip() {
    use officina_grid::filter::{merge, AutoFilter, Cond};
    let mut wb = book();
    let f = AutoFilter {
        rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 20, c1: 1 },
        conds: vec![
            (0, Cond::Compare(vec![(">".into(), 3.0), ("<".into(), 20.0)], true)),
            (1, Cond::Values(vec!["item 3".into(), "ITEM 5".into(), "item 9".into()])),
        ],
    };
    let mut st = wb.filtered(0, Some(f));
    st.hidden.push((100, 102));
    st.hidden = merge(st.hidden);
    wb.replace_filter(st);
    let want = wb.filter_state(0);
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.filter_state(0), want);
    assert_eq!(back.filter_state(1).filter, None);
}
