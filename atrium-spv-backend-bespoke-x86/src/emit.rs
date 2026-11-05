//! Emission: allocated VCode → x86-64 machine code through iced-x86.
//!
//! Scratch registers, never allocated: xmm14 and xmm15, eax/rax and
//! r11d/r11. A spilled operand is either used directly as a memory operand
//! (where the instruction has an r/m form) or loaded into a scratch. A
//! spilled result is computed into a scratch and stored.
//!
//! Frame: callee-saved GPRs that the allocator used are pushed, then
//! `sub rsp` reserves the 16-byte spill slots, keeping rsp 16-aligned.

use std::collections::HashMap;

use iced_x86::code_asm::*;
use iced_x86::BlockEncoderOptions;

use crate::regalloc::{Allocation, Loc};
use crate::vcode::{
    gpr, Addr, DivKind, FCond, GOp, GUnOp, ICond, Label, MInst, PReg, ShiftOp, VCode, VReg, XOp,
    XUnOp,
};
use crate::BackendError;

const XMM: [AsmRegisterXmm; 16] = [
    xmm0, xmm1, xmm2, xmm3, xmm4, xmm5, xmm6, xmm7, xmm8, xmm9, xmm10, xmm11, xmm12, xmm13, xmm14,
    xmm15,
];
const G64: [AsmRegister64; 16] =
    [rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15];
const G32: [AsmRegister32; 16] = [
    eax, ecx, edx, ebx, esp, ebp, esi, edi, r8d, r9d, r10d, r11d, r12d, r13d, r14d, r15d,
];

const XS0: AsmRegisterXmm = xmm14;
const XS1: AsmRegisterXmm = xmm15;

/// Callee-saved GPRs (SysV), in push order.
const CALLEE_SAVED: [u8; 6] = [3, 5, 12, 13, 14, 15];

pub(crate) fn enc(e: IcedError) -> BackendError {
    BackendError::Internal(format!("iced-x86: {e}"))
}

#[derive(Clone, Copy)]
enum G {
    R(AsmRegister32),
    M(AsmMemoryOperand),
}

#[derive(Clone, Copy)]
enum X {
    R(AsmRegisterXmm),
    M(AsmMemoryOperand),
}

/// `$a.$m($d, rm)` for a G operand.
macro_rules! g2 {
    ($a:expr, $m:ident, $d:expr, $s:expr) => {
        match $s {
            G::R(r) => $a.$m($d, r),
            G::M(m) => $a.$m($d, m),
        }
        .map_err(enc)
    };
}

/// `$a.$m($d, $s1, rm)` for an X operand.
macro_rules! x3 {
    ($a:expr, $m:ident, $d:expr, $s1:expr, $s2:expr) => {
        match $s2 {
            X::R(r) => $a.$m($d, $s1, r),
            X::M(m) => $a.$m($d, $s1, m),
        }
        .map_err(enc)
    };
}

struct Emit<'a> {
    a: CodeAssembler,
    al: &'a Allocation,
    frame: i32,
    saved: Vec<u8>,
    labels: HashMap<Label, CodeLabel>,
}

/// Emit a function. Returns code and `(code offset, spirv offset)` marks.
pub(crate) fn emit(vc: &VCode, alloc: &Allocation) -> Result<(Vec<u8>, Vec<(u32, u32)>), BackendError> {
    let mut used = [false; 16];
    for loc in &alloc.locs {
        if let Loc::Reg(PReg::G(r)) = loc {
            used[*r as usize] = true;
        }
    }
    let saved: Vec<u8> = CALLEE_SAVED.iter().copied().filter(|&r| used[r as usize]).collect();
    // On entry rsp ≡ 8 (mod 16). Keep it 16-aligned after the frame.
    let frame = if alloc.num_slots == 0 {
        0
    } else {
        let mut f = 16 * alloc.num_slots as i32;
        if (8 + 8 * saved.len() as i32 + f) % 16 != 0 {
            f += 8;
        }
        f
    };
    let mut e = Emit {
        a: CodeAssembler::new(64).map_err(enc)?,
        al: alloc,
        frame,
        saved,
        labels: HashMap::new(),
    };
    for i in 0..vc.num_labels {
        let l = e.a.create_label();
        e.labels.insert(Label(i), l);
    }

    let mut marks: Vec<(usize, u32)> = Vec::new();
    let insts = &vc.insts;
    for (i, inst) in insts.iter().enumerate() {
        if let MInst::Mark(s) = inst {
            marks.push((e.a.instructions().len(), *s));
            continue;
        }
        let next = next_label(insts, i + 1);
        e.inst(inst, next)?;
    }

    let result = e
        .a
        .assemble_options(0, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS)
        .map_err(enc)?;
    let offsets = result.inner.new_instruction_offsets;
    let code = result.inner.code_buffer;
    let pcs = marks
        .into_iter()
        .map(|(idx, spirv)| (offsets.get(idx).copied().unwrap_or(code.len() as u32), spirv))
        .collect();
    Ok((code, pcs))
}

