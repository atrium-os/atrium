//! Compare with…: the changes that would turn this workbook into another
//! copy, listed in a pane beside the grid (officina.md §16.68). A change is
//! looked at in place, taken from the other copy, or kept as it is here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use officina_grid::compare::{self, Change, MergeError};
use officina_grid::Workbook;

use crate::doc::Doc;

/// The pane's width, at the window's right over the grid.
pub const REVIEW_W: f32 = 360.0;
/// A change's row in the list.
pub const REVIEW_ROW_H: f32 = 30.0;

pub fn list_height(window_height: f32, selected: bool) -> f32 {
    let pane_height = (window_height - crate::app::TOOLBAR_H - crate::app::FORMULA_H - crate::app::STATUS_H).max(200.0);
    let available = pane_height - 116.0;
    if selected { (available * 0.55).max(REVIEW_ROW_H) } else { available }
}

/// Wrap selected details to the width of the scrollable detail pane.
pub fn detail_lines(change: &Change, mode: &ReviewMode, ours: &Workbook, theirs: &Workbook, style: &pergola::TextStyle) -> Vec<String> {
    let text = change.details(ours, theirs, matches!(mode, ReviewMode::Tracked { .. }));
    pergola::text::lines(&text, style, REVIEW_W - 34.0)
}

/// A press in the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewCmd {
    /// Pick the change at this place in the list (and show it).
    Pick(usize),
    Toggle(Group),
    /// Take the picked change from the other copy.
    Take,
    /// Leave the picked change out of the list (this copy's stays).
    Keep,
    /// Take every change that can be taken.
    TakeAll,
    Close,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ReviewMode {
    #[default]
    Compare,
    Tracked { author: String },
    Merge { sheets: Vec<String> },
}

pub fn merge_error(error: MergeError) -> &'static str {
    match error {
        MergeError::SheetLayout => "The workbooks must contain the same named sheets.",
        MergeError::StructuralAlignment => "The row or column changes could not be aligned safely.",
        MergeError::TooManyChanges => "The review exceeds 100,000 cell changes.",
    }
}

pub fn writable(wb: &Workbook, addr: officina_grid::CellAddr) -> bool {
    !wb.merge_at(addr).is_some_and(|r| (r.r0, r.c0) != (addr.row, addr.col))
        && wb.spilled_from(addr).is_none()
}

/// The pane: the other copy's name, its changes and the one picked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewPane {
    pub open: bool,
    pub mode: ReviewMode,
    pub name: String,
    pub changes: Arc<Vec<Change>>,
    pub(crate) groups: Arc<BTreeMap<Group, Vec<usize>>>,
    pub(crate) expanded: BTreeSet<Group>,
    /// More changes than are listed.
    pub more: bool,
    pub picked: Option<usize>,
    /// Kept as this copy has them: not listed again.
    pub kept: Arc<Vec<Change>>,
}

/// A collapsible group of changes on one sheet, or at workbook scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Group(pub Option<u16>, pub &'static str);

impl Group {
    fn of(change: &Change, mode: &ReviewMode) -> Self {
        let label = match change {
            Change::Cell { .. } if matches!(mode, ReviewMode::Merge { .. }) => "Cell conflicts",
            Change::Cell { .. } => "Cell edits",
            Change::Style { .. } => "Styles",
            Change::Comment(_) => "Comments",
            Change::Link(_) => "Hyperlinks",
            Change::Chart(_) => "Charts",
            Change::Picture(_) => "Pictures",
            Change::Merge(_) => "Merged cells",
            Change::Setting(_) => "Sheet settings",
            Change::RowHeight(_) => "Row heights",
            Change::ColumnWidth(_) => "Column widths",
            Change::Name(_) => "Defined names",
            Change::Table(_) => "Tables",
            Change::Pivot(_) => "Pivot tables",
            Change::ConditionalFormats(_) => "Conditional formatting",
            Change::Validations(_) => "Data validation",
            Change::RowsAdded { .. } | Change::RowsRemoved { .. } => "Rows",
            Change::SheetAdded { .. } | Change::SheetRemoved { .. } => "Sheets",
            Change::StructuralConflict { .. } => "Structural conflicts",
        };
        Self(change.sheet(), label)
    }
}

pub struct GroupRow {
    pub start: usize,
    pub group: Group,
    pub count: usize,
    pub expanded: bool,
}

impl ReviewPane {
    /// The changes from `ours` to `theirs` now, less those kept.
    pub fn refresh(&mut self, ours: &Workbook, theirs: &Workbook) {
        let (all, more) = compare::compare(ours, theirs);
        let mut matched = vec![false; self.kept.len()];
        let changes = all.into_iter().filter(|c| {
            if let Some(i) = self.kept.iter().enumerate().position(|(i, k)| !matched[i] && c.same_review_item(k)) {
                matched[i] = true;
                false
            } else { true }
        }).collect();
        self.more = more;
        self.replace_changes(changes);
    }

    pub fn replace_changes(&mut self, changes: Vec<Change>) {
        let mut groups = BTreeMap::<_, Vec<_>>::new();
        for (index, change) in changes.iter().enumerate() {
            groups.entry(Group::of(change, &self.mode)).or_default().push(index);
        }
        self.expanded.retain(|group| groups.contains_key(group));
        self.groups = Arc::new(groups);
        self.changes = Arc::new(changes);
        self.picked = None;
    }

