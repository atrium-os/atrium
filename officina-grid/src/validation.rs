//! Data validation (docs/spec/officina.md §16.83): rules over ranges that say
//! what a cell may hold (a value from a list, a whole or decimal number, a date,
//! a time, a text length, or a formula that must be TRUE) and what the person
//! is told: an input prompt, and an error that stops the entry, warns, or
//! only informs.
//!
//! Formulas are written for the first range's top-left and evaluated at the
//! cell, as conditional formats' are; they move with rows and columns the same
//! way (`crate::shift`).

use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::condfmt::CmpOp;
use crate::eval::{Ctx, Ev};
use crate::formula::{self, Expr};
use crate::value::Value;
use officina_core::wire::{Reader, WireError, Writer};

/// Validation rules per workbook, at most.
pub const MAX_VALIDATIONS: usize = 65_536;
/// Ranges per rule, at most.
pub const MAX_RANGES: usize = 1024;
/// A prompt's or error's title and text, bytes at most (Excel: 32 and 255
/// characters; files from elsewhere may hold more).
pub const MAX_MESSAGE: usize = 1024;
/// Items a list offers, at most.
pub const MAX_ITEMS: usize = 4096;

/// What a rule allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Allow {
    /// Anything (the rule only shows its prompt).
    Any,
    Whole,
    Decimal,
    /// A value from a range, an array of items, or a string split by commas.
    List,
    Date,
    Time,
    TextLength,
    /// `f1` must give TRUE.
    Custom,
}

impl Allow {
    pub const ALL: [Allow; 8] = [Allow::Any, Allow::Whole, Allow::Decimal, Allow::List, Allow::Date, Allow::Time, Allow::TextLength, Allow::Custom];
    /// OOXML's name (`type=`).
    pub fn name(self) -> &'static str {
        ["none", "whole", "decimal", "list", "date", "time", "textLength", "custom"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<Allow> {
        Allow::ALL.into_iter().find(|a| a.name() == s)
    }
}

/// What a value the rule doesn't allow does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ErrorStyle {
    /// It isn't taken.
    #[default]
    Stop,
    /// The person is asked whether to keep it.
    Warning,
    /// It is taken, and the person told.
    Information,
}

impl ErrorStyle {
    pub const ALL: [ErrorStyle; 3] = [ErrorStyle::Stop, ErrorStyle::Warning, ErrorStyle::Information];
    pub fn name(self) -> &'static str {
        ["stop", "warning", "information"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<ErrorStyle> {
        ErrorStyle::ALL.into_iter().find(|a| a.name() == s)
    }
}

/// A title and text shown to the person.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub title: Arc<str>,
    pub text: Arc<str>,
}

impl Message {
    pub fn new(title: &str, text: &str) -> Message {
        Message { title: cut(title).into(), text: cut(text).into() }
    }
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.text.is_empty()
    }
}

fn cut(s: &str) -> &str {
    let mut n = s.len().min(MAX_MESSAGE);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// One rule over ranges of a sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Validation {
    pub sheet: u16,
    pub ranges: Vec<Rect>,
    pub allow: Allow,
    /// For numbers, dates, times and lengths (lists and formulas ignore it).
    pub op: CmpOp,
    pub f1: Option<Arc<Expr>>,
    pub f2: Option<Arc<Expr>>,
    /// An empty cell is fine.
    pub allow_blank: bool,
    /// A list shows its drop-down in the cell.
    pub dropdown: bool,
    pub style: ErrorStyle,
    /// Whether the prompt shows when the cell is chosen, and the error when a
    /// value is refused (without it, any value is taken).
    pub show_prompt: bool,
    pub show_error: bool,
    pub prompt: Message,
    pub error: Message,
}

impl Validation {
    /// A rule allowing `allow` over `ranges` (of `sheet`), with the usual
    /// settings: blanks fine, a drop-down, stop on a wrong value.
    pub fn new(sheet: u16, ranges: Vec<Rect>, allow: Allow, op: CmpOp, f1: Option<Arc<Expr>>, f2: Option<Arc<Expr>>) -> Validation {
        Validation { sheet, ranges, allow, op, f1, f2, allow_blank: true, dropdown: true, style: ErrorStyle::Stop, show_prompt: true, show_error: true, prompt: Message::default(), error: Message::default() }
    }
    /// Exact literal lists retain each text item separately in a constant array.
    pub fn literal_list_items(&self) -> Option<Vec<&str>> {
        if self.allow != Allow::List {
            return None;
        }
        let Expr::Array(rows) = self.f1.as_deref()? else { return None };
        rows.iter().flatten().map(|item| match item {
            Expr::Str(text) => Some(text.as_ref()),
            _ => None,
        }).collect()
    }

