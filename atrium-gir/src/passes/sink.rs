//! Sinking: a pure instruction, or a load from memory the kernel never
//! writes, moves into the one `if` arm that uses it. Values computed up
//! front and consumed deep inside one branch otherwise stay live (in
//! registers) across everything in between and execute on every path.
//!
//! Only into `if` arms of the same region, never into loops (that would
//! repeat the work), and never past a use in the region itself.

use crate::*;
use std::collections::HashSet;

fn uses_in(n: &Node, v: ValueId) -> bool {
    match n {
        Node::Inst(i) => i.op.operands().contains(&v),
        Node::If { cond, then, els, .. } => *cond == v || then.iter().any(|m| uses_in(m, v)) || els.iter().any(|m| uses_in(m, v)),
        Node::Loop { inits, body, .. } => inits.contains(&v) || body.iter().any(|m| uses_in(m, v)),
    }
}

fn rename(r: &mut Region, from: ValueId, to: ValueId) {
    for n in r {
        match n {
            Node::Inst(i) => {
                for o in i.op.operands_mut() {
                    if *o == from {
                        *o = to;
                    }
                }
            }
            Node::If { cond, then, els, .. } => {
                if *cond == from {
                    *cond = to;
                }
                rename(then, from, to);
                rename(els, from, to);
            }
            Node::Loop { inits, body, .. } => {
                for o in inits.iter_mut() {
                    if *o == from {
                        *o = to;
                    }
                }
                rename(body, from, to);
            }
        }
    }
}

fn written(r: &Region, out: &mut HashSet<Space>) {
    for n in r {
        match n {
            Node::Inst(i) => {
                if let Op::Store(s, ..) | Op::Atomic(_, s, ..) = &i.op {
                    out.insert(*s);
                }
            }
            Node::If { then, els, .. } => {
                written(then, out);
                written(els, out);
            }
            Node::Loop { body, .. } => written(body, out),
        }
    }
}

fn movable(op: &Op, w: &HashSet<Space>) -> bool {
    match op {
        Op::Const(_) => false,
        Op::Load(s @ (Space::Buffer { .. } | Space::Push), _) => !w.contains(s),
        _ => op.is_pure(),
    }
}

fn sink_region(f: &mut Vec<Ty>, r: &mut Region, w: &HashSet<Space>) -> bool {
    let mut changed = false;
    let mut i = r.len();
    while i > 0 {
        i -= 1;
        let Node::Inst(inst) = &r[i] else { continue };
        let Some(v) = inst.result else { continue };
        if !movable(&inst.op, w) {
            continue;
        }
        let users: Vec<usize> = (i + 1..r.len()).filter(|&j| uses_in(&r[j], v)).collect();
        let [j] = users[..] else { continue };
        let Node::If { cond, then, els, .. } = &r[j] else { continue };
        if *cond == v {
            continue;
        }
        let (t, e) = (then.iter().any(|m| uses_in(m, v)), els.iter().any(|m| uses_in(m, v)));
        if !t && !e {
            continue;
        }
        let Node::Inst(moved) = r.remove(i) else { unreachable!() };
        let Node::If { then, els, .. } = &mut r[j - 1] else { unreachable!() };
        if t && e {
            // Used in both arms: a copy in each. A lane runs one arm, so
            // the work per lane is unchanged; the value no longer lives
            // across the whole `if` head.
            let v2 = ValueId(f.len() as u32);
            f.push(f[v.0 as usize]);
            rename(els, v, v2);
            els.insert(0, Node::Inst(Inst { result: Some(v2), op: moved.op.clone() }));
            then.insert(0, Node::Inst(moved));
        } else if t {
            then.insert(0, Node::Inst(moved))
        } else {
            els.insert(0, Node::Inst(moved))
        }
        changed = true;
    }
    for n in r.iter_mut() {
        match n {
            Node::Inst(_) => {}
            Node::If { then, els, .. } => {
                changed |= sink_region(f, then, w);
                changed |= sink_region(f, els, w);
            }
            Node::Loop { body, .. } => changed |= sink_region(f, body, w),
        }
    }
    changed
}

/// Sink in `f`; true if anything moved.
pub fn sink(f: &mut Function) -> bool {
    let mut w = HashSet::new();
    written(&f.body, &mut w);
    sink_region(&mut f.types, &mut f.body, &w)
}
