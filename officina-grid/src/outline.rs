//! Worksheet outline groups. Collapse is independent of manual hiding and filters.
use officina_core::wire::{Reader, WireError, Writer};

use crate::shift::{Axis, Shift};

pub const MAX_GROUPS: u64 = 1024;
pub const MAX_SPAN: u64 = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    pub start: u32,
    pub end: u32,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outline {
    last: Option<u32>,
    groups: Vec<Group>,
    hidden: Vec<(u32, u32)>,
}

impl Outline {
    pub fn last(&self) -> Option<u32> { self.last }

    pub fn groups(&self) -> &[Group] { &self.groups }
    pub fn hidden(&self) -> &[(u32, u32)] { &self.hidden }

    pub fn hides(&self, at: u32) -> bool {
        self.hidden.get(self.hidden.partition_point(|g| g.1 < at)).is_some_and(|g| g.0 <= at)
    }

    pub fn new(mut groups: Vec<Group>, axis: Axis) -> Option<Self> {
        if groups.len() as u64 > MAX_GROUPS { return None; }
        let limit = match axis { Axis::Rows => crate::addr::MAX_ROWS, Axis::Cols => crate::addr::MAX_COLS };
        groups.sort_unstable_by_key(|g| (g.start, std::cmp::Reverse(g.end)));
        let mut nesting = Vec::new();
        let mut span = 0;
        let mut last = None::<u32>;
        for g in &groups {
            if g.start > g.end || g.end >= limit { return None; }
            span += u64::from(g.end - g.start + 1);
            last = Some(last.map_or(g.end, |end| end.max(g.end)));
            while nesting.last().is_some_and(|p: &Group| p.end < g.start) { nesting.pop(); }
            if nesting.len() >= 8 || nesting.last().is_some_and(|p| p.end < g.end || (p.start, p.end) == (g.start, g.end)) {
                return None;
            }
            nesting.push(*g);
        }
        if span > MAX_SPAN { return None; }
        let mut hidden: Vec<(u32, u32)> = Vec::new();
        for g in groups.iter().filter(|g| g.collapsed) {
            if let Some(last) = hidden.last_mut().filter(|p| g.start <= p.1 + 1) {
                last.1 = last.1.max(g.end);
            } else {
                hidden.push((g.start, g.end));
            }
        }
        Some(Self { last, groups, hidden })
    }

    pub(crate) fn shifted(&self, shift: Shift) -> Option<Self> {
        let mut groups = Vec::<Group>::new();
        for g in &self.groups {
            let Some((start, end)) = shift.map_span(g.start, g.end) else { continue };
            if let Some(same) = groups.iter_mut().find(|p| (p.start, p.end) == (start, end)) {
                same.collapsed |= g.collapsed;
            } else {
                groups.push(Group { start, end, ..*g });
            }
        }
        Self::new(groups, shift.axis)
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        w.var(self.groups.len() as u64);
        for g in &self.groups {
            w.u32(g.start);
            w.u32(g.end);
            w.u8(u8::from(g.collapsed));
        }
    }

    pub(crate) fn decode(r: &mut Reader, axis: Axis) -> Result<Self, WireError> {
        let n = r.count("outline groups", MAX_GROUPS, 9)?;
        let mut groups = Vec::with_capacity(n);
        for _ in 0..n {
            let start = r.u32()?;
            let end = r.u32()?;
            let collapsed = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("outline collapse state")),
            };
            groups.push(Group { start, end, collapsed });
        }
        Self::new(groups, axis).ok_or(WireError::Invalid("worksheet outline"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ops::GridOp, shift::shift_op, Workbook};
    use officina_core::history::History;

    #[test]
    fn nested_outlines_round_trip_and_follow_structural_edits() {
        let mut wb = Workbook::new();
        wb.add_sheet("Outline").unwrap();
        wb.sheet_mut(0).col_sizes.set(4, 0.0);
        let outer = Group { start: 2, end: 10, collapsed: true };
        let inner = Group { start: 3, end: 7, collapsed: true };
        assert!(Outline::new(vec![outer, Group { start: 5, end: 12, ..inner }], Axis::Cols).is_none());
        let outline = Outline::new(vec![inner, outer], Axis::Cols).unwrap();
        assert_eq!(outline.hidden(), &[(2, 10)]);

        let op = GridOp::Outline { sheet: 0, axis: Axis::Cols, outline: outline.clone() };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let mut history = History::new();
        history.run(&mut wb, "Group", vec![op]);
        let shift = shift_op(&wb, 0, Axis::Cols, 5, 2).unwrap();
        history.run(&mut wb, "Insert", vec![shift]);
        let got = wb.sheet(0).outline(Axis::Cols);
        assert_eq!(got.groups(), &[Group { end: 12, ..outer }, Group { end: 9, ..inner }]);
        let reopened = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(reopened.sheet(0).outline(Axis::Cols), got);
        assert_eq!(reopened.sheet(0).view_col_sizes().size(7), 0.0);
        history.undo(&mut wb);
        assert_eq!(wb.sheet(0).outline(Axis::Cols), &outline);
        history.undo(&mut wb);
        assert!(!wb.has_outlines());
        assert_eq!(wb.sheet(0).view_col_sizes().size(4), 0.0);
        assert_eq!(wb.sheet(0).view_col_sizes().size(5), 64.0);
    }
}
