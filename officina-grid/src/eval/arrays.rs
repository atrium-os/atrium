//! Dynamic arrays (docs/spec/officina.md §16.31): functions that return arrays, `LET`
//! and `LAMBDA` with the functions that call lambdas, and the lifting of one-value
//! functions over arrays (`ABS(A1:A3)` is three values, as in Excel 365).

use super::funcs::{self, FUNCS};
use super::{to_bool, to_num, Array, Ctx, Ev, Lambda, Scope, MAX_ARRAY};
use crate::formula::{Expr, Func};
use crate::value::{compare, ErrorKind, Value};
use std::cmp::Ordering;
use std::sync::{Arc, OnceLock};

const ALL: u32 = u32::MAX;

/// Functions that take one value where an array may stand, and which argument
/// positions (bits) they lift over: given arrays there they give an array.
const LIFT: &[(&str, u32)] = &[
    ("ABS", ALL),
    ("ROUND", ALL),
    ("ROUNDUP", ALL),
    ("ROUNDDOWN", ALL),
    ("INT", ALL),
    ("TRUNC", ALL),
    ("MOD", ALL),
    ("POWER", ALL),
    ("SQRT", ALL),
    ("EXP", ALL),
    ("LN", ALL),
    ("LOG", ALL),
    ("LOG10", ALL),
    ("SIGN", ALL),
    ("IF", ALL),
    ("IFS", ALL),
    ("IFERROR", ALL),
    ("IFNA", ALL),
    ("NOT", ALL),
    ("ISBLANK", ALL),
    ("ISNUMBER", ALL),
    ("ISTEXT", ALL),
    ("ISLOGICAL", ALL),
    ("ISERROR", ALL),
    ("ISERR", ALL),
    ("ISNA", ALL),
    ("LEN", ALL),
    ("LEFT", ALL),
    ("RIGHT", ALL),
    ("MID", ALL),
    ("UPPER", ALL),
    ("LOWER", ALL),
    ("TRIM", ALL),
    ("CONCATENATE", ALL),
    ("REPT", ALL),
    ("FIND", ALL),
    ("SEARCH", ALL),
    ("SUBSTITUTE", ALL),
    ("TEXTAFTER", 0b11_1101), // the delimiter argument is a set, not lifted
    ("VALUE", ALL),
    ("TEXT", ALL),
    ("EXACT", ALL),
    ("DATE", ALL),
    ("YEAR", ALL),
    ("MONTH", ALL),
    ("DAY", ALL),
    ("WEEKDAY", ALL),
    ("CHOOSE", 1),
    ("MATCH", 1),
    ("VLOOKUP", 1),
    ("HLOOKUP", 1),
    ("XLOOKUP", 1),
    ("INDEX", 0b110),
    ("SUMIF", 0b10),
    ("COUNTIF", 0b10),
    ("AVERAGEIF", 0b10),
];

/// Functions whose result may be an array whatever their arguments.
const ARRAY_RESULT: &[&str] = &[
    "INDEX", "XLOOKUP", "SEQUENCE", "RANDARRAY", "TRANSPOSE", "FILTER", "SORT", "SORTBY", "UNIQUE", "VSTACK", "HSTACK", "TAKE", "DROP", "CHOOSEROWS", "CHOOSECOLS", "TOCOL", "TOROW", "WRAPROWS", "WRAPCOLS", "EXPAND", "MAP", "SCAN", "BYROW", "BYCOL", "MAKEARRAY",
];

/// Functions whose result is an array when an argument is one.
const PASS: &[&str] = &["LET", "REDUCE", "CHOOSE", "ROW", "COLUMN"];

struct Shapes {
    lift: Vec<u32>,
    array: Vec<bool>,
    pass: Vec<bool>,
}

fn shapes() -> &'static Shapes {
    static S: OnceLock<Shapes> = OnceLock::new();
    S.get_or_init(|| {
        let n = FUNCS.len();
        let mut s = Shapes { lift: vec![0; n], array: vec![false; n], pass: vec![false; n] };
        for (name, m) in LIFT {
            s.lift[funcs::lookup(name).expect("lifted function") as usize] = *m;
        }
        for name in ARRAY_RESULT {
            s.array[funcs::lookup(name).expect("array function") as usize] = true;
        }
        for name in PASS {
            s.pass[funcs::lookup(name).expect("pass function") as usize] = true;
        }
        s
    })
}

fn on(mask: u32, i: usize) -> bool {
    mask == ALL || (i < 32 && mask & (1 << i) != 0)
}

fn legacy_arg(id: usize, position: usize, arg: &Expr) -> bool {
    on(shapes().lift[id], position)
        || (position == 1 && FUNCS[id].name == "TEXTAFTER" && !matches!(arg, Expr::Array(_)))
}

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}

