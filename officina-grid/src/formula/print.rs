//! Printing an AST back to A1 formula text for a given cell.

use super::ast::*;
use crate::addr::{col_name, MAX_COLS, MAX_ROWS};
use crate::eval::funcs;

/// `=...` text of `e` as it reads in cell (`row`, `col`). `sheet_name` names sheets for
/// cross-sheet references (quoted when needed).
pub fn to_a1(e: &Expr, row: u32, col: u32, sheet_name: &dyn Fn(u16) -> String) -> String {
    let mut s = String::from("=");
    pr(e, row, col, sheet_name, &mut s);
    s
}

fn pr(e: &Expr, row: u32, col: u32, sn: &dyn Fn(u16) -> String, s: &mut String) {
    match e {
        Expr::Num(v) => s.push_str(&num(*v)),
        Expr::Str(t) => {
            s.push('"');
            s.push_str(&t.replace('"', "\"\""));
            s.push('"');
        }
        Expr::Bool(b) => s.push_str(if *b { "TRUE" } else { "FALSE" }),
        Expr::Err(k) => s.push_str(k.text()),
        Expr::Name(n) => s.push_str(n),
        Expr::Ref(r) => {
            sheet(r.sheet, sn, s);
            coord_col(r.col, col, s);
            coord_row(r.row, row, s);
        }
        Expr::Range(r) => {
            sheet(r.sheet, sn, s);
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
            pr(a, row, col, sn, s);
        }
        Expr::Plus(a) => {
            s.push('+');
            pr(a, row, col, sn, s);
        }
        Expr::Pct(a) => {
            pr(a, row, col, sn, s);
            s.push('%');
        }
        Expr::Paren(a) => {
            s.push('(');
            pr(a, row, col, sn, s);
            s.push(')');
        }
        Expr::Bin(op, a, b) => {
            pr(a, row, col, sn, s);
            s.push_str(op.text());
            pr(b, row, col, sn, s);
        }
        Expr::Call(f, args) => {
            match f {
                Func::Known(id) => s.push_str(funcs::FUNCS[*id as usize].name),
                Func::Unknown(n) => s.push_str(n),
            }
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                pr(a, row, col, sn, s);
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
                    pr(a, row, col, sn, s);
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
        let name = sn(id);
        let plain = !name.is_empty()
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && crate::addr::parse_a1(&name).is_none();
        if plain {
            s.push_str(&name);
        } else {
            s.push('\'');
            s.push_str(&name.replace('\'', "''"));
            s.push('\'');
        }
        s.push('!');
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
