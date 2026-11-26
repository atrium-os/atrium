//! Evaluate Formula (docs/spec/officina.md §16.142): a formula worked out
//! one part at a time, the leftmost innermost part first, as Excel's
//! Evaluate Formula dialog shows it. Each step replaces one reference,
//! operation or call by its value and prints the formula again.
//!
//! A part whose value can't stand in its place unchanged is worked out
//! whole with its parent instead: an empty cell (it reads as 0 or "" by
//! use), a range over [`MAX_SHOWN`] cells, and the arguments of functions
//! that read references or choose what to evaluate ([`WHOLE`]).

use super::{funcs::FUNCS, Ctx, Ev};
use crate::addr::CellAddr;
use crate::book::Workbook;
use crate::formula::{Expr, Func};
use crate::value::Value;

/// The most cells a range may have to be shown as an array constant.
pub const MAX_SHOWN: usize = 64;
/// The most steps returned.
pub const MAX_STEPS: usize = 256;

/// Functions worked out as one step: they read references (ROW, OFFSET,
/// the *IF family, SUBTOTAL…), evaluate only some arguments (IF, CHOOSE…),
/// or take functions as values.
pub const WHOLE: &[&str] = &[
    "IF", "IFS", "IFERROR", "IFNA", "CHOOSE", "SWITCH", "LET", "LAMBDA", "ROW", "ROWS", "COLUMN", "COLUMNS",
    "OFFSET", "INDIRECT", "INDEX", "ISREF", "CELL", "AREAS", "SUBTOTAL", "AGGREGATE", "SUMIF", "SUMIFS",
    "COUNTIF", "COUNTIFS", "AVERAGEIF", "AVERAGEIFS", "MAXIFS", "MINIFS", "COUNTBLANK", "FORMULATEXT",
    "ISFORMULA", "SHEET", "SHEETS", "HYPERLINK", "MAP", "REDUCE", "SCAN", "BYROW", "BYCOL", "MAKEARRAY",
    "GROUPBY", "PIVOTBY", "ISBLANK", "TRIMRANGE", "XLOOKUP", "LOOKUP", "VLOOKUP", "HLOOKUP", "MATCH", "XMATCH",
    "DSUM", "DAVERAGE", "DCOUNT", "DCOUNTA", "DGET", "DMAX", "DMIN", "DPRODUCT", "DSTDEV", "DSTDEVP", "DVAR", "DVARP",
];

enum Step {
    /// One part was replaced.
    Done,
    /// Everything here is a value already.
    Value,
    /// This part can't be shown by value: its parent goes whole.
    Blocked,
}

fn literal(e: &Expr) -> bool {
    match e {
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Err(_) => true,
        Expr::Array(rows) => rows.iter().flatten().all(literal),
        _ => false,
    }
}

fn of_value(v: Value) -> Option<Expr> {
    Some(match v {
        Value::Num(x) => Expr::Num(x),
        Value::Str(s) => Expr::Str(s),
        Value::Bool(b) => Expr::Bool(b),
        Value::Err(k) => Expr::Err(k),
        Value::Empty => return None,
    })
}

/// `e` worked out at the formula's place, as a constant that can stand there.
fn constant(c: &Ctx, e: &Expr) -> Option<Expr> {
    match c.eval(e) {
        Ev::V(v) => of_value(v),
        Ev::R(r) if r.cells() == 1 => of_value(c.wb.value(r.sheet, r.r0, r.c0)),
        Ev::R(r) if r.cells() <= MAX_SHOWN as u64 => {
            let mut rows = Vec::new();
            for row in r.r0..=r.r1 {
                let mut out = Vec::new();
                for col in r.c0..=r.c1 {
                    out.push(of_value(c.wb.value(r.sheet, row, col))?);
                }
                rows.push(out);
            }
            Some(Expr::Array(rows))
        }
        Ev::A(a) if a.vals.len() == 1 => of_value(a.vals[0].clone()),
        Ev::A(a) if a.vals.len() <= MAX_SHOWN && a.cols > 0 => {
            let vals: Option<Vec<Expr>> = a.vals.iter().cloned().map(of_value).collect();
            Some(Expr::Array(vals?.chunks(a.cols).map(<[Expr]>::to_vec).collect()))
        }
        _ => None,
    }
}

fn whole(f: &Func) -> bool {
    match f {
        Func::Known(id) => FUNCS.get(*id as usize).is_some_and(|s| WHOLE.contains(&s.name)),
        Func::Unknown(_) => true,
    }
}

/// Replaces `e` by its value when it can stand there.
fn settle(c: &Ctx, e: &mut Expr) -> Step {
    match constant(c, e) {
        Some(k) => {
            *e = k;
            Step::Done
        }
        None => Step::Blocked,
    }
}

/// One step on `e`: its leftmost innermost part replaced by its value.
fn step(c: &Ctx, e: &mut Expr) -> Step {
    if literal(e) {
        return Step::Value;
    }
    match e {
        Expr::Paren(inner) => {
            let r = step(c, inner);
            if literal(inner) {
                *e = (**inner).clone();
            }
            r
        }
        Expr::Neg(x) | Expr::Plus(x) | Expr::Pct(x) | Expr::At(x) => match step(c, x) {
            Step::Value => settle(c, e),
            Step::Blocked => settle(c, e),
            Step::Done => Step::Done,
        },
        Expr::Bin(_, a, b) => match step(c, a) {
            Step::Done => Step::Done,
            Step::Blocked => settle(c, e),
            Step::Value => match step(c, b) {
                Step::Done => Step::Done,
                _ => settle(c, e),
            },
        },
        Expr::Call(f, args) if !whole(f) => {
            for a in args.iter_mut() {
                match step(c, a) {
                    Step::Value => continue,
                    Step::Done => return Step::Done,
                    Step::Blocked => break,
                }
            }
            settle(c, e)
        }
        _ => settle(c, e),
    }
}

/// The steps of the formula at `a`: the formula as written, then after each
/// part is worked out, at most [`MAX_STEPS`]; the last is the value. `None`
/// when the cell holds no formula.
pub fn evaluate_steps(wb: &Workbook, a: CellAddr) -> Option<Vec<String>> {
    let g = wb.group_at(a.sheet, a.row, a.col)?;
    let mut e: Expr = (*wb.group(g).expr).clone();
    let c = Ctx::new(wb, a.sheet, a.row, a.col);
    let print = |e: &Expr| crate::formula::to_a1(e, a.row, a.col, &|s| wb.sheet_name(s));
    let mut out = vec![print(&e)];
    while out.len() < MAX_STEPS {
        match step(&c, &mut e) {
            Step::Done => out.push(print(&e)),
            _ => break,
        }
    }
    if !literal(&e) {
        // What couldn't be shown part by part, worked out whole.
        let v = c.eval_cell(&e);
        out.push(print(&of_value(v).unwrap_or(Expr::Num(0.0))));
    }
    Some(out)
}
