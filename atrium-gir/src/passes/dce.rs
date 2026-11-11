//! Dead code elimination.
//!
//! Removes pure instructions and loads whose results are unused, unused `if` and loop
//! results (with their `yield`/`break` operands), loop parameters that only
//! feed themselves, empty `if`s, and code after a node that never falls
//! through.
//!
//! Also dead carried values ([`carried`]): a loop parameter, loop result or
//! `if` result that only ever flows into other such slots, never into an
//! instruction that matters. mem2reg carries every variable a loop body
//! stores, so a variable written before it is read in each iteration (a
//! temporary declared in the loop) became a parameter that went round the
//! loop through the `if`s that write it, live for the whole loop.

use crate::*;
use std::collections::HashMap;

fn count_uses(r: &Region, uses: &mut HashMap<ValueId, usize>) {
    for n in r {
        match n {
            Node::Inst(i) => {
                for o in i.op.operands() {
                    *uses.entry(o).or_default() += 1;
                }
            }
            Node::If { cond, then, els, .. } => {
                *uses.entry(*cond).or_default() += 1;
                count_uses(then, uses);
                count_uses(els, uses);
            }
            Node::Loop { inits, body, .. } => {
                for v in inits {
                    *uses.entry(*v).or_default() += 1;
                }
                count_uses(body, uses);
            }
        }
    }
}

/// Drop slot `k` from every `yield` directly ending `r` (looking through
/// nested ifs is not needed: a yield always ends its own arm).
fn drop_yield_slot(r: &mut Region, k: usize) {
    if let Some(Node::Inst(Inst { op: Op::Yield(v), .. })) = r.last_mut() {
        v.remove(k);
    }
}

/// Drop slot `k` of `break` (or `continue`) for the loop whose body is `r`,
/// skipping nested loops.
fn drop_loop_slot(r: &mut Region, k: usize, is_break: bool) {
    for n in r {
        match n {
            Node::Inst(Inst { op: Op::Break(v), .. }) if is_break => {
                v.remove(k);
            }
            Node::Inst(Inst { op: Op::Continue(v), .. }) if !is_break => {
                v.remove(k);
            }
            Node::Inst(_) | Node::Loop { .. } => {}
            Node::If { then, els, .. } => {
                drop_loop_slot(then, k, is_break);
                drop_loop_slot(els, k, is_break);
            }
        }
    }
}

/// Uses of `p` that are exactly its own slot `k` in this loop's continues.
fn self_continue_uses(r: &Region, p: ValueId, k: usize) -> usize {
    let mut n = 0;
    for node in r {
        match node {
            Node::Inst(Inst { op: Op::Continue(v), .. }) if v.get(k) == Some(&p) => n += 1,
            Node::Inst(_) | Node::Loop { .. } => {}
            Node::If { then, els, .. } => n += self_continue_uses(then, p, k) + self_continue_uses(els, p, k),
        }
    }
    n
}

fn sweep(r: &mut Region, uses: &HashMap<ValueId, usize>) -> bool {
    let used = |v: &ValueId| uses.get(v).copied().unwrap_or(0) > 0;
    let mut changed = false;
    // Unreachable tail.
    if let Some(i) = r.iter().position(|n| match n {
        Node::Inst(i) => i.op.is_terminator(),
        Node::If { then, els, .. } => never_falls(then) && never_falls(els),
        Node::Loop { .. } => false,
    }) {
        if i + 1 < r.len() {
            r.truncate(i + 1);
            changed = true;
        }
    }
    let mut out = Region::new();
    for mut node in std::mem::take(r) {
        match &mut node {
            Node::Inst(i) => {
                if (i.op.is_pure() || matches!(i.op, Op::Load(..) | Op::Subgroup(..))) && i.result.map_or(false, |v| !used(&v)) {
                    changed = true;
                    continue;
                }
            }
            Node::If { then, els, results, .. } => {
                changed |= sweep(then, uses);
                changed |= sweep(els, uses);
                let mut k = results.len();
                while k > 0 {
                    k -= 1;
                    if !used(&results[k]) {
                        results.remove(k);
                        drop_yield_slot(then, k);
                        drop_yield_slot(els, k);
                        changed = true;
                    }
                }
                let empty = |a: &Region| a.is_empty() || matches!(a.as_slice(), [Node::Inst(Inst { op: Op::Yield(v), .. })] if v.is_empty());
                if results.is_empty() && empty(then) && empty(els) {
                    changed = true;
                    continue;
                }
            }
            Node::Loop { inits, params, body, results } => {
                changed |= sweep(body, uses);
                let mut k = results.len();
                while k > 0 {
                    k -= 1;
                    if !used(&results[k]) {
                        results.remove(k);
                        drop_loop_slot(body, k, true);
                        changed = true;
                    }
                }
                let mut k = params.len();
                while k > 0 {
                    k -= 1;
                    let p = params[k];
                    let total = uses.get(&p).copied().unwrap_or(0);
                    if total == self_continue_uses(body, p, k) {
                        params.remove(k);
                        inits.remove(k);
                        drop_loop_slot(body, k, false);
                        changed = true;
                    }
                }
            }
        }
        out.push(node);
    }
    *r = out;
    changed
}

