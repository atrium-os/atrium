//! Worksheet additions are staged together so formulas can refer to another
//! selected tab. Build only operations that can be journaled and undone.
use officina_core::history::Op;

use super::{annotations, layout, settings, tables, workbook_parts, Change};
use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::ops::{GridOp, Region};
use crate::Workbook;

pub fn take_added_sheets(
    local: &Workbook,
    incoming: &Workbook,
    names: &[&str],
) -> Option<Vec<GridOp>> {
    let mut preview = crate::structural_merge::copy(local).ok()?;
    let mut ops = Vec::new();
    let mut added = Vec::new();
    for &name in names {
        let source = incoming.sheet_id(name)?;
        if incoming.sheet_protected(source) {
            return None;
        }
        let identity = incoming.sheet(source).identity;
        let target = preview.add_identified_sheet(name, identity).ok()?;
        ops.push(GridOp::AddIdentifiedSheet {
            name: name.into(),
            identity,
        });
        added.push((target, source));
    }
    for (target, source) in &added {
        let t = *target;
        let s = *source;
        let area = Rect {
            sheet: s,
            r0: 0,
            c0: 0,
            r1: MAX_ROWS - 1,
            c1: MAX_COLS - 1,
        };
        let pivot_areas = tables::pivot_areas(incoming, s);
        let spill_areas: Vec<_> = incoming
            .spills()
            .filter(|(a, _)| a.sheet == s)
            .filter_map(|(a, _)| incoming.spill_area(*a))
            .collect();
        let Region {
            mut values, fills, ..
        } = incoming.snapshot(&area);
        values.retain(|&(r, c, _)| {
            !pivot_areas
                .iter()
                .chain(&spill_areas)
                .any(|a| a.contains(CellAddr::new(s, r, c)))
        });
        for fill in &fills {
            preview.parse(&fill.src, t, fill.r0, fill.col).ok()?;
        }
        let mut styles = incoming.style_snapshot(&area);
        styles.rect.sheet = t;
        let mut edits = Vec::new();
        if !values.is_empty() || !fills.is_empty() {
            let (rows, cols) = incoming.extent(s);
            let rect = Rect {
                sheet: t,
                r0: 0,
                c0: 0,
                r1: rows.saturating_sub(1),
                c1: cols.saturating_sub(1),
            };
            edits.push(GridOp::Replace(Region {
                rect,
                values,
                fills,
            }));
        }
        if !styles.spans.is_empty() {
            edits.push(GridOp::Restyle(styles));
        }
        edits.extend(annotations::take_sheet(&preview, t, incoming, s)?);
        edits.extend(layout::take_sheet(&preview, t, incoming, s));
        let mut config = Vec::new();
        settings::compare(&preview, t, incoming, s, &mut config);
        for entry in config {
            edits.extend(super::take(&preview, incoming, &entry)?);
        }
        for axis in [crate::shift::Axis::Rows, crate::shift::Axis::Cols] {
            let ids = &incoming.sheet(s).axis_ids[axis.index()];
            if !ids.is_implicit() {
                edits.push(GridOp::AxisIdentities {
                    sheet: t,
                    axis,
                    ids: ids.clone(),
                });
            }
        }
        for op in edits {
            op.clone().apply(&mut preview);
            ops.push(op);
        }
    }
    preview.recalc();
    let changes: Vec<_> = workbook_parts(&preview, incoming)
        .filter(|change| {
            change
                .sheet()
                .is_some_and(|s| added.iter().any(|&(t, _)| t == s))
                && match change {
                    Change::Table(change) => change.is_addition(),
                    Change::Pivot(change) => change.is_addition(),
                    _ => true,
                }
        })
        .collect();
    let (objects, taken) = super::take_objects(&preview, incoming, &changes);
    if taken != changes.len() {
        return None;
    }
    ops.extend(objects);
    Some(ops)
}

pub(super) fn matched_sheet(local: &Workbook, sheet: u16, other: &Workbook) -> Option<u16> {
    let id = local.sheet(sheet).identity;
    (id != 0)
        .then(|| (0..other.sheet_count() as u16).find(|&s| other.sheet(s).identity == id))
        .flatten()
        .or_else(|| other.sheet_id(&local.sheet_name(sheet)))
}

/// Use local spelling for identified sheets when comparing formulas and
/// references. Indices in the source stay the same.
pub fn named_for_review(
    local: &Workbook,
    source: &Workbook,
) -> Result<Option<Workbook>, super::MergeError> {
    let mut renames = Vec::new();
    for s in 0..local.sheet_count() as u16 {
        let Some(t) = matched_sheet(local, s, source) else {
            continue;
        };
        let name = local.sheet_name(s);
        if source.sheet_name(t) != name {
            renames.push((t, name));
        }
    }
    if renames.is_empty() {
        return Ok(None);
    }
    let mut copy = crate::structural_merge::copy(source)?;
    for (sheet, name) in renames {
        crate::sheetops::rename_sheet_op(&copy, sheet, &name)
            .ok_or(super::MergeError::SheetLayout)?
            .apply(&mut copy);
    }
    Ok(Some(copy))
}

pub struct RenamePlan {
    pub ops: Vec<GridOp>,
    pub conflicts: Vec<Change>,
}

pub fn plan_renames(base: &Workbook, local: &Workbook, source: &Workbook) -> RenamePlan {
    let mut plan = RenamePlan {
        ops: Vec::new(),
        conflicts: Vec::new(),
    };
    for sheet in 0..base.sheet_count() as u16 {
        let (Some(ours), Some(theirs)) = (
            matched_sheet(base, sheet, local),
            matched_sheet(base, sheet, source),
        ) else {
            continue;
        };
        let before = base.sheet_name(sheet);
        let local_name = local.sheet_name(ours);
        let incoming_name = source.sheet_name(theirs);
        if incoming_name == before || incoming_name == local_name {
            continue;
        }
        if local_name == before {
            if let Some(op) = crate::sheetops::rename_sheet_op(local, ours, &incoming_name) {
                plan.ops.push(op);
                continue;
            }
        }
        plan.conflicts.push(Change::SheetRenamed {
            sheet: ours,
            before: local_name,
            after: incoming_name,
        });
    }
    plan
}

pub(super) fn on_new_sheet(change: &Change, local: &Workbook, incoming: &Workbook) -> bool {
    if change.sheet().is_some() {
        return false;
    }
    match change {
        Change::Chart(change) => local.sheet_id(&change.key(local, incoming).0).is_none(),
        Change::Picture(change) => local.sheet_id(&change.key(local, incoming).0).is_none(),
        Change::Name(change) => change
            .scope
            .as_ref()
            .is_some_and(|name| local.sheet_id(name).is_none()),
        Change::Table(change) => change.on_new_sheet(local, incoming),
        Change::Pivot(change) => change.on_new_sheet(local, incoming),
        _ => false,
    }
}