/// Whether `e` may evaluate to more than one value (judged from the formula alone).
pub fn can_array(e: &Expr) -> bool {
    match e {
        Expr::Range(_) | Expr::Spill(_) | Expr::Table(_) | Expr::Invoke(..) => true,
        Expr::Name(n) => !n.is_empty() && !n.starts_with('\0'),
        Expr::Array(rows) => rows.len() * rows.first().map_or(0, |r| r.len()) > 1,
        Expr::Call(Func::Unknown(_), _) => true,
        Expr::Call(Func::Known(id), args) => {
            let s = shapes();
            let id = *id as usize;
            s.array[id] || (s.pass[id] && args.iter().any(can_array)) || (s.lift[id] != 0 && args.iter().enumerate().any(|(i, a)| legacy_arg(id, i, a) && can_array(a)))
        }
        Expr::Neg(a) | Expr::Plus(a) | Expr::Pct(a) | Expr::Paren(a) => can_array(a),
        Expr::Bin(_, a, b) => can_array(a) || can_array(b),
        _ => false,
    }
}

/// `e` as a formula from a file made before dynamic arrays: where it could give an
/// array, `@` (implicit intersection) goes on the references and calls that would
/// (`=A1:A5*2` reads `=@A1:A5*2`, as Excel shows it), so it keeps its one value.
pub fn legacy(e: Expr) -> Expr {
    if !can_array(&e) {
        return e;
    }
    let b = |x: Box<Expr>| Box::new(legacy(*x));
    match e {
        Expr::Neg(x) => Expr::Neg(b(x)),
        Expr::Plus(x) => Expr::Plus(b(x)),
        Expr::Pct(x) => Expr::Pct(b(x)),
        Expr::Paren(x) => Expr::Paren(b(x)),
        Expr::Bin(op, x, y) => Expr::Bin(op, b(x), b(y)),
        Expr::Call(Func::Known(id), args) if !shapes().array[id as usize] && shapes().lift[id as usize] != 0 => {
            Expr::Call(Func::Known(id), args.into_iter().enumerate().map(|(i, a)| if legacy_arg(id as usize, i, &a) { legacy(a) } else { a }).collect())
        }
        e => Expr::At(Box::new(e)),
    }
}

/// Undoes [`legacy`]: the `@`s it would put back, taken out.
pub fn unlegacy(e: &Expr) -> Expr {
    let b = |x: &Expr| Box::new(unlegacy(x));
    match e {
        Expr::At(x) if can_array(x) => (**x).clone(),
        Expr::Neg(x) => Expr::Neg(b(x)),
        Expr::Plus(x) => Expr::Plus(b(x)),
        Expr::Pct(x) => Expr::Pct(b(x)),
        Expr::Paren(x) => Expr::Paren(b(x)),
        Expr::Bin(op, x, y) => Expr::Bin(*op, b(x), b(y)),
        Expr::Call(Func::Known(id), args) if !shapes().array[*id as usize] && shapes().lift[*id as usize] != 0 => {
            Expr::Call(Func::Known(*id), args.iter().enumerate().map(|(i, a)| if legacy_arg(*id as usize, i, a) { unlegacy(a) } else { a.clone() }).collect())
        }
        e => e.clone(),
    }
}

/// How a formula goes in a file that also has readers from before dynamic arrays:
/// `(formula, dynamic)`. A formula that may spill is a dynamic array formula; one
/// that can't is written as before, without the `@`s such files imply.
pub fn for_file(e: &Expr) -> (Expr, bool) {
    if can_array(e) {
        return (e.clone(), true);
    }
    let u = unlegacy(e);
    if legacy(u.clone()) == *e {
        (u, false)
    } else {
        (e.clone(), true)
    }
}

/// `e` for formats without `@` and `A1#` (OpenDocument): `@` dropped, and each spill
/// reference replaced by what `spill` gives (the area it spills over now).
pub fn plain(e: &Expr, spill: &dyn Fn(&crate::formula::CellRef) -> Expr) -> Expr {
    let b = |x: &Expr| Box::new(plain(x, spill));
    match e {
        Expr::At(x) => plain(x, spill),
        Expr::Spill(r) => spill(r),
        Expr::Neg(x) => Expr::Neg(b(x)),
        Expr::Plus(x) => Expr::Plus(b(x)),
        Expr::Pct(x) => Expr::Pct(b(x)),
        Expr::Paren(x) => Expr::Paren(b(x)),
        Expr::Bin(op, x, y) => Expr::Bin(*op, b(x), b(y)),
        Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(|a| plain(a, spill)).collect()),
        Expr::Invoke(g, args) => Expr::Invoke(b(g), args.iter().map(|a| plain(a, spill)).collect()),
        e => e.clone(),
    }
}

