//! Column-chunked cell storage (docs/spec/officina.md §8.1).
//!
//! A column is a list of chunks of [`CHUNK_ROWS`] rows. Each chunk picks its encoding:
//!
//! - `Sparse`: up to [`SPARSE_MAX`] cells as sorted (row, cell) pairs, for scattered data;
//! - `Num`: a dense `f64` array plus a presence bitmap, for numeric columns (8 bytes and a
//!   bit per row);
//! - `Mixed`: a dense array of [`Cell`]s (16 bytes per row), for anything else.
//!
//! Chunks that become empty are dropped. Formula results are stored here like any value,
//! so a formula reading a cell reads storage and never re-evaluates.

use crate::strings::StrId;
use crate::value::ErrorKind;

pub const CHUNK_ROWS: u32 = 4096;
const CR: usize = CHUNK_ROWS as usize;
/// A sparse chunk becomes dense past this many cells.
pub const SPARSE_MAX: usize = 256;

/// A stored cell: a value with text as a string-pool id.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Cell {
    #[default]
    Empty,
    Num(f64),
    Str(StrId),
    Bool(bool),
    Err(ErrorKind),
}

impl Cell {
    pub fn is_empty(&self) -> bool {
        matches!(self, Cell::Empty)
    }
}

#[derive(Debug, Clone)]
pub enum Chunk {
    Sparse { rows: Vec<u16>, cells: Vec<Cell> },
    Num { vals: Box<[f64]>, present: Box<[u64]>, count: u32 },
    Mixed { cells: Box<[Cell]>, count: u32 },
}

impl Chunk {
    fn new() -> Self {
        Chunk::Sparse { rows: Vec::new(), cells: Vec::new() }
    }

    pub fn get(&self, off: usize) -> Cell {
        match self {
            Chunk::Sparse { rows, cells } => match rows.binary_search(&(off as u16)) {
                Ok(i) => cells[i],
                Err(_) => Cell::Empty,
            },
            Chunk::Num { vals, present, .. } => {
                if present[off / 64] >> (off % 64) & 1 != 0 {
                    Cell::Num(vals[off])
                } else {
                    Cell::Empty
                }
            }
            Chunk::Mixed { cells, .. } => cells[off],
        }
    }

    /// The number at `off` if the cell holds a number. The vectorised paths' read.
    #[inline]
    pub fn get_num(&self, off: usize) -> Option<f64> {
        match self {
            Chunk::Num { vals, present, .. } => (present[off / 64] >> (off % 64) & 1 != 0).then(|| vals[off]),
            _ => match self.get(off) {
                Cell::Num(x) => Some(x),
                _ => None,
            },
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Chunk::Sparse { rows, .. } => rows.len(),
            Chunk::Num { count, .. } | Chunk::Mixed { count, .. } => *count as usize,
        }
    }

    /// Sets a cell; returns false when the chunk is now empty.
    fn set(&mut self, off: usize, c: Cell) -> bool {
        match self {
            Chunk::Sparse { rows, cells } => {
                match rows.binary_search(&(off as u16)) {
                    Ok(i) => {
                        if c.is_empty() {
                            rows.remove(i);
                            cells.remove(i);
                        } else {
                            cells[i] = c;
                        }
                    }
                    Err(i) => {
                        if c.is_empty() {
                            return !rows.is_empty();
                        }
                        if rows.len() >= SPARSE_MAX {
                            self.densify();
                            return self.set(off, c);
                        }
                        rows.insert(i, off as u16);
                        cells.insert(i, c);
                    }
                }
                !rows.is_empty()
            }
            Chunk::Num { vals, present, count } => {
                let bit = 1u64 << (off % 64);
                let was = present[off / 64] & bit != 0;
                match c {
                    Cell::Empty => {
                        if was {
                            present[off / 64] &= !bit;
                            *count -= 1;
                        }
                        *count > 0
                    }
                    Cell::Num(x) => {
                        vals[off] = x;
                        if !was {
                            present[off / 64] |= bit;
                            *count += 1;
                        }
                        true
                    }
                    _ => {
                        self.to_mixed();
                        self.set(off, c)
                    }
                }
            }
            Chunk::Mixed { cells, count } => {
                let was = !cells[off].is_empty();
                cells[off] = c;
                match (was, c.is_empty()) {
                    (false, false) => *count += 1,
                    (true, true) => *count -= 1,
                    _ => {}
                }
                *count > 0
            }
        }
    }

