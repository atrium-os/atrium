//! Generate and verify the paired workbooks used by sheets-interop-vm-check.sh.
use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::condfmt::{CmpOp, CondFormat, Dxf, Rule, RuleKind};
use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const FILL: u32 = 0xC6EFCE;

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Sales").unwrap();
    for (row, (group, units)) in [("Group", "Units"), ("Alpha", "10"), ("Beta", "4")]
        .into_iter()
        .enumerate()
    {
        book.set_input(at(row as u32, 0), group).unwrap();
        book.set_input(at(row as u32, 1), units).unwrap();
    }
    book.set_input(at(0, 3), "Spill").unwrap();
    book.set_input(at(0, 4), "LET").unwrap();
    book.set_input(at(0, 5), "LAMBDA").unwrap();
    book.set_input(at(1, 3), "=SEQUENCE(3,1,B2,1)").unwrap();
    book.set_input(at(1, 4), "=LET(units,B2,units*2)").unwrap();
    book.set_input(at(1, 5), "=LAMBDA(units,units+1)(B2)")
        .unwrap();
    book.recalc();
    let rule = Rule {
        kind: RuleKind::CellIs {
            op: CmpOp::Greater,
            a: book.cond_formula("9", 0, (1, 3)).unwrap(),
            b: None,
        },
        dxf: Dxf {
            fill: Some(FILL),
            ..Dxf::default()
        },
        stop: true,
    };
    book.add_cond_format(CondFormat {
        sheet: 0,
        ranges: vec![Rect {
            sheet: 0,
            r0: 1,
            c0: 3,
            r1: 3,
            c1: 3,
        }],
        rules: vec![rule],
    })
    .unwrap();
    let source = Rect {
        sheet: 0,
        r0: 0,
        c0: 0,
        r1: 2,
        c1: 1,
    };
    let mut pivot = Pivot::new("Sales totals", source, at(0, 7));
    pivot.rows.push(PivotField::new(0));
    pivot.data.push(DataField {
        field: 1,
        agg: Agg::Sum,
        name: None,
        show_as: Default::default(),
    });
    book.add_pivot(pivot).unwrap();
    book.recalc();
    book
}

fn check(book: &Workbook, units: f64, pivot_units: f64) {
    assert_eq!(book.value(0, 1, 1), Value::Num(units), "B2 source value");
    for row in 1..=3 {
        let value = units + f64::from(row - 1);
        assert_eq!(
            book.get(at(row, 3)),
            Value::Num(value),
            "D{} spill",
            row + 1
        );
        assert_eq!(
            book.look(at(row, 3)).dxf.fill,
            (value > 9.0).then_some(FILL),
            "D{} conditional fill",
            row + 1
        );
    }
    assert_eq!(book.value(0, 1, 4), Value::Num(units * 2.0), "E2 LET");
    assert_eq!(book.value(0, 1, 5), Value::Num(units + 1.0), "F2 LAMBDA");
    for (col, formula) in [(3, "SEQUENCE("), (4, "LET("), (5, "LAMBDA(")] {
        assert!(book.formula_text(at(1, col)).unwrap().contains(formula));
    }
    assert_eq!(book.cond_formats().len(), 1);
    assert_eq!(book.pivots().len(), 1);
    assert_eq!(book.pivots()[0].name.as_ref(), "Sales totals");
    assert_eq!(
        book.value(0, 1, 8),
        Value::Num(pivot_units),
        "I2 Alpha subtotal"
    );
    assert_eq!(book.value(0, 2, 8), Value::Num(4.0), "I3 Beta subtotal");
    assert_eq!(
        book.value(0, 3, 8),
        Value::Num(pivot_units + 4.0),
        "I4 grand total"
    );
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir] if action == "make" => {
            let book = fixture();
            check(&book, 10.0, 10.0);
            let dir = Path::new(dir);
            fs::write(
                dir.join("sales.ods"),
                ods::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(
                dir.join("sales.xlsx"),
                xlsx::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [action, file, state] if action == "check" => {
            let bytes = fs::read(file).unwrap();
            let mut book = if file.ends_with(".ods") {
                ods::read(&bytes).unwrap().0
            } else if file.ends_with(".xlsx") {
                xlsx::read(&bytes).unwrap().0
            } else {
                officina_grid::native::load(&bytes).unwrap()
            };
            book.recalc();
            let (units, pivot_units) = match state.as_str() {
                "initial" => (10.0, 10.0),
                "edited" => (8.0, 10.0),
                "refreshed" => (8.0, 8.0),
                _ => panic!("Unknown checkpoint {state}"),
            };
            check(&book, units, pivot_units);
        }
        _ => panic!("Expected make DIR or check FILE STATE"),
    }
}
