//! The range dependency graph (docs/spec/officina.md §8.3).
//!
//! A formula group registers one **footprint** per reference it makes: the sheet, the
//! columns, and the rows as a function of the formula's row, either sliding with it
//! (`A2` filled down reads row r+d) or fixed (`$A$1`, `A1:A1000000`). Edges are between
//! footprints and ranges, never between cells, so a million-row group reading a
//! million-row range is one entry.
//!
//! [`DepIndex`] answers "which footprints touch this rectangle" with per-column-block
//! interval trees (sorted by first row, a max-tree over last rows), rebuilt lazily after
//! changes.

use std::sync::Arc;
use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::formula::{Coord, Expr};
use std::collections::HashMap;

pub type GroupId = u32;

/// Rows a footprint covers, for a formula in row `r`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// rows r+d0 ..= r+d1
    Slide { d0: i64, d1: i64 },
    /// rows a ..= b whatever the row
    Fixed { a: u32, b: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    pub sheet: u16,
    pub c0: u32,
    pub c1: u32,
    pub rows: Span,
}

impl Footprint {
    /// The rows of the group (`r0..=r1`) whose reference meets rows `x0..=x1`.
    pub fn affected(&self, r0: u32, r1: u32, x0: u32, x1: u32) -> Option<(u32, u32)> {
        match self.rows {
            Span::Fixed { a, b } => (a <= x1 && b >= x0).then_some((r0, r1)),
            Span::Slide { d0, d1 } => {
                let lo = (x0 as i64 - d1).max(r0 as i64);
                let hi = (x1 as i64 - d0).min(r1 as i64);
                (lo <= hi).then(|| (lo as u32, hi as u32))
            }
        }
    }
    /// Everything the footprint covers for formulas in rows `r0..=r1`.
    pub fn rect(&self, r0: u32, r1: u32) -> Option<Rect> {
        let (a, b) = match self.rows {
            Span::Fixed { a, b } => (a as i64, b as i64),
            Span::Slide { d0, d1 } => (r0 as i64 + d0, r1 as i64 + d1),
        };
        let a = a.max(0);
        let b = b.min(MAX_ROWS as i64 - 1);
        (a <= b).then(|| Rect { sheet: self.sheet, r0: a as u32, r1: b as u32, c0: self.c0, c1: self.c1 })
    }
    /// The rectangle read by the formula in row `r` (`None`: off the sheet).
    pub fn at(&self, r: u32) -> Option<Rect> {
        self.rect(r, r)
    }
}

/// The footprints of a formula in column `col` of `sheet`, for rows `r0..=r1`.
/// `names` gives what a name or structured reference stands for, seen from a cell (sheet,
/// row, column): a formula's footprints include those of the names and tables it uses
/// (names are relative to the using cell, as if inlined).
pub fn footprints(e: &Expr, sheet: u16, col: u32, r0: u32, r1: u32, names: &dyn Fn(u16, u32, u32, &Expr) -> Option<Arc<Expr>>) -> Vec<Footprint> {
    let mut out = footprints_d(e, sheet, col, r0, r1, names, 0);
    out.dedup();
    out
}

