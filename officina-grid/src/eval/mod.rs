//! Formula evaluation (docs/spec/officina.md §8.2).
//!
//! An [`Ev`] is what an expression evaluates to before a function or operator decides
//! how to use it: a single value, a reference to a rectangle of cells (kept as a
//! reference so `SUM(A:A)` walks stored chunks and `INDEX` can return a reference), or an
//! array. Operators work element by element on arrays and ranges (Excel's array
//! semantics); a cell's final result that is still a range is narrowed by implicit
//! intersection.

pub mod criteria;
pub mod dates;
pub mod funcs;

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::{BinOp, Expr, Func, RangeRef};
use crate::value::{compare, format_general, ErrorKind, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// The most cells an array may hold when a range is turned into one.
pub const MAX_ARRAY: u64 = 16_000_000;

#[derive(Debug, Clone)]
pub struct Array {
    pub rows: usize,
    pub cols: usize,
    pub vals: Vec<Value>,
}

impl Array {
    pub fn get(&self, r: usize, c: usize) -> &Value {
        // Broadcast a single row or column, as Excel does.
        let r = if self.rows == 1 { 0 } else { r };
        let c = if self.cols == 1 { 0 } else { c };
        self.vals.get(r * self.cols + c).unwrap_or(&Value::Err(ErrorKind::NA))
    }
}

#[derive(Debug, Clone)]
pub enum Ev {
    V(Value),
    R(Rect),
    A(Arc<Array>),
}

impl Ev {
    pub fn err(e: ErrorKind) -> Ev {
        Ev::V(Value::Err(e))
    }
}

/// Where a formula is being evaluated.
pub struct Ctx<'a> {
    pub wb: &'a Workbook,
    pub sheet: u16,
    pub row: u32,
    pub col: u32,
}

impl<'a> Ctx<'a> {
    pub fn new(wb: &'a Workbook, sheet: u16, row: u32, col: u32) -> Self {
        Ctx { wb, sheet, row, col }
    }

    /// The value a formula leaves in its cell.
    pub fn eval_cell(&self, e: &Expr) -> Value {
        match self.eval(e) {
            Ev::V(v) => fix(v),
            Ev::R(r) => fix(self.intersect(&r)),
            Ev::A(a) => fix(a.vals.first().cloned().unwrap_or(Value::Empty)),
        }
    }

    pub fn eval(&self, e: &Expr) -> Ev {
        match e {
            Expr::Num(v) => Ev::V(Value::Num(*v)),
            Expr::Str(s) => Ev::V(Value::Str(s.clone())),
            Expr::Bool(b) => Ev::V(Value::Bool(*b)),
            Expr::Err(k) => Ev::err(*k),
            Expr::Name(n) if n.is_empty() => Ev::V(Value::Empty),
            Expr::Name(n) => match self.wb.names.get(self.sheet, n) {
                Some(d) => {
                    let x = d.expr.clone();
                    crate::names::deeper(|| self.eval(&x)).unwrap_or(Ev::err(ErrorKind::Name))
                }
                None => Ev::err(ErrorKind::Name),
            },
            Expr::Ref(r) => {
                let sheet = r.sheet.unwrap_or(self.sheet);
                match (r.row.resolve(self.row, MAX_ROWS), r.col.resolve(self.col, MAX_COLS)) {
                    (Some(row), Some(col)) if (sheet as usize) < self.wb.sheet_count() => {
                        Ev::R(Rect { sheet, r0: row, c0: col, r1: row, c1: col })
                    }
                    _ => Ev::err(ErrorKind::Ref),
                }
            }
            Expr::Range(r) => match self.range(r) {
                Some(rect) => Ev::R(rect),
                None => Ev::err(ErrorKind::Ref),
            },
            Expr::Paren(a) | Expr::Plus(a) => self.eval(a),
            Expr::Neg(a) => self.map1(self.eval(a), |v| match to_num(&v) {
                Ok(x) => Value::num(-x),
                Err(e) => Value::Err(e),
            }),
            Expr::Pct(a) => self.map1(self.eval(a), |v| match to_num(&v) {
                Ok(x) => Value::num(x / 100.0),
                Err(e) => Value::Err(e),
            }),
            Expr::Bin(op, a, b) => {
                let x = self.eval(a);
                let y = self.eval(b);
                self.map2(x, y, |p, q| binop(*op, &p, &q))
            }
            Expr::Call(Func::Known(id), args) => (funcs::FUNCS[*id as usize].f)(self, args),
            Expr::Call(Func::Unknown(_), _) => Ev::err(ErrorKind::Name),
            Expr::Array(rows) => {
                let cols = rows.first().map_or(0, |r| r.len());
                let vals = rows.iter().flatten().map(|e| self.scalar(e)).collect();
                Ev::A(Arc::new(Array { rows: rows.len(), cols, vals }))
            }
        }
    }

