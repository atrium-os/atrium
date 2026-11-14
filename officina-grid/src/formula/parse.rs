//! Lexer and recursive-descent parser for Excel formula syntax (en-US separators).
//!
//! Bounded: at most [`MAX_FORMULA_LEN`] bytes and [`MAX_DEPTH`] levels of nesting, at
//! most 255 arguments to a call; anything over is a [`ParseError`], never a stack
//! overflow or a panic (fuzzed in tests/fuzz.rs).
//!
//! Precedence, lowest first: comparisons, `&`, `+ -`, `* /`, `^` (left-associative, as in
//! Excel), prefix `- +`, postfix `%`, then references, ranges (`:`), calls and literals.
//! Excel's unary minus binds tighter than `^`: `-2^2` is 4.

use super::ast::*;
use crate::addr::{parse_col, parse_row, MAX_COLS, MAX_ROWS};
use crate::eval::funcs;
use crate::value::ErrorKind;
use std::sync::Arc;

pub const MAX_FORMULA_LEN: usize = 8192;
pub const MAX_DEPTH: usize = 64;
pub const MAX_ARGS: usize = 255;
/// Cells in one array constant.
pub const MAX_ARRAY_CONST: usize = 65_536;

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub at: usize,
    pub msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at {})", self.msg, self.at)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Err(ErrorKind),
    /// A word: a reference, a name, a function name, TRUE/FALSE.
    Word(String),
    /// `Sheet1!` or `'My sheet'!`
    Sheet(String),
    /// `3:5`, `$3:$5` (whole rows), lexed as one token so it isn't a number
    Rows(String, String),
    Op(&'static str),
}

fn err<T>(at: usize, msg: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { at, msg: msg.into() })
}

fn lex(s: &str) -> Result<Vec<(usize, Tok)>, ParseError> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'"' => {
                let mut t = String::new();
                i += 1;
                loop {
                    if i >= b.len() {
                        return err(start, "text without its closing quote");
                    }
                    if b[i] == b'"' {
                        if i + 1 < b.len() && b[i + 1] == b'"' {
                            t.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch = s[i..].chars().next().unwrap();
                    t.push(ch);
                    i += ch.len_utf8();
                }
                out.push((start, Tok::Str(t)));
            }
            b'\'' => {
                // 'Sheet name'!  ('' inside is a quote)
                let mut t = String::new();
                i += 1;
                loop {
                    if i >= b.len() {
                        return err(start, "sheet name without its closing quote");
                    }
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            t.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch = s[i..].chars().next().unwrap();
                    t.push(ch);
                    i += ch.len_utf8();
                }
                if i >= b.len() || b[i] != b'!' {
                    return err(i, "expected ! after a quoted sheet name");
                }
                i += 1;
                out.push((start, Tok::Sheet(t)));
            }
            b'#' => {
                let rest = &s[i..];
                let mut found = None;
                for e in ["#DIV/0!", "#N/A", "#NAME?", "#NULL!", "#NUM!", "#REF!", "#VALUE!", "#CIRC!", "#LIMIT!"] {
                    if rest.as_bytes().get(..e.len()).is_some_and(|p| p.eq_ignore_ascii_case(e.as_bytes())) {
                        found = Some((e.len(), ErrorKind::from_text(e).unwrap()));
                        break;
                    }
                }
                let (n, e) = found.ok_or(ParseError { at: i, msg: "unknown error value".into() })?;
                i += n;
                out.push((start, Tok::Err(e)));
            }
            b'0'..=b'9' | b'.' | b'$' => {
                // Whole rows first: $?digits:$?digits
                if let Some((n, a, z)) = rows_token(&s[i..]) {
                    i += n;
                    out.push((start, Tok::Rows(a, z)));
                    continue;
                }
                if c == b'$' {
                    let n = word_len(&s[i..]);
                    out.push((start, Tok::Word(s[i..i + n].to_string())));
                    i += n;
                    continue;
                }
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j < b.len() && b[j] == b'.' {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
                    let mut k = j + 1;
                    if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                        k += 1;
                    }
                    if k < b.len() && b[k].is_ascii_digit() {
                        while k < b.len() && b[k].is_ascii_digit() {
                            k += 1;
                        }
                        j = k;
                    }
                }
                let txt = &s[i..j];
                if txt == "." {
                    return err(i, "a lone '.'");
                }
                // Digits followed by letters (e.g. "1A") is not a number.
                if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                    return err(i, "a number runs into a name");
                }
                let v: f64 = txt.parse().map_err(|_| ParseError { at: i, msg: "bad number".into() })?;
                if !v.is_finite() {
                    return err(i, "number too large");
                }
                out.push((start, Tok::Num(v)));
                i = j;
            }
            b'<' => {
                if s[i..].starts_with("<=") {
                    out.push((start, Tok::Op("<=")));
                    i += 2;
                } else if s[i..].starts_with("<>") {
                    out.push((start, Tok::Op("<>")));
                    i += 2;
                } else {
                    out.push((start, Tok::Op("<")));
                    i += 1;
                }
            }
            b'>' => {
                if s[i..].starts_with(">=") {
                    out.push((start, Tok::Op(">=")));
                    i += 2;
                } else {
                    out.push((start, Tok::Op(">")));
                    i += 1;
                }
            }
            b'+' | b'-' | b'*' | b'/' | b'^' | b'&' | b'=' | b'%' | b'(' | b')' | b',' | b';' | b':' | b'{' | b'}' => {
                let op = match c {
                    b'+' => "+",
                    b'-' => "-",
                    b'*' => "*",
                    b'/' => "/",
                    b'^' => "^",
                    b'&' => "&",
                    b'=' => "=",
                    b'%' => "%",
                    b'(' => "(",
                    b')' => ")",
                    b',' => ",",
                    b';' => ";",
                    b':' => ":",
                    b'{' => "{",
                    _ => "}",
                };
                out.push((start, Tok::Op(op)));
                i += 1;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'\\' || c >= 0x80 => {
                let n = word_len(&s[i..]);
                if n == 0 {
                    // A non-ASCII character that can't start a name (U+FFFD, a symbol).
                    return err(i, format!("unexpected character {:?}", s[i..].chars().next().unwrap()));
                }
                let w = &s[i..i + n];
                i += n;
                if i < b.len() && b[i] == b'!' {
                    i += 1;
                    out.push((start, Tok::Sheet(w.to_string())));
                } else {
                    out.push((start, Tok::Word(w.to_string())));
                }
            }
            _ => return err(i, format!("unexpected character {:?}", s[i..].chars().next().unwrap())),
        }
    }
    Ok(out)
}

