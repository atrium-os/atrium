//! Auto-filter: a header row over a range, conditions per column, and the rows that
//! fail them hidden. Hidden rows are per sheet ranges (a filter's, or hidden by
//! hand); the view's row sizes treat them as zero high ([`Sheet::view_row_sizes`]).
//!
//! Setting or changing a filter is one undoable op ([`GridOp::Filter`]) that swaps the
//! sheet's filter and hidden rows.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_core::wire::{Reader, WireError, Writer};
use crate::book::{Sheet, Workbook};
use crate::column::Sizes;
use crate::eval::funcs::text_key;
use crate::value::Value;

/// What a filtered column keeps.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// Cells showing one of these texts (as formatted; compared ignoring case). An
    /// empty string keeps empty cells.
    Values(Vec<String>),
    /// Numbers compared with a constant: (operator, value), `op` one of
    /// `= <> < <= > >=`; two joined by and (`true`) or or.
    Compare(Vec<(String, f64)>, bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoFilter {
    /// The range, its first row the header (with the filter buttons).
    pub rect: Rect,
    /// Conditions by column of the sheet (inside the range).
    pub conds: Vec<(u32, Cond)>,
}

/// A sheet's filter and hidden rows, as a [`GridOp::Filter`] carries them.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterState {
    pub sheet: u16,
    pub filter: Option<AutoFilter>,
    pub hidden: Vec<(u32, u32)>,
}