    pub fn range(&self, r: &RangeRef) -> Option<Rect> {
        let sheet = r.sheet.unwrap_or(self.sheet);
        if sheet as usize >= self.wb.sheet_count() {
            return None;
        }
        let a = r.r0.resolve(self.row, MAX_ROWS)?;
        let b = r.r1.resolve(self.row, MAX_ROWS)?;
        let c = r.c0.resolve(self.col, MAX_COLS)?;
        let d = r.c1.resolve(self.col, MAX_COLS)?;
        Some(Rect { sheet, r0: a.min(b), r1: a.max(b), c0: c.min(d), c1: c.max(d) })
    }

    /// One value from an expression in a place that wants one.
    pub fn scalar(&self, e: &Expr) -> Value {
        self.to_scalar(self.eval(e))
    }

    pub fn to_scalar(&self, ev: Ev) -> Value {
        match ev {
            Ev::V(v) => v,
            Ev::R(r) => self.intersect(&r),
            Ev::A(a) => a.vals.first().cloned().unwrap_or(Value::Empty),
        }
    }

    /// Implicit intersection: a one-cell range is its cell; a column range gives the
    /// cell in the formula's row, a row range the cell in its column; else `#VALUE!`.
    pub fn intersect(&self, r: &Rect) -> Value {
        if r.r0 == r.r1 && r.c0 == r.c1 {
            return self.wb.value(r.sheet, r.r0, r.c0);
        }
        if r.c0 == r.c1 && r.sheet == self.sheet && (r.r0..=r.r1).contains(&self.row) {
            return self.wb.value(r.sheet, self.row, r.c0);
        }
        if r.r0 == r.r1 && r.sheet == self.sheet && (r.c0..=r.c1).contains(&self.col) {
            return self.wb.value(r.sheet, r.r0, self.col);
        }
        Value::Err(ErrorKind::Value)
    }

    /// A rectangle clipped to where its sheet holds anything (whole-column ranges stop
    /// at the last used row). `None`: nothing there.
    pub fn clip(&self, r: &Rect) -> Option<Rect> {
        let (rows, cols) = self.wb.extent(r.sheet);
        if rows == 0 || cols == 0 || r.r0 >= rows || r.c0 >= cols {
            return None;
        }
        Some(Rect { r1: r.r1.min(rows - 1), c1: r.c1.min(cols - 1), ..*r })
    }

    /// A range or array as an array (bounded by [`MAX_ARRAY`]). Ranges are clipped to
    /// the used area, except that a range's shape is kept when it is within it.
    pub fn to_array(&self, ev: Ev) -> Result<Arc<Array>, ErrorKind> {
        match ev {
            Ev::A(a) => Ok(a),
            Ev::V(v) => Ok(Arc::new(Array { rows: 1, cols: 1, vals: vec![v] })),
            Ev::R(r) => {
                let Some(c) = self.clip(&r) else {
                    return Ok(Arc::new(Array { rows: 1, cols: 1, vals: vec![Value::Empty] }));
                };
                if c.cells() > MAX_ARRAY {
                    return Err(ErrorKind::Limit);
                }
                let (rows, cols) = (c.rows() as usize, c.cols() as usize);
                let mut vals = vec![Value::Empty; rows * cols];
                self.wb.each(&c, &mut |row, col, v| {
                    vals[(row - c.r0) as usize * cols + (col - c.c0) as usize] = v;
                });
                Ok(Arc::new(Array { rows, cols, vals }))
            }
        }
    }

    fn map1(&self, a: Ev, f: impl Fn(Value) -> Value) -> Ev {
        match a {
            Ev::V(v) => Ev::V(f(v)),
            Ev::R(r) if r.cells() == 1 => Ev::V(f(self.wb.value(r.sheet, r.r0, r.c0))),
            other => match self.to_array(other) {
                Ok(arr) => Ev::A(Arc::new(Array { rows: arr.rows, cols: arr.cols, vals: arr.vals.iter().cloned().map(f).collect() })),
                Err(e) => Ev::err(e),
            },
        }
    }

