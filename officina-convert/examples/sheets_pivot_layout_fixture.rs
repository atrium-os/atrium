//! Paired desktop regression for pivot layout and refreshed item order.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::pivot::{
    Agg, AxisSort, BaseItem, DataField, Pivot, PivotField, ReferenceCalc, ScopeItems, ShowAs,
    Subtotal,
};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const SOURCE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 5,
    c1: 3,
};
const LAYOUT: u32 = 5;
const SCOPE: u32 = 12;

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn measure(name: &str, agg: Agg, show_as: ShowAs) -> DataField {
    DataField {
        field: 1,
        agg,
        name: Some(name.into()),
        show_as,
    }
}

fn fixture(kind: &str) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Sales").unwrap();
    for (col, heading) in ["Region", "Score", "Team", "Product"]
        .into_iter()
        .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(heading));
    }
    for (row, (region, score, team, product)) in (1..).zip([
        ("East", 4.0, "A", "Alpha"),
        ("East", 7.0, "A", "Beta"),
        ("East", 5.0, "B", "Alpha"),
        ("West", 10.0, "C", "Alpha"),
        ("West", 6.0, "C", "Beta"),
    ]) {
        for (col, value) in [
            Value::str(region),
            Value::Num(score),
            Value::str(team),
            Value::str(product),
        ]
        .into_iter()
        .enumerate()
        {
            book.set_value(at(row, col as u32), value);
        }
    }
    let mut layout = Pivot::new("Regional layout", SOURCE, at(0, LAYOUT));
    layout.rows = vec![
        PivotField {
            outline: true,
            ..PivotField::new(0)
        },
        PivotField {
            outline: true,
            subtotal: Subtotal::Off,
            ..PivotField::new(2)
        },
        PivotField {
            sort: AxisSort::Data {
                caption: "Score total".into(),
                descending: true,
            },
            ..PivotField::new(3)
        },
    ];
    layout.values_on_rows = true;
    layout.data = vec![
        measure("Score total", Agg::Sum, ShowAs::Normal),
        measure(
            "Change",
            Agg::Sum,
            ShowAs::Reference {
                calc: ReferenceCalc::Difference,
                field: 3,
                item: BaseItem::Previous,
            },
        ),
    ];
    book.add_pivot(layout).unwrap();

    if kind == "xlsx" {
        let mut scope = Pivot::new("Positional scope", SOURCE, at(0, SCOPE));
        scope.rows.push(PivotField {
            sort: AxisSort::Selection {
                data: 0,
                descending: true,
                fields: vec![(3, ScopeItems::Positions(vec![0]))],
            },
            ..PivotField::new(0)
        });
        scope.cols.push(PivotField::new(3));
        scope.data = vec![
            measure("Score total", Agg::Sum, ShowAs::Normal),
            measure("Running", Agg::Sum, ShowAs::RunningTotal(0)),
        ];
        scope.subtotals = false;
        book.add_pivot(scope).unwrap();
    }
    book
}

fn write(book: &Workbook, kind: &str) -> Vec<u8> {
    match kind {
        "ods" => ods::write(book, Vec::new()).unwrap(),
        "xlsx" => xlsx::write(book, Vec::new()).unwrap(),
        _ => panic!("unknown format {kind}"),
    }
}

fn read(bytes: &[u8], kind: &str) -> Workbook {
    let (book, loaded) = match kind {
        "ods" => ods::read(bytes).unwrap(),
        "xlsx" => xlsx::read(bytes).unwrap(),
        _ => panic!("unknown format {kind}"),
    };
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    book
}

