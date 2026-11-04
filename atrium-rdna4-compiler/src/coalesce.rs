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
