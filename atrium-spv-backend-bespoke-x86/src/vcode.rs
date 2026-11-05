//! Lowering: atrium-spv-ir → VCode, a flat list of x86-shaped machine
//! instructions over virtual registers.
//!
//! Every IR value becomes one virtual register per lane: f32 lanes live in
//! the XMM class, i32/u32/bool lanes in the GPR class. Vectors are fully
//! scalarised (phase 5 adds packed forms). Pointers never get a register of
//! their own: they are an ABI argument register plus a constant offset.
//!
//! Blocks are laid out in reverse postorder, so every definition is lowered
//! before its uses (phi arms excepted, which only need the predecessor's
//! definitions). Phis become copies on the incoming edge; an edge out of a
//! conditional branch or switch gets its own little block for them, which
//! splits critical edges as a side effect.

use std::collections::{HashMap, HashSet};

use atrium_spv_ir::{
    BlockId, BuiltinKind, FloatKind, Function, Inst, IntKind, Op, StorageClass, Type, Value,
    ValueId, VecElement,
};

use crate::{unsupported, BackendError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct VReg(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// XMM register, f32 in lane 0.
    X,
    /// General-purpose register: 32-bit ints and bools, or a 64-bit pointer.
    G,
}

/// Physical register: hardware encoding number within its class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PReg {
    X(u8),
    G(u8),
}

