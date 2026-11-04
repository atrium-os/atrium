//! Register spilling, stage 1: spill everywhere on allocation failure
//! (docs: "RDNA4 register spilling design").
//!
//! When first-fit assignment runs out of VGPRs, the cheapest register live
//! at that point gets a slot in the lane's scratch space; every definition
//! is followed by a store of the components it wrote and every use is
//! preceded by a reload into a fresh short-lived register. Then the
//! intervals are recomputed and assignment runs again.
//!
//! EXEC: a store writes only the active lanes, exactly as the definition
//! wrote only those lanes of the register, so the slot holds what the
//! register would. A reload fills the lanes the use reads, except for
//! cross-lane reads, which reload with EXEC set to all lanes.
//!
//! Scratch accesses use ST mode (no address registers). The RDNA4 ISA guide
//! §5.7.1.2 says stores need no wait before their source is overwritten and
//! same-address store/load pairs from one wave stay in order, so the only
//! waits are LOADcnt waits for reloads, which emit already tracks.

use crate::mir::*;
use crate::regalloc::{assign, intervals, Assignment};
use crate::Error;
use std::collections::HashSet;

/// Allocation with spilling: the assignment and the scratch dwords per lane.
pub fn allocate(
    mi: &mut Vec<MI>,
    vregs: &mut Vec<VRegInfo>,
    v_first: u8,
    s_first: u8,
    v_limit: u32,
) -> Result<(Assignment, u32), Error> {
    let mut temps: HashSet<usize> = HashSet::new();
    let mut slots = 0u32;
    loop {
        let (start, end) = intervals(mi, vregs.len());
        let f = match assign(vregs, &start, &end, v_first, s_first, v_limit) {
            Ok(a) => return Ok((a, slots)),
            Err(f) => f,
        };
        if f.class != Class::V {
            return Err(f.error());
        }
        let depth = loop_depth(mi);
        let cost = |r: usize| -> f64 {
            let mut c = 0f64;
            for (p, m) in mi.iter().enumerate() {
                let (u, d) = m.regs();
                let k = u.iter().chain(&d).filter(|x| **x as usize == r).count();
                c += k as f64 * 10f64.powi(depth[p].min(6) as i32);
            }
            c / (end[r] - start[r] + 1) as f64
        };
        let victim = f
            .active
            .iter()
            .copied()
            .chain([f.reg])
            .filter(|r| vregs[*r].class == Class::V && !temps.contains(r))
            .map(|r| (cost(r), r))
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, r)| r)
            .ok_or_else(|| f.error())?;
        let size = vregs[victim].size as u32;
        let base = if size > 1 { slots.next_multiple_of(4) } else { slots };
        slots = base + size;
        rewrite(mi, vregs, &mut temps, victim as VR, base);
    }
}

fn loop_depth(mi: &[MI]) -> Vec<u32> {
    let mut d = 0;
    mi.iter()
        .map(|m| {
            match m {
                MI::LoopBegin => d += 1,
                MI::LoopEnd => d -= 1,
                _ => {}
            }
            d
        })
        .collect()
}

/// Reads other lanes' values, so a reload must fill every lane.
fn cross_lane(m: &MI) -> bool {
    match m {
        MI::Valu { name, .. } => ["READLANE", "READFIRSTLANE", "PERMLANE", "_DPP"].iter().any(|x| name.contains(x)),
        MI::Ds { name, .. } => ["PERMUTE", "SWIZZLE"].iter().any(|x| name.contains(x)),
        _ => false,
    }
}

/// Scratch accesses covering components `lo..hi` of a group whose slot
/// starts at dword `base`: b128 / b64 where the slot alignment allows, b32
/// otherwise (ST-mode IOFFSET must be aligned to the access size).
fn accesses(base: u32, lo: u32, hi: u32) -> Vec<(u32, u32)> {
    let mut v = vec![];
    let mut k = lo;
    while k < hi {
        let at = base + k;
        let w = if hi - k >= 4 && at % 4 == 0 { 4 } else if hi - k >= 2 && at % 2 == 0 { 2 } else { 1 };
        v.push((k, w));
        k += w;
    }
    v
}

fn name(load: bool, w: u32) -> &'static str {
    match (load, w) {
        (true, 4) => "SCRATCH_LOAD_B128",
        (true, 2) => "SCRATCH_LOAD_B64",
        (true, _) => "SCRATCH_LOAD_B32",
        (false, 4) => "SCRATCH_STORE_B128",
        (false, 2) => "SCRATCH_STORE_B64",
        (false, _) => "SCRATCH_STORE_B32",
    }
}

fn rewrite(mi: &mut Vec<MI>, vregs: &mut Vec<VRegInfo>, temps: &mut HashSet<usize>, x: VR, base: u32) {
    let old = std::mem::take(mi);
    for mut m in old {
        // Component ranges this instruction reads and writes.
        let (mut ulo, mut uhi, mut dlo, mut dhi) = (u32::MAX, 0u32, u32::MAX, 0u32);
        for (o, def, w) in m.operands_mut() {
            if let O::R(r, k) = *o {
                if r == x {
                    let (lo, hi) = (k as u32, k as u32 + w as u32);
                    if def {
                        dlo = dlo.min(lo);
                        dhi = dhi.max(hi);
                    } else {
                        ulo = ulo.min(lo);
                        uhi = uhi.max(hi);
                    }
                }
            }
        }
        if ulo == u32::MAX && dlo == u32::MAX {
            mi.push(m);
            continue;
        }
        let (lo, hi) = (ulo.min(dlo), uhi.max(dhi));
        let t = vregs.len() as VR;
        vregs.push(VRegInfo { class: Class::V, size: (hi - lo) as u8 });
        temps.insert(t as usize);
        for (o, _, _) in m.operands_mut() {
            if let O::R(r, k) = *o {
                if r == x {
                    *o = O::R(t, k - lo as u8);
                }
            }
        }
        if ulo != u32::MAX {
            let all = cross_lane(&m);
            let saved = all.then(|| {
                let s = vregs.len() as VR;
                vregs.push(VRegInfo { class: Class::S, size: 1 });
                temps.insert(s as usize);
                mi.push(MI::Salu { name: "S_MOV_B32", dst: O::R(s, 0), src: vec![O::Exec] });
                mi.push(MI::Salu { name: "S_MOV_B32", dst: O::Exec, src: vec![O::Imm(u32::MAX)] });
                s
            });
            for (k, w) in accesses(base, ulo, uhi) {
                mi.push(MI::Scratch { name: name(true, w), vdst: Some(O::R(t, (k - lo) as u8)), data: None, ioff: ((base + k) * 4) as i32 });
            }
            if let Some(s) = saved {
                mi.push(MI::Salu { name: "S_MOV_B32", dst: O::Exec, src: vec![O::R(s, 0)] });
            }
        }
        mi.push(m);
        if dlo != u32::MAX {
            for (k, w) in accesses(base, dlo, dhi) {
                mi.push(MI::Scratch { name: name(false, w), vdst: None, data: Some(O::R(t, (k - lo) as u8)), ioff: ((base + k) * 4) as i32 });
            }
        }
    }
}
