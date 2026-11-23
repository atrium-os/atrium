//! Excel calculated value fields evaluate against each output cell's source totals.

use super::{Acc, Agg, Pivot, MAX_GROUPS};
use crate::addr::MAX_COLS;
use crate::eval::binop;
use crate::formula::{BinOp, Expr};
use crate::{Value, Workbook};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone, PartialEq)]
pub struct CalculatedField {
    pub field: u32,
    pub name: Arc<str>,
    pub formula: Arc<str>,
}

const MAX_FORMULA: usize = 1024;

/// Match Excel field names, including single-quoted names with escaped apostrophes.
/// Use the workbook expression parser for operator precedence and numeric literals.
fn compile(
    wb: &Workbook,
    pivot: &Pivot,
    input: &str,
    names: &HashMap<String, Option<u32>>,
) -> Option<(Expr, Vec<u32>)> {
    if input.len() > MAX_FORMULA {
        return None;
    }
    let mut chars = input.trim().trim_start_matches('=').chars().peekable();
    let mut formula = String::from("=");
    let mut fields = Vec::new();
    while let Some(first) = chars.next() {
        if first.is_ascii_digit() || first == '.' {
            formula.push(first);
            while let Some(c) = chars.next_if(|c| c.is_ascii_digit() || *c == '.') {
                formula.push(c);
            }
            if let Some(c) = chars.next_if(|c| matches!(c, 'E' | 'e')) {
                formula.push(c);
                if let Some(sign) = chars.next_if(|c| matches!(c, '+' | '-')) {
                    formula.push(sign);
                }
                while let Some(c) = chars.next_if(|c| c.is_ascii_digit()) {
                    formula.push(c);
                }
            }
            continue;
        }
        if first.is_ascii_whitespace() || "+-*/^()%".contains(first) {
            formula.push(first);
            continue;
        }
        let mut name = String::new();
        if first == '\'' {
            loop {
                match chars.next()? {
                    '\'' if chars.next_if_eq(&'\'').is_some() => name.push('\''),
                    '\'' => break,
                    c => name.push(c),
                }
            }
        } else {
            if !first.is_alphabetic() && first != '_' {
                return None;
            }
            name.push(first);
            while let Some(c) = chars.next_if(|c| c.is_alphanumeric() || matches!(c, '_' | '.')) {
                name.push(c);
            }
        }
        let id = names.get(&name.to_lowercase()).copied().flatten()?;
        if !fields.contains(&id) {
            fields.push(id);
            if fields.len() > MAX_GROUPS {
                return None;
            }
        }
        use std::fmt::Write;
        write!(formula, "_PivotField{id}").ok()?;
    }
    let expr = wb.parse(&formula, pivot.source.sheet, 0, 0).ok()?;
    Some((expr, fields))
}

fn eval(expr: &Expr, read: &impl Fn(u32) -> Value) -> Value {
    match expr {
        Expr::Num(n) => Value::Num(*n),
        Expr::Name(name) => name
            .strip_prefix("_PivotField")
            .and_then(|s| s.parse().ok())
            .map_or(Value::Err(crate::ErrorKind::Name), read),
        Expr::Bin(op, a, b) => binop(*op, &eval(a, read), &eval(b, read)),
        Expr::Paren(inner) | Expr::Plus(inner) => eval(inner, read),
        Expr::Neg(inner) => binop(BinOp::Mul, &Value::Num(-1.0), &eval(inner, read)),
        Expr::Pct(inner) => binop(BinOp::Div, &eval(inner, read), &Value::Num(100.0)),
        _ => Value::Err(crate::ErrorKind::Value),
    }
}

pub(super) struct Plan {
    pub source: Vec<u32>,
    calculations: Vec<(u32, Expr)>,
}

impl Plan {
    pub fn new(wb: &Workbook, pivot: &Pivot) -> Result<Self, &'static str> {
        let mut plan = Self {
            source: Vec::new(),
            calculations: Vec::new(),
        };
        if pivot.calculations.is_empty() {
            return Ok(plan);
        }

        let width = pivot.source.c1 - pivot.source.c0 + 1;
        let mut names = HashMap::new();
        for (name, id) in (0..width)
            .map(|id| (wb.pivot_field_name(pivot, id), id))
            .chain(
                pivot
                    .calculations
                    .iter()
                    .map(|field| (field.name.to_string(), field.field)),
            )
        {
            names
                .entry(name.to_lowercase())
                .and_modify(|id| *id = None)
                .or_insert(Some(id));
        }
        let mut pending = Vec::new();
        for field in &pivot.calculations {
            let (expr, fields) = compile(wb, pivot, &field.formula, &names)
                .ok_or("its calculated field uses an unsupported formula or field reference")?;
            for &id in &fields {
                if id < MAX_COLS && !plan.source.contains(&id) {
                    plan.source.push(id);
                    if plan.source.len() > MAX_GROUPS {
                        return Err("its calculated fields depend on too many source columns");
                    }
                }
            }
            pending.push((field.field, expr, fields));
        }
        while !pending.is_empty() {
            let index = pending
                .iter()
                .position(|(_, _, deps)| {
                    deps.iter().all(|id| {
                        *id < MAX_COLS || plan.calculations.iter().any(|(field, _)| field == id)
                    })
                })
                .ok_or("its calculated fields form a dependency cycle")?;
            let (id, expr, _) = pending.remove(index);
            plan.calculations.push((id, expr));
        }
        Ok(plan)
    }

    pub fn apply(&self, pivot: &Pivot, totals: &mut Vec<Acc>) {
        let n = totals.len() - self.source.len();
        let mut results = Vec::with_capacity(self.calculations.len());
        for (id, expr) in &self.calculations {
            let value = eval(expr, &|field| {
                if let Some(index) = self.source.iter().position(|col| *col == field) {
                    return totals[n + index].result(Agg::Sum);
                }
                results.iter().find(|(id, _)| *id == field).map_or(
                    Value::Err(crate::ErrorKind::Name),
                    |(_, value): &(u32, Value)| value.clone(),
                )
            });
            results.push((*id, value));
        }
        for (index, field) in pivot.data.iter().enumerate() {
            if let Some((_, value)) = results.iter().find(|(id, _)| *id == field.field) {
                totals[index].add(value);
            }
        }
        totals.truncate(n);
    }
}
