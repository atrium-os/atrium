//! Compare with…: the changes that would turn this workbook into another
//! copy, listed in a pane beside the grid (officina.md §16.68). A change is
//! looked at in place, taken from the other copy, or kept as it is here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use officina_grid::compare::{self, Change, MergeError};
use officina_grid::{ops::GridOp, CellAddr, Workbook};

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
    Merge { sheets: Vec<String>, revision: u64 },
}

pub fn merge_error(error: MergeError) -> &'static str {
    match error {
        MergeError::SheetLayout => "The workbooks must contain the same named sheets.",
        MergeError::StructuralAlignment => "The row or column changes could not be aligned safely.",
        MergeError::TooManyChanges => "The review exceeds 100,000 changes.",
        MergeError::CannotApply => "The merge could not be applied within the workbook’s editing limits.",
    }
}

pub fn writable(wb: &Workbook, addr: officina_grid::CellAddr) -> bool {
    wb.can_edit_cells(officina_grid::Rect::cell(addr))
        && !wb.merge_at(addr).is_some_and(|r| (r.r0, r.c0) != (addr.row, addr.col))
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
            Change::TabOrder { .. } => "Tab order",
            Change::Setting(_) => "Sheet settings",
            Change::RowHeight(_) => "Row heights",
            Change::ColumnWidth(_) => "Column widths",
            Change::Name(_) => "Defined names",
            Change::Table(_) => "Tables",
            Change::Pivot(_) => "Pivot tables",
            Change::ConditionalFormats(_) => "Conditional formatting",
            Change::Validations(_) => "Data validation",
            Change::RowsAdded { .. } | Change::RowsRemoved { .. } => "Rows",
            Change::SheetAdded { .. } | Change::SheetRemoved { .. } | Change::SheetRenamed { .. } | Change::SheetConflict { .. } => "Sheets",
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
        self.refresh_after(ours, theirs, None);
    }

    /// Sheet indices in a kept difference belong to the old tab order.
    pub fn refresh_after(&mut self, ours: &Workbook, theirs: &Workbook, old_order: Option<&[String]>) {
        let (all, more) = compare::compare(ours, theirs);
        if let Some(names) = old_order {
            if !self.kept.is_empty() {
                let mut old = Workbook::new();
                for name in names { old.add_sheet(name).expect("existing sheet name"); }
                Arc::make_mut(&mut self.kept).retain_mut(|ch| ch.reorder_local(&old, ours).is_some());
            }
            self.expanded.clear();
        }
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

/// Merge against an aligned preview so the operation for an object can see
/// earlier edits, without making those edits visible before the undo step.
pub fn merge(d: &mut Doc, base_book: &Workbook, incoming: Workbook)
    -> Result<(Workbook, Vec<Change>, usize, bool), MergeError>
{
    use officina_core::history::Op;
    use officina_grid::structural_merge::{align, Alignment};

    let renames = compare::plan_renames(base_book, &d.wb, &incoming);
    let renamed_local = if renames.ops.is_empty() { None } else {
        let mut local = copy_for_review(&d.wb)?;
        for op in &renames.ops { op.clone().apply(&mut local); }
        Some(Box::new(local))
    };
    let working = renamed_local.as_deref().unwrap_or(&d.wb);
    let renamed_base = compare::named_for_review(working, base_book)?;
    let base_book = renamed_base.as_ref().unwrap_or(base_book);
    let incoming = compare::named_for_review(working, &incoming)?.unwrap_or(incoming);
    let (mut ops, mut preview, base, other, mut plan) = match align(base_book, working, &incoming)? {
        Alignment::Blocked(mut conflicts) => {
            conflicts.extend(renames.conflicts);
            return Ok((incoming, conflicts, 0, false));
        }
        Alignment::Positional(plan) => (Vec::new(), renamed_local, None, incoming, plan),
        Alignment::Aligned { ops, local, base, incoming, cells } =>
            (ops, Some(local), Some(base), *incoming, cells),
    };
    let structural = !ops.is_empty();
    let mut changes = renames.ops;
    let mut count = changes.len();
    changes.append(&mut ops);
    let mut ops = changes;
    let working = preview.as_deref().unwrap_or(&d.wb);
    compare::merge_parts(base.as_deref().unwrap_or(base_book), working, &other, &mut plan)?;

    let mut metadata = Vec::new();
    let mut cells = Vec::new();
    let mut conflicts = plan.conflicts;
    conflicts.extend(renames.conflicts);
    for change in plan.incoming {
        if matches!(change, Change::Cell { .. }) { cells.push(change); } else { metadata.push(change); }
    }
    if !metadata.is_empty() && preview.is_none() {
        preview = Some(Box::new(copy_for_review(&d.wb)?));
    }
    let mut removals = Vec::new();
    let mut additions = Vec::new();
    let mut tab_order = None;
    metadata.retain(|change| match change {
        Change::SheetRemoved { .. } => { removals.push(change.clone()); false }
        Change::SheetAdded { .. } => { additions.push(change.clone()); false }
        Change::TabOrder { .. } => { tab_order = Some(change.clone()); false }
        _ => true,
    });
    if let Some(wb) = &mut preview {
        let names: Vec<&str> = additions.iter().filter_map(|change| match change {
            Change::SheetAdded { name } => Some(name.as_str()),
            _ => None,
        }).collect();
        if !names.is_empty() {
            if let Some(edits) = compare::take_added_sheets(wb, &other, &names)
                .filter(|ops| crate::sheet_protection::allows(wb, ops))
            {
                apply_preview(wb, &mut ops, edits);
                count += names.len();
            } else {
                conflicts.extend(additions);
            }
        }
    }
    let mut applied = Vec::new();
    for change in cells {
        let working = preview.as_deref().unwrap_or(&d.wb);
        let Some(op) = merge_cell(working, &change) else { conflicts.push(change); continue };
        if let Some(wb) = &mut preview { op.clone().apply(wb); }
        ops.push(op);
        applied.push(change);
    }
    if let Some(wb) = &mut preview {
        // Pivot output is computed from the merged source values.
        if metadata.iter().any(|ch| matches!(ch, Change::Pivot(_))) { wb.recalc(); }
        for change in metadata {
            match compare::take(wb, &other, &change) {
                Some(edits) if crate::sheet_protection::allows(wb, &edits) => {
                    apply_preview(wb, &mut ops, edits);
                    count += 1;
                }
                _ => conflicts.push(change),
            }
        }
    }
    let before_removal = preview.as_ref().map(|wb| (0..wb.sheet_count() as u16)
        .map(|s| wb.sheet_name(s)).collect::<Vec<_>>());
    if let (Some(wb), Some(names)) = (&mut preview, &before_removal) {
        for change in removals {
            let Change::SheetRemoved { sheet } = change else { unreachable!() };
            let Some(sheet) = wb.sheet_id(&names[sheet as usize]) else { continue };
            let current = Change::SheetRemoved { sheet };
            if let Some(edits) = compare::take(wb, &other, &current).filter(|ops| crate::sheet_protection::allows(wb, ops)) {
                apply_preview(wb, &mut ops, edits);
                count += 1;
            } else {
                // Keep the original address until every removal has run.
                conflicts.push(change);
            }
        }
    }
    if let (Some(wb), Some(change)) = (&mut preview, tab_order) {
        let change = with_current_order(wb, change);
        if let Some(edits) = compare::take(wb, &other, &change).filter(|ops| crate::sheet_protection::allows(wb, ops)) {
            apply_preview(wb, &mut ops, edits);
            count += 1;
        } else {
            conflicts.push(change);
        }
    }
    if !d.run_action("Merge Independent Changes", ops) {
        return Err(MergeError::CannotApply);
    }
    for change in applied {
        let Change::Cell { sheet, row, col, ref theirs, .. } = change else { unreachable!() };
        let target = before_removal.as_ref().map_or(Some(sheet), |names| d.wb.sheet_id(&names[sheet as usize]));
        if target.is_some_and(|s| d.wb.input(CellAddr::new(s, row, col)) == *theirs) {
            count += 1;
        } else {
            conflicts.push(change);
        }
    }
    if let Some(names) = before_removal {
        if (0..d.wb.sheet_count() as u16).map(|s| d.wb.sheet_name(s)).ne(names.iter().cloned()) {
            let mut old = Workbook::new();
            for name in names { old.add_sheet(&name).expect("existing sheet name"); }
            conflicts.retain_mut(|change| change.reorder_local(&old, &d.wb).is_some());
        }
    }
    Ok((other, conflicts, count, structural))
}

fn apply_preview(wb: &mut Workbook, pending: &mut Vec<GridOp>, ops: Vec<GridOp>) {
    use officina_core::history::Op;
    for op in ops { op.clone().apply(wb); pending.push(op); }
}

fn with_current_order(wb: &Workbook, change: Change) -> Change {
    let Change::TabOrder { after, .. } = change else { return change };
    let current: Vec<_> = (0..wb.sheet_count() as u16).map(|s| wb.sheet_name(s)).collect();
    let mut order = after.iter().filter(|name| wb.sheet_id(name).is_some());
    let after = current.iter().map(|name| {
        if after.contains(name) { order.next().expect("shared name").clone() } else { name.clone() }
    }).collect();
    Change::TabOrder { before: current, after }
}

fn copy_for_review(wb: &Workbook) -> Result<Workbook, MergeError> {
    let bytes = officina_grid::native::save(wb, Vec::new()).map_err(|_| MergeError::StructuralAlignment)?;
    officina_grid::native::load(&bytes).map_err(|_| MergeError::StructuralAlignment)
}

fn merge_cell(wb: &Workbook, change: &Change) -> Option<GridOp> {
    let Change::Cell { sheet, row, col, ours, theirs } = change else { return None };
    let addr = CellAddr::new(*sheet, *row, *col);
    let parses = match theirs {
        officina_grid::book::Input::Formula(text) => wb.parse(text, *sheet, *row, *col).is_ok(),
        _ => true,
    };
    (parses && writable(wb, addr) && wb.pivot_at(addr).is_none() && wb.input(addr) == *ours)
        .then(|| GridOp::set(addr, theirs.clone()))
}

pub fn take_conflict(d: &mut Doc, other: &Workbook, change: &Change) -> bool {
    let ops = match change {
        Change::Cell { .. } => merge_cell(&d.wb, change).map(|op| vec![op]),
        Change::TabOrder { .. } => compare::take(&d.wb, other, &with_current_order(&d.wb, change.clone())),
        _ => {
            let Ok(named) = compare::named_for_review(&d.wb, other) else { return false; };
            compare::take(&d.wb, named.as_ref().unwrap_or(other), change)
        }
    };
    if !ops.is_some_and(|ops| d.run_action("Resolve Merge Conflict", ops)) { return false; }
    if let Change::Cell { sheet, row, col, theirs, .. } = change {
        return d.wb.input(CellAddr::new(*sheet, *row, *col)) == *theirs;
    }
    true
}

/// Takes `ch` from `theirs` into the document as one undo step; false if it
/// can't be taken (a sheet added or removed).
pub fn take(d: &mut Doc, theirs: &Workbook, ch: &Change) -> bool {
    let Ok(named) = compare::named_for_review(&d.wb, theirs) else { return false };
    match compare::take(&d.wb, named.as_ref().unwrap_or(theirs), ch) {
        Some(ops) => d.run_action(if ch.is_rows() { "Take Rows" } else { "Take Change" }, ops),
        None => false,
    }
}

/// Takes every change that can be taken as one undo step: a sheet with rows
/// added or removed whole, other sheets cell by cell. The number taken.
pub fn take_all(d: &mut Doc, theirs: &Workbook, changes: &[Change], kept: &[Change]) -> usize {
    let needs_preview = changes.iter().any(|ch| matches!(ch,
        Change::SheetAdded { .. } | Change::SheetRemoved { .. } | Change::SheetRenamed { .. }));
    let Ok(named) = compare::named_for_review(&d.wb, theirs) else { return 0 };
    let source = named.as_ref().unwrap_or(theirs);
    if !needs_preview {
        let (ops, n) = take_all_parts(&d.wb, source, changes, kept, true);
        return if d.run_action("Take All Changes", ops) { n } else { 0 };
    }
    let Ok(mut preview) = copy_for_review(&d.wb) else { return 0 };
    let names: Vec<_> = changes.iter().filter_map(|ch| match ch {
        Change::SheetAdded { name } => Some(name.as_str()),
        _ => None,
    }).collect();
    let mut ops = Vec::new();
    let mut n = 0;
    if !names.is_empty() {
        if let Some(edits) = compare::take_added_sheets(&preview, source, &names)
            .filter(|ops| crate::sheet_protection::allows(&preview, ops))
        {
            apply_preview(&mut preview, &mut ops, edits);
            n += names.len();
        }
    }
    let (parts, taken) = take_all_parts(&preview, source, changes, kept, false);
    apply_preview(&mut preview, &mut ops, parts);
    n += taken;
    for ch in changes {
        if matches!(ch, Change::SheetRenamed { .. }) {
            if let Some(edits) = compare::take(&preview, source, ch)
                .filter(|ops| crate::sheet_protection::allows(&preview, ops))
            {
                apply_preview(&mut preview, &mut ops, edits);
                n += 1;
            }
        }
    }
    for ch in changes {
        if let Change::SheetRemoved { sheet } = ch {
            let Some(sheet) = preview.sheet_id(&d.wb.sheet_name(*sheet)) else { continue };
            if let Some(edits) = compare::take(&preview, source, &Change::SheetRemoved { sheet })
                .filter(|ops| crate::sheet_protection::allows(&preview, ops))
            {
                apply_preview(&mut preview, &mut ops, edits);
                n += 1;
            }
        }
    }
    if !kept.iter().any(|ch| matches!(ch, Change::TabOrder { .. })) {
        if let Some(order) = compare::tab_order(&preview, theirs) {
            if let Some(edits) = compare::take(&preview, theirs, &order)
                .filter(|ops| crate::sheet_protection::allows(&preview, ops))
            {
                ops.extend(edits);
                n += 1;
            }
        }
    }
    if d.run_action("Take All Changes", ops) { n } else { 0 }
}

fn take_all_parts(wb: &Workbook, theirs: &Workbook, changes: &[Change], kept: &[Change], order: bool)
    -> (Vec<GridOp>, usize)
{
    let protected: std::collections::BTreeSet<_> = kept.iter().filter(|change| {
        matches!(change, Change::Cell { .. } | Change::Style { .. } | Change::Comment(_) | Change::Link(_) | Change::Merge(_) | Change::RowHeight(_) | Change::ColumnWidth(_)) || change.is_rows()
    }).filter_map(Change::sheet).collect();
    let whole: std::collections::BTreeSet<_> = changes.iter().filter(|c| c.is_rows()).filter_map(Change::sheet)
        .filter(|s| !protected.contains(s)).collect();
    let (objects, mut n) = compare::take_objects(wb, theirs, changes);
    let pivot_areas = objects.iter().find_map(|op| match op {
        officina_grid::ops::GridOp::Pivots(state) => Some(state.iter().filter_map(|(_, area)| *area).collect::<Vec<_>>()),
        _ => None,
    });
    let mut ops = Vec::new();
    let mut annotations = Vec::new();
    let mut layout = Vec::new();
    let mut done = std::collections::BTreeMap::new();
    for ch in changes {
        if ch.is_object() || matches!(ch, Change::SheetAdded { .. } | Change::SheetRemoved { .. } | Change::SheetRenamed { .. }) { continue; }
        let Some(s) = ch.sheet() else { continue };
        // Sheet settings are independent of the row replacement below.
        if whole.contains(&s) && !matches!(ch, Change::Setting(_)) {
            if let std::collections::btree_map::Entry::Vacant(entry) = done.entry(s) {
                let op = compare::take_sheet(wb, theirs, s);
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
            if let Some(more) = compare::take(wb, theirs, ch) {
                ops.extend(more);
                n += 1;
            }
        }
    }
    let (annotation_ops, taken) = compare::take_annotations(wb, &annotations);
    n += taken;
    ops.extend(annotation_ops);
    let (layout_ops, taken) = compare::take_layout(wb, &layout);
    n += taken;
    ops.extend(layout_ops);
    if pivot_areas.is_some() {
        ops.splice(..0, objects);
    } else {
        ops.extend(objects);
    }
    // All preceding operations address the original sheet indices.
    if let Some(order) = changes.iter().find(|ch| order && matches!(ch, Change::TabOrder { .. })) {
        if let Some(moves) = compare::take(wb, theirs, order) {
            ops.extend(moves);
            n += 1;
        }
    }
    (ops, n)
}