fn word_len(s: &str) -> usize {
    s.char_indices()
        .find(|&(_, ch)| !(ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '$' || ch == '\\'))
        .map_or(s.len(), |(i, _)| i)
}

/// `$?digits:$?digits` at the start of `s`, as whole rows.
fn rows_token(s: &str) -> Option<(usize, String, String)> {
    fn part(s: &str) -> Option<usize> {
        let b = s.as_bytes();
        let mut i = 0;
        if i < b.len() && b[i] == b'$' {
            i += 1;
        }
        let d0 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        (i > d0).then_some(i)
    }
    let a = part(s)?;
    if s.as_bytes().get(a) != Some(&b':') {
        return None;
    }
    let z = part(&s[a + 1..])?;
    let end = a + 1 + z;
    // Not followed by more of a word ("1:2A" is not rows).
    if s[end..].starts_with(|c: char| c.is_alphanumeric() || c == '.') {
        return None;
    }
    Some((end, s[..a].to_string(), s[a + 1..end].to_string()))
}

/// The sheet names a formula may refer to.
pub trait Sheets {
    fn sheet_id(&self, name: &str) -> Option<u16>;
}

impl<F: Fn(&str) -> Option<u16>> Sheets for F {
    fn sheet_id(&self, name: &str) -> Option<u16> {
        self(name)
    }
}

struct P<'a> {
    toks: Vec<(usize, Tok)>,
    pos: usize,
    row: u32,
    col: u32,
    depth: usize,
    sheets: &'a dyn Sheets,
    end: usize,
}

/// Parses `src` (with or without a leading `=`) for a formula in cell (`row`, `col`).
pub fn parse(src: &str, row: u32, col: u32, sheets: &dyn Sheets) -> Result<Expr, ParseError> {
    if src.len() > MAX_FORMULA_LEN {
        return err(MAX_FORMULA_LEN, "formula longer than 8192 bytes");
    }
    parse_any_len(src, row, col, sheets)
}

