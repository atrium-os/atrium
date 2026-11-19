//! Stable identities for grid axes. Untouched coordinates share an implicit
//! run, including a reserve beyond the visible grid so shifts retain identity
//! when they move blank coordinates through the edge.
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};

use officina_core::wire::{Reader, WireError, Writer};

const ORIGINAL: u32 = 1 << 24;
const MAX_RUNS: u64 = 4096;
const MAX_LEN: u32 = ORIGINAL * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub origin: u128,
    pub start: u32,
    pub len: u32,
}

impl Run {
    pub fn inserted(len: u32) -> Self {
        // Two randomized standard hash builders supply a nonce.
        // It identifies a shift across copies and carries no authority.
        let word = || std::collections::hash_map::RandomState::new().build_hasher().finish();
        Self { origin: (u128::from(word()) << 64 | u128::from(word())).max(1), start: 0, len }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AxisIds(Vec<Run>);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Difference {
    pub at: u32,
    pub remove: u32,
    pub added: Vec<Run>,
}

impl AxisIds {
    fn runs(&self) -> &[Run] {
        const DEFAULT: &[Run] = &[Run { origin: 0, start: 0, len: ORIGINAL }];
        if self.0.is_empty() { DEFAULT } else { &self.0 }
    }

    pub(crate) fn is_implicit(&self) -> bool { self.0.is_empty() }

    pub(crate) fn bytes(&self) -> u64 {
        (self.0.capacity() * std::mem::size_of::<Run>()) as u64
    }

    pub(crate) fn splice(&self, at: u32, remove: u32, added: &[Run]) -> Option<Self> {
        let mut next = Vec::new();
        let mut offset = 0;
        let end = at.checked_add(remove)?;
        for &run in self.runs() {
            let before = at.saturating_sub(offset).min(run.len);
            let after = end.saturating_sub(offset).min(run.len);
            if before != 0 { push(&mut next, Run { len: before, ..run }); }
            if offset <= at && at < offset + run.len {
                for &r in added { push(&mut next, r); }
            }
            if after < run.len {
                push(&mut next, Run { start: run.start + after, len: run.len - after, ..run });
            }
            offset += run.len;
        }
        let total = next.iter().map(|r| u64::from(r.len)).sum::<u64>();
        if end >= offset || next.len() as u64 > MAX_RUNS
            || !(u64::from(ORIGINAL / 2)..=u64::from(MAX_LEN)).contains(&total)
            || !ordered(&next)
        {
            return None;
        }
        Some(Self(next))
    }

    /// Match surviving runs by origin and interval. Each gap between matched
    /// runs records exactly which identities went away and which appeared.
    pub(crate) fn differences(&self, other: &Self) -> Option<Vec<Difference>> {
        if self == other { return Some(Vec::new()); }
        let mut by_origin: HashMap<u128, Vec<(u32, Run)>> = HashMap::new();
        let mut side_offset = 0;
        for &r in other.runs() {
            by_origin.entry(r.origin).or_default().push((side_offset, r));
            side_offset += r.len;
        }
        for runs in by_origin.values_mut() { runs.sort_unstable_by_key(|(_, r)| r.start); }
        let mut matches = Vec::new();
        let mut base_offset = 0;
        for &r in self.runs() {
            if let Some(side) = by_origin.get(&r.origin) {
                let i = side.partition_point(|(_, s)| s.start + s.len <= r.start);
                for &(pos, s) in &side[i..] {
                    if s.start >= r.start + r.len { break; }
                    let start = r.start.max(s.start);
                    let len = (r.start + r.len).min(s.start + s.len) - start;
                    matches.push((base_offset + start - r.start, pos + start - s.start, len));
                }
            }
            base_offset += r.len;
        }
        let (mut base, mut side) = (0, 0);
        let mut gaps = Vec::new();
        for (a, b, len) in matches {
            if a < base || b < side { return None; }
            if a != base || b != side {
                gaps.push(Difference { at: base, remove: a - base, added: other.slice(side, b) });
            }
            (base, side) = (a + len, b + len);
        }
        // Every valid layout retains the far tail of the original run.
        (base == base_offset && side == side_offset).then_some(gaps)
    }

    fn slice(&self, start: u32, end: u32) -> Vec<Run> {
        let mut out = Vec::new();
        let mut offset = 0;
        for &r in self.runs() {
            let a = start.saturating_sub(offset).min(r.len);
            let b = end.saturating_sub(offset).min(r.len);
            if a < b { push(&mut out, Run { start: r.start + a, len: b - a, ..r }); }
            offset += r.len;
            if offset >= end { break; }
        }
        out
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        w.var(self.0.len() as u64);
        for r in &self.0 {
            w.u64(r.origin as u64);
            w.u64((r.origin >> 64) as u64);
            w.var(r.start as u64);
            w.var(r.len as u64);
        }
    }

    pub(crate) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let bad = || WireError::Invalid("axis identities");
        let count = r.count("identity runs", MAX_RUNS, 18)?;
        if count == 0 { return Ok(Self::default()); }
        let mut runs = Vec::with_capacity(count);
        let mut total = 0;
        for _ in 0..count {
            let origin = u128::from(r.u64()?) | u128::from(r.u64()?) << 64;
            let start = r.len("identity start", ORIGINAL as u64 - 1)? as u32;
            let len = r.len("identity length", ORIGINAL as u64)? as u32;
            if len == 0 || start + len > ORIGINAL || len > MAX_LEN - total { return Err(bad()); }
            total += len;
            runs.push(Run { origin, start, len });
        }
        if total < ORIGINAL / 2 || !runs.last().is_some_and(|r| r.origin == 0 && r.start + r.len == ORIGINAL)
            || !ordered(&runs)
        {
            return Err(bad());
        }
        Ok(Self(runs))
    }
}

fn ordered(runs: &[Run]) -> bool {
    let mut previous = HashMap::new();
    runs.iter().all(|r| previous.insert(r.origin, r.start + r.len).is_none_or(|end| end <= r.start))
}

fn push(out: &mut Vec<Run>, run: Run) {
    if let Some(previous) = out.last_mut().filter(|p| p.origin == run.origin && p.start + p.len == run.start) {
        previous.len += run.len;
    } else {
        out.push(run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shift::{shift_op, Axis};
    use officina_core::history::Op;

    #[test]
    fn blank_axis_identities_survive_native_and_undo() {
        let mut wb = crate::Workbook::new();
        wb.add_sheet("Blank").unwrap();
        let op = shift_op(&wb, 0, Axis::Rows, 7, 2).unwrap();
        let op = crate::ops::GridOp::decode(&op.encode()).unwrap();
        let inverse = op.apply(&mut wb);
        shift_op(&wb, 0, Axis::Cols, 13, -3).unwrap().apply(&mut wb);
        let saved = crate::native::save_to_vec(&wb);
        let mut reopened = crate::native::load(&saved).unwrap();
        assert_eq!(reopened.sheet(0).axis_ids, wb.sheet(0).axis_ids);
        let [rows, cols] = &reopened.sheet(0).axis_ids;
        assert_eq!(AxisIds::default().differences(rows).unwrap()[0].at, 7);
        assert_eq!(AxisIds::default().differences(cols).unwrap(), vec![
            Difference { at: 13, remove: 3, added: Vec::new() },
        ]);
        let redo = inverse.apply(&mut reopened);
        assert!(reopened.sheet(0).axis_ids[0].is_implicit());
        redo.apply(&mut reopened);
        assert_eq!(reopened.sheet(0).axis_ids, wb.sheet(0).axis_ids);
    }
}