    pub fn resolve(&mut self, index: usize) {
        let mut changes = Arc::unwrap_or_clone(std::mem::take(&mut self.changes));
        changes.remove(index);
        self.replace_changes(changes);
    }

    pub fn toggle_group(&mut self, group: Group) {
        if !self.expanded.remove(&group) {
            if self.groups.contains_key(&group) { self.expanded.insert(group); }
        } else if self.picked.is_some_and(|i| Group::of(&self.changes[i], &self.mode) == group) {
            self.picked = None;
        }
    }

    pub fn pick(&mut self, index: usize) -> Option<&Change> {
        self.picked = self.changes.get(index).map(|_| index);
        let change = self.changes.get(index)?;
        self.expanded.insert(Group::of(change, &self.mode));
        Some(change)
    }

    pub fn group_rows(&self) -> (Vec<GroupRow>, usize) {
        let mut start = 0;
        let rows = self.groups.iter().map(|(&group, indices)| {
            let expanded = self.expanded.contains(&group);
            let row = GroupRow { start, group, count: indices.len(), expanded };
            start += 1 + if expanded { indices.len() } else { 0 };
            row
        }).collect();
        (rows, start)
    }

    pub fn group_items(&self, group: Group) -> &[usize] {
        &self.groups[&group]
    }

    /// What the list's head line says.
    pub fn summary(&self) -> String {
        let n = self.changes.len();
        match &self.mode {
            ReviewMode::Tracked { author } => return format!("{n} tracked cell edits · {author}"),
            ReviewMode::Merge { .. } => return format!("{n} conflicts with “{}”", self.name),
            ReviewMode::Compare => {}
        }
        let things = match n {
            0 => "No differences".to_string(),
            1 => "1 difference".to_string(),
            n => format!("{n}{} differences", if self.more { "+" } else { "" }),
        };
        format!("{things} from “{}”", self.name)
    }
}

/// Takes `ch` from `theirs` into the document as one undo step; false if it
/// can't be taken (a sheet added or removed).
pub fn take(d: &mut Doc, theirs: &Workbook, ch: &Change) -> bool {
    match compare::take(&d.wb, theirs, ch) {
        Some(ops) => {
            d.run(if ch.is_rows() { "Take Rows" } else { "Take Change" }, ops);
            true
        }
        None => false,
    }
}

/// Takes every change that can be taken as one undo step: a sheet with rows
/// added or removed whole, other sheets cell by cell. The number taken.
pub fn take_all(d: &mut Doc, theirs: &Workbook, changes: &[Change], kept: &[Change]) -> usize {
    let protected: std::collections::BTreeSet<_> = kept.iter().filter(|change| {
        matches!(change, Change::Cell { .. } | Change::Style { .. } | Change::Comment(_) | Change::Link(_) | Change::Merge(_) | Change::RowHeight(_) | Change::ColumnWidth(_)) || change.is_rows()
    }).filter_map(Change::sheet).collect();
    let whole: std::collections::BTreeSet<_> = changes.iter().filter(|c| c.is_rows()).filter_map(Change::sheet)
        .filter(|s| !protected.contains(s)).collect();
    let (objects, mut n) = compare::take_objects(&d.wb, theirs, changes);
    let pivot_areas = objects.iter().find_map(|op| match op {
        officina_grid::ops::GridOp::Pivots(state) => Some(state.iter().filter_map(|(_, area)| *area).collect::<Vec<_>>()),
        _ => None,
    });
    let mut ops = Vec::new();
    let mut annotations = Vec::new();
    let mut layout = Vec::new();
    let mut done = std::collections::BTreeMap::new();
    for ch in changes {
        if ch.is_object() { continue; }
        let Some(s) = ch.sheet() else { continue };
        // Sheet settings are independent of the row replacement below.
        if whole.contains(&s) && !matches!(ch, Change::Setting(_)) {
            if let std::collections::btree_map::Entry::Vacant(entry) = done.entry(s) {
                let op = compare::take_sheet(&d.wb, theirs, s);
                entry.insert(op.is_some());
                ops.extend(op);
            }
            if done[&s] {
                n += 1;
                continue;
            }
        }
        if let Change::Cell { sheet, row, col, .. } = ch {
            let cell = officina_grid::CellAddr::new(*sheet, *row, *col);
            if pivot_areas.as_ref().is_some_and(|areas| areas.iter().any(|area| area.contains(cell))) {
                continue;
            }
        }
        if matches!(ch, Change::Comment(_) | Change::Link(_)) {
            annotations.push(ch);
        } else if matches!(ch, Change::Merge(_) | Change::RowHeight(_) | Change::ColumnWidth(_)) {
            layout.push(ch);
        } else if !ch.is_rows() {
            if let Some(more) = compare::take(&d.wb, theirs, ch) {
                ops.extend(more);
                n += 1;
            }
        }
    }
    let (annotation_ops, taken) = compare::take_annotations(&d.wb, &annotations);
    n += taken;
    ops.extend(annotation_ops);
    let (layout_ops, taken) = compare::take_layout(&d.wb, &layout);
    n += taken;
    ops.extend(layout_ops);
    if pivot_areas.is_some() {
        ops.splice(..0, objects);
    } else {
        ops.extend(objects);
    }
    d.run("Take All Changes", ops);
    n
}
