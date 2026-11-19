//! Align three workbook copies before their cell changes are compared.
use std::collections::{BTreeMap, HashMap};

use officina_core::history::Op;

use crate::addr::CellAddr;
use crate::book::Input;
use crate::compare::{CellSnapshot, Change, MergeError, MergePlan};
use crate::ops::GridOp;
use crate::shift::{shift_op, Axis};
use crate::Workbook;

type Inputs = BTreeMap<(u32, u32), Input>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Splice {
    name: String,
    axis: Axis,
    at: u32,
    delta: i64,
}

pub enum Alignment {
    Positional(MergePlan),
    Blocked(Vec<Change>),
    Aligned { ops: Vec<GridOp>, local: Box<Workbook>, cells: MergePlan },
}

pub fn align(base: &CellSnapshot, ours: &Workbook, incoming: &Workbook) -> Result<Alignment, MergeError> {
    let local = CellSnapshot::capture(ours);
    let remote = CellSnapshot::capture(incoming);
    if base.sheets.keys().ne(local.sheets.keys()) || base.sheets.keys().ne(remote.sheets.keys()) {
        return Err(MergeError::SheetLayout);
    }
    if local == remote {
        return Ok(Alignment::Positional(MergePlan { incoming: Vec::new(), conflicts: Vec::new() }));
    }
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut conflicts = Vec::new();
    for (name, cells) in &base.sheets {
        let sheet = ours.sheet_id(name).expect("matching sheet");
        for axis in [Axis::Rows, Axis::Cols] {
            for (branch, shifts) in [(&local, &mut left), (&remote, &mut right)] {
                match infer(cells, &branch.sheets[name], axis) {
                    Ok(Some((at, delta))) => shifts.push(Splice { name: name.clone(), axis, at, delta }),
                    Ok(None) => {}
                    Err(()) => conflicts.push(flag(sheet, axis, 0, "Cannot align the row or column changes")),
                }
            }
        }
    }
    if !conflicts.is_empty() { return Ok(Alignment::Blocked(conflicts)); }
    if left.is_empty() && right.is_empty() {
        return base.plan_with(ours, &local, &remote).map(Alignment::Positional);
    }

    drop(local);
    drop(remote);
    let mut base_local = from_snapshot(base)?;
    let mut base_remote = from_snapshot(base)?;
    for shift in &left { apply(&mut base_local, shift)?; }
    for shift in &right { apply(&mut base_remote, shift)?; }

    for shift in left.iter().chain(&right) {
        let (others, branch, reference) = if left.contains(shift) { (&right, incoming, &base_remote) }
            else { (&left, ours, &base_local) };
        let related = others.iter().find(|s| s.name == shift.name && s.axis == shift.axis);
        if related == Some(shift) && shift.delta < 0 { continue; }
        let sheet = ours.sheet_id(&shift.name).expect("matching sheet");
        if related.is_some_and(|s| overlaps(s, shift)) {
            let conflict = flag(sheet, shift.axis, shift.at, "Overlapping structural edits");
            if !conflicts.contains(&conflict) { conflicts.push(conflict); }
            continue;
        }
        if shift.delta < 0 && changed_in_band(branch, reference, &translated(shift, related)) {
            conflicts.push(flag(sheet, shift.axis, shift.at, "Deletion overlaps a cell edit"));
        }
    }
    if !conflicts.is_empty() { return Ok(Alignment::Blocked(conflicts)); }

    drop(base_remote);
    let mut normalized_base = base_local;
    let mut normalized_local = copy(ours)?;
    let mut normalized_remote = copy(incoming)?;
    let mut ops = Vec::new();
    for shift in &right {
        if left.contains(shift) { continue; }
        let after_left = translated(shift, same_axis(&left, shift));
        let op = operation(&normalized_local, &after_left)?;
        op.clone().apply(&mut normalized_local);
        normalized_local.recalc();
        ops.push(op);
        apply(&mut normalized_base, &after_left)?;
    }
    for shift in &left {
        if right.contains(shift) { continue; }
        apply(&mut normalized_remote, &translated(shift, same_axis(&right, shift)))?;
    }
    let cells = CellSnapshot::capture(&normalized_base)
        .plan(&normalized_local, &CellSnapshot::capture(&normalized_remote))?;
    Ok(Alignment::Aligned { ops, local: Box::new(normalized_local), cells })
}

fn flag(sheet: u16, axis: Axis, at: u32, reason: &'static str) -> Change {
    Change::StructuralConflict { sheet, axis, at, reason }
}

fn same_axis<'a>(shifts: &'a [Splice], shift: &Splice) -> Option<&'a Splice> {
    shifts.iter().find(|s| s.name == shift.name && s.axis == shift.axis)
}

