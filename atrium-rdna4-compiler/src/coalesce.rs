//! Copy coalescing on the machine IR.
//!
//! `v_mov_b32 g.k, x` where `x` is a one-register VGPR value defined once
//! and read only by this copy: the definition writes `g.k` directly and the
//! copy goes. Gathering scattered results into a contiguous group (store
//! data, vector results) is where these copies come from.
//!
//! Only within straight-line code: between the definition and the copy
//! there may be no label, branch, loop marker or EXEC write (so both run
//! under the same lane mask) and no other reference to `g`.

use crate::mir::*;

fn dst_mut(m: &mut MI) -> Option<&mut O> {
    match m {
        MI::Valu { dst, .. } => Some(dst),
        MI::Global { vdst: Some(d), .. } | MI::Ds { vdst: Some(d), .. } => Some(d),
        _ => None,
    }
}

fn barrier(m: &MI) -> bool {
    matches!(m, MI::Label(_) | MI::Branch { .. } | MI::LoopBegin | MI::LoopEnd | MI::Endpgm)
        || matches!(m, MI::Salu { dst: O::Exec, .. })
}

/// Coalesce copies in place.
pub fn run(mi: &mut Vec<MI>, vregs: &[VRegInfo]) {
    let n = vregs.len();
    let (mut uses, mut defs) = (vec![0u32; n], vec![0u32; n]);
    let mut def_at = vec![usize::MAX; n];
    for (i, m) in mi.iter().enumerate() {
        let (u, d) = m.regs();
        for r in u {
            uses[r as usize] += 1;
        }
        for r in d {
            defs[r as usize] += 1;
            def_at[r as usize] = i;
        }
    }
    let mut dead = vec![false; mi.len()];
    for j in 0..mi.len() {
        let (g, k, x) = match &mi[j] {
            MI::Valu { name: "V_MOV_B32", dst: O::R(g, k), sdst: None, src, neg: 0 } => match src[..] {
                [O::R(x, 0)] => (*g, *k, x),
                _ => continue,
            },
            _ => continue,
        };
        let (xi, gi) = (vregs[x as usize], vregs[g as usize]);
        if x == g || xi.class != Class::V || xi.size != 1 || gi.class != Class::V {
            continue;
        }
        if uses[x as usize] != 1 || defs[x as usize] != 1 {
            continue;
        }
        let i = def_at[x as usize];
        if i >= j {
            continue;
        }
        let refs_g = |m: &MI| {
            let (u, d) = m.regs();
            u.contains(&g) || d.contains(&g)
        };
        if mi[i..j].iter().enumerate().any(|(o, m)| !dead[i + o] && (barrier(m) || refs_g(m))) {
            continue;
        }
        match dst_mut(&mut mi[i]) {
            Some(d) if *d == O::R(x, 0) => *d = O::R(g, k),
            _ => continue,
        }
        dead[j] = true;
    }
    let mut it = dead.iter();
    mi.retain(|_| !*it.next().unwrap());
}

/// Merge copy-connected VGPR groups whose live ranges only touch at the
/// copy: `p ← i` (component by component) where every reference to `p` or
/// `i` inside the window from `p`'s first to `i`'s last reference is one of
/// those copies. `i` is renamed to `p` and the copies go. Intervals are the
/// allocator's (extended across loops), so a value still live around a
/// loop is never merged. This removes the per-construct copies of loop
/// parameters and `if` results.
pub fn merge(mi: &mut Vec<MI>, vregs: &[VRegInfo]) {
    loop {
        let (start, end) = crate::regalloc::intervals(mi, vregs.len());
        // Every instruction position that references each register.
        let mut refs: Vec<Vec<usize>> = vec![vec![]; vregs.len()];
        for (pos, m) in mi.iter().enumerate() {
            let (u, d) = m.regs();
            for r in u.into_iter().chain(d) {
                let v = &mut refs[r as usize];
                if v.last() != Some(&pos) {
                    v.push(pos);
                }
            }
        }
        let mut done = None;
        'scan: for (j, m) in mi.iter().enumerate() {
            let MI::Valu { name: "V_MOV_B32", dst: O::R(p, _), sdst: None, src, neg: 0 } = m else { continue };
            let [O::R(i, _)] = src[..] else { continue };
            let (p, i) = (*p, i);
            let (pi, ii) = (vregs[p as usize], vregs[i as usize]);
            if p == i || pi.class != Class::V || ii.class != Class::V || pi.size != ii.size {
                continue;
            }
            let (ps, ie) = (start[p as usize], end[i as usize]);
            if ps > j || ie < j {
                continue;
            }
            let mut comps = 0u32;
            for pos in ps..=ie {
                let in_p = refs[p as usize].binary_search(&pos).is_ok();
                let in_i = refs[i as usize].binary_search(&pos).is_ok();
                if !in_p && !in_i {
                    continue;
                }
                match &mi[pos] {
                    MI::Valu { name: "V_MOV_B32", dst: O::R(a, k), sdst: None, src, neg: 0 }
                        if *a == p && src[..] == [O::R(i, *k)] =>
                    {
                        comps |= 1 << k;
                    }
                    _ => continue 'scan,
                }
            }
            if comps != (1u32 << pi.size) - 1 {
                continue;
            }
            done = Some((p, i, ps, ie));
            break;
        }
        let Some((p, i, ps, ie)) = done else { return };
        let ren = |o: &mut O| {
            if let O::R(r, _) = o {
                if *r == i {
                    *r = p;
                }
            }
        };
        let mut keep = vec![true; mi.len()];
        for (pos, m) in mi.iter_mut().enumerate() {
            if (ps..=ie).contains(&pos) && matches!(m, MI::Valu { name: "V_MOV_B32", dst: O::R(a, _), .. } if *a == p) {
                keep[pos] = false;
                continue;
            }
            match m {
                MI::Valu { dst, sdst, src, .. } => {
                    ren(dst);
                    if let Some(s) = sdst {
                        ren(s);
                    }
                    src.iter_mut().for_each(ren);
                }
                MI::Salu { dst, src, .. } => {
                    ren(dst);
                    src.iter_mut().for_each(ren);
                }
                MI::Global { vdst, vaddr, data, .. } => {
                    vdst.iter_mut().for_each(ren);
                    ren(vaddr);
                    data.iter_mut().for_each(ren);
                }
                MI::Ds { vdst, addr, data, .. } => {
                    vdst.iter_mut().for_each(ren);
                    ren(addr);
                    data.iter_mut().for_each(ren);
                }
                MI::Smem { sdst, soff, .. } => {
                    ren(sdst);
                    soff.iter_mut().for_each(ren);
                }
                _ => {}
            }
        }
        let mut it = keep.iter();
        mi.retain(|_| *it.next().unwrap());
    }
}