/// The lifting mask for a call when one of its lifted arguments may be an array.
pub(crate) fn lifted(id: u16, args: &[Expr]) -> Option<u32> {
    let m = shapes().lift[id as usize];
    (m != 0 && args.iter().enumerate().any(|(i, a)| on(m, i) && can_array(a))).then_some(m)
}

fn multi(e: &Ev) -> bool {
    match e {
        Ev::R(r) => r.cells() > 1,
        Ev::A(a) => a.rows * a.cols > 1,
        _ => false,
    }
}

/// An element of an array taken to `rows` × `cols`: one row or column broadcasts,
/// past the end is `#N/A` (Excel's rule for arrays of different sizes).
fn at(a: &Array, r: usize, c: usize) -> Value {
    a.get(r, c).clone()
}

fn one(c: &Ctx, ev: Ev) -> Value {
    match ev {
        Ev::A(a) if a.rows * a.cols > 1 => Value::Err(ErrorKind::Calc),
        ev => c.to_scalar(ev),
    }
}

impl Ctx<'_> {
    /// Calls function `id` once per element of the arrays in its lifted arguments.
    pub(crate) fn lift(&self, id: u16, mask: u32, args: &[Expr]) -> Ev {
        let f = FUNCS[id as usize].f;
        let evs: Vec<Option<Ev>> = args.iter().enumerate().map(|(i, a)| (on(mask, i) && !missing(a)).then(|| self.eval(a))).collect();
        let names: Vec<Arc<str>> = (0..args.len()).map(|i| Arc::from(format!("\0{i}").as_str())).collect();
        let call_args: Vec<Expr> = args.iter().enumerate().map(|(i, a)| if evs[i].is_some() { Expr::Name(names[i].clone()) } else { a.clone() }).collect();
        if !evs.iter().flatten().any(multi) {
            let mut scope = self.scope.clone();
            for (i, ev) in evs.into_iter().enumerate() {
                if let Some(ev) = ev {
                    scope = Some(Arc::new(Scope { name: names[i].clone(), val: ev, omitted: false, next: scope }));
                }
            }
            return f(&self.with(scope), &call_args);
        }
        let mut arrs: Vec<(usize, Arc<Array>)> = Vec::new();
        for (i, ev) in evs.into_iter().enumerate() {
            if let Some(ev) = ev {
                match self.to_array(ev) {
                    Ok(a) => arrs.push((i, a)),
                    Err(k) => return Ev::err(k),
                }
            }
        }
        let rows = arrs.iter().map(|(_, a)| a.rows).max().unwrap_or(1);
        let cols = arrs.iter().map(|(_, a)| a.cols).max().unwrap_or(1);
        if (rows * cols) as u64 > MAX_ARRAY {
            return Ev::err(ErrorKind::Limit);
        }
        let mut vals = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for col in 0..cols {
                let mut scope = self.scope.clone();
                for (i, a) in &arrs {
                    scope = Some(Arc::new(Scope { name: names[*i].clone(), val: Ev::V(at(a, r, col)), omitted: false, next: scope }));
                }
                let c2 = self.with(scope);
                vals.push(c2.to_scalar(f(&c2, &call_args)));
            }
        }
        Ev::A(Arc::new(Array { rows, cols, vals }))
    }

    fn arr(&self, a: &Expr) -> Result<Arc<Array>, ErrorKind> {
        match self.eval(a) {
            Ev::V(Value::Err(k)) => Err(k),
            ev => self.to_array(ev),
        }
    }

    fn lambda(&self, a: &Expr) -> Result<Arc<Lambda>, ErrorKind> {
        match self.eval(a) {
            Ev::F(l) => Ok(l),
            Ev::V(Value::Err(k)) => Err(k),
            _ => Err(ErrorKind::Value),
        }
    }

    fn int(&self, args: &[Expr], i: usize, default: i64) -> Result<i64, ErrorKind> {
        match args.get(i) {
            Some(a) if !missing(a) => {
                let x = self.number(&self.scalar(a))?.trunc();
                if x.abs() > 1e15 {
                    return Err(ErrorKind::Num);
                }
                Ok(x as i64)
            }
            _ => Ok(default),
        }
    }

    fn num_or(&self, args: &[Expr], i: usize, default: f64) -> Result<f64, ErrorKind> {
        match args.get(i) {
            Some(a) if !missing(a) => self.number(&self.scalar(a)),
            _ => Ok(default),
        }
    }

    fn flag(&self, args: &[Expr], i: usize) -> Result<bool, ErrorKind> {
        match args.get(i) {
            Some(a) if !missing(a) => to_bool(&self.scalar(a)),
            _ => Ok(false),
        }
    }
}

macro_rules! tri {
    ($x:expr) => {
        match $x {
            Ok(v) => v,
            Err(k) => return Ev::err(k),
        }
    };
}