    /// The cell formulas are written for: the first range's top-left.
    pub fn origin(&self) -> (u32, u32) {
        self.ranges.first().map_or((0, 0), |r| (r.r0, r.c0))
    }
    pub fn covers(&self, a: CellAddr) -> bool {
        a.sheet == self.sheet && self.ranges.iter().any(|r| r.contains(a))
    }
    /// The rule with its formulas replaced by `f` of them.
    pub fn map_exprs(&self, f: &dyn Fn(&Expr) -> Expr) -> Validation {
        let m = |e: &Option<Arc<Expr>>| e.as_ref().map(|e| Arc::new(f(e)));
        Validation { f1: m(&self.f1), f2: m(&self.f2), ..self.clone() }
    }
}

/// Why a value isn't allowed: the rule that refuses it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub style: ErrorStyle,
    pub error: Message,
    /// What the rule allows, in words, for a refusal without a message of its own.
    pub wants: String,
}

impl Workbook {
    pub fn validations(&self) -> &[Validation] {
        &self.validations
    }

    /// Adds a rule; Err when it doesn't fit.
    pub fn add_validation(&mut self, v: Validation) -> Result<usize, &'static str> {
        if self.validations.len() >= MAX_VALIDATIONS {
            return Err("too many validation rules");
        }
        if v.ranges.is_empty() || v.ranges.len() > MAX_RANGES {
            return Err("a validation rule needs ranges");
        }
        if (v.sheet as usize) >= self.sheet_count() || v.ranges.iter().any(|r| r.sheet != v.sheet || r.r1 >= MAX_ROWS || r.c1 >= MAX_COLS || r.r0 > r.r1 || r.c0 > r.c1) {
            return Err("a validation rule's range is off its sheet");
        }
        self.validations.push(v);
        Ok(self.validations.len() - 1)
    }

    /// Swaps in a whole list (rules that don't fit are left out); the old one back.
    pub fn replace_validations(&mut self, vs: Vec<Validation>) -> Vec<Validation> {
        let old = std::mem::take(&mut self.validations);
        for v in vs {
            let _ = self.add_validation(v);
        }
        old
    }

    /// The rule over cell `a` (the last one, as Excel keeps one per cell).
    pub fn validation_at(&self, a: CellAddr) -> Option<&Validation> {
        self.validations.iter().rev().find(|v| v.covers(a))
    }

    /// A rule's formula parsed as written for its first cell.
    pub fn validation_formula(&self, src: &str, sheet: u16, origin: (u32, u32)) -> Result<Arc<Expr>, formula::ParseError> {
        self.cond_formula(src, sheet, origin)
    }

    /// The items a list rule over `a` offers, in order, without repeats.
    pub fn list_items(&self, a: CellAddr) -> Vec<Value> {
        let Some(v) = self.validation_at(a).filter(|v| v.allow == Allow::List) else { return Vec::new() };
        let Some(e) = &v.f1 else { return Vec::new() };
        let ctx = Ctx::new(self, a.sheet, a.row, a.col);
        let mut out: Vec<Value> = Vec::new();
        let mut push = |x: Value| {
            if !x.is_empty() && out.len() < MAX_ITEMS && !out.iter().any(|y| same(y, &x)) {
                out.push(x);
            }
        };
        match ctx.eval(e) {
            Ev::V(Value::Str(s)) => {
                for p in s.split(',') {
                    let p = p.trim();
                    if !p.is_empty() {
                        push(Value::Str(p.into()));
                    }
                }
            }
            Ev::V(x) => push(x),
            Ev::R(r) => {
                let (rows, cols) = ((r.r1 - r.r0 + 1) as u64, (r.c1 - r.c0 + 1) as u64);
                if rows.saturating_mul(cols) <= MAX_ITEMS as u64 * 4 {
                    for row in r.r0..=r.r1 {
                        for col in r.c0..=r.c1 {
                            push(self.value(r.sheet, row, col));
                        }
                    }
                }
            }
            Ev::A(arr) => {
                for x in arr.vals.iter().take(MAX_ITEMS * 4) {
                    push(x.clone());
                }
            }
            Ev::F(_) => {}
        }
        out
    }

    /// Whether what cell `a` holds now is allowed: Err with the rule that
    /// refuses it. Checked after an entry goes in (a custom formula may read
    /// the cell itself, as Excel's do), so a refused entry is undone.
    pub fn check_cell(&self, a: CellAddr) -> Result<(), Refusal> {
        let Some(v) = self.validation_at(a) else { return Ok(()) };
        let value = &self.value(a.sheet, a.row, a.col);
        if !v.show_error || v.allow == Allow::Any {
            return Ok(());
        }
        if value.is_empty() {
            return if v.allow_blank { Ok(()) } else { Err(self.refusal(v, a)) };
        }
        let ctx = Ctx::new(self, a.sheet, a.row, a.col);
        let num = |e: &Option<Arc<Expr>>| -> Option<f64> {
            let e = e.as_ref()?;
            match ctx.to_scalar(ctx.eval(e)) {
                Value::Num(x) => Some(x),
                Value::Bool(b) => Some(b as u8 as f64),
                Value::Str(s) => s.trim().parse().ok(),
                _ => None,
            }
        };
        let cmp = |x: f64| -> bool {
            let (Some(a), b) = (num(&v.f1), num(&v.f2)) else { return true };
            let eps = |p: f64, q: f64| (p - q).abs() <= 1e-9 * p.abs().max(q.abs()).max(1.0);
            match v.op {
                CmpOp::Between => b.is_some_and(|b| x >= a.min(b) - 1e-12 && x <= a.max(b) + 1e-12),
                CmpOp::NotBetween => b.is_some_and(|b| x < a.min(b) || x > a.max(b)),
                CmpOp::Equal => eps(x, a),
                CmpOp::NotEqual => !eps(x, a),
                CmpOp::Greater => x > a,
                CmpOp::Less => x < a,
                CmpOp::GreaterOrEqual => x >= a || eps(x, a),
                CmpOp::LessOrEqual => x <= a || eps(x, a),
            }
        };
        let ok = match v.allow {
            Allow::Any => true,
            Allow::Whole => matches!(value, Value::Num(x) if x.fract() == 0.0 && cmp(*x)),
            Allow::Decimal | Allow::Date => matches!(value, Value::Num(x) if cmp(*x)),
            Allow::Time => matches!(value, Value::Num(x) if *x >= 0.0 && cmp(*x)),
            Allow::TextLength => cmp(value.display().chars().count() as f64),
            Allow::List => {
                let items = self.list_items(a);
                items.is_empty() || items.iter().any(|y| same(y, value))
            }
            Allow::Custom => match &v.f1 {
                Some(e) => matches!(ctx.to_scalar(ctx.eval(e)), Value::Bool(true)) || matches!(ctx.to_scalar(ctx.eval(e)), Value::Num(x) if x != 0.0),
                None => true,
            },
        };
        if ok {
            Ok(())
        } else {
            Err(self.refusal(v, a))
        }
    }

    /// The cells of `area` that hold a value their rule doesn't allow (Excel's
    /// Circle Invalid Data), row by row, at most `limit`; at most `budget`
    /// cells are checked. A rule's empty cells count only when it doesn't
    /// allow blanks.
    pub fn invalid_cells(&self, area: &Rect, limit: usize, budget: u64) -> Vec<CellAddr> {
        let mut out = Vec::new();
        let mut checked = 0u64;
        let mut seen = std::collections::HashSet::new();
        for v in self.validations.iter().filter(|v| v.sheet == area.sheet && v.show_error && v.allow != Allow::Any) {
            for r in &v.ranges {
                let (r0, r1, c0, c1) = (r.r0.max(area.r0), r.r1.min(area.r1), r.c0.max(area.c0), r.c1.min(area.c1));
                if r0 > r1 || c0 > c1 {
                    continue;
                }
                for row in r0..=r1 {
                    for col in c0..=c1 {
                        checked += 1;
                        if checked > budget || out.len() >= limit {
                            return out;
                        }
                        let a = CellAddr::new(area.sheet, row, col);
                        // A cell is its first rule's.
                        if !seen.insert((row, col)) || !std::ptr::eq(self.validation_at(a).map_or(std::ptr::null(), |x| x as *const _), v) {
                            continue;
                        }
                        if self.check_cell(a).is_err() {
                            out.push(a);
                        }
                    }
                }
            }
        }
        out.sort_by_key(|a| (a.row, a.col));
        out
    }

    fn refusal(&self, v: &Validation, a: CellAddr) -> Refusal {
        let ftext = |e: &Option<Arc<Expr>>| e.as_ref().map(|e| self.cond_formula_text(e, v.origin())).unwrap_or_default();
        let (x, y) = (ftext(&v.f1), ftext(&v.f2));
        let what = match v.allow {
            Allow::Whole => "a whole number",
            Allow::Decimal => "a number",
            Allow::Date => "a date",
            Allow::Time => "a time",
            Allow::TextLength => "text whose length is",
            _ => "",
        };
        let range = match v.op {
            CmpOp::Between => format!("between {x} and {y}"),
            CmpOp::NotBetween => format!("not between {x} and {y}"),
            CmpOp::Equal => format!("equal to {x}"),
            CmpOp::NotEqual => format!("not equal to {x}"),
            CmpOp::Greater => format!("greater than {x}"),
            CmpOp::Less => format!("less than {x}"),
            CmpOp::GreaterOrEqual => format!("at least {x}"),
            CmpOp::LessOrEqual => format!("at most {x}"),
        };
        let wants = match v.allow {
            Allow::List => {
                let items: Vec<String> = self.list_items(a).iter().take(8).map(|i| i.display()).collect();
                format!("one of: {}", items.join(", "))
            }
            Allow::Custom => format!("a value for which {x} is TRUE"),
            Allow::Any => String::new(),
            _ => format!("{what} {range}"),
        };
        Refusal { style: v.style, error: v.error.clone(), wants }
    }
}

