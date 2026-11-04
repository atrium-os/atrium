//! Instruction selection: GIR → machine IR.
//!
//! G3 scope: every data value lives in VGPRs; lane masks (`bool`) and buffer
//! addresses live in SGPRs. Control flow follows the region tree with EXEC
//! masking:
//!
//! * `if`: `s_and_saveexec` narrows to the then-lanes, the else arm runs on
//!   `save & !cond`, and the merge restores `save` minus lanes that left.
//! * loops: a break mask and a continue mask per loop; lanes that continue
//!   (or fall off the end) rejoin at the bottom and the loop repeats while
//!   any lane is active; after the loop EXEC is the break mask.
//! * `return`: lanes move to a function-wide return mask and EXEC drops them.
//!
//! Regions whose mask is empty are skipped with `s_cbranch_execz`.

use crate::mir::*;
use crate::{Abi, Error};
use atrium_gir::passes::uniformity::Uniformity;
use atrium_gir::*;
use std::collections::HashMap;

struct LoopRegs {
    brk: VR,
    cont: VR,
    params: Vec<ValueId>,
    results: Vec<ValueId>,
}

pub(crate) struct Isel<'a> {
    f: &'a Function,
    #[allow(dead_code)]
    u: &'a Uniformity,
    abi: &'a Abi,
    pub mi: Vec<MI>,
    pub vregs: Vec<VRegInfo>,
    map: HashMap<ValueId, (VR, u8)>,
    consts: HashMap<ValueId, u32>,
    uses: HashMap<ValueId, usize>,
    labels: u32,
    ret: VR,
    loops: Vec<LoopRegs>,
    yields: Vec<Vec<ValueId>>,
}

fn unsup<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error::Unsupported(s.into()))
}

fn count_uses(r: &Region, uses: &mut HashMap<ValueId, usize>) {
    for n in r {
        match n {
            Node::Inst(i) => {
                for o in i.op.operands() {
                    *uses.entry(o).or_default() += 1;
                }
            }
            Node::If { cond, then, els, .. } => {
                *uses.entry(*cond).or_default() += 1;
                count_uses(then, uses);
                count_uses(els, uses);
            }
            Node::Loop { inits, body, .. } => {
                for v in inits {
                    *uses.entry(*v).or_default() += 1;
                }
                count_uses(body, uses);
            }
        }
    }
}

/// True if `r` contains a `return`, or a `break`/`continue` of the loop it
/// sits in (not of a nested loop).
fn leaves(r: &Region) -> bool {
    r.iter().any(|n| match n {
        Node::Inst(i) => matches!(i.op, Op::Return | Op::Break(_) | Op::Continue(_)),
        Node::If { then, els, .. } => leaves(then) || leaves(els),
        Node::Loop { body, .. } => contains_return(body),
    })
}

fn contains_return(r: &Region) -> bool {
    r.iter().any(|n| match n {
        Node::Inst(i) => matches!(i.op, Op::Return),
        Node::If { then, els, .. } => contains_return(then) || contains_return(els),
        Node::Loop { body, .. } => contains_return(body),
    })
}

/// Inline constant or not.
fn inline(bits: u32) -> bool {
    crate::asm::inline_const(bits).is_some()
}

impl<'a> Isel<'a> {
    pub fn new(f: &'a Function, u: &'a Uniformity, abi: &'a Abi) -> Self {
        let mut uses = HashMap::new();
        count_uses(&f.body, &mut uses);
        Isel {
            f,
            u,
            abi,
            mi: vec![],
            vregs: vec![],
            map: HashMap::new(),
            consts: HashMap::new(),
            uses,
            labels: 0,
            ret: 0,
            loops: vec![],
            yields: vec![],
        }
    }

    fn vreg(&mut self, class: Class, size: u8) -> VR {
        self.vregs.push(VRegInfo { class, size });
        self.vregs.len() as VR - 1
    }

    fn label(&mut self) -> u32 {
        self.labels += 1;
        self.labels
    }

    fn salu(&mut self, name: &'static str, dst: O, src: Vec<O>) {
        self.mi.push(MI::Salu { name, dst, src });
    }

