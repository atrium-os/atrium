//! GROUPBY and PIVOTBY (docs/spec/officina.md §16.140): values aggregated
//! by the distinct keys of one or more fields, with totals and sorting.

use super::{to_bool, Array, Ctx, Ev, Lambda, MAX_ARRAY};
use crate::formula::Expr;
use crate::value::{compare, ErrorKind, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

type R<T> = Result<T, ErrorKind>;

/// Records times output cells one call may touch.
const MAX_WORK: usize = 200_000_000;

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn opt_int(c: &Ctx, a: &[Expr], i: usize) -> R<Option<i64>> {
    match a.get(i) {
        Some(x) if !missing(x) => {
            let v = c.number(&c.scalar(x))?.trunc();
            if v.abs() > 1e6 {
                return Err(ErrorKind::Value);
            }
            Ok(Some(v as i64))
        }
        _ => Ok(None),
    }
}
fn array(c: &Ctx, a: &Expr) -> R<Arc<Array>> {
    match c.eval(a) {
        Ev::V(Value::Err(k)) => Err(k),
        ev => c.to_array(ev),
    }
}
fn row(a: &Array, r: usize) -> &[Value] {
    &a.vals[r * a.cols..(r + 1) * a.cols]
}
fn cmp_keys(x: &[Value], y: &[Value]) -> Ordering {
    x.iter().zip(y).map(|(a, b)| compare(a, b)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
}

/// field_headers: 0 none, 1 present but hidden, 2 none but generated, 3
/// present and shown; by default present and shown when the first row of
/// values is all text and the second has a number.
fn headers(c: &Ctx, a: &[Expr], i: usize, values: &Array) -> R<(bool, bool)> {
    match opt_int(c, a, i)? {
        Some(0) => Ok((false, false)),
        Some(1) => Ok((true, false)),
        Some(2) => Ok((false, true)),
        Some(3) => Ok((true, true)),
        Some(_) => Err(ErrorKind::Value),
        None => {
            let guess = values.rows >= 2
                && row(values, 0).iter().all(|v| matches!(v, Value::Str(_)))
                && row(values, 1).iter().any(|v| matches!(v, Value::Num(_)));
            Ok((guess, guess))
        }
    }
}

/// One output line on an axis: its label and the leaves (positions in the
/// sorted distinct keys) it covers; `depth` is how many key fields it
/// names (all of them for a leaf, fewer for a subtotal, none for the total).
struct Slot {
    label: Vec<Value>,
    from: usize,
    to: usize,
}

/// An axis (rows or columns): the distinct keys in output order, each
/// record's leaf, and the lines to show with their totals.
struct Axis {
    leaves: Vec<Vec<Value>>,
    leaf_of: Vec<usize>,
    slots: Vec<Slot>,
}

/// Builds an axis from the key rows of the kept records, ordered by
/// `order` (a comparison of two leaves' first records) and with totals to
/// `depth` levels (1 the grand total, 2 also subtotals by the first field,
/// …; negative puts them before what they total).
fn axis(keys: &Array, start: usize, kept: &[usize], depth: i64, order: &dyn Fn(usize, usize) -> Ordering) -> R<Axis> {
    let k = keys.cols;
    // Distinct keys, in first-seen order, then sorted.
    let mut index: Vec<usize> = Vec::new();
    let mut leaf_rec: Vec<usize> = Vec::new();
    let mut leaf_of = vec![usize::MAX; keys.rows];
    let mut sorted: Vec<usize> = kept.to_vec();
    sorted.sort_by(|&x, &y| cmp_keys(row(keys, start + x), row(keys, start + y)));
    for (i, &r) in sorted.iter().enumerate() {
        if i == 0 || cmp_keys(row(keys, start + sorted[i - 1]), row(keys, start + r)).is_ne() {
            leaf_rec.push(r);
            index.push(index.len());
        }
        leaf_of[r] = leaf_rec.len() - 1;
    }
    index.sort_by(|&x, &y| order(leaf_rec[x], leaf_rec[y]).then(x.cmp(&y)));
    // Keep the hierarchy: parents in the order their first leaf comes.
    let prefix = |l: usize, n: usize| -> &[Value] { &row(keys, start + leaf_rec[l])[..n] };
    for level in (1..k).rev() {
        // Each prefix's first position in the current order.
        let mut by_prefix: Vec<usize> = (0..index.len()).collect();
        by_prefix.sort_by(|&x, &y| cmp_keys(prefix(index[x], level), prefix(index[y], level)).then(x.cmp(&y)));
        let mut first = vec![0; index.len()];
        let mut head = 0;
        for (i, &pos) in by_prefix.iter().enumerate() {
            if i == 0 || cmp_keys(prefix(index[by_prefix[i - 1]], level), prefix(index[pos], level)).is_ne() {
                head = pos;
            }
            first[pos] = head;
        }
        let mut order: Vec<usize> = (0..index.len()).collect();
        order.sort_by_key(|&pos| (first[pos], pos));
        index = order.into_iter().map(|pos| index[pos]).collect();
    }
    let rank: Vec<usize> = {
        let mut r = vec![0; index.len()];
        for (pos, &l) in index.iter().enumerate() {
            r[l] = pos;
        }
        r
    };
    let leaf_of: Vec<usize> = leaf_of.iter().map(|&l| if l == usize::MAX { l } else { rank[l] }).collect();
    let leaves: Vec<Vec<Value>> = index.iter().map(|&l| row(keys, start + leaf_rec[l]).to_vec()).collect();
    let levels = depth.unsigned_abs().min(k as u64) as usize;
    let before = depth < 0;
    let mut slots = Vec::new();
    let blank = |n: usize, v: &[Value]| -> Vec<Value> { (0..k).map(|j| if j < n { v[j].clone() } else { Value::Empty }).collect() };
    // Subtotals for prefixes of length 1..levels-1 around their leaves.
    fn walk(leaves: &[Vec<Value>], from: usize, to: usize, n: usize, levels: usize, before: bool, slots: &mut Vec<Slot>, blank: &dyn Fn(usize, &[Value]) -> Vec<Value>) {
        if n + 1 >= levels || n + 1 >= leaves.first().map_or(0, Vec::len) {
            for l in from..to {
                slots.push(Slot { label: leaves[l].clone(), from: l, to: l + 1 });
            }
            return;
        }
        let mut i = from;
        while i < to {
            let mut j = i + 1;
            while j < to && cmp_keys(&leaves[j][..=n], &leaves[i][..=n]).is_eq() {
                j += 1;
            }
            let total = Slot { label: blank(n + 1, &leaves[i]), from: i, to: j };
            if before {
                slots.push(total);
                walk(leaves, i, j, n + 1, levels, before, slots, blank);
            } else {
                walk(leaves, i, j, n + 1, levels, before, slots, blank);
                slots.push(total);
            }
            i = j;
        }
    }
    let grand = |label| Slot { label, from: 0, to: leaves.len() };
    let mut total_label = vec![Value::str("Total")];
    total_label.resize(k, Value::Empty);
    if levels >= 1 && before {
        slots.push(grand(total_label.clone()));
    }
    walk(&leaves, 0, leaves.len(), 0, levels, before, &mut slots, &blank);
    if levels >= 1 && !before {
        slots.push(grand(total_label));
    }
    Ok(Axis { leaves, leaf_of, slots })
}

/// The records kept by filter_array (TRUE rows), after any header row.
fn kept(c: &Ctx, a: &[Expr], i: usize, n: usize, start: usize) -> R<Vec<usize>> {
    match a.get(i).filter(|x| !missing(x)) {
        None => Ok((0..n).collect()),
        Some(x) => {
            let f = array(c, x)?;
            if f.vals.len() != n + start && f.vals.len() != n {
                return Err(ErrorKind::Value);
            }
            let skip = f.vals.len() - n;
            let mut out = Vec::new();
            for r in 0..n {
                match &f.vals[skip + r] {
                    Value::Err(k) => return Err(*k),
                    v if to_bool(v).unwrap_or(false) => out.push(r),
                    _ => {}
                }
            }
            Ok(out)
        }
    }
}

/// The function applied to one column of a group's values.
fn apply(c: &Ctx, f: &Lambda, vals: Vec<Value>, all: &Arc<Array>) -> Value {
    if vals.is_empty() {
        return Value::Empty;
    }
    let col = Ev::A(Arc::new(Array { rows: vals.len(), cols: 1, vals }));
    let args = if f.params.len() >= 2 { vec![Some(col), Some(Ev::A(all.clone()))] } else { vec![Some(col)] };
    match c.call_with(f, args) {
        Ev::A(a) if a.vals.len() == 1 => a.vals[0].clone(),
        Ev::A(_) => Value::Err(ErrorKind::Calc),
        ev => c.to_scalar(ev),
    }
}

/// A sort argument: column index (1-based, negative descending) or none.
fn sort_arg(c: &Ctx, a: &[Expr], i: usize) -> R<Option<i64>> {
    match a.get(i).filter(|x| !missing(x)) {
        None => Ok(None),
        Some(x) => {
            let arr = array(c, x)?;
            match arr.vals.first() {
                Some(Value::Num(v)) if v.trunc() != 0.0 && v.abs() <= 1e6 => Ok(Some(v.trunc() as i64)),
                Some(Value::Err(k)) => Err(*k),
                _ => Err(ErrorKind::Value),
            }
        }
    }
}

/// GROUPBY(row_fields, values, function, [field_headers], [total_depth],
/// [sort_order], [filter_array], [field_relationship]).
pub(super) fn groupby(c: &Ctx, a: &[Expr]) -> Ev {
    match groupby_(c, a) {
        Ok(ev) => ev,
        Err(k) => Ev::err(k),
    }
}
fn groupby_(c: &Ctx, a: &[Expr]) -> R<Ev> {
    let keys = array(c, &a[0])?;
    let values = array(c, &a[1])?;
    let f = c.lambda(&a[2])?;
    if keys.rows != values.rows || keys.rows == 0 {
        return Err(ErrorKind::Value);
    }
    let (has, show) = headers(c, a, 3, &values)?;
    let start = usize::from(has);
    let n = keys.rows - start;
    let depth = opt_int(c, a, 4)?.unwrap_or(1);
    if depth.abs() > keys.cols as i64 + 1 {
        return Err(ErrorKind::Value);
    }
    let sort = sort_arg(c, a, 5)?;
    let rel = opt_int(c, a, 7)?.unwrap_or(0);
    if !(0..=1).contains(&rel) {
        return Err(ErrorKind::Value);
    }
    let kept = kept(c, a, 6, n, start)?;
    let (k, v) = (keys.cols, values.cols);
    if n.saturating_mul(v).saturating_mul(keys.cols + 2) > MAX_WORK {
        return Err(ErrorKind::Limit);
    }
    let vcol = |r: usize, j: usize| values.vals[(start + r) * v + j].clone();
    let all: Vec<Arc<Array>> = (0..v)
        .map(|j| Arc::new(Array { rows: kept.len().max(1), cols: 1, vals: if kept.is_empty() { vec![Value::Empty] } else { kept.iter().map(|&r| vcol(r, j)).collect() } }))
        .collect();
    // A leaf's records, for sorting by a value column.
    let mut by_key: HashMap<usize, Vec<usize>> = HashMap::new();
    {
        let mut sorted = kept.clone();
        sorted.sort_by(|&x, &y| cmp_keys(row(&keys, start + x), row(&keys, start + y)));
        let mut head = usize::MAX;
        for (i, &r) in sorted.iter().enumerate() {
            if i == 0 || cmp_keys(row(&keys, start + sorted[i - 1]), row(&keys, start + r)).is_ne() {
                head = r;
            }
            by_key.entry(head).or_default().push(r);
        }
    }
    let agg_of = |first: usize, j: usize| -> Value {
        let recs = by_key.get(&first).cloned().unwrap_or_default();
        apply(c, &f, recs.iter().map(|&r| vcol(r, j)).collect(), &all[j])
    };
    let cache: std::cell::RefCell<HashMap<(usize, usize), Value>> = Default::default();
    let order = |x: usize, y: usize| -> Ordering {
        let kx = row(&keys, start + x);
        let ky = row(&keys, start + y);
        match sort {
            None => cmp_keys(kx, ky),
            Some(s) => {
                let i = s.unsigned_abs() as usize - 1;
                let o = if i < k {
                    compare(&kx[i], &ky[i])
                } else {
                    let j = i - k;
                    let mut cache = cache.borrow_mut();
                    let vx = cache.entry((x, j)).or_insert_with(|| agg_of(x, j)).clone();
                    let vy = cache.entry((y, j)).or_insert_with(|| agg_of(y, j)).clone();
                    compare(&vx, &vy)
                };
                if s < 0 { o.reverse() } else { o }.then_with(|| cmp_keys(kx, ky))
            }
        }
    };
    if let Some(s) = sort {
        if s.unsigned_abs() as usize > k + v {
            return Err(ErrorKind::Value);
        }
    }
    let ax = axis(&keys, start, &kept, depth, &order)?;
    // Records of each leaf, in leaf order.
    let mut recs: Vec<Vec<usize>> = vec![Vec::new(); ax.leaves.len()];
    for &r in &kept {
        recs[ax.leaf_of[r]].push(r);
    }
    let width = k + v;
    let mut out: Vec<Value> = Vec::new();
    if show {
        for j in 0..k {
            out.push(if has { keys.vals[j].clone() } else { Value::str(&format!("Row Field {}", j + 1)) });
        }
        for j in 0..v {
            out.push(if has { values.vals[j].clone() } else { Value::str(&format!("Value {}", j + 1)) });
        }
    }
    for slot in &ax.slots {
        out.extend(slot.label.iter().cloned());
        for j in 0..v {
            let vals: Vec<Value> = (slot.from..slot.to).flat_map(|l| recs[l].iter().map(|&r| vcol(r, j))).collect();
            out.push(apply(c, &f, vals, &all[j]));
        }
        if out.len() as u64 > MAX_ARRAY {
            return Err(ErrorKind::Limit);
        }
    }
    let rows = out.len() / width;
    if rows == 0 {
        return Err(ErrorKind::Calc);
    }
    Ok(Ev::A(Arc::new(Array { rows, cols: width, vals: out })))
}

/// PIVOTBY(row_fields, col_fields, values, function, [field_headers],
/// [row_total_depth], [row_sort_order], [col_total_depth],
/// [col_sort_order], [filter_array], [relative_to]).
pub(super) fn pivotby(c: &Ctx, a: &[Expr]) -> Ev {
    match pivotby_(c, a) {
        Ok(ev) => ev,
        Err(k) => Ev::err(k),
    }
}
fn pivotby_(c: &Ctx, a: &[Expr]) -> R<Ev> {
    let rkeys = array(c, &a[0])?;
    let ckeys = array(c, &a[1])?;
    let values = array(c, &a[2])?;
    let f = c.lambda(&a[3])?;
    if rkeys.rows != values.rows || ckeys.rows != values.rows || values.rows == 0 {
        return Err(ErrorKind::Value);
    }
    let (has, show) = headers(c, a, 4, &values)?;
    let start = usize::from(has);
    let n = values.rows - start;
    let rdepth = opt_int(c, a, 5)?.unwrap_or(1);
    let cdepth = opt_int(c, a, 7)?.unwrap_or(1);
    let rsort = sort_arg(c, a, 6)?;
    let csort = sort_arg(c, a, 8)?;
    let relative = opt_int(c, a, 10)?.unwrap_or(0);
    if !(0..=4).contains(&relative) || rdepth.abs() > rkeys.cols as i64 + 1 || cdepth.abs() > ckeys.cols as i64 + 1 {
        return Err(ErrorKind::Value);
    }
    let kept = kept(c, a, 9, n, start)?;
    let v = values.cols;
    let vcol = |r: usize, j: usize| values.vals[(start + r) * v + j].clone();
    let all: Vec<Arc<Array>> = (0..v)
        .map(|j| Arc::new(Array { rows: kept.len().max(1), cols: 1, vals: if kept.is_empty() { vec![Value::Empty] } else { kept.iter().map(|&r| vcol(r, j)).collect() } }))
        .collect();
    // Sorting by key only (a value sort on PIVOTBY sorts by the totals).
    let by = |keys: &Arc<Array>, sort: Option<i64>| {
        let keys = keys.clone();
        move |x: usize, y: usize| -> Ordering {
            let (kx, ky) = (row(&keys, start + x), row(&keys, start + y));
            match sort {
                Some(s) if (s.unsigned_abs() as usize) <= keys.cols => {
                    let i = s.unsigned_abs() as usize - 1;
                    let o = compare(&kx[i], &ky[i]);
                    if s < 0 { o.reverse() } else { o }.then_with(|| cmp_keys(kx, ky))
                }
                _ => cmp_keys(kx, ky),
            }
        }
    };
    let rorder = by(&rkeys, rsort);
    let corder = by(&ckeys, csort);
    let rax = axis(&rkeys, start, &kept, rdepth, &rorder)?;
    let cax = axis(&ckeys, start, &kept, cdepth, &corder)?;
    let (rk, ck) = (rkeys.cols, ckeys.cols);
    let width = rk + cax.slots.len() * v;
    let header_rows = ck + usize::from(show && v > 1);
    let height = header_rows + rax.slots.len() + usize::from(show);
    if (width as u64) * (height as u64) > MAX_ARRAY {
        return Err(ErrorKind::Limit);
    }
    if n.saturating_mul(cax.slots.len()).saturating_mul(rk.max(1) + 1) > MAX_WORK {
        return Err(ErrorKind::Limit);
    }
    let mut out = vec![Value::Empty; width * height];
    // Column headers: one row per column field, then value names.
    if show {
        for j in 0..rk {
            out[(header_rows) * width + j] = if has { rkeys.vals[j].clone() } else { Value::str(&format!("Row Field {}", j + 1)) };
        }
    }
    for (s, slot) in cax.slots.iter().enumerate() {
        for i in 0..ck {
            for j in 0..v {
                out[i * width + rk + s * v + j] = slot.label[i].clone();
            }
        }
        if show && v > 1 {
            for j in 0..v {
                out[ck * width + rk + s * v + j] = if has { values.vals[j].clone() } else { Value::str(&format!("Value {}", j + 1)) };
            }
        }
    }
    let mut rrecs: Vec<Vec<usize>> = vec![Vec::new(); rax.leaves.len()];
    for &r in &kept {
        rrecs[rax.leaf_of[r]].push(r);
    }
    let body = header_rows + usize::from(show);
    for (ri, rslot) in rax.slots.iter().enumerate() {
        let line = body + ri;
        for (j, label) in rslot.label.iter().enumerate() {
            out[line * width + j] = label.clone();
        }
        let records: Vec<usize> = (rslot.from..rslot.to).flat_map(|l| rrecs[l].iter().copied()).collect();
        for (ci, cslot) in cax.slots.iter().enumerate() {
            for j in 0..v {
                let vals: Vec<Value> = records
                    .iter()
                    .filter(|&&r| (cslot.from..cslot.to).contains(&cax.leaf_of[r]))
                    .map(|&r| vcol(r, j))
                    .collect();
                out[line * width + rk + ci * v + j] = apply(c, &f, vals, &all[j]);
            }
        }
    }
    Ok(Ev::A(Arc::new(Array { rows: height, cols: width, vals: out })))
}
