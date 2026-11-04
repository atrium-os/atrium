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
    let (start, end) = intervals(mi, n);
    assign(vregs, &start, &end, v_first, s_first, 256).map_err(|f| f.error())
}

/// Why assignment failed: register `reg` found no room while `active`
/// were live.
pub struct Failure {
    /// The register that did not fit.
    pub reg: usize,
    /// Registers live at that point.
    pub active: Vec<usize>,
    /// Its class.
    pub class: Class,
}

impl Failure {
    /// As a compile error.
    pub fn error(&self) -> Error {
        Error::Unsupported(format!("out of {:?} registers", self.class))
    }
}

/// Live interval `[start, end]` (instruction positions) of every virtual
/// register, extended across loops; `start == usize::MAX` if unused.
pub fn intervals(mi: &[MI], n: usize) -> (Vec<usize>, Vec<usize>) {
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
    (start, end)
}

/// First-fit assignment over the intervals, with at most `v_limit` VGPRs.
pub fn assign(vregs: &[VRegInfo], start: &[usize], end: &[usize], v_first: u8, s_first: u8, v_limit: u32) -> Result<Assignment, Failure> {
    let n = vregs.len();
    let mut order: Vec<usize> = (0..n).filter(|r| start[*r] != usize::MAX).collect();
    order.sort_by_key(|r| (start[*r], *r));
    let mut phys = vec![0u8; n];
    let mut active: Vec<usize> = vec![];
    let (mut vmax, mut smax) = (v_first as u32, s_first as u32);
    for r in order {
        active.retain(|a| end[*a] >= start[r]);
        let info = vregs[r];
        let (first, limit) = match info.class {
            Class::V => (v_first as u32, v_limit),
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
        // SMEM destinations: 2 SGPRs even-aligned, 3+ aligned to 4.
        let align = match (info.class, info.size) {
            (Class::S, 2) => 2,
            (Class::S, n) if n > 2 => 4,
            _ => 1,
        };
        let Some(base) = (first..=limit.saturating_sub(info.size as u32)).filter(|b| b % align == 0).find(|b| !busy(*b)) else {
            return Err(Failure { reg: r, active: active.clone(), class: info.class });
        };
        phys[r] = base as u8;
        match info.class {
            Class::V => vmax = vmax.max(base + info.size as u32),
            Class::S => smax = smax.max(base + info.size as u32),
        }
        active.push(r);
    }
    Ok(Assignment { phys, vgprs: vmax, sgprs: smax })
}