    /// VALU op; materialises extra distinct literals (VOP3 takes one).
    fn valu(&mut self, name: &'static str, dst: O, src: Vec<O>, neg: u8) {
        let mut lit: Option<u32> = None;
        let mut src2 = vec![];
        for s in src {
            match s {
                O::Imm(b) if !inline(b) => match lit {
                    None => {
                        lit = Some(b);
                        src2.push(s);
                    }
                    Some(l) if l == b => src2.push(s),
                    Some(_) => {
                        let t = self.vreg(Class::V, 1);
                        self.mi.push(MI::Valu { name: "V_MOV_B32", dst: O::R(t, 0), sdst: None, src: vec![s], neg: 0 });
                        src2.push(O::R(t, 0));
                    }
                },
                _ => src2.push(s),
            }
        }
        self.mi.push(MI::Valu { name, dst, sdst: None, src: src2, neg });
    }

    fn ty(&self, v: ValueId) -> Ty {
        self.f.ty(v)
    }

    /// Operand for component `c` of value `v`.
    fn src(&self, v: ValueId, c: u8) -> Result<O, Error> {
        if let Some(b) = self.consts.get(&v) {
            return Ok(O::Imm(if self.ty(v) == Ty::BOOL { if *b != 0 { u32::MAX } else { 0 } } else { *b }));
        }
        match self.map.get(&v) {
            Some((r, o)) => Ok(O::R(*r, o + c)),
            None => Err(Error::Internal(format!("{v} has no register"))),
        }
    }

    /// A VGPR group holding all of `v` contiguously (copying if needed).
    fn vgroup(&mut self, v: ValueId) -> Result<O, Error> {
        if self.ty(v).scalar() == Scalar::Bool {
            return unsup("bool vector data");
        }
        if let (Some((r, o)), None) = (self.map.get(&v).copied(), self.consts.get(&v)) {
            return Ok(O::R(r, o));
        }
        let n = self.ty(v).comps() as u8;
        let g = self.vreg(Class::V, n);
        for c in 0..n {
            let s = self.src(v, c)?;
            self.valu("V_MOV_B32", O::R(g, c), vec![s], 0);
        }
        Ok(O::R(g, 0))
    }

    fn def_v(&mut self, v: ValueId) -> VR {
        let n = self.ty(v).comps() as u8;
        let r = self.vreg(Class::V, n);
        self.map.insert(v, (r, 0));
        r
    }

    fn def_s(&mut self, v: ValueId) -> VR {
        let r = self.vreg(Class::S, 1);
        self.map.insert(v, (r, 0));
        r
    }

    /// Allocate the register for a value written in several places (if
    /// results, loop parameters and results).
    fn def_slot(&mut self, v: ValueId) {
        if self.ty(v) == Ty::BOOL {
            self.def_s(v);
        } else {
            self.def_v(v);
        }
    }

    /// Write `src` into `dst`'s register(s) for the active lanes only.
    fn copy_into(&mut self, dst: ValueId, src: ValueId) -> Result<(), Error> {
        let (d, o) = self.map[&dst];
        if self.ty(dst) == Ty::BOOL {
            // SALU writes ignore EXEC: d = (d & !exec) | (s & exec).
            let s = self.src(src, 0)?;
            self.salu("S_AND_NOT1_B32", O::R(d, 0), vec![O::R(d, 0), O::Exec]);
            match s {
                O::Imm(0) => {}
                O::Imm(_) => self.salu("S_OR_B32", O::R(d, 0), vec![O::R(d, 0), O::Exec]),
                _ => {
                    let t = self.vreg(Class::S, 1);
                    self.salu("S_AND_B32", O::R(t, 0), vec![s, O::Exec]);
                    self.salu("S_OR_B32", O::R(d, 0), vec![O::R(d, 0), O::R(t, 0)]);
                }
            }
            return Ok(());
        }
        for c in 0..self.ty(dst).comps() as u8 {
            let s = self.src(src, c)?;
            self.valu("V_MOV_B32", O::R(d, o + c), vec![s], 0);
        }
        Ok(())
    }

