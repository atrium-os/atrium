//! Sheet-level review of ordered rules. Priority and overlapping ranges make
//! the order part of the behavior of both collections.
use std::cell::Cell;
use std::collections::BTreeMap;

use crate::addr::Rect;
use crate::condfmt::{CondFormat, Rule, MAX_FORMATS};
use crate::formula::Expr;
use crate::ops::GridOp;
use crate::validation::{Validation, MAX_VALIDATIONS};
use crate::Workbook;

use super::Change;

#[derive(Debug, Clone, PartialEq)]
pub struct RuleChange<T> {
    pub(super) sheet: u16,
    source_sheet: u16,
    before: Vec<T>,
    after: Vec<T>,
}

impl<T: std::fmt::Debug> RuleChange<T> {
    pub(super) fn details(&self) -> (String, String) {
        let show = |rules: &[T]| if rules.is_empty() { "(none)".into() } else { super::review_value(&rules) };
        (show(&self.before), show(&self.after))
    }
}

trait SheetRule: Clone + PartialEq {
    fn sheet(&self) -> u16;
    fn ranges(&self) -> &[Rect];
    fn remap(&self, sheet: u16, expr: &dyn Fn(&Expr) -> Expr) -> Self;
}

impl SheetRule for CondFormat {
    fn sheet(&self) -> u16 { self.sheet }
    fn ranges(&self) -> &[Rect] { &self.ranges }
    fn remap(&self, sheet: u16, expr: &dyn Fn(&Expr) -> Expr) -> Self {
        Self {
            sheet,
            ranges: self.ranges.iter().map(|r| Rect { sheet, ..*r }).collect(),
            rules: self.rules.iter().map(|rule| Rule {
                kind: rule.kind.map_exprs(expr),
                dxf: rule.dxf.clone(),
                stop: rule.stop,
            }).collect(),
        }
    }
}

impl SheetRule for Validation {
    fn sheet(&self) -> u16 { self.sheet }
    fn ranges(&self) -> &[Rect] { &self.ranges }
    fn remap(&self, sheet: u16, expr: &dyn Fn(&Expr) -> Expr) -> Self {
        let mut rule = self.map_exprs(expr);
        rule.sheet = sheet;
        for range in &mut rule.ranges {
            range.sheet = sheet;
        }
        rule
    }
}

#[allow(private_bounds)]
impl<T: SheetRule> RuleChange<T> {
    pub(super) fn same_incoming(&self, other: &Self, source: &Workbook, other_source: &Workbook) -> bool {
        let map = sheet_map(source, other_source);
        self.after.len() == other.after.len() && self.after.iter().zip(&other.after)
            .all(|(left, right)| remap(left, other.source_sheet, &map).as_ref() == Some(right))
    }

    pub(super) fn reorder(&mut self, old: &Workbook, new: &Workbook) -> Option<()> {
        self.sheet = new.sheet_id(&old.sheet_name(self.sheet))?;
        let map = sheet_map(old, new);
        for rule in &mut self.before { *rule = remap(rule, self.sheet, &map)?; }
        Some(())
    }

    pub(super) fn anchor(&self) -> Option<(u16, u32, u32)> {
        let range = self.before.first().or(self.after.first())?.ranges().first()?;
        Some((self.sheet, range.r0, range.c0))
    }

    pub(super) fn describe(&self, label: &str) -> String {
        let action = match (self.before.is_empty(), self.after.is_empty()) {
            (true, _) => "added",
            (_, true) => "removed",
            _ => "changed",
        };
        format!("{label} {action}")
    }
}

fn group<T: SheetRule>(rules: &[T]) -> BTreeMap<u16, Vec<&T>> {
    let mut grouped = BTreeMap::<_, Vec<_>>::new();
    for rule in rules {
        grouped.entry(rule.sheet()).or_default().push(rule);
    }
    grouped
}

fn sheet_map(src: &Workbook, dst: &Workbook) -> Vec<Option<u16>> {
    (0..src.sheet_count() as u16).map(|s| dst.sheet_id(&src.sheet_name(s))).collect()
}

