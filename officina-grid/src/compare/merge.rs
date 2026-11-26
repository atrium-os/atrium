//! Once the workbook axes are aligned, compare each editable workbook part
//! against the common version. Ordered sheet rules are one item per rule kind.
use officina_core::history::Op;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    annotations, layout, parts, settings, tab_order, workbook_parts, CellSnapshot, Change,
    MergeError, MergePlan, MAX_CHANGES,
};
use crate::addr::{CellAddr, Rect, MAX_ROWS};
use crate::Workbook;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Comment(CellAddr),
    Link(CellAddr),
    Chart(String, u32, u32),
    Picture(String, u32, u32),
    Merge(CellAddr),
    RowHeight(CellAddr, bool),
    ColumnWidth(CellAddr, bool),
    Setting(u16, u8),
    Name(Option<String>, String),
    Table(String),
    Pivot(String),
    ConditionalFormats(u16),
    Validations(u16),
    TabOrder,
}

fn key(change: &Change, local: &Workbook, source: &Workbook) -> Key {
    match change {
        Change::Comment(c) => Key::Comment(c.at),
        Change::Link(c) => Key::Link(c.at),
        Change::Chart(c) => {
            let (s, r, c) = c.key(local, source);
            Key::Chart(s, r, c)
        }
        Change::Picture(c) => {
            let (s, r, c) = c.key(local, source);
            Key::Picture(s, r, c)
        }
        Change::Merge(c) => Key::Merge(c.at),
        Change::RowHeight(c) => Key::RowHeight(c.at, c.whole_axis),
        Change::ColumnWidth(c) => Key::ColumnWidth(c.at, c.whole_axis),
        Change::Setting(c) => Key::Setting(c.sheet, c.kind()),
        Change::Name(c) => Key::Name(c.scope.clone(), c.key.clone()),
        Change::Table(c) => Key::Table(c.key()),
        Change::Pivot(c) => Key::Pivot(c.key()),
        Change::ConditionalFormats(c) => Key::ConditionalFormats(c.sheet),
        Change::Validations(c) => Key::Validations(c.sheet),
        Change::TabOrder { .. } => Key::TabOrder,
        _ => unreachable!("workbook metadata"),
    }
}

fn same_incoming(change: &Change, old: &Change, source: &Workbook, base: &Workbook) -> bool {
    match (change, old) {
        (Change::Chart(a), Change::Chart(b)) => a.same_incoming(b, source, base),
        (Change::Picture(a), Change::Picture(b)) => a.same_incoming(b, source, base),
        (Change::Table(a), Change::Table(b)) => a.same_incoming(b, source, base),
        (Change::Pivot(a), Change::Pivot(b)) => a.same_incoming(b, source, base),
        (Change::ConditionalFormats(a), Change::ConditionalFormats(b)) => {
            a.same_incoming(b, source, base)
        }
        (Change::Validations(a), Change::Validations(b)) => a.same_incoming(b, source, base),
        _ => change == old,
    }
}

fn differences(local: &Workbook, other: &Workbook) -> Result<Vec<Change>, MergeError> {
    let mut changes = Vec::new();
    for sheet in 0..local.sheet_count() as u16 {
        let Some(next) = other.sheet_id(&local.sheet_name(sheet)) else {
            continue;
        };
        let pairs = [(0, 0, MAX_ROWS)];
        if settings::compare(local, sheet, other, next, &mut changes)
            || annotations::compare(local, sheet, other, next, &pairs, &mut changes)
            || layout::compare(local, sheet, other, next, &pairs, &mut changes)
        {
            return Err(MergeError::TooManyChanges);
        }
    }
    for change in workbook_parts(local, other).chain(tab_order(local, other)) {
        changes.push(change);
        if changes.len() >= MAX_CHANGES {
            return Err(MergeError::TooManyChanges);
        }
    }
    Ok(changes)
}

fn push(plan: &mut MergePlan, change: Change, conflict: bool) -> Result<(), MergeError> {
    if plan.incoming.len() + plan.conflicts.len() >= MAX_CHANGES {
        return Err(MergeError::TooManyChanges);
    }
    if conflict {
        plan.conflicts.push(change);
    } else {
        plan.incoming.push(change);
    }
    Ok(())
}

