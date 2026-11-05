//! Register allocation: block liveness, one live interval per vreg (the
//! hull of everywhere it is live), and linear scan over the two register
//! classes.
//!
//! x86 has 16 XMM and 15 usable GPRs, so spilling is normal, not a retry
//! path. A spilled vreg lives in its stack slot for its whole life ("spill
//! everywhere"): the emitter loads it into a scratch register at each use
//! or uses it as a memory operand, and stores it after each definition. Two
//! XMM registers (xmm14, xmm15) and two GPRs (rax, r11) are kept out of
//! allocation as those scratches.
//!
//! Positions: instruction `i` reads its operands at `2i` and writes its
//! result at `2i + 1`, so a result can take the register of an operand that
//! dies at the same instruction.
//!
//! `ATRIUM_SPV_FORCE_SPILL=1` spills every vreg that isn't an ABI argument,
//! so small test shaders exercise the spill paths too.

use std::collections::HashMap;

use crate::vcode::{Class, Label, MInst, PReg, VCode, VReg};

/// Where a vreg lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loc {
    Reg(PReg),
    /// 16-byte stack slot index.
    Slot(u32),
}

pub(crate) struct Allocation {
    pub locs: Vec<Loc>,
    pub num_slots: u32,
}

/// Allocatable GPRs, in preference order: caller-saved first. rax and r11
/// are scratch; rsp is the stack pointer.
pub(crate) const GPR_ORDER: [u8; 13] = [10, 9, 1, 8, 2, 6, 7, 3, 5, 12, 13, 14, 15];
/// Allocatable XMM registers. High first, so the frag-coord argument
/// registers xmm0-3 are taken last.
pub(crate) const XMM_ORDER: [u8; 14] = [13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0];

fn force_spill() -> bool {
    std::env::var("ATRIUM_SPV_FORCE_SPILL").map(|v| v != "0").unwrap_or(false)
}

struct BitSet(Vec<u64>);

impl BitSet {
    fn new(n: usize) -> Self {
        BitSet(vec![0; n.div_ceil(64)])
    }
    fn insert(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
    fn contains(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(w, &bits)| {
            (0..64).filter(move |b| bits & (1 << b) != 0).map(move |b| w * 64 + b)
        })
    }
}

/// Live interval per vreg: `[start, end]` in positions, or None if unused.
fn intervals(vc: &VCode) -> Vec<Option<(u32, u32)>> {
    let n = vc.classes.len();
    let insts = &vc.insts;

    // Split into segments at labels.
    let mut seg_start = vec![0usize];
    for (i, inst) in insts.iter().enumerate() {
        if i > 0 && matches!(inst, MInst::Label(_)) {
            seg_start.push(i);
        }
    }
    let nseg = seg_start.len();
    let seg_end: Vec<usize> = (0..nseg)
        .map(|s| if s + 1 < nseg { seg_start[s + 1] - 1 } else { insts.len() - 1 })
        .collect();
    let mut label_seg: HashMap<Label, usize> = HashMap::new();
    for (s, &i) in seg_start.iter().enumerate() {
        if let MInst::Label(l) = insts[i] {
            label_seg.insert(l, s);
        }
    }

    // Per segment: upward-exposed uses, defs, successors.
    let mut gen: Vec<BitSet> = (0..nseg).map(|_| BitSet::new(n)).collect();
    let mut kill: Vec<BitSet> = (0..nseg).map(|_| BitSet::new(n)).collect();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); nseg];
    let mut tmp = Vec::new();
    let mut labels = Vec::new();
    for s in 0..nseg {
        for inst in &insts[seg_start[s]..=seg_end[s]] {
            tmp.clear();
            inst.uses(&mut tmp);
            for v in &tmp {
                if !kill[s].contains(v.0 as usize) {
                    gen[s].insert(v.0 as usize);
                }
            }
            tmp.clear();
            inst.defs(&mut tmp);
            for v in &tmp {
                kill[s].insert(v.0 as usize);
            }
            labels.clear();
            inst.targets(&mut labels);
            for l in &labels {
                if let Some(&t) = label_seg.get(l) {
                    if !succs[s].contains(&t) {
                        succs[s].push(t);
                    }
                }
            }
        }
        // A segment that doesn't end in a jump or return falls through.
        if !matches!(insts[seg_end[s]], MInst::Jmp(_) | MInst::Br { .. } | MInst::Ret) && s + 1 < nseg {
            succs[s].push(s + 1);
        }
    }

    // Backward dataflow to a fixed point.
    let words = n.div_ceil(64);
    let mut live_in: Vec<Vec<u64>> = vec![vec![0; words]; nseg];
    let mut live_out: Vec<Vec<u64>> = vec![vec![0; words]; nseg];
    let mut changed = true;
    while changed {
        changed = false;
        for s in (0..nseg).rev() {
            let mut out = vec![0u64; words];
            for &t in &succs[s] {
                for w in 0..words {
                    out[w] |= live_in[t][w];
                }
            }
            let mut inn = vec![0u64; words];
            for w in 0..words {
                inn[w] = gen[s].0[w] | (out[w] & !kill[s].0[w]);
            }
            if inn != live_in[s] || out != live_out[s] {
                live_in[s] = inn;
                live_out[s] = out;
                changed = true;
            }
        }
    }

    let mut iv: Vec<Option<(u32, u32)>> = vec![None; n];
    let mut extend = |v: usize, p: u32| {
        iv[v] = Some(match iv[v] {
            None => (p, p),
            Some((a, b)) => (a.min(p), b.max(p)),
        });
    };
    for s in 0..nseg {
        let (sp, ep) = ((2 * seg_start[s]) as u32, (2 * seg_end[s] + 1) as u32);
        for v in BitSet(live_in[s].clone()).iter() {
            extend(v, sp);
        }
        for v in BitSet(live_out[s].clone()).iter() {
            extend(v, ep);
        }
        for i in seg_start[s]..=seg_end[s] {
            tmp.clear();
            insts[i].uses(&mut tmp);
            for v in &tmp {
                extend(v.0 as usize, 2 * i as u32);
            }
            tmp.clear();
            insts[i].defs(&mut tmp);
            for v in &tmp {
                extend(v.0 as usize, 2 * i as u32 + 1);
            }
        }
    }
    iv
}