pub(crate) mod gpr {
    pub const RDX: u8 = 2;
    pub const RSI: u8 = 6;
    pub const RDI: u8 = 7;
    pub const R8: u8 = 8;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Label(pub u32);

/// `[base + off]`, base a pointer-holding GPR vreg.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Addr {
    pub base: VReg,
    pub off: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum XOp {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
    And,
    Xor,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum XUnOp {
    Sqrt,
    /// `vroundss` with this immediate.
    Round(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
}

impl GOp {
    pub fn commutative(self) -> bool {
        !matches!(self, GOp::Sub)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ShiftOp {
    Shl,
    LShr,
    AShr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GUnOp {
    Neg,
    Not,
    Lzcnt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DivKind {
    UDiv,
    SDiv,
    UMod,
    /// SPIR-V OpSMod: floored, sign of the divisor.
    SMod,
}

/// Integer condition for `cmp a, b`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ICond {
    Eq,
    Ne,
    Ult,
    Ule,
    Ugt,
    Uge,
    Slt,
    Sle,
    Sgt,
    Sge,
}

/// Float comparison, SPIR-V semantics (ordered = false on NaN).
#[derive(Clone, Copy, Debug)]
pub(crate) enum FCond {
    OrdEq,
    OrdNe,
    OrdLt,
    OrdLe,
    OrdGt,
    OrdGe,
    UnordEq,
    UnordNe,
    UnordLt,
    UnordLe,
    UnordGt,
    UnordGe,
}

#[derive(Clone, Debug)]
pub(crate) enum MInst {
    /// PC-map marker: the next instruction comes from this SPIR-V offset.
    Mark(u32),
    Label(Label),
    /// Function entry: defines the ABI argument vregs (precoloured).
    Entry { defs: Vec<VReg> },

    XConst { d: VReg, bits: u32 },
    XMov { d: VReg, s: VReg },
    XBin { op: XOp, d: VReg, a: VReg, b: VReg },
    /// d = a * b + c, fused.
    XFma { d: VReg, a: VReg, b: VReg, c: VReg },
    XUn { op: XUnOp, d: VReg, a: VReg },
    XLoad { d: VReg, addr: Addr },
    XStore { addr: Addr, s: VReg },
    /// d = c != 0 ? t : f  (c is a GPR bool).
    XSelect { d: VReg, c: VReg, t: VReg, f: VReg },

    GConst { d: VReg, imm: i32 },
    GMov { d: VReg, s: VReg },
    GBin { op: GOp, d: VReg, a: VReg, b: VReg },
    GShift { op: ShiftOp, d: VReg, a: VReg, b: VReg },
    GUn { op: GUnOp, d: VReg, a: VReg },
    GDiv { kind: DivKind, d: VReg, a: VReg, b: VReg },
    GCmp { cc: ICond, d: VReg, a: VReg, b: VReg },
    FCmp { cc: FCond, d: VReg, a: VReg, b: VReg },
    GSelect { d: VReg, c: VReg, t: VReg, f: VReg },
    GLoad { d: VReg, addr: Addr },
    GStore { addr: Addr, s: VReg },

    /// i32 → f32.
    CvtSToF { d: VReg, a: VReg },
    /// u32 → f32.
    CvtUToF { d: VReg, a: VReg },
    /// f32 → i32, saturating, NaN → 0.
    CvtFToS { d: VReg, a: VReg },
    /// f32 → u32, saturating, NaN → 0.
    CvtFToU { d: VReg, a: VReg },
    /// Bit moves between the classes (Bitcast).
    MovXToG { d: VReg, s: VReg },
    MovGToX { d: VReg, s: VReg },

    Jmp(Label),
    /// if c != 0 goto t else goto f.
    Br { c: VReg, t: Label, f: Label },
    /// if s == imm goto t (falls through otherwise).
    CaseEq { s: VReg, imm: i32, t: Label },
    Ret,
}

impl MInst {
    pub fn uses(&self, out: &mut Vec<VReg>) {
        use MInst::*;
        match self {
            Mark(_) | Label(_) | Entry { .. } | XConst { .. } | GConst { .. } | Jmp(_) | Ret => {}
            XMov { s, .. } | GMov { s, .. } | MovXToG { s, .. } | MovGToX { s, .. } => out.push(*s),
            XBin { a, b, .. }
            | GBin { a, b, .. }
            | GShift { a, b, .. }
            | GDiv { a, b, .. }
            | GCmp { a, b, .. }
            | FCmp { a, b, .. } => {
                out.push(*a);
                out.push(*b);
            }
            XFma { a, b, c, .. } => out.extend([*a, *b, *c]),
            XUn { a, .. } | GUn { a, .. } => out.push(*a),
            CvtSToF { a, .. } | CvtUToF { a, .. } | CvtFToS { a, .. } | CvtFToU { a, .. } => {
                out.push(*a)
            }
            XLoad { addr, .. } | GLoad { addr, .. } => out.push(addr.base),
            XStore { addr, s } | GStore { addr, s } => {
                out.push(addr.base);
                out.push(*s);
            }
            XSelect { c, t, f, .. } | GSelect { c, t, f, .. } => out.extend([*c, *t, *f]),
            Br { c, .. } => out.push(*c),
            CaseEq { s, .. } => out.push(*s),
        }
    }

    pub fn defs(&self, out: &mut Vec<VReg>) {
        use MInst::*;
        match self {
            Entry { defs } => out.extend(defs.iter().copied()),
            XConst { d, .. }
            | XMov { d, .. }
            | XBin { d, .. }
            | XFma { d, .. }
            | XUn { d, .. }
            | XLoad { d, .. }
            | XSelect { d, .. }
            | GConst { d, .. }
            | GMov { d, .. }
            | GBin { d, .. }
            | GShift { d, .. }
            | GUn { d, .. }
            | GDiv { d, .. }
            | GCmp { d, .. }
            | FCmp { d, .. }
            | GSelect { d, .. }
            | GLoad { d, .. }
            | CvtSToF { d, .. }
            | CvtUToF { d, .. }
            | CvtFToS { d, .. }
            | CvtFToU { d, .. }
            | MovXToG { d, .. }
            | MovGToX { d, .. } => out.push(*d),
            _ => {}
        }
    }

    pub fn targets(&self, out: &mut Vec<Label>) {
        match self {
            MInst::Jmp(l) => out.push(*l),
            MInst::Br { t, f, .. } => {
                out.push(*t);
                out.push(*f);
            }
            MInst::CaseEq { t, .. } => out.push(*t),
            _ => {}
        }
    }
}

/// A pointer value: ABI argument vreg plus constant byte offset.
#[derive(Clone, Copy, Debug)]
struct Ptr {
    base: VReg,
    off: i32,
}

/// Fragment ABI argument (SysV FsMain).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Arg {
    InVaryings,
    Uniforms,
    PushConstants,
    OutColor,
    FragCoord(u8),
}

impl Arg {
    fn preg(self) -> PReg {
        match self {
            Arg::InVaryings => PReg::G(gpr::RDI),
            Arg::Uniforms => PReg::G(gpr::RSI),
            Arg::PushConstants => PReg::G(gpr::RDX),
            Arg::OutColor => PReg::G(gpr::R8),
            Arg::FragCoord(i) => PReg::X(i),
        }
    }
}

pub(crate) struct VCode {
    pub insts: Vec<MInst>,
    pub classes: Vec<Class>,
    /// Precoloured vregs (ABI arguments).
    pub fixed: HashMap<VReg, PReg>,
    /// Allocation hints: prefer the register of the given vreg.
    pub hints: HashMap<VReg, VReg>,
    pub num_labels: u32,
}

struct Lower<'f> {
    func: &'f Function,
    code: Vec<MInst>,
    classes: Vec<Class>,
    fixed: HashMap<VReg, PReg>,
    hints: HashMap<VReg, VReg>,
    lanes: HashMap<ValueId, Vec<VReg>>,
    ptrs: HashMap<ValueId, Ptr>,
    args: HashMap<Arg, VReg>,
    block_labels: HashMap<BlockId, Label>,
    num_labels: u32,
}

fn elem_class(e: &VecElement) -> Result<Class, BackendError> {
    match e {
        VecElement::F32 => Ok(Class::X),
        VecElement::I32 | VecElement::U32 => Ok(Class::G),
        VecElement::F64 => unsupported("f64 vectors"),
    }
}

/// Lane classes of a value type (pointers excluded).
pub(crate) fn lane_classes(ty: &Type) -> Result<Vec<Class>, BackendError> {
    Ok(match ty {
        Type::F32 => vec![Class::X],
        Type::Bool | Type::I32 | Type::U32 => vec![Class::G],
        Type::Vec2(e) => vec![elem_class(e)?; 2],
        Type::Vec3(e) => vec![elem_class(e)?; 3],
        Type::Vec4(e) => vec![elem_class(e)?; 4],
        other => return unsupported(format!("values of type {other:?}")),
    })
}

/// Blocks reachable from the entry, in reverse postorder.
fn rpo(func: &Function) -> Result<Vec<BlockId>, BackendError> {
    let succs = |b: BlockId| -> Vec<BlockId> {
        let Some(block) = func.blocks.get(&b) else { return vec![] };
        match block.insts.last().map(|i| &i.op) {
            Some(Op::Branch(t)) => vec![*t],
            Some(Op::BranchCond { t_block, f_block, .. }) => vec![*t_block, *f_block],
            Some(Op::Switch { cases, default, .. }) => {
                let mut v: Vec<BlockId> = cases.iter().map(|c| c.1).collect();
                v.push(*default);
                v
            }
            _ => vec![],
        }
    };
    let mut post = Vec::new();
    let mut seen = HashSet::new();
    // Iterative DFS: (block, next successor index).
    let mut stack = vec![(func.entry_block, 0usize)];
    seen.insert(func.entry_block);
    while let Some((b, i)) = stack.pop() {
        let s = succs(b);
        if i < s.len() {
            stack.push((b, i + 1));
            if seen.insert(s[i]) {
                if !func.blocks.contains_key(&s[i]) {
                    return Err(BackendError::Internal(format!("branch to missing block {:?}", s[i])));
                }
                stack.push((s[i], 0));
            }
        } else {
            post.push(b);
        }
    }
    post.reverse();
    Ok(post)
}

pub(crate) fn lower(func: &Function) -> Result<VCode, BackendError> {
    let mut l = Lower {
        func,
        code: vec![MInst::Entry { defs: vec![] }],
        classes: Vec::new(),
        fixed: HashMap::new(),
        hints: HashMap::new(),
        lanes: HashMap::new(),
        ptrs: HashMap::new(),
        args: HashMap::new(),
        block_labels: HashMap::new(),
        num_labels: 0,
    };
    let order = rpo(func)?;
    for &b in &order {
        let lab = l.new_label();
        l.block_labels.insert(b, lab);
    }
    for &b in &order {
        let block = &func.blocks[&b];
        l.code.push(MInst::Label(l.block_labels[&b]));
        for inst in &block.insts {
            l.code.push(MInst::Mark(inst.source_spirv_offset));
            l.lower_inst(b, inst)?;
        }
    }
    let mut defs: Vec<(Arg, VReg)> = l.args.iter().map(|(a, v)| (*a, *v)).collect();
    defs.sort_by_key(|(_, v)| *v);
    l.code[0] = MInst::Entry { defs: defs.iter().map(|(_, v)| *v).collect() };
    Ok(VCode {
        insts: l.code,
        classes: l.classes,
        fixed: l.fixed,
        hints: l.hints,
        num_labels: l.num_labels,
    })
}

impl<'f> Lower<'f> {
    fn new_label(&mut self) -> Label {
        let l = Label(self.num_labels);
        self.num_labels += 1;
        l
    }

    fn vreg(&mut self, c: Class) -> VReg {
        let v = VReg(self.classes.len() as u32);
        self.classes.push(c);
        v
    }

    fn arg(&mut self, a: Arg) -> VReg {
        if let Some(v) = self.args.get(&a) {
            return *v;
        }
        let c = match a.preg() {
            PReg::X(_) => Class::X,
            PReg::G(_) => Class::G,
        };
        let v = self.vreg(c);
        self.fixed.insert(v, a.preg());
        self.args.insert(a, v);
        v
    }

    fn lanes_of(&self, v: &Value) -> Result<Vec<VReg>, BackendError> {
        self.lanes
            .get(&v.id)
            .cloned()
            .ok_or_else(|| BackendError::Internal(format!("value {:?} used before definition", v.id)))
    }

    /// Fresh vregs for a result value.
    fn define(&mut self, res: &Value) -> Result<Vec<VReg>, BackendError> {
        let cls = lane_classes(&res.ty)?;
        let regs: Vec<VReg> = cls.into_iter().map(|c| self.vreg(c)).collect();
        self.lanes.insert(res.id, regs.clone());
        Ok(regs)
    }

    /// Phi destinations are created on first sight, which may be an edge
    /// copy in a predecessor lowered before the phi's own block.
    fn phi_regs(&mut self, res: &Value) -> Result<Vec<VReg>, BackendError> {
        if let Some(r) = self.lanes.get(&res.id) {
            return Ok(r.clone());
        }
        self.define(res)
    }

    fn result<'i>(inst: &'i Inst) -> Result<&'i Value, BackendError> {
        inst.result
            .as_ref()
            .ok_or_else(|| BackendError::Internal(format!("{:?} without result", inst.op)))
    }

    fn class_of(&self, v: VReg) -> Class {
        self.classes[v.0 as usize]
    }

    fn ptr_of(&mut self, v: &Value) -> Result<Ptr, BackendError> {
        if let Some(p) = self.ptrs.get(&v.id) {
            return Ok(*p);
        }
        // A bare variable: its storage class picks the ABI argument.
        let sc = match &v.ty {
            Type::Pointer(sc, _) => *sc,
            other => return unsupported(format!("pointer of type {other:?}")),
        };
        let arg = match sc {
            StorageClass::Input => Arg::InVaryings,
            StorageClass::Uniform => Arg::Uniforms,
            StorageClass::PushConstant => Arg::PushConstants,
            StorageClass::Output => Arg::OutColor,
            other => return unsupported(format!("fragment storage class {other:?}")),
        };
        let p = Ptr { base: self.arg(arg), off: 0 };
        self.ptrs.insert(v.id, p);
        Ok(p)
    }

    /// Lane-wise binary op; a scalar operand is broadcast to the vector.
    fn lanewise2(
        &mut self,
        inst: &Inst,
        a: &Value,
        b: &Value,
        want: Class,
        mk: impl Fn(VReg, VReg, VReg) -> MInst,
    ) -> Result<(), BackendError> {
        let res = Self::result(inst)?;
        let mut la = self.lanes_of(a)?;
        let mut lb = self.lanes_of(b)?;
        let n = lane_classes(&res.ty)?.len();
        if la.len() == 1 && n > 1 {
            la = vec![la[0]; n];
        }
        if lb.len() == 1 && n > 1 {
            lb = vec![lb[0]; n];
        }
        if la.len() != n || lb.len() != n {
            return unsupported(format!("{}: lane counts", op_name(&inst.op)));
        }
        if la.iter().chain(&lb).any(|v| self.class_of(*v) != want) {
            return unsupported(format!("{} on {:?}", op_name(&inst.op), a.ty));
        }
        let d = self.define(res)?;
        for i in 0..n {
            self.code.push(mk(d[i], la[i], lb[i]));
        }
        Ok(())
    }

    /// Lane-wise unary op from class `from` to the result's class.
    fn lanewise1(
        &mut self,
        inst: &Inst,
        a: &Value,
        from: Class,
        mk: impl Fn(VReg, VReg) -> MInst,
    ) -> Result<(), BackendError> {
        let res = Self::result(inst)?;
        let la = self.lanes_of(a)?;
        if la.iter().any(|v| self.class_of(*v) != from) {
            return unsupported(format!("{} on {:?}", op_name(&inst.op), a.ty));
        }
        let d = self.define(res)?;
        if d.len() != la.len() {
            return unsupported(format!("{}: lane counts", op_name(&inst.op)));
        }
        for i in 0..d.len() {
            self.code.push(mk(d[i], la[i]));
        }
        Ok(())
    }

    fn xbin(&mut self, inst: &Inst, a: &Value, b: &Value, op: XOp) -> Result<(), BackendError> {
        self.lanewise2(inst, a, b, Class::X, |d, a, b| MInst::XBin { op, d, a, b })
    }

    fn gbin(&mut self, inst: &Inst, a: &Value, b: &Value, op: GOp) -> Result<(), BackendError> {
        self.lanewise2(inst, a, b, Class::G, |d, a, b| MInst::GBin { op, d, a, b })?;
        self.hint_last(inst);
        Ok(())
    }

    /// Hint each lane result of a two-address op to its first operand.
    fn hint_last(&mut self, inst: &Inst) {
        let Some(res) = &inst.result else { return };
        let Some(d) = self.lanes.get(&res.id).cloned() else { return };
        let n = d.len();
        for k in 0..n {
            if let Some(MInst::GBin { a, .. } | MInst::GUn { a, .. }) =
                self.code.get(self.code.len() - n + k)
            {
                self.hints.insert(d[k], *a);
            }
        }
    }

    fn xmask(&mut self, inst: &Inst, a: &Value, bits: u32, op: XOp) -> Result<(), BackendError> {
        let m = self.vreg(Class::X);
        self.code.push(MInst::XConst { d: m, bits });
        self.lanewise1(inst, a, Class::X, |d, a| MInst::XBin { op, d, a, b: m })
    }

    fn icmp(&mut self, inst: &Inst, a: &Value, b: &Value, cc: ICond) -> Result<(), BackendError> {
        self.lanewise2(inst, a, b, Class::G, |d, a, b| MInst::GCmp { cc, d, a, b })
    }

    fn fcmp(&mut self, inst: &Inst, a: &Value, b: &Value, cc: FCond) -> Result<(), BackendError> {
        let res = Self::result(inst)?;
        let (la, lb) = (self.lanes_of(a)?, self.lanes_of(b)?);
        if la.len() != lb.len() || la.iter().chain(&lb).any(|v| self.class_of(*v) != Class::X) {
            return unsupported(format!("{} on {:?}", op_name(&inst.op), a.ty));
        }
        let d: Vec<VReg> = (0..la.len()).map(|_| self.vreg(Class::G)).collect();
        if lane_classes(&res.ty)?.len() != d.len() {
            return unsupported("float compare result lanes");
        }
        self.lanes.insert(res.id, d.clone());
        for i in 0..d.len() {
            self.code.push(MInst::FCmp { cc, d: d[i], a: la[i], b: lb[i] });
        }
        Ok(())
    }

    fn div(&mut self, inst: &Inst, a: &Value, b: &Value, kind: DivKind) -> Result<(), BackendError> {
        self.lanewise2(inst, a, b, Class::G, |d, a, b| MInst::GDiv { kind, d, a, b })
    }

    fn shift(&mut self, inst: &Inst, a: &Value, b: &Value, op: ShiftOp) -> Result<(), BackendError> {
        self.lanewise2(inst, a, b, Class::G, |d, a, b| MInst::GShift { op, d, a, b })
    }

    /// Emit the copies for edge `from → to`. Reads all sources before
    /// writing any destination when they overlap (the swap problem).
    fn edge_copies(&mut self, from: BlockId, to: BlockId) -> Result<(), BackendError> {
        let block = &self.func.blocks[&to];
        let mut moves: Vec<(VReg, VReg)> = Vec::new();
        for inst in &block.insts {
            let Op::Phi(arms) = &inst.op else { continue };
            let res = Self::result(inst)?;
            let arm = arms.iter().find(|a| a.from == from).ok_or_else(|| {
                BackendError::Internal(format!("phi in {to:?} has no arm from {from:?}"))
            })?;
            let src = self.lanes_of(&arm.value)?;
            let dst = self.phi_regs(res)?;
            if src.len() != dst.len() {
                return Err(BackendError::Internal("phi lane count".into()));
            }
            moves.extend(dst.into_iter().zip(src));
        }
        let dsts: HashSet<VReg> = moves.iter().map(|m| m.0).collect();
        let overlap = moves.iter().any(|(d, s)| d != s && dsts.contains(s));
        let mv = |d: VReg, s: VReg, c: Class| match c {
            Class::X => MInst::XMov { d, s },
            Class::G => MInst::GMov { d, s },
        };
        if overlap {
            let mut temps = Vec::new();
            for &(_, s) in &moves {
                let c = self.class_of(s);
                let t = self.vreg(c);
                self.hints.insert(t, s);
                self.code.push(mv(t, s, c));
                temps.push(t);
            }
            for (k, &(d, _)) in moves.iter().enumerate() {
                let c = self.class_of(d);
                self.hints.entry(d).or_insert(temps[k]);
                self.code.push(mv(d, temps[k], c));
            }
        } else {
            for &(d, s) in &moves {
                if d != s {
                    let c = self.class_of(d);
                    self.hints.entry(d).or_insert(s);
                    self.code.push(mv(d, s, c));
                }
            }
        }
        Ok(())
    }

    fn has_phis(&self, b: BlockId) -> bool {
        self.func.blocks[&b].insts.iter().any(|i| matches!(i.op, Op::Phi(_)))
    }

    fn lower_inst(&mut self, cur: BlockId, inst: &Inst) -> Result<(), BackendError> {
        use MInst as M;
        match &inst.op {
            // ── constants ─────────────────────────────────────────────
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let d = self.define(Self::result(inst)?)?;
                self.code.push(M::XConst { d: d[0], bits: (*value as f32).to_bits() });
            }
            Op::ConstInt { value, kind: IntKind::I32 | IntKind::U32 } => {
                let res = Self::result(inst)?;
                let d = self.define(res)?;
                if self.class_of(d[0]) != Class::G {
                    return unsupported(format!("ConstInt of type {:?}", res.ty));
                }
                self.code.push(M::GConst { d: d[0], imm: *value as i32 });
            }
            Op::ConstNull => {
                let res = Self::result(inst)?;
                let d = self.define(res)?;
                for v in d {
                    match self.class_of(v) {
                        Class::X => self.code.push(M::XConst { d: v, bits: 0 }),
                        Class::G => self.code.push(M::GConst { d: v, imm: 0 }),
                    }
                }
            }
            Op::ConstVec(parts) => {
                let res = Self::result(inst)?;
                let mut regs = Vec::new();
                for p in parts {
                    regs.extend(self.lanes_of(p)?);
                }
                if regs.len() != lane_classes(&res.ty)?.len() {
                    return unsupported("ConstVec lane count");
                }
                self.lanes.insert(res.id, regs);
            }

            // ── float arithmetic ──────────────────────────────────────
            Op::FAdd(a, b) => self.xbin(inst, a, b, XOp::Add)?,
            Op::FSub(a, b) => self.xbin(inst, a, b, XOp::Sub)?,
            Op::FMul(a, b) => self.xbin(inst, a, b, XOp::Mul)?,
            Op::FDiv(a, b) => self.xbin(inst, a, b, XOp::Div)?,
            Op::FMin(a, b) => self.xbin(inst, a, b, XOp::Min)?,
            Op::FMax(a, b) => self.xbin(inst, a, b, XOp::Max)?,
            Op::Fma(a, b, c) => {
                let res = Self::result(inst)?;
                let (la, lb, lc) = (self.lanes_of(a)?, self.lanes_of(b)?, self.lanes_of(c)?);
                let d = self.define(res)?;
                let n = d.len();
                if la.len() != n || lb.len() != n || lc.len() != n {
                    return unsupported("Fma lane counts");
                }
                for i in 0..n {
                    self.code.push(M::XFma { d: d[i], a: la[i], b: lb[i], c: lc[i] });
                }
            }
            Op::FNeg(a) => self.xmask(inst, a, 0x8000_0000, XOp::Xor)?,
            Op::FAbs(a) => self.xmask(inst, a, 0x7fff_ffff, XOp::And)?,
            Op::FSqrt(a) => self.lanewise1(inst, a, Class::X, |d, a| M::XUn { op: XUnOp::Sqrt, d, a })?,
            // vroundss imm: 8 = suppress precision exception; 1 floor,
            // 2 ceil, 3 trunc.
            Op::FFloor(a) => {
                self.lanewise1(inst, a, Class::X, |d, a| M::XUn { op: XUnOp::Round(9), d, a })?
            }
            Op::FCeil(a) => {
                self.lanewise1(inst, a, Class::X, |d, a| M::XUn { op: XUnOp::Round(10), d, a })?
            }
            Op::FTrunc(a) => {
                self.lanewise1(inst, a, Class::X, |d, a| M::XUn { op: XUnOp::Round(11), d, a })?
            }
            Op::Dot(a, b) => {
                // Multiply and add separately (no FMA): the interpreter and
                // Cranelift round after each step and tests compare exactly.
                let res = Self::result(inst)?;
                let (la, lb) = (self.lanes_of(a)?, self.lanes_of(b)?);
                if la.len() != lb.len() || la.is_empty() || res.ty != Type::F32 {
                    return unsupported("Dot shape");
                }
                let mut acc = self.vreg(Class::X);
                self.code.push(M::XBin { op: XOp::Mul, d: acc, a: la[0], b: lb[0] });
                for i in 1..la.len() {
                    let p = self.vreg(Class::X);
                    self.code.push(M::XBin { op: XOp::Mul, d: p, a: la[i], b: lb[i] });
                    let s = self.vreg(Class::X);
                    self.code.push(M::XBin { op: XOp::Add, d: s, a: acc, b: p });
                    acc = s;
                }
                self.lanes.insert(res.id, vec![acc]);
            }

            // ── integer arithmetic ────────────────────────────────────
            Op::IAdd(a, b) => self.gbin(inst, a, b, GOp::Add)?,
            Op::ISub(a, b) => self.gbin(inst, a, b, GOp::Sub)?,
            Op::IMul(a, b) => self.gbin(inst, a, b, GOp::Mul)?,
            Op::BitAnd(a, b) => self.gbin(inst, a, b, GOp::And)?,
            Op::BitOr(a, b) => self.gbin(inst, a, b, GOp::Or)?,
            Op::BitXor(a, b) => self.gbin(inst, a, b, GOp::Xor)?,
            Op::UDiv(a, b) => self.div(inst, a, b, DivKind::UDiv)?,
            Op::SDiv(a, b) => self.div(inst, a, b, DivKind::SDiv)?,
            Op::UMod(a, b) => self.div(inst, a, b, DivKind::UMod)?,
            Op::SMod(a, b) => self.div(inst, a, b, DivKind::SMod)?,
            Op::Shl(a, b) => self.shift(inst, a, b, ShiftOp::Shl)?,
            Op::LShr(a, b) => self.shift(inst, a, b, ShiftOp::LShr)?,
            Op::AShr(a, b) => self.shift(inst, a, b, ShiftOp::AShr)?,
            Op::INeg(a) => {
                self.lanewise1(inst, a, Class::G, |d, a| M::GUn { op: GUnOp::Neg, d, a })?;
                self.hint_last(inst);
            }
            Op::BitNot(a) => {
                self.lanewise1(inst, a, Class::G, |d, a| M::GUn { op: GUnOp::Not, d, a })?;
                self.hint_last(inst);
            }
            Op::Clz(a) => self.lanewise1(inst, a, Class::G, |d, a| M::GUn { op: GUnOp::Lzcnt, d, a })?,

            // ── comparisons ───────────────────────────────────────────
            Op::IEq(a, b) => self.icmp(inst, a, b, ICond::Eq)?,
            Op::INe(a, b) => self.icmp(inst, a, b, ICond::Ne)?,
            Op::ULt(a, b) => self.icmp(inst, a, b, ICond::Ult)?,
            Op::ULe(a, b) => self.icmp(inst, a, b, ICond::Ule)?,
            Op::UGt(a, b) => self.icmp(inst, a, b, ICond::Ugt)?,
            Op::UGe(a, b) => self.icmp(inst, a, b, ICond::Uge)?,
            Op::SLt(a, b) => self.icmp(inst, a, b, ICond::Slt)?,
            Op::SLe(a, b) => self.icmp(inst, a, b, ICond::Sle)?,
            Op::SGt(a, b) => self.icmp(inst, a, b, ICond::Sgt)?,
            Op::SGe(a, b) => self.icmp(inst, a, b, ICond::Sge)?,
            Op::FOrdEq(a, b) => self.fcmp(inst, a, b, FCond::OrdEq)?,
            Op::FOrdNe(a, b) => self.fcmp(inst, a, b, FCond::OrdNe)?,
            Op::FOrdLt(a, b) => self.fcmp(inst, a, b, FCond::OrdLt)?,
            Op::FOrdLe(a, b) => self.fcmp(inst, a, b, FCond::OrdLe)?,
            Op::FOrdGt(a, b) => self.fcmp(inst, a, b, FCond::OrdGt)?,
            Op::FOrdGe(a, b) => self.fcmp(inst, a, b, FCond::OrdGe)?,
            Op::FUnordEq(a, b) => self.fcmp(inst, a, b, FCond::UnordEq)?,
            Op::FUnordNe(a, b) => self.fcmp(inst, a, b, FCond::UnordNe)?,
            Op::FUnordLt(a, b) => self.fcmp(inst, a, b, FCond::UnordLt)?,
            Op::FUnordLe(a, b) => self.fcmp(inst, a, b, FCond::UnordLe)?,
            Op::FUnordGt(a, b) => self.fcmp(inst, a, b, FCond::UnordGt)?,
            Op::FUnordGe(a, b) => self.fcmp(inst, a, b, FCond::UnordGe)?,

            Op::Select { cond, t_val, f_val } => {
                let res = Self::result(inst)?;
                let (lc, lt, lf) = (self.lanes_of(cond)?, self.lanes_of(t_val)?, self.lanes_of(f_val)?);
                let d = self.define(res)?;
                let n = d.len();
                if lt.len() != n || lf.len() != n || !(lc.len() == 1 || lc.len() == n) {
                    return unsupported("Select lane counts");
                }
                if lc.iter().any(|v| self.class_of(*v) != Class::G) {
                    return unsupported("Select on a non-integer condition");
                }
                for i in 0..n {
                    let c = if lc.len() == 1 { lc[0] } else { lc[i] };
                    self.code.push(match self.class_of(d[i]) {
                        Class::X => M::XSelect { d: d[i], c, t: lt[i], f: lf[i] },
                        Class::G => M::GSelect { d: d[i], c, t: lt[i], f: lf[i] },
                    });
                }
            }

            // ── conversions ───────────────────────────────────────────
            Op::ConvertSToF(a) => self.lanewise1(inst, a, Class::G, |d, a| M::CvtSToF { d, a })?,
            Op::ConvertUToF(a) => self.lanewise1(inst, a, Class::G, |d, a| M::CvtUToF { d, a })?,
            Op::ConvertFToS(a) => self.lanewise1(inst, a, Class::X, |d, a| M::CvtFToS { d, a })?,
            Op::ConvertFToU(a) => self.lanewise1(inst, a, Class::X, |d, a| M::CvtFToU { d, a })?,
            Op::SConvert(a, IntKind::I32 | IntKind::U32)
            | Op::UConvert(a, IntKind::I32 | IntKind::U32)
            | Op::FConvert(a, FloatKind::F32) => {
                let res = Self::result(inst)?;
                let la = self.lanes_of(a)?;
                if lane_classes(&res.ty)? != la.iter().map(|v| self.class_of(*v)).collect::<Vec<_>>() {
                    return unsupported(format!("{} {:?} -> {:?}", op_name(&inst.op), a.ty, res.ty));
                }
                self.lanes.insert(res.id, la);
            }
            Op::Bitcast(a, _) => {
                let res = Self::result(inst)?;
                let la = self.lanes_of(a)?;
                let want = lane_classes(&res.ty)?;
                if want.len() != la.len() {
                    return unsupported("Bitcast changing the lane count");
                }
                let mut out = Vec::new();
                for (i, &s) in la.iter().enumerate() {
                    let have = self.class_of(s);
                    if have == want[i] {
                        out.push(s);
                        continue;
                    }
                    let d = self.vreg(want[i]);
                    self.code.push(match want[i] {
                        Class::G => M::MovXToG { d, s },
                        Class::X => M::MovGToX { d, s },
                    });
                    out.push(d);
                }
                self.lanes.insert(res.id, out);
            }

            // ── vectors ───────────────────────────────────────────────
            Op::VectorExtract { vector, index } => {
                let res = Self::result(inst)?;
                let l = self.lanes_of(vector)?;
                let r = *l
                    .get(*index as usize)
                    .ok_or_else(|| BackendError::Internal("VectorExtract index".into()))?;
                self.lanes.insert(res.id, vec![r]);
            }
            Op::VectorInsert { vector, scalar, index } => {
                let res = Self::result(inst)?;
                let mut l = self.lanes_of(vector)?;
                let s = self.lanes_of(scalar)?;
                *l.get_mut(*index as usize)
                    .ok_or_else(|| BackendError::Internal("VectorInsert index".into()))? = s[0];
                self.lanes.insert(res.id, l);
            }
            Op::VectorShuffle { src1, src2, components } => {
                let res = Self::result(inst)?;
                let (l1, l2) = (self.lanes_of(src1)?, self.lanes_of(src2)?);
                let mut regs = Vec::new();
                for &c in components {
                    let c = c as usize;
                    regs.push(if c < l1.len() {
                        l1[c]
                    } else if c - l1.len() < l2.len() {
                        l2[c - l1.len()]
                    } else {
                        return unsupported("VectorShuffle undefined component");
                    });
                }
                self.lanes.insert(res.id, regs);
            }

            // ── memory ────────────────────────────────────────────────
            Op::LoadBuiltin(BuiltinKind::FragCoord) => {
                let res = Self::result(inst)?;
                let regs: Vec<VReg> = (0..4).map(|i| self.arg(Arg::FragCoord(i))).collect();
                let n = lane_classes(&res.ty)?.len();
                self.lanes.insert(res.id, regs[..n.min(4)].to_vec());
            }
            Op::AccessChain { base, byte_offset } => {
                let res = Self::result(inst)?;
                let p = self.ptr_of(base)?;
                let off = i32::try_from(*byte_offset)
                    .ok()
                    .and_then(|o| p.off.checked_add(o))
                    .ok_or_else(|| BackendError::Unsupported("AccessChain offset overflow".into()))?;
                self.ptrs.insert(res.id, Ptr { base: p.base, off });
            }
            Op::Load(ptr) => {
                let res = Self::result(inst)?;
                let p = self.ptr_of(ptr)?;
                let d = self.define(res)?;
                for (i, &v) in d.iter().enumerate() {
                    let addr = Addr { base: p.base, off: p.off + 4 * i as i32 };
                    self.code.push(match self.class_of(v) {
                        Class::X => M::XLoad { d: v, addr },
                        Class::G => M::GLoad { d: v, addr },
                    });
                }
            }
            Op::Store { ptr, value } => {
                let p = self.ptr_of(ptr)?;
                if !matches!(&ptr.ty, Type::Pointer(StorageClass::Output, _))
                    && self.args.get(&Arg::OutColor) != Some(&p.base)
                {
                    return unsupported("fragment store to a non-output pointer");
                }
                for (i, v) in self.lanes_of(value)?.into_iter().enumerate() {
                    let addr = Addr { base: p.base, off: p.off + 4 * i as i32 };
                    self.code.push(match self.class_of(v) {
                        Class::X => M::XStore { addr, s: v },
                        Class::G => M::GStore { addr, s: v },
                    });
                }
            }

            // ── control flow ──────────────────────────────────────────
            Op::Phi(_) => {
                // Destinations are written by the edge copies.
                self.phi_regs(Self::result(inst)?)?;
            }
            Op::Branch(t) => {
                self.edge_copies(cur, *t)?;
                self.code.push(M::Jmp(self.block_labels[t]));
            }
            Op::BranchCond { cond, t_block, f_block } => {
                let c = self.lanes_of(cond)?;
                if c.len() != 1 || self.class_of(c[0]) != Class::G {
                    return unsupported("BranchCond on a non-scalar condition");
                }
                let mut pending = Vec::new();
                let mut target = |l: &mut Self, b: BlockId| -> Label {
                    if l.has_phis(b) {
                        let e = l.new_label();
                        pending.push((e, b));
                        e
                    } else {
                        l.block_labels[&b]
                    }
                };
                let (t, f) = if t_block == f_block {
                    let x = target(self, *t_block);
                    (x, x)
                } else {
                    (target(self, *t_block), target(self, *f_block))
                };
                self.code.push(M::Br { c: c[0], t, f });
                self.edge_blocks(cur, pending)?;
            }
            Op::Switch { selector, cases, default } => {
                let s = self.lanes_of(selector)?;
                if s.len() != 1 || self.class_of(s[0]) != Class::G {
                    return unsupported("Switch on a non-scalar selector");
                }
                let mut edge_label: HashMap<BlockId, Label> = HashMap::new();
                let mut pending = Vec::new();
                let mut target = |l: &mut Self, b: BlockId| -> Label {
                    if !l.has_phis(b) {
                        return l.block_labels[&b];
                    }
                    *edge_label.entry(b).or_insert_with(|| {
                        let e = l.new_label();
                        pending.push((e, b));
                        e
                    })
                };
                for &(v, b) in cases {
                    let t = target(self, b);
                    self.code.push(M::CaseEq { s: s[0], imm: v as i32, t });
                }
                let d = target(self, *default);
                self.code.push(M::Jmp(d));
                self.edge_blocks(cur, pending)?;
            }
            Op::Return => self.code.push(M::Ret),

            other => return unsupported(op_name(other)),
        }
        Ok(())
    }

    /// Edge blocks for phi copies: `label: copies; jmp target`.
    fn edge_blocks(&mut self, cur: BlockId, pending: Vec<(Label, BlockId)>) -> Result<(), BackendError> {
        for (e, b) in pending {
            self.code.push(MInst::Label(e));
            self.edge_copies(cur, b)?;
            self.code.push(MInst::Jmp(self.block_labels[&b]));
        }
        Ok(())
    }
}

pub(crate) fn op_name(op: &Op) -> String {
    let s = format!("{op:?}");
    s.split(|c: char| c == '(' || c == ' ' || c == '{').next().unwrap_or("?").to_string()
}

/// Remove blocks that do nothing but jump (`label: [marks] jmp t`) by
/// pointing their predecessors at `t`. Afterwards every label is preceded by
/// a real instruction, so the emitter never needs two labels on one spot.
/// The entry block stays (nothing jumps to it, and it is the fall-in).
pub(crate) fn thread_jumps(vc: &mut VCode) {
    let insts = &vc.insts;
    let first = insts.iter().find_map(|i| match i {
        MInst::Label(l) => Some(*l),
        _ => None,
    });
    let mut redirect: HashMap<Label, Label> = HashMap::new();
    for (i, inst) in insts.iter().enumerate() {
        let MInst::Label(l) = inst else { continue };
        if Some(*l) == first {
            continue;
        }
        let mut j = i + 1;
        while matches!(insts.get(j), Some(MInst::Mark(_))) {
            j += 1;
        }
        if let Some(MInst::Jmp(t)) = insts.get(j) {
            if t != l {
                redirect.insert(*l, *t);
            }
        }
    }
    // Resolve chains; a label caught in a jump cycle keeps its block.
    let resolve = |l: Label| -> Option<Label> {
        let mut cur = l;
        for _ in 0..=redirect.len() {
            match redirect.get(&cur) {
                Some(&n) => cur = n,
                None => return Some(cur),
            }
        }
        None
    };
    let finals: HashMap<Label, Label> =
        redirect.keys().filter_map(|&l| resolve(l).map(|t| (l, t))).collect();
    if finals.is_empty() {
        return;
    }
    let map = |l: &mut Label| {
        if let Some(t) = finals.get(l) {
            *l = *t;
        }
    };
    let mut out = Vec::with_capacity(vc.insts.len());
    let mut skipping_jmp = false;
    for mut inst in std::mem::take(&mut vc.insts) {
        match &mut inst {
            MInst::Label(l) if finals.contains_key(l) => {
                skipping_jmp = true;
                continue;
            }
            MInst::Jmp(_) if skipping_jmp => {
                skipping_jmp = false;
                continue;
            }
            MInst::Mark(_) => {}
            MInst::Jmp(l) => map(l),
            MInst::Br { t, f, .. } => {
                map(t);
                map(f);
            }
            MInst::CaseEq { t, .. } => map(t),
            _ => {}
        }
        out.push(inst);
    }
    vc.insts = out;
}

/// Drop instructions whose results are never read. Every instruction with
/// a result is free of side effects (loads included, and division cannot
/// trap), so this only has to iterate until nothing changes.
pub(crate) fn dce(vc: &mut VCode) {
    let mut tmp = Vec::new();
    loop {
        let mut used = vec![false; vc.classes.len()];
        for inst in &vc.insts {
            tmp.clear();
            inst.uses(&mut tmp);
            for v in &tmp {
                used[v.0 as usize] = true;
            }
        }
        let before = vc.insts.len();
        vc.insts.retain(|inst| {
            if matches!(inst, MInst::Entry { .. }) {
                return true;
            }
            let mut defs = Vec::new();
            inst.defs(&mut defs);
            defs.is_empty() || defs.iter().any(|v| used[v.0 as usize])
        });
        if vc.insts.len() == before {
            break;
        }
    }
}
