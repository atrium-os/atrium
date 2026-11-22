//! Sorting a range by columns, as Excel sorts: one undoable edit that moves whole rows
//! of the range (values, formulas and styles).
//!
//! Order, ascending: numbers, text (ignoring case), logicals (FALSE before TRUE),
//! errors (kept in their order); descending reverses that; empty cells go last either
//! way. The sort is stable. A moved formula is copied, as Excel does: its relative
//! references move with it.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::addr::{CellAddr, Rect};
use crate::book::Workbook;
use crate::eval::funcs::text_key;
use crate::formula::{self, Expr};
use crate::ops::{Fill, GridOp, Region, StyleRegion};
use crate::style::StyleId;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    /// A column of the sheet (inside the range).
    pub col: u32,
    pub descending: bool,
}

/// What a cell sorts by.
#[derive(Debug, Clone, PartialEq)]
enum K {
    Num(f64),
    Text(String),
    Bool(bool),
    Err,
    Empty,
}

impl K {
    fn of(v: &Value) -> K {
        match v {
            Value::Num(x) => K::Num(*x),
            Value::Str(s) if s.is_empty() => K::Empty,
            Value::Str(s) => K::Text(text_key(s)),
            Value::Bool(b) => K::Bool(*b),
            Value::Err(_) => K::Err,
            Value::Empty => K::Empty,
        }
    }
    fn rank(&self) -> u8 {
        match self {
            K::Num(_) => 0,
            K::Text(_) => 1,
            K::Bool(_) => 2,
            K::Err => 3,
            K::Empty => 4,
        }
    }
}

fn cmp(a: &K, b: &K, desc: bool) -> Ordering {
    match (a, b) {
        (K::Empty, K::Empty) => Ordering::Equal,
        (K::Empty, _) => Ordering::Greater,
        (_, K::Empty) => Ordering::Less,
        _ => {
            let o = match (a, b) {
                (K::Num(x), K::Num(y)) => x.total_cmp(y),
                (K::Text(x), K::Text(y)) => x.cmp(y),
                (K::Bool(x), K::Bool(y)) => x.cmp(y),
                _ => a.rank().cmp(&b.rank()),
            };
            if desc {
                o.reverse()
            } else {
                o
            }
        }
    }
}

