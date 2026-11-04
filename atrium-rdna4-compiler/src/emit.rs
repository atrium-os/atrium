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
//!   with `va_vdst = 0` first;
//! * before every branch all of the above are resolved, since the tracking
//!   is linear and a loop's back edge would otherwise skip it.

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
    /// Outstanding LDS ops in issue order (returned registers; empty for
    /// stores). DSCNT counts them all and LDS returns in order.
    ds: VecDeque<Vec<u16>>,
    valu_wrote_s: HashSet<u16>,
    valu_read_s: HashSet<u16>,
    trans: HashSet<u16>,
    /// SGPRs an outstanding SMEM load will write (KMCNT; out of order).
    smem: HashSet<u16>,
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
        if let Some(i) = self.ds.iter().rposition(|l| l.iter().any(|r| regs.contains(r))) {
            let left = self.ds.len() - 1 - i;
            self.a.sopp("S_WAIT_DSCNT", left as u16);
            self.ds.drain(..=i);
        }
    }

    /// Wait for every SMEM load if any of `regs` is one of its destinations.
    fn wait_k(&mut self, regs: &[u16]) {
        if regs.iter().any(|r| self.smem.contains(r)) {
            self.a.sopp("S_WAIT_KMCNT", 0);
            self.smem.clear();
        }
    }

    fn drain(&mut self) {
        if !self.smem.is_empty() {
            self.a.sopp("S_WAIT_KMCNT", 0);
            self.smem.clear();
        }
        if !self.loads.is_empty() {
            self.a.sopp("S_WAIT_LOADCNT", 0);
            self.loads.clear();
        }
        self.drain_ds();
    }

    fn drain_ds(&mut self) {
        if !self.ds.is_empty() {
            self.a.sopp("S_WAIT_DSCNT", 0);
            self.ds.clear();
        }
    }

    fn sgpr_reads(&mut self, codes: &[u16]) {
        if codes.iter().any(|c| self.valu_wrote_s.contains(c)) {
            self.a.sopp("S_WAIT_ALU", WAIT_VA_SDST);
            self.valu_wrote_s.clear();
        }
    }

    /// Resolve every tracked ALU hazard. Tracking is linear, so before a
    /// branch (a loop's back edge in particular) nothing may stay pending.
    fn flush_alu(&mut self) {
        let mut w = 0xffffu16;
        if !self.valu_wrote_s.is_empty() {
            w &= WAIT_VA_SDST;
        }
        if !self.valu_read_s.is_empty() {
            w &= WAIT_SA_SDST;
        }
        if !self.trans.is_empty() {
            w &= WAIT_VA_VDST;
        }
        if w != 0xffff {
            self.a.sopp("S_WAIT_ALU", w);
        }
        self.valu_wrote_s.clear();
        self.valu_read_s.clear();
        self.trans.clear();
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
                let mut kregs = sreads.clone();
                if d < 256 {
                    kregs.push(d);
                }
                if let Some(sd) = sdst {
                    kregs.push(self.code(*sd));
                }
                self.wait_k(&kregs);
                self.sgpr_reads(&sreads);
                if vreads.iter().any(|r| self.trans.contains(r)) {
                    self.a.sopp("S_WAIT_ALU", WAIT_VA_VDST);
                    self.trans.clear();
                }
                for s in &sreads {
                    self.valu_read_s.insert(*s);
                }
                let vdst_field = if d >= 256 { d - 256 } else { d };
                // Compact VOP1 / VOP2 (one dword) when the operands allow it.
                if sdst.is_none() && *neg == 0 && d >= 256 {
                    if let Some((op, enc, c0, c1)) = compact(name, &codes) {
                        let mut f: Vec<(&str, u64)> = vec![("VDST", vdst_field as u64), ("SRC0", c0 as u64)];
                        if let Some(c1) = c1 {
                            f.push(("VSRC1", (c1 - 256) as u64));
                        }
                        self.a.emit(op, enc, &f);
                        if let Some(l) = src.iter().find_map(|s| match s {
                            O::Imm(b) if inline_const(*b).is_none() => Some(*b),
                            _ => None,
                        }) {
                            self.a.words.push(l);
                        }
                        if *name == "V_RCP_F32" || *name == "V_RCP_IFLAG_F32" || *name == "V_SQRT_F32" {
                            self.trans.insert(d);
                        }
                        return;
                    }
                }
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
                if d >= 256 && (*name == "V_RCP_F32" || *name == "V_RCP_IFLAG_F32" || *name == "V_SQRT_F32") {
                    self.trans.insert(d);
                }
            }
            MI::Salu { name, dst, src } => {
                let codes: Vec<u16> = src.iter().map(|s| self.code(*s)).collect();
                let d = self.code(*dst);
                let mut kregs = codes.clone();
                kregs.push(d);
                self.wait_k(&kregs);
                self.sgpr_reads(&codes);
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
            MI::Smem { name, sdst, sbase, soff, ioff } => {
                let n: u16 = match *name {
                    "S_LOAD_B32" => 1,
                    "S_LOAD_B64" => 2,
                    "S_LOAD_B96" => 3,
                    "S_LOAD_B128" => 4,
                    "S_LOAD_B256" => 8,
                    _ => 16,
                };
                let d = self.code(*sdst);
                let so = soff.map(|o| self.code(o));
                let mut reads = vec![*sbase as u16, *sbase as u16 + 1];
                reads.extend(so);
                let mut k = reads.clone();
                k.extend(d..d + n);
                self.wait_k(&k);
                self.sgpr_reads(&reads);
                if (d..d + n).any(|r| self.valu_read_s.contains(&r)) {
                    self.a.sopp("S_WAIT_ALU", WAIT_SA_SDST);
                    self.valu_read_s.clear();
                }
                self.a.emit(name, "ENC_SMEM", &[
                    ("SDATA", d as u64),
                    ("SBASE", (*sbase / 2) as u64),
                    ("SOFFSET", so.unwrap_or(crate::asm::NULL) as u64),
                    ("IOFFSET", (*ioff as u32 & 0xff_ffff) as u64),
                ]);
                self.smem.extend(d..d + n);
            }
            MI::Ds { name, vdst, addr, data, off } => {
                let w: u16 = if name.ends_with("B128") { 4 } else if name.ends_with("B64") { 2 } else { 1 };
                let mut touch = self.regs(*addr, 1);
                if let Some(x) = data {
                    touch.extend(self.regs(*x, w));
                }
                let dregs = vdst.map(|x| self.regs(x, w)).unwrap_or_default();
                touch.extend(dregs.iter().copied());
                self.wait_for(&touch);
                let mut f: Vec<(&str, u64)> = vec![
                    ("ADDR", (self.code(*addr) - 256) as u64),
                    ("OFFSET0", (*off & 0xff) as u64),
                    ("OFFSET1", (*off >> 8) as u64),
                ];
                if let Some(x) = vdst {
                    f.push(("VDST", (self.code(*x) - 256) as u64));
                }
                if let Some(x) = data {
                    f.push(("DATA0", (self.code(*x) - 256) as u64));
                }
                self.a.emit(name, "ENC_VDS", &f);
                self.ds.push_back(dregs);
            }
            MI::Barrier => {
                // LDS writes before the barrier are visible after it.
                self.drain_ds();
                self.a.emit("S_BARRIER_SIGNAL", "ENC_SOP1", &[("SSRC0", 193), ("SDST", 0)]); // -1: the workgroup barrier
                self.a.sopp("S_BARRIER_WAIT", 0xffff);
            }
            MI::Label(l) => {
                self.drain();
                self.labels.insert(*l, self.a.pos());
            }
            MI::Branch { name, target } => {
                self.drain();
                self.flush_alu();
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
        ds: VecDeque::new(),
        valu_wrote_s: HashSet::new(),
        valu_read_s: HashSet::new(),
        trans: HashSet::new(),
        smem: HashSet::new(),
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

/// The VOP1 / VOP2 form of a VOP3 op, if it has one and its sources fit:
/// (opcode, encoding, src0, VGPR src1). VOP2 needs a VGPR in src1, so
/// commutative ops (and sub, through its reversed opcode) may swap.
/// Opcodes with implicit VCC operands or accumulators are left in VOP3.
fn compact(name: &'static str, codes: &[u16]) -> Option<(&'static str, &'static str, u16, Option<u16>)> {
    use atrium_rdna4_isa::instruction;
    let has = |op: &str, enc: &str| {
        instruction(op).map_or(false, |d| d.encodings.iter().any(|e| atrium_rdna4_isa::ENCODINGS[e.encoding].name == enc))
    };
    if ["CNDMASK", "_CO_", "FMAC", "FMAMK", "FMAAK", "DOT2", "READ", "WRITELANE"].iter().any(|x| name.contains(x)) {
        return None;
    }
    match codes {
        [c0] if has(name, "ENC_VOP1") => Some((name, "ENC_VOP1", *c0, None)),
        [c0, c1] if has(name, "ENC_VOP2") => {
            if *c1 >= 256 {
                return Some((name, "ENC_VOP2", *c0, Some(*c1)));
            }
            if *c0 < 256 {
                return None;
            }
            const COMMUTATIVE: [&str; 14] = [
                "V_ADD_F32", "V_MUL_F32", "V_ADD_NC_U32", "V_AND_B32", "V_OR_B32", "V_XOR_B32",
                "V_MIN_NUM_F32", "V_MAX_NUM_F32", "V_MIN_U32", "V_MAX_U32", "V_MIN_I32", "V_MAX_I32",
                "V_MUL_U32_U24", "V_MUL_I32_I24",
            ];
            let swapped = if COMMUTATIVE.contains(&name) {
                name
            } else {
                match name {
                    "V_SUB_F32" => "V_SUBREV_F32",
                    "V_SUBREV_F32" => "V_SUB_F32",
                    "V_SUB_NC_U32" => "V_SUBREV_NC_U32",
                    "V_SUBREV_NC_U32" => "V_SUB_NC_U32",
                    _ => return None,
                }
            };
            has(swapped, "ENC_VOP2").then_some((swapped, "ENC_VOP2", *c1, Some(*c0)))
        }
        _ => None,
    }
}
