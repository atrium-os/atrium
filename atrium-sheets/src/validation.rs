//! The data validation panel's model (docs/spec/officina.md §16.84): the rule
//! over the cursor's cell, or a new one for the selection, as the panel shows it,
//! and the changes it makes, each one undo step ([`GridOp::validations`]).
//!
//! The panel is drawn by [`crate::app`]; everything here is plain data and
//! testable without a window.

use officina_grid::condfmt::CmpOp;
use officina_grid::formula::Expr;
use officina_grid::ops::GridOp;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation, MAX_VALIDATIONS};
use officina_grid::{CellAddr, Rect, Workbook};

use crate::condfmt::{parse_ranges, ranges_text};
use crate::doc::Doc;

/// What a rule allows, as the Allow menu names them, in [`Allow::ALL`] order.
pub const ALLOWS: [&str; 8] = ["Any value", "Whole number", "Decimal", "List", "Date", "Time", "Text length", "Custom formula"];
/// The comparisons, in [`CmpOp::ALL`] order.
pub const OPS: [&str; 8] = ["between", "not between", "equal to", "not equal to", "greater than", "less than", "greater than or equal to", "less than or equal to"];
/// The error styles, in [`ErrorStyle::ALL`] order.
pub const STYLES: [&str; 3] = ["Stop: refuse the value", "Warning: ask to keep it", "Information: take it and say so"];

/// The panel's text fields, in order.
pub const RANGE: usize = 0;
pub const A: usize = 1;
pub const B: usize = 2;
pub const PROMPT_TITLE: usize = 3;
pub const PROMPT_TEXT: usize = 4;
pub const ERROR_TITLE: usize = 5;
pub const ERROR_TEXT: usize = 6;
pub const FIELDS: usize = 7;

/// The panel: one rule's settings. Text fields live in the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Panel {
    pub open: bool,
    /// The rule being changed (its index), or a new one.
    pub editing: Option<usize>,
    /// Into [`ALLOWS`], [`OPS`], [`STYLES`].
    pub allow: usize,
    pub op: usize,
    pub style: usize,
    pub blank: bool,
    pub dropdown: bool,
    pub show_prompt: bool,
    pub show_error: bool,
    /// Why Save didn't save.
    pub error: String,
}

/// Whether the allow kind compares (and so shows the comparison menu).
pub fn compares(allow: usize) -> bool {
    matches!(Allow::ALL.get(allow), Some(Allow::Whole | Allow::Decimal | Allow::Date | Allow::Time | Allow::TextLength))
}

/// The first field's placeholder for an allow kind, or None when it has none.
pub fn field_a(allow: usize) -> Option<&'static str> {
    Some(match Allow::ALL.get(allow)? {
        Allow::Any => return None,
        Allow::List => "Items split by commas, or =$A$1:$A$9",
        Allow::Custom => "Formula, e.g. =B2>A2",
        Allow::Date => "Date, e.g. 2026-01-31",
        Allow::Time => "Time, e.g. 09:30",
        Allow::TextLength => "Length",
        _ => "Value or =formula",
    })
}

/// Whether the comparison needs a second value.
pub fn two(op: usize) -> bool {
    op < 2
}

/// A rule's formula as its field shows it: a list's items, a number, or
/// `=formula`.
fn field_text(wb: &Workbook, v: &Validation, e: &Option<std::sync::Arc<Expr>>) -> String {
    let Some(e) = e else { return String::new() };
    match (&**e, v.allow) {
        (Expr::Str(t), Allow::List) => t.split(',').map(str::trim).collect::<Vec<_>>().join(", "),
        (Expr::Num(x), _) => officina_grid::value::format_general(*x),
        _ => format!("={}", wb.cond_formula_text(e, v.origin())),
    }
}