/// The label placed right after position `i`, if only markers come first.
fn next_label(insts: &[MInst], mut i: usize) -> Option<Label> {
    while i < insts.len() {
        match &insts[i] {
            MInst::Mark(_) => i += 1,
            MInst::Label(l) => return Some(*l),
            _ => return None,
        }
    }
    None
}

impl<'a> Emit<'a> {
    fn loc(&self, v: VReg) -> Loc {
        self.al.locs[v.0 as usize]
    }

    fn slot_d(&self, s: u32) -> AsmMemoryOperand {
        dword_ptr(rsp + 16 * s as i32)
    }

    fn slot_x(&self, s: u32) -> AsmMemoryOperand {
        xmmword_ptr(rsp + 16 * s as i32)
    }

    fn label(&self, l: Label) -> CodeLabel {
        self.labels[&l]
    }

    // ── operand access ────────────────────────────────────────────────

    fn g(&self, v: VReg) -> G {
        match self.loc(v) {
            Loc::Reg(PReg::G(r)) => G::R(G32[r as usize]),
            Loc::Slot(s) => G::M(self.slot_d(s)),
            Loc::Reg(PReg::X(_)) => unreachable!("GPR vreg in an XMM register"),
        }
    }

    /// GPR operand in a register, loading a spilled one into `scratch`.
    fn g_reg(&mut self, v: VReg, scratch: AsmRegister32) -> Result<AsmRegister32, BackendError> {
        match self.g(v) {
            G::R(r) => Ok(r),
            G::M(m) => {
                self.a.mov(scratch, m).map_err(enc)?;
                Ok(scratch)
            }
        }
    }

    /// Register a GPR result is computed into: its own, or eax if spilled.
    fn g_dst(&self, v: VReg) -> AsmRegister32 {
        match self.g(v) {
            G::R(r) => r,
            G::M(_) => eax,
        }
    }

    fn g_fin(&mut self, v: VReg, r: AsmRegister32) -> Result<(), BackendError> {
        match self.g(v) {
            G::R(d) if d == r => Ok(()),
            G::R(d) => self.a.mov(d, r).map_err(enc),
            G::M(m) => self.a.mov(m, r).map_err(enc),
        }
    }

    /// Pointer base (always an ABI argument register in practice).
    fn g64(&mut self, v: VReg) -> Result<AsmRegister64, BackendError> {
        match self.loc(v) {
            Loc::Reg(PReg::G(r)) => Ok(G64[r as usize]),
            Loc::Slot(s) => {
                self.a.mov(r11, qword_ptr(rsp + 16 * s as i32)).map_err(enc)?;
                Ok(r11)
            }
            Loc::Reg(PReg::X(_)) => unreachable!(),
        }
    }

    fn mem(&mut self, addr: Addr) -> Result<AsmMemoryOperand, BackendError> {
        let b = self.g64(addr.base)?;
        Ok(dword_ptr(b + addr.off))
    }

    fn x_preg(&self, v: VReg) -> Option<AsmRegisterXmm> {
        match self.loc(v) {
            Loc::Reg(PReg::X(r)) => Some(XMM[r as usize]),
            _ => None,
        }
    }

    /// XMM operand as r/m with a 32-bit memory size (scalar ops).
    fn x_ss(&self, v: VReg) -> X {
        match self.loc(v) {
            Loc::Reg(PReg::X(r)) => X::R(XMM[r as usize]),
            Loc::Slot(s) => X::M(self.slot_d(s)),
            Loc::Reg(PReg::G(_)) => unreachable!("XMM vreg in a GPR"),
        }
    }