    fn densify(&mut self) {
        if let Chunk::Sparse { rows, cells } = self {
            let all_num = cells.iter().all(|c| matches!(c, Cell::Num(_)));
            *self = if all_num {
                let mut vals = vec![0.0; CR].into_boxed_slice();
                let mut present = vec![0u64; CR / 64].into_boxed_slice();
                for (r, c) in rows.iter().zip(cells.iter()) {
                    if let Cell::Num(x) = c {
                        vals[*r as usize] = *x;
                        present[*r as usize / 64] |= 1 << (*r % 64);
                    }
                }
                Chunk::Num { vals, present, count: rows.len() as u32 }
            } else {
                let mut d = vec![Cell::Empty; CR].into_boxed_slice();
                for (r, c) in rows.iter().zip(cells.iter()) {
                    d[*r as usize] = *c;
                }
                Chunk::Mixed { cells: d, count: rows.len() as u32 }
            };
        }
    }

    fn to_mixed(&mut self) {
        if let Chunk::Num { vals, present, count } = self {
            let mut d = vec![Cell::Empty; CR].into_boxed_slice();
            for i in 0..CR {
                if present[i / 64] >> (i % 64) & 1 != 0 {
                    d[i] = Cell::Num(vals[i]);
                }
            }
            *self = Chunk::Mixed { cells: d, count: *count };
        }
    }

    /// Calls `f(off, cell)` for each non-empty cell with `lo <= off <= hi`, in order.
    pub fn each(&self, lo: usize, hi: usize, f: &mut dyn FnMut(usize, Cell)) {
        match self {
            Chunk::Sparse { rows, cells } => {
                let start = rows.partition_point(|&r| (r as usize) < lo);
                for i in start..rows.len() {
                    let r = rows[i] as usize;
                    if r > hi {
                        break;
                    }
                    f(r, cells[i]);
                }
            }
            Chunk::Num { vals, present, .. } => {
                let mut w = lo / 64;
                while w <= hi / 64 {
                    let mut bits = present[w];
                    if bits != 0 {
                        while bits != 0 {
                            let b = bits.trailing_zeros() as usize;
                            let i = w * 64 + b;
                            if i >= lo && i <= hi {
                                f(i, Cell::Num(vals[i]));
                            }
                            bits &= bits - 1;
                        }
                    }
                    w += 1;
                }
            }
            Chunk::Mixed { cells, .. } => {
                for (i, c) in cells.iter().enumerate().take(hi + 1).skip(lo) {
                    if !c.is_empty() {
                        f(i, *c);
                    }
                }
            }
        }
    }

    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Chunk>()
            + match self {
                Chunk::Sparse { rows, cells } => rows.capacity() * 2 + cells.capacity() * std::mem::size_of::<Cell>(),
                Chunk::Num { .. } => CR * 8 + CR / 8,
                Chunk::Mixed { .. } => CR * std::mem::size_of::<Cell>(),
            }
    }
}

/// One column of a sheet.
#[derive(Debug, Clone, Default)]
pub struct Column {
    chunks: Vec<Option<Box<Chunk>>>,
    /// Bumped on every write; lookup caches key on it.
    pub generation: u64,
}

impl Column {
    #[inline]
    pub fn chunk(&self, row: u32) -> Option<&Chunk> {
        self.chunks.get((row / CHUNK_ROWS) as usize).and_then(|c| c.as_deref())
    }

    #[inline]
    pub fn get(&self, row: u32) -> Cell {
        match self.chunk(row) {
            Some(c) => c.get((row % CHUNK_ROWS) as usize),
            None => Cell::Empty,
        }
    }

    #[inline]
    pub fn get_num(&self, row: u32) -> Option<f64> {
        self.chunk(row)?.get_num((row % CHUNK_ROWS) as usize)
    }

    pub fn set(&mut self, row: u32, c: Cell) {
        self.generation += 1;
        let ci = (row / CHUNK_ROWS) as usize;
        if ci >= self.chunks.len() {
            if c.is_empty() {
                return;
            }
            self.chunks.resize_with(ci + 1, || None);
        }
        let slot = &mut self.chunks[ci];
        if slot.is_none() {
            if c.is_empty() {
                return;
            }
            *slot = Some(Box::new(Chunk::new()));
        }
        if !slot.as_mut().unwrap().set((row % CHUNK_ROWS) as usize, c) {
            *slot = None;
            while matches!(self.chunks.last(), Some(None)) {
                self.chunks.pop();
            }
        }
    }

    /// Writes a run of numbers starting at `row` (the bulk path for loading and for
    /// vectorised formula results). `None` clears.
    pub fn set_nums(&mut self, row: u32, vals: &[Option<f64>]) {
        for (i, v) in vals.iter().enumerate() {
            self.set(row + i as u32, v.map_or(Cell::Empty, Cell::Num));
        }
    }

