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

use crate::addr::Rect;
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

/// Cell-graph edges one on-demand evaluation may walk (cycles among large fills).
pub const MAX_DEMAND_EDGES: u64 = 64 << 20;

impl Workbook {
    /// Recalculates everything affected by changes since the last call.
    pub fn recalc(&mut self) -> RecalcStats {
        let t0 = Instant::now();
        let mut stats = RecalcStats::default();
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
            if scc.len() == 1 {
                let g = scc[0];
                let rows = dirty.remove(&g).unwrap_or_default();
                if !edges.contains(&(g, g)) {
                    self.eval_parallel(g, &rows, &mut stats);
                    continue;
                }
                match self.self_order(g) {
                    Some(up) => {
                        self.eval_sequential(g, &rows, up, &mut stats);
                        continue;
                    }
                    None => {
                        let mut m = HashMap::new();
                        m.insert(g, rows);
                        self.eval_demand(&m, &mut stats);
                    }
                }
            } else {
                let mut m = HashMap::new();
                for g in scc {
                    m.insert(g, dirty.remove(&g).unwrap_or_default());
                }
                self.eval_demand(&m, &mut stats);
            }
        }
        stats.millis = t0.elapsed().as_secs_f64() * 1000.0;
        stats
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
        if let (Some(f), Some(b)) = (&grp.fast, bound) {
            if let Some(x) = f.run(b, row) {
                return (Value::Num(x), true);
            }
        }
        (Ctx::new(self, grp.sheet, row, grp.col).eval_cell(&grp.expr), false)
    }

    fn eval_parallel(&mut self, g: GroupId, rows: &RowSet, stats: &mut RecalcStats) {
        let total = rows.len();
        if total == 0 {
            return;
        }
        stats.groups += 1;
        stats.cells += total;
        let (sheet, col) = (self.group(g).sheet, self.group(g).col);
        // Partition the runs into about 4 parts per thread.
        let threads = self.threads.max(1);
        let parts: Vec<Vec<(u32, u32)>> = if total < PAR_MIN || threads == 1 {
            vec![rows.0.clone()]
        } else {
            let per = total.div_ceil(threads as u64 * 4).max(PAR_MIN / 2);
            let mut parts = vec![];
            let mut cur = vec![];
            let mut n = 0u64;
            for &(a, b) in &rows.0 {
                let mut s = a;
                loop {
                    let room = per - n;
                    let e = if (b - s) as u64 + 1 > room { s + room as u32 - 1 } else { b };
                    cur.push((s, e));
                    n += (e - s) as u64 + 1;
                    if n == per {
                        parts.push(std::mem::take(&mut cur));
                        n = 0;
                    }
                    if e == b {
                        break;
                    }
                    s = e + 1;
                }
            }
            if !cur.is_empty() {
                parts.push(cur);
            }
            parts
        };
        let this: &Workbook = self;
        let compute = |part: &Vec<(u32, u32)>| -> (Vec<Value>, u64) {
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
        let results: Vec<(Vec<Value>, u64)> = if parts.len() == 1 {
            vec![compute(&parts[0])]
        } else {
            std::thread::scope(|s| {
                let hs: Vec<_> = parts.iter().map(|p| std::thread::Builder::new().stack_size(16 << 20).spawn_scoped(s, || compute(p)).unwrap()).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            })
        };
        for (part, (vals, fast)) in parts.iter().zip(results) {
            stats.fast_cells += fast;
            let mut it = vals.into_iter();
            for &(a, b) in part {
                for r in a..=b {
                    let v = it.next().unwrap();
                    self.store(sheet, r, col, &v);
                }
            }
        }
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
