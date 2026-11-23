//! Compare named tables and pivot definitions. Generated pivot output belongs
//! to the pivot and is written by the same edit as a manual pivot refresh.
use std::collections::BTreeMap;

use crate::addr::{CellAddr, Rect};
use crate::ops::GridOp;
use crate::pivot::Pivot;
use crate::tables::{Table, MAX_TABLES};
use crate::Workbook;

use super::Change;

#[derive(Debug, Clone, PartialEq)]
pub struct TableChange {
    at: Option<CellAddr>,
    before: Option<Box<Table>>,
    after: Option<Box<Table>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PivotChange {
    at: Option<CellAddr>,
    before: Option<Box<Pivot>>,
    after: Option<Box<Pivot>>,
}

impl TableChange {
    pub(super) fn is_addition(&self) -> bool { self.before.is_none() }

    pub(super) fn on_new_sheet(&self, local: &Workbook, incoming: &Workbook) -> bool {
        self.after.as_ref().is_some_and(|obj| local.sheet_id(&incoming.sheet_name(obj.rect.sheet)).is_none())
    }

    pub(super) fn key(&self) -> String {
        self.before.as_ref().or(self.after.as_ref()).unwrap().name.to_uppercase()
    }

    pub(super) fn same_incoming(&self, other: &Self, source: &Workbook, other_source: &Workbook) -> bool {
        match (&self.after, &other.after) {
            (Some(a), Some(b)) => map_table(a, source, other_source).as_ref() == Some(b.as_ref()),
            (None, None) => true,
            _ => false,
        }
    }

    pub(super) fn details(&self) -> (String, String) {
        (self.before.as_deref().map_or_else(|| "(none)".into(), super::review_value),
         self.after.as_deref().map_or_else(|| "(none)".into(), super::review_value))
    }

    pub(super) fn reorder(&mut self, old: &Workbook, new: &Workbook) -> Option<()> {
        if let Some(a) = &mut self.at { a.sheet = new.sheet_id(&old.sheet_name(a.sheet))?; }
        if let Some(obj) = &mut self.before { **obj = map_table(obj, old, new)?; }
        Some(())
    }

    pub(super) fn at(&self) -> Option<CellAddr> { self.at }

    pub(super) fn describe(&self) -> String {
        let obj = self.after.as_ref().or(self.before.as_ref()).unwrap();
        format!("Table {} {}", obj.name, super::parts::action(self.before.is_some(), self.after.is_some()))
    }
}

impl PivotChange {
    pub(super) fn is_addition(&self) -> bool { self.before.is_none() }

    pub(super) fn on_new_sheet(&self, local: &Workbook, incoming: &Workbook) -> bool {
        self.after.as_ref().is_some_and(|obj| local.sheet_id(&incoming.sheet_name(obj.dest.sheet)).is_none())
    }

    pub(super) fn key(&self) -> String {
        self.before.as_ref().or(self.after.as_ref()).unwrap().name.to_uppercase()
    }

    pub(super) fn same_incoming(&self, other: &Self, source: &Workbook, other_source: &Workbook) -> bool {
        match (&self.after, &other.after) {
            (Some(a), Some(b)) => map_pivot(a, source, other_source).as_ref() == Some(b.as_ref()),
            (None, None) => true,
            _ => false,
        }
    }

    pub(super) fn details(&self) -> (String, String) {
        (self.before.as_deref().map_or_else(|| "(none)".into(), super::review_value),
         self.after.as_deref().map_or_else(|| "(none)".into(), super::review_value))
    }

    pub(super) fn reorder(&mut self, old: &Workbook, new: &Workbook) -> Option<()> {
        if let Some(a) = &mut self.at { a.sheet = new.sheet_id(&old.sheet_name(a.sheet))?; }
        if let Some(obj) = &mut self.before { **obj = map_pivot(obj, old, new)?; }
        Some(())
    }

    pub(super) fn at(&self) -> Option<CellAddr> { self.at }

    pub(super) fn describe(&self) -> String {
        let obj = self.after.as_ref().or(self.before.as_ref()).unwrap();
        format!("Pivot table {} {}", obj.name, super::parts::action(self.before.is_some(), self.after.is_some()))
    }
}

fn map_sheet(src: &Workbook, dst: &Workbook, sheet: u16) -> Option<u16> {
    dst.sheet_id(&src.sheet_name(sheet))
}

fn map_table(table: &Table, src: &Workbook, dst: &Workbook) -> Option<Table> {
    let mut table = table.clone();
    table.rect.sheet = map_sheet(src, dst, table.rect.sheet)?;
    Some(table)
}

fn map_pivot(pivot: &Pivot, src: &Workbook, dst: &Workbook) -> Option<Pivot> {
    let mut pivot = pivot.clone();
    pivot.source.sheet = map_sheet(src, dst, pivot.source.sheet)?;
    pivot.dest.sheet = map_sheet(src, dst, pivot.dest.sheet)?;
    Some(pivot)
}

fn named_pairs<'a, T>(
    ours: &'a [T], theirs: &'a [T], name: impl Fn(&T) -> &str,
) -> impl Iterator<Item = (Option<&'a T>, Option<&'a T>)> {
    type Groups<'a, T> = BTreeMap<String, (Vec<&'a T>, Vec<&'a T>)>;
    let mut groups = Groups::new();
    for item in ours {
        groups.entry(name(item).to_uppercase()).or_default().0.push(item);
    }
    for item in theirs {
        groups.entry(name(item).to_uppercase()).or_default().1.push(item);
    }
    groups.into_values().flat_map(|(before, after)| {
        (0..before.len().max(after.len())).map(move |i| (before.get(i).copied(), after.get(i).copied()))
    })
}