    /// Buffer address: (vaddr, immediate offset).
    fn address(&mut self, off: ValueId) -> Result<(O, i32), Error> {
        if let Some(k) = self.consts.get(&off).copied() {
            if (k as i32) < (1 << 23) && (k as i32) >= -(1 << 23) {
                let z = self.vreg(Class::V, 1);
                self.valu("V_MOV_B32", O::R(z, 0), vec![O::Imm(0)], 0);
                return Ok((O::R(z, 0), k as i32));
            }
        }
        if let Some(Op::Bin(BinOp::IAdd, a, b)) = self.def_of(off) {
            for (x, y) in [(a, b), (b, a)] {
                if let (Some(k), false) = (self.consts.get(&y).copied(), self.consts.contains_key(&x)) {
                    if (k as i32).abs() < (1 << 23) {
                        return Ok((self.src(x, 0)?, k as i32));
                    }
                }
            }
        }
        Ok((self.src(off, 0)?, 0))
    }

    fn def_of(&self, v: ValueId) -> Option<Op> {
        fn find(r: &Region, v: ValueId) -> Option<Op> {
            for n in r {
                match n {
                    Node::Inst(i) if i.result == Some(v) => return Some(i.op.clone()),
                    Node::Inst(_) => {}
                    Node::If { then, els, .. } => {
                        if let Some(o) = find(then, v).or_else(|| find(els, v)) {
                            return Some(o);
                        }
                    }
                    Node::Loop { body, .. } => {
                        if let Some(o) = find(body, v) {
                            return Some(o);
                        }
                    }
                }
            }
            None
        }
        find(&self.f.body, v)
    }

    fn saddr(&self, s: Space) -> Result<u8, Error> {
        match s {
            Space::Buffer { set, binding } => self
                .abi
                .buffers
                .iter()
                .position(|b| *b == (set, binding))
                .map(|i| 2 * i as u8)
                .ok_or_else(|| Error::Internal(format!("buffer ({set}, {binding}) not in the ABI"))),
            other => unsup(format!("{other:?} memory in G3")),
        }
    }