    /// XMM operand as r/m with a 128-bit memory size (packed bit ops).
    fn x_ps(&self, v: VReg) -> X {
        match self.loc(v) {
            Loc::Reg(PReg::X(r)) => X::R(XMM[r as usize]),
            Loc::Slot(s) => X::M(self.slot_x(s)),
            Loc::Reg(PReg::G(_)) => unreachable!("XMM vreg in a GPR"),
        }
    }

    fn x_reg(&mut self, v: VReg, scratch: AsmRegisterXmm) -> Result<AsmRegisterXmm, BackendError> {
        match self.x_ss(v) {
            X::R(r) => Ok(r),
            X::M(m) => {
                self.a.vmovss(scratch, m).map_err(enc)?;
                Ok(scratch)
            }
        }
    }

    fn x_dst(&self, v: VReg) -> AsmRegisterXmm {
        self.x_preg(v).unwrap_or(XS1)
    }

    fn x_fin(&mut self, v: VReg, r: AsmRegisterXmm) -> Result<(), BackendError> {
        match self.x_ss(v) {
            X::R(d) if d == r => Ok(()),
            X::R(d) => self.a.vmovaps(d, r).map_err(enc),
            X::M(m) => self.a.vmovss(m, r).map_err(enc),
        }
    }

    /// Set flags for "is `c` non-zero".
    fn test_bool(&mut self, c: VReg) -> Result<(), BackendError> {
        match self.g(c) {
            G::R(r) => self.a.test(r, r).map_err(enc),
            G::M(m) => self.a.cmp(m, 0).map_err(enc),
        }
    }

    // ── instructions ──────────────────────────────────────────────────