/// The panel and its fields for the rule over `cursor`, or a new rule for `sel`.
pub fn open(wb: &Workbook, sel: Rect, cursor: (u32, u32)) -> (Panel, [String; FIELDS]) {
    let at = CellAddr::new(sel.sheet, cursor.0, cursor.1);
    match wb.validations().iter().position(|v| v.covers(at)) {
        Some(i) => {
            let v = &wb.validations()[i];
            let p = Panel {
                open: true,
                editing: Some(i),
                allow: v.allow as usize,
                op: v.op as usize,
                style: v.style as usize,
                blank: v.allow_blank,
                dropdown: v.dropdown,
                show_prompt: v.show_prompt,
                show_error: v.show_error,
                error: String::new(),
            };
            let f = [ranges_text(&v.ranges), field_text(wb, v, &v.f1), field_text(wb, v, &v.f2), v.prompt.title.to_string(), v.prompt.text.to_string(), v.error.title.to_string(), v.error.text.to_string()];
            (p, f)
        }
        None => {
            let p = Panel { open: true, editing: None, allow: Allow::List as usize, op: 0, style: 0, blank: true, dropdown: true, show_prompt: true, show_error: true, error: String::new() };
            let mut f: [String; FIELDS] = Default::default();
            f[RANGE] = ranges_text(&[sel]);
            (p, f)
        }
    }
}

/// `2026-01-31` as `DATE(2026,1,31)`.
fn date_formula(t: &str) -> Option<String> {
    let mut it = t.split(['-', '/']).map(|p| p.trim().parse::<u32>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    (it.next().is_none() && (1900..=9999).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)).then(|| format!("DATE({y},{m},{d})"))
}

/// `9:30` or `9:30:15` as `TIME(9,30,15)`.
fn time_formula(t: &str) -> Option<String> {
    let mut it = t.split(':').map(|p| p.trim().parse::<u32>().ok());
    let (h, m) = (it.next()??, it.next()??);
    let s = match it.next() {
        Some(s) => s?,
        None => 0,
    };
    (it.next().is_none() && h < 24 && m < 60 && s < 60).then(|| format!("TIME({h},{m},{s})"))
}

/// A field's value as a formula for `allow`, or why not.
fn value_formula(allow: Allow, t: &str, what: &str) -> Result<String, String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(format!("Type the {what}."));
    }
    if let Some(f) = t.strip_prefix('=') {
        return Ok(f.to_string());
    }
    let num = officina_grid::eval::parse_number(t).map(|x| officina_grid::value::format_general(x));
    let f = match allow {
        Allow::Date => date_formula(t).or(num),
        Allow::Time => time_formula(t).or(num),
        _ => num,
    };
    f.ok_or_else(|| match allow {
        Allow::Date => format!("The {what} isn't a date (2026-01-31) or =formula."),
        Allow::Time => format!("The {what} isn't a time (09:30) or =formula."),
        _ => format!("The {what} isn't a number or =formula."),
    })
}

