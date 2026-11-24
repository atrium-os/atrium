//! SpreadsheetML describes groups by the depth of individual detail rows.
//! A collapsed flag belongs to their adjacent summary row.
use std::collections::BTreeMap;

use officina_grid::outline::{Group, Outline};
use officina_grid::shift::Axis;
use officina_grid::{book::Sheet, Workbook};

pub(super) const MAX_ROW_RECORDS: usize =
    (officina_grid::outline::MAX_SPAN + officina_grid::outline::MAX_GROUPS) as usize;

#[derive(Clone, Copy, Default)]
pub(super) struct Row {
    pub level: u8,
    pub summary: bool,
}

pub(super) fn decode(rows: &BTreeMap<u32, Row>, below: bool) -> Option<Outline> {
    let mut groups = Vec::new();
    let max = rows.values().map(|r| r.level).max().unwrap_or(0);
    if max > 7 {
        return None;
    }
    for level in 1..=max {
        let mut details = rows
            .iter()
            .filter(|(_, r)| r.level >= level)
            .map(|(&r, _)| r)
            .peekable();
        while let Some(start) = details.next() {
            let mut end = start;
            while details.next_if_eq(&(end + 1)).is_some() {
                end += 1;
            }
            let summary = if below {
                end.checked_add(1)
            } else {
                start.checked_sub(1)
            };
            let collapsed = summary
                .and_then(|at| rows.get(&at))
                .is_some_and(|r| r.summary);
            if groups.len() as u64 >= officina_grid::outline::MAX_GROUPS {
                return None;
            }
            groups.push(Group {
                start,
                end,
                collapsed,
            });
        }
    }
    Outline::new(groups, Axis::Rows)
}

pub(super) fn encode(outline: &Outline) -> BTreeMap<u32, Row> {
    let mut rows = BTreeMap::<u32, Row>::new();
    for group in outline.groups() {
        for at in group.start..=group.end {
            rows.entry(at).or_default().level += 1;
        }
        if group.collapsed {
            rows.entry(group.end + 1).or_default().summary = true;
        }
    }
    rows
}

fn can_write_sheet(sheet: &Sheet) -> bool {
    if !sheet.outline(Axis::Cols).groups().is_empty() {
        return false;
    }
    let outline = sheet.outline(Axis::Rows);
    if outline.groups().is_empty() {
        return true;
    }
    let rows = encode(outline);
    // Adjacent same-level groups can merge, and groups ending together
    // share a summary bit. Keep native Save As when Excel cannot encode them.
    if rows
        .last_key_value()
        .is_some_and(|(&r, _)| r >= super::write::XLSX_ROWS)
        || decode(&rows, true).as_ref() != Some(outline)
    {
        return false;
    }
    // The single hidden bit cannot carry two independent reasons to hide a row.
    !sheet.hidden_rows().iter().any(|&(a, b)| {
        let hidden = outline.hidden();
        hidden
            .get(hidden.partition_point(|&(_, end)| end < a))
            .is_some_and(|&(start, _)| start <= b)
    }) && !sheet
        .row_sizes
        .overrides()
        .iter()
        .any(|&(r, h)| h == 0.0 && outline.hides(r))
}

/// Whether SpreadsheetML can preserve every group and its independent hidden rows.
pub fn supports_outlines(wb: &Workbook) -> bool {
    (0..wb.sheet_count()).all(|s| can_write_sheet(wb.sheet(s as u16)))
}