    fn inst(&mut self, inst: &MInst, next: Option<Label>) -> Result<(), BackendError> {
        use MInst as M;
        match inst {
            M::Mark(_) => {}
            M::Label(l) => {
                let mut lab = self.label(*l);
                self.a.set_label(&mut lab).map_err(enc)?;
                self.labels.insert(*l, lab);
            }
            M::Entry { .. } => {
                for &r in &self.saved.clone() {
                    self.a.push(G64[r as usize]).map_err(enc)?;
                }
                if self.frame > 0 {
                    self.a.sub(rsp, self.frame).map_err(enc)?;
                }
            }
            M::Ret => {
                if self.frame > 0 {
                    self.a.add(rsp, self.frame).map_err(enc)?;
                }
                for &r in self.saved.clone().iter().rev() {
                    self.a.pop(G64[r as usize]).map_err(enc)?;
                }
                self.a.ret().map_err(enc)?;
            }

            // ── XMM ───────────────────────────────────────────────────
            M::XConst { d, bits } => match self.x_preg(*d) {
                Some(x) if *bits == 0 => self.a.vxorps(x, x, x).map_err(enc)?,
                Some(x) => {
                    self.a.mov(eax, *bits as i32).map_err(enc)?;
                    self.a.vmovd(x, eax).map_err(enc)?;
                }
                None => {
                    let Loc::Slot(s) = self.loc(*d) else { unreachable!() };
                    self.a.mov(self.slot_d(s), *bits as i32).map_err(enc)?;
                }
            },
            M::XMov { d, s } => self.xmov(*d, *s)?,
            M::XBin { op, d, a, b } => {
                let ra = self.x_reg(*a, XS0)?;
                let rd = self.x_dst(*d);
                let (bss, bps) = (self.x_ss(*b), self.x_ps(*b));
                let a = &mut self.a;
                match op {
                    XOp::Add => x3!(a, vaddss, rd, ra, bss)?,
                    XOp::Sub => x3!(a, vsubss, rd, ra, bss)?,
                    XOp::Mul => x3!(a, vmulss, rd, ra, bss)?,
                    XOp::Div => x3!(a, vdivss, rd, ra, bss)?,
                    XOp::Min => x3!(a, vminss, rd, ra, bss)?,
                    XOp::Max => x3!(a, vmaxss, rd, ra, bss)?,
                    XOp::And => x3!(a, vandps, rd, ra, bps)?,
                    XOp::Xor => x3!(a, vxorps, rd, ra, bps)?,
                }
                self.x_fin(*d, rd)?;
            }
            M::XFma { d, a, b, c } => self.fma(*d, *a, *b, *c)?,
            M::XUn { op, d, a } => {
                let ra = self.x_reg(*a, XS0)?;
                let rd = self.x_dst(*d);
                match op {
                    XUnOp::Sqrt => self.a.vsqrtss(rd, ra, ra).map_err(enc)?,
                    XUnOp::Round(imm) => self.a.vroundss(rd, ra, ra, *imm as i32).map_err(enc)?,
                }
                self.x_fin(*d, rd)?;
            }
            M::XLoad { d, addr } => {
                let m = self.mem(*addr)?;
                match self.loc(*d) {
                    Loc::Reg(PReg::X(r)) => self.a.vmovss(XMM[r as usize], m).map_err(enc)?,
                    Loc::Slot(s) => {
                        self.a.mov(eax, m).map_err(enc)?;
                        self.a.mov(self.slot_d(s), eax).map_err(enc)?;
                    }
                    _ => unreachable!(),
                }
            }
            M::XStore { addr, s } => {
                let m = self.mem(*addr)?;
                match self.loc(*s) {
                    Loc::Reg(PReg::X(r)) => self.a.vmovss(m, XMM[r as usize]).map_err(enc)?,
                    Loc::Slot(k) => {
                        self.a.mov(eax, self.slot_d(k)).map_err(enc)?;
                        self.a.mov(m, eax).map_err(enc)?;
                    }
                    _ => unreachable!(),
                }
            }
            M::XSelect { d, c, t, f } => {
                // Mask from the bool: 0 → 0, 1 → all ones; vblendvps picks
                // t where the mask's sign bit is set.
                g2!(self.a, mov, eax, self.g(*c))?;
                self.a.neg(eax).map_err(enc)?;
                self.a.vmovd(XS1, eax).map_err(enc)?;
                let rf = self.x_reg(*f, XS0)?;
                let rd = self.x_preg(*d).unwrap_or(XS0);
                match self.x_ps(*t) {
                    X::R(r) => self.a.vblendvps(rd, rf, r, XS1).map_err(enc)?,
                    X::M(m) => self.a.vblendvps(rd, rf, m, XS1).map_err(enc)?,
                }
                self.x_fin(*d, rd)?;
            }

            // ── GPR ───────────────────────────────────────────────────
            M::GConst { d, imm } => match self.g(*d) {
                G::R(r) if *imm == 0 => self.a.xor(r, r).map_err(enc)?,
                G::R(r) => self.a.mov(r, *imm).map_err(enc)?,
                G::M(m) => self.a.mov(m, *imm).map_err(enc)?,
            },
            M::GMov { d, s } => match (self.g(*d), self.g(*s)) {
                (G::R(rd), G::R(rs)) if rd == rs => {}
                (G::R(rd), src) => g2!(self.a, mov, rd, src)?,
                (G::M(md), G::R(rs)) => self.a.mov(md, rs).map_err(enc)?,
                (G::M(md), G::M(ms)) => {
                    if self.loc(*d) != self.loc(*s) {
                        self.a.mov(eax, ms).map_err(enc)?;
                        self.a.mov(md, eax).map_err(enc)?;
                    }
                }
            },
            M::GBin { op, d, a, b } => self.gbin(*op, *d, *a, *b)?,
            M::GShift { op, d, a, b } => self.shift(*op, *d, *a, *b)?,
            M::GUn { op, d, a } => {
                let rd = self.g_dst(*d);
                match op {
                    GUnOp::Lzcnt => g2!(self.a, lzcnt, rd, self.g(*a))?,
                    GUnOp::Neg | GUnOp::Not => {
                        if self.g(*a).reg() != Some(rd) {
                            g2!(self.a, mov, rd, self.g(*a))?;
                        }
                        if *op == GUnOp::Neg {
                            self.a.neg(rd).map_err(enc)?;
                        } else {
                            self.a.not(rd).map_err(enc)?;
                        }
                    }
                }
                self.g_fin(*d, rd)?;
            }
            M::GDiv { kind, d, a, b } => self.div(*kind, *d, *a, *b)?,
            M::GCmp { cc, d, a, b } => {
                let ra = self.g_reg(*a, eax)?;
                g2!(self.a, cmp, ra, self.g(*b))?;
                let a = &mut self.a;
                match cc {
                    ICond::Eq => a.sete(al),
                    ICond::Ne => a.setne(al),
                    ICond::Ult => a.setb(al),
                    ICond::Ule => a.setbe(al),
                    ICond::Ugt => a.seta(al),
                    ICond::Uge => a.setae(al),
                    ICond::Slt => a.setl(al),
                    ICond::Sle => a.setle(al),
                    ICond::Sgt => a.setg(al),
                    ICond::Sge => a.setge(al),
                }
                .map_err(enc)?;
                self.bool_from_al(*d)?;
            }
            M::FCmp { cc, d, a, b } => {
                use FCond::*;
                let (x, y) = match cc {
                    OrdLt | OrdLe | UnordGt | UnordGe => (*b, *a),
                    _ => (*a, *b),
                };
                let rx = self.x_reg(x, XS0)?;
                match self.x_ss(y) {
                    X::R(r) => self.a.vucomiss(rx, r).map_err(enc)?,
                    X::M(m) => self.a.vucomiss(rx, m).map_err(enc)?,
                }
                let a = &mut self.a;
                match cc {
                    OrdEq => {
                        a.sete(al).map_err(enc)?;
                        a.setnp(r11b).map_err(enc)?;
                        a.and(al, r11b).map_err(enc)?;
                    }
                    UnordNe => {
                        a.setne(al).map_err(enc)?;
                        a.setp(r11b).map_err(enc)?;
                        a.or(al, r11b).map_err(enc)?;
                    }
                    OrdNe => a.setne(al).map_err(enc)?,
                    OrdLt | OrdGt => a.seta(al).map_err(enc)?,
                    OrdLe | OrdGe => a.setae(al).map_err(enc)?,
                    UnordEq => a.sete(al).map_err(enc)?,
                    UnordLt | UnordGt => a.setb(al).map_err(enc)?,
                    UnordLe | UnordGe => a.setbe(al).map_err(enc)?,
                }
                self.bool_from_al(*d)?;
            }
            M::GSelect { d, c, t, f } => {
                self.test_bool(*c)?;
                let rd = self.g_dst(*d);
                if self.g(*t).reg() == Some(rd) {
                    g2!(self.a, cmove, rd, self.g(*f))?;
                } else {
                    if self.g(*f).reg() != Some(rd) {
                        g2!(self.a, mov, rd, self.g(*f))?; // mov keeps the flags
                    }
                    g2!(self.a, cmovne, rd, self.g(*t))?;
                }
                self.g_fin(*d, rd)?;
            }
            M::GLoad { d, addr } => {
                let m = self.mem(*addr)?;
                let rd = self.g_dst(*d);
                self.a.mov(rd, m).map_err(enc)?;
                self.g_fin(*d, rd)?;
            }
            M::GStore { addr, s } => {
                let rs = self.g_reg(*s, eax)?;
                let m = self.mem(*addr)?;
                self.a.mov(m, rs).map_err(enc)?;
            }

            // ── conversions ───────────────────────────────────────────
            M::CvtSToF { d, a } => {
                let rd = self.x_dst(*d);
                self.a.vxorps(rd, rd, rd).map_err(enc)?;
                match self.g(*a) {
                    G::R(r) => self.a.vcvtsi2ss(rd, rd, r).map_err(enc)?,
                    G::M(m) => self.a.vcvtsi2ss(rd, rd, m).map_err(enc)?,
                }
                self.x_fin(*d, rd)?;
            }
            M::CvtUToF { d, a } => {
                // Zero-extend to 64 bits (a 32-bit mov does), convert as i64.
                g2!(self.a, mov, eax, self.g(*a))?;
                let rd = self.x_dst(*d);
                self.a.vxorps(rd, rd, rd).map_err(enc)?;
                self.a.vcvtsi2ss(rd, rd, rax).map_err(enc)?;
                self.x_fin(*d, rd)?;
            }
            M::CvtFToS { d, a } => {
                let ra = self.x_reg(*a, XS0)?;
                let mut done = self.a.create_label();
                let mut nan = self.a.create_label();
                let a = &mut self.a;
                a.vcvttss2si(eax, ra).map_err(enc)?;
                a.cmp(eax, i32::MIN).map_err(enc)?;
                a.jne(done).map_err(enc)?;
                // Out of range or NaN: NaN → 0, positive → i32::MAX,
                // negative stays i32::MIN.
                a.vucomiss(ra, ra).map_err(enc)?;
                a.jp(nan).map_err(enc)?;
                a.vmovd(r11d, ra).map_err(enc)?;
                a.test(r11d, r11d).map_err(enc)?;
                a.js(done).map_err(enc)?;
                a.mov(eax, i32::MAX).map_err(enc)?;
                a.jmp(done).map_err(enc)?;
                a.set_label(&mut nan).map_err(enc)?;
                a.xor(eax, eax).map_err(enc)?;
                a.set_label(&mut done).map_err(enc)?;
                a.nop().map_err(enc)?;
                self.g_fin(*d, eax)?;
            }
            M::CvtFToU { d, a } => {
                let ra = self.x_reg(*a, XS0)?;
                let mut done = self.a.create_label();
                let mut special = self.a.create_label();
                let mut zero = self.a.create_label();
                let a = &mut self.a;
                a.vcvttss2si(rax, ra).map_err(enc)?;
                a.test(rax, rax).map_err(enc)?;
                a.js(special).map_err(enc)?;
                a.mov(r11d, -1).map_err(enc)?; // r11 = 0xffff_ffff
                a.cmp(rax, r11).map_err(enc)?;
                a.jbe(done).map_err(enc)?;
                a.mov(eax, -1).map_err(enc)?;
                a.jmp(done).map_err(enc)?;
                // rax < 0: x ≤ -1, NaN, or x ≥ 2^63.
                a.set_label(&mut special).map_err(enc)?;
                a.vucomiss(ra, ra).map_err(enc)?;
                a.jp(zero).map_err(enc)?;
                a.vmovd(r11d, ra).map_err(enc)?;
                a.test(r11d, r11d).map_err(enc)?;
                a.js(zero).map_err(enc)?;
                a.mov(eax, -1).map_err(enc)?;
                a.jmp(done).map_err(enc)?;
                a.set_label(&mut zero).map_err(enc)?;
                a.xor(eax, eax).map_err(enc)?;
                a.set_label(&mut done).map_err(enc)?;
                a.nop().map_err(enc)?;
                self.g_fin(*d, eax)?;
            }
            M::MovXToG { d, s } => match (self.g(*d), self.x_ss(*s)) {
                (G::R(rd), X::R(rs)) => self.a.vmovd(rd, rs).map_err(enc)?,
                (G::R(rd), X::M(ms)) => self.a.mov(rd, ms).map_err(enc)?,
                (G::M(md), X::R(rs)) => self.a.vmovd(md, rs).map_err(enc)?,
                (G::M(md), X::M(ms)) => {
                    self.a.mov(eax, ms).map_err(enc)?;
                    self.a.mov(md, eax).map_err(enc)?;
                }
            },
            M::MovGToX { d, s } => match (self.x_ss(*d), self.g(*s)) {
                (X::R(rd), G::R(rs)) => self.a.vmovd(rd, rs).map_err(enc)?,
                (X::R(rd), G::M(ms)) => self.a.vmovd(rd, ms).map_err(enc)?,
                (X::M(md), G::R(rs)) => self.a.mov(md, rs).map_err(enc)?,
                (X::M(md), G::M(ms)) => {
                    self.a.mov(eax, ms).map_err(enc)?;
                    self.a.mov(md, eax).map_err(enc)?;
                }
            },

            // ── control ───────────────────────────────────────────────
            M::Jmp(l) => {
                if next != Some(*l) {
                    self.a.jmp(self.label(*l)).map_err(enc)?;
                }
            }
            M::Br { c, t, f } => {
                self.test_bool(*c)?;
                if t == f {
                    if next != Some(*t) {
                        self.a.jmp(self.label(*t)).map_err(enc)?;
                    }
                } else if next == Some(*f) {
                    self.a.jne(self.label(*t)).map_err(enc)?;
                } else if next == Some(*t) {
                    self.a.je(self.label(*f)).map_err(enc)?;
                } else {
                    self.a.jne(self.label(*t)).map_err(enc)?;
                    self.a.jmp(self.label(*f)).map_err(enc)?;
                }
            }
            M::CaseEq { s, imm, t } => {
                match self.g(*s) {
                    G::R(r) => self.a.cmp(r, *imm).map_err(enc)?,
                    G::M(m) => self.a.cmp(m, *imm).map_err(enc)?,
                }
                self.a.je(self.label(*t)).map_err(enc)?;
            }
        }
        Ok(())
    }