/// The longest stored formula text a file may hold: printing a parsed formula can
/// lengthen it (`.5` prints as `0.5`), so stored text gets more room than typed text.
pub const MAX_STORED_FORMULA_LEN: usize = 4 * MAX_FORMULA_LEN;

/// [`parse`] for formula text read back from a file, which this program printed.
pub fn parse_stored(src: &str, row: u32, col: u32, sheets: &dyn Sheets) -> Result<Expr, ParseError> {
    if src.len() > MAX_STORED_FORMULA_LEN {
        return err(MAX_STORED_FORMULA_LEN, "stored formula too long");
    }
    parse_any_len(src, row, col, sheets)
}

fn parse_any_len(src: &str, row: u32, col: u32, sheets: &dyn Sheets) -> Result<Expr, ParseError> {
    let body = src.strip_prefix('=').unwrap_or(src);
    let off = src.len() - body.len();
    let toks = lex(body).map_err(|e| ParseError { at: e.at + off, ..e })?;
    let mut p = P { toks, pos: 0, row, col, depth: 0, sheets, end: body.len() };
    let e = p.expr().map_err(|e| ParseError { at: e.at + off, ..e })?;
    if p.pos != p.toks.len() {
        return err(p.at() + off, "unexpected text after the formula");
    }
    Ok(e)
}

