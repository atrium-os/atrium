//! Register allocation: live intervals over the linear machine IR, extended
//! across loops, then first-fit assignment per class.
//!
//! Linear intervals are sound under EXEC masking because every lane moves
//! through the code in linear order except at loop back edges: a value live
//! into a loop stays live to the loop's end, and a value defined in a loop
//! and used after it is live from the loop's start (lanes that left early
//! must keep their copy while the others iterate).

use crate::mir::*;
use crate::Error;

/// Physical register base for each virtual register.
pub struct Assignment {
    /// `phys[vr]`.
    pub phys: Vec<u8>,
    /// Highest VGPR used + 1.
    pub vgprs: u32,
    /// Highest SGPR used + 1.
    pub sgprs: u32,
}

/// Allocate. VGPRs start at `v_first`, SGPRs at `s_first`.
pub fn allocate(mi: &[MI], vregs: &[VRegInfo], v_first: u8, s_first: u8) -> Result<Assignment, Error> {
    let n = vregs.len();
    let mut start = vec![usize::MAX; n];
    let mut end = vec![0usize; n];
    let mut loops: Vec<(usize, usize)> = vec![];
    let mut open: Vec<usize> = vec![];
    for (pos, m) in mi.iter().enumerate() {
        match m {
            MI::LoopBegin => open.push(pos),
            MI::LoopEnd => loops.push((open.pop().expect("unbalanced loop"), pos)),
            _ => {}
        }
        let (u, d) = m.regs();
        for r in u.into_iter().chain(d) {
            let r = r as usize;
            start[r] = start[r].min(pos);
            end[r] = end[r].max(pos);
        }
    }
    loop {
        let mut changed = false;
        for &(b, e) in &loops {
            for r in 0..n {
                if start[r] == usize::MAX {
                    continue;
                }
                if start[r] < b && end[r] >= b && end[r] < e {
                    end[r] = e;
                    changed = true;
                }
                if start[r] > b && start[r] <= e && end[r] > e {
                    start[r] = b;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut order: Vec<usize> = (0..n).filter(|r| start[*r] != usize::MAX).collect();
    order.sort_by_key(|r| (start[*r], *r));
    let mut phys = vec![0u8; n];
    let mut active: Vec<usize> = vec![];
    let (mut vmax, mut smax) = (v_first as u32, s_first as u32);
    for r in order {
        active.retain(|a| end[*a] >= start[r]);
        let info = vregs[r];
        let (first, limit) = match info.class {
            Class::V => (v_first as u32, 256u32),
            Class::S => (s_first as u32, 106u32),
        };
        let busy = |base: u32| {
            active.iter().any(|a| {
                let ai = vregs[*a];
                ai.class == info.class && {
                    let (ab, ae) = (phys[*a] as u32, phys[*a] as u32 + ai.size as u32);
                    base < ae && ab < base + info.size as u32
                }
            })
        };
        let base = (first..=limit - info.size as u32).find(|b| !busy(*b)).ok_or_else(|| {
            Error::Unsupported(format!("out of {:?} registers (spilling not implemented)", info.class))
        })?;
        phys[r] = base as u8;
        match info.class {
            Class::V => vmax = vmax.max(base + info.size as u32),
            Class::S => smax = smax.max(base + info.size as u32),
        }
        active.push(r);
    }
    Ok(Assignment { phys, vgprs: vmax, sgprs: smax })
}
