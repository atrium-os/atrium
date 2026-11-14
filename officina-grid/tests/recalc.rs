//! Recalculation: incremental equals full, the vectorised path equals the general one,
//! fills that read themselves run in order, and cycles give #CIRC!.

use officina_grid::addr::{a1, col_name};
use officina_grid::{CellAddr, Value, Workbook};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const ROWS: u32 = 60;
const COLS: u32 = 6;

#[derive(Clone, Debug)]
enum Edit {
    Input(u32, u32, String),
    Fill(u32, u32, u32, String),
}

fn random_formula(g: &mut Rng, row: u32) -> String {
    let cell = |g: &mut Rng| {
        let c = g.below(COLS as u64) as u32;
        // Mostly nearby rows, as real sheets do; relative or absolute.
        let r = (row as i64 + g.below(7) as i64 - 3).clamp(0, ROWS as i64 - 1) as u32;
        if g.below(4) == 0 {
            format!("${}${}", col_name(c), r + 1)
        } else {
            a1(r, c)
        }
    };
    let range = |g: &mut Rng| {
        let c0 = g.below(COLS as u64) as u32;
        let c1 = (c0 + g.below(2) as u32).min(COLS - 1);
        let r0 = g.below(ROWS as u64) as u32;
        let r1 = (r0 + g.below(10) as u32).min(ROWS - 1);
        format!("{}:{}", a1(r0, c0), a1(r1, c1))
    };
    match g.below(9) {
        0 => format!("={}+{}", cell(g), cell(g)),
        1 => format!("={}*2-{}", cell(g), g.below(5)),
        2 => format!("=SUM({})", range(g)),
        3 => format!("=IF({}>3,{},{})", cell(g), cell(g), g.below(9)),
        4 => format!("=MAX({},{})", range(g), cell(g)),
        5 => format!("=COUNTIF({},\">2\")", range(g)),
        6 => format!("={}/{}", cell(g), cell(g)),
        7 => format!("=IFERROR({}&\"\",\"e\")", cell(g)),
        _ => format!("=SUMIF({},\"<5\")+{}", range(g), cell(g)),
    }
}

fn random_edit(g: &mut Rng) -> Edit {
    let r = g.below(ROWS as u64) as u32;
    let c = g.below(COLS as u64) as u32;
    match g.below(10) {
        0..=3 => Edit::Input(r, c, format!("{}", g.below(10))),
        4 => Edit::Input(r, c, ["", "x", "TRUE", "#N/A"][g.below(4) as usize].to_string()),
        5..=7 => Edit::Input(r, c, random_formula(g, r)),
        _ => {
            let r1 = (r + g.below(20) as u32).min(ROWS - 1);
            Edit::Fill(c, r, r1, random_formula(g, r))
        }
    }
}

fn apply(wb: &mut Workbook, e: &Edit) {
    match e {
        Edit::Input(r, c, s) => wb.set_input(CellAddr::new(0, *r, *c), s).unwrap(),
        Edit::Fill(c, r0, r1, s) => {
            wb.fill_formula(0, *c, *r0, *r1, s).unwrap();
        }
    }
}

fn snapshot(wb: &Workbook) -> Vec<Value> {
    let mut v = Vec::new();
    for r in 0..ROWS {
        for c in 0..COLS {
            v.push(wb.get(CellAddr::new(0, r, c)));
        }
    }
    v
}

#[test]
fn incremental_equals_full() {
    for seed in 1..=std::env::var("SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40u64) {
        let mut g = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.threads = 1 + (seed % 4) as usize;
        let mut log = Vec::new();
        for step in 0..60 {
            for _ in 0..1 + g.below(4) {
                let e = random_edit(&mut g);
                apply(&mut wb, &e);
                log.push(e);
            }
            wb.recalc();
            if step % 6 == 5 {
                let mut full = Workbook::new();
                full.add_sheet("S").unwrap();
                full.threads = 1;
                for e in &log {
                    apply(&mut full, e);
                }
                full.recalc_all();
                let (a, b) = (snapshot(&wb), snapshot(&full));
                for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                    let same = match (x, y) {
                        (Value::Num(p), Value::Num(q)) => p == q || (p - q).abs() <= 1e-9 * p.abs().max(q.abs()),
                        _ => x == y,
                    };
                    assert!(same, "seed {seed} step {step} cell {}: incremental {x:?}, full {y:?}", a1(i as u32 / COLS, i as u32 % COLS));
                }
            }
        }
    }
}

