//! Formula evaluation (docs/spec/officina.md §8.2).
//!
//! An [`Ev`] is what an expression evaluates to before a function or operator decides
//! how to use it: a single value, a reference to a rectangle of cells (kept as a
//! reference so `SUM(A:A)` walks stored chunks and `INDEX` can return a reference), or an
//! array. Operators work element by element on arrays and ranges (Excel's array
//! semantics); a cell's final result that is still a range is narrowed by implicit
//! intersection.

pub mod arrays;
pub mod criteria;
pub mod dates;
pub mod funcs;
pub mod ifcache;
mod fin;
mod bonds;
mod conv;
mod dist;
mod linest;
mod more;
mod stats;
mod refs;
mod special;
mod textafter;
mod matrix;
mod complex;
mod mathx;
mod spread;
mod database;
mod textmore;
mod convert;
mod groupby;

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::{BinOp, Expr, Func, RangeRef, TableArea, TableRef};
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
        if r >= self.rows || c >= self.cols {
            return &Value::Err(ErrorKind::NA);
        }
        self.vals.get(r * self.cols + c).unwrap_or(&Value::Err(ErrorKind::NA))
    }
}

#[derive(Debug, Clone)]
pub enum Ev {
    V(Value),
    R(Rect),
    A(Arc<Array>),
    /// A function value: what `LAMBDA` makes.
    F(Arc<Lambda>),
}

/// A `LAMBDA`: its parameters, its body and the names it saw where it was made.
#[derive(Debug)]
pub struct Lambda {
    pub params: Vec<Arc<str>>,
    pub body: Expr,
    pub scope: Option<Arc<Scope>>,
}

/// Names bound by `LET` and lambda calls, innermost first.
#[derive(Debug)]
pub struct Scope {
    pub name: Arc<str>,
    pub val: Ev,
    /// A lambda parameter its call left out (`ISOMITTED`).
    pub omitted: bool,
    pub next: Option<Arc<Scope>>,
}

/// How deeply lambdas may call each other (a recursive name's guard).
pub const MAX_LAMBDA_DEPTH: u32 = 64;

thread_local! {
    static LAMBDA_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The largest dynamic array a formula may spill.
pub const MAX_SPILL: u64 = 1 << 22;

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
    /// `LET` names and lambda parameters in scope.
    pub scope: Option<Arc<Scope>>,
}

impl<'a> Ctx<'a> {
    pub fn new(wb: &'a Workbook, sheet: u16, row: u32, col: u32) -> Self {
        Ctx { wb, sheet, row, col, scope: None }
    }

