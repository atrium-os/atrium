//! Incremental recalculation (docs/spec/officina.md §8.3).
//!
//! 1. **Dirty propagation.** Changed rectangles (value edits) and new or volatile groups
//!    seed it; each newly dirty run of a group's rows is looked up in the dependency
//!    index, and the rows of dependent groups it reaches become dirty in turn. Work is
//!    per (group, run of rows), never per cell.
//! 2. **Order.** The edges found on the way form a graph over dirty groups; its strongly
//!    connected components, in topological order, are evaluated one after another.
//! 3. **Evaluation.** A group that doesn't read itself has independent rows: they are
//!    computed in parallel (partitions over threads), through the vectorised path when it
//!    compiled. A group that reads only rows above itself (a running total) runs top
//!    down, and one reading only rows below runs bottom up. Anything else (real cycles,
//!    or groups reading each other) is evaluated cell by cell in the order of its cell
//!    graph's strongly connected components; cells on a cycle get `#CIRC!`.

use crate::addr::{CellAddr, Rect};
use crate::book::Workbook;
use crate::deps::{GroupId, RowSet, Span};
use crate::eval::Ctx;
use crate::value::{ErrorKind, Value};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Debug, Default, Clone)]
pub struct RecalcStats {
    /// Groups evaluated.
    pub groups: usize,
    /// Cells evaluated.
    pub cells: u64,
    /// Of which by the vectorised path.
    pub fast_cells: u64,
    /// Cells evaluated on demand (cycles and mutually dependent groups).
    pub demand_cells: u64,
    pub millis: f64,
}

/// Below this many rows a group is evaluated on one thread.
const PAR_MIN: u64 = 8192;

/// Rows in a slice of a group evaluated against a deadline.
const SLICE: u64 = 8;
/// A wave of slices quicker than this doubles the next one.
const WAVE_QUICK: std::time::Duration = std::time::Duration::from_millis(4);

/// Cell-graph edges one on-demand evaluation may walk (cycles among large fills).
pub const MAX_DEMAND_EDGES: u64 = 64 << 20;

/// Walk dirty runs without allocating partitions for work after this wave.
struct RowCursor<'a> {
    tail: Option<(u32, u32)>,
    rest: std::slice::Iter<'a, (u32, u32)>,
}

impl<'a> RowCursor<'a> {
    fn new(rows: &'a RowSet) -> Self {
        let mut rest = rows.0.iter();
        Self { tail: rest.next().copied(), rest }
    }

    fn take(&mut self, mut capacity: u64) -> Vec<(u32, u32)> {
        let mut part = Vec::new();
        while capacity > 0 {
            let Some((first, last)) = self.tail else { break };
            let size = u64::from(last - first) + 1;
            let taken = capacity.min(size);
            let end = first + (taken - 1) as u32;
            part.push((first, end));
            capacity -= taken;
            self.tail = if end == last { self.rest.next().copied() } else { Some((end + 1, last)) };
        }
        part
    }

    fn remaining(self) -> RowSet {
        RowSet(self.tail.into_iter().chain(self.rest.copied()).collect())
    }
}

impl Workbook {
    /// Recalculates everything affected by changes since the last call: rounds of
    /// [`Workbook::recalc_pass`] while dynamic arrays spill into cells others read.
    pub fn recalc(&mut self) -> RecalcStats {
        self.recalc_until(None).0
    }

    /// Recalculates for about `budget`, then stops between groups (or between
    /// slices of a long group) and leaves the rest pending: true when everything
    /// is done. Call it again, as the UI does each frame, while
    /// [`Workbook::recalc_pending`]; cells not yet done keep their old values and
    /// [`Workbook::is_stale`] says which. Edits made in between join the work.
    pub fn recalc_for(&mut self, budget: std::time::Duration) -> (RecalcStats, bool) {
        self.recalc_until(Some(Instant::now() + budget))
    }

