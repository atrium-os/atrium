//! Final emission: physical registers, wait-counter and hazard insertion,
//! encoding, branch fixups.
//!
//! Waits (GFX12 split counters): global loads and returning atomics count on
//! LOADCNT and complete in order, so a use of a pending result waits until
//! only the younger loads are outstanding. At labels and branches all loads
//! are drained (conservative; refined later).
//!
//! Hazards (conservative first cut, to be narrowed against the ISA guide and
//! R9700 microtests):
//! * an SGPR written by a VALU (compares, `v_div_scale`) is read by anything
//!   → `s_wait_alu` with `va_sdst = 0` first;
//! * an SGPR read by a VALU as a lane mask or operand is then written by a
//!   SALU → `s_wait_alu` with `sa_sdst = 0` first;
//! * a transcendental result (`v_rcp`) is read by a VALU → `s_wait_alu`
//!   with `va_vdst = 0` first.

use crate::asm::{inline_const, Asm, EXEC_LO, LITERAL};
use crate::mir::*;
use crate::regalloc::Assignment;
use std::collections::{HashMap, HashSet, VecDeque};

const VCC_LO: u16 = 106;
/// `s_wait_alu` immediates (DEPCTR fields; 0xffff waits for nothing).
const WAIT_VA_SDST: u16 = 0xf1ff;
const WAIT_SA_SDST: u16 = 0xfffe;
const WAIT_VA_VDST: u16 = 0x0fff;

struct E<'a> {
    a: Asm,
    asg: &'a Assignment,
    vregs: &'a [VRegInfo],
    loads: VecDeque<Vec<u16>>,
    valu_wrote_s: HashSet<u16>,
    valu_read_s: HashSet<u16>,
    trans: HashSet<u16>,
    labels: HashMap<u32, usize>,
    fixups: Vec<(usize, u32)>,
}

impl<'a> E<'a> {
    /// Operand code, plus the register codes it covers (`n` registers).
    fn code(&self, o: O) -> u16 {
        match o {
            O::R(r, c) => {
                let p = self.asg.phys[r as usize] as u16 + c as u16;
                match self.vregs[r as usize].class {
                    Class::V => 256 + p,
                    Class::S => p,
                }
            }
            O::PV(p) => 256 + p as u16,
            O::PS(p) => p as u16,
            O::Exec => EXEC_LO,
            O::Vcc => VCC_LO,
            O::Imm(b) => inline_const(b).unwrap_or(LITERAL),
        }
    }

    fn regs(&self, o: O, n: u16) -> Vec<u16> {
        match o {
            O::Imm(_) => vec![],
            _ => {
                let c = self.code(o);
                (c..c + n).collect()
            }
        }
    }

    fn wait_for(&mut self, regs: &[u16]) {
        if let Some(i) = self.loads.iter().rposition(|l| l.iter().any(|r| regs.contains(r))) {
            let left = self.loads.len() - 1 - i;
            self.a.sopp("S_WAIT_LOADCNT", left as u16);
            self.loads.drain(..=i);
        }
    }

    fn drain(&mut self) {
        if !self.loads.is_empty() {
            self.a.sopp("S_WAIT_LOADCNT", 0);
            self.loads.clear();
        }
    }

    fn sgpr_reads(&mut self, codes: &[u16]) {
        if codes.iter().any(|c| self.valu_wrote_s.contains(c)) {
            self.a.sopp("S_WAIT_ALU", WAIT_VA_SDST);
            self.valu_wrote_s.clear();
        }
    }

