//! An index of the -IF functions' ranges while a formula group is evaluated.
//!
//! `=SUMIF(C:C, C1, B:B)` filled down n rows walks column C n times: n² cell reads,
//! minutes at 100,000 rows. While a group's rows are evaluated against a workbook
//! that can't change (the parallel path of recalculation), a range read with an
//! equality criterion is walked once into a map from the cells' keys to their
//! count and sums, and every later row is a lookup. Sums are added in the order
//! the walk would add them, so results are the same to the last bit. One cache
//! serves every thread and every slice of the group.
//!
//! Criteria such as `">"&A1` on one range are answered from the range's
//! numbers sorted once (grouped by the keys of any equality criteria beside
//! them), with counts by binary search. Sums from the sorted order are used
//! only when every number summed is a whole number and their magnitudes add up
//! to under 2^53, so any order of adding gives the same bits; otherwise sums
//! are walked as before.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::criteria::{Key, Side};
use crate::addr::Rect;
use crate::value::{ErrorKind, Value};

/// What the matching cells add up to.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Agg {
    /// Cells matched.
    pub count: u64,
    /// The numbers among the values beside them, and how many there were.
    pub sum: f64,
    pub nums: u64,
    /// The first matching error, in range traversal order.
    pub error: Option<ErrorKind>,
}

impl Agg {
    pub fn add(&mut self, value: Option<Value>) {
        self.count += 1;
        match value {
            Some(Value::Num(x)) => {
                self.sum += x;
                self.nums += 1;
            }
            Some(Value::Err(error)) => { self.error.get_or_insert(error); }
            _ => {}
        }
    }
}

/// The ranges a call reads: its criteria ranges and the range it sums.
pub type Shape = (Vec<Rect>, Option<Rect>);

/// Ranges seen in this many calls before they are indexed (a range that moves
/// with the row, `$C$1:C1`, is seen once and never indexed).
const SEEN: u32 = 2;
/// Indexes kept at once on a thread; past it the oldest go.
const MAX_INDEXES: usize = 8;
/// Distinct keys an index may hold before the range is left unindexed.
pub const MAX_KEYS: usize = 4 << 20;

/// The indexes of one group's evaluation, shared by its threads.
#[derive(Default)]
pub struct Shared(Mutex<Cache>);

#[derive(Default)]
struct Cache {
    seen: HashMap<(Shape, bool), u32>,
    built: Vec<((Shape, bool), Option<Arc<Index>>)>,
}

enum Index {
    Eq(HashMap<Vec<Key>, Agg>),
    Sorted { groups: HashMap<Vec<Key>, Sorted>, exact: bool },
}

/// One group's numbers in order, with the counts and sums of the values beside
/// them up to each position.
#[derive(Default)]
struct Sorted {
    xs: Vec<f64>,
    nums: Vec<u64>,
    sums: Vec<f64>,
}

impl Sorted {
    fn agg(&self, bounds: &[(Side, f64)], exact: bool) -> Agg {
        let (mut lo, mut hi) = (0, self.xs.len());
        for &(side, b) in bounds {
            match side {
                Side::Gt => lo = lo.max(self.xs.partition_point(|&x| x <= b)),
                Side::Ge => lo = lo.max(self.xs.partition_point(|&x| x < b)),
                Side::Lt => hi = hi.min(self.xs.partition_point(|&x| x < b)),
                Side::Le => hi = hi.min(self.xs.partition_point(|&x| x <= b)),
            }
        }
        if lo >= hi {
            return Agg::default();
        }
        let sum = if exact { self.sums[hi] - self.sums[lo] } else { f64::NAN };
        Agg { count: (hi - lo) as u64, sum: if sum == 0.0 { 0.0 } else { sum }, nums: self.nums[hi] - self.nums[lo], error: None }
    }
}

/// Past this, sums of whole numbers in any order may round differently.
const EXACT: f64 = 9007199254740992.0;