/// The rule the panel's settings and fields make.
pub fn build(wb: &Workbook, sheet: u16, p: &Panel, f: &[String; FIELDS]) -> Result<Validation, String> {
    let ranges = parse_ranges(&f[RANGE], sheet)?;
    if ranges.len() > officina_grid::validation::MAX_RANGES {
        return Err(format!("At most {} ranges.", officina_grid::validation::MAX_RANGES));
    }
    let origin = (ranges[0].r0, ranges[0].c0);
    let allow = *Allow::ALL.get(p.allow).ok_or("Pick what the cells allow.")?;
    let op = CmpOp::ALL[p.op.min(7)];
    let parse = |src: &str, what: &str| wb.validation_formula(src, sheet, origin).map(Some).map_err(|_| format!("The {what} isn't a formula Sheets can read."));
    let (f1, f2) = match allow {
        Allow::Any => (None, None),
        Allow::List => {
            let t = f[A].trim();
            if let Some(src) = t.strip_prefix('=') {
                (parse(src, "list")?, None)
            } else {
                let items: Vec<&str> = t.split(',').map(str::trim).filter(|i| !i.is_empty()).collect();
                if items.is_empty() {
                    return Err("Type the list's items, split by commas, or =range.".into());
                }
                let text = items.join(",");
                (parse(&format!("\"{}\"", text.replace('"', "\"\"")), "list")?, None)
            }
        }
        Allow::Custom => {
            let t = f[A].trim();
            let src = t.strip_prefix('=').unwrap_or(t);
            if src.trim().is_empty() {
                return Err("Type a formula that is TRUE for allowed values.".into());
            }
            (parse(src, "formula")?, None)
        }
        _ => {
            let x = parse(&value_formula(allow, &f[A], if two(p.op) { "lowest value" } else { "value" })?, "value")?;
            let y = if two(p.op) { parse(&value_formula(allow, &f[B], "highest value")?, "highest value")? } else { None };
            (x, y)
        }
    };
    let mut v = Validation::new(sheet, ranges, allow, if compares(p.allow) { op } else { CmpOp::Between }, f1, f2);
    v.allow_blank = p.blank;
    v.dropdown = p.dropdown;
    v.style = ErrorStyle::ALL[p.style.min(2)];
    v.show_prompt = p.show_prompt;
    v.show_error = p.show_error;
    v.prompt = Message::new(f[PROMPT_TITLE].trim(), f[PROMPT_TEXT].trim());
    v.error = Message::new(f[ERROR_TITLE].trim(), f[ERROR_TEXT].trim());
    Ok(v)
}

/// The cells of `r` not in `cut`, as up to four rectangles.
fn minus(r: &Rect, cut: &Rect) -> Vec<Rect> {
    if r.sheet != cut.sheet || cut.r0 > r.r1 || cut.r1 < r.r0 || cut.c0 > r.c1 || cut.c1 < r.c0 {
        return vec![*r];
    }
    let mut out = Vec::new();
    if cut.r0 > r.r0 {
        out.push(Rect { r1: cut.r0 - 1, ..*r });
    }
    if cut.r1 < r.r1 {
        out.push(Rect { r0: cut.r1 + 1, ..*r });
    }
    let (r0, r1) = (r.r0.max(cut.r0), r.r1.min(cut.r1));
    if cut.c0 > r.c0 {
        out.push(Rect { r0, r1, c1: cut.c0 - 1, ..*r });
    }
    if cut.c1 < r.c1 {
        out.push(Rect { r0, r1, c0: cut.c1 + 1, ..*r });
    }
    out
}

/// The rules with `cut`'s cells taken out of every one but `keep` (rules left
/// with no cells go).
fn without(vs: Vec<Validation>, cut: &[Rect], keep: Option<usize>) -> Vec<Validation> {
    vs.into_iter()
        .enumerate()
        .filter_map(|(i, mut v)| {
            if Some(i) != keep {
                for c in cut {
                    v.ranges = v.ranges.iter().flat_map(|r| minus(r, c)).collect();
                }
                v.ranges.truncate(officina_grid::validation::MAX_RANGES);
            }
            (!v.ranges.is_empty()).then_some(v)
        })
        .collect()
}

/// Saves the panel's rule: its cells leave any other rule (a cell has one rule,
/// as in Excel); a changed rule keeps its place, a new one goes first.
pub fn save(d: &mut Doc, p: &Panel, f: &[String; FIELDS]) -> Result<(), String> {
    let v = build(&d.wb, d.sheet, p, f)?;
    let mut vs = d.wb.validations().to_vec();
    let label = match p.editing {
        Some(i) if i < vs.len() => {
            vs[i] = v.clone();
            "Change Validation"
        }
        Some(_) => return Err("That rule is gone.".into()),
        None => {
            if vs.len() >= MAX_VALIDATIONS {
                return Err("This workbook has too many validation rules.".into());
            }
            vs.insert(0, v.clone());
            "New Validation"
        }
    };
    let keep = Some(p.editing.unwrap_or(0));
    let vs = without(vs, &v.ranges, keep);
    let op = GridOp::validations(&d.wb, &vs);
    d.run(label, vec![op]);
    Ok(())
}

