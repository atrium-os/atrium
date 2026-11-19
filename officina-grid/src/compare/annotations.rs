//! Sparse cell metadata follows the same row alignment as values and styles.
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::addr::{a1, CellAddr, Rect};
use crate::comments::{Comment, MAX_COMMENTS};
use crate::links::{Link, MAX_LINKS};
use crate::ops::GridOp;
use crate::Workbook;

use super::parts::{action, RowRun};
use super::{Change, MAX_CHANGES};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationChange<T> {
    pub(super) at: CellAddr,
    sheet_name: Arc<str>,
    before: Option<T>,
    after: Option<T>,
}

impl<T> AnnotationChange<T> {
    pub(super) fn versions(&self) -> (Option<&T>, Option<&T>) {
        (self.before.as_ref(), self.after.as_ref())
    }

    pub(super) fn on_current_sheet(&self, wb: &Workbook) -> bool {
        (self.at.sheet as usize) < wb.sheet_count()
            && wb.sheet(self.at.sheet).name.eq_ignore_ascii_case(&self.sheet_name)
    }

    pub(super) fn describe(&self, label: &str) -> String {
        let verb = action(self.before.is_some(), self.after.is_some());
        format!("{label} {} {verb}", a1(self.at.row, self.at.col))
    }
}

impl AnnotationChange<Comment> {
    pub(super) fn label(&self) -> &'static str {
        if self.before.iter().chain(&self.after).any(|comment| !comment.thread().is_empty()) {
            "Comment thread"
        } else {
            "Comment"
        }
    }
}

impl<T: Clone + PartialEq> AnnotationChange<T> {
    pub(super) fn take(&self, current: Option<&T>, count: usize, limit: usize) -> Option<Edit<T>> {
        if current != self.before.as_ref() || (current.is_none() && self.after.is_some() && count >= limit) {
            return None;
        }
        Some((self.at.row, self.at.col, self.after.clone()))
    }
}

type Coordinate = (u32, u32);
type Edit<T> = (u32, u32, Option<T>);