/// Mark what matters, propagating through carried slots only where the
/// slot they feed matters.
fn mark(r: &Region, live: &mut std::collections::HashSet<ValueId>, ifs: &mut Vec<Vec<ValueId>>, loops: &mut Vec<(Vec<ValueId>, Vec<ValueId>)>) {
    let add = |v: ValueId, live: &mut std::collections::HashSet<ValueId>| {
        live.insert(v);
    };
    for (idx, n) in r.iter().enumerate() {
        match n {
            Node::Inst(i) => match &i.op {
                // A yield ends its own arm: slot k matters if the if's result k does.
                Op::Yield(v) if idx + 1 == r.len() => {
                    if let Some(res) = ifs.last() {
                        for (k, x) in v.iter().enumerate() {
                            if res.get(k).is_some_and(|rv| live.contains(rv)) {
                                add(*x, live);
                            }
                        }
                    }
                }
                Op::Continue(v) | Op::Break(v) => {
                    let is_break = matches!(i.op, Op::Break(_));
                    if let Some((params, results)) = loops.last() {
                        let slots = if is_break { results } else { params };
                        for (k, x) in v.iter().enumerate() {
                            if slots.get(k).is_some_and(|sv| live.contains(sv)) {
                                add(*x, live);
                            }
                        }
                    }
                }
                op if op.is_pure() || matches!(op, Op::Load(..) | Op::Subgroup(..)) => {
                    if i.result.is_some_and(|v| live.contains(&v)) {
                        for o in op.operands() {
                            add(o, live);
                        }
                    }
                }
                op => {
                    for o in op.operands() {
                        add(o, live);
                    }
                }
            },
            Node::If { cond, then, els, results } => {
                add(*cond, live);
                ifs.push(results.clone());
                mark(then, live, ifs, loops);
                mark(els, live, ifs, loops);
                ifs.pop();
            }
            Node::Loop { inits, params, body, results } => {
                for (k, x) in inits.iter().enumerate() {
                    if live.contains(&params[k]) {
                        add(*x, live);
                    }
                }
                loops.push((params.clone(), results.clone()));
                // Nested ifs inside the loop see their own yields; a fresh
                // if-stack keeps a yield from reaching an outer if.
                let mut inner_ifs = Vec::new();
                mark(body, live, &mut inner_ifs, loops);
                loops.pop();
            }
        }
    }
}

/// Drop carried slots nothing needs (see the module docs).
fn drop_dead_slots(r: &mut Region, live: &std::collections::HashSet<ValueId>) -> bool {
    let mut changed = false;
    for n in r.iter_mut() {
        match n {
            Node::Inst(_) => {}
            Node::If { then, els, results, .. } => {
                changed |= drop_dead_slots(then, live);
                changed |= drop_dead_slots(els, live);
                let mut k = results.len();
                while k > 0 {
                    k -= 1;
                    if !live.contains(&results[k]) {
                        results.remove(k);
                        drop_yield_slot(then, k);
                        drop_yield_slot(els, k);
                        changed = true;
                    }
                }
            }
            Node::Loop { inits, params, body, results } => {
                changed |= drop_dead_slots(body, live);
                let mut k = results.len();
                while k > 0 {
                    k -= 1;
                    if !live.contains(&results[k]) {
                        results.remove(k);
                        drop_loop_slot(body, k, true);
                        changed = true;
                    }
                }
                let mut k = params.len();
                while k > 0 {
                    k -= 1;
                    if !live.contains(&params[k]) {
                        params.remove(k);
                        inits.remove(k);
                        drop_loop_slot(body, k, false);
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

/// Remove dead carried values: loop parameters, loop results and `if`
/// results whose values only flow into other carried slots. Returns true
/// if anything changed.
pub fn carried(f: &mut Function) -> bool {
    let mut live = std::collections::HashSet::new();
    loop {
        let before = live.len();
        mark(&f.body, &mut live, &mut Vec::new(), &mut Vec::new());
        if live.len() == before {
            break;
        }
    }
    drop_dead_slots(&mut f.body, &live)
}

/// Run DCE to a fixed point. Returns true if anything changed.
pub fn dce(f: &mut Function) -> bool {
    let mut any = carried(f);
    loop {
        let mut uses = HashMap::new();
        count_uses(&f.body, &mut uses);
        if !sweep(&mut f.body, &uses) {
            return any;
        }
        any = true;
    }
}