    fn bool_from_al(&mut self, d: VReg) -> Result<(), BackendError> {
        let rd = self.g_dst(d);
        self.a.movzx(rd, al).map_err(enc)?;
        self.g_fin(d, rd)
    }

    fn xmov(&mut self, d: VReg, s: VReg) -> Result<(), BackendError> {
        match (self.loc(d), self.loc(s)) {
            (a, b) if a == b => Ok(()),
            (Loc::Reg(PReg::X(rd)), Loc::Reg(PReg::X(rs))) => {
                self.a.vmovaps(XMM[rd as usize], XMM[rs as usize]).map_err(enc)
            }
            (Loc::Reg(PReg::X(rd)), Loc::Slot(k)) => {
                self.a.vmovss(XMM[rd as usize], self.slot_d(k)).map_err(enc)
            }
            (Loc::Slot(k), Loc::Reg(PReg::X(rs))) => {
                self.a.vmovss(self.slot_d(k), XMM[rs as usize]).map_err(enc)
            }
            (Loc::Slot(kd), Loc::Slot(ks)) => {
                self.a.mov(eax, self.slot_d(ks)).map_err(enc)?;
                self.a.mov(self.slot_d(kd), eax).map_err(enc)
            }
            _ => unreachable!("XMov across classes"),
        }
    }