    /// Whether recalculation has work left (a [`Workbook::recalc_for`] that ran out
    /// of time, or edits since the last one).
    pub fn recalc_pending(&self) -> bool {
        !self.pending.is_empty() || !self.changed.is_empty()
    }

    /// Whether the formula in a cell has inputs that changed and hasn't been
    /// recalculated since (its value is the old one).
    pub fn is_stale(&self, sheet: u16, row: u32, col: u32) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        self.group_at(sheet, row, col).is_some_and(|g| self.pending.get(&g).is_some_and(|rs| rs.contains(row)))
    }

    fn recalc_until(&mut self, deadline: Option<Instant>) -> (RecalcStats, bool) {
        let t0 = Instant::now();
        let mut stats = RecalcStats::default();
        self.recalcs += 1;
        self.spill_edits();
        for _ in 0..crate::spill::MAX_SPILL_ROUNDS {
            let done = self.recalc_pass(&mut stats, deadline);
            let more = self.apply_spills();
            if !done {
                stats.millis = t0.elapsed().as_secs_f64() * 1000.0;
                return (stats, false);
            }
            if !more {
                break;
            }
        }
        self.claims.clear();
        // Past the last round: the last writes' readers once more, without spilling.
        if !self.changed.is_empty() {
            let done = self.recalc_pass(&mut stats, deadline);
            self.spill_out.lock().unwrap().clear();
            if !done {
                stats.millis = t0.elapsed().as_secs_f64() * 1000.0;
                return (stats, false);
            }
        }
        stats.millis = t0.elapsed().as_secs_f64() * 1000.0;
        (stats, true)
    }

    /// One pass; false when `deadline` passed first (what is left is pending).
    fn recalc_pass(&mut self, stats: &mut RecalcStats, deadline: Option<Instant>) -> bool {
        let mut dirty: HashMap<GroupId, RowSet> = HashMap::new();
        let mut edges: HashSet<(GroupId, GroupId)> = HashSet::new();
        // Work list: (source group, rows newly dirty) or (None, changed rect).
        let mut work: Vec<(Option<GroupId>, Rect)> = Vec::new();

        for r in std::mem::take(&mut self.changed) {
            work.push((None, r));
        }
        let pending: Vec<(GroupId, RowSet)> = std::mem::take(&mut self.pending).into_iter().collect();
        let vol: Vec<GroupId> = self.volatile.iter().copied().collect();
        let seed = |wb: &Workbook, g: GroupId, a: u32, b: u32, dirty: &mut HashMap<GroupId, RowSet>, work: &mut Vec<(Option<GroupId>, Rect)>| {
            let grp = wb.group(g);
            for (x, y) in dirty.entry(g).or_default().add(a, b) {
                work.push((Some(g), Rect { sheet: grp.sheet, r0: x, r1: y, c0: grp.col, c1: grp.col }));
            }
        };
        for (g, rs) in pending {
            if self.groups.get(g as usize).is_some_and(|x| x.is_some()) {
                for (a, b) in rs.0 {
                    seed(self, g, a, b, &mut dirty, &mut work);
                }
            }
        }
        for g in vol {
            let (a, b) = (self.group(g).r0, self.group(g).r1);
            seed(self, g, a, b, &mut dirty, &mut work);
        }

        let mut hits = Vec::new();
        while let Some((src, rect)) = work.pop() {
            hits.clear();
            self.deps.query(&rect, &mut hits);
            for &id in &hits {
                let e = self.deps.entry(id).clone();
                let h = e.group;
                let grp = self.group(h);
                let Some((a, b)) = e.fp.affected(grp.r0, grp.r1, rect.r0, rect.r1) else { continue };
                if let Some(s) = src {
                    edges.insert((s, h));
                }
                let (sheet, col) = (grp.sheet, grp.col);
                for (x, y) in dirty.entry(h).or_default().add(a, b) {
                    work.push((Some(h), Rect { sheet, r0: x, r1: y, c0: col, c1: col }));
                }
            }
        }

        // Strongly connected components, in dependency order.
        let nodes: Vec<GroupId> = dirty.keys().copied().collect();
        let mut adj: HashMap<GroupId, Vec<GroupId>> = HashMap::new();
        for &(a, b) in &edges {
            adj.entry(a).or_default().push(b);
        }
        let sccs = tarjan(&nodes, &adj);

        for scc in sccs.into_iter().rev() {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                // Out of time: what is still dirty waits for the next call.
                for (g, rs) in dirty {
                    let p = self.pending.entry(g).or_default();
                    for (a, b) in rs.0 {
                        p.add(a, b);
                    }
                }
                return false;
            }
            if scc.len() == 1 {
                let g = scc[0];
                let rows = dirty.remove(&g).unwrap_or_default();
                if !edges.contains(&(g, g)) {
                    if !self.eval_parallel(g, &rows, stats, deadline) {
                        for (g, rs) in dirty {
                            let p = self.pending.entry(g).or_default();
                            for (a, b) in rs.0 {
                                p.add(a, b);
                            }
                        }
                        return false;
                    }
                    continue;
                }
                match self.self_order(g) {
                    Some(up) => {
                        self.eval_sequential(g, &rows, up, stats);
                        continue;
                    }
                    None => {
                        let mut m = HashMap::new();
                        m.insert(g, rows);
                        self.eval_demand(&m, stats);
                    }
                }
            } else {
                let mut m = HashMap::new();
                for g in scc {
                    m.insert(g, dirty.remove(&g).unwrap_or_default());
                }
                self.eval_demand(&m, stats);
            }
        }
        true
    }

    /// For a group that reads itself: `Some(true)` if every self-reference is to rows
    /// strictly above (evaluate top down), `Some(false)` if strictly below, else `None`.
    fn self_order(&self, g: GroupId) -> Option<bool> {
        let grp = self.group(g);
        let mut above = true;
        let mut below = true;
        for fp in &grp.fps {
            if fp.sheet != grp.sheet || !(fp.c0..=fp.c1).contains(&grp.col) {
                continue;
            }
            match fp.rows {
                Span::Slide { d0, d1 } => {
                    above &= d1 < 0;
                    below &= d0 > 0;
                }
                Span::Fixed { a, b } => {
                    // A fixed range inside the group's rows is a real cycle for some rows.
                    if a <= grp.r1 && b >= grp.r0 {
                        return None;
                    }
                }
            }
        }
        if above {
            Some(true)
        } else if below {
            Some(false)
        } else {
            None
        }
    }

    fn eval_cell(&self, g: GroupId, row: u32, bound: Option<&[Option<&crate::column::Column>]>) -> (Value, bool) {
        let grp = self.group(g);
        let at = CellAddr::new(grp.sheet, row, grp.col);
        let known = self.spill_known(&at);
        if let (Some(f), Some(b)) = (&grp.fast, bound) {
            if let Some(x) = f.run(b, row) {
                if known {
                    self.spill_result(at, None);
                }
                return (Value::Num(x), true);
            }
        }
        let (v, arr) = Ctx::new(self, grp.sheet, row, grp.col).eval_spill(&grp.expr);
        if arr.is_some() || known {
            self.spill_result(at, arr);
        }
        (v, false)
    }

    /// False when `deadline` passed between slices (the rest is pending).
    fn eval_parallel(&mut self, g: GroupId, rows: &RowSet, stats: &mut RecalcStats, deadline: Option<Instant>) -> bool {
        let total = rows.len();
        if total == 0 {
            return true;
        }
        stats.groups += 1;
        let (sheet, col) = (self.group(g).sheet, self.group(g).col);
        let threads = self.threads.max(1);
        // Full recalculation keeps roughly four parts per thread. With a
        // deadline each worker gets one part in the current wave.
        let per = if total < PAR_MIN || threads == 1 {
            total
        } else {
            total.div_ceil(threads as u64 * 4).max(PAR_MIN / 2)
        };
        let mut rows = RowCursor::new(rows);
        let shared = std::sync::Arc::new(crate::eval::ifcache::Shared::default());
        let compute = |this: &Workbook, part: &[(u32, u32)]| -> (Vec<Value>, u64) {
            // What the group reads can't change until every part is done (it
            // doesn't read its own column).
            let _frozen = crate::eval::ifcache::Frozen::new(&shared);
            let grp = this.group(g);
            let bound = grp.fast.as_ref().map(|f| f.bind(this));
            let mut out = Vec::with_capacity(part.iter().map(|&(a, b)| (b - a) as usize + 1).sum());
            let mut fast = 0;
            for &(a, b) in part {
                for r in a..=b {
                    let (v, f) = this.eval_cell(g, r, bound.as_deref());
                    fast += f as u64;
                    out.push(v);
                }
            }
            (out, fast)
        };
        let mut per_thread = 1u64;
        let mut first = true;
        while rows.tail.is_some() {
            if !first && deadline.is_some_and(|d| Instant::now() >= d) {
                self.pending.insert(g, rows.remaining());
                return false;
            }
            first = false;
            let (workers, capacity) = if deadline.is_some() {
                (threads, SLICE * per_thread)
            } else {
                (total.div_ceil(per) as usize, per)
            };
            let mut parts = Vec::with_capacity(workers);
            for _ in 0..workers {
                if rows.tail.is_none() { break; }
                parts.push(rows.take(capacity));
            }
            let t = Instant::now();
            let this: &Workbook = self;
            let results: Vec<(Vec<Value>, u64)> = if parts.len() == 1 {
                vec![compute(this, &parts[0])]
            } else {
                std::thread::scope(|s| {
                    let workers: Vec<_> = parts.iter().map(|part| {
                        let compute = &compute;
                        std::thread::Builder::new().stack_size(16 << 20)
                            .spawn_scoped(s, move || compute(this, part)).unwrap()
                    }).collect();
                    workers.into_iter().map(|h| h.join().unwrap()).collect()
                })
            };
            if t.elapsed() < WAVE_QUICK {
                per_thread = (per_thread * 2).min(1 << 14);
            } else if t.elapsed() > WAVE_QUICK * 4 {
                per_thread = (per_thread / 2).max(1);
            }
            for (part, (vals, fast)) in parts.into_iter().zip(results) {
                stats.fast_cells += fast;
                stats.cells += vals.len() as u64;
                let mut it = vals.into_iter();
                for (a, b) in part {
                    for r in a..=b {
                        let v = it.next().unwrap();
                        self.store(sheet, r, col, &v);
                    }
                }
            }
        }
        true
    }

    fn eval_sequential(&mut self, g: GroupId, rows: &RowSet, top_down: bool, stats: &mut RecalcStats) {
        stats.groups += 1;
        stats.cells += rows.len();
        let (sheet, col) = (self.group(g).sheet, self.group(g).col);
        let mut order: Vec<(u32, u32)> = rows.0.clone();
        if !top_down {
            order.reverse();
        }
        let fast = self.group(g).fast.clone();
        for (a, b) in order {
            let mut r = if top_down { a } else { b };
            loop {
                // Bind per row: the previous row's write may have made a new chunk.
                let v = match &fast {
                    Some(f) => match f.run(&f.bind(self), r) {
                        Some(x) => {
                            stats.fast_cells += 1;
                            Value::Num(x)
                        }
                        None => self.eval_cell(g, r, None).0,
                    },
                    None => self.eval_cell(g, r, None).0,
                };
                self.store(sheet, r, col, &v);
                if top_down {
                    if r == b {
                        break;
                    }
                    r += 1;
                } else {
                    if r == a {
                        break;
                    }
                    r -= 1;
                }
            }
        }
    }

    /// Cell-by-cell evaluation for groups that depend on each other.
    ///
    /// Builds the cell graph of the set (each cell to the cells of the set it reads, by
    /// its references, as Excel judges circularity), finds its strongly connected
    /// components, and evaluates them precedents first. Every cell of a component with
    /// more than one cell, or reading itself, is `#CIRC!`; the rest are evaluated
    /// normally. The result does not depend on the order cells are visited in, so an
    /// incremental recalculation agrees with a full one. Past [`MAX_DEMAND_EDGES`] the
    /// whole set is `#LIMIT!` rather than an unbounded walk.
    fn eval_demand(&mut self, set: &HashMap<GroupId, RowSet>, stats: &mut RecalcStats) {
        stats.groups += set.len();
        let mut cells: Vec<(GroupId, u32)> = Vec::new();
        let mut keys: Vec<GroupId> = set.keys().copied().collect();
        keys.sort_unstable();
        for g in &keys {
            for &(a, b) in &set[g].0 {
                for r in a..=b {
                    cells.push((*g, r));
                }
            }
        }
        let id: HashMap<(GroupId, u32), u32> = cells.iter().enumerate().map(|(i, &c)| (c, i as u32)).collect();
        let mut budget = MAX_DEMAND_EDGES;
        // The cells of the set that cell `v` reads.
        let succ = |wb: &Workbook, v: u32, budget: &mut u64| -> Option<Vec<u32>> {
            let (g, r) = cells[v as usize];
            let mut out = Vec::new();
            for fp in &wb.group(g).fps {
                let Some(rect) = fp.at(r) else { continue };
                for h in &keys {
                    let hg = wb.group(*h);
                    if hg.sheet != rect.sheet || !(rect.c0..=rect.c1).contains(&hg.col) {
                        continue;
                    }
                    for (a, b) in set[h].within(rect.r0, rect.r1) {
                        let n = (b - a) as u64 + 1;
                        if n > *budget {
                            return None;
                        }
                        *budget -= n;
                        out.extend((a..=b).map(|x| id[&(*h, x)]));
                    }
                }
            }
            Some(out)
        };

        // Iterative Tarjan; components come out precedents first.
        const NONE: u32 = u32::MAX;
        let n = cells.len();
        let mut index = vec![NONE; n];
        let mut low = vec![0u32; n];
        let mut on = vec![false; n];
        let mut st: Vec<u32> = Vec::new();
        let mut next = 0u32;
        let mut comps: Vec<(Vec<u32>, bool)> = Vec::new();
        for root in 0..n as u32 {
            if index[root as usize] != NONE {
                continue;
            }
            let Some(s0) = succ(self, root, &mut budget) else { return self.demand_limit(&cells, stats) };
            let mut call: Vec<(u32, Vec<u32>, usize, bool)> = vec![(root, s0, 0, false)];
            index[root as usize] = next;
            low[root as usize] = next;
            next += 1;
            st.push(root);
            on[root as usize] = true;
            while let Some(top) = call.last_mut() {
                let v = top.0;
                if top.2 < top.1.len() {
                    let w = top.1[top.2];
                    top.2 += 1;
                    if w == v {
                        top.3 = true;
                    }
                    if index[w as usize] == NONE {
                        let Some(sw) = succ(self, w, &mut budget) else { return self.demand_limit(&cells, stats) };
                        index[w as usize] = next;
                        low[w as usize] = next;
                        next += 1;
                        st.push(w);
                        on[w as usize] = true;
                        call.push((w, sw, 0, false));
                    } else if on[w as usize] {
                        low[v as usize] = low[v as usize].min(index[w as usize]);
                    }
                } else {
                    let self_loop = top.3;
                    call.pop();
                    if let Some(p) = call.last() {
                        low[p.0 as usize] = low[p.0 as usize].min(low[v as usize]);
                    }
                    if low[v as usize] == index[v as usize] {
                        let mut comp = Vec::new();
                        loop {
                            let w = st.pop().unwrap();
                            on[w as usize] = false;
                            comp.push(w);
                            if w == v {
                                break;
                            }
                        }
                        let circ = self_loop || comp.len() > 1;
                        comps.push((comp, circ));
                    }
                }
            }
        }
        for (comp, circ) in comps {
            for v in comp {
                let (g, r) = cells[v as usize];
                let val = if circ { Value::Err(ErrorKind::Circ) } else { self.eval_cell(g, r, None).0 };
                let grp = self.group(g);
                let (sheet, col) = (grp.sheet, grp.col);
                self.store(sheet, r, col, &val);
                stats.cells += 1;
                stats.demand_cells += 1;
            }
        }
    }

    fn demand_limit(&mut self, cells: &[(GroupId, u32)], stats: &mut RecalcStats) {
        for &(g, r) in cells {
            let grp = self.group(g);
            let (sheet, col) = (grp.sheet, grp.col);
            self.store(sheet, r, col, &Value::Err(ErrorKind::Limit));
            stats.cells += 1;
        }
    }

    /// Recalculates every formula from scratch (the reference for tests, and what a
    /// "recalculate all" command runs).
    pub fn recalc_all(&mut self) -> RecalcStats {
        let ids: Vec<(GroupId, u32, u32)> = self.groups().map(|(g, grp)| (g, grp.r0, grp.r1)).collect();
        for (g, a, b) in ids {
            self.pending.entry(g).or_default().add(a, b);
        }
        self.recalc()
    }
}