    fn emit(&mut self, m: &MI) {
        match m {
            MI::Valu { name, dst, sdst, src, neg } => {
                let codes: Vec<u16> = src.iter().map(|s| self.code(*s)).collect();
                let vreads: Vec<u16> = codes.iter().copied().filter(|c| *c >= 256).collect();
                let sreads: Vec<u16> = codes.iter().copied().filter(|c| *c < 106 || *c == VCC_LO).collect();
                let mut sreads = sreads;
                if *name == "V_DIV_FMAS_F32" {
                    sreads.push(VCC_LO);
                }
                let d = self.code(*dst);
                let mut touch = vreads.clone();
                if d >= 256 {
                    touch.push(d);
                }
                self.wait_for(&touch);
                self.sgpr_reads(&sreads);
                if vreads.iter().any(|r| self.trans.contains(r)) {
                    self.a.sopp("S_WAIT_ALU", WAIT_VA_VDST);
                    self.trans.clear();
                }
                for s in &sreads {
                    self.valu_read_s.insert(*s);
                }
                let vdst_field = if d >= 256 { d - 256 } else { d };
                let names = ["SRC0", "SRC1", "SRC2"];
                let mut f: Vec<(&str, u64)> = vec![("VDST", vdst_field as u64)];
                for (i, c) in codes.iter().enumerate() {
                    f.push((names[i], *c as u64));
                }
                if *neg != 0 {
                    f.push(("NEG", *neg as u64));
                }
                let enc = if let Some(sd) = sdst {
                    let s = self.code(*sd);
                    f.push(("SDST", s as u64));
                    self.valu_wrote_s.insert(s);
                    "VOP3_SDST_ENC"
                } else {
                    "ENC_VOP3"
                };
                self.a.emit(name, enc, &f);
                if let Some(l) = src.iter().find_map(|s| match s {
                    O::Imm(b) if inline_const(*b).is_none() => Some(*b),
                    _ => None,
                }) {
                    self.a.words.push(l);
                }
                if d < 256 {
                    self.valu_wrote_s.insert(d);
                }
                if d >= 256 && (*name == "V_RCP_F32" || *name == "V_SQRT_F32") {
                    self.trans.insert(d);
                }
            }
            MI::Salu { name, dst, src } => {
                let codes: Vec<u16> = src.iter().map(|s| self.code(*s)).collect();
                self.sgpr_reads(&codes);
                let d = self.code(*dst);
                if self.valu_read_s.contains(&d) {
                    self.a.sopp("S_WAIT_ALU", WAIT_SA_SDST);
                    self.valu_read_s.clear();
                }
                if codes.len() == 1 {
                    self.a.sop1(name, d, codes[0]);
                } else {
                    self.a.sop2(name, d, codes[0], codes[1]);
                }
                if let Some(l) = src.iter().find_map(|s| match s {
                    O::Imm(b) if inline_const(*b).is_none() => Some(*b),
                    _ => None,
                }) {
                    self.a.words.push(l);
                }
            }
            MI::Global { name, vdst, vaddr, saddr, data, ioff, th, scope } => {
                let width = |n: &str| -> u16 {
                    if n.ends_with("B128") { 4 } else if n.ends_with("B96") { 3 } else if n.ends_with("B64") { 2 } else { 1 }
                };
                let w = width(name);
                let mut touch = self.regs(*vaddr, 1);
                if let Some(x) = data {
                    touch.extend(self.regs(*x, if name.contains("ATOMIC") { 1 } else { w }));
                }
                let dregs = vdst.map(|x| self.regs(x, if name.contains("ATOMIC") { 1 } else { w })).unwrap_or_default();
                touch.extend(dregs.iter().copied());
                self.wait_for(&touch);
                self.sgpr_reads(&[*saddr as u16, *saddr as u16 + 1]);
                let mut f: Vec<(&str, u64)> = vec![
                    ("VADDR", (self.code(*vaddr) - 256) as u64),
                    ("SADDR", *saddr as u64),
                    ("IOFFSET", (*ioff as u32 & 0xff_ffff) as u64),
                    ("TH", *th as u64),
                    ("SCOPE", *scope as u64),
                ];
                if let Some(x) = vdst {
                    f.push(("VDST", (self.code(*x) - 256) as u64));
                }
                if let Some(x) = data {
                    f.push(("VSRC", (self.code(*x) - 256) as u64));
                }
                self.a.emit(name, "ENC_VGLOBAL", &f);
                if !dregs.is_empty() {
                    self.loads.push_back(dregs);
                }
            }
            MI::Label(l) => {
                self.drain();
                self.labels.insert(*l, self.a.pos());
            }
            MI::Branch { name, target } => {
                self.drain();
                let p = self.a.branch(name);
                self.fixups.push((p, *target));
            }
            MI::LoopBegin | MI::LoopEnd => {}
            MI::Endpgm => self.a.sopp("S_ENDPGM", 0),
        }
    }
}

/// Emit machine code for `mi` under `asg`.
pub fn emit(mi: &[MI], vregs: &[VRegInfo], asg: &Assignment) -> Vec<u32> {
    let mut e = E {
        a: Asm::default(),
        asg,
        vregs,
        loads: VecDeque::new(),
        valu_wrote_s: HashSet::new(),
        valu_read_s: HashSet::new(),
        trans: HashSet::new(),
        labels: HashMap::new(),
        fixups: vec![],
    };
    for m in mi {
        e.emit(m);
    }
    for (at, l) in std::mem::take(&mut e.fixups) {
        let target = e.labels[&l];
        e.a.patch_branch(at, target);
    }
    e.a.words
}