fn mk(rows: usize, cols: usize, vals: Vec<Value>) -> Ev {
    if rows == 0 || cols == 0 {
        return Ev::err(ErrorKind::Calc);
    }
    Ev::A(Arc::new(Array { rows, cols, vals }))
}

fn size(rows: i64, cols: i64) -> Result<(usize, usize), ErrorKind> {
    if rows < 1 || cols < 1 {
        return Err(ErrorKind::Calc);
    }
    if rows as u64 * cols as u64 > MAX_ARRAY {
        return Err(ErrorKind::Limit);
    }
    Ok((rows as usize, cols as usize))
}

fn transposed(a: &Array) -> Array {
    let mut vals = Vec::with_capacity(a.vals.len());
    for c in 0..a.cols {
        for r in 0..a.rows {
            vals.push(a.vals[r * a.cols + c].clone());
        }
    }
    Array { rows: a.cols, cols: a.rows, vals }
}

fn row_of(a: &Array, r: usize) -> &[Value] {
    &a.vals[r * a.cols..(r + 1) * a.cols]
}

// ---------- making arrays ----------

pub(super) fn sequence(c: &Ctx, a: &[Expr]) -> Ev {
    let rows = tri!(c.int(a, 0, 1));
    let cols = tri!(c.int(a, 1, 1));
    let start = tri!(c.num_or(a, 2, 1.0));
    let step = tri!(c.num_or(a, 3, 1.0));
    let (rows, cols) = tri!(size(rows, cols));
    mk(rows, cols, (0..rows * cols).map(|i| Value::num(start + step * i as f64)).collect())
}

pub(super) fn randarray(c: &Ctx, a: &[Expr]) -> Ev {
    let rows = tri!(c.int(a, 0, 1));
    let cols = tri!(c.int(a, 1, 1));
    let lo = tri!(c.num_or(a, 2, 0.0));
    let hi = tri!(c.num_or(a, 3, 1.0));
    let whole = tri!(c.flag(a, 4));
    if lo > hi {
        return Ev::err(ErrorKind::Value);
    }
    let (rows, cols) = tri!(size(rows, cols));
    let (lo2, hi2) = (lo.ceil(), hi.floor());
    if whole && lo2 > hi2 {
        return Ev::err(ErrorKind::Value);
    }
    mk(rows, cols, (0..rows * cols).map(|_| Value::num(if whole { lo2 + (funcs::rng() * (hi2 - lo2 + 1.0)).floor() } else { lo + funcs::rng() * (hi - lo) })).collect())
}

pub(super) fn makearray(c: &Ctx, a: &[Expr]) -> Ev {
    let rows = tri!(c.int(a, 0, 1));
    let cols = tri!(c.int(a, 1, 1));
    let (rows, cols) = tri!(size(rows, cols));
    let l = tri!(c.lambda(&a[2]));
    if l.params.len() != 2 {
        return Ev::err(ErrorKind::Value);
    }
    let mut vals = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        for k in 0..cols {
            vals.push(one(c, c.call_with(&l, vec![Some(Ev::V(Value::Num(r as f64 + 1.0))), Some(Ev::V(Value::Num(k as f64 + 1.0)))])));
        }
    }
    mk(rows, cols, vals)
}

// ---------- reshaping ----------

pub(super) fn transpose(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.arr(&a[0]));
    Ev::A(Arc::new(transposed(&x)))
}

fn stack(c: &Ctx, a: &[Expr], vertical: bool) -> Ev {
    let mut parts = Vec::with_capacity(a.len());
    for x in a {
        let p = tri!(c.arr(x));
        parts.push(if vertical { p } else { Arc::new(transposed(&p)) });
    }
    let cols = parts.iter().map(|p| p.cols).max().unwrap_or(1);
    let rows: usize = parts.iter().map(|p| p.rows).sum();
    if (rows * cols) as u64 > MAX_ARRAY {
        return Ev::err(ErrorKind::Limit);
    }
    let mut vals = Vec::with_capacity(rows * cols);
    for p in &parts {
        for r in 0..p.rows {
            for k in 0..cols {
                vals.push(if k < p.cols { p.vals[r * p.cols + k].clone() } else { Value::Err(ErrorKind::NA) });
            }
        }
    }
    let out = Array { rows, cols, vals };
    Ev::A(Arc::new(if vertical { out } else { transposed(&out) }))
}

pub(super) fn vstack(c: &Ctx, a: &[Expr]) -> Ev {
    stack(c, a, true)
}
pub(super) fn hstack(c: &Ctx, a: &[Expr]) -> Ev {
    stack(c, a, false)
}