thread_local! {
    static CACHE: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

/// While held, -IF calls on this thread may index their ranges into `shared`:
/// hold it only while the ranges a group reads can't change.
pub struct Frozen(());

impl Frozen {
    pub fn new(shared: &Arc<Shared>) -> Frozen {
        CACHE.with(|c| *c.borrow_mut() = Some(shared.clone()));
        Frozen(())
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        CACHE.with(|c| *c.borrow_mut() = None);
    }
}

/// The matching cells' totals for `keys` over `shape`, from an index built by
/// `build` (which walks the ranges calling its argument with each matching
/// candidate's keys and value), or None to walk as usual.
pub fn lookup(shape: Shape, keys: &[Key], build: impl FnOnce(&mut dyn FnMut(Vec<Key>, Option<Value>)) -> bool) -> Option<Agg> {
    with_index((shape, false), build_eq(build), |idx| match idx {
        Index::Eq(m) => Some(m.get(keys).copied().unwrap_or_default()),
        Index::Sorted { .. } => None,
    })
}

/// The totals of the cells whose equality keys are `keys` and whose number
/// meets every one of `bounds`, from an index built by `build` (which calls its
/// argument with each candidate's keys, number and value beside it), or None to
/// walk as usual. `sum` says whether the caller needs the sum: without an exact
/// one, an index answers only counts.
pub fn lookup_sorted(shape: Shape, keys: &[Key], bounds: &[(Side, f64)], sum: bool, build: impl FnOnce(&mut dyn FnMut(Vec<Key>, f64, Option<Value>)) -> bool) -> Option<Agg> {
    with_index((shape, true), build_sorted(build), |idx| match idx {
        Index::Sorted { groups, exact } => {
            if sum && !exact {
                return None;
            }
            Some(groups.get(keys).map(|g| g.agg(bounds, *exact)).unwrap_or_default())
        }
        Index::Eq(_) => None,
    })
}

fn build_eq(build: impl FnOnce(&mut dyn FnMut(Vec<Key>, Option<Value>)) -> bool) -> impl FnOnce() -> Option<Index> {
    move || {
        let mut map: HashMap<Vec<Key>, Agg> = HashMap::new();
        let mut over = false;
        let ok = build(&mut |k, v| {
            if over {
                return;
            }
            if map.len() >= MAX_KEYS && !map.contains_key(&k) {
                over = true;
                return;
            }
            map.entry(k).or_default().add(v);
        });
        (ok && !over).then_some(Index::Eq(map))
    }
}

fn build_sorted(build: impl FnOnce(&mut dyn FnMut(Vec<Key>, f64, Option<Value>)) -> bool) -> impl FnOnce() -> Option<Index> {
    move || {
        let mut rows: HashMap<Vec<Key>, Vec<(f64, Option<f64>)>> = HashMap::new();
        let (mut n, mut over, mut exact, mut mag) = (0usize, false, true, 0.0f64);
        let ok = build(&mut |k, x, v| {
            if over {
                return;
            }
            n += 1;
            if n > MAX_KEYS || x.is_nan() {
                over = true;
                return;
            }
            let v = match v {
                Some(Value::Num(v)) => {
                    mag += v.abs();
                    exact &= v.fract() == 0.0 && mag < EXACT;
                    Some(v)
                }
                // Sorting loses the original order of errors. Walk for sums
                // and averages when there is an error in the value range.
                Some(Value::Err(_)) => { exact = false; None }
                _ => None,
            };
            rows.entry(k).or_default().push((x, v));
        });
        if !ok || over {
            return None;
        }
        let groups = rows
            .into_iter()
            .map(|(k, mut r)| {
                r.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut g = Sorted { xs: Vec::with_capacity(r.len()), nums: vec![0], sums: vec![0.0] };
                for (x, v) in r {
                    g.xs.push(x);
                    g.nums.push(g.nums.last().unwrap() + v.is_some() as u64);
                    g.sums.push(if exact { g.sums.last().unwrap() + v.unwrap_or(0.0) } else { 0.0 });
                }
                (k, g)
            })
            .collect();
        Some(Index::Sorted { groups, exact })
    }
}

fn with_index(key: (Shape, bool), build: impl FnOnce() -> Option<Index>, ask: impl FnOnce(&Index) -> Option<Agg>) -> Option<Agg> {
    let shared = CACHE.with(|c| c.borrow().clone())?;
    let mut cache = shared.0.lock().unwrap_or_else(|e| e.into_inner());
    let cache = &mut *cache;
    if let Some((_, idx)) = cache.built.iter().find(|(s, _)| *s == key) {
        return ask(idx.as_ref()?);
    }
    let n = cache.seen.entry(key.clone()).or_insert(0);
    *n += 1;
    if *n < SEEN {
        return None;
    }
    let idx = build().map(Arc::new);
    if cache.built.len() >= MAX_INDEXES {
        cache.built.remove(0);
    }
    let out = idx.as_deref().and_then(ask);
    cache.built.push((key, idx));
    out
}
