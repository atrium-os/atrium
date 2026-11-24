//! Run a bounded subset of the same dirty dependency graph before its normal pass.

use super::{GroupId, RecalcStats, RowSet, Workbook};
use crate::addr::Rect;
use crate::book::Group;
use crate::eval::arrays::can_array;
use crate::formula::{Expr, Func};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// A viewport read of a whole-column aggregate must not monopolize priority.
const MAX_PRIORITY_ROWS: u64 = 4096;

fn interest(group: &Group, dirty: &RowSet, view: &[Rect]) -> RowSet {
    let mut rows = RowSet::default();
    for r in view.iter().filter(|r| r.sheet == group.sheet && r.r0 <= r.r1 && r.c0 <= group.col && group.col <= r.c1) {
        for (start, end) in dirty.within(r.r0, r.r1) {
            rows.add(start, end);
        }
    }
    rows
}

/// A union of source rows read by a consumer run may conservatively include
/// extra rows for a mixed absolute/relative reference.
fn reads<'a>(consumer: &'a Group, source: &'a Group, rows: &'a RowSet) -> impl Iterator<Item = Rect> + 'a {
    consumer.fps.iter().filter(move |fp| fp.sheet == source.sheet && fp.c0 <= source.col && source.col <= fp.c1)
        .flat_map(move |fp| rows.0.iter().filter_map(move |&(a, b)| fp.rect(a, b)))
}

fn wants(consumer: &Group, source: &Group, rows: &RowSet, dirty: &RowSet) -> Option<RowSet> {
    let mut needed = RowSet::default();
    for bound in reads(consumer, source, rows) {
        for (a, b) in dirty.within(bound.r0, bound.r1) {
            if u64::from(b - a) >= MAX_PRIORITY_ROWS { return None; }
            needed.add(a, b);
            if needed.len() > MAX_PRIORITY_ROWS { return None; }
        }
    }
    Some(needed)
}

impl Workbook {
    pub(super) fn recalc_visible(
        &mut self,
        dirty: &mut HashMap<GroupId, RowSet>,
        edges: &HashSet<(GroupId, GroupId)>,
        components: &[Vec<GroupId>],
        stats: &mut RecalcStats,
        deadline: Instant,
        view: &[Rect],
    ) {
        let start = Instant::now();
        let plan_until = start + deadline.saturating_duration_since(start) / 3;
        // Changes to a spill can introduce dependencies on its newly written
        // cells. Groups reading its sheet stay on the normal spill rounds.
        let mut spill_sheets = HashSet::new();
        for &g in dirty.keys() {
            if Instant::now() >= plan_until { return; }
            let group = self.group(g);
            if can_array(&group.expr) { spill_sheets.insert(group.sheet); }
        }
        let eligible = |id| {
            let g = self.group(id);
            if edges.contains(&(id, id)) || g.volatile || can_array(&g.expr)
                || g.fps.iter().any(|fp| spill_sheets.contains(&fp.sheet)) {
                return false;
            }
            let mut static_refs = true;
            g.expr.walk(&mut |e| {
                if matches!(e, Expr::Spill(_) | Expr::Invoke(..) | Expr::Call(Func::Unknown(_), _)) ||
                    matches!(e, Expr::Name(n) if !n.is_empty()) {
                    static_refs = false;
                }
            });
            static_refs
        };
        let mut incoming: HashMap<GroupId, Vec<GroupId>> = HashMap::new();
        for &(source, consumer) in edges {
            if Instant::now() >= plan_until { return; }
            incoming.entry(consumer).or_default().push(source);
        }
        let mut priority = HashMap::<GroupId, RowSet>::new();
        // This order runs consumers first for planning, then sources first
        // for evaluation. Strongly connected groups keep their normal path.
        for component in components {
            if Instant::now() >= plan_until { return; }
            let &[id] = component.as_slice() else { continue };
            let group = self.group(id);
            let shown = interest(group, &dirty[&id], view);
            let mut wanted = priority.remove(&id).unwrap_or_default();
            if shown.is_empty() && wanted.is_empty() { continue; }
            if !eligible(id) { continue; }
            for (a, b) in shown.0 { wanted.add(a, b); }
            if wanted.len() > MAX_PRIORITY_ROWS { continue; }
            for &source in incoming.get(&id).into_iter().flatten() {
                let Some(needed) = wants(group, self.group(source), &wanted, &dirty[&source]) else { continue };
                if needed.is_empty() { continue; }
                let rows = priority.entry(source).or_default();
                if rows.len() + needed.len() <= MAX_PRIORITY_ROWS {
                    for (a, b) in needed.0 { rows.add(a, b); }
                }
            }
            priority.insert(id, wanted);
        }

        for component in components.iter().rev() {
            if Instant::now() >= deadline { break; }
            let &[id] = component.as_slice() else { continue };
            let Some(rows) = priority.remove(&id) else { continue };
            // A blocked or oversized prerequisite remains dirty. Let the
            // ordinary pass evaluate it before taking on any of its readers.
            let group = self.group(id);
            let blocked = incoming.get(&id).into_iter().flatten().any(|&source| {
                reads(group, self.group(source), &rows).any(|r| dirty[&source].within(r.r0, r.r1).next().is_some())
            });
            if blocked { continue; }
            let completed = match self.eval_parallel(id, &rows, stats, Some(deadline)) {
                Some(left) => rows.without(&left),
                None => rows,
            };
            let rest = dirty.get_mut(&id).unwrap();
            *rest = rest.without(&completed);
        }
    }
}