/// Rows or columns picked by index: `TAKE`, `DROP`, `CHOOSEROWS`, `CHOOSECOLS`.
fn pick(x: &Array, rows: &[usize], cols: &[usize]) -> Ev {
    let mut vals = Vec::with_capacity(rows.len() * cols.len());
    for &r in rows {
        for &k in cols {
            vals.push(x.vals[r * x.cols + k].clone());
        }
    }
    mk(rows.len(), cols.len(), vals)
}

fn take_range(n: usize, k: Option<i64>, drop: bool) -> Result<Vec<usize>, ErrorKind> {
    let Some(k) = k else { return Ok((0..n).collect()) };
    if k == 0 && !drop {
        return Err(ErrorKind::Calc);
    }
    let m = (k.unsigned_abs() as usize).min(n);
    let r: Vec<usize> = match (drop, k >= 0) {
        (false, true) => (0..m).collect(),
        (false, false) => (n - m..n).collect(),
        (true, true) => (m..n).collect(),
        (true, false) => (0..n - m).collect(),
    };
    if r.is_empty() {
        return Err(ErrorKind::Calc);
    }
    Ok(r)
}

fn take_drop(c: &Ctx, a: &[Expr], drop: bool) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let opt = |i: usize| -> Result<Option<i64>, ErrorKind> {
        match a.get(i) {
            Some(e) if !missing(e) => c.int(a, i, 0).map(Some),
            _ => Ok(None),
        }
    };
    let rows = tri!(take_range(x.rows, tri!(opt(1)), drop));
    let cols = tri!(take_range(x.cols, tri!(opt(2)), drop));
    pick(&x, &rows, &cols)
}

pub(super) fn take(c: &Ctx, a: &[Expr]) -> Ev {
    take_drop(c, a, false)
}
pub(super) fn drop(c: &Ctx, a: &[Expr]) -> Ev {
    take_drop(c, a, true)
}

fn choose_lines(c: &Ctx, a: &[Expr], rows: bool) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let n = if rows { x.rows } else { x.cols };
    let mut idx = Vec::new();
    for e in &a[1..] {
        let v = tri!(c.arr(e));
        for k in v.vals.iter() {
            let i = tri!(to_num(k)).trunc();
            if i == 0.0 || i.abs() > n as f64 {
                return Ev::err(ErrorKind::Value);
            }
            idx.push(if i > 0.0 { i as usize - 1 } else { n - (-i) as usize });
        }
    }
    let all: Vec<usize> = (0..if rows { x.cols } else { x.rows }).collect();
    if rows {
        pick(&x, &idx, &all)
    } else {
        pick(&x, &all, &idx)
    }
}

pub(super) fn chooserows(c: &Ctx, a: &[Expr]) -> Ev {
    choose_lines(c, a, true)
}
pub(super) fn choosecols(c: &Ctx, a: &[Expr]) -> Ev {
    choose_lines(c, a, false)
}

fn to_line(c: &Ctx, a: &[Expr], col: bool) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let ignore = tri!(c.int(a, 1, 0));
    if !(0..=3).contains(&ignore) {
        return Ev::err(ErrorKind::Value);
    }
    let by_col = tri!(c.flag(a, 2));
    let x = if by_col { Arc::new(transposed(&x)) } else { x };
    let vals: Vec<Value> = x
        .vals
        .iter()
        .filter(|v| !(ignore & 1 != 0 && v.is_empty() || ignore & 2 != 0 && matches!(v, Value::Err(_))))
        .cloned()
        .collect();
    let n = vals.len();
    if col {
        mk(n, 1, vals)
    } else {
        mk(1, n, vals)
    }
}

pub(super) fn tocol(c: &Ctx, a: &[Expr]) -> Ev {
    to_line(c, a, true)
}
pub(super) fn torow(c: &Ctx, a: &[Expr]) -> Ev {
    to_line(c, a, false)
}

fn wrap(c: &Ctx, a: &[Expr], rows: bool) -> Ev {
    let x = tri!(c.arr(&a[0]));
    if x.rows > 1 && x.cols > 1 {
        return Ev::err(ErrorKind::Value);
    }
    let w = tri!(c.int(a, 1, 1));
    if w < 1 {
        return Ev::err(ErrorKind::Num);
    }
    let pad = match a.get(2) {
        Some(e) if !missing(e) => c.scalar(e),
        _ => Value::Err(ErrorKind::NA),
    };
    let w = (w as usize).min(x.vals.len().max(1));
    let lines = x.vals.len().div_ceil(w);
    let mut vals = Vec::with_capacity(lines * w);
    for i in 0..lines * w {
        vals.push(x.vals.get(i).cloned().unwrap_or_else(|| pad.clone()));
    }
    let out = Array { rows: lines, cols: w, vals };
    Ev::A(Arc::new(if rows { out } else { transposed(&out) }))
}