fn mapped<'a, T>(entries: &'a BTreeMap<Coordinate, T>, pairs: &'a [RowRun], remote: bool)
    -> impl Iterator<Item = (Coordinate, &'a T)>
{
    let mut index = 0;
    entries.iter().filter_map(move |(&(r, c), entry)| {
        loop {
            let &(local, incoming, len) = pairs.get(index)?;
            let start = if remote { incoming } else { local };
            if r < start { return None; }
            if r < start + len {
                return Some(((local + (r - start), c), entry));
            }
            index += 1;
        }
    })
}

pub(super) fn differences<'a, T: PartialEq + 'a>(
    local: impl Iterator<Item = (Coordinate, &'a T)>,
    remote: impl Iterator<Item = (Coordinate, &'a T)>,
    mut emit: impl FnMut(Coordinate, Option<&'a T>, Option<&'a T>) -> bool,
) -> bool {
    let (mut local, mut remote) = (local.peekable(), remote.peekable());
    let end = (u32::MAX, u32::MAX);
    while local.peek().is_some() || remote.peek().is_some() {
        let a = local.peek().map_or(end, |(at, _)| *at);
        let b = remote.peek().map_or(end, |(at, _)| *at);
        let left = if a <= b { local.next().map(|(_, v)| v) } else { None };
        let right = if b <= a { remote.next().map(|(_, v)| v) } else { None };
        if left != right && emit(a.min(b), left, right) {
            return true;
        }
    }
    false
}

fn compare_kind<T: Clone + PartialEq>(
    local: &BTreeMap<Coordinate, T>, remote: &BTreeMap<Coordinate, T>,
    sheet: u16, sheet_name: &Arc<str>, pairs: &[RowRun], out: &mut Vec<Change>, wrap: fn(AnnotationChange<T>) -> Change,
) -> bool {
    differences(mapped(local, pairs, false), mapped(remote, pairs, true), |(row, col), before, after| {
        out.push(wrap(AnnotationChange {
            at: CellAddr::new(sheet, row, col), sheet_name: sheet_name.clone(),
            before: before.cloned(), after: after.cloned(),
        }));
        out.len() >= MAX_CHANGES
    })
}

pub(super) fn compare(ours: &Workbook, s: u16, theirs: &Workbook, t: u16, pairs: &[RowRun], out: &mut Vec<Change>) -> bool {
    let sheet_name: Arc<str> = ours.sheet_name(s).into();
    compare_kind(ours.comments(s), theirs.comments(t), s, &sheet_name, pairs, out, Change::Comment)
        || compare_kind(ours.links(s), theirs.links(t), s, &sheet_name, pairs, out, Change::Link)
}

fn copy_rows<T: Clone>(
    entries: &BTreeMap<Coordinate, T>, rows: RangeInclusive<u32>, dest: u32,
) -> Vec<Edit<T>> {
    let first = *rows.start();
    entries.range((first, 0)..=(*rows.end(), u32::MAX))
        .take(MAX_CHANGES + 1)
        .map(|(&(row, col), value)| (dest + (row - first), col, Some(value.clone()))).collect()
}

pub(super) fn insert_rows(ours: &Workbook, theirs: &Workbook, source: Rect, target: Rect) -> Option<Vec<GridOp>> {
    let rows = source.r0..=source.r1;
    let comments = copy_rows(theirs.comments(source.sheet), rows.clone(), target.r0);
    let links = copy_rows(theirs.links(source.sheet), rows, target.r0);
    if comments.len() + links.len() > MAX_CHANGES
        || ours.comments(target.sheet).len() + comments.len() > MAX_COMMENTS
        || ours.links(target.sheet).len() + links.len() > MAX_LINKS {
        return None;
    }
    Some(ops(target.sheet, comments, links))
}

fn sheet_edits<T: Clone + PartialEq>(
    local: &BTreeMap<Coordinate, T>, remote: &BTreeMap<Coordinate, T>,
) -> Option<Vec<Edit<T>>> {
    let mut removed = Vec::new();
    let mut updated = Vec::new();
    let truncated = differences(local.iter().map(|(&at, v)| (at, v)), remote.iter().map(|(&at, v)| (at, v)), |(r, c), _, after| {
        match after {
            None => removed.push((r, c, None)),
            Some(value) => updated.push((r, c, Some(value.clone()))),
        }
        removed.len() + updated.len() > MAX_CHANGES
    });
    if truncated { return None; }
    removed.extend(updated);
    Some(removed)
}

pub(super) fn take_sheet(ours: &Workbook, s: u16, theirs: &Workbook, t: u16) -> Option<Vec<GridOp>> {
    let comments = sheet_edits(ours.comments(s), theirs.comments(t))?;
    let links = sheet_edits(ours.links(s), theirs.links(t))?;
    (comments.len() + links.len() <= MAX_CHANGES).then(|| ops(s, comments, links))
}

/// Plan sparse edits against one running capacity per sheet. Removals come
/// first so a selected addition can use their space in the same transaction.
fn selected<'a, T: Clone + PartialEq + 'a>(
    wb: &'a Workbook, changes: impl Iterator<Item = &'a AnnotationChange<T>>,
    current: impl Fn(&'a Workbook, u16) -> &'a BTreeMap<Coordinate, T>, limit: usize,
) -> BTreeMap<u16, Vec<Edit<T>>> {
    let mut groups = BTreeMap::<u16, (usize, Vec<Edit<T>>)>::new();
    let mut seen = BTreeSet::new();
    let (removed, upsert): (Vec<_>, Vec<_>) = changes.partition(|change| change.after.is_none());
    for change in removed.into_iter().chain(upsert) {
        let CellAddr { sheet, row, col } = change.at;
        if !change.on_current_sheet(wb) { continue; }
        let entries = current(wb, sheet);
        let before = entries.get(&(row, col));
        let (count, set) = groups.entry(sheet).or_insert_with(|| (entries.len(), Vec::new()));
        if let Some(edit) = change.take(before, *count, limit).filter(|_| seen.insert(change.at)) {
            *count = *count - usize::from(before.is_some()) + usize::from(change.after.is_some());
            set.push(edit);
        }
    }
    groups.into_iter().filter_map(|(s, (_, set))| (!set.is_empty()).then_some((s, set))).collect()
}

pub(super) fn take_selected(wb: &Workbook, changes: &[&Change]) -> (Vec<GridOp>, usize) {
    let comments = changes.iter().filter_map(|change| match change { Change::Comment(c) => Some(c), _ => None });
    let links = changes.iter().filter_map(|change| match change { Change::Link(c) => Some(c), _ => None });
    let mut count = 0;
    let mut ops = Vec::new();
    for (sheet, set) in selected(wb, comments, Workbook::comments, MAX_COMMENTS) {
        count += set.len();
        ops.push(GridOp::Comments { sheet, set });
    }
    for (sheet, set) in selected(wb, links, Workbook::links, MAX_LINKS) {
        count += set.len();
        ops.push(GridOp::Links { sheet, set });
    }
    (ops, count)
}

fn ops(sheet: u16, comments: Vec<Edit<Comment>>, links: Vec<Edit<Link>>) -> Vec<GridOp> {
    let mut ops = Vec::new();
    if !comments.is_empty() { ops.push(GridOp::Comments { sheet, set: comments }); }
    if !links.is_empty() { ops.push(GridOp::Links { sheet, set: links }); }
    ops
}