    /// Calls `f(row, cell)` for each non-empty cell in `r0..=r1`, in row order, skipping
    /// empty chunks without looking at them.
    pub fn each(&self, r0: u32, r1: u32, f: &mut dyn FnMut(u32, Cell)) {
        if self.chunks.is_empty() || r0 > r1 {
            return;
        }
        let last = self.chunks.len() as u32 - 1;
        let c0 = r0 / CHUNK_ROWS;
        let c1 = (r1 / CHUNK_ROWS).min(last);
        for ci in c0..=c1 {
            if let Some(ch) = &self.chunks[ci as usize] {
                let base = ci * CHUNK_ROWS;
                let lo = if ci == c0 { (r0 % CHUNK_ROWS) as usize } else { 0 };
                let hi = if ci == r1 / CHUNK_ROWS { (r1 % CHUNK_ROWS) as usize } else { CR - 1 };
                ch.each(lo, hi, &mut |o, c| f(base + o as u32, c));
            }
        }
    }

    /// As [`each`](Self::each), but runs of 64 present numbers in a numeric chunk go to
    /// `nums` as one slice (the aggregates' hot path); every other cell goes to `other`.
    pub fn each_num_run(&self, r0: u32, r1: u32, nums: &mut dyn FnMut(&[f64]), other: &mut dyn FnMut(u32, Cell)) {
        if self.chunks.is_empty() || r0 > r1 {
            return;
        }
        let last = self.chunks.len() as u32 - 1;
        let c0 = r0 / CHUNK_ROWS;
        let c1 = (r1 / CHUNK_ROWS).min(last);
        for ci in c0..=c1 {
            let Some(ch) = &self.chunks[ci as usize] else { continue };
            let base = ci * CHUNK_ROWS;
            let lo = if ci == c0 { (r0 % CHUNK_ROWS) as usize } else { 0 };
            let hi = if ci == r1 / CHUNK_ROWS { (r1 % CHUNK_ROWS) as usize } else { CR - 1 };
            match &**ch {
                Chunk::Num { vals, present, .. } => {
                    for w in lo / 64..=hi / 64 {
                        let (a, b) = (w * 64, w * 64 + 63);
                        if present[w] == u64::MAX && a >= lo && b <= hi {
                            nums(&vals[a..=b]);
                        } else {
                            let mut bits = present[w];
                            while bits != 0 {
                                let i = a + bits.trailing_zeros() as usize;
                                if i >= lo && i <= hi {
                                    nums(std::slice::from_ref(&vals[i]));
                                }
                                bits &= bits - 1;
                            }
                        }
                    }
                }
                _ => ch.each(lo, hi, &mut |o, c| other(base + o as u32, c)),
            }
        }
    }

    /// One past the last row holding a cell (0 when empty).
    pub fn extent(&self) -> u32 {
        for (ci, ch) in self.chunks.iter().enumerate().rev() {
            if let Some(ch) = ch {
                let mut last = 0;
                ch.each(0, CR - 1, &mut |o, _| last = o);
                return ci as u32 * CHUNK_ROWS + last as u32 + 1;
            }
        }
        0
    }

    pub fn count(&self) -> usize {
        self.chunks.iter().flatten().map(|c| c.count()).sum()
    }

    pub fn bytes(&self) -> usize {
        self.chunks.capacity() * 8 + self.chunks.iter().flatten().map(|c| c.bytes()).sum::<usize>()
    }

    /// Puts a whole chunk in place (the native format's load). The slot must be empty.
    pub(crate) fn put_chunk(&mut self, n: u32, c: Chunk) {
        self.generation += 1;
        let n = n as usize;
        if n >= self.chunks.len() {
            self.chunks.resize_with(n + 1, || None);
        }
        debug_assert!(self.chunks[n].is_none());
        self.chunks[n] = Some(Box::new(c));
    }

    /// The chunks in use, for the native format: (chunk number, chunk).
    pub fn chunks(&self) -> impl Iterator<Item = (u32, &Chunk)> {
        self.chunks.iter().enumerate().filter_map(|(i, c)| c.as_deref().map(|c| (i as u32, c)))
    }
}

/// Row heights or column widths: a default plus overrides, with offsets by prefix sums
/// (docs/spec/officina.md §8.1). Queries are O(log k) in the number of overrides.
#[derive(Debug, Clone)]
pub struct Sizes {
    pub default: f64,
    over: Vec<(u32, f64)>,
    /// prefix[i] = sum of (size - default) for over[..i]
    prefix: Vec<f64>,
}