pub fn merge_parts(
    base: &Workbook,
    local: &Workbook,
    incoming: &Workbook,
    plan: &mut MergePlan,
) -> Result<(), MergeError> {
    let local_changes = differences(local, base)?;
    let mut by_key = BTreeMap::<_, Vec<_>>::new();
    for change in &local_changes {
        by_key
            .entry(key(change, local, base))
            .or_default()
            .push(change);
    }
    let mut blocked_keys = BTreeSet::new();
    let mut blocked_cells = BTreeSet::new();

    for sheet in 0..local.sheet_count() as u16 {
        let name = local.sheet_name(sheet);
        if incoming.sheet_id(&name).is_some() || base.sheet_id(&name).is_none() {
            continue;
        }
        let (cells, parts) = deletion_effects(local, sheet)?;
        let local_inputs = CellSnapshot::for_merge(local);
        let base_inputs = CellSnapshot::for_merge(base);
        let changed_dependant = cells.iter().any(|&(s, row, col)| {
            let name = local.sheet_name(s).to_ascii_lowercase();
            let local = local_inputs
                .sheets
                .get(&name)
                .and_then(|cells| cells.get(&(row, col)));
            let base = base_inputs
                .sheets
                .get(&name)
                .and_then(|cells| cells.get(&(row, col)));
            local != base
        }) || parts.iter().any(|key| by_key.contains_key(key));
        if changed_dependant || sheet_changed(local, base, &name)? || local.sheet_protected(sheet) {
            push(plan, Change::SheetRemoved { sheet }, true)?;
            blocked_cells.extend(cells);
            blocked_keys.extend(parts);
        } else {
            push(plan, Change::SheetRemoved { sheet }, false)?;
        }
    }
    for sheet in 0..incoming.sheet_count() as u16 {
        let name = incoming.sheet_name(sheet);
        match (local.sheet_id(&name), base.sheet_id(&name)) {
            (None, None) => push(plan, Change::SheetAdded { name }, false)?,
            (None, Some(_)) if sheet_changed(incoming, base, &name)? => {
                push(plan, Change::SheetAdded { name }, true)?
            }
            (Some(local_sheet), None) if sheet_changed(local, incoming, &name)? => push(
                plan,
                Change::SheetConflict {
                    name,
                    sheet: Some(local_sheet),
                    versions: Box::new([
                        "Locally added worksheet".into(),
                        "Different incoming worksheet with the same name".into(),
                    ]),
                },
                true,
            )?,
            _ => {}
        }
    }

    let mut independent = Vec::new();
    for change in plan.incoming.drain(..) {
        if change.at().is_some_and(|addr| {
            matches!(change, Change::Cell { .. }) && blocked_cells.contains(&addr)
        }) {
            plan.conflicts.push(change);
        } else {
            independent.push(change);
        }
    }
    plan.incoming = independent;
    for change in differences(local, incoming)? {
        // Worksheet actions own metadata placed on a tab that is going away.
        // An addition copies its placed objects as part of the same operation.
        if change
            .sheet()
            .is_some_and(|s| incoming.sheet_id(&local.sheet_name(s)).is_none())
            || super::sheets::on_new_sheet(&change, local, incoming)
        {
            continue;
        }
        let key = key(&change, local, incoming);
        let previous = by_key.get_mut(&key);
        let conflict = if let Some(previous) = previous {
            if let Some(index) = previous
                .iter()
                .position(|old| same_incoming(&change, old, incoming, base))
            {
                previous.swap_remove(index);
                continue;
            }
            !previous.is_empty()
        } else {
            false
        };
        push(plan, change, conflict || blocked_keys.contains(&key))?;
    }
    for sheet in 0..local.sheet_count() as u16 {
        let name = local.sheet_name(sheet);
        let (Some(origin), Some(source)) = (base.sheet_id(&name), incoming.sheet_id(&name)) else {
            continue;
        };
        merge_styles(
            [(base, origin), (local, sheet), (incoming, source)],
            sheet,
            plan,
        )?;
    }
    Ok(())
}

