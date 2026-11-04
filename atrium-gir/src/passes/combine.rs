//! Load combining: a `construct` of scalar loads from consecutive 4-byte
//! offsets of one buffer becomes a single vector load.
//!
//! Frontends split vector loads into per-component loads (Slang's
//! structured buffers do). Targets load up to 16 bytes per instruction, so
//! `construct(load b+o, load b+o+4, load b+o+8, load b+o+12)` is one
//! 128-bit load. The construct itself is replaced by the load at its own
//! position, so every offset it needs is already defined; the scalar loads
//! become dead unless something else uses them. Only straight-line code
//! between the first load and the construct is considered, with no store,
//! atomic or barrier in between, so the memory read is the same.

use crate::*;
use std::collections::HashMap;

fn defs(r: &Region, out: &mut HashMap<ValueId, Op>) {
    for n in r {
        match n {
            Node::Inst(i) => {
                if let Some(v) = i.result {
                    out.insert(v, i.op.clone());
                }
            }
            Node::If { then, els, .. } => {
                defs(then, out);
                defs(els, out);
            }
            Node::Loop { body, .. } => defs(body, out),
        }
    }
}

/// `off` as base + constant (base `None` for a constant offset).
fn affine(defs: &HashMap<ValueId, Op>, off: ValueId) -> (Option<ValueId>, i64) {
    match defs.get(&off) {
        Some(Op::Const(c)) => (None, *c as i32 as i64),
        Some(Op::Bin(BinOp::IAdd, a, b)) => match (defs.get(a), defs.get(b)) {
            (_, Some(Op::Const(c))) => (Some(*a), *c as i32 as i64),
            (Some(Op::Const(c)), _) => (Some(*b), *c as i32 as i64),
            _ => (Some(off), 0),
        },
        _ => (Some(off), 0),
    }
}

fn region(r: &mut Region, defs: &HashMap<ValueId, Op>, types: &[Ty]) -> bool {
    let mut changed = false;
    // Position of each load result defined directly in this region.
    let mut at: HashMap<ValueId, usize> = HashMap::new();
    for j in 0..r.len() {
        match &mut r[j] {
            Node::If { then, els, .. } => {
                changed |= region(then, defs, types);
                changed |= region(els, defs, types);
                continue;
            }
            Node::Loop { body, .. } => {
                changed |= region(body, defs, types);
                continue;
            }
            Node::Inst(_) => {}
        }
        let Node::Inst(i) = &r[j] else { unreachable!() };
        if let (Op::Load(..), Some(v)) = (&i.op, i.result) {
            at.insert(v, j);
            continue;
        }
        let Op::Construct(parts) = &i.op else { continue };
        let Some(res) = i.result else { continue };
        let ty = types[res.0 as usize];
        if ty.scalar() == Scalar::Bool || !(2..=4).contains(&parts.len()) {
            continue;
        }
        let mut space = None;
        let mut first = j;
        let mut base: Option<(Option<ValueId>, i64, ValueId)> = None;
        let mut ok = true;
        for (k, p) in parts.iter().enumerate() {
            let (Some(&pos), Some(Op::Load(s, off))) = (at.get(p), defs.get(p)) else {
                ok = false;
                break;
            };
            if types[p.0 as usize].comps() != 1 || *space.get_or_insert(*s) != *s {
                ok = false;
                break;
            }
            first = first.min(pos);
            let (b, c) = affine(defs, *off);
            match base {
                None => base = Some((b, c, *off)),
                Some((b0, c0, _)) => {
                    if b != b0 || c != c0 + 4 * k as i64 {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        // Nothing between the first load and here may write memory.
        let clean = r[first..j].iter().all(|n| match n {
            Node::Inst(i) => !matches!(i.op, Op::Store(..) | Op::Atomic(..) | Op::Barrier),
            _ => false,
        });
        if !clean {
            continue;
        }
        let (space, (_, _, off0)) = (space.unwrap(), base.unwrap());
        r[j] = Node::Inst(Inst { result: Some(res), op: Op::Load(space, off0) });
        changed = true;
    }
    changed
}

/// Combine loads in `f`. Returns true if anything changed.
pub fn combine_loads(f: &mut Function) -> bool {
    let mut d = HashMap::new();
    defs(&f.body, &mut d);
    let types = f.types.clone();
    region(&mut f.body, &d, &types)
}