/// Takes rules off the cells of `sel` (other cells keep theirs).
pub fn clear(d: &mut Doc, sel: Rect) -> bool {
    let vs = d.wb.validations().to_vec();
    let n = vs.iter().filter(|v| v.ranges.iter().any(|r| !minus(r, &sel).eq(&[*r]))).count();
    if n == 0 {
        return false;
    }
    let vs = without(vs, &[sel], None);
    let op = GridOp::validations(&d.wb, &vs);
    d.run("Clear Validation", vec![op]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::Value;

    fn sel(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
        Rect { sheet: 0, r0, c0, r1, c1 }
    }

    #[test]
    fn rules_are_made_changed_and_cleared() {
        let mut d = Doc::new("v");
        // A new list rule over A1:A10.
        let (mut p, mut f) = open(&d.wb, sel(0, 0, 9, 0), (0, 0));
        assert_eq!((p.editing, f[RANGE].as_str()), (None, "A1:A10"));
        f[A] = "Red,  Green ,Blue".into();
        f[PROMPT_TITLE] = "Colour".into();
        save(&mut d, &p, &f).unwrap();
        assert_eq!(d.wb.list_items(CellAddr::new(0, 4, 0)), vec![Value::str("Red"), Value::str("Green"), Value::str("Blue")]);
        // It opens again as it was saved.
        let (q, g) = open(&d.wb, sel(3, 0, 3, 0), (3, 0));
        assert_eq!((q.editing, g[A].as_str(), g[PROMPT_TITLE].as_str()), (Some(0), "Red, Green, Blue", "Colour"));
        // A date rule over A5:B6 takes those cells from the list rule.
        (p, f) = open(&d.wb, sel(4, 0, 5, 1), (4, 1));
        p.allow = Allow::Date as usize;
        p.op = 4;
        f[A] = "2026-01-31".into();
        f[RANGE] = "A5:B6".into();
        p.editing = None;
        save(&mut d, &p, &f).unwrap();
        assert_eq!(d.wb.validations().len(), 2);
        assert_eq!(d.wb.validation_at(CellAddr::new(0, 4, 0)).unwrap().allow, Allow::Date);
        assert_eq!(d.wb.validation_at(CellAddr::new(0, 3, 0)).unwrap().allow, Allow::List);
        assert_eq!(d.wb.validation_at(CellAddr::new(0, 7, 0)).unwrap().allow, Allow::List);
        let (_, g) = open(&d.wb, sel(4, 0, 4, 0), (4, 0));
        assert_eq!(g[A], "=DATE(2026,1,31)");
        assert!(!d.type_into(4, 0, "5"));
        assert!(d.type_into(4, 0, "=DATE(2026,2,1)"));
        // Bad values are refused with why.
        f[A] = "soon".into();
        assert_eq!(build(&d.wb, 0, &p, &f).unwrap_err(), "The value isn't a date (2026-01-31) or =formula.");
        p.allow = Allow::Whole as usize;
        p.op = 0;
        f[A] = "1".into();
        f[B] = String::new();
        assert_eq!(build(&d.wb, 0, &p, &f).unwrap_err(), "Type the highest value.");
        // Clearing the column takes both rules off it; undo brings them back.
        assert!(clear(&mut d, sel(0, 0, 1_048_575, 0)));
        assert!(d.wb.validation_at(CellAddr::new(0, 3, 0)).is_none());
        assert!(d.wb.validation_at(CellAddr::new(0, 4, 1)).is_some());
        d.undo();
        assert_eq!(d.wb.validation_at(CellAddr::new(0, 3, 0)).unwrap().allow, Allow::List);
    }
}
