//! Spilling dynamic arrays (docs/spec/officina.md §16.31).
//!
//! A formula whose result has more than one cell spills it: the anchor (the formula's
//! cell) shows the first value and the rest are written into the cells to its right and
//! below as plain values. Recalculation evaluates formulas as usual and collects the
//! arrays; between rounds [`Workbook::apply_spills`] writes them, or shows `#SPILL!`
//! when a cell in the way holds anything of its own (a value, a formula, another
//! spill). Writing a spill changes cells, so their readers recalculate in another
//! round. Edits that touch a spill's area, or the area a blocked spill wants,
//! re-evaluate its anchor.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::eval::Array;
use crate::value::{ErrorKind, Value};
use std::sync::Arc;

/// A spill as written: its area (anchor included) and the array.
#[derive(Debug, Clone)]
pub struct Spill {
    pub area: Rect,
    pub arr: Arc<Array>,
}

/// Rounds of recalculation one edit may take for spills to settle.
pub const MAX_SPILL_ROUNDS: usize = 8;

/// The value a spilled cell holds: an empty element shows 0, as in Excel.
fn shown(v: &Value) -> Value {
    match v {
        Value::Empty => Value::Num(0.0),
        Value::Num(x) => Value::num(*x),
        v => v.clone(),
    }
}

impl Spill {
    fn value_at(&self, row: u32, col: u32) -> Value {
        let (r, c) = ((row - self.area.r0) as usize, (col - self.area.c0) as usize);
        shown(&self.arr.vals[r * self.arr.cols + c])
    }
}

impl Workbook {
    /// The area a dynamic array anchored at `a` spills into (anchor included); a
    /// formula there with a one-cell result gives just its cell. `None`: no formula,
    /// or its spill is blocked.
    pub fn spill_area(&self, a: CellAddr) -> Option<Rect> {
        if let Some(s) = self.spills.get(&a) {
            return Some(s.area);
        }
        if self.blocked.contains_key(&a) {
            return None;
        }
        self.group_at(a.sheet, a.row, a.col).map(|_| Rect::cell(a))
    }

    /// The anchor of the spill a cell is part of (not the anchor itself).
    pub fn spilled_from(&self, a: CellAddr) -> Option<CellAddr> {
        self.spills.iter().find(|(k, s)| **k != a && s.area.contains(a)).map(|(k, _)| *k)
    }

    /// Every spill, by anchor.
    pub fn spills(&self) -> impl Iterator<Item = (&CellAddr, &Spill)> {
        self.spills.iter()
    }

    /// Says that a file holds the spill of the formula at `anchor` over `area` (its
    /// cached values): the next recalculation takes those cells as the spill's own
    /// rather than as something in its way.
    pub fn claim_spill(&mut self, anchor: CellAddr, area: Rect) {
        self.claims.insert(anchor, area);
    }

    /// Whether the cell had a spill or a blocked one at the last recalculation.
    pub(crate) fn spill_known(&self, a: &CellAddr) -> bool {
        (!self.spills.is_empty() && self.spills.contains_key(a)) || (!self.blocked.is_empty() && self.blocked.contains_key(a))
    }

    pub(crate) fn spill_result(&self, a: CellAddr, arr: Option<Arc<Array>>) {
        self.spill_out.lock().unwrap().push((a, arr));
    }

    /// Before a recalculation: anchors whose spill area (or wanted area) was edited
    /// are evaluated again, and spills whose formula is gone are cleared.
    pub(crate) fn spill_edits(&mut self) {
        if self.spills.is_empty() && self.blocked.is_empty() {
            return;
        }
        let gone: Vec<CellAddr> = self.spills.keys().chain(self.blocked.keys()).filter(|a| self.group_at(a.sheet, a.row, a.col).is_none()).copied().collect();
        for a in gone {
            self.blocked.remove(&a);
            if let Some(s) = self.spills.remove(&a) {
                self.clear_spill(a, &s);
                self.changed.push(s.area);
            }
        }
        let mut again = Vec::new();
        for r in &self.changed {
            for (a, s) in &self.spills {
                if s.area.intersects(r) && !(r.cells() == 1 && Rect::cell(*a) == *r) {
                    again.push(*a);
                }
            }
            for (a, area) in &self.blocked {
                if area.intersects(r) {
                    again.push(*a);
                }
            }
        }
        for a in again {
            if let Some(g) = self.group_at(a.sheet, a.row, a.col) {
                self.pending.entry(g).or_default().add(a.row, a.row);
            }
        }
    }

    /// Clears the cells of a spill that still hold its values (not the anchor).
    fn clear_spill(&mut self, a: CellAddr, s: &Spill) {
        let mut own = Vec::new();
        self.each(&s.area, &mut |row, col, v| {
            if (row, col) != (a.row, a.col) && v == s.value_at(row, col) {
                own.push((row, col));
            }
        });
        for (row, col) in own {
            self.store(s.area.sheet, row, col, &Value::Empty);
        }
    }

