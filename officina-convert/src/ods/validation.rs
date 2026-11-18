//! Data validation in `.ods` (docs/spec/officina.md §16.83): a
//! `table:content-validations` list before the tables, each rule named, and the
//! cells it covers naming it (`table:content-validation-name`).
//!
//! A condition is OpenDocument's text: `cell-content-is-whole-number() and
//! cell-content-is-between(1,100)`, `cell-content-is-in-list("Yes";"No")`,
//! `cell-content-text-length()<=10`, `is-true-formula(F2>E2)`; its formulas are
//! relative to the base cell.

use std::collections::HashMap;
use std::fmt::Write as _;

use officina_grid::condfmt::CmpOp;
use officina_grid::formula::Expr;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::Workbook;

use super::condfmt::{cell_addr, of};
use super::formula::sheet_a1;

/// Cells past a sheet's last used row a rule's range is written for, at most
/// (a whole-column rule would otherwise name a million cells).
pub const ROWS_PAST: u32 = 10_000;
/// Cells named per workbook, at most.
pub const MAX_NAMED: usize = 500_000;

fn esc(s: &str) -> String {
    let mut o = String::new();
    super::write::esc(&mut o, s);
    o
}

/// The rule's name in the file.
pub fn name(i: usize) -> String {
    format!("val{}", i + 1)
}

fn cmp(op: CmpOp, x: &str, y: &str, what: &str) -> String {
    match op {
        CmpOp::Between => format!("{what}-is-between({x},{y})"),
        CmpOp::NotBetween => format!("{what}-is-not-between({x},{y})"),
        CmpOp::Equal => format!("{what}()={x}"),
        CmpOp::NotEqual => format!("{what}()!={x}"),
        CmpOp::Greater => format!("{what}()>{x}"),
        CmpOp::Less => format!("{what}()<{x}"),
        CmpOp::GreaterOrEqual => format!("{what}()>={x}"),
        CmpOp::LessOrEqual => format!("{what}()<={x}"),
    }
}

/// A rule's `table:condition` (without its namespace), or None for "any value".
pub fn condition(wb: &Workbook, v: &Validation) -> Option<String> {
    let o = v.origin();
    let f = |e: &Option<std::sync::Arc<Expr>>| e.as_ref().map(|e| of(wb, e, o)).unwrap_or_else(|| "0".into());
    let (x, y) = (f(&v.f1), f(&v.f2));
    Some(match v.allow {
        Allow::Any => return None,
        Allow::List => {
            let items = match v.f1.as_deref() {
                // Items in a text split by commas, as Excel keeps them: one string each.
                Some(Expr::Str(t)) => t.split(',').map(|i| format!("\"{}\"", i.replace('"', "\"\""))).collect::<Vec<_>>().join(";"),
                _ => x,
            };
            format!("cell-content-is-in-list({items})")
        }
        Allow::Custom => format!("is-true-formula({x})"),
        Allow::TextLength => cmp(v.op, &x, &y, "cell-content-text-length"),
        Allow::Whole | Allow::Decimal | Allow::Date | Allow::Time => {
            let kind = match v.allow {
                Allow::Whole => "whole-number",
                Allow::Decimal => "decimal-number",
                Allow::Date => "date",
                _ => "time",
            };
            format!("cell-content-is-{kind}() and {}", cmp(v.op, &x, &y, "cell-content"))
        }
    })
}

