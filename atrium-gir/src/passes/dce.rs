//! Dead code elimination.
//!
//! Removes pure instructions and loads whose results are unused, unused `if` and loop
//! results (with their `yield`/`break` operands), loop parameters that only
//! feed themselves, empty `if`s, and code after a node that never falls
//! through.

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

/// Run DCE to a fixed point. Returns true if anything changed.
pub fn dce(f: &mut Function) -> bool {
    let mut any = false;
    loop {
        let mut uses = HashMap::new();
        count_uses(&f.body, &mut uses);
        if !sweep(&mut f.body, &uses) {
            return any;
        }
        any = true;
    }
}