impl P<'_> {
    fn at(&self) -> usize {
        self.toks.get(self.pos).map_or(self.end, |t| t.0)
    }
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.1)
    }
    fn peek_op(&self) -> Option<&'static str> {
        match self.peek() {
            Some(Tok::Op(o)) => Some(o),
            _ => None,
        }
    }
    fn eat(&mut self, op: &str) -> bool {
        if self.peek_op() == Some(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, op: &str) -> Result<(), ParseError> {
        if self.eat(op) {
            Ok(())
        } else {
            err(self.at(), format!("expected '{op}'"))
        }
    }
    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return err(self.at(), "formula nested too deeply");
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.enter()?;
        let mut a = self.concat()?;
        loop {
            let op = match self.peek_op() {
                Some("=") => BinOp::Eq,
                Some("<>") => BinOp::Ne,
                Some("<") => BinOp::Lt,
                Some("<=") => BinOp::Le,
                Some(">") => BinOp::Gt,
                Some(">=") => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let b = self.concat()?;
            a = Expr::Bin(op, Box::new(a), Box::new(b));
        }
        self.depth -= 1;
        Ok(a)
    }
    fn concat(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.additive()?;
        while self.eat("&") {
            let b = self.additive()?;
            a = Expr::Bin(BinOp::Concat, Box::new(a), Box::new(b));
        }
        Ok(a)
    }
    fn additive(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.mult()?;
        loop {
            let op = match self.peek_op() {
                Some("+") => BinOp::Add,
                Some("-") => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let b = self.mult()?;
            a = Expr::Bin(op, Box::new(a), Box::new(b));
        }
        Ok(a)
    }
    fn mult(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.power()?;
        loop {
            let op = match self.peek_op() {
                Some("*") => BinOp::Mul,
                Some("/") => BinOp::Div,
                _ => break,
            };
            self.pos += 1;
            let b = self.power()?;
            a = Expr::Bin(op, Box::new(a), Box::new(b));
        }
        Ok(a)
    }
    fn power(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.unary()?;
        while self.eat("^") {
            let b = self.unary()?;
            a = Expr::Bin(BinOp::Pow, Box::new(a), Box::new(b));
        }
        Ok(a)
    }
    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.eat("-") {
            self.enter()?;
            let e = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Neg(Box::new(e)));
        }
        if self.eat("+") {
            self.enter()?;
            let e = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Plus(Box::new(e)));
        }
        let mut e = self.primary()?;
        while self.eat("%") {
            e = Expr::Pct(Box::new(e));
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let at = self.at();
        let Some((_, t)) = self.toks.get(self.pos).cloned() else {
            return err(at, "formula ends too soon");
        };
        self.pos += 1;
        match t {
            Tok::Num(v) => Ok(Expr::Num(v)),
            Tok::Str(s) => Ok(Expr::Str(Arc::from(crate::strings::clip(&s)))),
            Tok::Err(e) => Ok(Expr::Err(e)),
            Tok::Op("(") => {
                let e = self.expr()?;
                self.expect(")")?;
                Ok(Expr::Paren(Box::new(e)))
            }
            Tok::Op("{") => self.array(),
            Tok::Sheet(name) => {
                let sheet = self.sheets.sheet_id(&name);
                let next = self.toks.get(self.pos).cloned();
                self.pos += 1;
                let e = match next {
                    Some((_, Tok::Word(w))) => self.word_ref(Some(sheet), &w, at)?,
                    Some((_, Tok::Rows(a, z))) => self.rows(Some(sheet), &a, &z, at)?,
                    _ => return err(at, "expected a reference after the sheet name"),
                };
                Ok(e)
            }
            Tok::Rows(a, z) => self.rows(None, &a, &z, at),
            Tok::Word(w) => {
                if self.peek_op() == Some("(") {
                    self.pos += 1;
                    return self.call(&w, at);
                }
                let u = w.to_ascii_uppercase();
                if u == "TRUE" {
                    return Ok(Expr::Bool(true));
                }
                if u == "FALSE" {
                    return Ok(Expr::Bool(false));
                }
                self.word_ref(None, &w, at)
            }
            Tok::Op(o) => err(at, format!("unexpected '{o}'")),
        }
    }

    /// The sheet for a reference: `None` for the formula's sheet, `Some(None)` for a
    /// sheet name that doesn't exist (a `#REF!`).
    fn word_ref(&mut self, sheet: Option<Option<u16>>, w: &str, at: usize) -> Result<Expr, ParseError> {
        let sh = match sheet {
            None => None,
            Some(Some(id)) => Some(id),
            Some(None) => return self.skip_range_tail().map(|_| Expr::Err(ErrorKind::Ref)),
        };
        if let Some((r, c)) = self.cell(w) {
            if self.eat(":") {
                let at2 = self.at();
                let Some((_, Tok::Word(w2))) = self.toks.get(self.pos).cloned() else {
                    return err(at2, "expected the end of the range");
                };
                self.pos += 1;
                let (r2, c2) = self.cell(&w2).ok_or(ParseError { at: at2, msg: "bad end of range".into() })?;
                return Ok(Expr::Range(RangeRef { sheet: sh, r0: r, c0: c, r1: r2, c1: c2, kind: RangeKind::Cells }));
            }
            return Ok(Expr::Ref(CellRef { sheet: sh, row: r, col: c }));
        }
        if let Some(c) = self.colpart(w) {
            if self.eat(":") {
                let at2 = self.at();
                let Some((_, Tok::Word(w2))) = self.toks.get(self.pos).cloned() else {
                    return err(at2, "expected the end of the column range");
                };
                self.pos += 1;
                let c2 = self.colpart(&w2).ok_or(ParseError { at: at2, msg: "bad end of column range".into() })?;
                return Ok(Expr::Range(RangeRef::whole_cols(sh, c, c2)));
            }
        }
        if sheet.is_some() {
            return err(at, "expected a cell or range after the sheet name");
        }
        if w.contains('$') {
            return err(at, format!("not a reference: {w}"));
        }
        Ok(Expr::Name(Arc::from(w)))
    }

    fn skip_range_tail(&mut self) -> Result<(), ParseError> {
        if self.eat(":") {
            self.pos += 1;
        }
        Ok(())
    }

    fn rows(&mut self, sheet: Option<Option<u16>>, a: &str, z: &str, at: usize) -> Result<Expr, ParseError> {
        let r0 = self.rowpart(a).ok_or(ParseError { at, msg: "bad row".into() })?;
        let r1 = self.rowpart(z).ok_or(ParseError { at, msg: "bad row".into() })?;
        match sheet {
            Some(None) => Ok(Expr::Err(ErrorKind::Ref)),
            Some(Some(s)) => Ok(Expr::Range(RangeRef::whole_rows(Some(s), r0, r1))),
            None => Ok(Expr::Range(RangeRef::whole_rows(None, r0, r1))),
        }
    }

    fn colpart(&self, s: &str) -> Option<Coord> {
        let (abs, s) = match s.strip_prefix('$') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let c = parse_col(s)?;
        Some(if abs { Coord::Abs(c) } else { Coord::Rel(c as i32 - self.col as i32) })
    }
    fn rowpart(&self, s: &str) -> Option<Coord> {
        let (abs, s) = match s.strip_prefix('$') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let r = parse_row(s)?;
        Some(if abs { Coord::Abs(r) } else { Coord::Rel(r as i64 as i32 - self.row as i64 as i32) })
    }
    /// `$A$1`, `A$1`, `$A1`, `A1` -> (row, col)
    fn cell(&self, w: &str) -> Option<(Coord, Coord)> {
        let b = w.as_bytes();
        let mut i = 0;
        if i < b.len() && b[i] == b'$' {
            i += 1;
        }
        let l0 = i;
        while i < b.len() && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i == l0 {
            return None;
        }
        let colpart = &w[..i];
        if i < b.len() && b[i] == b'$' {
            i += 1;
        }
        let d0 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == d0 || i != b.len() {
            return None;
        }
        let rowpart = &w[colpart.len()..];
        let c = self.colpart(colpart)?;
        let r = self.rowpart(rowpart)?;
        // Relative offsets must fit; rows to 2^31 fit in i32 offsets both ways.
        let _ = (MAX_ROWS, MAX_COLS);
        Some((r, c))
    }

    fn call(&mut self, name: &str, at: usize) -> Result<Expr, ParseError> {
        self.enter()?;
        let mut u = name.to_ascii_uppercase();
        for p in ["_XLFN._XLWS.", "_XLFN.", "_XLWS."] {
            if let Some(r) = u.strip_prefix(p) {
                u = r.to_string();
            }
        }
        let mut args = Vec::new();
        if !self.eat(")") {
            loop {
                // An empty argument (`IF(A1,,2)`) is a missing value.
                if matches!(self.peek_op(), Some(",") | Some(")")) {
                    args.push(Expr::Name(Arc::from("")));
                } else {
                    args.push(self.expr()?);
                }
                if args.len() > MAX_ARGS {
                    return err(at, "more than 255 arguments");
                }
                if self.eat(",") {
                    continue;
                }
                self.expect(")")?;
                break;
            }
        }
        self.depth -= 1;
        let f = match funcs::lookup(&u) {
            Some(id) => {
                let spec = &funcs::FUNCS[id as usize];
                if args.len() < spec.min as usize || args.len() > spec.max as usize {
                    return err(at, format!("{} takes {}{} arguments", spec.name, spec.min, if spec.max as usize == MAX_ARGS { " or more".to_string() } else if spec.max == spec.min { String::new() } else { format!(" to {}", spec.max) }));
                }
                Func::Known(id)
            }
            None => Func::Unknown(Arc::from(u.as_str())),
        };
        Ok(Expr::Call(f, args))
    }

    fn array(&mut self) -> Result<Expr, ParseError> {
        let mut rows = vec![Vec::new()];
        let mut n = 0;
        loop {
            let at = self.at();
            let neg = self.eat("-");
            let Some((_, t)) = self.toks.get(self.pos).cloned() else {
                return err(at, "array constant without its closing brace");
            };
            self.pos += 1;
            let e = match t {
                Tok::Num(v) => Expr::Num(if neg { -v } else { v }),
                Tok::Str(s) if !neg => Expr::Str(Arc::from(s.as_str())),
                Tok::Err(e) if !neg => Expr::Err(e),
                Tok::Word(w) if !neg && w.eq_ignore_ascii_case("TRUE") => Expr::Bool(true),
                Tok::Word(w) if !neg && w.eq_ignore_ascii_case("FALSE") => Expr::Bool(false),
                _ => return err(at, "only constants go in an array constant"),
            };
            n += 1;
            if n > MAX_ARRAY_CONST {
                return err(at, "array constant too big");
            }
            rows.last_mut().unwrap().push(e);
            if self.eat(",") {
                continue;
            }
            if self.eat(";") {
                rows.push(Vec::new());
                continue;
            }
            self.expect("}")?;
            break;
        }
        let w = rows[0].len();
        if rows.iter().any(|r| r.len() != w) {
            return err(self.at(), "array constant rows differ in length");
        }
        Ok(Expr::Array(rows))
    }
}