    fn inst(&mut self, i: &Inst) -> Result<(), Error> {
        let res = i.result;
        let rty = res.map(|r| self.ty(r));
        match &i.op {
            Op::Const(b) => {
                self.consts.insert(res.unwrap(), *b);
            }
            Op::Un(o, a) if rty == Some(Ty::BOOL) => {
                debug_assert_eq!(*o, UnOp::Not);
                let s = self.src(*a, 0)?;
                let d = self.def_s(res.unwrap());
                self.salu("S_NOT_B32", O::R(d, 0), vec![s]);
            }
            Op::Un(o, a) => {
                let s = self.src(*a, 0)?;
                let d = O::R(self.def_v(res.unwrap()), 0);
                match o {
                    UnOp::INeg => self.valu("V_SUB_NC_U32", d, vec![O::Imm(0), s], 0),
                    UnOp::Not => self.valu("V_NOT_B32", d, vec![s], 0),
                    UnOp::FNeg => self.valu("V_XOR_B32", d, vec![s, O::Imm(0x8000_0000)], 0),
                    UnOp::FAbs => self.valu("V_AND_B32", d, vec![s, O::Imm(0x7fff_ffff)], 0),
                    UnOp::FSqrt => return unsup("fsqrt (needs a correctly rounded sequence)"),
                    UnOp::FFloor => self.valu("V_FLOOR_F32", d, vec![s], 0),
                    UnOp::FCeil => self.valu("V_CEIL_F32", d, vec![s], 0),
                    UnOp::FTrunc => self.valu("V_TRUNC_F32", d, vec![s], 0),
                    UnOp::BitReverse => self.valu("V_BFREV_B32", d, vec![s], 0),
                    UnOp::Clz => return unsup("clz"),
                }
            }
            Op::Bin(o, a, b) if rty == Some(Ty::BOOL) => {
                let (x, y) = (self.src(*a, 0)?, self.src(*b, 0)?);
                let d = self.def_s(res.unwrap());
                let name = match o {
                    BinOp::And => "S_AND_B32",
                    BinOp::Or => "S_OR_B32",
                    BinOp::Xor => "S_XOR_B32",
                    _ => return Err(Error::Internal(format!("{o:?} on bool"))),
                };
                self.salu(name, O::R(d, 0), vec![x, y]);
            }
            Op::Bin(o, a, b) => {
                let (x, y) = (self.src(*a, 0)?, self.src(*b, 0)?);
                let d = O::R(self.def_v(res.unwrap()), 0);
                let (name, swap) = match o {
                    BinOp::IAdd => ("V_ADD_NC_U32", false),
                    BinOp::ISub => ("V_SUB_NC_U32", false),
                    BinOp::IMul => ("V_MUL_LO_U32", false),
                    BinOp::And => ("V_AND_B32", false),
                    BinOp::Or => ("V_OR_B32", false),
                    BinOp::Xor => ("V_XOR_B32", false),
                    BinOp::Shl => ("V_LSHLREV_B32", true),
                    BinOp::LShr => ("V_LSHRREV_B32", true),
                    BinOp::AShr => ("V_ASHRREV_I32", true),
                    BinOp::FAdd => ("V_ADD_F32", false),
                    BinOp::FSub => ("V_SUB_F32", false),
                    BinOp::FMul => ("V_MUL_F32", false),
                    BinOp::FMin => ("V_MIN_NUM_F32", false),
                    BinOp::FMax => ("V_MAX_NUM_F32", false),
                    BinOp::FDiv => {
                        self.fdiv(d, x, y);
                        return Ok(());
                    }
                    other => return unsup(format!("{other:?} with a variable divisor")),
                };
                self.valu(name, d, if swap { vec![y, x] } else { vec![x, y] }, 0);
            }
            Op::Fma(a, b, c) => {
                let s = vec![self.src(*a, 0)?, self.src(*b, 0)?, self.src(*c, 0)?];
                let d = O::R(self.def_v(res.unwrap()), 0);
                self.valu("V_FMA_F32", d, s, 0);
            }
            Op::Cmp(o, a, b) => {
                let (x, y) = (self.src(*a, 0)?, self.src(*b, 0)?);
                let d = O::R(self.def_s(res.unwrap()), 0);
                if self.ty(*a) == Ty::BOOL {
                    let name = if *o == CmpOp::IEq { "S_XNOR_B32" } else { "S_XOR_B32" };
                    self.salu(name, d, vec![x, y]);
                    return Ok(());
                }
                let name = match o {
                    CmpOp::IEq => "V_CMP_EQ_U32",
                    CmpOp::INe => "V_CMP_NE_U32",
                    CmpOp::ULt => "V_CMP_LT_U32",
                    CmpOp::ULe => "V_CMP_LE_U32",
                    CmpOp::SLt => "V_CMP_LT_I32",
                    CmpOp::SLe => "V_CMP_LE_I32",
                    CmpOp::FOEq => "V_CMP_EQ_F32",
                    CmpOp::FONe => "V_CMP_LG_F32",
                    CmpOp::FOLt => "V_CMP_LT_F32",
                    CmpOp::FOLe => "V_CMP_LE_F32",
                    CmpOp::FUEq => "V_CMP_NLG_F32",
                    CmpOp::FUNe => "V_CMP_NEQ_F32",
                    CmpOp::FULt => "V_CMP_NGE_F32",
                    CmpOp::FULe => "V_CMP_NGT_F32",
                };
                self.valu(name, d, vec![x, y], 0);
            }
            Op::Select(c, t, e) => {
                let m = self.src(*c, 0)?;
                let r = res.unwrap();
                if let O::Imm(b) = m {
                    // Constant condition (folding normally removes these).
                    let pick = if b != 0 { *t } else { *e };
                    self.def_slot(r);
                    return self.copy_into(r, pick);
                }
                if self.ty(r) == Ty::BOOL {
                    let (x, y) = (self.src(*t, 0)?, self.src(*e, 0)?);
                    let d = self.def_s(r);
                    let tmp = self.vreg(Class::S, 1);
                    self.salu("S_AND_B32", O::R(d, 0), vec![m, x]);
                    self.salu("S_AND_NOT1_B32", O::R(tmp, 0), vec![y, m]);
                    self.salu("S_OR_B32", O::R(d, 0), vec![O::R(d, 0), O::R(tmp, 0)]);
                    return Ok(());
                }
                let n = self.ty(r).comps() as u8;
                let d = self.def_v(r);
                for k in 0..n {
                    let (x, y) = (self.src(*t, k)?, self.src(*e, k)?);
                    self.valu("V_CNDMASK_B32", O::R(d, k), vec![y, x, m], 0);
                }
            }
            Op::Conv(o, a) => {
                let s = self.src(*a, 0)?;
                let d = O::R(self.def_v(res.unwrap()), 0);
                match o {
                    ConvOp::SToF => self.valu("V_CVT_F32_I32", d, vec![s], 0),
                    ConvOp::UToF => self.valu("V_CVT_F32_U32", d, vec![s], 0),
                    ConvOp::FToS => self.valu("V_CVT_I32_F32", d, vec![s], 0),
                    ConvOp::FToU => self.valu("V_CVT_U32_F32", d, vec![s], 0),
                    ConvOp::BoolToI => self.valu("V_CNDMASK_B32", d, vec![O::Imm(0), O::Imm(1), s], 0),
                }
            }
            Op::Bitcast(a) | Op::Extract(a, _) if self.consts.contains_key(a) => {
                let k = match &i.op {
                    Op::Extract(_, _) => return unsup("extract from a constant vector"),
                    _ => self.consts[a],
                };
                self.consts.insert(res.unwrap(), k);
            }
            Op::Bitcast(a) => {
                let m = self.map[a];
                self.map.insert(res.unwrap(), m);
            }
            Op::Extract(a, k) => {
                let (r, o) = self.map[a];
                self.map.insert(res.unwrap(), (r, o + k));
            }
            Op::Construct(parts) => {
                let d = self.def_v(res.unwrap());
                for (k, p) in parts.iter().enumerate() {
                    let s = self.src(*p, 0)?;
                    self.valu("V_MOV_B32", O::R(d, k as u8), vec![s], 0);
                }
            }
            Op::Builtin(b) => self.builtin(*b, res.unwrap())?,
            Op::Load(space, off) => {
                let saddr = self.saddr(*space)?;
                let (vaddr, ioff) = self.address(*off)?;
                let r = res.unwrap();
                let name = match self.ty(r).comps() {
                    1 => "GLOBAL_LOAD_B32",
                    2 => "GLOBAL_LOAD_B64",
                    3 => "GLOBAL_LOAD_B96",
                    _ => "GLOBAL_LOAD_B128",
                };
                if self.ty(r) == Ty::BOOL {
                    return unsup("bool load");
                }
                let d = self.def_v(r);
                self.mi.push(MI::Global { name, vdst: Some(O::R(d, 0)), vaddr, saddr, data: None, ioff, th: 0, scope: 0 });
            }
            Op::Store(space, off, v) => {
                let saddr = self.saddr(*space)?;
                let (vaddr, ioff) = self.address(*off)?;
                let name = match self.ty(*v).comps() {
                    1 => "GLOBAL_STORE_B32",
                    2 => "GLOBAL_STORE_B64",
                    3 => "GLOBAL_STORE_B96",
                    _ => "GLOBAL_STORE_B128",
                };
                let data = self.vgroup(*v)?;
                self.mi.push(MI::Global { name, vdst: None, vaddr, saddr, data: Some(data), ioff, th: 0, scope: 0 });
            }
            Op::Atomic(o, space, off, v) => {
                let saddr = self.saddr(*space)?;
                let (vaddr, ioff) = self.address(*off)?;
                let name = match o {
                    AtomicOp::Add => "GLOBAL_ATOMIC_ADD_U32",
                    AtomicOp::And => "GLOBAL_ATOMIC_AND_B32",
                    AtomicOp::Or => "GLOBAL_ATOMIC_OR_B32",
                    AtomicOp::Xor => "GLOBAL_ATOMIC_XOR_B32",
                    AtomicOp::SMin => "GLOBAL_ATOMIC_MIN_I32",
                    AtomicOp::SMax => "GLOBAL_ATOMIC_MAX_I32",
                    AtomicOp::UMin => "GLOBAL_ATOMIC_MIN_U32",
                    AtomicOp::UMax => "GLOBAL_ATOMIC_MAX_U32",
                    AtomicOp::Exchange => "GLOBAL_ATOMIC_SWAP_B32",
                };
                let r = res.unwrap();
                let used = self.uses.get(&r).copied().unwrap_or(0) > 0;
                if *o == AtomicOp::Add && self.u.is_uniform(*off) && self.u.is_uniform(*v) {
                    return self.wave_atomic_add(name, saddr, vaddr, ioff, *v, r, used);
                }
                let data = self.vgroup(*v)?;
                let vdst = if used { Some(O::R(self.def_v(r), 0)) } else { None };
                // Device scope: other workgroups see the result.
                self.mi.push(MI::Global { name, vdst, vaddr, saddr, data: Some(data), ioff, th: used as u8, scope: 2 });
            }
            Op::Barrier => return unsup("barrier (needs workgroup memory, G4)"),
            Op::Yield(vals) => {
                let targets = self.yields.last().cloned().unwrap_or_default();
                for (t, v) in targets.iter().zip(vals) {
                    self.copy_into(*t, *v)?;
                }
            }
            Op::Continue(vals) | Op::Break(vals) => {
                let is_break = matches!(i.op, Op::Break(_));
                let l = self.loops.last().ok_or_else(|| Error::Internal("break outside loop".into()))?;
                let (slots, mask) = if is_break { (l.results.clone(), l.brk) } else { (l.params.clone(), l.cont) };
                for (t, v) in slots.iter().zip(vals) {
                    self.copy_into(*t, *v)?;
                }
                self.salu("S_OR_B32", O::R(mask, 0), vec![O::R(mask, 0), O::Exec]);
                self.salu("S_MOV_B32", O::Exec, vec![O::Imm(0)]);
            }
            Op::Return => {
                let r = self.ret;
                self.salu("S_OR_B32", O::R(r, 0), vec![O::R(r, 0), O::Exec]);
                self.salu("S_MOV_B32", O::Exec, vec![O::Imm(0)]);
            }
        }
        Ok(())
    }