pub(crate) fn allocate(vc: &VCode) -> Allocation {
    let iv = intervals(vc);
    let n = vc.classes.len();
    let force = force_spill();

    let mut order: Vec<usize> = (0..n).filter(|&v| iv[v].is_some()).collect();
    // By start; precoloured first on ties.
    order.sort_by_key(|&v| (iv[v].unwrap().0, !vc.fixed.contains_key(&VReg(v as u32)), v));

    let mut locs: Vec<Loc> = vec![Loc::Slot(u32::MAX); n];
    // Per class: register → vreg holding it now.
    let mut held: HashMap<PReg, usize> = HashMap::new();
    // Active register intervals: (end, vreg).
    let mut active: Vec<(u32, usize)> = Vec::new();
    // Slot reuse: slot k is free for any interval starting after free_after[k].
    let mut free_after: Vec<u32> = Vec::new();

    let new_slot = |start: u32, end: u32, free_after: &mut Vec<u32>| -> u32 {
        for (k, f) in free_after.iter_mut().enumerate() {
            if *f < start {
                *f = end;
                return k as u32;
            }
        }
        free_after.push(end);
        (free_after.len() - 1) as u32
    };

    for &v in &order {
        let (start, end) = iv[v].unwrap();
        // Expire.
        active.retain(|&(e, u)| {
            if e < start {
                if let Loc::Reg(r) = locs[u] {
                    held.remove(&r);
                }
                false
            } else {
                true
            }
        });

        if let Some(&r) = vc.fixed.get(&VReg(v as u32)) {
            // ABI arguments start at the entry, before anything else, so
            // their register is free.
            debug_assert!(!held.contains_key(&r));
            held.insert(r, v);
            locs[v] = Loc::Reg(r);
            active.push((end, v));
            continue;
        }
        if force {
            locs[v] = Loc::Slot(new_slot(start, end, &mut free_after));
            continue;
        }

        let class = vc.classes[v];
        let mk = |i: u8| match class {
            Class::X => PReg::X(i),
            Class::G => PReg::G(i),
        };
        let candidates: &[u8] = match class {
            Class::X => &XMM_ORDER,
            Class::G => &GPR_ORDER,
        };
        // Hint first: the register of the hinted vreg, if free now.
        let hinted = vc.hints.get(&VReg(v as u32)).and_then(|h| match locs[h.0 as usize] {
            Loc::Reg(r) if !held.contains_key(&r) && candidates.iter().any(|&c| mk(c) == r) => Some(r),
            _ => None,
        });
        let free = hinted.or_else(|| candidates.iter().map(|&c| mk(c)).find(|r| !held.contains_key(r)));
        if let Some(r) = free {
            held.insert(r, v);
            locs[v] = Loc::Reg(r);
            active.push((end, v));
            continue;
        }
        // Spill whichever of this interval and the active same-class,
        // non-precoloured intervals ends last.
        let victim = active
            .iter()
            .filter(|&&(_, u)| vc.classes[u] == class && !vc.fixed.contains_key(&VReg(u as u32)))
            .max_by_key(|&&(e, _)| e)
            .copied();
        match victim {
            Some((ve, u)) if ve > end => {
                let r = match locs[u] {
                    Loc::Reg(r) => r,
                    Loc::Slot(_) => unreachable!("active interval without a register"),
                };
                let (us, _) = iv[u].unwrap();
                locs[u] = Loc::Slot(new_slot(us, ve, &mut free_after));
                active.retain(|&(_, w)| w != u);
                held.insert(r, v);
                locs[v] = Loc::Reg(r);
                active.push((end, v));
            }
            _ => {
                locs[v] = Loc::Slot(new_slot(start, end, &mut free_after));
            }
        }
    }
    Allocation { locs, num_slots: free_after.len() as u32 }
}
