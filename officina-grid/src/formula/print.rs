//! Printing an AST back to A1 formula text for a given cell.

use super::ast::*;
use crate::addr::{col_name, MAX_COLS, MAX_ROWS};
use crate::eval::funcs;
use std::sync::Arc;

/// `=...` text of `e` as it reads in cell (`row`, `col`). `sheet_name` names sheets for
/// cross-sheet references (quoted when needed).
pub fn to_a1(e: &Expr, row: u32, col: u32, sheet_name: &dyn Fn(u16) -> String) -> String {
    let mut s = String::from("=");
    pr(e, row, col, &Out { sn: sheet_name, file: None, params: Default::default() }, &mut s);
    s
}

/// [`to_a1`] in the form `.xlsx` files carry: structured references name their table
/// (`table_at(row, col)` names the table around a cell) and spell `@` as
/// `[#This Row]`.
pub fn to_a1_file(e: &Expr, row: u32, col: u32, sheet_name: &dyn Fn(u16) -> String, table_at: &dyn Fn(u32, u32) -> Option<Arc<str>>) -> String {
    let mut s = String::from("=");
    pr(e, row, col, &Out { sn: sheet_name, file: Some(table_at), params: Default::default() }, &mut s);
    s
}

struct Out<'a> {
    sn: &'a dyn Fn(u16) -> String,
    file: Option<&'a dyn Fn(u32, u32) -> Option<Arc<str>>>,
    /// LET names and LAMBDA parameters in scope: files spell them `_xlpm.x`.
    params: std::cell::RefCell<Vec<Arc<str>>>,
}

impl Out<'_> {
    /// The LET name or LAMBDA parameter `n` is, as it was written.
    fn param(&self, n: &str) -> Option<Arc<str>> {
        self.params.borrow().iter().rev().find(|p| p.eq_ignore_ascii_case(n)).cloned()
    }
    fn prefix(&self, s: &mut String) {
        if self.file.is_some() {
            s.push_str("_xlpm.");
        }
    }
}

fn pr(e: &Expr, row: u32, col: u32, o: &Out, s: &mut String) {
    match e {
        Expr::Num(v) => s.push_str(&num(*v)),
        Expr::Str(t) => {
            s.push('"');
            s.push_str(&t.replace('"', "\"\""));
            s.push('"');
        }
        Expr::Bool(b) => s.push_str(if *b { "TRUE" } else { "FALSE" }),
        Expr::Err(k) => s.push_str(k.text()),
        Expr::Name(n) => {
            if o.param(n).is_some() {
                o.prefix(s);
            }
            s.push_str(n)
        }
        Expr::Table(t) => {
            let named = match (o.file, t.table.is_empty()) {
                (Some(at), true) => at(row, col),
                _ => None,
            };
            table(t, named.as_deref(), o.file.is_some(), s);
        }
        Expr::Ref(r) => {
            sheet(r.sheet, o.sn, s);
            coord_col(r.col, col, s);
            coord_row(r.row, row, s);
        }
        Expr::Range(r) => {
            sheet(r.sheet, o.sn, s);
            match r.kind {
                RangeKind::Cells => {
                    coord_col(r.c0, col, s);
                    coord_row(r.r0, row, s);
                    s.push(':');
                    coord_col(r.c1, col, s);
                    coord_row(r.r1, row, s);
                }
                RangeKind::Cols => {
                    coord_col(r.c0, col, s);
                    s.push(':');
                    coord_col(r.c1, col, s);
                }
                RangeKind::Rows => {
                    coord_row(r.r0, row, s);
                    s.push(':');
                    coord_row(r.r1, row, s);
                }
            }
        }
        Expr::Neg(a) => {
            s.push('-');
            pr(a, row, col, o, s);
        }
        Expr::Plus(a) => {
            s.push('+');
            pr(a, row, col, o, s);
        }
        Expr::Pct(a) => {
            pr(a, row, col, o, s);
            s.push('%');
        }
        Expr::Paren(a) => {
            s.push('(');
            pr(a, row, col, o, s);
            s.push(')');
        }
        Expr::Bin(op, a, b) => {
            pr(a, row, col, o, s);
            s.push_str(op.text());
            pr(b, row, col, o, s);
        }
        Expr::Call(f, args) => {
            let name: Arc<str> = match f {
                Func::Known(id) => Arc::from(funcs::FUNCS[*id as usize].name),
                Func::Unknown(n) => match o.param(n) {
                    Some(p) => {
                        o.prefix(s);
                        p
                    }
                    None => n.clone(),
                },
            };
            s.push_str(&name);
            let name = &*name;
            let scoped = o.params.borrow().len();
            let (let_, lambda) = (name == "LET", name == "LAMBDA");
            if lambda {
                for a in &args[..args.len().saturating_sub(1)] {
                    if let Expr::Name(n) = a {
                        o.params.borrow_mut().push(n.clone());
                    }
                }
            }
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                if let (true, 0, Expr::Name(n)) = (let_ && i + 1 < args.len(), i % 2, a) {
                    o.params.borrow_mut().push(n.clone());
                }
                pr(a, row, col, o, s);
            }
            s.push(')');
            o.params.borrow_mut().truncate(scoped);
        }
        Expr::Spill(r) => {
            if o.file.is_some() {
                s.push_str("_xlfn.ANCHORARRAY(");
            }
            sheet(r.sheet, o.sn, s);
            coord_col(r.col, col, s);
            coord_row(r.row, row, s);
            s.push(if o.file.is_some() { ')' } else { '#' });
        }
        Expr::At(a) => {
            if o.file.is_some() {
                s.push_str("_xlfn.SINGLE(");
                pr(a, row, col, o, s);
                s.push(')');
            } else {
                s.push('@');
                pr(a, row, col, o, s);
            }
        }
        Expr::Invoke(g, args) => {
            pr(g, row, col, o, s);
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                pr(a, row, col, o, s);
            }
            s.push(')');
        }
        Expr::Array(rows) => {
            s.push('{');
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    s.push(';');
                }
                for (j, a) in r.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    pr(a, row, col, o, s);
                }
            }
            s.push('}');
        }
    }
}