    /// d = a * b + c, picking the FMA form that needs no copy when d
    /// shares a register with an operand.
    fn fma(&mut self, d: VReg, a: VReg, b: VReg, c: VReg) -> Result<(), BackendError> {
        let rd = self.x_dst(d);
        let reg_is = |e: &Self, v: VReg| e.x_preg(v) == Some(rd) && e.x_preg(d).is_some();
        if reg_is(self, a) {
            let rb = self.x_reg(b, XS0)?;
            x3!(self.a, vfmadd213ss, rd, rb, self.x_ss(c))?;
        } else if reg_is(self, b) {
            let ra = self.x_reg(a, XS0)?;
            x3!(self.a, vfmadd213ss, rd, ra, self.x_ss(c))?;
        } else if reg_is(self, c) {
            let ra = self.x_reg(a, XS0)?;
            x3!(self.a, vfmadd231ss, rd, ra, self.x_ss(b))?;
        } else {
            match self.x_ss(a) {
                X::R(r) => self.a.vmovaps(rd, r).map_err(enc)?,
                X::M(m) => self.a.vmovss(rd, m).map_err(enc)?,
            }
            let rb = self.x_reg(b, XS0)?;
            x3!(self.a, vfmadd213ss, rd, rb, self.x_ss(c))?;
        }
        self.x_fin(d, rd)
    }