/// The new order of the rows `rect.r0..=rect.r1`: `order[i]` is the row that goes to
/// row `rect.r0 + i`.
pub fn order(wb: &Workbook, rect: &Rect, keys: &[SortKey]) -> Vec<u32> {
    let rows: Vec<u32> = (rect.r0..=rect.r1).collect();
    let cols: Vec<Vec<K>> = keys.iter().map(|k| rows.iter().map(|&r| K::of(&wb.get(CellAddr::new(rect.sheet, r, k.col)))).collect()).collect();
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|&a, &b| {
        for (k, key) in keys.iter().enumerate() {
            let o = cmp(&cols[k][a], &cols[k][b], key.descending);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    idx.into_iter().map(|i| rows[i]).collect()
}

/// The edit that sorts `rect` by `keys` (the caller leaves a header row out of
/// `rect`). `None` when the rows are already in order or a key is outside the range.
/// Whether `rect`'s rows can be sorted with the merged cells in it, as
/// Excel allows: none, or every row merged across the same columns (one row
/// high each, all inside the range), so the merges stay where they are.
pub fn merges_allow_sort(wb: &Workbook, rect: &Rect) -> bool {
    let mut first: Option<Vec<(u32, u32)>> = None;
    let mut row = rect.r0;
    let mut spans: Vec<(u32, u32)> = Vec::new();
    let mut rows = 0u64;
    let done = |spans: &mut Vec<(u32, u32)>, first: &mut Option<Vec<(u32, u32)>>| -> bool {
        spans.sort_unstable();
        match first {
            None => {
                *first = Some(std::mem::take(spans));
                true
            }
            Some(f) => {
                let same = f == spans;
                spans.clear();
                same
            }
        }
    };
    let mut any = false;
    for m in wb.merges_in(rect) {
        any = true;
        if m.r0 != m.r1 || m.r0 < rect.r0 || m.r1 > rect.r1 || m.c0 < rect.c0 || m.c1 > rect.c1 {
            return false;
        }
        if m.r0 != row {
            if m.r0 != row + 1 || spans.is_empty() || !done(&mut spans, &mut first) {
                return false;
            }
            rows += 1;
            row = m.r0;
        }
        spans.push((m.c0, m.c1));
    }
    if !any {
        return true;
    }
    if row != rect.r1 || !done(&mut spans, &mut first) {
        return false;
    }
    rows + 1 == (rect.r1 - rect.r0 + 1) as u64
}

pub fn sort_op(wb: &Workbook, rect: Rect, keys: &[SortKey]) -> Option<GridOp> {
    if keys.is_empty() || keys.iter().any(|k| k.col < rect.c0 || k.col > rect.c1) || rect.sheet as usize >= wb.sheet_count() {
        return None;
    }
    let ord = order(wb, &rect, keys);
    if ord.iter().enumerate().all(|(i, &r)| r == rect.r0 + i as u32) {
        return None;
    }
    let s = rect.sheet;
    let mut values = Vec::new();
    let mut fills: Vec<Fill> = Vec::new();
    // Per column, the last fill and the expression it came from, to extend runs.
    let mut last: Vec<Option<(usize, Arc<Expr>)>> = vec![None; (rect.c1 - rect.c0 + 1) as usize];
    for (i, &src) in ord.iter().enumerate() {
        let dest = rect.r0 + i as u32;
        for c in rect.c0..=rect.c1 {
            let slot = &mut last[(c - rect.c0) as usize];
            if let Some(g) = wb.group_at(s, src, c) {
                let e = wb.group(g).expr.clone();
                // Same relative expression on the row above: the fill goes on.
                if let Some((k, pe)) = slot {
                    if fills[*k].r1 + 1 == dest && (Arc::ptr_eq(pe, &e) || **pe == *e) {
                        fills[*k].r1 = dest;
                        continue;
                    }
                }
                let text = formula::to_a1(&e, dest, c, &|id| wb.sheet_name(id));
                fills.push(Fill { col: c, r0: dest, r1: dest, src: text });
                *slot = Some((fills.len() - 1, e));
            } else {
                *slot = None;
                let v = wb.value(s, src, c);
                if !v.is_empty() {
                    values.push((dest, c, v));
                }
            }
        }
    }
    let mut ops = vec![GridOp::Replace(Region { rect, values, fills })];
    // Styles, if the range has any.
    let snap = wb.style_snapshot(&rect);
    if !snap.spans.is_empty() {
        let sh = wb.sheet(s);
        let mut spans = Vec::new();
        for c in rect.c0..=rect.c1 {
            let ids: Vec<StyleId> = ord.iter().map(|&r| sh.style_id(r, c)).collect();
            let mut i = 0;
            while i < ids.len() {
                let mut j = i + 1;
                while j < ids.len() && ids[j] == ids[i] {
                    j += 1;
                }
                if ids[i] != 0 {
                    spans.push((c, rect.r0 + i as u32, rect.r0 + j as u32 - 1, wb.styles.get(ids[i]).clone()));
                }
                i = j;
            }
        }
        ops.push(GridOp::Restyle(StyleRegion { rect, spans }));
    }
    // Comments and links go with their rows.
    let mut dest_of = std::collections::HashMap::with_capacity(ord.len());
    for (i, &src) in ord.iter().enumerate() {
        dest_of.insert(src, rect.r0 + i as u32);
    }
    let to = |r: u32, c: u32| Some(if (rect.r0..=rect.r1).contains(&r) && (rect.c0..=rect.c1).contains(&c) { (dest_of.get(&r).copied().unwrap_or(r), c) } else { (r, c) });
    let set = crate::links::moved(wb.comments(s), to);
    if !set.is_empty() {
        ops.push(GridOp::Comments { sheet: s, set });
    }
    let set = crate::links::moved(wb.links(s), to);
    if !set.is_empty() {
        ops.push(GridOp::Links { sheet: s, set });
    }
    Some(if ops.len() == 1 { ops.pop().unwrap() } else { GridOp::Batch(ops) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Style;
    use officina_core::history::Op;

    fn col(wb: &Workbook, c: u32, n: u32) -> Vec<String> {
        (0..n).map(|r| wb.get(CellAddr::new(0, r, c)).display()).collect()
    }

    #[test]
    fn excel_order_and_undo() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let vals = ["b", "10", "", "TRUE", "A", "#N/A", "2", "FALSE", "a"];
        for (r, v) in vals.iter().enumerate() {
            wb.set_input(CellAddr::new(0, r as u32, 0), v).unwrap();
            wb.set_value(CellAddr::new(0, r as u32, 1), Value::Num(r as f64));
            // A formula reading its own row's B, copied with its row.
            wb.set_formula(CellAddr::new(0, r as u32, 2), &format!("=B{}*10", r + 1)).unwrap();
        }
        wb.set_style(&Rect { sheet: 0, r0: 1, c0: 0, r1: 1, c1: 0 }, Style::default().with_num_fmt("0.00"));
        wb.recalc();
        let rect = Rect { sheet: 0, r0: 0, c0: 0, r1: 8, c1: 2 };
        let before: Vec<Vec<String>> = (0..3).map(|c| col(&wb, c, 9)).collect();
        let op = sort_op(&wb, rect, &[SortKey { col: 0, descending: false }]).unwrap();
        let undo = op.apply(&mut wb);
        wb.recalc();
        assert_eq!(col(&wb, 0, 9), ["2", "10", "A", "a", "b", "FALSE", "TRUE", "#N/A", ""]);
        // Rows moved whole: B and C came along, C still reading its own row.
        assert_eq!(col(&wb, 1, 9), ["6", "1", "4", "8", "0", "7", "3", "5", "2"]);
        assert_eq!(col(&wb, 2, 9), ["60", "10", "40", "80", "0", "70", "30", "50", "20"]);
        assert_eq!(wb.formula_text(CellAddr::new(0, 0, 2)).unwrap(), "=B1*10");
        // Undo puts everything back.
        undo.apply(&mut wb);
        wb.recalc();
        assert_eq!((0..3).map(|c| col(&wb, c, 9)).collect::<Vec<_>>(), before);
        assert_eq!(wb.style(CellAddr::new(0, 1, 0)).num_fmt.as_ref(), "0.00");
        // Descending, empties still last; a second key breaks ties.
        let op = sort_op(&wb, rect, &[SortKey { col: 0, descending: true }]).unwrap();
        op.apply(&mut wb);
        wb.recalc();
        assert_eq!(col(&wb, 0, 9), ["#N/A", "TRUE", "FALSE", "b", "A", "a", "10", "2", ""]);
        // The style moved with its cell.
        assert_eq!(wb.style(CellAddr::new(0, 6, 0)).num_fmt.as_ref(), "0.00");
        assert_eq!(wb.style(CellAddr::new(0, 1, 0)).num_fmt.as_ref(), "General");
    }

    #[test]
    fn two_keys_preserve_the_order_of_exact_ties() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        for (row, (region, rank)) in [("South", 2.0), ("North", 1.0), ("North", 2.0), ("North", 2.0), ("South", 3.0)].into_iter().enumerate() {
            wb.set_value(CellAddr::new(0, row as u32, 0), Value::str(region));
            wb.set_value(CellAddr::new(0, row as u32, 1), Value::Num(rank));
        }
        let rect = Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 1 };
        let keys = [SortKey { col: 0, descending: false }, SortKey { col: 1, descending: true }];
        assert_eq!(order(&wb, &rect, &keys), [2, 3, 1, 4, 0]);
    }

    #[test]
    fn a_sorted_fill_stays_one_fill() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        for r in 0..1000u32 {
            wb.set_value(CellAddr::new(0, r, 0), Value::Num(((r * 7919) % 1000) as f64));
        }
        wb.fill_formula(0, 1, 0, 999, "=A1*2").unwrap();
        wb.recalc();
        let op = sort_op(&wb, Rect { sheet: 0, r0: 0, c0: 0, r1: 999, c1: 1 }, &[SortKey { col: 0, descending: false }]).unwrap();
        let GridOp::Replace(ref region) = op else { panic!("no styles, one region") };
        assert_eq!(region.fills.len(), 1);
        let bytes = op.encode();
        let back = GridOp::decode(&bytes).unwrap();
        back.apply(&mut wb);
        wb.recalc();
        for r in 0..1000u32 {
            assert_eq!(wb.get(CellAddr::new(0, r, 0)), Value::Num(r as f64));
            assert_eq!(wb.get(CellAddr::new(0, r, 1)), Value::Num(2.0 * r as f64));
        }
        assert!(sort_op(&wb, Rect { sheet: 0, r0: 0, c0: 0, r1: 999, c1: 1 }, &[SortKey { col: 0, descending: false }]).is_none());
    }

    #[test]
    fn comments_move_with_their_rows() {
        use crate::comments::Comment;
        use officina_core::history::Op;
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        for (r, v) in [3.0, 1.0, 2.0].iter().enumerate() {
            wb.set_value(crate::addr::CellAddr::new(0, r as u32, 0), crate::Value::Num(*v));
        }
        wb.set_comments(0, vec![(0, 1, Some(Comment::new("", "three"))), (1, 0, Some(Comment::new("", "one")))]);
        let op = sort_op(&wb, Rect { sheet: 0, r0: 0, c0: 0, r1: 2, c1: 1 }, &[SortKey { col: 0, descending: false }]).unwrap();
        let inv = op.apply(&mut wb);
        assert_eq!(wb.comment(0, 0, 0).map(|k| &*k.text), Some("one"));
        assert_eq!(wb.comment(0, 2, 1).map(|k| &*k.text), Some("three"));
        assert_eq!(wb.comments(0).len(), 2);
        inv.apply(&mut wb);
        assert_eq!(wb.comment(0, 0, 1).map(|k| &*k.text), Some("three"));
        assert_eq!(wb.comment(0, 1, 0).map(|k| &*k.text), Some("one"));
    }

    #[test]
    fn merges_allow_a_sort_only_when_every_row_is_merged_alike() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let r = |r0, c0, r1, c1| Rect { sheet: 0, r0, c0, r1, c1 };
        let all = r(0, 0, 3, 3);
        assert!(merges_allow_sort(&wb, &all));
        // B:C merged on every row, D alone: fine.
        wb.set_merges(0, (0..4).map(|k| r(k, 1, k, 2)).collect());
        assert!(merges_allow_sort(&wb, &all));
        // One row missing its merge.
        wb.set_merges(0, (0..3).map(|k| r(k, 1, k, 2)).collect());
        assert!(!merges_allow_sort(&wb, &all));
        // A row merged differently.
        wb.set_merges(0, (0..3).map(|k| r(k, 1, k, 2)).chain([r(3, 1, 3, 3)]).collect());
        assert!(!merges_allow_sort(&wb, &all));
        // Two spans a row, alike.
        wb.set_merges(0, (0..4).flat_map(|k| [r(k, 0, k, 1), r(k, 2, k, 3)]).collect());
        assert!(merges_allow_sort(&wb, &all));
        // A merge two rows high, or one crossing the range's edge.
        wb.set_merges(0, vec![r(0, 0, 1, 0)]);
        assert!(!merges_allow_sort(&wb, &all));
        wb.set_merges(0, (0..4).map(|k| r(k, 3, k, 4)).collect());
        assert!(!merges_allow_sort(&wb, &all));
    }
}