    /// This place with other names in scope.
    pub fn with(&self, scope: Option<Arc<Scope>>) -> Ctx<'a> {
        Ctx { wb: self.wb, sheet: self.sheet, row: self.row, col: self.col, scope }
    }

    /// `name` bound to `val` on top of this scope.
    pub fn bind(&self, name: Arc<str>, val: Ev, omitted: bool) -> Option<Arc<Scope>> {
        Some(Arc::new(Scope { name, val, omitted, next: self.scope.clone() }))
    }

    /// A `LET` name or lambda parameter in scope.
    pub fn local(&self, n: &str) -> Option<&Scope> {
        let mut s = self.scope.as_deref();
        while let Some(x) = s {
            if x.name.eq_ignore_ascii_case(n) {
                return Some(x);
            }
            s = x.next.as_deref();
        }
        None
    }

    /// The value a formula leaves in its cell, narrowed to one value (a formula that
    /// can't spill).
    pub fn eval_cell(&self, e: &Expr) -> Value {
        match self.eval(e) {
            Ev::V(v) => fix(v),
            Ev::R(r) => fix(self.intersect(&r)),
            Ev::A(a) => fix(a.vals.first().cloned().unwrap_or(Value::Empty)),
            Ev::F(_) => Value::Err(ErrorKind::Calc),
        }
    }

    /// A formula's result as a dynamic array: the anchor's value and, when the result
    /// has more than one cell, the whole array to spill.
    pub fn eval_spill(&self, e: &Expr) -> (Value, Option<Arc<Array>>) {
        let arr = match self.eval(e) {
            Ev::V(v) => return (fix(v), None),
            Ev::F(_) => return (Value::Err(ErrorKind::Calc), None),
            Ev::R(r) if r.cells() == 1 => return (fix(self.wb.value(r.sheet, r.r0, r.c0)), None),
            Ev::R(r) => {
                if r.cells() > MAX_SPILL {
                    return (Value::Err(ErrorKind::Spill), None);
                }
                let (rows, cols) = (r.rows() as usize, r.cols() as usize);
                let mut vals = vec![Value::Empty; rows * cols];
                self.wb.each(&r, &mut |row, col, v| vals[(row - r.r0) as usize * cols + (col - r.c0) as usize] = v);
                Arc::new(Array { rows, cols, vals })
            }
            Ev::A(a) => a,
        };
        let first = fix(arr.vals.first().cloned().unwrap_or(Value::Empty));
        if arr.rows * arr.cols <= 1 {
            return (first, None);
        }
        if (arr.rows * arr.cols) as u64 > MAX_SPILL {
            return (Value::Err(ErrorKind::Spill), None);
        }
        (first, Some(arr))
    }

    /// Calls a lambda with argument expressions (evaluated here); a missing one is
    /// an omitted parameter.
    pub fn call(&self, l: &Lambda, args: &[Expr]) -> Ev {
        if args.len() > l.params.len() {
            return Ev::err(ErrorKind::Value);
        }
        let vals: Vec<Option<Ev>> = args.iter().map(|a| (!matches!(a, Expr::Name(n) if n.is_empty())).then(|| self.eval(a))).collect();
        self.call_with(l, vals)
    }

    /// Calls a lambda with values; `None` (or too few) leaves parameters omitted.
    pub fn call_with(&self, l: &Lambda, vals: Vec<Option<Ev>>) -> Ev {
        if vals.len() > l.params.len() {
            return Ev::err(ErrorKind::Value);
        }
        let mut scope = l.scope.clone();
        let mut vals = vals.into_iter();
        for p in &l.params {
            let (val, omitted) = match vals.next().flatten() {
                Some(v) => (v, false),
                None => (Ev::V(Value::Empty), true),
            };
            scope = Some(Arc::new(Scope { name: p.clone(), val, omitted, next: scope }));
        }
        let d = LAMBDA_DEPTH.with(|d| d.get());
        if d >= MAX_LAMBDA_DEPTH {
            return Ev::err(ErrorKind::Limit);
        }
        LAMBDA_DEPTH.with(|x| x.set(d + 1));
        let out = self.with(scope).eval(&l.body);
        LAMBDA_DEPTH.with(|x| x.set(d));
        out
    }

    /// What a name evaluates to when it is called as a function (`F(2)` with `F` a
    /// `LET` name or a defined name bound to a `LAMBDA`).
    fn callee(&self, n: &Arc<str>) -> Option<Ev> {
        if let Some(x) = self.local(n) {
            return Some(x.val.clone());
        }
        let x = self.wb.bound(self.sheet, self.row, self.col, &Expr::Name(n.clone()))?;
        crate::names::deeper(|| self.with(None).eval(&x))
    }

    pub fn eval(&self, e: &Expr) -> Ev {
        match e {
            Expr::Num(v) => Ev::V(Value::Num(*v)),
            Expr::Str(s) => Ev::V(Value::Str(s.clone())),
            Expr::Bool(b) => Ev::V(Value::Bool(*b)),
            Expr::Err(k) => Ev::err(*k),
            Expr::Name(n) if n.is_empty() => Ev::V(Value::Empty),
            Expr::Name(n) => match self.local(n) {
                Some(x) => x.val.clone(),
                // A defined name means the same wherever it is used: no LET names.
                None => match self.wb.bound(self.sheet, self.row, self.col, e) {
                    Some(x) => crate::names::deeper(|| self.with(None).eval(&x)).unwrap_or(Ev::err(ErrorKind::Name)),
                    None => Ev::err(ErrorKind::Name),
                },
            },
            Expr::Table(t) => match self.wb.resolve_table(self.sheet, self.row, self.col, t) {
                // `@` outside the table's data rows: Excel's #VALUE!.
                Some(_) if t.area == TableArea::ThisRow && !self.this_row_ok(t) => Ev::err(ErrorKind::Value),
                Some(x) => self.eval(&x),
                None => Ev::err(ErrorKind::Ref),
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
            Expr::Neg(a) => self.map1(self.eval(a), |v| match self.number(&v) {
                Ok(x) => Value::num(-x),
                Err(e) => Value::Err(e),
            }),
            Expr::Pct(a) => self.map1(self.eval(a), |v| match self.number(&v) {
                Ok(x) => Value::num(x / 100.0),
                Err(e) => Value::Err(e),
            }),
            Expr::Bin(op, a, b) => {
                let x = self.eval(a);
                let y = self.eval(b);
                self.map2(x, y, |p, q| binop_with(*op, &p, &q, |v| self.number(v)))
            }
            Expr::Call(Func::Known(id), args) => match arrays::lifted(*id, args) {
                Some(mask) => self.lift(*id, mask, args),
                None => (funcs::FUNCS[*id as usize].f)(self, args),
            },
            Expr::Call(Func::Unknown(n), args) => match self.callee(n) {
                Some(Ev::F(l)) => self.call(&l, args),
                Some(Ev::V(Value::Err(k))) if k != ErrorKind::Name => Ev::err(k),
                Some(_) => Ev::err(ErrorKind::Value),
                None => Ev::err(ErrorKind::Name),
            },
            Expr::Invoke(g, args) => match self.eval(g) {
                Ev::F(l) => self.call(&l, args),
                Ev::V(Value::Err(k)) => Ev::err(k),
                _ => Ev::err(ErrorKind::Value),
            },
            Expr::At(a) => match self.eval(a) {
                Ev::R(r) => Ev::V(self.intersect(&r)),
                Ev::A(x) => Ev::V(x.vals.first().cloned().unwrap_or(Value::Empty)),
                Ev::F(_) => Ev::err(ErrorKind::Calc),
                v => v,
            },
            Expr::Spill(r) => {
                let sheet = r.sheet.unwrap_or(self.sheet);
                match (r.row.resolve(self.row, MAX_ROWS), r.col.resolve(self.col, MAX_COLS)) {
                    (Some(row), Some(col)) if (sheet as usize) < self.wb.sheet_count() => match self.wb.spill_area(crate::addr::CellAddr::new(sheet, row, col)) {
                        Some(area) => Ev::R(area),
                        None => Ev::err(ErrorKind::Ref),
                    },
                    _ => Ev::err(ErrorKind::Ref),
                }
            }
            Expr::Array(rows) => {
                let cols = rows.first().map_or(0, |r| r.len());
                let vals = rows.iter().flatten().map(|e| self.scalar(e)).collect();
                Ev::A(Arc::new(Array { rows: rows.len(), cols, vals }))
            }
        }
    }

    fn this_row_ok(&self, t: &TableRef) -> bool {
        let tb = if t.table.is_empty() { self.wb.table_at(self.sheet, self.row, self.col) } else { self.wb.table(&t.table) };
        tb.and_then(|tb| tb.data_rows()).is_some_and(|(a, b)| (a..=b).contains(&self.row))
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

    pub(crate) fn numeric_text(&self, s: &str) -> Option<f64> {
        parse_number(s).or_else(|| self.wb.date_system().formula_text(s))
    }

    pub(crate) fn number(&self, v: &Value) -> Result<f64, ErrorKind> {
        match v {
            Value::Str(s) => self.numeric_text(s).ok_or(ErrorKind::Value),
            v => to_num(v),
        }
    }

    pub fn to_scalar(&self, ev: Ev) -> Value {
        match ev {
            Ev::V(v) => v,
            Ev::R(r) => self.intersect(&r),
            Ev::A(a) => a.vals.first().cloned().unwrap_or(Value::Empty),
            Ev::F(_) => Value::Err(ErrorKind::Calc),
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

    /// A range or array as an array (bounded by [`MAX_ARRAY`]). Explicit ranges
    /// include blank cells. Whole-axis references use the used extent on that axis.
    pub fn to_array(&self, ev: Ev) -> Result<Arc<Array>, ErrorKind> {
        match ev {
            Ev::A(a) => Ok(a),
            Ev::V(v) => Ok(Arc::new(Array { rows: 1, cols: 1, vals: vec![v] })),
            Ev::F(_) => Err(ErrorKind::Calc),
            Ev::R(mut c) => {
                let (rows, cols) = self.wb.extent(c.sheet);
                if c.r0 == 0 && c.r1 == MAX_ROWS - 1 {
                    c.r1 = rows.saturating_sub(1);
                }
                if c.c0 == 0 && c.c1 == MAX_COLS - 1 {
                    c.c1 = cols.saturating_sub(1);
                }
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

    pub(crate) fn map1(&self, a: Ev, f: impl Fn(Value) -> Value) -> Ev {
        match a {
            Ev::V(v) => Ev::V(f(v)),
            Ev::R(r) if r.cells() == 1 => Ev::V(f(self.wb.value(r.sheet, r.r0, r.c0))),
            other => match self.to_array(other) {
                Ok(arr) => Ev::A(Arc::new(Array { rows: arr.rows, cols: arr.cols, vals: arr.vals.iter().cloned().map(f).collect() })),
                Err(e) => Ev::err(e),
            },
        }
    }

    pub(crate) fn map2(&self, a: Ev, b: Ev, f: impl Fn(Value, Value) -> Value) -> Ev {
        let single = |e: &Ev| match e {
            Ev::V(_) => true,
            Ev::R(r) => r.cells() == 1,
            Ev::A(a) => a.rows == 1 && a.cols == 1,
            Ev::F(_) => true,
        };
        if single(&a) && single(&b) {
            return Ev::V(f(self.to_scalar(a), self.to_scalar(b)));
        }
        let (x, y) = match (self.to_array(a), self.to_array(b)) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ev::err(e),
        };
        let rows = x.rows.max(y.rows);
        let cols = x.cols.max(y.cols);
        if rows as u64 * cols as u64 > MAX_ARRAY {
            return Ev::err(ErrorKind::Limit);
        }
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

/// Numeric coercion where no workbook calendar is available.
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
/// optional trailing `%`. Calendar dates are handled separately in formulas.
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
    binop_with(op, a, b, to_num)
}

fn binop_with(op: BinOp, a: &Value, b: &Value, number: impl Fn(&Value) -> Result<f64, ErrorKind>) -> Value {
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
            let (x, y) = match (number(a), number(b)) {
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