#[test]
fn vectorised_path_matches_general_semantics() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let n = 50_000u32;
    let vals: Vec<f64> = (0..n).map(|i| i as f64 * 0.25 - 100.0).collect();
    wb.set_numbers(0, 0, 0, &vals);
    // Cells the fast path must hand back to the general one.
    wb.set_input(CellAddr::new(0, 10, 0), "3").unwrap(); // a number still
    wb.set_input(CellAddr::new(0, 11, 0), "x").unwrap();
    wb.set_input(CellAddr::new(0, 12, 0), "").unwrap();
    wb.set_input(CellAddr::new(0, 13, 0), "TRUE").unwrap();
    wb.set_value(CellAddr::new(0, 14, 0), Value::str("4"));
    wb.fill_formula(0, 1, 0, n - 1, "=A1*2+1").unwrap();
    wb.fill_formula(0, 2, 0, n - 1, "=1/A1").unwrap();
    let st = wb.recalc();
    assert!(st.fast_cells > 0, "{st:?}");
    for r in 0..n {
        let b = wb.get(CellAddr::new(0, r, 1));
        let c = wb.get(CellAddr::new(0, r, 2));
        let (wb_, wc) = match r {
            10 => (Value::Num(7.0), Value::Num(1.0 / 3.0)),
            11 => (Value::Err(officina_grid::ErrorKind::Value), Value::Err(officina_grid::ErrorKind::Value)),
            12 => (Value::Num(1.0), Value::Err(officina_grid::ErrorKind::Div0)),
            13 => (Value::Num(3.0), Value::Num(1.0)),
            14 => (Value::Num(9.0), Value::Num(0.25)),
            _ => {
                let x = vals[r as usize];
                (Value::Num(x * 2.0 + 1.0), if x == 0.0 { Value::Err(officina_grid::ErrorKind::Div0) } else { Value::Num(1.0 / x) })
            }
        };
        assert_eq!(b, wb_, "B{}", r + 1);
        assert_eq!(c, wc, "C{}", r + 1);
    }
}

#[test]
fn running_sums_run_in_order() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let n = 100_000u32;
    wb.set_numbers(0, 0, 0, &vec![1.0; n as usize]);
    wb.set_input(CellAddr::new(0, 0, 1), "=A1").unwrap();
    wb.fill_formula(0, 1, 1, n - 1, "=B1+A2").unwrap(); // top-down
    wb.set_input(CellAddr::new(0, n - 1, 2), "=A100000").unwrap();
    wb.fill_formula(0, 2, 0, n - 2, "=C2+A1").unwrap(); // bottom-up
    wb.recalc();
    assert_eq!(wb.get(CellAddr::new(0, n - 1, 1)), Value::Num(n as f64));
    assert_eq!(wb.get(CellAddr::new(0, 0, 2)), Value::Num(n as f64));
    // One edit near the top moves every sum below it (B) and the top one (C).
    wb.set_input(CellAddr::new(0, 5, 0), "11").unwrap();
    wb.recalc();
    assert_eq!(wb.get(CellAddr::new(0, n - 1, 1)), Value::Num(n as f64 + 10.0));
    assert_eq!(wb.get(CellAddr::new(0, 4, 1)), Value::Num(5.0));
    assert_eq!(wb.get(CellAddr::new(0, 0, 2)), Value::Num(n as f64 + 10.0));
    assert_eq!(wb.get(CellAddr::new(0, 6, 2)), Value::Num(n as f64 - 6.0));
}

#[test]
fn cycles_give_circ_and_recover() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let set = |wb: &mut Workbook, a: &str, s: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    };
    let get = |wb: &Workbook, a: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.get(CellAddr::new(0, r, c))
    };
    set(&mut wb, "A1", "=B1+1");
    set(&mut wb, "B1", "=A1+1");
    set(&mut wb, "C1", "=A1*10");
    set(&mut wb, "D1", "=SUM(D2:D3)");
    set(&mut wb, "D2", "=D1");
    set(&mut wb, "E1", "=E1");
    set(&mut wb, "F1", "5");
    set(&mut wb, "F2", "=F1*2");
    wb.recalc();
    let circ = Value::Err(officina_grid::ErrorKind::Circ);
    for a in ["A1", "B1", "C1", "D1", "D2", "E1"] {
        assert_eq!(get(&wb, a), circ, "{a}");
    }
    assert_eq!(get(&wb, "F2"), Value::Num(10.0), "cells outside the cycle are untouched");
    set(&mut wb, "B1", "5");
    wb.recalc();
    assert_eq!(get(&wb, "A1"), Value::Num(6.0));
    assert_eq!(get(&wb, "C1"), Value::Num(60.0));
    // Circularity is judged by references, as in Excel: a branch not taken still counts.
    set(&mut wb, "G1", "1");
    set(&mut wb, "G2", "=IF(G1=1,G1,G2)");
    wb.recalc();
    assert_eq!(get(&wb, "G2"), circ);
    // A dependent that ignores the cycle's value is evaluated normally.
    set(&mut wb, "A1", "=B1+1");
    set(&mut wb, "B1", "=A1");
    set(&mut wb, "H1", "=IF(TRUE,7,A1)");
    wb.recalc();
    assert_eq!(get(&wb, "A1"), circ);
    assert_eq!(get(&wb, "H1"), Value::Num(7.0));
}

#[test]
fn volatile_formulas_recalculate_every_time() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.set_input(CellAddr::new(0, 0, 0), "=RAND()").unwrap();
    wb.set_input(CellAddr::new(0, 0, 1), "=A1+0").unwrap();
    wb.recalc();
    let a = wb.get(CellAddr::new(0, 0, 0));
    wb.recalc();
    let b = wb.get(CellAddr::new(0, 0, 0));
    assert_ne!(a, b);
    assert_eq!(wb.get(CellAddr::new(0, 0, 1)), b);
}