fn merge_styles(
    copies: [(&Workbook, u16); 3],
    sheet: u16,
    plan: &mut MergePlan,
) -> Result<(), MergeError> {
    let columns = copies.map(|(book, id)| &book.sheet(id).styles);
    let count = columns.iter().map(|cols| cols.len()).max().unwrap_or(0);
    for col in 0..count {
        let runs = columns.map(|cols| {
            cols.get(col)
                .map_or_else(Vec::new, |col| col.styled(MAX_ROWS - 1))
        });
        let mut indices = [0; 3];
        let mut row = 0;
        while row < MAX_ROWS {
            let segments =
                std::array::from_fn::<_, 3, _>(|i| parts::segment(&runs[i], &mut indices[i], row));
            let next = segments.iter().map(|segment| segment.1).min().unwrap();
            let [before, ours, theirs] =
                std::array::from_fn(|i| copies[i].0.styles.get(segments[i].0));
            if theirs != before && ours != theirs {
                let rect = Rect {
                    sheet,
                    r0: row,
                    r1: next - 1,
                    c0: col as u32,
                    c1: col as u32,
                };
                push(
                    plan,
                    Change::Style {
                        rect,
                        ours: Box::new(ours.clone()),
                        theirs: Box::new(theirs.clone()),
                    },
                    ours != before,
                )?;
            }
            row = next;
        }
    }
    Ok(())
}

/// A structural deletion must also protect edits to sheet metadata in its
/// footprint. Workbook objects whose full footprint is handled by the shift
/// operation use the same comparison as the subsequent parts merge.
pub(crate) fn parts_changed_in_band(
    base: &Workbook,
    branch: &Workbook,
    name: &str,
    axis: crate::shift::Axis,
    at: u32,
    count: u32,
) -> Result<bool, MergeError> {
    use crate::shift::Axis;
    let Some(sheet) = base.sheet_id(name) else {
        return Ok(false);
    };
    let band = at..at + count;
    let intersects = |rect: Rect| {
        let (lo, hi) = match axis {
            Axis::Rows => (rect.r0, rect.r1),
            Axis::Cols => (rect.c0, rect.c1),
        };
        rect.sheet == sheet && lo < band.end && band.start <= hi
    };
    let changes = differences(base, branch)?;
    if changes.iter().any(|change| {
        let hit = |s, row, col| {
            s == sheet
                && band.contains(&match axis {
                    Axis::Rows => row,
                    Axis::Cols => col,
                })
        };
        match change {
            Change::Merge(change) => change.intersects(intersects),
            Change::RowHeight(c) => {
                axis == Axis::Rows && !c.whole_axis && hit(c.at.sheet, c.at.row, c.at.col)
            }
            Change::ColumnWidth(c) => {
                axis == Axis::Cols && !c.whole_axis && hit(c.at.sheet, c.at.row, c.at.col)
            }
            Change::Name(_) | Change::TabOrder { .. } | Change::Setting(_) => false,
            _ => change.at().is_some_and(|(s, r, c)| hit(s, r, c)),
        }
    }) {
        return Ok(true);
    }

    let mut styles = Vec::new();
    let other = branch.sheet_id(name).expect("aligned sheets");
    if parts::styles(base, sheet, branch, other, &[(0, 0, MAX_ROWS)], &mut styles) {
        return Err(MergeError::TooManyChanges);
    }
    Ok(styles
        .into_iter()
        .any(|change| matches!(change, Change::Style { rect, .. } if intersects(rect))))
}

fn sheet_changed(left: &Workbook, right: &Workbook, name: &str) -> Result<bool, MergeError> {
    let (Some(l), Some(r)) = (left.sheet_id(name), right.sheet_id(name)) else {
        return Ok(true);
    };
    let name = name.to_ascii_lowercase();
    let a = CellSnapshot::for_merge(left);
    let b = CellSnapshot::for_merge(right);
    if a.sheets[&name] != b.sheets[&name]
        || a.axes[&name] != b.axes[&name]
        || left.sheet(l).protection != right.sheet(r).protection
    {
        return Ok(true);
    }
    let mut styles = Vec::new();
    parts::styles(left, l, right, r, &[(0, 0, MAX_ROWS)], &mut styles);
    Ok(!styles.is_empty()
        || differences(left, right)?
            .iter()
            .any(|c| c.sheet() == Some(l)))
}