fn footprints_d(e: &Expr, sheet: u16, col: u32, r0: u32, r1: u32, names: &dyn Fn(u16, u32, u32, &Expr) -> Option<Arc<Expr>>, depth: u32) -> Vec<Footprint> {
    let mut out = Vec::new();
    let mut named: Vec<Arc<Expr>> = Vec::new();
    let rowspan = |a: Coord, b: Coord| -> Span {
        match (a, b) {
            (Coord::Rel(x), Coord::Rel(y)) => Span::Slide { d0: x.min(y) as i64, d1: x.max(y) as i64 },
            (Coord::Abs(x), Coord::Abs(y)) => Span::Fixed { a: x.min(y), b: x.max(y) },
            (Coord::Abs(x), Coord::Rel(d)) | (Coord::Rel(d), Coord::Abs(x)) => {
                let lo = (x as i64).min(r0 as i64 + d as i64).max(0);
                let hi = (x as i64).max(r1 as i64 + d as i64).min(MAX_ROWS as i64 - 1);
                Span::Fixed { a: lo as u32, b: hi.max(lo) as u32 }
            }
        }
    };
    e.walk(&mut |n| match n {
        Expr::Ref(r) => {
            if let Some(c) = r.col.resolve(col, MAX_COLS) {
                out.push(Footprint { sheet: r.sheet.unwrap_or(sheet), c0: c, c1: c, rows: rowspan(r.row, r.row) });
            }
        }
        Expr::Range(r) => {
            if let (Some(a), Some(b)) = (r.c0.resolve(col, MAX_COLS), r.c1.resolve(col, MAX_COLS)) {
                out.push(Footprint { sheet: r.sheet.unwrap_or(sheet), c0: a.min(b), c1: a.max(b), rows: rowspan(r.r0, r.r1) });
            }
        }
        Expr::Name(x) if x.is_empty() => {}
        Expr::Name(_) | Expr::Table(_) if depth < crate::names::MAX_NAME_DEPTH => {
            if let Some(x) = names(sheet, r0, col, n) {
                named.push(x);
            }
        }
        _ => {}
    });
    for x in named {
        out.extend(footprints_d(&x, sheet, col, r0, r1, names, depth + 1));
    }
    out
}

#[derive(Debug, Clone)]
pub struct DepEntry {
    pub group: GroupId,
    pub fp: Footprint,
    pub rect: Rect,
}

const BLOCK: u32 = 64;
/// Footprints wider than this many column blocks go in the sheet's wide list.
const WIDE_BLOCKS: u32 = 16;

#[derive(Debug, Default)]
struct Bucket {
    ids: Vec<u32>,
    built: Option<Built>,
}

#[derive(Debug)]
struct Built {
    /// entry ids sorted by first row
    ids: Vec<u32>,
    starts: Vec<u32>,
    size: usize,
    /// max-tree over last rows, leaves at size..2*size
    tree: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct DepIndex {
    entries: Vec<Option<DepEntry>>,
    free: Vec<u32>,
    buckets: HashMap<(u16, u32), Bucket>,
    wide: HashMap<u16, Bucket>,
}

impl DepIndex {
    fn blocks(rect: &Rect) -> Option<(u32, u32)> {
        let (b0, b1) = (rect.c0 / BLOCK, rect.c1 / BLOCK);
        (b1 - b0 < WIDE_BLOCKS).then_some((b0, b1))
    }

    pub fn add(&mut self, group: GroupId, fp: Footprint, rect: Rect) -> u32 {
        let e = DepEntry { group, fp, rect };
        let id = match self.free.pop() {
            Some(id) => {
                self.entries[id as usize] = Some(e);
                id
            }
            None => {
                self.entries.push(Some(e));
                (self.entries.len() - 1) as u32
            }
        };
        match Self::blocks(&rect) {
            Some((b0, b1)) => {
                for b in b0..=b1 {
                    let bk = self.buckets.entry((rect.sheet, b)).or_default();
                    bk.ids.push(id);
                    bk.built = None;
                }
            }
            None => {
                let bk = self.wide.entry(rect.sheet).or_default();
                bk.ids.push(id);
                bk.built = None;
            }
        }
        id
    }

    pub fn remove(&mut self, id: u32) {
        let Some(e) = self.entries[id as usize].take() else { return };
        let rect = e.rect;
        let strip = |bk: &mut Bucket| {
            bk.ids.retain(|&x| x != id);
            bk.built = None;
        };
        match Self::blocks(&rect) {
            Some((b0, b1)) => {
                for b in b0..=b1 {
                    if let Some(bk) = self.buckets.get_mut(&(rect.sheet, b)) {
                        strip(bk);
                    }
                }
            }
            None => {
                if let Some(bk) = self.wide.get_mut(&rect.sheet) {
                    strip(bk);
                }
            }
        }
        self.free.push(id);
    }

