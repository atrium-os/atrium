//! Sparse sheet geometry in two-way review.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::addr::{a1, col_name, CellAddr, Rect};
use crate::merges::MAX_MERGES;
use crate::ops::GridOp;
use crate::shift::{Axis, Shift};
use crate::Workbook;

use super::annotations::differences;
use super::parts::{action, RowRun};
use super::{Change, MAX_CHANGES};

#[derive(Debug, Clone, PartialEq)]
pub struct MergeChange {
    pub(super) at: CellAddr,
    name: Arc<str>,
    before: Option<Rect>,
    after: Option<Rect>,
}

impl MergeChange {
    pub(super) fn reorder(&mut self, sheet: u16) {
        self.at.sheet = sheet;
        for r in self.before.iter_mut().chain(self.after.iter_mut()) { r.sheet = sheet; }
    }

    pub(super) fn ranges(&self) -> (String, String) {
        let show = |rect: Option<Rect>| rect.map_or_else(|| "(none)".into(), |r| {
            format!("{}:{}", a1(r.r0, r.c0), a1(r.r1, r.c1))
        });
        (show(self.before), show(self.after))
    }

    pub(super) fn describe(&self) -> String {
        let rect = self.after.or(self.before).unwrap();
        let range = format!("{}:{}", a1(rect.r0, rect.c0), a1(rect.r1, rect.c1));
        let verb = action(self.before.is_some(), self.after.is_some());
        format!("Merge {range} {verb}")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Size {
    over: Option<f64>,
    /// The width to restore when a hidden column is unhidden.
    hidden: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizeChange {
    pub(super) at: CellAddr,
    name: Arc<str>,
    whole_axis: bool,
    before: Size,
    after: Size,
}

impl SizeChange {
    pub(super) fn describe(&self, rows: bool) -> String {
        let label = match (self.whole_axis, rows) {
            (true, true) => "Default row height".into(),
            (true, false) => "Default column width".into(),
            (false, true) => format!("Row {} height", self.at.row + 1),
            (false, false) => format!("Column {} width", col_name(self.at.col)),
        };
        let show = |size: Size| match (size.over, size.hidden) {
            (Some(0.0), Some(width)) => format!("hidden ({width})"),
            (Some(width), _) => width.to_string(),
            _ => "default".into(),
        };
        format!("{label}: {} → {}", show(self.before), show(self.after))
    }
}

fn matches_sheet(wb: &Workbook, sheet: u16, name: &str) -> bool {
    (sheet as usize) < wb.sheet_count() && wb.sheet(sheet).name.eq_ignore_ascii_case(name)
}

/// The paired run containing a row, and where that row lands locally.
fn project(pairs: &[RowRun], row: u32, remote: bool) -> Option<(usize, u32)> {
    let start = |(local, incoming, _): RowRun| if remote { incoming } else { local };
    let index = pairs.partition_point(|&run| start(run) + run.2 <= row);
    let run = *pairs.get(index)?;
    (start(run) <= row).then(|| (index, run.0 + row - start(run)))
}

fn merge_map(wb: &Workbook, sheet: u16, local: u16, pairs: &[RowRun], remote: bool) -> BTreeMap<(u32, u32), Option<Rect>> {
    wb.merges(sheet).iter().filter_map(|rect| {
        let (run, r0) = project(pairs, rect.r0, remote)?;
        let col = rect.c0;
        // A merge intersecting a structural edit waits until the rows are
        // resolved. Its paired anchor still masks a counterpart on the other side.
        let rect = project(pairs, rect.r1, remote).filter(|&(end, _)| end == run)
            .map(|(_, r1)| Rect { sheet: local, r0, r1, ..*rect });
        Some(((r0, col), rect))
    }).collect()
}

fn default_size(wb: &Workbook, sheet: u16, rows: bool) -> f64 {
    let sh = wb.sheet(sheet);
    if rows { sh.row_sizes.default } else { sh.col_sizes.default }
}

fn size_map(wb: &Workbook, sheet: u16, rows: bool) -> BTreeMap<u32, Size> {
    let sh = wb.sheet(sheet);
    let sizes = if rows { &sh.row_sizes } else { &sh.col_sizes };
    let mut map: BTreeMap<_, _> = sizes.overrides().iter().map(|&(i, over)| (i, Size { over: Some(over), hidden: None })).collect();
    if !rows {
        for &(i, width) in &sh.hidden_widths {
            map.entry(i).or_default().hidden = Some(width);
        }
    }
    map
}

pub(super) fn compare(ours: &Workbook, s: u16, theirs: &Workbook, t: u16, pairs: &[RowRun], out: &mut Vec<Change>) -> bool {
    let name: Arc<str> = ours.sheet_name(s).into();
    let left = merge_map(ours, s, s, pairs, false);
    let right = merge_map(theirs, t, s, pairs, true);
    if differences(left.iter().map(|(&at, v)| (at, v)), right.iter().map(|(&at, v)| (at, v)), |(r, c), before, after| {
        if before != Some(&None) && after != Some(&None) {
            out.push(Change::Merge(MergeChange {
                at: CellAddr::new(s, r, c), name: name.clone(),
                before: before.copied().flatten(), after: after.copied().flatten(),
            }));
        }
        out.len() >= MAX_CHANGES
    }) {
        return true;
    }
    for rows in [true, false] {
        let wrap = if rows { Change::RowHeight } else { Change::ColumnWidth };
        let before = default_size(ours, s, rows);
        let after = default_size(theirs, t, rows);
        if before != after {
            out.push(wrap(SizeChange {
                at: CellAddr::new(s, 0, 0), name: name.clone(), whole_axis: true,
                before: Size { over: Some(before), hidden: None },
                after: Size { over: Some(after), hidden: None },
            }));
            if out.len() >= MAX_CHANGES { return true; }
        }
        let left = size_map(ours, s, rows);
        let right = size_map(theirs, t, rows);
        let entries = |i, remote| if rows { project(pairs, i, remote).map(|(_, r)| (r, 0)) } else { Some((0, i)) };
        if differences(
            left.iter().filter_map(|(&i, v)| Some((entries(i, false)?, v))),
            right.iter().filter_map(|(&i, v)| Some((entries(i, true)?, v))),
            |(r, c), before, after| {
                let change = SizeChange {
                    at: CellAddr::new(s, r, c), name: name.clone(), whole_axis: false,
                    before: before.copied().unwrap_or_default(), after: after.copied().unwrap_or_default(),
                };
                out.push(wrap(change));
                out.len() >= MAX_CHANGES
            },
        ) {
            return true;
        }
    }
    false
}

fn size_ops(sheet: u16, rows: bool, default: f64, map: BTreeMap<u32, Size>) -> Vec<GridOp> {
    let over = map.iter().filter_map(|(&i, size)| Some((i, size.over?))).collect();
    let mut ops = vec![GridOp::LayoutSizes { sheet, rows, default, over }];
    if !rows {
        let list = map.into_iter().filter_map(|(i, size)| Some((i, size.hidden?))).collect();
        ops.push(GridOp::HiddenWidths { sheet, list });
    }
    ops
}

pub(super) fn take(ours: &Workbook, changes: &[&Change]) -> (Vec<GridOp>, usize) {
    let mut merges = BTreeMap::<u16, Vec<Rect>>::new();
    let mut sizes = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut count = 0;
    let removal = |ch: &&Change| matches!(ch, Change::Merge(m) if m.after.is_none());
    for ch in changes.iter().filter(|ch| removal(ch)).chain(changes.iter().filter(|ch| !removal(ch))) {
        match ch {
            Change::Merge(change) => {
                let sheet = change.at.sheet;
                if !matches_sheet(ours, sheet, &change.name) { continue; }
                let list = merges.entry(sheet).or_insert_with(|| ours.merges(sheet).to_vec());
                let old = list.iter().position(|r| r.r0 == change.at.row && r.c0 == change.at.col);
                if old.map(|i| list[i]) != change.before { continue; }
                if let Some(rect) = change.after {
                    if (old.is_none() && list.len() == MAX_MERGES)
                        || list.iter().enumerate().any(|(i, other)| Some(i) != old && rect.intersects(other)) {
                        continue;
                    }
                }
                if !seen.insert((0, change.at, false)) { continue; }
                if let Some(i) = old { list.remove(i); }
                list.extend(change.after);
            }
            Change::RowHeight(change) | Change::ColumnWidth(change) => {
                let rows = matches!(ch, Change::RowHeight(_));
                let sheet = change.at.sheet;
                if !matches_sheet(ours, sheet, &change.name) { continue; }
                let (default, map) = sizes.entry((sheet, rows))
                    .or_insert_with(|| (default_size(ours, sheet, rows), size_map(ours, sheet, rows)));
                let i = if rows { change.at.row } else { change.at.col };
                let current = if change.whole_axis {
                    Size { over: Some(*default), hidden: None }
                } else {
                    map.get(&i).copied().unwrap_or_default()
                };
                if current != change.before
                    || !seen.insert((if rows { 1 } else { 2 }, change.at, change.whole_axis)) { continue; }
                if change.whole_axis {
                    *default = change.after.over.unwrap();
                } else if change.after == Size::default() {
                    map.remove(&i);
                } else {
                    map.insert(i, change.after);
                }
            }
            _ => continue,
        }
        count += 1;
    }
    let mut ops = Vec::new();
    for (sheet, mut list) in merges {
        list.sort_unstable_by_key(|r| (r.r0, r.c0));
        if list != ours.merges(sheet) { ops.push(GridOp::Merges { sheet, list }); }
    }
    for ((sheet, rows), (default, map)) in sizes {
        if default != default_size(ours, sheet, rows) || map != size_map(ours, sheet, rows) {
            ops.extend(size_ops(sheet, rows, default, map));
        }
    }
    (ops, count)
}

pub(super) fn insert_rows(ours: &Workbook, theirs: &Workbook, source: Rect, target: Rect) -> Vec<GridOp> {
    let sheet = target.sheet;
    let shift = Shift { axis: Axis::Rows, at: target.r0, n: target.rows() as i64 };
    let mut ops = Vec::new();
    let inserted: Vec<_> = theirs.sheet(source.sheet).row_sizes.overrides().iter()
        .filter(|(i, _)| (source.r0..=source.r1).contains(i)).collect();
    if !inserted.is_empty() {
        let mut over: Vec<_> = ours.sheet(sheet).row_sizes.overrides().iter()
            .filter_map(|&(i, v)| Some((shift.map(i)?, v))).collect();
        over.extend(inserted.into_iter().map(|&(i, v)| (target.r0 + i - source.r0, v)));
        over.sort_unstable_by_key(|(i, _)| *i);
        ops.push(GridOp::LayoutSizes { sheet, rows: true, default: default_size(ours, sheet, true), over });
    }

    let mut merges: Vec<_> = ours.merges(sheet).iter().filter_map(|&r| shift.map_rect(r)).filter(|r| r.cells() > 1).collect();
    let original = merges.len();
    for rect in theirs.merges(source.sheet).iter().filter(|r| r.r0 >= source.r0 && r.r1 <= source.r1) {
        let added = Rect { sheet, r0: target.r0 + rect.r0 - source.r0, r1: target.r0 + rect.r1 - source.r0, ..*rect };
        if merges.len() < MAX_MERGES && !merges.iter().any(|r| r.intersects(&added)) {
            merges.push(added);
        }
    }
    if merges.len() != original { ops.push(GridOp::Merges { sheet, list: merges }); }
    ops
}

pub(super) fn take_sheet(ours: &Workbook, s: u16, theirs: &Workbook, t: u16) -> Vec<GridOp> {
    let mut ops = Vec::new();
    let list: Vec<_> = theirs.merges(t).iter().map(|r| Rect { sheet: s, ..*r }).collect();
    if list != ours.merges(s) { ops.push(GridOp::Merges { sheet: s, list }); }
    for rows in [true, false] {
        let sizes = size_map(theirs, t, rows);
        let default = default_size(theirs, t, rows);
        if default != default_size(ours, s, rows) || sizes != size_map(ours, s, rows) {
            ops.extend(size_ops(s, rows, default, sizes));
        }
    }
    ops
}