    fn map2(&self, a: Ev, b: Ev, f: impl Fn(Value, Value) -> Value) -> Ev {
        let single = |e: &Ev| match e {
            Ev::V(_) => true,
            Ev::R(r) => r.cells() == 1,
            Ev::A(a) => a.rows == 1 && a.cols == 1,
        };
        if single(&a) && single(&b) {
            return Ev::V(f(self.to_scalar(a), self.to_scalar(b)));
        }
        let (x, y) = match (self.to_array(a), self.to_array(b)) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ev::err(e),
        };
        let rows = if x.rows == 1 { y.rows } else if y.rows == 1 { x.rows } else { x.rows.min(y.rows) };
        let cols = if x.cols == 1 { y.cols } else if y.cols == 1 { x.cols } else { x.cols.min(y.cols) };
        let mut vals = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for c in 0..cols {
                vals.push(f(x.get(r, c).clone(), y.get(r, c).clone()));
            }
        }
        Ev::A(Arc::new(Array { rows, cols, vals }))
    }
}

/// A result as stored: errors stay errors, `-0` becomes 0.
fn fix(v: Value) -> Value {
    match v {
        Value::Num(x) => Value::num(x),
        v => v,
    }
}

/// Excel's number coercion of a single value.
pub fn to_num(v: &Value) -> Result<f64, ErrorKind> {
    match v {
        Value::Empty => Ok(0.0),
        Value::Num(x) => Ok(*x),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Str(s) => parse_number(s).ok_or(ErrorKind::Value),
        Value::Err(e) => Err(*e),
    }
}

/// Text as a number: optional spaces, sign, digits with an optional exponent, an
/// optional trailing `%`. (Dates and currency in text come with locale support.)
pub fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (t, pct) = match t.strip_suffix('%') {
        Some(r) => (r.trim_end(), true),
        None => (t, false),
    };
    if !t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E')) {
        return None;
    }
    let x: f64 = t.parse().ok()?;
    if !x.is_finite() {
        return None;
    }
    Some(if pct { x / 100.0 } else { x })
}

pub fn to_text(v: &Value) -> Result<Arc<str>, ErrorKind> {
    match v {
        Value::Empty => Ok(Arc::from("")),
        Value::Num(x) => Ok(Arc::from(format_general(*x).as_str())),
        Value::Str(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(Arc::from(if *b { "TRUE" } else { "FALSE" })),
        Value::Err(e) => Err(*e),
    }
}

pub fn to_bool(v: &Value) -> Result<bool, ErrorKind> {
    match v {
        Value::Empty => Ok(false),
        Value::Num(x) => Ok(*x != 0.0),
        Value::Bool(b) => Ok(*b),
        Value::Str(s) => {
            if s.eq_ignore_ascii_case("TRUE") {
                Ok(true)
            } else if s.eq_ignore_ascii_case("FALSE") {
                Ok(false)
            } else {
                Err(ErrorKind::Value)
            }
        }
        Value::Err(e) => Err(*e),
    }
}

pub fn binop(op: BinOp, a: &Value, b: &Value) -> Value {
    if let Value::Err(e) = a {
        return Value::Err(*e);
    }
    if let Value::Err(e) = b {
        return Value::Err(*e);
    }
    match op {
        BinOp::Concat => match (to_text(a), to_text(b)) {
            (Ok(x), Ok(y)) => {
                let mut s = String::with_capacity(x.len() + y.len());
                s.push_str(&x);
                s.push_str(&y);
                Value::str(crate::strings::clip(&s))
            }
            (Err(e), _) | (_, Err(e)) => Value::Err(e),
        },
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let o = compare(a, b);
            Value::Bool(match op {
                BinOp::Eq => o == Ordering::Equal,
                BinOp::Ne => o != Ordering::Equal,
                BinOp::Lt => o == Ordering::Less,
                BinOp::Le => o != Ordering::Greater,
                BinOp::Gt => o == Ordering::Greater,
                _ => o != Ordering::Less,
            })
        }
        _ => {
            let (x, y) = match (to_num(a), to_num(b)) {
                (Ok(x), Ok(y)) => (x, y),
                (Err(e), _) | (_, Err(e)) => return Value::Err(e),
            };
            arith(op, x, y)
        }
    }
}

#[inline]
pub fn arith(op: BinOp, x: f64, y: f64) -> Value {
    match op {
        BinOp::Add => Value::num(x + y),
        BinOp::Sub => Value::num(x - y),
        BinOp::Mul => Value::num(x * y),
        BinOp::Div => {
            if y == 0.0 {
                Value::Err(ErrorKind::Div0)
            } else {
                Value::num(x / y)
            }
        }
        BinOp::Pow => {
            if x == 0.0 && y == 0.0 {
                Value::Err(ErrorKind::Num)
            } else if x == 0.0 && y < 0.0 {
                Value::Err(ErrorKind::Div0)
            } else {
                Value::num(x.powf(y))
            }
        }
        _ => Value::Err(ErrorKind::Value),
    }
}
