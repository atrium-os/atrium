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
//! SGPRs spill into lanes of a VGPR (`v_writelane_b32` /
//! `v_readlane_b32`, which ignore EXEC): one VGPR holds 32 spilled SGPRs.
//! That VGPR is never itself spilled.
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
/// Reloads are reused through a straight run; if the longer reload
/// intervals leave no room, it falls back to a reload at every use.
pub fn allocate(
    mi: &mut Vec<MI>,
    vregs: &mut Vec<VRegInfo>,
    v_first: u8,
    s_first: u8,
    v_limit: u32,
    s_limit: u32,
) -> Result<(Assignment, u32), Error> {
    let (mi0, vregs0) = (mi.clone(), vregs.clone());
    match allocate_with(mi, vregs, v_first, s_first, v_limit, s_limit, true) {
        Ok(r) => Ok(r),
        Err(_) => {
            *mi = mi0;
            *vregs = vregs0;
            allocate_with(mi, vregs, v_first, s_first, v_limit, s_limit, false)
        }
    }
}

fn allocate_with(
    mi: &mut Vec<MI>,
    vregs: &mut Vec<VRegInfo>,
    v_first: u8,
    s_first: u8,
    v_limit: u32,
    s_limit: u32,
    reuse: bool,
) -> Result<(Assignment, u32), Error> {
    let mut temps: HashSet<usize> = HashSet::new();
    let mut slots = 0u32;
    // SGPR spill VGPRs and how many of their lanes are taken.
    let mut lanes: Vec<(VR, u32)> = vec![];
    loop {
        let (start, end) = intervals(mi, vregs.len());
        let f = match assign(vregs, &start, &end, v_first, s_first, v_limit, s_limit) {
            Ok(a) => return Ok((a, slots)),
            Err(f) => f,
        };
        let depth = loop_depth(mi);
        let cost = |r: usize| -> f64 {
            let mut c = 0f64;
            for (p, m) in mi.iter().enumerate() {
                let (u, d) = m.regs();
                let k = u.iter().chain(&d).filter(|x| **x as usize == r).count();
                c += k as f64 * 10f64.powi(depth[p].min(6) as i32);
            }
            let c = c / (end[r] - start[r] + 1) as f64;
            // Recomputing costs one VALU op, a reload a memory round trip.
            if remat_def(mi, r as VR).is_some() { c * 0.1 } else { c }
        };
        let victim = f
            .active
            .iter()
            .copied()
            .chain([f.reg])
            .filter(|r| vregs[*r].class == f.class && !temps.contains(r))
            .map(|r| (cost(r), r))
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, r)| r)
            .ok_or_else(|| f.error())?;
        if f.class == Class::S {
            let n = vregs[victim].size as u32;
            let at = match lanes.last() {
                Some(&(_, used)) if used + n <= 32 => lanes.len() - 1,
                _ => {
                    let l = vregs.len() as VR;
                    vregs.push(VRegInfo { class: Class::V, size: 1 });
                    temps.insert(l as usize);
                    lanes.push((l, 0));
                    lanes.len() - 1
                }
            };
            let (l, first) = lanes[at];
            lanes[at].1 += n;
            rewrite_s(mi, vregs, &mut temps, victim as VR, l, first);
            continue;
        }
        if let Some(def) = remat_def(mi, victim as VR) {
            remat(mi, vregs, &mut temps, victim as VR, def);
            continue;
        }
        let size = vregs[victim].size as u32;
        let base = if size > 1 { slots.next_multiple_of(4) } else { slots };
        slots = base + size;
        rewrite(mi, vregs, &mut temps, victim as VR, base, reuse);
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

/// The single definition of `x`, if it can be recomputed at each use
/// instead of spilled: a one-register `v_mov_b32` of a constant or SGPR
/// (registers no instruction writes). Recomputing under the use's EXEC
/// gives the value in every lane the use reads.
fn remat_def(mi: &[MI], x: VR) -> Option<MI> {
    let mut def = None;
    for m in mi {
        let (_, d) = m.regs();
        if d.contains(&x) {
            if def.is_some() {
                return None;
            }
            def = Some(m);
        }
    }
    match def? {
        m @ MI::Valu { name: "V_MOV_B32", dst: O::R(_, 0), sdst: None, src, neg: 0 }
            if matches!(src[..], [O::Imm(_)]) =>
        {
            Some(m.clone())
        }
        _ => None,
    }
}