    fn gbin(&mut self, op: GOp, d: VReg, a: VReg, b: VReg) -> Result<(), BackendError> {
        fn apply(asm: &mut CodeAssembler, op: GOp, d: AsmRegister32, s: G) -> Result<(), BackendError> {
            match op {
                GOp::Add => g2!(asm, add, d, s),
                GOp::Sub => g2!(asm, sub, d, s),
                GOp::Mul => g2!(asm, imul_2, d, s),
                GOp::And => g2!(asm, and, d, s),
                GOp::Or => g2!(asm, or, d, s),
                GOp::Xor => g2!(asm, xor, d, s),
            }
        }
        let (ga, gb) = (self.g(a), self.g(b));
        let G::R(rd) = self.g(d) else {
            g2!(self.a, mov, eax, ga)?;
            apply(&mut self.a, op, eax, gb)?;
            return self.g_fin(d, eax);
        };
        if ga.reg() == Some(rd) {
            apply(&mut self.a, op, rd, gb)
        } else if gb.reg() == Some(rd) {
            if op.commutative() {
                apply(&mut self.a, op, rd, ga)
            } else {
                g2!(self.a, mov, eax, ga)?;
                apply(&mut self.a, op, eax, gb)?;
                self.a.mov(rd, eax).map_err(enc)
            }
        } else if let (GOp::Add, G::R(x), G::R(y)) = (op, ga, gb) {
            let (x, y) = (to64(x), to64(y));
            self.a.lea(rd, ptr(x + y)).map_err(enc)
        } else {
            g2!(self.a, mov, rd, ga)?;
            apply(&mut self.a, op, rd, gb)
        }
    }

