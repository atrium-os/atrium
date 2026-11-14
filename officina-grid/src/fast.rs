//! The vectorised path for arithmetic formula groups (docs/spec/officina.md §8.3).
//!
//! A formula made only of numbers, single-cell references and `+ - * / ^ %` and unary
//! minus compiles to a small stack program over `f64`. A group runs it row after row
//! reading column chunks directly; any row where an input isn't a number (text, an
//! error) or the arithmetic would give an error falls back to the general evaluator, so
//! results are identical either way (checked in tests/recalc.rs).

use crate::addr::MAX_ROWS;
use crate::book::Workbook;
use crate::column::{Cell, Column};
use crate::formula::{BinOp, Coord, Expr};
use crate::addr::MAX_COLS;

const MAX_STACK: usize = 32;

#[derive(Debug, Clone)]
enum Op {
    Const(f64),
    Load(usize, Coord),
    Neg,
    Pct,
    Bin(BinOp),
}

#[derive(Debug, Clone)]
pub struct Fast {
    ops: Vec<Op>,
    /// (sheet, column) of each load slot
    loads: Vec<(u16, u32)>,
}

impl Fast {
    pub fn compile(e: &Expr, sheet: u16, col: u32) -> Option<Fast> {
        let mut f = Fast { ops: Vec::new(), loads: Vec::new() };
        let mut depth = 0usize;
        let mut max = 0usize;
        f.emit(e, sheet, col, &mut depth, &mut max)?;
        (max <= MAX_STACK).then_some(f)
    }

    fn emit(&mut self, e: &Expr, sheet: u16, col: u32, d: &mut usize, max: &mut usize) -> Option<()> {
        match e {
            Expr::Num(x) => {
                self.ops.push(Op::Const(*x));
                *d += 1;
            }
            Expr::Ref(r) => {
                let c = r.col.resolve(col, MAX_COLS)?;
                let key = (r.sheet.unwrap_or(sheet), c);
                let slot = match self.loads.iter().position(|k| *k == key) {
                    Some(i) => i,
                    None => {
                        self.loads.push(key);
                        self.loads.len() - 1
                    }
                };
                self.ops.push(Op::Load(slot, r.row));
                *d += 1;
            }
            Expr::Paren(a) | Expr::Plus(a) => self.emit(a, sheet, col, d, max)?,
            Expr::Neg(a) => {
                self.emit(a, sheet, col, d, max)?;
                self.ops.push(Op::Neg);
            }
            Expr::Pct(a) => {
                self.emit(a, sheet, col, d, max)?;
                self.ops.push(Op::Pct);
            }
            Expr::Bin(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow), a, b) => {
                self.emit(a, sheet, col, d, max)?;
                self.emit(b, sheet, col, d, max)?;
                self.ops.push(Op::Bin(*op));
                *d -= 1;
            }
            _ => return None,
        }
        *max = (*max).max(*d);
        Some(())
    }

    /// Columns bound for one batch of rows.
    pub fn bind<'a>(&self, wb: &'a Workbook) -> Vec<Option<&'a Column>> {
        self.loads
            .iter()
            .map(|&(s, c)| if (s as usize) < wb.sheet_count() { wb.sheet(s).column(c) } else { None })
            .collect()
    }

    /// The result for row `row`, or `None` to fall back to the general evaluator.
    #[inline]
    pub fn run(&self, cols: &[Option<&Column>], row: u32) -> Option<f64> {
        let mut st = [0f64; MAX_STACK];
        let mut sp = 0usize;
        for op in &self.ops {
            match *op {
                Op::Const(x) => {
                    st[sp] = x;
                    sp += 1;
                }
                Op::Load(slot, rc) => {
                    let r = rc.resolve(row, MAX_ROWS)?;
                    let x = match cols[slot] {
                        None => 0.0,
                        Some(c) => match c.chunk(r) {
                            None => 0.0,
                            Some(ch) => match ch.get_num((r % crate::column::CHUNK_ROWS) as usize) {
                                Some(x) => x,
                                None => match ch.get((r % crate::column::CHUNK_ROWS) as usize) {
                                    Cell::Empty => 0.0,
                                    _ => return None,
                                },
                            },
                        },
                    };
                    st[sp] = x;
                    sp += 1;
                }
                Op::Neg => st[sp - 1] = -st[sp - 1],
                Op::Pct => st[sp - 1] /= 100.0,
                Op::Bin(op) => {
                    let y = st[sp - 1];
                    let x = st[sp - 2];
                    sp -= 1;
                    st[sp - 1] = match op {
                        BinOp::Add => x + y,
                        BinOp::Sub => x - y,
                        BinOp::Mul => x * y,
                        BinOp::Div => {
                            if y == 0.0 {
                                return None;
                            }
                            x / y
                        }
                        _ => {
                            if x == 0.0 && y <= 0.0 {
                                return None;
                            }
                            x.powf(y)
                        }
                    };
                }
            }
        }
        let v = st[0];
        v.is_finite().then_some(if v == 0.0 { 0.0 } else { v })
    }
}
