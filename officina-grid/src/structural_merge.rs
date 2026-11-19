//! Align three workbook copies before their cell changes are compared.
use std::collections::{BTreeMap, HashMap};

use officina_core::history::Op;

use crate::addr::CellAddr;
use crate::axis_ids::Run;
use crate::book::Input;
use crate::compare::{CellSnapshot, Change, MergeError, MergePlan};
use crate::ops::GridOp;
use crate::shift::{shift_with_ids, Axis};
use crate::Workbook;

type Inputs = BTreeMap<(u32, u32), Input>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Patch {
    name: String,
    axis: Axis,
    at: u32,
    remove: u32,
    insert: u32,
    /// Legacy alignment has no recorded identities to carry over.
    added: Option<Vec<Run>>,
}

impl Patch {
    fn delta(&self) -> i64 { i64::from(self.insert) - i64::from(self.remove) }

    fn on_same_axis(&self, other: &Self) -> bool {
        self.name == other.name && self.axis == other.axis
    }
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
            let index = axis.index();
            let origin = &base.axes[name][index];
            let recorded = [&local.axes[name][index], &remote.axes[name][index]]
                .into_iter().any(|ids| !ids.is_implicit()) || !origin.is_implicit();
            for (branch, patches) in [(&local, &mut left), (&remote, &mut right)] {
                if recorded {
                    match origin.differences(&branch.axes[name][index]) {
                        Some(gaps) => patches.extend(gaps.into_iter().map(|gap| Patch {
                            name: name.clone(), axis, at: gap.at, remove: gap.remove,
                            insert: gap.added.iter().map(|r| r.len).sum(), added: Some(gap.added),
                        })),
                        None => conflicts.push(flag(sheet, axis, 0, "Cannot align recorded axis identities")),
                    }
                } else {
                    match infer(cells, &branch.sheets[name], axis) {
                        Ok(Some((at, delta))) => patches.push(Patch {
                            name: name.clone(), axis, at,
                            remove: (-delta).max(0) as u32, insert: delta.max(0) as u32, added: None,
                        }),
                        Ok(None) => {}
                        Err(()) => conflicts.push(flag(sheet, axis, 0, "Cannot align the row or column changes")),
                    }
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
    normalize(&mut base_local, &left, &[], false)?;
    normalize(&mut base_remote, &right, &[], false)?;

    for (patches, others, branch, reference) in [
        (&left, &right, incoming, &base_remote),
        (&right, &left, ours, &base_local),
    ] {
        let branch = CellSnapshot::capture(branch);
        let reference = CellSnapshot::capture(reference);
        for patch in patches {
            // Matching recorded inserts have the same identity. Two independent
            // inserts at the same gap have different identities and conflict.
            let shared = others.contains(patch) && (patch.insert == 0 || patch.added.is_some());
            if shared { continue; }
            let sheet = ours.sheet_id(&patch.name).expect("matching sheet");
            if others.iter().any(|p| patch.on_same_axis(p) && overlaps(p, patch)) {
                let conflict = flag(sheet, patch.axis, patch.at, "Overlapping structural edits");
                if !conflicts.contains(&conflict) { conflicts.push(conflict); }
                continue;
            }
            if patch.remove != 0 && changed_in_band(&branch, &reference, &translated(patch, others.iter())?) {
                conflicts.push(flag(sheet, patch.axis, patch.at, "Deletion overlaps a cell edit"));
            }
        }
    }
    if !conflicts.is_empty() { return Ok(Alignment::Blocked(conflicts)); }

    drop(base_remote);
    let mut normalized_base = base_local;
    let mut normalized_local = copy(ours)?;
    let mut normalized_remote = copy(incoming)?;
    let ops = normalize(&mut normalized_local, &right, &left, true)?;
    normalize(&mut normalized_base, &right, &left, false)?;
    normalize(&mut normalized_remote, &left, &right, false)?;
    let cells = CellSnapshot::capture(&normalized_base)
        .plan(&normalized_local, &CellSnapshot::capture(&normalized_remote))?;
    Ok(Alignment::Aligned { ops, local: Box::new(normalized_local), cells })
}

fn flag(sheet: u16, axis: Axis, at: u32, reason: &'static str) -> Change {
    Change::StructuralConflict { sheet, axis, at, reason }
}

fn overlaps(a: &Patch, b: &Patch) -> bool {
    let ar = a.at..a.at + a.remove;
    let br = b.at..b.at + b.remove;
    (a.remove != 0 && b.remove != 0 && ar.start < br.end && br.start < ar.end)
        || (a.insert != 0 && (br.contains(&a.at) || (b.insert != 0 && a.at == b.at)))
        || (b.insert != 0 && ar.contains(&b.at))
}

fn translated<'a>(patch: &Patch, previous: impl Iterator<Item = &'a Patch>) -> Result<Patch, MergeError> {
    let delta: i64 = previous.filter(|p| patch.on_same_axis(p) && p.at < patch.at)
        .map(Patch::delta).sum();
    let mut out = patch.clone();
    out.at = u32::try_from(i64::from(patch.at) + delta).map_err(|_| MergeError::StructuralAlignment)?;
    Ok(out)
}

fn normalize(wb: &mut Workbook, patches: &[Patch], previous: &[Patch], keep: bool) -> Result<Vec<GridOp>, MergeError> {
    let mut done = Vec::new();
    let mut ops = Vec::new();
    for patch in patches {
        if previous.contains(patch) { continue; }
        let place = translated(patch, previous.iter().chain(done.iter().copied()))?;
        let sheet = wb.sheet_id(&place.name).ok_or(MergeError::SheetLayout)?;
        for (n, ids) in [
            (-i64::from(place.remove), place.added.as_ref().map(|_| &[][..])),
            (i64::from(place.insert), place.added.as_deref()),
        ] {
            if n == 0 { continue; }
            let op = shift_with_ids(wb, sheet, place.axis, place.at, n, ids)
                .ok_or(MergeError::StructuralAlignment)?;
            if keep { ops.push(op.clone()); }
            op.apply(wb);
            wb.recalc();
        }
        done.push(patch);
    }
    Ok(ops)
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
        wb.sheet_mut(sheet).axis_ids = snapshot.axes[name].clone();
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

fn changed_in_band(branch: &CellSnapshot, reference: &CellSnapshot, patch: &Patch) -> bool {
    let local = &branch.sheets[&patch.name];
    let before = &reference.sheets[&patch.name];
    let band = patch.at..patch.at + patch.remove;
    local.keys().chain(before.keys()).any(|&addr| {
        band.contains(&coordinate(patch.axis, addr)) && local.get(&addr) != before.get(&addr)
    })
}
