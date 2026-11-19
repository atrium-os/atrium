//! Compare with…: the changes that would turn this workbook into another
//! copy, listed in a pane beside the grid (officina.md §16.68). A change is
//! looked at in place, taken from the other copy, or kept as it is here.

use officina_grid::compare::{self, Change, MergeError};
use officina_grid::Workbook;

use crate::doc::Doc;

/// The pane's width, at the window's right over the grid.
pub const REVIEW_W: f32 = 360.0;
/// A change's row in the list.
pub const REVIEW_ROW_H: f32 = 30.0;

/// A press in the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewCmd {
    /// Pick the change at this place in the list (and show it).
    Pick(usize),
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
    pub changes: Vec<Change>,
    /// More changes than are listed.
    pub more: bool,
    pub picked: Option<usize>,
    /// Kept as this copy has them: not listed again.
    pub kept: Vec<Change>,
}

impl ReviewPane {
    /// The changes from `ours` to `theirs` now, less those kept.
    pub fn refresh(&mut self, ours: &Workbook, theirs: &Workbook) {
        let (all, more) = compare::compare(ours, theirs);
        self.changes = all.into_iter().filter(|c| !self.kept.contains(c)).collect();
        self.more = more;
        self.picked = self.picked.filter(|_| !self.changes.is_empty()).map(|i| i.min(self.changes.len() - 1));
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
pub fn take_all(d: &mut Doc, theirs: &Workbook, changes: &[Change]) -> usize {
    let whole: Vec<u16> = changes.iter().filter(|c| c.is_rows()).filter_map(Change::sheet).collect();
    let mut ops = Vec::new();
    let mut n = 0;
    let mut done: Vec<u16> = Vec::new();
    for ch in changes {
        let Some(s) = ch.sheet() else { continue };
        if whole.contains(&s) {
            if !done.contains(&s) {
                if let Some(op) = compare::take_sheet(&d.wb, theirs, s) {
                    ops.push(op);
                    done.push(s);
                }
            }
            n += 1;
        } else if let Some(more) = compare::take(&d.wb, theirs, ch) {
            ops.extend(more);
            n += 1;
        }
    }
    d.run("Take All Changes", ops);
    n
}
