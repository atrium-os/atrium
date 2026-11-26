//! Pivot tables through .xlsx and .ods (docs/spec/officina.md §16.32): written as
//! Excel and LibreOffice write them, read back the same, with the same cells.

use officina_convert::zip::Archive;
use officina_convert::{ods, xlsx};
use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Rect, Value, Workbook};

pub fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Report").unwrap();
    let rows = [
        ["Region", "Rep", "Quarter", "Units", "Price"],
        ["North", "Ana", "Q1", "10", "2.5"],
        ["North", "Ana", "Q2", "20", "3"],
        ["North", "Bo", "Q1", "5", "4"],
        ["South", "Cy", "Q1", "7", "1"],
        ["South", "Cy", "Q2", "3", "2"],
        ["East", "Di", "Q2", "8", "5"],
        ["East", "Ed", "Q1", "", "1.5"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                wb.set_input(CellAddr::new(0, r as u32, c as u32), v).unwrap();
            }
        }
    }
    wb.recalc();
    let src = Rect { sheet: 0, r0: 0, c0: 0, r1: 7, c1: 4 };
    let mut p = Pivot::new("Units by region", src, CellAddr::new(1, 2, 1));
    p.rows = vec![PivotField::new(0), PivotField { hidden: vec![Value::str("Bo")], ..PivotField::new(1) }];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![DataField { field: 3, agg: Agg::Sum, name: None, show_as: Default::default() }];
    wb.add_pivot(p).unwrap();
    let mut q = Pivot::new("Prices", src, CellAddr::new(0, 0, 7));
    q.rows = vec![PivotField::new(1)];
    q.pages = vec![PivotField { hidden: vec![Value::str("South")], ..PivotField::new(0) }];
    q.data = vec![DataField { field: 4, agg: Agg::Average, name: Some("Mean price".into()), show_as: Default::default() }, DataField { field: 3, agg: Agg::Count, name: None, show_as: Default::default() }];
    q.subtotals = false;
    q.grand_col = false;
    wb.add_pivot(q).unwrap();
    wb.recalc();
    wb
}

fn part(file: &[u8], name: &str) -> String {
    let z = Archive::open(file).unwrap();
    let e = z.entries.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no {name}"));
    String::from_utf8(z.read(e).unwrap()).unwrap()
}

fn cells(wb: &Workbook) -> Vec<(u16, u32, u32, Value)> {
    let mut v = Vec::new();
    for s in 0..wb.sheet_count() as u16 {
        for r in 0..30 {
            for c in 0..12 {
                let x = wb.get(CellAddr::new(s, r, c));
                if !x.is_empty() {
                    v.push((s, r, c, x));
                }
            }
        }
    }
    v
}