/// The same item, as a list compares them: numbers by value, text ignoring case.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x.to_lowercase() == y.to_lowercase(),
        (Value::Num(x), Value::Str(s)) | (Value::Str(s), Value::Num(x)) => s.trim().parse::<f64>().is_ok_and(|y| y == *x),
        (Value::Bool(x), Value::Bool(y)) => x == y,
        _ => false,
    }
}

impl Workbook {
    /// A list of rules as bytes (formulas as text, as the native file keeps them).
    pub fn encode_validations(&self, vs: &[Validation]) -> Vec<u8> {
        let mut w = Writer::new();
        w.var(vs.len() as u64);
        for v in vs {
            v.encode(&mut w, self);
        }
        w.into_bytes()
    }

    /// Reads a list written by [`encode_validations`](Self::encode_validations).
    pub fn decode_validations(&self, b: &[u8]) -> Result<Vec<Validation>, WireError> {
        let mut r = Reader::new(b);
        let n = r.count("validation rules", MAX_VALIDATIONS as u64, 8)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(Validation::decode(&mut r, self)?);
        }
        if !r.is_empty() {
            return Err(WireError::Invalid("trailing bytes"));
        }
        Ok(out)
    }
}

impl Validation {
    pub fn encode(&self, w: &mut Writer, wb: &Workbook) {
        let origin = self.origin();
        let ftext = |e: &Option<Arc<Expr>>| e.as_ref().map(|e| wb.cond_formula_text(e, origin)).unwrap_or_default();
        w.u16(self.sheet);
        w.var(self.ranges.len() as u64);
        for r in &self.ranges {
            for v in [r.r0, r.c0, r.r1, r.c1] {
                w.var(v as u64);
            }
        }
        w.u8(self.allow as u8);
        w.u8(self.op as u8);
        w.str(&ftext(&self.f1));
        w.str(&ftext(&self.f2));
        w.u8(self.allow_blank as u8 | (self.dropdown as u8) << 1 | (self.show_prompt as u8) << 2 | (self.show_error as u8) << 3);
        w.u8(self.style as u8);
        for m in [&self.prompt, &self.error] {
            w.str(&m.title);
            w.str(&m.text);
        }
    }