    /// Correctly rounded f32 division (the standard RDNA sequence; exact
    /// with f32 denormals enabled).
    fn fdiv(&mut self, d: O, a: O, b: O) {
        let t = |s: &mut Self| O::R(s.vreg(Class::V, 1), 0);
        let (den, num, r, e, q, e2, q2, e3, q3) = (t(self), t(self), t(self), t(self), t(self), t(self), t(self), t(self), t(self));
        let push = |s: &mut Self, name: &'static str, dst: O, sdst: Option<O>, src: Vec<O>, neg: u8| {
            s.mi.push(MI::Valu { name, dst, sdst, src, neg });
        };
        // Literal operands go to registers first: the sequence reuses them.
        let a = if let O::Imm(x) = a { if inline(x) { a } else { let r = t(self); self.valu("V_MOV_B32", r, vec![a], 0); r } } else { a };
        let b = if let O::Imm(x) = b { if inline(x) { b } else { let r = t(self); self.valu("V_MOV_B32", r, vec![b], 0); r } } else { b };
        push(self, "V_DIV_SCALE_F32", den, Some(O::Vcc), vec![b, b, a], 0);
        push(self, "V_DIV_SCALE_F32", num, Some(O::Vcc), vec![a, b, a], 0);
        push(self, "V_RCP_F32", r, None, vec![den], 0);
        push(self, "V_FMA_F32", e, None, vec![den, r, O::Imm(1.0f32.to_bits())], 1);
        push(self, "V_FMA_F32", r, None, vec![e, r, r], 0);
        push(self, "V_MUL_F32", q, None, vec![num, r], 0);
        push(self, "V_FMA_F32", e2, None, vec![den, q, num], 1);
        push(self, "V_FMA_F32", q2, None, vec![e2, r, q], 0);
        push(self, "V_FMA_F32", e3, None, vec![den, q2, num], 1);
        push(self, "V_DIV_FMAS_F32", q3, None, vec![e3, r, q2], 0);
        push(self, "V_DIV_FIXUP_F32", d, None, vec![q3, b, a], 0);
    }

    /// Workgroup id component `k`. GFX12 has architected workgroup-id
    /// registers (ISA guide 3.5.3.4, CS SGPR load): X in TTMP9, Y in
    /// TTMP7[15:0], Z in TTMP7[31:16]; nothing is loaded after the user SGPRs.
    fn tgid(&mut self, k: u8) -> O {
        const TTMP7: u8 = 115;
        const TTMP9: u8 = 117;
        if k == 0 {
            return O::PS(TTMP9);
        }
        let t = self.vreg(Class::V, 1);
        self.valu("V_BFE_U32", O::R(t, 0), vec![O::PS(TTMP7), O::Imm(16 * (k as u32 - 1)), O::Imm(16)], 0);
        O::R(t, 0)
    }

    /// Atomic add of a uniform value `v` to a uniform address: one atomic
    /// per wave instead of one per lane. The first active lane adds
    /// `v * popcount(exec)`; every lane's result is the old value plus `v`
    /// times the number of active lanes below it (what the lanes would
    /// have seen in lane order). Lanes contending on one counter is the
    /// common append pattern, and per-lane atomics serialise at the L2.
    #[allow(clippy::too_many_arguments)]
    fn wave_atomic_add(&mut self, name: &'static str, saddr: u8, vaddr: O, ioff: i32, v: ValueId, r: ValueId, used: bool) -> Result<(), Error> {
        let s = |me: &mut Self| O::R(me.vreg(Class::S, 1), 0);
        // The per-lane addend as a scalar (constant, or read from lane 0 of
        // the active lanes: it is the same in all of them).
        let k = match self.src(v, 0)? {
            O::Imm(b) => O::Imm(b),
            x => {
                let t = s(self);
                self.valu("V_READFIRSTLANE_B32", t, vec![x], 0);
                t
            }
        };
        let n = s(self);
        self.salu("S_BCNT1_I32_B32", n, vec![O::Exec]);
        let total = if k == O::Imm(1) {
            n
        } else {
            let t = s(self);
            self.salu("S_MUL_I32", t, vec![n, k]);
            t
        };
        let data = O::R(self.vreg(Class::V, 1), 0);
        self.valu("V_MOV_B32", data, vec![total], 0);
        let (first, one, save) = (s(self), s(self), s(self));
        self.salu("S_CTZ_I32_B32", first, vec![O::Exec]);
        self.salu("S_LSHL_B32", one, vec![O::Imm(1), first]);
        // exec &= that lane (empty if no lane is active).
        self.salu("S_AND_SAVEEXEC_B32", save, vec![one]);
        let old = if used { Some(O::R(self.vreg(Class::V, 1), 0)) } else { None };
        self.mi.push(MI::Global { name, vdst: old, vaddr, saddr, data: Some(data), ioff, th: used as u8, scope: 2 });
        self.salu("S_MOV_B32", O::Exec, vec![save]);
        if let Some(old) = old {
            let base = s(self);
            self.valu("V_READFIRSTLANE_B32", base, vec![old], 0);
            let d = O::R(self.def_v(r), 0);
            self.valu("V_MBCNT_LO_U32_B32", d, vec![O::Exec, O::Imm(0)], 0);
            match k {
                O::Imm(1) => self.valu("V_ADD_NC_U32", d, vec![base, d], 0),
                // Lane index < 32, so a 24-bit multiply is exact for small k.
                O::Imm(b) if b < 1 << 24 => self.valu("V_MAD_U32_U24", d, vec![d, k, base], 0),
                _ => {
                    self.valu("V_MUL_LO_U32", d, vec![d, k], 0);
                    self.valu("V_ADD_NC_U32", d, vec![base, d], 0);
                }
            }
        }
        Ok(())
    }

    fn builtin(&mut self, b: Builtin, r: ValueId) -> Result<(), Error> {
        let ls = self.f.local_size;
        match b {
            Builtin::LocalId | Builtin::GlobalId => {
                let d = self.def_v(r);
                for k in 0..3u8 {
                    // Packed local id: x [9:0], y [19:10], z [29:20] in v0.
                    self.valu("V_BFE_U32", O::R(d, k), vec![O::PV(0), O::Imm(10 * k as u32), O::Imm(10)], 0);
                    if b == Builtin::GlobalId {
                        let g = self.tgid(k);
                        self.valu("V_MAD_U32_U24", O::R(d, k), vec![g, O::Imm(ls[k as usize]), O::R(d, k)], 0);
                    }
                }
            }
            Builtin::WorkgroupId => {
                let d = self.def_v(r);
                for k in 0..3u8 {
                    let g = self.tgid(k);
                    self.valu("V_MOV_B32", O::R(d, k), vec![g], 0);
                }
            }
            Builtin::LocalIndex => {
                let d = O::R(self.def_v(r), 0);
                let t = self.vreg(Class::V, 1);
                // x + lx * (y + ly * z)
                self.valu("V_BFE_U32", O::R(t, 0), vec![O::PV(0), O::Imm(20), O::Imm(10)], 0);
                self.valu("V_BFE_U32", d, vec![O::PV(0), O::Imm(10), O::Imm(10)], 0);
                self.valu("V_MAD_U32_U24", O::R(t, 0), vec![O::R(t, 0), O::Imm(ls[1]), d], 0);
                self.valu("V_BFE_U32", d, vec![O::PV(0), O::Imm(0), O::Imm(10)], 0);
                self.valu("V_MAD_U32_U24", d, vec![O::R(t, 0), O::Imm(ls[0]), d], 0);
            }
        }
        Ok(())
    }

    /// Restore EXEC after a region: `save` minus lanes that left.
    fn restore(&mut self, save: VR) {
        self.salu("S_MOV_B32", O::Exec, vec![O::R(save, 0)]);
        let r = self.ret;
        self.salu("S_AND_NOT1_B32", O::Exec, vec![O::Exec, O::R(r, 0)]);
        if let Some(l) = self.loops.last() {
            let (b, c) = (l.brk, l.cont);
            self.salu("S_AND_NOT1_B32", O::Exec, vec![O::Exec, O::R(b, 0)]);
            self.salu("S_AND_NOT1_B32", O::Exec, vec![O::Exec, O::R(c, 0)]);
        }
    }

    fn region(&mut self, r: &Region) -> Result<(), Error> {
        for n in r {
            match n {
                Node::Inst(i) => self.inst(i)?,
                Node::If { cond, then, els, results } => {
                    for v in results {
                        self.def_slot(*v);
                    }
                    let c = self.src(*cond, 0)?;
                    let save = self.vreg(Class::S, 1);
                    let (l_else, l_end) = (self.label(), self.label());
                    self.salu("S_AND_SAVEEXEC_B32", O::R(save, 0), vec![c]);
                    self.mi.push(MI::Branch { name: "S_CBRANCH_EXECZ", target: l_else });
                    self.yields.push(results.clone());
                    self.region(then)?;
                    self.mi.push(MI::Label(l_else));
                    if !els.is_empty() {
                        self.salu("S_AND_NOT1_B32", O::Exec, vec![O::R(save, 0), c]);
                        self.mi.push(MI::Branch { name: "S_CBRANCH_EXECZ", target: l_end });
                        self.region(els)?;
                    }
                    self.yields.pop();
                    self.mi.push(MI::Label(l_end));
                    if leaves(then) || leaves(els) {
                        self.restore(save);
                    } else {
                        self.salu("S_MOV_B32", O::Exec, vec![O::R(save, 0)]);
                    }
                }
                Node::Loop { inits, params, body, results } => {
                    for v in params.iter().chain(results) {
                        self.def_slot(*v);
                    }
                    for (p, i) in params.iter().zip(inits) {
                        self.copy_into(*p, *i)?;
                    }
                    let (brk, cont) = (self.vreg(Class::S, 1), self.vreg(Class::S, 1));
                    self.salu("S_MOV_B32", O::R(brk, 0), vec![O::Imm(0)]);
                    let top = self.label();
                    self.mi.push(MI::LoopBegin);
                    self.mi.push(MI::Label(top));
                    self.salu("S_MOV_B32", O::R(cont, 0), vec![O::Imm(0)]);
                    self.loops.push(LoopRegs { brk, cont, params: params.clone(), results: results.clone() });
                    self.region(body)?;
                    self.loops.pop();
                    self.salu("S_OR_B32", O::Exec, vec![O::Exec, O::R(cont, 0)]);
                    self.mi.push(MI::Branch { name: "S_CBRANCH_EXECNZ", target: top });
                    self.mi.push(MI::LoopEnd);
                    self.salu("S_MOV_B32", O::Exec, vec![O::R(brk, 0)]);
                }
            }
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<(), Error> {
        self.ret = self.vreg(Class::S, 1);
        let r = self.ret;
        self.salu("S_MOV_B32", O::R(r, 0), vec![O::Imm(0)]);
        let body = &self.f.body;
        self.region(body)?;
        self.mi.push(MI::Endpgm);
        Ok(())
    }
}
