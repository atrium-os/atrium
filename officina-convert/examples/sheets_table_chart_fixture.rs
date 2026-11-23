//! Paired named ranges, filters, tables and charts for the installed Sheets gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, LegendPos, Series};
use officina_grid::filter::{AutoFilter, Cond};
use officina_grid::tables::Table;
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn rect(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
    Rect {
        sheet: 0,
        r0,
        c0,
        r1,
        c1,
    }
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Sales").unwrap();
    book.add_sheet("Check").unwrap();
    for (row, (group, units)) in [
        ("Region", "Units"),
        ("North", "10"),
        ("South", "5"),
        ("West", "7"),
    ]
    .into_iter()
    .enumerate()
    {
        book.set_input(at(row as u32, 0), group).unwrap();
        book.set_input(at(row as u32, 1), units).unwrap();
    }
    for (row, (name, units)) in [("Product", "Units"), ("Tea", "10"), ("Coffee", "4")]
        .into_iter()
        .enumerate()
    {
        book.set_input(at(row as u32, 4), name).unwrap();
        book.set_input(at(row as u32, 5), units).unwrap();
    }
    for (name, scope, definition) in [
        ("ForecastUnits", None, "=Sales!$F$2"),
        ("Rate", None, "=2"),
        ("Rate", Some(0), "=3"),
    ] {
        book.define_name(name, scope, definition).unwrap();
    }
    book.set_input(at(0, 3), "=Rate*ForecastUnits").unwrap();
    book.set_input(CellAddr::new(1, 0, 0), "=Rate*ForecastUnits")
        .unwrap();
    book.add_table(Table {
        name: "Orders".into(),
        rect: rect(0, 4, 2, 5),
        header: true,
        totals: false,
        columns: vec!["Product".into(), "Units".into()],
        style: Some("TableStyleMedium2".into()),
        stripes: true, filter: None,
    })
    .unwrap();
    book.set_input(at(1, 3), "=SUM(Orders[Units])").unwrap();
    let filter = AutoFilter {
        rect: rect(0, 0, 3, 1),
        conds: vec![(0, Cond::Values(vec!["North".into()]))],
    };
    book.recalc();
    book.replace_filter(book.filtered(0, Some(filter)));
    book.add_chart(Chart {
        kind: ChartKind::Column,
        grouping: Grouping::Standard,
        title: Some(ChartText::Lit("Orders by product".into())),
        series: vec![Series {
            name: Some(ChartText::Lit("Units shipped".into())),
            cats: Some(rect(1, 4, 2, 4)),
            vals: rect(1, 5, 2, 5),
            kind: None,
        }],
        legend: Some(LegendPos::Bottom),
        x_title: None,
        y_title: None,
        anchor: Anchor {
            sheet: 0,
            from: (0, 7, 0.0, 0.0),
            to: (16, 15, 0.0, 0.0),
        },
    })
    .unwrap();
    book
}

// The ODS importer can read the external typed-label extension. Our writer
// leaves it out for Calc compatibility, so add it to the input fixture.
fn external_ods(book: &Workbook) -> Vec<u8> {
    let package = ods::write(book, Vec::new()).unwrap();
    let archive = Archive::open(&package).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &archive.entries {
        let bytes = archive.read(entry).unwrap();
        let bytes = if entry.name == "Object 1/content.xml" {
            let xml = String::from_utf8(bytes).unwrap();
            assert!(xml.contains("<chart:series "));
            xml.replacen(
                "<chart:series ",
                r#"<chart:series xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" loext:label-string="Units shipped" "#,
                1,
            ).into_bytes()
        } else {
            bytes
        };
        out.add(&entry.name, &bytes).unwrap();
    }
    out.finish().unwrap()
}

fn check(book: &Workbook, format: &str, state: &str) {
    let (units, show_south, typed_label) = match state {
        "initial" => (10.0, false, true),
        "edited" => (8.0, false, format == "xlsx"),
        "filtered" => (8.0, true, format == "xlsx"),
        "restored" => (8.0, false, format == "xlsx"),
        _ => panic!("Unknown checkpoint {state}"),
    };

    assert_eq!(book.get(at(1, 5)), Value::Num(units));
    // Excel's internal filter name belongs to the XLSX representation.
    assert_eq!(book.names().len(), if format == "xlsx" { 4 } else { 3 });
    assert_eq!(
        book.names().get(0, "_xlnm._FilterDatabase").is_some(),
        format == "xlsx"
    );
    assert_eq!(
        book.get(at(0, 3)),
        Value::Num(3.0 * units),
        "sheet-scoped rate"
    );
    assert_eq!(
        book.value(1, 0, 0),
        Value::Num(2.0 * units),
        "workbook-scoped rate"
    );
    assert_eq!(book.get(at(1, 3)), Value::Num(units + 4.0), "table total");
    let table = book.table("Orders").unwrap();
    assert_eq!(table.rect, rect(0, 4, 2, 5));
    assert_eq!(table.columns[1].as_ref(), "Units");
    let formula = book.formula_text(at(1, 3)).unwrap();
    if format == "xlsx" {
        assert!(formula.contains("Orders[Units]"), "{formula}");
        assert_eq!(table.style.as_deref(), Some("TableStyleMedium2"));
        assert!(table.stripes);
    } else {
        assert!(formula.contains("$F$2:$F$3"), "{formula}");
        assert!(table.style.is_none() && !table.stripes);
    }
    let filter = book.filter_state(0);
    assert_eq!(
        filter.hidden,
        if show_south {
            vec![(3, 3)]
        } else {
            vec![(2, 3)]
        }
    );
    let filter = filter.filter.unwrap();
    assert_eq!(filter.rect, rect(0, 0, 3, 1));
    let expected = if show_south {
        vec!["North".into(), "South".into()]
    } else {
        vec!["North".into()]
    };
    assert_eq!(filter.conds, vec![(0, Cond::Values(expected))]);
    assert_eq!(book.charts().len(), 1);
    let chart = &book.charts()[0];
    assert_eq!(
        chart.title,
        Some(ChartText::Lit("Orders by product".into()))
    );
    assert_eq!(chart.kind, ChartKind::Column);
    let data = chart.data(book, 2);
    if show_south {
        assert_eq!(data[0].cats, ["Tea", "Coffee"]);
        assert_eq!(data[0].vals, [Some(units), Some(4.0)]);
    } else {
        assert_eq!(data[0].cats, ["Tea"]);
        assert_eq!(data[0].vals, [Some(units)]);
    }
    assert_eq!(
        chart.series[0].name,
        typed_label.then(|| ChartText::Lit("Units shipped".into()))
    );
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir] if action == "make" => {
            let book = fixture();
            let dir = Path::new(dir);
            fs::write(dir.join("sales.ods"), external_ods(&book)).unwrap();
            fs::write(
                dir.join("sales.xlsx"),
                xlsx::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [action, file, state, format] if action == "check" => {
            let bytes = fs::read(file).unwrap();
            let mut book = if file.ends_with(".ods") {
                ods::read(&bytes).unwrap().0
            } else if file.ends_with(".xlsx") {
                let (book, loaded) = xlsx::read(&bytes).unwrap();
                assert!(loaded
                    .report
                    .human()
                    .iter()
                    .any(|note| note.contains("table styles kept but not drawn")));
                book
            } else {
                officina_grid::native::load(&bytes).unwrap()
            };
            book.recalc();
            check(&book, format, state);
        }
        _ => panic!("Expected make DIR or check FILE STATE FORMAT"),
    }
}