/// Find the edits to other sheets that deletion would perform as a side
/// effect, so a locally changed reference can block automatic deletion.
fn deletion_effects(
    wb: &Workbook,
    sheet: u16,
) -> Result<(BTreeSet<(u16, u32, u32)>, BTreeSet<Key>), MergeError> {
    let Some(op) = crate::sheetops::delete_sheet_op(wb, sheet) else {
        return Ok(Default::default());
    };
    let mut without = crate::structural_merge::copy(wb)?;
    op.apply(&mut without);
    let a = CellSnapshot::for_merge(wb);
    let b = CellSnapshot::for_merge(&without);
    let cells = a
        .sheets
        .iter()
        .filter_map(|(name, before)| {
            let s = wb.sheet_id(name)?;
            let after = b.sheets.get(name)?;
            Some(
                before
                    .iter()
                    .filter(move |(addr, value)| after.get(addr) != Some(value))
                    .map(move |(&(r, c), _)| (s, r, c)),
            )
        })
        .flatten()
        .collect();
    let parts = differences(wb, &without)?
        .into_iter()
        .filter(|change| change.sheet() != Some(sheet))
        .map(|change| key(&change, wb, &without))
        .collect();
    Ok((cells, parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, Picture, Series};
    use crate::links::Link;

    #[test]
    fn workbook_parts_follow_their_three_way_base_across_tab_orders() {
        let flavor = |version: u16| {
            let mut wb = Workbook::new();
            wb.add_sheet("Summary").unwrap();
            wb.add_sheet("Data").unwrap();
            wb.define_name("Rate", None, &format!("={version}"))
                .unwrap();
            let at = |row| CellAddr::new(0, row, 0);
            let anchor = |row| Anchor {
                sheet: 0,
                from: (row, 0, 0.0, 0.0),
                to: (row + 3, 3, 0.0, 0.0),
            };
            wb.add_chart(Chart {
                kind: ChartKind::Line,
                grouping: Grouping::Standard,
                title: Some(ChartText::Lit(
                    if version == 2 {
                        "Local chart"
                    } else {
                        "Original chart"
                    }
                    .into(),
                )),
                legend: None,
                x_title: None,
                y_title: None,
                series: vec![Series {
                    name: None,
                    cats: None,
                    vals: Rect::cell(CellAddr::new(1, 0, 0)),
                    kind: None,
                }],
                anchor: anchor(1), alt: None
            })
            .unwrap();
            wb.add_picture(Picture {
                anchor: anchor(10),
                bytes: [
                    b"\x89PNG\r\n\x1a\n".as_slice(),
                    &[if version == 3 { 3 } else { 1 }],
                ]
                .concat()
                .into(),
                link: None,
                crop: [0; 4],
                rotation: 0, alt: None
            })
            .unwrap();
            wb.set_comments(
                0,
                vec![(
                    at(20).row,
                    0,
                    Some(crate::comments::Comment::new(
                        "Ada",
                        if version == 3 {
                            "Incoming note"
                        } else {
                            "Base note"
                        },
                    )),
                )],
            );
            wb.set_links(
                0,
                vec![(
                    at(21).row,
                    0,
                    Link::new(if version == 2 { "#Data!B2" } else { "#Data!A1" }),
                )],
            );
            if version != 2 {
                assert!(wb.move_sheet(1, 0));
            }
            wb
        };
        let base = flavor(1);
        let mut local = flavor(2);
        let source = flavor(3);
        let mut plan = MergePlan {
            incoming: Vec::new(),
            conflicts: Vec::new(),
        };
        merge_parts(&base, &local, &source, &mut plan).unwrap();
        assert_eq!(plan.incoming.len(), 2);
        assert_eq!(plan.conflicts.len(), 1);
        assert!(matches!(plan.conflicts[0], Change::Name(_)));
        for change in plan.incoming.iter().chain(&plan.conflicts) {
            for op in super::super::take(&local, &source, change).unwrap() {
                op.apply(&mut local);
            }
        }
        assert_eq!(local.name_text(local.names().get(0, "Rate").unwrap()), "=3");
        assert_eq!(
            local.charts()[0].title,
            Some(ChartText::Lit("Local chart".into()))
        );
        assert_eq!(local.charts()[0].series[0].vals.sheet, 1);
        assert_eq!(local.pictures()[0].bytes.last(), Some(&3));
        assert_eq!(
            local.comment(0, 20, 0).unwrap().text.as_ref(),
            "Incoming note"
        );
        assert_eq!(local.link(0, 21, 0).unwrap().target.as_ref(), "#Data!B2");
    }
}