    /// Reads one rule (its sheets must exist in `wb`, for its formulas).
    pub fn decode(r: &mut Reader, wb: &Workbook) -> Result<Validation, WireError> {
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() {
            return Err(WireError::Invalid("validation sheet"));
        }
        let n = r.count("ranges", MAX_RANGES as u64, 4)?;
        if n == 0 {
            return Err(WireError::Invalid("validation range"));
        }
        let mut ranges = Vec::with_capacity(n);
        for _ in 0..n {
            let mut v = [0u32; 4];
            for (i, x) in v.iter_mut().enumerate() {
                let y = r.var()?;
                if y >= if i % 2 == 0 { MAX_ROWS } else { MAX_COLS } as u64 {
                    return Err(WireError::Invalid("validation range"));
                }
                *x = y as u32;
            }
            if v[0] > v[2] || v[1] > v[3] {
                return Err(WireError::Invalid("validation range"));
            }
            ranges.push(Rect { sheet, r0: v[0], c0: v[1], r1: v[2], c1: v[3] });
        }
        let allow = *Allow::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("validation kind"))?;
        let op = *CmpOp::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("validation operator"))?;
        let origin = (ranges[0].r0, ranges[0].c0);
        let mut fx = || -> Result<Option<Arc<Expr>>, WireError> {
            let t = r.str("formula", formula::MAX_STORED_FORMULA_LEN as u64)?;
            if t.is_empty() {
                return Ok(None);
            }
            wb.parse_stored(&format!("={t}"), origin.0, origin.1).map(|e| Some(Arc::new(e))).map_err(|_| WireError::Invalid("validation formula"))
        };
        let (f1, f2) = (fx()?, fx()?);
        let flags = r.u8()?;
        if flags > 15 {
            return Err(WireError::Invalid("validation flags"));
        }
        let style = *ErrorStyle::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("validation style"))?;
        let mut msg = || -> Result<Message, WireError> {
            let title = r.str("message", MAX_MESSAGE as u64)?.to_string();
            let text = r.str("message", MAX_MESSAGE as u64)?.to_string();
            Ok(Message::new(&title, &text))
        };
        let (prompt, error) = (msg()?, msg()?);
        Ok(Validation { sheet, ranges, allow, op, f1, f2, allow_blank: flags & 1 != 0, dropdown: flags & 2 != 0, show_prompt: flags & 4 != 0, show_error: flags & 8 != 0, style, prompt, error })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_cells_are_found_in_an_area() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let f = wb.validation_formula("10", 0, (0, 0)).unwrap();
        let mut v = Validation::new(0, vec![Rect { sheet: 0, r0: 0, c0: 0, r1: 99, c1: 0 }], Allow::Whole, CmpOp::LessOrEqual, Some(f), None);
        v.allow_blank = true;
        wb.add_validation(v).unwrap();
        for (r, x) in [(0, 5.0), (3, 12.0), (7, 2.5), (50, 99.0)] {
            wb.set_value(CellAddr::new(0, r, 0), Value::Num(x));
        }
        wb.set_value(CellAddr::new(0, 4, 1), Value::Num(500.0));
        let all = Rect { sheet: 0, r0: 0, c0: 0, r1: 200, c1: 5 };
        let at = |v: Vec<CellAddr>| v.into_iter().map(|a| (a.row, a.col)).collect::<Vec<_>>();
        assert_eq!(at(wb.invalid_cells(&all, 100, 1 << 20)), [(3, 0), (7, 0), (50, 0)]);
        assert_eq!(at(wb.invalid_cells(&Rect { r1: 10, ..all }, 100, 1 << 20)), [(3, 0), (7, 0)]);
        assert_eq!(wb.invalid_cells(&all, 1, 1 << 20).len(), 1);
    }

    use super::*;
    use crate::ops::GridOp;
    use officina_core::history::History;

    fn book() -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.add_sheet("Lists").unwrap();
        for (i, s) in ["Red", "Green", "Blue", "Green"].iter().enumerate() {
            wb.set_value(CellAddr::new(1, i as u32, 0), Value::str(s));
        }
        wb.recalc();
        wb
    }

    fn rect(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
        Rect { sheet: 0, r0, c0, r1, c1 }
    }

    #[test]
    fn rules_allow_and_refuse_values() {
        let mut wb = book();
        let f = |wb: &Workbook, s: &str, at: (u32, u32)| Some(wb.validation_formula(s, 0, at).unwrap());
        let list = Validation::new(0, vec![rect(0, 0, 9, 0)], Allow::List, CmpOp::Between, f(&wb, "Lists!$A$1:$A$4", (0, 0)), None);
        let literal = Validation::new(0, vec![rect(0, 1, 9, 1)], Allow::List, CmpOp::Between, f(&wb, "\"Yes, No ,Maybe\"", (0, 1)), None);
        let whole = Validation::new(0, vec![rect(0, 2, 9, 2)], Allow::Whole, CmpOp::Between, f(&wb, "1", (0, 2)), f(&wb, "10", (0, 2)));
        let mut length = Validation::new(0, vec![rect(0, 3, 9, 3)], Allow::TextLength, CmpOp::LessOrEqual, f(&wb, "3", (0, 3)), None);
        length.style = ErrorStyle::Warning;
        length.error = Message::new("Too long", "Three letters at most.");
        // Custom: more than the cell to its left (relative, written for E1).
        let custom = Validation::new(0, vec![rect(0, 4, 9, 4)], Allow::Custom, CmpOp::Between, f(&wb, "E1>D1", (0, 4)), None);
        for v in [list, literal, whole, length, custom] {
            wb.add_validation(v).unwrap();
        }
        let at = |r, c| CellAddr::new(0, r, c);
        let s = |t: &str| Value::str(t);
        let check = |wb: &mut Workbook, a: CellAddr, v: Value| {
            wb.set_value(a, v);
            wb.recalc();
            wb.check_cell(a)
        };
        assert_eq!(wb.list_items(at(3, 0)), vec![s("Red"), s("Green"), s("Blue")]);
        assert_eq!(wb.list_items(at(0, 1)), vec![s("Yes"), s("No"), s("Maybe")]);
        assert!(check(&mut wb, at(3, 0), s("green")).is_ok());
        let no = check(&mut wb, at(3, 0), s("Pink")).unwrap_err();
        assert_eq!(no.wants, "one of: Red, Green, Blue");
        assert!(check(&mut wb, at(2, 1), s("No")).is_ok());
        assert!(check(&mut wb, at(2, 2), Value::Num(10.0)).is_ok());
        assert!(check(&mut wb, at(2, 2), Value::Num(2.5)).is_err());
        assert_eq!(check(&mut wb, at(2, 2), Value::Num(11.0)).unwrap_err().wants, "a whole number between 1 and 10");
        let w = check(&mut wb, at(0, 3), s("Four")).unwrap_err();
        assert_eq!((w.style, &*w.error.title), (ErrorStyle::Warning, "Too long"));
        assert!(check(&mut wb, at(0, 3), Value::Empty).is_ok());
        wb.set_value(at(5, 3), Value::Num(4.0));
        assert!(check(&mut wb, at(5, 4), Value::Num(5.0)).is_ok());
        assert!(check(&mut wb, at(5, 4), Value::Num(3.0)).is_err());
        // Outside every rule: anything.
        assert!(check(&mut wb, at(20, 0), s("Pink")).is_ok());
    }

    #[test]
    fn rules_are_undone_saved_and_move_with_rows() {
        let mut wb = book();
        let e = wb.validation_formula("E3>D3", 0, (2, 4)).unwrap();
        let mut v = Validation::new(0, vec![rect(2, 4, 5, 4)], Allow::Custom, CmpOp::Between, Some(e), None);
        v.prompt = Message::new("Price", "More than the cost.");
        let op = GridOp::validations(&wb, &[v.clone()]);
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Validation", vec![op]);
        assert_eq!(wb.validations(), &[v.clone()]);
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.validations(), &[v.clone()]);
        // Two rows inserted above: the range and the formula move.
        let ins = crate::shift::shift_op(&wb, 0, crate::shift::Axis::Rows, 0, 2).unwrap();
        h.run(&mut wb, "Insert Rows", vec![ins]);
        let m = &wb.validations()[0];
        assert_eq!(m.ranges[0], rect(4, 4, 7, 4));
        assert_eq!(wb.cond_formula_text(m.f1.as_ref().unwrap(), m.origin()), "E5>D5");
        h.undo(&mut wb);
        h.undo(&mut wb);
        assert!(wb.validations().is_empty());
    }
}
