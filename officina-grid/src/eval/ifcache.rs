//! An index of the -IF functions' ranges while a formula group is evaluated.
//!
//! `=SUMIF(C:C, C1, B:B)` filled down n rows walks column C n times: n² cell reads,
//! minutes at 100,000 rows. While a group's rows are evaluated against a workbook
//! that can't change (the parallel path of recalculation), a range read with an
//! equality criterion is walked once into a map from the cells' keys to their
//! count and sums, and every later row is a lookup. Sums are added in the order
//! the walk would add them, so results are the same to the last bit.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use super::criteria::Key;
use crate::addr::Rect;

/// What the matching cells add up to.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Agg {
    /// Cells matched.
    pub count: u64,
    /// The numbers among the values beside them, and how many there were.
    pub sum: f64,
    pub nums: u64,
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

#[derive(Default)]
struct Cache {
    seen: HashMap<Shape, u32>,
    built: Vec<(Shape, Option<Arc<HashMap<Vec<Key>, Agg>>>)>,
}

thread_local! {
    static CACHE: RefCell<Option<Cache>> = const { RefCell::new(None) };
}

/// While held, -IF calls on this thread may index their ranges: hold it only
/// while the workbook can't change.
pub struct Frozen(());

impl Frozen {
    pub fn new() -> Frozen {
        CACHE.with(|c| *c.borrow_mut() = Some(Cache::default()));
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
pub fn lookup(shape: Shape, keys: &[Key], build: impl FnOnce(&mut dyn FnMut(Vec<Key>, Option<f64>)) -> bool) -> Option<Agg> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let cache = c.as_mut()?;
        if let Some((_, idx)) = cache.built.iter().find(|(s, _)| *s == shape) {
            let idx = idx.as_ref()?;
            return Some(idx.get(keys).copied().unwrap_or_default());
        }
        let n = cache.seen.entry(shape.clone()).or_insert(0);
        *n += 1;
        if *n < SEEN {
            return None;
        }
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
            let a = map.entry(k).or_default();
            a.count += 1;
            if let Some(x) = v {
                a.sum += x;
                a.nums += 1;
            }
        });
        if cache.built.len() >= MAX_INDEXES {
            cache.built.remove(0);
        }
        let idx = (ok && !over).then(|| Arc::new(map));
        let out = idx.as_ref().map(|m| m.get(keys).copied().unwrap_or_default());
        cache.built.push((shape, idx));
        out
    })
}