    /// Whether anything of its own is in the way of a spill over `area`: a value that
    /// isn't this spill's own (from `old`), or a formula.
    fn spill_blocked(&self, a: CellAddr, area: &Rect, old: Option<&Spill>) -> bool {
        let mut hit = false;
        self.each(area, &mut |row, col, v| {
            if hit || (row, col) == (a.row, a.col) {
                return;
            }
            let at = CellAddr::new(a.sheet, row, col);
            let own = match old {
                Some(o) => o.area.contains(at) && o.value_at(row, col) == v,
                None => self.claims.get(&a).is_some_and(|c| c.contains(at)),
            };
            hit |= !v.is_empty() && !own;
        });
        if hit {
            return true;
        }
        let Some(sh) = self.sheets.get(area.sheet as usize) else { return true };
        if sh.fidx.is_empty() {
            return false;
        }
        for c in area.c0..=area.c1.min(sh.fidx.len().saturating_sub(1) as u32) {
            let f = &sh.fidx[c as usize];
            for (_, &g) in f.range(..=area.r1).rev() {
                let grp = self.group(g);
                if grp.r1 < area.r0 {
                    break;
                }
                // The anchor's own cell is the only formula allowed.
                let (lo, hi) = (grp.r0.max(area.r0), grp.r1.min(area.r1));
                if lo <= hi && (c != a.col || lo < hi || lo != a.row) {
                    return true;
                }
            }
        }
        false
    }

    /// Clears the cells a file said a spill holds, but those in `keep`.
    fn drop_claim(&mut self, a: CellAddr, c: Rect, keep: Option<Rect>) {
        let mut cells = Vec::new();
        self.each(&c, &mut |row, col, _| {
            let at = CellAddr::new(a.sheet, row, col);
            if at != a && !keep.is_some_and(|k| k.contains(at)) {
                cells.push(at);
            }
        });
        for at in cells {
            if self.group_at(at.sheet, at.row, at.col).is_none() {
                self.store(at.sheet, at.row, at.col, &Value::Empty);
            }
        }
        self.changed.push(c);
    }

    /// Writes the arrays the last round produced; true when that changed any cell
    /// (their readers then need another round).
    pub(crate) fn apply_spills(&mut self) -> bool {
        let out = std::mem::take(&mut *self.spill_out.lock().unwrap());
        let mut any = false;
        for (a, arr) in out {
            if self.group_at(a.sheet, a.row, a.col).is_none() {
                continue;
            }
            let old = self.spills.get(&a).cloned();
            let claim = self.claims.remove(&a);
            let was_blocked = self.blocked.get(&a).copied();
            let Some(arr) = arr else {
                // A one-cell result now: the old spill goes.
                if let Some(c) = claim {
                    self.drop_claim(a, c, None);
                    any = true;
                }
                self.blocked.remove(&a);
                if let Some(o) = self.spills.remove(&a) {
                    self.clear_spill(a, &o);
                    self.changed.push(o.area);
                    any = true;
                }
                continue;
            };
            let fits = a.row as u64 + arr.rows as u64 <= MAX_ROWS as u64 && a.col as u64 + arr.cols as u64 <= MAX_COLS as u64;
            let area = Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: (a.row as u64 + arr.rows as u64 - 1).min(MAX_ROWS as u64 - 1) as u32, c1: (a.col as u64 + arr.cols as u64 - 1).min(MAX_COLS as u64 - 1) as u32 };
            if let Some(c) = claim {
                self.claims.insert(a, c);
            }
            let blocked = !fits || self.spill_blocked(a, &area, old.as_ref());
            // A file's spill cells outside what spills now are stale.
            if let (Some(c), None) = (self.claims.remove(&a), &old) {
                self.drop_claim(a, c, (!blocked).then_some(area));
                any = true;
            }
            if blocked {
                if let Some(o) = self.spills.remove(&a) {
                    self.clear_spill(a, &o);
                    self.changed.push(o.area);
                    any = true;
                }
                self.store(a.sheet, a.row, a.col, &Value::Err(ErrorKind::Spill));
                self.blocked.insert(a, area);
                if was_blocked != Some(area) || old.is_some() {
                    self.changed.push(Rect::cell(a));
                    any = true;
                }
                continue;
            }
            self.blocked.remove(&a);
            let new = Spill { area, arr };
            let same = old.as_ref().is_some_and(|o| o.area == new.area && o.arr.vals == new.arr.vals);
            if let (Some(o), false) = (&old, same) {
                self.clear_spill(a, o);
                self.changed.push(o.area);
            }
            // Write what differs (an edit may have cleared a spilled cell).
            let mut wrote = false;
            for r in area.r0..=area.r1 {
                for c in area.c0..=area.c1 {
                    if (r, c) == (a.row, a.col) {
                        continue;
                    }
                    let v = new.value_at(r, c);
                    if self.value(a.sheet, r, c) != v {
                        self.store(a.sheet, r, c, &v);
                        wrote = true;
                    }
                }
            }
            if !same || wrote || was_blocked.is_some() {
                self.changed.push(area);
                any = true;
            }
            self.spills.insert(a, new);
        }
        any
    }
}