impl Sizes {
    pub fn new(default: f64) -> Self {
        Sizes { default, over: Vec::new(), prefix: vec![0.0] }
    }
    pub fn set(&mut self, i: u32, size: f64) {
        match self.over.binary_search_by_key(&i, |e| e.0) {
            Ok(k) => {
                if size == self.default {
                    self.over.remove(k);
                } else {
                    self.over[k].1 = size;
                }
            }
            Err(k) => {
                if size != self.default {
                    self.over.insert(k, (i, size));
                }
            }
        }
        self.prefix.clear();
        self.prefix.push(0.0);
        let mut s = 0.0;
        for &(_, v) in &self.over {
            s += v - self.default;
            self.prefix.push(s);
        }
    }
    /// Sizes from a default and overrides in any order (a later duplicate wins); for
    /// loading many at once, as [`set`](Self::set) costs O(overrides) each.
    pub fn from_overrides(default: f64, mut over: Vec<(u32, f64)>) -> Self {
        over.reverse();
        over.sort_by_key(|e| e.0);
        over.dedup_by_key(|e| e.0);
        over.retain(|e| e.1 != default);
        Self::from_parts(default, over)
    }
    /// Sizes from a default and overrides sorted by index (the native format's load).
    pub(crate) fn from_parts(default: f64, over: Vec<(u32, f64)>) -> Self {
        let mut prefix = Vec::with_capacity(over.len() + 1);
        prefix.push(0.0);
        let mut s = 0.0;
        for &(_, v) in &over {
            s += v - default;
            prefix.push(s);
        }
        Sizes { default, over, prefix }
    }
    /// The overrides, sorted by index.
    pub fn overrides(&self) -> &[(u32, f64)] {
        &self.over
    }
    pub fn size(&self, i: u32) -> f64 {
        match self.over.binary_search_by_key(&i, |e| e.0) {
            Ok(k) => self.over[k].1,
            Err(_) => self.default,
        }
    }
    /// Where row (or column) `i` starts.
    pub fn offset(&self, i: u32) -> f64 {
        let k = self.over.partition_point(|e| e.0 < i);
        i as f64 * self.default + self.prefix[k]
    }
    /// The row (or column) at position `y` (>= 0).
    pub fn index_at(&self, y: f64) -> u32 {
        // Binary search on offset(i), monotonic when every size > 0.
        let (mut lo, mut hi) = (0u64, crate::addr::MAX_ROWS as u64);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if self.offset(mid as u32) <= y {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_transitions() {
        let mut c = Column::default();
        for r in 0..300 {
            c.set(r * 3, Cell::Num(r as f64));
        }
        assert!(matches!(c.chunk(0), Some(Chunk::Num { .. })));
        c.set(1, Cell::Str(4));
        assert!(matches!(c.chunk(0), Some(Chunk::Mixed { .. })));
        assert_eq!(c.get(3), Cell::Num(1.0));
        assert_eq!(c.get(1), Cell::Str(4));
        assert_eq!(c.get(2), Cell::Empty);
        assert_eq!(c.count(), 301);
        let mut n = 0;
        c.each(0, 30, &mut |_, _| n += 1);
        assert_eq!(n, 12); // rows 0,3,...,30 and row 1
        for r in 0..300 {
            c.set(r * 3, Cell::Empty);
        }
        c.set(1, Cell::Empty);
        assert_eq!(c.count(), 0);
        assert_eq!(c.extent(), 0);
        assert!(c.chunk(0).is_none());
    }

    #[test]
    fn sparse_far_rows() {
        let mut c = Column::default();
        c.set(2_000_000_000, Cell::Bool(true));
        assert_eq!(c.get(2_000_000_000), Cell::Bool(true));
        assert_eq!(c.extent(), 2_000_000_001);
        let mut seen = vec![];
        c.each(0, crate::addr::MAX_ROWS - 1, &mut |r, _| seen.push(r));
        assert_eq!(seen, vec![2_000_000_000]);
    }

    #[test]
    fn sizes() {
        let mut s = Sizes::new(20.0);
        s.set(2, 40.0);
        s.set(5, 10.0);
        assert_eq!(s.offset(0), 0.0);
        assert_eq!(s.offset(3), 80.0);
        assert_eq!(s.offset(6), 130.0);
        assert_eq!(s.index_at(79.0), 2);
        assert_eq!(s.index_at(80.0), 3);
        assert_eq!(s.index_at(1e9), 49_999_999); // 49_999_999*20 + 10 <= 1e9
        s.set(2, 20.0);
        assert_eq!(s.offset(3), 60.0);
    }
}