    fn shift(&mut self, op: ShiftOp, d: VReg, a: VReg, b: VReg) -> Result<(), BackendError> {
        let rb = self.g_reg(b, r11d)?;
        let rd = self.g_dst(d);
        let ga = self.g(a);
        macro_rules! sh {
            ($m:ident) => {
                match ga {
                    G::R(r) => self.a.$m(rd, r, rb),
                    G::M(m) => self.a.$m(rd, m, rb),
                }
                .map_err(enc)
            };
        }
        match op {
            ShiftOp::Shl => sh!(shlx),
            ShiftOp::LShr => sh!(shrx),
            ShiftOp::AShr => sh!(sarx),
        }?;
        self.g_fin(d, rd)
    }

    /// Integer divide/remainder with SPIR-V-undefined cases made safe, as
    /// the arm64 backend does: x/0 = 0, x%0 = x, INT_MIN/-1 = INT_MIN,
    /// INT_MIN%-1 = 0.
    fn div(&mut self, kind: DivKind, d: VReg, a: VReg, b: VReg) -> Result<(), BackendError> {
        let signed = matches!(kind, DivKind::SDiv | DivKind::SMod);
        let rem = matches!(kind, DivKind::UMod | DivKind::SMod);
        // The divisor, as it will be after rdx is clobbered.
        let gb = match self.loc(b) {
            Loc::Reg(PReg::G(r)) if r == gpr::RDX => G::R(r11d),
            _ => self.g(b),
        };
        let mut zero = self.a.create_label();
        let mut done = self.a.create_label();
        let mut go = self.a.create_label();
        self.a.mov(r11, rdx).map_err(enc)?;
        g2!(self.a, mov, eax, self.g(a))?;
        match gb {
            G::R(r) => self.a.cmp(r, 0).map_err(enc)?,
            G::M(m) => self.a.cmp(m, 0).map_err(enc)?,
        }
        self.a.je(zero).map_err(enc)?;
        if signed {
            match gb {
                G::R(r) => self.a.cmp(r, -1).map_err(enc)?,
                G::M(m) => self.a.cmp(m, -1).map_err(enc)?,
            }
            self.a.jne(go).map_err(enc)?;
            if rem {
                self.a.xor(eax, eax).map_err(enc)?;
            } else {
                self.a.neg(eax).map_err(enc)?;
            }
            self.a.jmp(done).map_err(enc)?;
        }
        self.a.set_label(&mut go).map_err(enc)?;
        if signed {
            self.a.cdq().map_err(enc)?;
            match gb {
                G::R(r) => self.a.idiv(r).map_err(enc)?,
                G::M(m) => self.a.idiv(m).map_err(enc)?,
            }
        } else {
            self.a.xor(edx, edx).map_err(enc)?;
            match gb {
                G::R(r) => self.a.div(r).map_err(enc)?,
                G::M(m) => self.a.div(m).map_err(enc)?,
            }
        }
        if rem {
            self.a.mov(eax, edx).map_err(enc)?;
        }
        if kind == DivKind::SMod {
            // Floored: add the divisor when the remainder is non-zero and
            // its sign differs from the divisor's.
            self.a.test(eax, eax).map_err(enc)?;
            self.a.je(done).map_err(enc)?;
            self.a.mov(edx, eax).map_err(enc)?;
            g2!(self.a, xor, edx, gb)?;
            self.a.jns(done).map_err(enc)?;
            g2!(self.a, add, eax, gb)?;
        }
        self.a.jmp(done).map_err(enc)?;
        self.a.set_label(&mut zero).map_err(enc)?;
        if !rem {
            self.a.xor(eax, eax).map_err(enc)?;
        } else {
            self.a.nop().map_err(enc)?; // remainder by zero: eax already = a
        }
        self.a.set_label(&mut done).map_err(enc)?;
        self.a.mov(rdx, r11).map_err(enc)?;
        self.g_fin(d, eax)
    }
}

impl G {
    fn reg(self) -> Option<AsmRegister32> {
        match self {
            G::R(r) => Some(r),
            G::M(_) => None,
        }
    }
}

fn to64(r: AsmRegister32) -> AsmRegister64 {
    let i = G32.iter().position(|&x| x == r).expect("GPR");
    G64[i]
}