fn num(v: f64) -> String {
    // Shortest text that reads back as the same f64.
    let t = format!("{}", v);
    if t.len() > 20 {
        format!("{:e}", v).replace('e', "E")
    } else {
        t
    }
}

fn sheet(sh: Option<u16>, sn: &dyn Fn(u16) -> String, s: &mut String) {
    if let Some(id) = sh {
        push_sheet_name(&sn(id), s);
        s.push('!');
    }
}

/// A sheet name as a formula writes it: bare when it reads back as a name,
/// else in single quotes with quotes doubled.
pub(crate) fn push_sheet_name(name: &str, s: &mut String) {
    let plain = !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && crate::addr::parse_a1(name).is_none();
    if plain {
        s.push_str(name);
    } else {
        s.push('\'');
        s.push_str(&name.replace('\'', "''"));
        s.push('\'');
    }
}

fn coord_col(c: Coord, at: u32, s: &mut String) {
    match c {
        Coord::Abs(v) => {
            s.push('$');
            s.push_str(&col_name(v));
        }
        Coord::Rel(d) => match Coord::Rel(d).resolve(at, MAX_COLS) {
            Some(v) => s.push_str(&col_name(v)),
            None => s.push_str("#REF!"),
        },
    }
}

fn coord_row(r: Coord, at: u32, s: &mut String) {
    match r {
        Coord::Abs(v) => {
            s.push('$');
            s.push_str(&(v as u64 + 1).to_string());
        }
        Coord::Rel(d) => match Coord::Rel(d).resolve(at, MAX_ROWS) {
            Some(v) => s.push_str(&(v as u64 + 1).to_string()),
            None => s.push_str("#REF!"),
        },
    }
}

/// A column name inside brackets, with `[ ] # '` escaped by `'`.
fn col_esc(c: &str, s: &mut String) {
    for ch in c.chars() {
        if matches!(ch, '[' | ']' | '#' | '\'') {
            s.push('\'');
        }
        s.push(ch);
    }
}

/// Whether a column name can stand alone in one pair of brackets (`T[Amount]`).
fn simple_col(c: &str) -> bool {
    !c.is_empty() && c.trim() == c && !c.starts_with('#') && !c.chars().any(|ch| matches!(ch, ',' | ':' | '@' | '[' | ']' | '\''))
}

fn cols(c: &(Arc<str>, Arc<str>), s: &mut String) {
    s.push('[');
    col_esc(&c.0, s);
    s.push(']');
    if c.1 != c.0 {
        s.push_str(":[");
        col_esc(&c.1, s);
        s.push(']');
    }
}

fn table(t: &TableRef, named: Option<&str>, file: bool, s: &mut String) {
    s.push_str(named.unwrap_or(&t.table));
    let kw = match t.area {
        TableArea::Data => None,
        TableArea::All => Some("[#All]"),
        TableArea::Headers => Some("[#Headers]"),
        TableArea::Totals => Some("[#Totals]"),
        TableArea::HeadersData => Some("[#Headers],[#Data]"),
        TableArea::DataTotals => Some("[#Data],[#Totals]"),
        TableArea::ThisRow if file => Some("[#This Row]"),
        TableArea::ThisRow => {
            s.push_str("[@");
            match &t.cols {
                Some(c) if c.0 == c.1 && simple_col(&c.0) => s.push_str(&c.0),
                Some(c) => cols(c, s),
                None => {}
            }
            s.push(']');
            return;
        }
    };
    match (kw, &t.cols) {
        (None, None) if t.table.is_empty() && named.is_none() => s.push_str("[#Data]"),
        (None, None) => s.push_str("[]"),
        (None, Some(c)) if c.0 == c.1 && simple_col(&c.0) => {
            s.push('[');
            s.push_str(&c.0);
            s.push(']');
        }
        (Some(k), None) if !k.contains(',') => s.push_str(k),
        (k, c) => {
            s.push('[');
            if let Some(k) = k {
                s.push_str(k);
            }
            if let Some(c) = c {
                if k.is_some() {
                    s.push(',');
                }
                cols(c, s);
            }
            s.push(']');
        }
    }
}