pub(super) fn wraprows(c: &Ctx, a: &[Expr]) -> Ev {
    wrap(c, a, true)
}
pub(super) fn wrapcols(c: &Ctx, a: &[Expr]) -> Ev {
    wrap(c, a, false)
}

pub(super) fn expand(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let rows = tri!(c.int(a, 1, x.rows as i64));
    let cols = tri!(c.int(a, 2, x.cols as i64));
    if rows < x.rows as i64 || cols < x.cols as i64 {
        return Ev::err(ErrorKind::Value);
    }
    let (rows, cols) = tri!(size(rows, cols));
    let pad = match a.get(3) {
        Some(e) if !missing(e) => c.scalar(e),
        _ => Value::Err(ErrorKind::NA),
    };
    let mut vals = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        for k in 0..cols {
            vals.push(if r < x.rows && k < x.cols { x.vals[r * x.cols + k].clone() } else { pad.clone() });
        }
    }
    mk(rows, cols, vals)
}

// ---------- filtering, sorting, unique ----------

pub(super) fn filter(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let inc = tri!(c.arr(&a[1]));
    let by_rows = inc.cols == 1 && inc.rows == x.rows;
    let by_cols = inc.rows == 1 && inc.cols == x.cols;
    if !by_rows && !by_cols {
        return Ev::err(ErrorKind::Value);
    }
    let mut keep = Vec::new();
    for (i, v) in inc.vals.iter().enumerate() {
        let t = match v {
            Value::Err(k) => return Ev::err(*k),
            Value::Str(_) => return Ev::err(ErrorKind::Value),
            v => tri!(to_bool(v)),
        };
        if t {
            keep.push(i);
        }
    }
    if keep.is_empty() {
        return match a.get(2) {
            Some(e) if !missing(e) => c.eval(e),
            _ => Ev::err(ErrorKind::Calc),
        };
    }
    if by_rows {
        pick(&x, &keep, &(0..x.cols).collect::<Vec<_>>())
    } else {
        pick(&x, &(0..x.rows).collect::<Vec<_>>(), &keep)
    }
}

/// Excel's sort order: numbers, text, logicals, errors; blanks always last.
fn sort_cmp(a: &Value, b: &Value, desc: bool) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => {
            let o = match (a, b) {
                (Value::Err(_), Value::Err(_)) => Ordering::Equal,
                _ => compare(a, b),
            };
            if desc {
                o.reverse()
            } else {
                o
            }
        }
    }
}

fn order_of(v: &Value) -> Result<bool, ErrorKind> {
    match to_num(v)? {
        x if x == 1.0 => Ok(false),
        x if x == -1.0 => Ok(true),
        _ => Err(ErrorKind::Value),
    }
}