    pub fn entry(&self, id: u32) -> &DepEntry {
        self.entries[id as usize].as_ref().unwrap()
    }

    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ids of every footprint touching `q`, deduplicated.
    pub fn query(&mut self, q: &Rect, out: &mut Vec<u32>) {
        let start = out.len();
        let entries = &self.entries;
        let visit = |bk: &mut Bucket, out: &mut Vec<u32>| {
            if bk.ids.is_empty() {
                return;
            }
            let built = bk.built.get_or_insert_with(|| build(entries, &bk.ids));
            let k = built.starts.partition_point(|&s| s <= q.r1);
            if k > 0 {
                collect(built, 1, 0, built.size, k, q.r0, out);
            }
        };
        let (b0, b1) = (q.c0 / BLOCK, q.c1 / BLOCK);
        if b1 - b0 <= 1024 {
            for b in b0..=b1 {
                if let Some(bk) = self.buckets.get_mut(&(q.sheet, b)) {
                    visit(bk, out);
                }
            }
        } else {
            for ((s, b), bk) in self.buckets.iter_mut() {
                if *s == q.sheet && (b0..=b1).contains(b) {
                    visit(bk, out);
                }
            }
        }
        if let Some(bk) = self.wide.get_mut(&q.sheet) {
            visit(bk, out);
        }
        // Exact test (columns too), then dedup what this query added.
        let tail = &mut out[start..];
        tail.sort_unstable();
        let mut w = start;
        for i in start..out.len() {
            let id = out[i];
            if (w > start && out[w - 1] == id) || !self.entries[id as usize].as_ref().is_some_and(|e| e.rect.intersects(q)) {
                continue;
            }
            out[w] = id;
            w += 1;
        }
        out.truncate(w);
    }
}

fn build(entries: &[Option<DepEntry>], ids: &[u32]) -> Built {
    let mut v: Vec<u32> = ids.iter().copied().filter(|&i| entries[i as usize].is_some()).collect();
    v.sort_by_key(|&i| entries[i as usize].as_ref().unwrap().rect.r0);
    let starts: Vec<u32> = v.iter().map(|&i| entries[i as usize].as_ref().unwrap().rect.r0).collect();
    let size = v.len().next_power_of_two().max(1);
    let mut tree = vec![0u32; 2 * size];
    for (k, &i) in v.iter().enumerate() {
        tree[size + k] = entries[i as usize].as_ref().unwrap().rect.r1;
    }
    for n in (1..size).rev() {
        tree[n] = tree[2 * n].max(tree[2 * n + 1]);
    }
    Built { ids: v, starts, size, tree }
}

/// Leaves in [lo, hi) of node `n`, below position `k`, whose last row is >= `x0`.
fn collect(b: &Built, n: usize, lo: usize, hi: usize, k: usize, x0: u32, out: &mut Vec<u32>) {
    if lo >= k || lo >= b.ids.len() || b.tree[n] < x0 {
        return;
    }
    if hi - lo == 1 {
        out.push(b.ids[lo]);
        return;
    }
    let mid = (lo + hi) / 2;
    collect(b, 2 * n, lo, mid, k, x0, out);
    collect(b, 2 * n + 1, mid, hi, k, x0, out);
}

/// A set of rows as sorted, disjoint, non-adjacent inclusive intervals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowSet(pub Vec<(u32, u32)>);

impl RowSet {
    /// Adds `a..=b`; returns the parts that were not already in the set.
    pub fn add(&mut self, a: u32, b: u32) -> Vec<(u32, u32)> {
        let v = &mut self.0;
        let mut new = Vec::new();
        // Find the first interval that could touch [a-1, b+1].
        let i = v.partition_point(|&(_, e)| (e as u64) + 1 < a as u64);
        let mut j = i;
        let mut lo = a;
        let mut hi = b;
        let mut cur = a as u64;
        while j < v.len() && v[j].0 as u64 <= b as u64 + 1 {
            let (s, e) = v[j];
            if (s as u64) > cur && cur <= b as u64 {
                new.push((cur as u32, (s as u64 - 1).min(b as u64) as u32));
            }
            cur = cur.max(e as u64 + 1);
            lo = lo.min(s);
            hi = hi.max(e);
            j += 1;
        }
        if cur <= b as u64 {
            new.push((cur as u32, b));
        }
        v.splice(i..j, [(lo, hi)]);
        new
    }
    pub fn contains(&self, r: u32) -> bool {
        let i = self.0.partition_point(|&(_, e)| e < r);
        i < self.0.len() && self.0[i].0 <= r
    }
    pub fn len(&self) -> u64 {
        self.0.iter().map(|&(a, b)| (b - a) as u64 + 1).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The intervals clipped to `a..=b`.
    pub fn within(&self, a: u32, b: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        let i = self.0.partition_point(|&(_, e)| e < a);
        self.0[i..].iter().take_while(move |&&(s, _)| s <= b).map(move |&(s, e)| (s.max(a), e.min(b)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rowset() {
        let mut s = RowSet::default();
        assert_eq!(s.add(10, 20), vec![(10, 20)]);
        assert_eq!(s.add(15, 25), vec![(21, 25)]);
        assert_eq!(s.add(0, 5), vec![(0, 5)]);
        assert_eq!(s.add(6, 9), vec![(6, 9)]);
        assert_eq!(s.0, vec![(0, 25)]);
        assert_eq!(s.add(30, 40), vec![(30, 40)]);
        assert_eq!(s.add(0, 50), vec![(26, 29), (41, 50)]);
        assert_eq!(s.0, vec![(0, 50)]);
        assert!(s.contains(50) && !s.contains(51));
        assert_eq!(s.add(u32::MAX - 1, u32::MAX), vec![(u32::MAX - 1, u32::MAX)]);
    }

    #[test]
    fn index_queries() {
        let mut d = DepIndex::default();
        let r = |r0, c0, r1, c1| Rect { sheet: 0, r0, c0, r1, c1 };
        let fp = Footprint { sheet: 0, c0: 0, c1: 0, rows: Span::Fixed { a: 0, b: 0 } };
        let a = d.add(1, fp, r(0, 0, 99, 0));
        let b = d.add(2, fp, r(50, 0, 60, 2));
        let c = d.add(3, fp, r(0, 0, 10, 100_000)); // wide
        let mut out = vec![];
        d.query(&r(55, 1, 55, 1), &mut out);
        out.sort();
        assert_eq!(out, vec![b]);
        out.clear();
        d.query(&r(5, 0, 5, 0), &mut out);
        out.sort();
        assert_eq!(out, vec![a, c]);
        d.remove(a);
        out.clear();
        d.query(&r(5, 0, 5, 0), &mut out);
        assert_eq!(out, vec![c]);
    }

    #[test]
    fn affected_rows() {
        // B2:B1000001 = A1 + A2 (rows r-1 and r): changing A10 affects rows 10 and 11
        // (0-based 9 hits formulas at 9 and 10).
        let fp = Footprint { sheet: 0, c0: 0, c1: 0, rows: Span::Slide { d0: -1, d1: 0 } };
        assert_eq!(fp.affected(1, 1_000_000, 9, 9), Some((9, 10)));
        assert_eq!(fp.affected(1, 1_000_000, 0, 0), Some((1, 1)));
        let fixed = Footprint { rows: Span::Fixed { a: 0, b: 9 }, ..fp };
        assert_eq!(fixed.affected(5, 7, 3, 3), Some((5, 7)));
        assert_eq!(fixed.affected(5, 7, 10, 10), None);
    }
}