fn remap<T: SheetRule>(rule: &T, sheet: u16, sheets: &[Option<u16>]) -> Option<T> {
    let missing = Cell::new(false);
    let update = |expr: &Expr| crate::book::remap_sheets(expr, &|s| {
        sheets.get(s as usize).copied().flatten().unwrap_or_else(|| {
            missing.set(true);
            0
        })
    });
    let rule = rule.remap(sheet, &update);
    (!missing.get()).then_some(rule)
}

fn differences<T: SheetRule>(
    local: &[T], remote: &[T], sheets: &[Option<u16>], wrap: fn(RuleChange<T>) -> Change,
) -> Vec<Change> {
    let mut local = group(local);
    let mut remote = group(remote);
    let mut changes = Vec::new();
    for (source_sheet, sheet) in sheets.iter().enumerate() {
        let Some(sheet) = *sheet else { continue };
        let source_sheet = source_sheet as u16;
        let before = local.remove(&sheet).unwrap_or_default();
        let after = remote.remove(&source_sheet).unwrap_or_default();
        let same = before.len() == after.len() && before.iter().zip(&after)
            .all(|(left, right)| remap(*right, sheet, sheets).as_ref() == Some(*left));
        if !same {
            changes.push(wrap(RuleChange {
                sheet,
                source_sheet,
                before: before.into_iter().cloned().collect(),
                after: after.into_iter().cloned().collect(),
            }));
        }
    }
    changes
}

pub(super) fn compare(ours: &Workbook, theirs: &Workbook) -> impl Iterator<Item = Change> {
    let map = sheet_map(theirs, ours);
    differences(ours.cond_formats(), theirs.cond_formats(), &map, Change::ConditionalFormats)
        .into_iter().chain(differences(ours.validations(), theirs.validations(), &map, Change::Validations))
}

fn replacements<'a, T: SheetRule + 'a>(
    current: &[T], changes: impl Iterator<Item = &'a RuleChange<T>>,
    map: &[Option<u16>], limit: usize,
) -> Option<(Vec<T>, usize)> {
    let mut changes = changes.peekable();
    changes.peek()?;
    let mut grouped = group(current);
    let mut updated = BTreeMap::new();
    let mut len = current.len();
    for change in changes {
        // Guard against stale review items before replacing a sheet's rules.
        let before = grouped.remove(&change.sheet).unwrap_or_default();
        if !before.into_iter().eq(&change.before)
            || map.get(change.source_sheet as usize) != Some(&Some(change.sheet)) {
            continue;
        }
        let Some(after) = change.after.iter().map(|r| remap(r, change.sheet, map))
            .collect::<Option<Vec<_>>>() else { continue };
        len = len - change.before.len() + after.len();
        updated.insert(change.sheet, after);
    }
    let count = updated.len();
    if count == 0 || len > limit { return None; }
    let mut rules = Vec::with_capacity(len);
    // Keep the original order outside the sheets being updated.
    for rule in current {
        if let Some(list) = updated.get_mut(&rule.sheet()) {
            rules.append(list);
        } else {
            rules.push(rule.clone());
        }
    }
    rules.extend(updated.into_values().flatten());
    Some((rules, count))
}

pub(super) fn take(ours: &Workbook, theirs: &Workbook, changes: &[Change], ops: &mut Vec<GridOp>) -> usize {
    if !changes.iter().any(|change| matches!(change, Change::ConditionalFormats(_) | Change::Validations(_))) {
        return 0;
    }
    let map = sheet_map(theirs, ours);
    let formats = changes.iter().filter_map(|change| match change {
        Change::ConditionalFormats(rules) => Some(rules),
        _ => None,
    });
    let validations = changes.iter().filter_map(|change| match change {
        Change::Validations(rules) => Some(rules),
        _ => None,
    });
    let mut count = 0;
    if let Some((rules, taken)) = replacements(ours.cond_formats(), formats, &map, MAX_FORMATS) {
        ops.push(GridOp::cond_formats(ours, &rules));
        count += taken;
    }
    if let Some((rules, taken)) = replacements(ours.validations(), validations, &map, MAX_VALIDATIONS) {
        ops.push(GridOp::validations(ours, &rules));
        count += taken;
    }
    count
}