pub(super) fn compare(ours: &Workbook, theirs: &Workbook) -> impl Iterator<Item = Change> {
    let tables: Vec<_> = named_pairs(ours.tables(), theirs.tables(), |t| &t.name)
        .filter_map(|(before, after)| {
            let mapped = after.and_then(|t| map_table(t, theirs, ours));
            if before == mapped.as_ref() && after.is_none() == mapped.is_none() { return None; }
            let at = before.or(mapped.as_ref()).map(|t| CellAddr::new(t.rect.sheet, t.rect.r0, t.rect.c0));
            Some(Change::Table(TableChange { at, before: before.cloned().map(Box::new), after: after.cloned().map(Box::new) }))
        }).collect();
    let pivots: Vec<_> = named_pairs(ours.pivots(), theirs.pivots(), |p| &p.name)
        .filter_map(|(before, after)| {
            let mapped = after.and_then(|p| map_pivot(p, theirs, ours));
            if before == mapped.as_ref() && after.is_none() == mapped.is_none() { return None; }
            let at = before.or(mapped.as_ref()).map(|p| p.dest);
            Some(Change::Pivot(PivotChange { at, before: before.cloned().map(Box::new), after: after.cloned().map(Box::new) }))
        }).collect();
    tables.into_iter().chain(pivots)
}

/// Output rectangles on a sheet, including the page fields.
pub(super) fn pivot_areas(wb: &Workbook, sheet: u16) -> Vec<Rect> {
    wb.pivots().iter().enumerate().filter(|(_, p)| p.dest.sheet == sheet)
        .filter_map(|(i, _)| wb.pivot_shape(i)?.area).collect()
}

pub(super) fn take(ours: &Workbook, theirs: &Workbook, changes: &[Change], ops: &mut Vec<GridOp>) -> usize {
    let mut tables = None;
    let mut pivots = None;
    let mut pivot_ops = Vec::new();
    let mut count = 0;
    let names = ops.iter().find_map(|op| if let GridOp::Names(bytes) = op {
        ours.decode_names(bytes).ok()
    } else { None });
    let removal = |change: &Change| match change {
        Change::Table(change) => change.after.is_none(),
        Change::Pivot(change) => change.after.is_none(),
        _ => false,
    };
    let ordered = changes.iter().filter(|change| removal(change))
        .chain(changes.iter().filter(|change| !removal(change)));
    for change in ordered {
        match change {
            Change::Table(change) => {
                let tables = tables.get_or_insert_with(|| ours.tables().to_vec());
                let at = match &change.before {
                    Some(before) => match tables.iter().position(|table| table == before.as_ref()) {
                        Some(at) => Some(at),
                        None => continue,
                    },
                    None => None,
                };
                if let Some(after) = &change.after {
                    let Some(after) = map_table(after, theirs, ours) else { continue };
                    let name_taken = match &names {
                        Some(names) => names.iter().any(|name| name.name.eq_ignore_ascii_case(&after.name)),
                        None => ours.names().iter().any(|name| name.name.eq_ignore_ascii_case(&after.name)),
                    };
                    if name_taken || tables.iter().enumerate().any(|(i, table)| Some(i) != at &&
                        (table.name.to_uppercase() == after.name.to_uppercase() || table.rect.intersects(&after.rect))) {
                        continue;
                    }
                    match at {
                        Some(i) => tables[i] = after,
                        None if tables.len() < MAX_TABLES => tables.push(after),
                        None => continue,
                    }
                } else if let Some(i) = at {
                    tables.remove(i);
                }
            }
            Change::Pivot(change) => {
                let state = pivots.get_or_insert_with(|| ours.pivot_state());
                let at = match &change.before {
                    Some(before) => match state.iter().position(|(pivot, _)| pivot == before.as_ref()) {
                        Some(at) => Some(at),
                        None => continue,
                    },
                    None => None,
                };
                let after = match &change.after {
                    Some(after) => match map_pivot(after, theirs, ours) {
                        Some(after) => Some(after),
                        None => continue,
                    },
                    None => None,
                };
                let mut candidate = state.clone();
                let Ok(out) = ours.plan_pivot(&mut candidate, at, after.clone()) else { continue };
                let old = at.and_then(|i| state[i].1);
                if old.is_some_and(|old| state.iter().enumerate().any(|(j, (_, area))| Some(j) != at &&
                    area.is_some_and(|area| area.intersects(&old)))) {
                    continue;
                }
                // Output writes must not change another pivot's inputs before
                // the collection is installed, or cover their own new source.
                if out.iter().any(|op| match op {
                    GridOp::Replace(region) => after.as_ref().is_some_and(|p| p.source.intersects(&region.rect))
                        || state.iter().enumerate().any(|(j, (p, _))| Some(j) != at && p.source.intersects(&region.rect)),
                    _ => false,
                }) {
                    continue;
                }
                *state = candidate;
                pivot_ops.extend(out);
            }
            _ => continue,
        }
        count += 1;
    }
    if let Some(tables) = tables.filter(|tables| tables != ours.tables()) {
        ops.push(GridOp::Tables(tables));
    }
    if let Some(pivots) = pivots.filter(|state| !pivot_ops.is_empty() || *state != ours.pivot_state()) {
        ops.extend(pivot_ops);
        ops.push(GridOp::Pivots(pivots));
    }
    count
}