fn overlaps(a: &Splice, b: &Splice) -> bool {
    let end = |s: &Splice| s.at + (-s.delta).max(0) as u32;
    match (a.delta > 0, b.delta > 0) {
        (true, true) => a.at == b.at,
        (true, false) => (b.at..end(b)).contains(&a.at),
        (false, true) => (a.at..end(a)).contains(&b.at),
        (false, false) => a.at < end(b) && b.at < end(a),
    }
}

fn translated(shift: &Splice, previous: Option<&Splice>) -> Splice {
    let mut shift = shift.clone();
    if let Some(previous) = previous.filter(|p| p.at < shift.at) {
        shift.at = (i64::from(shift.at) + previous.delta) as u32;
    }
    shift
}

fn coordinate(axis: Axis, (row, col): (u32, u32)) -> u32 {
    match axis { Axis::Rows => row, Axis::Cols => col }
}

// Match literals that occur exactly once on each side. Formula text and
// repeated labels cannot by themselves establish a row or column identity.
fn anchors(cells: &Inputs) -> HashMap<String, Option<(u32, u32)>> {
    let mut out = HashMap::new();
    for (&addr, input) in cells {
        if let Input::Value(value) = input {
            out.entry(format!("{value:?}")).and_modify(|a| *a = None).or_insert(Some(addr));
        }
    }
    out
}

fn infer(base: &Inputs, side: &Inputs, axis: Axis) -> Result<Option<(u32, i64)>, ()> {
    let left = anchors(base);
    let right = anchors(side);
    let mut pairs: Vec<_> = left.into_iter().filter_map(|(key, a)| {
        Some((coordinate(axis, a?), coordinate(axis, (*right.get(&key)?)?)))
    }).collect();
    pairs.sort_unstable();
    pairs.dedup();
    let Some(first) = pairs.iter().position(|(a, b)| a != b) else { return Ok(None); };
    let extent = |cells: &Inputs| cells.keys().map(|&a| coordinate(axis, a) + 1).max().unwrap_or(0);
    // Moving a literal between cells without changing the used axis is
    // ordinary cell editing. A splice needs both displacement and growth/shrinkage.
    let delta = i64::from(extent(side)) - i64::from(extent(base));
    if delta == 0 {
        let ordered = pairs.windows(2).all(|p| p[0].0 < p[1].0 && p[0].1 < p[1].1);
        if pairs.len() > 1 && ordered { return Err(()); }
        return Ok(None);
    }
    let (a, b) = pairs[first];
    if i64::from(b) - i64::from(a) != delta || pairs[first..].iter().any(|&(a, b)| i64::from(b) - i64::from(a) != delta) {
        return Err(());
    }
    let at = i64::from(a) + delta.min(0);
    if at < 0 || first.checked_sub(1).map_or(0, |i| i64::from(pairs[i].0) + 1) != at {
        return Err(());
    }
    Ok(Some((at as u32, delta)))
}

fn from_snapshot(snapshot: &CellSnapshot) -> Result<Workbook, MergeError> {
    let mut wb = Workbook::new();
    for name in snapshot.sheets.keys() { wb.add_sheet(name).map_err(|_| MergeError::SheetLayout)?; }
    for (name, cells) in &snapshot.sheets {
        let sheet = wb.sheet_id(name).expect("matching sheet");
        for (&(row, col), value) in cells {
            wb.set(CellAddr::new(sheet, row, col), value).map_err(|_| MergeError::StructuralAlignment)?;
        }
    }
    wb.recalc();
    Ok(wb)
}

fn copy(wb: &Workbook) -> Result<Workbook, MergeError> {
    let bytes = crate::native::save(wb, Vec::new()).map_err(|_| MergeError::StructuralAlignment)?;
    crate::native::load(&bytes).map_err(|_| MergeError::StructuralAlignment)
}

fn operation(wb: &Workbook, splice: &Splice) -> Result<GridOp, MergeError> {
    let sheet = wb.sheet_id(&splice.name).ok_or(MergeError::SheetLayout)?;
    shift_op(wb, sheet, splice.axis, splice.at, splice.delta).ok_or(MergeError::StructuralAlignment)
}

fn apply(wb: &mut Workbook, splice: &Splice) -> Result<(), MergeError> {
    operation(wb, splice)?.apply(wb);
    wb.recalc();
    Ok(())
}

fn changed_in_band(branch: &Workbook, reference: &Workbook, splice: &Splice) -> bool {
    let local = CellSnapshot::capture(branch);
    let before = CellSnapshot::capture(reference);
    let local = &local.sheets[&splice.name];
    let before = &before.sheets[&splice.name];
    let band = splice.at..splice.at + (-splice.delta) as u32;
    local.keys().chain(before.keys()).any(|&addr| {
        band.contains(&coordinate(splice.axis, addr)) && local.get(&addr) != before.get(&addr)
    })
}