pub(super) fn sort(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let by_col = tri!(c.flag(a, 3));
    let x = if by_col { Arc::new(transposed(&x)) } else { x };
    let idx: Vec<usize> = match a.get(1) {
        Some(e) if !missing(e) => {
            let v = tri!(c.arr(e));
            let mut out = Vec::new();
            for k in v.vals.iter() {
                let i = tri!(to_num(k)).trunc();
                if i < 1.0 || i > x.cols as f64 {
                    return Ev::err(ErrorKind::Value);
                }
                out.push(i as usize - 1);
            }
            out
        }
        _ => vec![0],
    };
    let ord: Vec<bool> = match a.get(2) {
        Some(e) if !missing(e) => {
            let v = tri!(c.arr(e));
            let mut out = Vec::new();
            for k in v.vals.iter() {
                out.push(tri!(order_of(k)));
            }
            out
        }
        _ => vec![false],
    };
    let mut rows: Vec<usize> = (0..x.rows).collect();
    rows.sort_by(|&p, &q| {
        for (j, &k) in idx.iter().enumerate() {
            let desc = *ord.get(j).or(ord.last()).unwrap_or(&false);
            let o = sort_cmp(&x.vals[p * x.cols + k], &x.vals[q * x.cols + k], desc);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    let out = pick(&x, &rows, &(0..x.cols).collect::<Vec<_>>());
    match (by_col, out) {
        (true, Ev::A(o)) => Ev::A(Arc::new(transposed(&o))),
        (_, o) => o,
    }
}

pub(super) fn sortby(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let mut keys: Vec<(Arc<Array>, bool)> = Vec::new();
    let mut i = 1;
    let mut by_cols = None;
    while i < a.len() {
        let k = tri!(c.arr(&a[i]));
        let cols = k.rows == 1 && k.cols == x.cols && x.cols > 1;
        let rows = k.cols == 1 && k.rows == x.rows;
        let this = if rows { false } else if cols { true } else { return Ev::err(ErrorKind::Value) };
        if by_cols.is_some_and(|b| b != this) {
            return Ev::err(ErrorKind::Value);
        }
        by_cols = Some(this);
        let desc = match a.get(i + 1) {
            Some(e) if !missing(e) => tri!(order_of(&c.scalar(e))),
            _ => false,
        };
        keys.push((k, desc));
        i += 2;
    }
    let by_cols = by_cols.unwrap_or(false);
    let n = if by_cols { x.cols } else { x.rows };
    let mut ix: Vec<usize> = (0..n).collect();
    ix.sort_by(|&p, &q| {
        for (k, desc) in &keys {
            let o = sort_cmp(&k.vals[p], &k.vals[q], *desc);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    if by_cols {
        pick(&x, &(0..x.rows).collect::<Vec<_>>(), &ix)
    } else {
        pick(&x, &ix, &(0..x.cols).collect::<Vec<_>>())
    }
}

fn same_line(a: &[Value], b: &[Value]) -> bool {
    a.iter().zip(b).all(|(p, q)| match (p, q) {
        (Value::Err(x), Value::Err(y)) => x == y,
        (Value::Err(_), _) | (_, Value::Err(_)) => false,
        (Value::Str(_), Value::Num(_)) | (Value::Num(_), Value::Str(_)) => false,
        _ => p.is_empty() == q.is_empty() && compare(p, q) == Ordering::Equal,
    })
}

pub(super) fn unique(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let by_col = tri!(c.flag(a, 1));
    let once = tri!(c.flag(a, 2));
    let x = if by_col { Arc::new(transposed(&x)) } else { x };
    // First occurrences, with counts (lines compare case-insensitively).
    let mut firsts: Vec<(usize, usize)> = Vec::new();
    let mut buckets: std::collections::HashMap<u64, Vec<usize>> = std::collections::HashMap::new();
    for r in 0..x.rows {
        let line = row_of(&x, r);
        let h = line_hash(line);
        let b = buckets.entry(h).or_default();
        match b.iter().find(|&&f| same_line(row_of(&x, firsts[f].0), line)) {
            Some(&f) => firsts[f].1 += 1,
            None => {
                b.push(firsts.len());
                firsts.push((r, 1));
            }
        }
    }
    let rows: Vec<usize> = firsts.iter().filter(|(_, n)| !once || *n == 1).map(|(r, _)| *r).collect();
    if rows.is_empty() {
        return Ev::err(ErrorKind::Calc);
    }
    let out = pick(&x, &rows, &(0..x.cols).collect::<Vec<_>>());
    match (by_col, out) {
        (true, Ev::A(o)) => Ev::A(Arc::new(transposed(&o))),
        (_, o) => o,
    }
}

fn line_hash(line: &[Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in line {
        match v {
            Value::Empty => 0u8.hash(&mut h),
            Value::Num(x) => (1u8, if *x == 0.0 { 0 } else { x.to_bits() }).hash(&mut h),
            Value::Str(s) => (2u8, funcs::text_key(s)).hash(&mut h),
            Value::Bool(b) => (3u8, *b).hash(&mut h),
            Value::Err(k) => (4u8, *k as u8).hash(&mut h),
        }
    }
    h.finish()
}

// ---------- LET, LAMBDA and the functions that call lambdas ----------

pub(super) fn let_(c: &Ctx, a: &[Expr]) -> Ev {
    let mut cx = c.with(c.scope.clone());
    for p in a[..a.len() - 1].chunks(2) {
        let Expr::Name(n) = &p[0] else { return Ev::err(ErrorKind::Value) };
        let v = cx.eval(&p[1]);
        cx = cx.with(cx.bind(n.clone(), v, false));
    }
    cx.eval(&a[a.len() - 1])
}

pub(super) fn lambda(c: &Ctx, a: &[Expr]) -> Ev {
    let mut params = Vec::with_capacity(a.len() - 1);
    for p in &a[..a.len() - 1] {
        let Expr::Name(n) = p else { return Ev::err(ErrorKind::Value) };
        if params.iter().any(|q: &Arc<str>| q.eq_ignore_ascii_case(n)) {
            return Ev::err(ErrorKind::Value);
        }
        params.push(n.clone());
    }
    Ev::F(Arc::new(Lambda { params, body: a[a.len() - 1].clone(), scope: c.scope.clone() }))
}

pub(super) fn isomitted(c: &Ctx, a: &[Expr]) -> Ev {
    match &a[0] {
        Expr::Name(n) => Ev::V(Value::Bool(c.local(n).is_some_and(|s| s.omitted))),
        _ => Ev::V(Value::Bool(false)),
    }
}

pub(super) fn map(c: &Ctx, a: &[Expr]) -> Ev {
    let l = tri!(c.lambda(&a[a.len() - 1]));
    if l.params.len() != a.len() - 1 {
        return Ev::err(ErrorKind::Value);
    }
    let mut arrs = Vec::with_capacity(a.len() - 1);
    for e in &a[..a.len() - 1] {
        arrs.push(tri!(c.arr(e)));
    }
    let rows = arrs.iter().map(|x| x.rows).max().unwrap_or(1);
    let cols = arrs.iter().map(|x| x.cols).max().unwrap_or(1);
    let mut vals = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        for k in 0..cols {
            let args = arrs.iter().map(|x| Some(Ev::V(at(x, r, k)))).collect();
            vals.push(one(c, c.call_with(&l, args)));
        }
    }
    mk(rows, cols, vals)
}

fn fold(c: &Ctx, a: &[Expr], keep: bool) -> Ev {
    let (init, arr, f) = match a.len() {
        2 => (Ev::V(Value::Empty), &a[0], &a[1]),
        _ => (if missing(&a[0]) { Ev::V(Value::Empty) } else { c.eval(&a[0]) }, &a[1], &a[2]),
    };
    let x = tri!(c.arr(arr));
    let l = tri!(c.lambda(f));
    if l.params.len() != 2 {
        return Ev::err(ErrorKind::Value);
    }
    let mut acc = init;
    let mut vals = Vec::with_capacity(if keep { x.vals.len() } else { 0 });
    for v in x.vals.iter() {
        acc = c.call_with(&l, vec![Some(acc), Some(Ev::V(v.clone()))]);
        if keep {
            vals.push(one(c, acc.clone()));
        }
    }
    if keep {
        mk(x.rows, x.cols, vals)
    } else {
        acc
    }
}

pub(super) fn reduce(c: &Ctx, a: &[Expr]) -> Ev {
    fold(c, a, false)
}
pub(super) fn scan(c: &Ctx, a: &[Expr]) -> Ev {
    fold(c, a, true)
}

fn by_line(c: &Ctx, a: &[Expr], rows: bool) -> Ev {
    let x = tri!(c.arr(&a[0]));
    let l = tri!(c.lambda(&a[1]));
    if l.params.len() != 1 {
        return Ev::err(ErrorKind::Value);
    }
    let x = if rows { x } else { Arc::new(transposed(&x)) };
    let mut vals = Vec::with_capacity(x.rows);
    for r in 0..x.rows {
        let line = Arc::new(Array { rows: 1, cols: x.cols, vals: row_of(&x, r).to_vec() });
        let line = if rows { line } else { Arc::new(transposed(&line)) };
        vals.push(one(c, c.call_with(&l, vec![Some(Ev::A(line))])));
    }
    if rows {
        mk(x.rows, 1, vals)
    } else {
        mk(1, x.rows, vals)
    }
}

pub(super) fn byrow(c: &Ctx, a: &[Expr]) -> Ev {
    by_line(c, a, true)
}
pub(super) fn bycol(c: &Ctx, a: &[Expr]) -> Ev {
    by_line(c, a, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::parse;

    fn p(s: &str) -> Expr {
        parse(s, 0, 0, &|_: &str| None).unwrap()
    }

    #[test]
    fn what_can_spill() {
        assert!(can_array(&p("=A1:A3")));
        assert!(can_array(&p("=A1:A3*2")));
        assert!(can_array(&p("=ABS(A1:A3)")));
        assert!(can_array(&p("=SEQUENCE(3)")));
        assert!(!can_array(&p("=SUM(A1:A3)")));
        assert!(!can_array(&p("=A1+1")));
        assert!(!can_array(&p("=ABS(A1)")));
        assert!(!can_array(&p("=@A1:A3")));
        assert_eq!(legacy(p("=A1:A3*2")), p("=@A1:A3*2"));
        assert_eq!(legacy(p("=IF(A1:A3>1,B1:B3,0)")), p("=IF(@A1:A3>1,@B1:B3,0)"));
        assert_eq!(legacy(p("=INDEX(A1:B3,0,1)")), p("=@INDEX(A1:B3,0,1)"));
        for f in ["=@A1:A3*2", "=IF(@A1:A3>1,@B1:B3,0)", "=SUM(A1:A3)", "=@A1:A3"] {
            let (u, dynamic) = for_file(&p(f));
            assert!(!dynamic, "{f}");
            assert_eq!(legacy(u), p(f));
        }
        // An `@` a file from before dynamic arrays wouldn't imply stays (as SINGLE).
        assert_eq!(for_file(&p("=SUM(@A1:A3)")), (p("=SUM(@A1:A3)"), false));
        assert!(for_file(&p("=A1:A3")).1);
        assert_eq!(legacy(p("=A1:A3")), p("=@A1:A3"));
        assert_eq!(legacy(p("=SUM(A1:A3)")), p("=SUM(A1:A3)"));
    }
}