impl FilterState {
    /// As journal or file bytes (after the sheet).
    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.sheet);
        w.var(self.hidden.len() as u64);
        for &(a, b) in &self.hidden {
            w.var(a as u64);
            w.var((b - a) as u64);
        }
        match &self.filter {
            None => w.u8(0),
            Some(f) => {
                w.u8(1);
                for x in [f.rect.r0, f.rect.c0, f.rect.r1, f.rect.c1] {
                    w.var(x as u64);
                }
                w.var(f.conds.len() as u64);
                for (c, cond) in &f.conds {
                    w.var(*c as u64);
                    match cond {
                        Cond::Values(v) => {
                            w.u8(0);
                            w.var(v.len() as u64);
                            for s in v {
                                w.str(s);
                            }
                        }
                        Cond::Compare(cs, and) => {
                            w.u8(if *and { 2 } else { 1 });
                            w.var(cs.len() as u64);
                            for (op, x) in cs {
                                w.str(op);
                                w.f64(*x);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Reads [`FilterState::encode`]; bounded, never panics.
    pub fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let sheet = r.u16()?;
        let n = r.count("hidden ranges", MAX_ROWS as u64, 2)?;
        let mut hidden = Vec::with_capacity(n);
        let mut next = 0u64;
        for _ in 0..n {
            let a = r.len("row", MAX_ROWS as u64 - 1)? as u64;
            let len = r.len("rows", MAX_ROWS as u64 - 1)? as u64;
            if a < next || a + len >= MAX_ROWS as u64 {
                return Err(WireError::Invalid("hidden rows out of order"));
            }
            hidden.push((a as u32, (a + len) as u32));
            next = a + len + 2;
        }
        let filter = match r.u8()? {
            0 => None,
            1 => {
                let row = |r: &mut Reader| r.len("row", MAX_ROWS as u64 - 1).map(|x| x as u32);
                let col = |r: &mut Reader| r.len("column", MAX_COLS as u64 - 1).map(|x| x as u32);
                let (r0, c0, r1, c1) = (row(r)?, col(r)?, row(r)?, col(r)?);
                if r0 > r1 || c0 > c1 {
                    return Err(WireError::Invalid("filter range"));
                }
                let k = r.count("filter columns", (c1 - c0 + 1) as u64, 2)?;
                let mut conds = Vec::with_capacity(k);
                for _ in 0..k {
                    let c = col(r)?;
                    if c < c0 || c > c1 {
                        return Err(WireError::Invalid("filter column outside its range"));
                    }
                    let cond = match r.u8()? {
                        0 => {
                            let m = r.count("filter values", 1 << 20, 1)?;
                            let mut v = Vec::with_capacity(m);
                            for _ in 0..m {
                                v.push(r.str("filter value", crate::strings::MAX_TEXT as u64)?.to_string());
                            }
                            Cond::Values(v)
                        }
                        t @ (1 | 2) => {
                            let m = r.count("filter tests", 2, 9)?;
                            let mut cs = Vec::with_capacity(m);
                            for _ in 0..m {
                                let op = r.str("operator", 2)?;
                                if !["=", "<>", "<", "<=", ">", ">="].contains(&op) {
                                    return Err(WireError::Invalid("filter operator"));
                                }
                                let x = r.f64()?;
                                if !x.is_finite() {
                                    return Err(WireError::Invalid("filter number"));
                                }
                                cs.push((op.to_string(), x));
                            }
                            Cond::Compare(cs, t == 2)
                        }
                        _ => return Err(WireError::Invalid("filter kind")),
                    };
                    conds.push((c, cond));
                }
                Some(AutoFilter { rect: Rect { sheet, r0, c0, r1, c1 }, conds })
            }
            _ => return Err(WireError::Invalid("filter flag")),
        };
        Ok(FilterState { sheet, filter, hidden })
    }
}

impl Cond {
    fn keeps(&self, shown: &str, v: &Value) -> bool {
        match self {
            Cond::Values(vals) => {
                let k = text_key(shown);
                vals.iter().any(|x| text_key(x) == k)
            }
            Cond::Compare(cs, and) => {
                let Value::Num(x) = v else { return false };
                let test = |(op, y): &(String, f64)| match op.as_str() {
                    "=" => x == y,
                    "<>" => x != y,
                    "<" => x < y,
                    "<=" => x <= y,
                    ">" => x > y,
                    ">=" => x >= y,
                    _ => true,
                };
                if *and {
                    cs.iter().all(test)
                } else {
                    cs.iter().any(test)
                }
            }
        }
    }
}

/// Merges sorted row ranges, joining touching ones.
pub fn merge(mut v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    v.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(l) if a <= l.1.saturating_add(1) => l.1 = l.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

impl Sheet {
    /// Whether row `r` is hidden.
    pub fn row_hidden(&self, r: u32) -> bool {
        let k = self.hidden.partition_point(|h| h.1 < r);
        self.hidden.get(k).is_some_and(|h| h.0 <= r)
    }

    /// Row sizes as the view lays them out: hidden rows zero high. O(overrides +
    /// hidden ranges); the app keeps it until rows or sizes change.
    pub fn view_row_sizes(&self) -> Sizes {
        if self.hidden.is_empty() {
            return self.row_sizes.clone();
        }
        let mut over: Vec<(u32, f64)> = self.row_sizes.overrides().iter().copied().filter(|(r, _)| !self.row_hidden(*r)).collect();
        for &(a, b) in &self.hidden {
            over.extend((a..=b).map(|r| (r, 0.0)));
        }
        Sizes::from_overrides(self.row_sizes.default, over)
    }
}

impl Workbook {
    /// The sheet's filter and hidden rows now.
    pub fn filter_state(&self, sheet: u16) -> FilterState {
        let sh = self.sheet(sheet);
        FilterState { sheet, filter: sh.filter.clone(), hidden: sh.hidden.clone() }
    }

    /// Replaces the sheet's filter and hidden rows; returns what they were.
    pub fn replace_filter(&mut self, st: FilterState) -> FilterState {
        if st.sheet as usize >= self.sheet_count() {
            return st;
        }
        let old = self.filter_state(st.sheet);
        let sh = self.sheet_mut(st.sheet);
        sh.filter = st.filter;
        sh.hidden = merge(st.hidden);
        old
    }

    /// The state `filter` leaves the sheet in: rows of its range (below the header)
    /// failing a condition hidden, rows hidden outside the range kept hidden.
    pub fn filtered(&self, sheet: u16, filter: Option<AutoFilter>) -> FilterState {
        let sh = self.sheet(sheet);
        // Rows hidden outside the old filter's range stay hidden by hand.
        let old = sh.filter.as_ref().map(|f| (f.rect.r0 + 1, f.rect.r1));
        let mut hidden: Vec<(u32, u32)> = Vec::new();
        for &(a, b) in &sh.hidden {
            match old {
                Some((x, y)) if a <= y && b >= x => {
                    if a < x {
                        hidden.push((a, x - 1));
                    }
                    if b > y {
                        hidden.push((y + 1, b));
                    }
                }
                _ => hidden.push((a, b)),
            }
        }
        if let Some(f) = &filter {
            let mut run: Option<(u32, u32)> = None;
            for r in f.rect.r0 + 1..=f.rect.r1 {
                let keep = f.conds.iter().all(|(c, cond)| {
                    let a = CellAddr::new(sheet, r, *c);
                    cond.keeps(&self.formatted(a).text, &self.get(a))
                });
                if !keep {
                    run = match run {
                        Some((a, b)) if b + 1 == r => Some((a, r)),
                        Some(x) => {
                            hidden.push(x);
                            Some((r, r))
                        }
                        None => Some((r, r)),
                    };
                }
            }
            hidden.extend(run);
        }
        FilterState { sheet, filter, hidden: merge(hidden) }
    }

    /// The values a filter column offers to pick from: distinct shown texts below the
    /// header, sorted, with how many rows show each (at most `max`).
    pub fn filter_values(&self, rect: &Rect, col: u32, max: usize) -> Vec<(String, u32)> {
        let mut m: std::collections::BTreeMap<String, (String, u32)> = std::collections::BTreeMap::new();
        for r in rect.r0 + 1..=rect.r1 {
            let t = self.formatted(CellAddr::new(rect.sheet, r, col)).text;
            let e = m.entry(text_key(&t)).or_insert((t, 0));
            e.1 += 1;
            if m.len() > max {
                break;
            }
        }
        m.into_values().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::GridOp;
    use officina_core::history::Op;

    #[test]
    fn filter_hides_and_undo_shows() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_input(CellAddr::new(0, 0, 0), "Fruit").unwrap();
        wb.set_input(CellAddr::new(0, 0, 1), "Count").unwrap();
        for (r, (f, n)) in [("apple", 3), ("Pear", 10), ("apple", 7), ("fig", 1), ("pear", 4)].iter().enumerate() {
            wb.set_input(CellAddr::new(0, r as u32 + 1, 0), f).unwrap();
            wb.set_value(CellAddr::new(0, r as u32 + 1, 1), Value::Num(*n as f64));
        }
        // A row hidden by hand, outside the range.
        wb.sheet_mut(0).hidden = vec![(20, 22)];
        let rect = Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 1 };
        assert_eq!(wb.filter_values(&rect, 0, 100), [("apple".to_string(), 2), ("fig".to_string(), 1), ("Pear".to_string(), 2)]);
        let f = AutoFilter { rect, conds: vec![(0, Cond::Values(vec!["pear".into(), "apple".into()])), (1, Cond::Compare(vec![(">".into(), 3.0)], true))] };
        let op = GridOp::Filter(wb.filtered(0, Some(f.clone())));
        let undo = op.apply(&mut wb);
        assert_eq!(wb.sheet(0).hidden, [(1, 1), (4, 4), (20, 22)]);
        assert_eq!(wb.sheet(0).filter.as_ref(), Some(&f));
        let v = wb.sheet(0).view_row_sizes();
        assert_eq!(v.offset(5) - v.offset(1), 2.0 * 20.0);
        assert!(wb.sheet(0).row_hidden(21) && !wb.sheet(0).row_hidden(2));
        // The native file keeps both.
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.filter_state(0), wb.filter_state(0));
        // Clearing the filter keeps the hand-hidden rows.
        let st = wb.filtered(0, None);
        assert_eq!(st.hidden, [(20, 22)]);
        let bytes = undo.encode();
        GridOp::decode(&bytes).unwrap().apply(&mut wb);
        assert_eq!(wb.sheet(0).hidden, [(20, 22)]);
        assert!(wb.sheet(0).filter.is_none());
    }
}