/// `office:spreadsheet`'s `table:content-validations`, or nothing.
pub fn content_xml(wb: &Workbook) -> String {
    let vs = wb.validations();
    if vs.is_empty() {
        return String::new();
    }
    let mut out = String::from("<table:content-validations>");
    for (i, v) in vs.iter().enumerate() {
        let o = v.origin();
        let _ = write!(out, r#"<table:content-validation table:name="{}""#, name(i));
        if let Some(c) = condition(wb, v) {
            let _ = write!(out, r#" table:condition="of:{}""#, esc(&c));
        }
        let list = match (v.allow, v.dropdown) {
            (Allow::List, true) => "unsorted",
            _ => "none",
        };
        let _ = write!(out, r#" table:allow-empty-cell="{}" table:display-list="{list}" table:base-cell-address="{}">"#, v.allow_blank, esc(&cell_addr(&sheet_a1(&wb.sheet_name(v.sheet)), o.0, o.1)));
        let msg = |out: &mut String, m: &Message| {
            for (k, line) in m.text.split('\n').enumerate() {
                let _ = k;
                let _ = write!(out, "<text:p>{}</text:p>", esc(line));
            }
        };
        let _ = write!(out, r#"<table:help-message table:title="{}" table:display="{}">"#, esc(&v.prompt.title), v.show_prompt);
        if !v.prompt.text.is_empty() {
            msg(&mut out, &v.prompt);
        }
        out.push_str("</table:help-message>");
        let _ = write!(out, r#"<table:error-message table:message-type="{}" table:title="{}" table:display="{}">"#, v.style.name(), esc(&v.error.title), v.show_error);
        if !v.error.text.is_empty() {
            msg(&mut out, &v.error);
        }
        out.push_str("</table:error-message></table:content-validation>");
    }
    out.push_str("</table:content-validations>");
    out
}

/// The cells of sheet `s` that name a rule, with the rule's index (the first
/// rule over a cell wins, as the engine applies them). `rows` is the sheet's
/// used rows; `budget` counts down the workbook's named cells.
pub fn sheet_cells(wb: &Workbook, s: u16, rows: u32, budget: &mut usize) -> HashMap<(u32, u32), usize> {
    let mut m = HashMap::new();
    for (i, v) in wb.validations().iter().enumerate().filter(|(_, v)| v.sheet == s) {
        for r in &v.ranges {
            let r1 = r.r1.min(rows.max(r.r0).saturating_add(ROWS_PAST)).min(super::write::ODS_ROWS - 1);
            for row in r.r0..=r1 {
                for col in r.c0..=r.c1.min(super::write::ODS_COLS - 1) {
                    if *budget == 0 {
                        return m;
                    }
                    if let std::collections::hash_map::Entry::Vacant(e) = m.entry((row, col)) {
                        e.insert(i);
                        *budget -= 1;
                    }
                }
            }
        }
    }
    m
}

/// A rule as the file has it.
#[derive(Default, Debug)]
pub struct RuleIn {
    pub name: String,
    pub condition: Option<String>,
    pub base: Option<String>,
    pub allow_blank: bool,
    pub dropdown: bool,
    pub prompt: (String, String, bool),
    pub error: (String, String, bool, String),
}

/// `table:content-validations`'s inner XML as rules.
#[cfg(feature = "read")]
pub fn read_rules(inner: &[u8]) -> Vec<RuleIn> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(inner);
    let mut buf = Vec::new();
    let mut out: Vec<RuleIn> = Vec::new();
    // Which message is open (0 help, 1 error), and whether a paragraph is.
    let mut msg: Option<usize> = None;
    let mut paras = 0usize;
    let mut in_p = false;
    loop {
        buf.clear();
        let Ok(ev) = r.read_event_into(&mut buf) else { break };
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let a = super::read::attrs(e);
                let get = |k: &str| a.get(k).cloned().unwrap_or_default();
                let empty = matches!(ev, Event::Empty(_));
                match e.name().as_ref() {
                    b"table:content-validation" if out.len() < officina_grid::validation::MAX_VALIDATIONS => out.push(RuleIn {
                        name: get("table:name"),
                        condition: a.get("table:condition").cloned(),
                        base: a.get("table:base-cell-address").cloned(),
                        allow_blank: a.get("table:allow-empty-cell").is_none_or(|v| v == "true"),
                        dropdown: a.get("table:display-list").is_none_or(|v| v != "none"),
                        ..Default::default()
                    }),
                    b"table:help-message" | b"table:error-message" => {
                        let Some(v) = out.last_mut() else { continue };
                        let show = a.get("table:display").is_some_and(|v| v == "true");
                        if e.name().as_ref() == b"table:help-message" {
                            v.prompt = (get("table:title"), String::new(), show);
                            msg = (!empty).then_some(0);
                        } else {
                            v.error = (get("table:title"), String::new(), show, a.get("table:message-type").cloned().unwrap_or_else(|| "stop".into()));
                            msg = (!empty).then_some(1);
                        }
                        paras = 0;
                    }
                    b"text:p" if msg.is_some() => {
                        if let Some(v) = out.last_mut() {
                            let t = if msg == Some(0) { &mut v.prompt.1 } else { &mut v.error.1 };
                            if paras > 0 {
                                t.push('\n');
                            }
                        }
                        paras += 1;
                        in_p = !empty;
                    }
                    b"text:line-break" if in_p => {
                        if let Some(v) = out.last_mut() {
                            if msg == Some(0) { &mut v.prompt.1 } else { &mut v.error.1 }.push('\n');
                        }
                    }
                    b"table:error-macro" => {
                        if let Some(v) = out.last_mut() {
                            v.error.2 = false;
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) if in_p => {
                if let (Some(v), Ok(s)) = (out.last_mut(), t.unescape()) {
                    let t = if msg == Some(0) { &mut v.prompt.1 } else { &mut v.error.1 };
                    if t.len() + s.len() <= officina_grid::validation::MAX_MESSAGE * 4 {
                        t.push_str(&s);
                    }
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"text:p" => in_p = false,
                b"table:help-message" | b"table:error-message" => msg = None,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

/// Splits at `sep` outside strings, brackets and parentheses.
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut args = Vec::new();
    let (mut depth, mut quote, mut cur) = (0i32, false, String::new());
    for c in s.chars() {
        match c {
            '"' => quote = !quote,
            '(' | '[' | '{' if !quote => depth += 1,
            ')' | ']' | '}' if !quote => depth -= 1,
            c if c == sep && !quote && depth == 0 => {
                args.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    args.push(cur);
    args
}

/// `name(args)` with `args` its whole inside, when `s` is exactly that call.
fn inside<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    s.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')
}

/// A rule's condition as what it allows, its comparison, and its formulas
/// (OpenFormula text without the namespace).
fn parse_condition(c: &str) -> Option<(Allow, CmpOp, Option<String>, Option<String>)> {
    let c = c.trim();
    let c = c.strip_prefix("of:").or_else(|| c.strip_prefix("ooo:")).or_else(|| c.strip_prefix("oooc:")).unwrap_or(c).trim();
    if let Some(x) = inside(c, "is-true-formula") {
        return Some((Allow::Custom, CmpOp::Between, Some(x.into()), None));
    }
    if let Some(x) = inside(c, "cell-content-is-in-list") {
        let items = split_top(x, ';');
        // Strings: one text of the items split by commas, as Excel keeps them.
        let texts: Option<Vec<String>> = items.iter().map(|i| super::condfmt::unquote(i)).collect();
        let f = match texts {
            Some(t) if items.len() > 1 || x.trim().starts_with('"') => format!("\"{}\"", t.join(",").replace('"', "\"\"")),
            _ if items.len() == 1 => x.to_string(),
            _ => return None,
        };
        return Some((Allow::List, CmpOp::Between, Some(f), None));
    }
    let compare = |rest: &str, what: &str| -> Option<(CmpOp, Option<String>, Option<String>)> {
        let rest = rest.trim();
        for (n, op) in [("-is-between", CmpOp::Between), ("-is-not-between", CmpOp::NotBetween)] {
            if let Some(x) = inside(rest, &format!("{what}{n}")) {
                let a = split_top(x, ',');
                return (a.len() == 2).then(|| (op, Some(a[0].clone()), Some(a[1].clone())));
            }
        }
        let tail = rest.strip_prefix(what)?.strip_prefix("()")?.trim_start();
        let ops = [("!=", CmpOp::NotEqual), ("<>", CmpOp::NotEqual), (">=", CmpOp::GreaterOrEqual), ("<=", CmpOp::LessOrEqual), ("=", CmpOp::Equal), (">", CmpOp::Greater), ("<", CmpOp::Less)];
        let (op, x) = ops.iter().find_map(|(p, op)| tail.strip_prefix(p).map(|r| (*op, r)))?;
        Some((op, Some(x.trim().into()), None))
    };
    if c.starts_with("cell-content-text-length") {
        let (op, a, b) = compare(c, "cell-content-text-length")?;
        return Some((Allow::TextLength, op, a, b));
    }
    for (kind, allow) in [("whole-number", Allow::Whole), ("decimal-number", Allow::Decimal), ("date", Allow::Date), ("time", Allow::Time)] {
        if let Some(rest) = c.strip_prefix(&format!("cell-content-is-{kind}()")) {
            let rest = rest.trim_start().strip_prefix("and")?;
            let (op, a, b) = compare(rest, "cell-content")?;
            return Some((allow, op, a, b));
        }
    }
    None
}

/// Rules read and the ranges whose cells named them, as the engine's rules.
#[cfg(feature = "read")]
pub fn make(wb: &mut Workbook, rules: Vec<RuleIn>, mut cells: HashMap<String, Vec<officina_grid::addr::Rect>>, rep: &mut crate::report::Report) {
    use crate::report::Kind;
    for rule in rules {
        let Some(mut ranges) = cells.remove(&rule.name) else { continue };
        let sheet = ranges[0].sheet;
        ranges.retain(|r| r.sheet == sheet);
        if ranges.len() > officina_grid::validation::MAX_RANGES {
            rep.note(Kind::Dropped, "data validation rules over too many separate ranges");
            continue;
        }
        let (allow, op, f1, f2) = match rule.condition.as_deref() {
            None => (Allow::Any, CmpOp::Between, None, None),
            Some(c) => match parse_condition(c) {
                Some(x) => x,
                None => {
                    rep.note_at(Kind::Dropped, "data validation rules of kinds Officina doesn't have", || c.chars().take(60).collect());
                    continue;
                }
            },
        };
        let base = rule.base.as_deref().and_then(|b| super::read::target_rect(wb, b)).filter(|b| b.0 == sheet).map_or((ranges[0].r0, ranges[0].c0), |b| (b.1, b.2));
        let origin = (ranges[0].r0, ranges[0].c0);
        // A formula relative to the base cell, as one relative to the first range.
        let fx = |src: &Option<String>| -> Result<Option<std::sync::Arc<Expr>>, ()> {
            let Some(src) = src else { return Ok(None) };
            let a1 = super::formula::from_of(&format!("of:={src}")).ok_or(())?;
            let e = wb.parse(&a1, sheet, base.0, base.1).map_err(|_| ())?;
            let text = officina_grid::formula::to_a1(&e, origin.0, origin.1, &|i| wb.sheet_name(i));
            wb.validation_formula(&text, sheet, origin).map(Some).map_err(|_| ())
        };
        let (Ok(f1), Ok(f2)) = (fx(&f1), fx(&f2)) else {
            rep.note(Kind::Dropped, "data validation rules with formulas this engine can't read");
            continue;
        };
        let mut v = Validation::new(sheet, ranges, allow, op, f1, f2);
        v.allow_blank = rule.allow_blank;
        v.dropdown = allow != Allow::List || rule.dropdown;
        v.show_prompt = rule.prompt.2;
        v.show_error = rule.error.2;
        v.style = ErrorStyle::from_name(&rule.error.3).unwrap_or_default();
        v.prompt = Message::new(&rule.prompt.0, &rule.prompt.1);
        v.error = Message::new(&rule.error.0, &rule.error.1);
        if wb.add_validation(v).is_err() {
            rep.note(Kind::Dropped, "data validation rules past the limits");
        }
    }
}