/// Tarjan's SCC algorithm, iterative. Returns components in reverse topological order
/// (a component comes before the ones that point to it).
fn tarjan(nodes: &[GroupId], adj: &HashMap<GroupId, Vec<GroupId>>) -> Vec<Vec<GroupId>> {
    let mut index: HashMap<GroupId, (usize, usize)> = HashMap::new(); // (index, lowlink)
    let mut on: HashSet<GroupId> = HashSet::new();
    let mut st: Vec<GroupId> = Vec::new();
    let mut out = Vec::new();
    let mut next = 0usize;
    let empty = Vec::new();
    for &root in nodes {
        if index.contains_key(&root) {
            continue;
        }
        // (node, next child position)
        let mut call: Vec<(GroupId, usize)> = vec![(root, 0)];
        index.insert(root, (next, next));
        next += 1;
        st.push(root);
        on.insert(root);
        while let Some(&mut (v, ref mut i)) = call.last_mut() {
            let succ = adj.get(&v).unwrap_or(&empty);
            if *i < succ.len() {
                let w = succ[*i];
                *i += 1;
                if let Some(&(wi, _)) = index.get(&w) {
                    if on.contains(&w) {
                        let e = index.get_mut(&v).unwrap();
                        e.1 = e.1.min(wi);
                    }
                } else if nodes_contains(adj, &index, w) {
                    index.insert(w, (next, next));
                    next += 1;
                    st.push(w);
                    on.insert(w);
                    call.push((w, 0));
                }
            } else {
                call.pop();
                let (vi, vl) = index[&v];
                if let Some(&(p, _)) = call.last() {
                    let e = index.get_mut(&p).unwrap();
                    e.1 = e.1.min(vl);
                }
                if vi == vl {
                    let mut comp = Vec::new();
                    loop {
                        let w = st.pop().unwrap();
                        on.remove(&w);
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(comp);
                }
            }
        }
    }
    out
}

/// Every edge target is a dirty group (edges are only made to groups made dirty), so
/// any unvisited target is a node.
fn nodes_contains(_adj: &HashMap<GroupId, Vec<GroupId>>, _index: &HashMap<GroupId, (usize, usize)>, _w: GroupId) -> bool {
    true
}