#[test]
fn xlsx_round_trip() {
    let wb = book();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let t = part(&file, "xl/pivotTables/pivotTable1.xml");
    assert!(t.contains(r#"<location ref="B3:F12" firstHeaderRow="1" firstDataRow="2" firstDataCol="2"/>"#), "{t}");
    assert!(t.contains(r#"<pivotField axis="axisRow" compact="0" outline="0" showAll="0" sortType="ascending"><items count="6"><item x="0"/><item x="1" h="1"/>"#), "{t}");
    assert!(t.contains(r#"<rowFields count="2"><field x="0"/><field x="1"/></rowFields><colFields count="1"><field x="2"/></colFields><dataFields count="1"><dataField name="Sum of Units" fld="3" baseField="0" baseItem="0"/>"#), "{t}");
    let c = part(&file, "xl/pivotCache/pivotCacheDefinition1.xml");
    assert!(c.contains(r#"<worksheetSource ref="A1:E8" sheet="Data"/>"#), "{c}");
    assert!(c.contains(r#"<cacheField name="Region" numFmtId="0"><sharedItems count="3"><s v="East"/><s v="North"/><s v="South"/></sharedItems></cacheField>"#), "{c}");
    assert!(c.contains(r#"<cacheField name="Units" numFmtId="0"><sharedItems containsSemiMixedTypes="0" containsString="0" containsNumber="1" containsInteger="1" minValue="3" maxValue="20" containsBlank="1"/>"#), "{c}");
    let t2 = part(&file, "xl/pivotTables/pivotTable2.xml");
    assert!(t2.contains(r#"<pageFields count="1"><pageField fld="0" hier="-1"/></pageFields>"#), "{t2}");
    assert!(t2.contains(r#"<dataField name="Mean price" fld="4" subtotal="average" baseField="0" baseItem="0"/>"#), "{t2}");
    assert!(part(&file, "xl/workbook.xml").contains(r#"<pivotCaches><pivotCache cacheId="1" r:id="rIdPc1"/><pivotCache cacheId="2" r:id="rIdPc2"/></pivotCaches>"#));
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert_eq!(back.pivot_shape(1), wb.pivot_shape(1));
    assert_eq!(cells(&back), cells(&wb));
}

#[test]
fn xlsx_compact_round_trip() {
    let mut wb = book();
    let mut p = wb.pivots()[0].clone();
    p.compact = true;
    wb.set_pivot(0, p).unwrap();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let t = part(&file, "xl/pivotTables/pivotTable1.xml");
    assert!(!t.contains(r#"compact="0""#) && t.contains(r#"<pivotField axis="axisRow" showAll="0" sortType="ascending">"#), "{t}");
    assert!(part(&file, "xl/pivotTables/pivotTable2.xml").contains(r#"compact="0""#));
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert!(!wb.pivot_shape(0).unwrap().indents.is_empty());
    assert_eq!(cells(&back), cells(&wb));
}

#[test]
fn ods_round_trip() {
    let wb = book();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert_eq!(cells(&back), cells(&wb));
    // Compact form, as LibreOffice marks it.
    let mut wb = wb;
    let mut p = wb.pivots()[0].clone();
    p.compact = true;
    wb.set_pivot(0, p).unwrap();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = ods::read(&file).unwrap();
    assert!(back.pivots()[0].compact && !back.pivots()[1].compact);
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(cells(&back), cells(&wb));
}

#[test]
fn excel_display_modes_survive_save_and_refresh() {
    use officina_grid::pivot::{BaseItem, ReferenceCalc, ShowAs};

    let relative = |calc, item| ShowAs::Reference { calc, field: 1, item };
    for (mode, want) in [
        (ShowAs::PercentOfRow, 8.0 / 12.0),
        (ShowAs::PercentOfColumn, 8.0 / 14.0),
        (ShowAs::PercentOfTotal, 8.0 / 20.0),
        (ShowAs::Index, 160.0 / 168.0),
        (relative(ReferenceCalc::Difference, BaseItem::Previous), 4.0),
        (relative(ReferenceCalc::Percent, BaseItem::Named(Value::str("Q1"))), 2.0),
        (relative(ReferenceCalc::PercentDifference, BaseItem::Previous), 1.0),
        (ShowAs::RunningTotal(1), 12.0),
        (ShowAs::PercentOfParentRow, 8.0 / 14.0),
        (ShowAs::PercentOfParentColumn, 8.0 / 12.0),
        (ShowAs::PercentOfParent(1), 8.0 / 12.0),
        (ShowAs::PercentOfRunningTotal(1), 1.0),
        (ShowAs::Rank { field: 1, descending: false }, 2.0),
        (ShowAs::Rank { field: 1, descending: true }, 1.0),
    ] {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").unwrap();
        for (row, (region, quarter, units)) in (1..).zip([
            ("East", "Q1", 2.0), ("East", "Q2", 6.0),
            ("West", "Q1", 4.0), ("West", "Q2", 8.0),
        ]) {
            wb.set_value(CellAddr::new(0, row, 0), Value::str(region));
            wb.set_value(CellAddr::new(0, row, 1), Value::str(quarter));
            wb.set_value(CellAddr::new(0, row, 2), Value::Num(units));
        }
        let source = Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 2 };
        let mut p = Pivot::new("Display", source, CellAddr::new(0, 0, 4));
        p.rows.push(PivotField::new(0));
        p.cols.push(PivotField::new(1));
        p.data.push(DataField { field: 2, agg: Agg::Sum, name: None, show_as: mode.clone() });
        wb.add_pivot(p).unwrap();
        let (mut reopened, loaded) = xlsx::read(&xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
        assert!(loaded.report.is_empty(), "{mode:?} {:?}", loaded.report);
        assert_eq!(reopened.pivots()[0].data[0].show_as, mode);
        assert_eq!(reopened.get(CellAddr::new(0, 3, 6)), Value::Num(want), "{mode:?}");
        // Force a fresh result by writing an incorrect output. Refresh uses only
        // source records and the definition that survived Save.
        reopened.set_value(CellAddr::new(0, 3, 6), Value::Num(-9.0));
        reopened.refresh_pivot(0).unwrap();
        assert_eq!(reopened.get(CellAddr::new(0, 3, 6)), Value::Num(want), "{mode:?}");
    }
}

#[test]
fn ods_intervals_and_calendar_groups_survive_save_and_rebucket_source_edits() {
    use officina_grid::pivot::{DatePart, GroupField, GroupKind, ShowAs};

    let dates = officina_grid::eval::dates::DateSystem::Excel1900;
    let day = |m, d| dates.date(2026.0, m, d).unwrap();
    for (kind, input, moved, before, after) in [
        (GroupKind::Intervals { start: Some(0.0), end: Some(20.0), step: 10.0, dates: false },
            [3.0, 7.0, 20.0], 12.0, ["0-9", "10-20"], ["0-9", "10-20"]),
        (GroupKind::DateParts { start: Some(day(1.0, 1.0)), end: None, part: DatePart::Months },
            [day(1.0, 3.0), day(1.0, 9.0), day(2.0, 1.0)], day(2.0, 6.0), ["Jan", "Feb"], ["Jan", "Feb"]),
    ] {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").unwrap();
        wb.set_value(CellAddr::new(0, 0, 0), Value::str("Bucket"));
        wb.set_value(CellAddr::new(0, 0, 1), Value::str("Units"));
        for (i, (key, units)) in input.into_iter().zip([2.0, 6.0, 10.0]).enumerate() {
            let row = i as u32 + 1;
            wb.set_value(CellAddr::new(0, row, 0), Value::Num(key));
            wb.set_value(CellAddr::new(0, row, 1), Value::Num(units));
        }
        let source = Rect { sheet: 0, r0: 0, c0: 0, r1: 3, c1: 1 };
        let mut p = Pivot::new("Buckets", source, CellAddr::new(0, 0, 3));
        p.rows.push(PivotField::new(0));
        p.groups.push(GroupField { field: 0, source: 0, name: "Bucket".into(), kind });
        p.data.push(DataField { field: 1, agg: Agg::Sum, name: None, show_as: ShowAs::Normal });
        wb.add_pivot(p).unwrap();
        let (mut reopened, loaded) = ods::read(&ods::write(&wb, Vec::new()).unwrap()).unwrap();
        assert!(loaded.report.is_empty(), "{:?}", loaded.report);
        assert_eq!(reopened.pivots(), wb.pivots());
        let output = |wb: &Workbook, labels: [&str; 2], totals: [f64; 2]| {
            for (row, (label, value)) in (1..).zip(labels.into_iter().zip(totals)) {
                assert_eq!(wb.get(CellAddr::new(0, row, 3)), Value::str(label));
                assert_eq!(wb.get(CellAddr::new(0, row, 4)), Value::Num(value));
            }
        };
        output(&reopened, before, [8.0, 10.0]);
        reopened.set_value(CellAddr::new(0, 1, 0), Value::Num(moved));
        reopened.refresh_pivot(0).unwrap();
        output(&reopened, after, [6.0, 12.0]);
        let (reopened, _) = ods::read(&ods::write(&reopened, Vec::new()).unwrap()).unwrap();
        output(&reopened, after, [6.0, 12.0]);
    }
}

#[test]
fn ods_value_sort_and_running_totals_are_independent_under_column_parents() {
    use officina_grid::pivot::{AxisSort, ShowAs};

    let at = |r, c| CellAddr::new(0, r, c);
    let mut book = Workbook::new();
    book.add_sheet("Source").unwrap();
    for (col, header) in (0..).zip(["Region", "Store", "Sales"]) {
        book.set_value(at(0, col), Value::str(header));
    }
    for (row, (region, store, sales)) in (1..).zip([
        ("North", "A", 1.0), ("North", "B", 9.0), ("South", "A", 8.0), ("South", "B", 2.0),
    ]) {
        book.set_value(at(row, 0), Value::str(region));
        book.set_value(at(row, 1), Value::str(store));
        book.set_value(at(row, 2), Value::Num(sales));
    }
    let source = Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 2 };
    let mut pivot = Pivot::new("By region", source, at(0, 4));
    pivot.cols = vec![PivotField::new(0), PivotField {
        sort: AxisSort::Data { caption: "Sum - Sales".into(), descending: true },
        ..PivotField::new(1)
    }];
    pivot.data = vec![
        DataField { field: 2, agg: Agg::Sum, name: None, show_as: ShowAs::Normal },
        DataField { field: 2, agg: Agg::Sum, name: Some("Running total".into()), show_as: ShowAs::RunningTotal(1) },
    ];
    pivot.subtotals = false;
    pivot.grand_col = false;
    book.add_pivot(pivot).unwrap();
    let (mut book, loaded) = ods::read(&ods::write(&book, Vec::new()).unwrap()).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report);
    let check = |book: &Workbook, labels: [&str; 4], running: [f64; 4]| {
        assert_eq!([5, 7, 9, 11].map(|col| book.get(at(2, col))), labels.map(Value::str));
        assert_eq!([6, 8, 10, 12].map(|col| book.get(at(4, col))), running.map(Value::Num));
    };
    check(&book, ["B", "A", "A", "B"], [9.0, 10.0, 8.0, 10.0]);
    book.set_value(at(1, 2), Value::Num(10.0));
    book.refresh_pivot(0).unwrap();
    check(&book, ["A", "B", "A", "B"], [10.0, 19.0, 8.0, 10.0]);
}

/// Sales by date and amount for the grouping and slicer tests (§16.164).
fn dated_book() -> Workbook {
    let dates = officina_grid::eval::dates::DateSystem::Excel1900;
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    for (c, h) in ["Date", "Region", "Amount"].into_iter().enumerate() {
        wb.set_value(CellAddr::new(0, 0, c as u32), Value::str(h));
    }
    let rows = [((2024, 1, 5), "West", 10.0), ((2024, 2, 9), "East", 25.0), ((2024, 4, 2), "West", 40.0),
        ((2024, 8, 20), "East", 5.0), ((2025, 3, 1), "West", 70.0), ((2025, 11, 30), "East", 15.0)];
    for (i, ((y, m, d), region, amount)) in rows.into_iter().enumerate() {
        let r = i as u32 + 1;
        wb.set_value(CellAddr::new(0, r, 0), Value::Num(dates.date(y as f64, m as f64, d as f64).unwrap()));
        wb.set_value(CellAddr::new(0, r, 1), Value::str(region));
        wb.set_value(CellAddr::new(0, r, 2), Value::Num(amount));
    }
    wb.recalc();
    wb
}

/// Two pivots of `dated_book`: by years and quarters of a date grouped by
/// months, filtered by a Region slicer and a 2024 timeline; and by amounts
/// in bins of 25.
fn grouped_book() -> Workbook {
    use officina_grid::charts::Anchor;
    use officina_grid::pivot::{DatePart, GroupField, GroupKind, Slicer};
    use officina_grid::addr::MAX_COLS;

    let dates = officina_grid::eval::dates::DateSystem::Excel1900;
    let mut wb = dated_book();
    let src = Rect { sheet: 0, r0: 0, c0: 0, r1: 6, c1: 2 };
    // Years, then quarters (derived) over the date column grouped by months in place.
    let mut p = Pivot::new("By period", src, CellAddr::new(0, 0, 5));
    let (years, quarters) = (MAX_COLS, MAX_COLS + 1);
    p.groups = vec![
        GroupField { field: 0, source: 0, name: "Date".into(), kind: GroupKind::DateParts { start: None, end: None, part: DatePart::Months } },
        GroupField { field: years, source: 0, name: "Years".into(), kind: GroupKind::DateParts { start: None, end: None, part: DatePart::Years } },
        GroupField { field: quarters, source: 0, name: "Quarters".into(), kind: GroupKind::DateParts { start: None, end: None, part: DatePart::Quarters } },
    ];
    p.rows = vec![PivotField::new(years), PivotField { hidden: vec![Value::str("Q4")], ..PivotField::new(quarters) }];
    p.data = vec![DataField { field: 2, agg: Agg::Sum, name: None, show_as: Default::default() }];
    // A slicer on Region (not in the layout) showing only West; a timeline on Date.
    p.set_hidden(1, vec![Value::str("East")]);
    let (from, to) = (dates.date(2024.0, 1.0, 1.0).unwrap(), dates.date(2025.0, 1.0, 1.0).unwrap());
    p.set_range(0, Some((from, to)));
    p.slicers = vec![
        Slicer { name: "Region".into(), field: 1, anchor: Anchor { sheet: 0, from: (2, 12, 0.0, 0.0), to: (12, 14, 0.0, 0.0) }, timeline: None },
        Slicer { name: "Date".into(), field: 0, anchor: Anchor { sheet: 0, from: (14, 12, 0.0, 0.0), to: (18, 18, 0.0, 0.0) }, timeline: Some(DatePart::Months) },
    ];
    wb.add_pivot(p).unwrap();
    // Amounts in bins of 25 from 0.
    let mut q = Pivot::new("By amount", src, CellAddr::new(0, 20, 5));
    q.groups = vec![GroupField { field: 2, source: 2, name: "Amount".into(), kind: GroupKind::Intervals { start: Some(0.0), end: None, step: 25.0, dates: false } }];
    q.rows = vec![PivotField::new(2)];
    q.data = vec![DataField { field: 2, agg: Agg::Count, name: None, show_as: Default::default() }];
    wb.add_pivot(q).unwrap();
    wb.recalc();
    wb
}

#[test]
fn xlsx_date_and_interval_groups_filters_and_slicers_round_trip() {
    let wb = grouped_book();
    // West in 2024 only: Q1 10 and Q2 40.
    let at = |wb: &Workbook, r, c| wb.get(CellAddr::new(0, r, c));
    assert_eq!((at(&wb, 1, 5), at(&wb, 1, 6), at(&wb, 1, 7)), (Value::str("2024"), Value::str("Q1"), Value::Num(10.0)));
    assert_eq!((at(&wb, 2, 6), at(&wb, 2, 7), at(&wb, 3, 7)), (Value::str("Q2"), Value::Num(40.0), Value::Num(50.0)));

    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let c = part(&file, "xl/pivotCache/pivotCacheDefinition1.xml");
    assert!(c.contains(r#"containsDate="1""#) && c.contains(r#"<d v="2024-01-05T00:00:00"/>"#), "{c}");
    assert!(c.contains(r#"<rangePr groupBy="months" autoStart="1" autoEnd="1" startDate="2024-01-05T00:00:00" endDate="2025-11-30T00:00:00"/>"#), "{c}");
    assert!(c.contains(r#"<rangePr groupBy="quarters""#) && c.contains(r#"<rangePr groupBy="years""#), "{c}");
    let t = part(&file, "xl/pivotTables/pivotTable1.xml");
    assert!(t.contains(r#"<filter fld="0" type="dateBetween""#) && t.contains(r#"<of:slicer name="Region" fld="1" sheet="Data" from="2 12 0 0" to="12 14 0 0"/>"#), "{t}");
    assert!(t.contains(r#"<items count="3"><item x="0" h="1"/><item x="1"/>"#), "{t}");
    let c2 = part(&file, "xl/pivotCache/pivotCacheDefinition2.xml");
    assert!(c2.contains(r#"<rangePr autoStart="0" autoEnd="1" startNum="0" endNum="70" groupInterval="25"/>"#), "{c2}");

    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert_eq!(cells(&back), cells(&wb));
}

#[test]
fn ods_slicers_timelines_and_their_filters_round_trip() {
    let wb = grouped_book();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let content = part(&file, "content.xml");
    assert!(content.contains(r#"<officina:slicer officina:name="Region" officina:field="Region" officina:sheet="Data""#), "{content}");
    assert!(content.contains(r#"<table:data-pilot-member table:name="East" table:display="false""#), "{content}");
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    // LibreOffice lists grouped fields in its own order.
    let sorted = |wb: &Workbook| wb.pivots().iter().cloned().map(|mut p| { p.groups.sort_by_key(|g| g.field); p }).collect::<Vec<_>>();
    assert_eq!(sorted(&back), sorted(&wb));
    assert_eq!(cells(&back), cells(&wb));
}

/// Excel's own names for date groups ("Qtr1"), and an inclusive end on its
/// date filter, read as this engine's.
#[test]
fn excel_date_group_labels_and_inclusive_date_filters_read_as_ours() {
    use officina_convert::zip::ZipWriter;
    let wb = grouped_book();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let z = Archive::open(&file).unwrap();
    let mut w = ZipWriter::new(Vec::new());
    for e in &z.entries {
        let data = z.read(e).unwrap();
        let text = String::from_utf8_lossy(&data).into_owned();
        let text = match e.name.as_str() {
            "xl/pivotCache/pivotCacheDefinition1.xml" => (1..=4).fold(text, |t, q| t.replace(&format!(r#"<s v="Q{q}"/>"#), &format!(r#"<s v="Qtr{q}"/>"#))),
            "xl/pivotTables/pivotTable1.xml" => text.replace(r#"operator="lessThan" val="45658""#, r#"operator="lessThanOrEqual" val="45657""#),
            _ => { w.add(&e.name, &data).unwrap(); continue; }
        };
        w.add(&e.name, text.as_bytes()).unwrap();
    }
    let file = w.finish().unwrap();
    assert!(part(&file, "xl/pivotCache/pivotCacheDefinition1.xml").contains("Qtr4"));
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
}