fn check(book: &Workbook, state: &str, kind: &str) {
    let refreshed = state == "refreshed";
    let edited = matches!(state, "edited" | "refreshed");
    assert_eq!(
        book.get(at(1, 4)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(7.0)
        }
    );
    assert_eq!(
        book.get(at(1, 1)),
        Value::Num(if edited { 8.0 } else { 4.0 })
    );
    let layout = &book.pivots()[0];
    assert!(layout.values_on_rows, "{kind} {state}");
    assert!(layout.rows[0].outline);
    assert!(layout.rows[1].outline);
    assert_eq!(layout.rows[1].subtotal, Subtotal::Off);
    assert_eq!(book.get(at(0, LAYOUT + 3)), Value::str("Values"));
    assert_eq!(book.get(at(1, LAYOUT)), Value::str("East"));
    assert_eq!(book.get(at(1, LAYOUT + 3)), Value::str("Score total"));
    assert_eq!(
        book.get(at(1, LAYOUT + 4)),
        Value::Num(if refreshed { 20.0 } else { 16.0 })
    );
    assert_eq!(book.get(at(3, LAYOUT + 1)), Value::str("A"));
    assert_eq!(book.get(at(3, LAYOUT + 4)), Value::Empty);
    assert_eq!(
        book.get(at(4, LAYOUT + 2)),
        Value::str(if refreshed { "Alpha" } else { "Beta" })
    );
    assert_eq!(book.get(at(5, LAYOUT + 3)), Value::str("Change"));
    assert_eq!(book.get(at(5, LAYOUT + 4)), Value::Empty);
    assert_eq!(
        book.get(at(6, LAYOUT + 2)),
        Value::str(if refreshed { "Beta" } else { "Alpha" })
    );
    assert_eq!(
        book.get(at(7, LAYOUT + 4)),
        Value::Num(if refreshed { -1.0 } else { -3.0 })
    );
    assert_eq!(book.get(at(8, LAYOUT + 1)), Value::str("B"));
    assert_eq!(book.get(at(8, LAYOUT + 4)), Value::Empty);

    if kind == "xlsx" {
        let scope = &book.pivots()[1];
        assert_eq!(
            scope.rows[0].sort,
            AxisSort::Selection {
                data: 0,
                descending: true,
                fields: vec![(3, ScopeItems::Positions(vec![0]))],
            }
        );
        assert_eq!(
            book.get(at(3, SCOPE)),
            Value::str(if refreshed { "East" } else { "West" })
        );
        assert_eq!(
            book.get(at(4, SCOPE)),
            Value::str(if refreshed { "West" } else { "East" })
        );
        assert_eq!(
            book.get(at(3, SCOPE + 2)),
            Value::Num(if refreshed { 13.0 } else { 10.0 })
        );
        assert_eq!(
            book.get(at(4, SCOPE + 2)),
            Value::Num(if refreshed { 23.0 } else { 19.0 })
        );
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            for kind in ["ods", "xlsx"] {
                fs::write(
                    Path::new(dir).join(format!("sales.{kind}")),
                    write(&fixture(kind), kind),
                )
                .unwrap();
            }
        }
        [command, file, state, kind] if command == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes, kind)
            };
            check(&book, state, kind);
        }
        _ => panic!("usage: sheets_pivot_layout_fixture make DIR | check FILE PHASE FORMAT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;
    use officina_grid::{native, ops::GridOp};

    #[test]
    fn paired_layouts_and_scope_keep_last_output_until_refresh() {
        for kind in ["ods", "xlsx"] {
            let reopen = |book: &Workbook| read(&write(book, kind), kind);
            let mut book = reopen(&fixture(kind));
            check(&book, "initial", kind);
            book.set_value(at(1, 4), Value::Num(7.0));
            book = reopen(&book);
            check(&book, "saved", kind);
            book.set_value(at(1, 1), Value::Num(8.0));
            book = native::load(&native::save_to_vec(&book)).unwrap();
            book = reopen(&book);
            check(&book, "edited", kind);
            let mut undo = Vec::new();
            for i in 0..book.pivots().len() {
                let refresh = book
                    .pivot_op(Some(i), Some(book.pivots()[i].clone()))
                    .unwrap();
                undo.push(GridOp::decode(&refresh.encode()).unwrap().apply(&mut book));
            }
            check(&reopen(&book), "refreshed", kind);
            let redo: Vec<_> = undo
                .into_iter()
                .rev()
                .map(|op| op.apply(&mut book))
                .collect();
            check(&book, "edited", kind);
            for op in redo.into_iter().rev() {
                op.apply(&mut book);
            }
            check(&book, "refreshed", kind);
        }
    }

    #[test]
    fn multi_member_scope_merges_the_selected_records_for_average() {
        let mut book = fixture("xlsx");
        let mut pivot = book.pivots()[1].clone();
        pivot.rows[0].sort = AxisSort::Selection {
            data: 0,
            descending: true,
            fields: vec![(
                3,
                ScopeItems::Members(vec![Value::str("Alpha"), Value::str("Beta")]),
            )],
        };
        pivot.data[0].agg = Agg::Average;
        // Gamma is outside the selection. The grand total would put East first
        // and an unweighted mean of the Alpha/Beta averages would do the same.
        book.set_value(at(2, 1), Value::Num(13.0));
        book.set_value(at(6, 0), Value::str("East"));
        book.set_value(at(6, 1), Value::Num(100.0));
        book.set_value(at(6, 3), Value::str("Gamma"));
        pivot.source.r1 = 6;
        book.pivot_op(Some(1), Some(pivot))
            .unwrap()
            .apply(&mut book);
        // East has three selected records at 22/3, West two at 16/2.
        assert_eq!(book.get(at(3, SCOPE)), Value::str("West"));
        let mut book = read(&write(&book, "xlsx"), "xlsx");
        assert!(
            matches!(&book.pivots()[1].rows[0].sort, AxisSort::Selection {
            fields, .. } if matches!(&fields[0].1, ScopeItems::Members(items) if items.len() == 2))
        );
        book.set_value(at(1, 1), Value::Num(20.0));
        book.refresh_pivot(1).unwrap();
        assert_eq!(book.get(at(3, SCOPE)), Value::str("East"));
    }

    #[test]
    fn absolute_position_follows_the_visible_opposite_axis() {
        let initial = fixture("xlsx");
        assert_eq!(initial.get(at(3, SCOPE)), Value::str("West"));
        let mut book = read(&write(&initial, "xlsx"), "xlsx");
        let mut pivot = book.pivots()[1].clone();
        pivot.cols[0].sort = AxisSort::Manual(vec![Value::str("Beta"), Value::str("Alpha")]);
        book.pivot_op(Some(1), Some(pivot))
            .unwrap()
            .apply(&mut book);
        let reopened = read(&write(&book, "xlsx"), "xlsx");
        assert_eq!(reopened.get(at(3, SCOPE)), Value::str("East"));
        assert_eq!(reopened.get(at(3, SCOPE + 1)), Value::Num(7.0));
    }

    #[test]
    fn percentage_formats_follow_vertical_measures_without_touching_numeric_rows() {
        let mut book = fixture("ods");
        let mut pivot = book.pivots()[0].clone();
        pivot.data[1].show_as = ShowAs::PercentOfRow;
        let explicit = officina_grid::style::Style::default().with_num_fmt("0.000");
        book.set_style(&Rect::cell(at(5, LAYOUT + 4)), explicit);
        let refresh = book.pivot_op(Some(0), Some(pivot)).unwrap();
        let undo = GridOp::decode(&refresh.encode()).unwrap().apply(&mut book);
        assert_eq!(book.style(at(4, LAYOUT + 4)).num_fmt.as_ref(), "General");
        assert_eq!(book.style(at(5, LAYOUT + 4)).num_fmt.as_ref(), "0.000");
        assert_eq!(book.style(at(7, LAYOUT + 4)).num_fmt.as_ref(), "0.00%");
        let reopened = read(&write(&book, "ods"), "ods");
        assert_eq!(reopened.style(at(7, LAYOUT + 4)).num_fmt.as_ref(), "0.00%");
        undo.apply(&mut book);
        assert_eq!(book.style(at(7, LAYOUT + 4)).num_fmt.as_ref(), "General");
    }

    #[test]
    fn an_unhandled_subtotal_function_leaves_the_saved_cells_in_place() {
        use officina_convert::zip::{Archive, ZipWriter};
        let book = fixture("xlsx");
        let file = write(&book, "xlsx");
        let zip = Archive::open(&file).unwrap();
        let mut out = ZipWriter::new(Vec::new());
        for entry in &zip.entries {
            let mut data = zip.read(entry).unwrap();
            if entry.name == "xl/pivotTables/pivotTable1.xml" {
                let xml = String::from_utf8(data).unwrap();
                data = xml
                    .replacen(r#"axis="axisRow""#, r#"axis="axisRow" avgSubtotal="1""#, 1)
                    .into_bytes();
            }
            out.add(&entry.name, &data).unwrap();
        }
        let bytes = out.finish().unwrap();
        let (back, loaded) = xlsx::read(&bytes).unwrap();
        assert_eq!(back.pivots().len(), 1);
        assert!(loaded
            .report
            .human()
            .iter()
            .any(|note| note.contains("custom pivot subtotal functions")));
        assert_eq!(back.get(at(1, LAYOUT + 4)), Value::Num(16.0));
    }

    #[test]
    fn outline_bottom_and_automatic_subtotal_survive_ods() {
        let mut book = fixture("ods");
        let mut pivot = book.pivots()[0].clone();
        pivot.rows[0].subtotal = Subtotal::Bottom;
        book.pivot_op(Some(0), Some(pivot))
            .unwrap()
            .apply(&mut book);
        let reopened = read(&write(&book, "ods"), "ods");
        let outer = &reopened.pivots()[0].rows[0];
        assert!(outer.outline);
        assert_eq!(outer.subtotal, Subtotal::Bottom);
        assert_eq!(reopened.get(at(1, LAYOUT)), Value::str("East"));
        assert_eq!(reopened.get(at(1, LAYOUT + 4)), Value::Empty);
        assert!((2..14).any(
            |row| reopened.get(at(row, LAYOUT)) == Value::str("East Total")
                && reopened.get(at(row, LAYOUT + 4)) == Value::Num(16.0)
        ));
    }
}