/// Replace `x` by a recomputation of `def` before each use.
fn remat(mi: &mut Vec<MI>, vregs: &mut Vec<VRegInfo>, temps: &mut HashSet<usize>, x: VR, def: MI) {
    let old = std::mem::take(mi);
    for mut m in old {
        let (u, d) = m.regs();
        if d.contains(&x) {
            continue; // the original definition goes
        }
        if !u.contains(&x) {
            mi.push(m);
            continue;
        }
        let t = vregs.len() as VR;
        vregs.push(VRegInfo { class: Class::V, size: 1 });
        temps.insert(t as usize);
        let mut again = def.clone();
        for (o, is_def, _) in again.operands_mut() {
            if is_def {
                *o = O::R(t, 0);
            }
        }
        for (o, _, _) in m.operands_mut() {
            if *o == O::R(x, 0) {
                *o = O::R(t, 0);
            }
        }
        mi.push(again);
        mi.push(m);
    }
}

/// Spill SGPR group `x` into lanes `first..` of VGPR `l`: a writelane
/// after each def, a readlane into a fresh SGPR before each use.
fn rewrite_s(mi: &mut Vec<MI>, vregs: &mut Vec<VRegInfo>, temps: &mut HashSet<usize>, x: VR, l: VR, first: u32) {
    let old = std::mem::take(mi);
    for mut m in old {
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
        vregs.push(VRegInfo { class: Class::S, size: (hi - lo) as u8 });
        temps.insert(t as usize);
        for (o, _, _) in m.operands_mut() {
            if let O::R(r, k) = *o {
                if r == x {
                    *o = O::R(t, k - lo as u8);
                }
            }
        }
        if ulo != u32::MAX {
            for k in ulo..uhi {
                mi.push(MI::Valu { name: "V_READLANE_B32", dst: O::R(t, (k - lo) as u8), sdst: None, src: vec![O::R(l, 0), O::Imm(first + k)], neg: 0 });
            }
        }
        mi.push(m);
        if dlo != u32::MAX {
            for k in dlo..dhi {
                mi.push(MI::Valu { name: "V_WRITELANE_B32", dst: O::R(l, 0), sdst: None, src: vec![O::R(t, (k - lo) as u8), O::Imm(first + k)], neg: 0 });
            }
        }
    }
}

/// Ends a straight run: a reloaded value cannot be reused past it (other
/// lanes may be active, or control may arrive from elsewhere).
fn run_end(m: &MI) -> bool {
    matches!(m, MI::Label(_) | MI::Branch { .. } | MI::LoopBegin | MI::LoopEnd | MI::Endpgm | MI::Barrier)
        || matches!(m, MI::Salu { dst: O::Exec, .. })
        || matches!(m, MI::Salu { name, .. } if name.contains("EXEC"))
        || matches!(m, MI::Valu { name, .. } if name.contains("CMPX"))
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

fn rewrite(mi: &mut Vec<MI>, vregs: &mut Vec<VRegInfo>, temps: &mut HashSet<usize>, x: VR, base: u32, reuse: bool) {
    let old = std::mem::take(mi);
    // The register holding components of `x` in this straight run: (temp,
    // its first component, its size, mask of components it holds).
    let mut cache: Option<(VR, u32, u32, u32)> = None;
    for mut m in old {
        if run_end(&m) {
            cache = None;
        }
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
        let range = |a: u32, b: u32| if a >= b { 0 } else { ((1u64 << b) - (1u64 << a)) as u32 };
        let all = cross_lane(&m);
        // Reuse the run's register if it covers this instruction and
        // holds every component read.
        let hit = cache.filter(|&(_, clo, csize, mask)| {
            !all && clo <= lo && hi <= clo + csize && (ulo == u32::MAX || range(ulo, uhi) & !mask == 0)
        });
        let (t, tlo) = match hit {
            Some((t, clo, _, _)) => (t, clo),
            None => {
                let t = vregs.len() as VR;
                vregs.push(VRegInfo { class: Class::V, size: (hi - lo) as u8 });
                temps.insert(t as usize);
                (t, lo)
            }
        };
        for (o, _, _) in m.operands_mut() {
            if let O::R(r, k) = *o {
                if r == x {
                    *o = O::R(t, k - tlo as u8);
                }
            }
        }
        let mut mask = match hit {
            Some((_, _, _, mask)) => mask,
            None => 0,
        };
        if ulo != u32::MAX && hit.is_none() {
            mask |= range(ulo, uhi);
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
        let ends = run_end(&m);
        mi.push(m);
        if dlo != u32::MAX {
            mask |= range(dlo, dhi);
            for (k, w) in accesses(base, dlo, dhi) {
                mi.push(MI::Scratch { name: name(false, w), vdst: None, data: Some(O::R(t, (k - tlo) as u8)), ioff: ((base + k) * 4) as i32 });
            }
        }
        // A reload for a cross-lane read ran with all lanes; its register
        // is fine to keep. An instruction that ends a run drops it.
        cache = if ends || !reuse { None } else {
            let size = vregs[t as usize].size as u32;
            Some((t, tlo, size, mask))
        };
    }
}
