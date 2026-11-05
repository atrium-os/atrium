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
    BlockId, BuiltinKind, FloatKind, Function, ImageDimensionality, Inst, IntKind, Op, ShaderStage, StorageClass, Type,
    Value, ValueId, VecElement,
};

use crate::{unsupported, BackendError};

/// `atrium_spv_runtime::IMG_TABLE_BARRIER_OFFSET`: the image table's slot
/// for the workgroup barrier function. Pinned by a unit test.
pub(crate) const IMG_TABLE_BARRIER_OFFSET: i32 = 64;
/// `atrium_spv_runtime::IMG_TABLE_DESC_BASE`: the compute image table's
/// first storage-image descriptor pointer. Pinned by a unit test.
pub(crate) const IMG_TABLE_DESC_BASE: i32 = 72;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct VReg(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// XMM register, f32 in lane 0.
    X,
    /// General-purpose register holding a 32-bit int or bool (upper half
    /// always zero, as every 32-bit write leaves it).
    G,
    /// General-purpose register holding a 64-bit pointer.
    P,
    /// XMM register, packed f32 lanes (2-4 of them; the rest undefined).
    V,
}

impl Class {
    /// Register file: XMM or GPR.
    pub fn is_xmm(self) -> bool {
        matches!(self, Class::X | Class::V)
    }
}

/// Physical register: hardware encoding number within its class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PReg {
    X(u8),
    G(u8),
}

pub(crate) mod gpr {
    pub const RCX: u8 = 1;
    pub const RDX: u8 = 2;
    pub const RSI: u8 = 6;
    pub const RDI: u8 = 7;
    pub const R8: u8 = 8;
    pub const R9: u8 = 9;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Label(pub u32);

/// Base of an address: a pointer vreg, or this function's local area
/// (LocalAlloc) on the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Base {
    V(VReg),
    Locals,
    /// The helper-call area: a 16-byte out buffer that runtime helpers
    /// read or write.
    Helper,
}

/// `[base + off]`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Addr {
    pub base: Base,
    pub off: i32,
}

impl Addr {
    /// The GPR the base lives in, if it is a register-allocated vreg.
    /// A spilled base is reloaded into r11, which is never a temporary.
    pub fn base_reg(&self, locs: &[crate::regalloc::Loc]) -> Option<u8> {
        match self.base {
            Base::V(v) => match locs[v.0 as usize] {
                crate::regalloc::Loc::Reg(PReg::G(r)) => Some(r),
                _ => None,
            },
            Base::Locals | Base::Helper => None,
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AtomicOp {
    Add,
    And,
    Or,
    Xor,
    SMin,
    SMax,
    UMin,
    UMax,
    Xchg,
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
    /// Bit reverse (bswap, then swap nibbles, pairs and bits).
    Rbit,
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

/// One argument of a [`MInst::CallHelper`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum CArg {
    /// A vreg; its class picks the register file.
    V(VReg),
    /// A 32-bit immediate (integer register).
    Imm(i32),
    /// The 64-bit pointer at `[table + off]` (a descriptor).
    TableLoad(i32),
    /// The address of the helper area's out buffer.
    OutBuf,
}

/// Bytes of helper area a call with `n` arguments needs: the out buffer.
pub(crate) fn helper_area_bytes(_n: usize) -> u32 {
    16
}

#[derive(Clone, Debug)]
pub(crate) enum MInst {
    /// PC-map marker: the next instruction comes from this SPIR-V offset.
    Mark(u32),
    Label(Label),
    /// Function entry: defines the ABI argument vregs (precoloured).
    Entry { defs: Vec<VReg> },
    /// Load a stack-passed argument; `off` is from the entry rsp (8 = the
    /// first stack argument). `wide` loads 64 bits.
    StackArg { d: VReg, off: i32, wide: bool },
    /// d = base + zext(idx) * stride (64-bit pointer arithmetic).
    PtrAdd { d: VReg, base: Base, idx: VReg, stride: u32 },
    /// Load a 64-bit pointer.
    PLoad { d: VReg, addr: Addr },
    /// 64-bit register move.
    PMov { d: VReg, s: VReg },
    /// `call [table + off]`, a runtime helper taking no arguments.
    /// Clobbers every caller-saved register.
    Call { table: VReg, off: i32 },
    /// `call [table + fn_off]` with SysV arguments, a runtime helper
    /// (texture sampling, storage images, derivatives). Integer-class
    /// arguments go to rdi, rsi, rdx, rcx, r8, r9 in order, X arguments to
    /// xmm0-7. `ret` takes eax or xmm0 by its class. Clobbers every
    /// caller-saved register.
    CallHelper { table: VReg, fn_off: i32, args: Vec<CArg>, ret: Option<VReg> },
    /// d = old *addr; *addr = op(old, v), atomically.
    AtomicRmw { op: AtomicOp, d: VReg, addr: Addr, v: VReg },
    /// d = old *addr; if old == expected { *addr = desired }, atomically.
    AtomicCas { d: VReg, addr: Addr, expected: VReg, desired: VReg },
    /// Sequentially consistent store.
    AtomicStore { addr: Addr, s: VReg },

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
    /// Packed f32 (class V): load or store `n` (2-4) lanes.
    VLoad { d: VReg, addr: Addr, n: u8 },
    VStore { addr: Addr, s: VReg, n: u8 },
    /// Lane-wise add/sub/mul/div/min/max.
    VBin { op: XOp, d: VReg, a: VReg, b: VReg },
    /// d = a * b + c lane-wise, fused.
    VFma { d: VReg, a: VReg, b: VReg, c: VReg },
    /// Every lane = the scalar s.
    VBroadcast { d: VReg, s: VReg },
    /// Packed from scalar lanes.
    VPack { d: VReg, lanes: Vec<VReg> },
    /// Scalar d = lane `lane` of packed s.
    XExtract { d: VReg, s: VReg, lane: u8 },
    /// GBin, GShift and GCmp with a constant right operand.
    GBinI { op: GOp, d: VReg, a: VReg, imm: i32 },
    GShiftI { op: ShiftOp, d: VReg, a: VReg, imm: u8 },
    GCmpI { cc: ICond, d: VReg, a: VReg, imm: i32 },
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
            Mark(_) | Label(_) | Entry { .. } | StackArg { .. } | XConst { .. } | GConst { .. }
            | Jmp(_) | Ret => {}
            PMov { s, .. } => out.push(*s),
            Call { table, .. } => out.push(*table),
            CallHelper { table, args, .. } => {
                out.push(*table);
                for a in args {
                    if let CArg::V(v) = a {
                        out.push(*v);
                    }
                }
            }
            AtomicRmw { addr, v, .. } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
                out.push(*v);
            }
            AtomicCas { addr, expected, desired, .. } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
                out.push(*expected);
                out.push(*desired);
            }
            AtomicStore { addr, s } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
                out.push(*s);
            }
            PtrAdd { base, idx, .. } => {
                if let Base::V(b) = base {
                    out.push(*b);
                }
                out.push(*idx);
            }
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
            XUn { a, .. } | GUn { a, .. } | GBinI { a, .. } | GShiftI { a, .. } | GCmpI { a, .. } => {
                out.push(*a)
            }
            VLoad { addr, .. } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
            }
            VStore { addr, s, .. } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
                out.push(*s);
            }
            VBin { a, b, .. } => out.extend([*a, *b]),
            VFma { a, b, c, .. } => out.extend([*a, *b, *c]),
            VBroadcast { s, .. } | XExtract { s, .. } => out.push(*s),
            VPack { lanes, .. } => out.extend(lanes.iter().copied()),
            CvtSToF { a, .. } | CvtUToF { a, .. } | CvtFToS { a, .. } | CvtFToU { a, .. } => {
                out.push(*a)
            }
            XLoad { addr, .. } | GLoad { addr, .. } | PLoad { addr, .. } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
            }
            XStore { addr, s } | GStore { addr, s } => {
                if let Base::V(b) = addr.base {
                    out.push(b);
                }
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
            CallHelper { ret, .. } => out.extend(ret.iter().copied()),
            StackArg { d, .. }
            | PtrAdd { d, .. }
            | PLoad { d, .. }
            | PMov { d, .. }
            | AtomicRmw { d, .. }
            | AtomicCas { d, .. } => out.push(*d),
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
            | GBinI { d, .. }
            | VLoad { d, .. }
            | VBin { d, .. }
            | VFma { d, .. }
            | VBroadcast { d, .. }
            | VPack { d, .. }
            | XExtract { d, .. }
            | GShiftI { d, .. }
            | GCmpI { d, .. }
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

    pub fn is_call(&self) -> bool {
        matches!(self, MInst::Call { .. } | MInst::CallHelper { .. })
    }

    /// Has side effects beyond its result (kept by DCE).
    pub fn has_effects(&self) -> bool {
        matches!(
            self,
            MInst::Call { .. } | MInst::CallHelper { .. } | MInst::AtomicRmw { .. } | MInst::AtomicCas { .. }
        )
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

/// A pointer value: base plus constant byte offset.
#[derive(Clone, Copy, Debug)]
struct Ptr {
    base: Base,
    off: i32,
}

/// An ABI argument of the entry point (SysV, the loader's signatures).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Arg {
    // FsMain(in_varyings rdi, uniforms rsi, push_constants rdx,
    //        frag_coord xmm0-3, samples_mask ecx, out_color r8,
    //        out_depth r9, front_facing [8], primitive_id [16])
    FsInVaryings,
    FsUniforms,
    FsPush,
    FsFragCoord(u8),
    FsOutColor,
    FsOutDepth,
    FsFrontFacing,
    FsPrimitiveId,
    // VsMain(in_attributes rdi, in_attr_strides rsi, uniforms rdx,
    //        push_constants rcx, vertex_index r8d, instance_index r9d,
    //        out_position [8], out_varyings [16], out_clip_distance [24],
    //        ssbo_table [32])
    VsIn,
    VsUniforms,
    VsPush,
    VsVertexIndex,
    VsInstanceIndex,
    VsOutPosition,
    VsOutVaryings,
    VsSsboTable,
    // CsMain(uniforms rdi (also the image table), push_constants rsi,
    //        out_buffer rdx (SSBO, or SSBO table with >= 2 bindings),
    //        workgroup_id ecx r8d r9d, local_id [8] [16] [24],
    //        workgroup_buf [32])
    CsUniforms,
    CsPush,
    CsOut,
    CsWg(u8),
    CsLid(u8),
    CsWgBuf,
}

enum ArgSrc {
    Reg(PReg),
    /// Offset from the entry rsp.
    Stack(i32),
}

impl Arg {
    fn class(self) -> Class {
        match self {
            Arg::FsFragCoord(_) => Class::X,
            Arg::FsFrontFacing
            | Arg::FsPrimitiveId
            | Arg::VsVertexIndex
            | Arg::VsInstanceIndex
            | Arg::CsWg(_)
            | Arg::CsLid(_) => Class::G,
            _ => Class::P,
        }
    }

    fn src(self) -> ArgSrc {
        use ArgSrc::*;
        match self {
            Arg::FsInVaryings => Reg(PReg::G(gpr::RDI)),
            Arg::FsUniforms => Reg(PReg::G(gpr::RSI)),
            Arg::FsPush => Reg(PReg::G(gpr::RDX)),
            Arg::FsFragCoord(i) => Reg(PReg::X(i)),
            Arg::FsOutColor => Reg(PReg::G(gpr::R8)),
            Arg::FsOutDepth => Reg(PReg::G(gpr::R9)),
            Arg::FsFrontFacing => Stack(8),
            Arg::FsPrimitiveId => Stack(16),
            Arg::VsIn => Reg(PReg::G(gpr::RDI)),
            Arg::VsUniforms => Reg(PReg::G(gpr::RDX)),
            Arg::VsPush => Reg(PReg::G(gpr::RCX)),
            Arg::VsVertexIndex => Reg(PReg::G(gpr::R8)),
            Arg::VsInstanceIndex => Reg(PReg::G(gpr::R9)),
            Arg::VsOutPosition => Stack(8),
            Arg::VsOutVaryings => Stack(16),
            Arg::VsSsboTable => Stack(32),
            Arg::CsUniforms => Reg(PReg::G(gpr::RDI)),
            Arg::CsPush => Reg(PReg::G(gpr::RSI)),
            Arg::CsOut => Reg(PReg::G(gpr::RDX)),
            Arg::CsWg(0) => Reg(PReg::G(gpr::RCX)),
            Arg::CsWg(1) => Reg(PReg::G(gpr::R8)),
            Arg::CsWg(_) => Reg(PReg::G(gpr::R9)),
            Arg::CsLid(i) => Stack(8 + 8 * i as i32),
            Arg::CsWgBuf => Stack(32),
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
    /// Bytes of LocalAlloc storage the frame must provide.
    pub local_bytes: u32,
    /// Bytes of helper-call area (above the locals), 0 without helpers.
    pub helper_bytes: u32,
}

struct Lower<'f> {
    func: &'f Function,
    code: Vec<MInst>,
    classes: Vec<Class>,
    fixed: HashMap<VReg, PReg>,
    hints: HashMap<VReg, VReg>,
    lanes: HashMap<ValueId, Vec<VReg>>,
    /// Values that also have a packed (class V) form, defined where the
    /// value is, so it dominates every use.
    packed: HashMap<ValueId, VReg>,
    /// Mat4 values loaded from memory: their columns, packed.
    packed_mat: HashMap<ValueId, Vec<VReg>>,
    ptrs: HashMap<ValueId, Ptr>,
    args: HashMap<Arg, VReg>,
    /// Precoloured argument vregs, defined by Entry.
    raw_args: Vec<VReg>,
    block_labels: HashMap<BlockId, Label>,
    num_labels: u32,
    /// Copies of register arguments into ordinary vregs, placed right
    /// after the prologue.
    arg_copies: Vec<MInst>,
    /// Stack-passed arguments and other entry loads, after the copies.
    stack_args: Vec<MInst>,
    local_bytes: u32,
    helper_bytes: u32,
    /// Image and sampled-image values: their descriptor (set, binding).
    images: HashMap<ValueId, (u32, u32)>,
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
        // Column-major: lane 4*col + row.
        Type::Mat4(VecElement::F32) => vec![Class::X; 16],
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
        packed: HashMap::new(),
        packed_mat: HashMap::new(),
        ptrs: HashMap::new(),
        args: HashMap::new(),
        raw_args: Vec::new(),
        block_labels: HashMap::new(),
        num_labels: 0,
        arg_copies: Vec::new(),
        stack_args: Vec::new(),
        local_bytes: func.scratch_bytes,
        helper_bytes: 0,
        images: HashMap::new(),
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
    l.code[0] = MInst::Entry { defs: l.raw_args.clone() };
    let mut entry = std::mem::take(&mut l.arg_copies);
    entry.append(&mut l.stack_args);
    l.code.splice(1..1, entry);
    Ok(VCode {
        insts: l.code,
        classes: l.classes,
        fixed: l.fixed,
        hints: l.hints,
        num_labels: l.num_labels,
        local_bytes: l.local_bytes,
        helper_bytes: l.helper_bytes,
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
        let v = self.vreg(a.class());
        match a.src() {
            ArgSrc::Reg(r) => {
                // The precoloured register lives only until this copy,
                // so the value itself is free to move (to a callee-saved
                // register across a call, say). The hint makes the copy
                // vanish when it doesn't need to.
                let raw = self.vreg(a.class());
                self.fixed.insert(raw, r);
                self.raw_args.push(raw);
                self.hints.insert(v, raw);
                self.arg_copies.push(match a.class() {
                    Class::X => MInst::XMov { d: v, s: raw },
                    Class::G => MInst::GMov { d: v, s: raw },
                    Class::P => MInst::PMov { d: v, s: raw },
                    Class::V => unreachable!("no packed ABI arguments"),
                });
            }
            ArgSrc::Stack(off) => {
                self.stack_args.push(MInst::StackArg { d: v, off, wide: a.class() == Class::P });
            }
        }
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
        let p = self.var_ptr(v)?;
        self.ptrs.insert(v.id, p);
        Ok(p)
    }

    /// A bare variable: storage class and the frontend's per-variable
    /// routing pick the ABI argument and offset, as in Cranelift's
    /// `resolve_pointer_param`.
    fn var_ptr(&mut self, v: &Value) -> Result<Ptr, BackendError> {
        let func = self.func;
        let sc = match &v.ty {
            Type::Pointer(sc, _) => *sc,
            other => return unsupported(format!("pointer of type {other:?}")),
        };
        let at = |l: &mut Self, a: Arg, off: u32| -> Result<Ptr, BackendError> {
            let off = i32::try_from(off).map_err(|_| BackendError::Unsupported("offset".into()))?;
            Ok(Ptr { base: Base::V(l.arg(a)), off })
        };
        match func.stage {
            ShaderStage::Fragment => {
                if func.frag_depth_output == Some(v.id) {
                    return at(self, Arg::FsOutDepth, 0);
                }
                match sc {
                    StorageClass::Input => {
                        let off = func.input_varying_byte_offset.get(&v.id).copied().unwrap_or(0);
                        at(self, Arg::FsInVaryings, off)
                    }
                    StorageClass::Output => {
                        let off = func.output_varying_byte_offset.get(&v.id).copied().unwrap_or(0);
                        at(self, Arg::FsOutColor, off)
                    }
                    StorageClass::Uniform => at(self, Arg::FsUniforms, 0),
                    StorageClass::PushConstant => at(self, Arg::FsPush, 0),
                    other => unsupported(format!("fragment storage class {other:?}")),
                }
            }
            ShaderStage::Vertex => match sc {
                StorageClass::Input => {
                    let off = func.input_varying_byte_offset.get(&v.id).copied().unwrap_or(0);
                    at(self, Arg::VsIn, off)
                }
                StorageClass::Output => match func.output_varying_byte_offset.get(&v.id) {
                    Some(&off) => at(self, Arg::VsOutVaryings, off),
                    None => at(self, Arg::VsOutPosition, 0),
                },
                StorageClass::Uniform => at(self, Arg::VsUniforms, 0),
                StorageClass::PushConstant => at(self, Arg::VsPush, 0),
                StorageClass::StorageBuffer => {
                    let Some(&(_set, binding)) = func.ssbo_bindings.get(&v.id.0) else {
                        return unsupported("vertex StorageBuffer without a binding");
                    };
                    // slot[binding] of the descriptor table holds the base.
                    let table = self.arg(Arg::VsSsboTable);
                    let d = self.vreg(Class::P);
                    let off = i32::try_from(binding * 8)
                        .map_err(|_| BackendError::Unsupported("binding".into()))?;
                    // Loaded once at entry, so every use is dominated.
                    self.stack_args.push(MInst::PLoad { d, addr: Addr { base: Base::V(table), off } });
                    Ok(Ptr { base: Base::V(d), off: 0 })
                }
                other => unsupported(format!("vertex storage class {other:?}")),
            },
            ShaderStage::Compute => match sc {
                StorageClass::Uniform => at(self, Arg::CsUniforms, 0),
                StorageClass::PushConstant => at(self, Arg::CsPush, 0),
                StorageClass::StorageBuffer if func.ssbo_bindings.len() >= 2 => {
                    let Some(&(_set, binding)) = func.ssbo_bindings.get(&v.id.0) else {
                        return unsupported("StorageBuffer without a binding");
                    };
                    let table = self.arg(Arg::CsOut);
                    let d = self.vreg(Class::P);
                    let off = i32::try_from(binding * 8)
                        .map_err(|_| BackendError::Unsupported("binding".into()))?;
                    self.stack_args.push(MInst::PLoad { d, addr: Addr { base: Base::V(table), off } });
                    Ok(Ptr { base: Base::V(d), off: 0 })
                }
                StorageClass::StorageBuffer => at(self, Arg::CsOut, 0),
                StorageClass::Workgroup => match func.workgroup_var_offset.get(&v.id) {
                    Some(&off) => at(self, Arg::CsWgBuf, off),
                    None => unsupported("Workgroup variable without an offset"),
                },
                other => unsupported(format!("compute storage class {other:?}")),
            },
        }
    }

    fn load_lanes(&mut self, p: Ptr, d: &[VReg]) {
        for (i, &v) in d.iter().enumerate() {
            let addr = Addr { base: p.base, off: p.off + 4 * i as i32 };
            self.code.push(match self.class_of(v) {
                Class::X => MInst::XLoad { d: v, addr },
                _ => MInst::GLoad { d: v, addr },
            });
        }
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
            return unsupported(format!("{}: lane counts {} {} -> {n}", op_name(&inst.op), la.len(), lb.len()));
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
        let res = Self::result(inst)?;
        // Min/Max stay scalar: matching Cranelift's NaN and ±0 rules takes
        // a compare-and-branch per lane (see the emitter).
        let lanewise_exact = matches!(op, XOp::Add | XOp::Sub | XOp::Mul | XOp::Div);
        if let (Some(n), true) = (packable(&res.ty), lanewise_exact) {
            if self.packed.contains_key(&a.id) || self.packed.contains_key(&b.id) {
                if let (Some(pa), Some(pb)) = (self.as_packed(a, n)?, self.as_packed(b, n)?) {
                    let d = self.vreg(Class::V);
                    self.code.push(MInst::VBin { op, d, a: pa, b: pb });
                    return self.define_packed(res, d, n);
                }
            }
        }
        self.lanewise2(inst, a, b, Class::X, |d, a, b| MInst::XBin { op, d, a, b })
    }

    /// The packed form of `v` with `n` lanes: its own, a broadcast of a
    /// scalar, or packed from its lanes here (not cached: this point need
    /// not dominate later uses). None if `v` isn't f32 lanes.
    fn as_packed(&mut self, v: &Value, n: usize) -> Result<Option<VReg>, BackendError> {
        if let Some(&p) = self.packed.get(&v.id) {
            return Ok(Some(p));
        }
        let l = self.lanes_of(v)?;
        if l.iter().any(|&r| self.class_of(r) != Class::X) {
            return Ok(None);
        }
        let d = self.vreg(Class::V);
        if l.len() == 1 {
            self.code.push(MInst::VBroadcast { d, s: l[0] });
        } else if l.len() == n {
            self.code.push(MInst::VPack { d, lanes: l });
        } else {
            return Ok(None);
        }
        Ok(Some(d))
    }

    /// Define `res` from packed `p`: record it, and extract every lane
    /// right here (DCE drops the extracts nobody reads).
    fn define_packed(&mut self, res: &Value, p: VReg, n: usize) -> Result<(), BackendError> {
        let mut lanes = Vec::with_capacity(n);
        for k in 0..n {
            let d = self.vreg(Class::X);
            if k == 0 {
                self.hints.insert(d, p);
            }
            self.code.push(MInst::XExtract { d, s: p, lane: k as u8 });
            lanes.push(d);
        }
        self.lanes.insert(res.id, lanes);
        self.packed.insert(res.id, p);
        Ok(())
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
        let mv = |d: VReg, s: VReg, c: Class| match c {
            Class::X => MInst::XMov { d, s },
            Class::G => MInst::GMov { d, s },
            Class::P => MInst::PMov { d, s },
            Class::V => unreachable!("packed values never flow through phis"),
        };
        // Sequentialise the parallel copy: emit a move once nothing still
        // pending reads its destination; break a cycle by saving one
        // destination's old value in a temporary.
        let mut pending: Vec<(VReg, VReg)> = moves.into_iter().filter(|(d, s)| d != s).collect();
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .position(|&(d, _)| !pending.iter().any(|&(_, s)| s == d));
            match ready {
                Some(k) => {
                    let (d, s) = pending.remove(k);
                    let c = self.class_of(d);
                    self.hints.entry(d).or_insert(s);
                    self.code.push(mv(d, s, c));
                }
                None => {
                    let d = pending[0].0;
                    let c = self.class_of(d);
                    let t = self.vreg(c);
                    self.hints.insert(t, d);
                    self.code.push(mv(t, d, c));
                    for m in pending.iter_mut() {
                        if m.1 == d {
                            m.1 = t;
                        }
                    }
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
            // A scalar constant, even when the frontend types it as a
            // vector: like Cranelift, it is one value that lane-wise ops
            // broadcast.
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let res = Self::result(inst)?;
                if lane_classes(&res.ty)?.iter().any(|&c| c != Class::X) {
                    return unsupported(format!("ConstFloat of type {:?}", res.ty));
                }
                let d = self.vreg(Class::X);
                self.code.push(M::XConst { d, bits: (*value as f32).to_bits() });
                self.lanes.insert(res.id, vec![d]);
            }
            Op::ConstInt { value, kind: IntKind::I32 | IntKind::U32 } => {
                let res = Self::result(inst)?;
                if lane_classes(&res.ty)?.iter().any(|&c| c != Class::G) {
                    return unsupported(format!("ConstInt of type {:?}", res.ty));
                }
                let d = self.vreg(Class::G);
                self.code.push(M::GConst { d, imm: *value as i32 });
                self.lanes.insert(res.id, vec![d]);
            }
            Op::ConstNull => {
                let res = Self::result(inst)?;
                let d = self.define(res)?;
                for v in d {
                    match self.class_of(v) {
                        Class::X => self.code.push(M::XConst { d: v, bits: 0 }),
                        _ => self.code.push(M::GConst { d: v, imm: 0 }),
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
                if let Some(n) = packable(&res.ty) {
                    if [a, b, c].iter().any(|v| self.packed.contains_key(&v.id)) {
                        if let (Some(pa), Some(pb), Some(pc)) =
                            (self.as_packed(a, n)?, self.as_packed(b, n)?, self.as_packed(c, n)?)
                        {
                            let d = self.vreg(Class::V);
                            self.code.push(M::VFma { d, a: pa, b: pb, c: pc });
                            return self.define_packed(res, d, n);
                        }
                    }
                }
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
            Op::Rbit(a) => self.lanewise1(inst, a, Class::G, |d, a| M::GUn { op: GUnOp::Rbit, d, a })?,

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
                        _ => M::GSelect { d: d[i], c, t: lt[i], f: lf[i] },
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
                        Class::X => M::MovGToX { d, s },
                        _ => M::MovXToG { d, s },
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
            Op::LoadBuiltin(kind) => {
                let res = Self::result(inst)?;
                let stage = self.func.stage;
                let regs: Vec<VReg> = match (stage, kind) {
                    (ShaderStage::Fragment, BuiltinKind::FragCoord) => {
                        let n = lane_classes(&res.ty)?.len().min(4);
                        (0..n as u8).map(|i| self.arg(Arg::FsFragCoord(i))).collect()
                    }
                    (ShaderStage::Fragment, BuiltinKind::FrontFacing) => vec![self.arg(Arg::FsFrontFacing)],
                    (ShaderStage::Fragment, BuiltinKind::PrimitiveId) => vec![self.arg(Arg::FsPrimitiveId)],
                    (ShaderStage::Vertex, BuiltinKind::VertexIndex) => vec![self.arg(Arg::VsVertexIndex)],
                    (ShaderStage::Vertex, BuiltinKind::InstanceIndex) => vec![self.arg(Arg::VsInstanceIndex)],
                    (ShaderStage::Vertex, BuiltinKind::BaseVertex | BuiltinKind::BaseInstance) => {
                        // 0 for direct draws; the index args are absolute.
                        let d = self.vreg(Class::G);
                        self.code.push(M::GConst { d, imm: 0 });
                        vec![d]
                    }
                    (ShaderStage::Compute, BuiltinKind::WorkgroupId) => {
                        (0..3).map(|i| self.arg(Arg::CsWg(i))).collect()
                    }
                    (ShaderStage::Compute, BuiltinKind::LocalInvocationId) => {
                        (0..3).map(|i| self.arg(Arg::CsLid(i))).collect()
                    }
                    (ShaderStage::Compute, BuiltinKind::GlobalInvocationId) => {
                        // WorkgroupId * LocalSize + LocalInvocationId.
                        let ls = self.func.local_size.unwrap_or((1, 1, 1));
                        let ls = [ls.0, ls.1, ls.2];
                        let mut out = Vec::new();
                        for i in 0..3 {
                            let (wg, lid) = (self.arg(Arg::CsWg(i as u8)), self.arg(Arg::CsLid(i as u8)));
                            let scaled = if ls[i] == 1 {
                                wg
                            } else {
                                let c = self.gconst(ls[i] as i32);
                                let m = self.vreg(Class::G);
                                self.code.push(M::GBin { op: GOp::Mul, d: m, a: wg, b: c });
                                m
                            };
                            let d = self.vreg(Class::G);
                            self.code.push(M::GBin { op: GOp::Add, d, a: scaled, b: lid });
                            out.push(d);
                        }
                        out
                    }
                    (ShaderStage::Compute, BuiltinKind::WorkgroupSize) => {
                        let ls = self.func.local_size.unwrap_or((1, 1, 1));
                        [ls.0, ls.1, ls.2].iter().map(|&v| self.gconst(v as i32)).collect()
                    }
                    (ShaderStage::Compute, BuiltinKind::LocalInvocationIndex) => {
                        // lz * (sx * sy) + ly * sx + lx
                        let ls = self.func.local_size.unwrap_or((1, 1, 1));
                        let (lx, ly, lz) = (self.arg(Arg::CsLid(0)), self.arg(Arg::CsLid(1)), self.arg(Arg::CsLid(2)));
                        let mut terms = vec![lx];
                        for (l, scale) in [(ly, ls.0), (lz, ls.0.saturating_mul(ls.1))] {
                            let c = self.gconst(scale as i32);
                            let m = self.vreg(Class::G);
                            self.code.push(M::GBin { op: GOp::Mul, d: m, a: l, b: c });
                            terms.push(m);
                        }
                        let mut acc = terms[0];
                        for &t in &terms[1..] {
                            let d = self.vreg(Class::G);
                            self.code.push(M::GBin { op: GOp::Add, d, a: acc, b: t });
                            acc = d;
                        }
                        vec![acc]
                    }
                    (s, k) => return unsupported(format!("LoadBuiltin({k:?}) in {s:?}")),
                };
                self.lanes.insert(res.id, regs);
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
            Op::PtrOffsetDynamic { base, index, stride } => {
                let res = Self::result(inst)?;
                let p = self.ptr_of(base)?;
                let idx = self.lanes_of(index)?;
                if idx.len() != 1 || self.class_of(idx[0]) != Class::G {
                    return unsupported("PtrOffsetDynamic index type");
                }
                let d = self.vreg(Class::P);
                self.code.push(M::PtrAdd { d, base: p.base, idx: idx[0], stride: *stride });
                self.ptrs.insert(res.id, Ptr { base: Base::V(d), off: p.off });
            }
            Op::LocalAlloc { offset, bytes } => {
                let res = Self::result(inst)?;
                let off = i32::try_from(*offset).map_err(|_| BackendError::Unsupported("LocalAlloc".into()))?;
                self.local_bytes = self.local_bytes.max(offset.saturating_add(*bytes));
                self.ptrs.insert(res.id, Ptr { base: Base::Locals, off });
            }
            Op::Load(ptr) => {
                let res = Self::result(inst)?;
                let p = self.ptr_of(ptr)?;
                let d = self.define(res)?;
                self.load_lanes(p, &d);
                // Also the packed load; DCE keeps whichever form is used.
                if matches!(res.ty, Type::Mat4(VecElement::F32)) {
                    let c: Vec<VReg> = (0..4)
                        .map(|j| {
                            let v = self.vreg(Class::V);
                            self.code.push(M::VLoad { d: v, addr: Addr { base: p.base, off: p.off + 16 * j }, n: 4 });
                            v
                        })
                        .collect();
                    self.packed_mat.insert(res.id, c);
                }
                if let Some(n) = packable(&res.ty) {
                    let v = self.vreg(Class::V);
                    self.code.push(M::VLoad { d: v, addr: Addr { base: p.base, off: p.off }, n: n as u8 });
                    self.packed.insert(res.id, v);
                }
            }
            Op::Store { ptr, value } => {
                let p = self.ptr_of(ptr)?;
                if let (Some(n), Some(&v)) = (packable(&value.ty), self.packed.get(&value.id)) {
                    self.code.push(M::VStore { addr: Addr { base: p.base, off: p.off }, s: v, n: n as u8 });
                    return Ok(());
                }
                for (i, v) in self.lanes_of(value)?.into_iter().enumerate() {
                    let addr = Addr { base: p.base, off: p.off + 4 * i as i32 };
                    self.code.push(match self.class_of(v) {
                        Class::X => M::XStore { addr, s: v },
                        _ => M::GStore { addr, s: v },
                    });
                }
            }
            Op::MatrixTimesVector { matrix, vector } => {
                // out[i] = sum_j m[j][i] * v[j], multiply then add (no FMA),
                // in Cranelift's order, so results match bit for bit.
                let res = Self::result(inst)?;
                let cols: Vec<VReg> = match &matrix.ty {
                    Type::Pointer(..) => {
                        let p = self.ptr_of(matrix)?;
                        let d: Vec<VReg> = (0..16).map(|_| self.vreg(Class::X)).collect();
                        self.load_lanes(p, &d);
                        d
                    }
                    _ => self.lanes_of(matrix)?,
                };
                let v = self.lanes_of(vector)?;
                if cols.len() != 16 || v.len() != 4 || lane_classes(&res.ty)?.len() != 4 {
                    return unsupported("MatrixTimesVector shape");
                }
                // Packed: acc = c0 * v0; acc += c_j * v_j, multiply then add,
                // in Cranelift's order per lane.
                {
                    let pcols: Vec<VReg> = match &matrix.ty {
                        Type::Pointer(..) => {
                            let p = self.ptr_of(matrix)?;
                            (0..4)
                                .map(|j| {
                                    let d = self.vreg(Class::V);
                                    let addr = Addr { base: p.base, off: p.off + 16 * j };
                                    self.code.push(M::VLoad { d, addr, n: 4 });
                                    d
                                })
                                .collect()
                        }
                        _ => match self.packed_mat.get(&matrix.id) {
                            Some(c) => c.clone(),
                            None => {
                                // Only lanes: the scalar form, no packing.
                                let mut out = Vec::new();
                                for i in 0..4 {
                                    let mut acc = self.vreg(Class::X);
                                    self.code.push(M::XBin { op: XOp::Mul, d: acc, a: cols[i], b: v[0] });
                                    for j in 1..4 {
                                        let p = self.vreg(Class::X);
                                        self.code.push(M::XBin { op: XOp::Mul, d: p, a: cols[4 * j + i], b: v[j] });
                                        let s = self.vreg(Class::X);
                                        self.code.push(M::XBin { op: XOp::Add, d: s, a: acc, b: p });
                                        acc = s;
                                    }
                                    out.push(acc);
                                }
                                self.lanes.insert(res.id, out);
                                return Ok(());
                            }
                        },
                    };
                    let mut acc = None;
                    for j in 0..4 {
                        let b = self.vreg(Class::V);
                        self.code.push(M::VBroadcast { d: b, s: v[j] });
                        let p = self.vreg(Class::V);
                        self.code.push(M::VBin { op: XOp::Mul, d: p, a: pcols[j], b });
                        acc = Some(match acc {
                            None => p,
                            Some(a) => {
                                let s = self.vreg(Class::V);
                                self.code.push(M::VBin { op: XOp::Add, d: s, a, b: p });
                                s
                            }
                        });
                    }
                    return self.define_packed(res, acc.unwrap(), 4);
                }
            }

            // ── compute: atomics and barriers ─────────────────────────
            Op::Barrier => {
                if self.func.stage != ShaderStage::Compute {
                    return unsupported("Barrier outside compute");
                }
                // The image table (the uniforms argument) holds the
                // runtime's barrier function at IMG_TABLE_BARRIER_OFFSET.
                let table = self.arg(Arg::CsUniforms);
                self.code.push(M::Call { table, off: IMG_TABLE_BARRIER_OFFSET });
            }
            Op::AtomicLoad(ptr) => {
                let res = Self::result(inst)?;
                let p = self.ptr_of(ptr)?;
                let d = self.define(res)?;
                if d.len() != 1 || self.class_of(d[0]) != Class::G {
                    return unsupported("AtomicLoad type");
                }
                self.load_lanes(p, &d);
            }
            Op::AtomicStore { ptr, value } => {
                let p = self.ptr_of(ptr)?;
                let s = self.lanes_of(value)?;
                if s.len() != 1 || self.class_of(s[0]) != Class::G {
                    return unsupported("AtomicStore type");
                }
                self.code.push(M::AtomicStore { addr: Addr { base: p.base, off: p.off }, s: s[0] });
            }
            Op::AtomicIAdd { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::Add)?,
            Op::AtomicAnd { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::And)?,
            Op::AtomicOr { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::Or)?,
            Op::AtomicXor { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::Xor)?,
            Op::AtomicSMin { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::SMin)?,
            Op::AtomicSMax { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::SMax)?,
            Op::AtomicUMin { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::UMin)?,
            Op::AtomicUMax { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::UMax)?,
            Op::AtomicExchange { ptr, value } => self.atomic(inst, ptr, value, AtomicOp::Xchg)?,
            Op::AtomicCompareExchange { ptr, expected, desired } => {
                let res = Self::result(inst)?;
                let p = self.ptr_of(ptr)?;
                let (e, w) = (self.lanes_of(expected)?, self.lanes_of(desired)?);
                let d = self.define(res)?;
                if [&e, &w, &d].iter().any(|l| l.len() != 1 || self.class_of(l[0]) != Class::G) {
                    return unsupported("AtomicCompareExchange type");
                }
                self.code.push(M::AtomicCas {
                    d: d[0],
                    addr: Addr { base: p.base, off: p.off },
                    expected: e[0],
                    desired: w[0],
                });
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

            Op::ImageHandle { .. }
            | Op::CombineSampledImage { .. }
            | Op::Derivative { .. }
            | Op::ImageSampleImplicitLod { .. }
            | Op::ImageSampleExplicitLod { .. }
            | Op::ImageSampleDref { .. }
            | Op::ImageGather { .. }
            | Op::ImageFetch { .. }
            | Op::SampledImageQuerySizeLod { .. }
            | Op::ImageQuerySize(_)
            | Op::ImageTexelPointer { .. }
            | Op::ImageRead { .. }
            | Op::ImageWrite { .. }
            | Op::ImageReadLod { .. }
            | Op::ImageWriteLod { .. } => self.image_op(inst)?,

            Op::PackHalf2x16(v) => {
                let res = Self::result(inst)?;
                let l = self.lanes_class(v, Class::X, 2, "PackHalf2x16")?;
                let d = self.define(res)?;
                if d.len() != 1 || self.class_of(d[0]) != Class::G {
                    return unsupported("PackHalf2x16 result");
                }
                let lo = self.f32_to_f16(l[0]);
                let hi = self.f32_to_f16(l[1]);
                let sixteen = self.gconst(16);
                let hi = self.g2(|d| M::GShift { op: ShiftOp::Shl, d, a: hi, b: sixteen });
                self.code.push(M::GBin { op: GOp::Or, d: d[0], a: lo, b: hi });
            }
            Op::UnpackHalf2x16(v) => {
                let res = Self::result(inst)?;
                let w = self.lanes_class(v, Class::G, 1, "UnpackHalf2x16")?;
                let d = self.define(res)?;
                if d.len() != 2 || d.iter().any(|&r| self.class_of(r) != Class::X) {
                    return unsupported("UnpackHalf2x16 result");
                }
                let mask = self.gconst(0xffff);
                let lo = self.g2(|d| M::GBin { op: GOp::And, d, a: w[0], b: mask });
                let sixteen = self.gconst(16);
                let hi = self.g2(|d| M::GShift { op: ShiftOp::LShr, d, a: w[0], b: sixteen });
                self.f16_to_f32(lo, d[0]);
                self.f16_to_f32(hi, d[1]);
            }

            other => return unsupported(op_name(other)),
        }
        Ok(())
    }

    /// A fresh G vreg defined by `f(d)`.
    fn g2(&mut self, f: impl FnOnce(VReg) -> MInst) -> VReg {
        let d = self.vreg(Class::G);
        self.code.push(f(d));
        d
    }

    fn gbin_imm(&mut self, op: GOp, a: VReg, imm: i32) -> VReg {
        let b = self.gconst(imm);
        self.g2(|d| MInst::GBin { op, d, a, b })
    }

    fn gshift_imm(&mut self, op: ShiftOp, a: VReg, imm: i32) -> VReg {
        let b = self.gconst(imm);
        self.g2(|d| MInst::GShift { op, d, a, b })
    }

    fn gcmp_imm(&mut self, cc: ICond, a: VReg, imm: i32) -> VReg {
        let b = self.gconst(imm);
        self.g2(|d| MInst::GCmp { cc, d, a, b })
    }

    fn gsel(&mut self, c: VReg, t: VReg, f: VReg) -> VReg {
        self.g2(|d| MInst::GSelect { d, c, t, f })
    }

    /// f32 → f16 bits, round to nearest even, NaN → 0x7e00: Cranelift's
    /// `f32_to_f16_bits`, step for step, so both backends agree on every
    /// input (F16C would keep NaN payloads).
    fn f32_to_f16(&mut self, x: VReg) -> VReg {
        use MInst as M;
        let f = self.g2(|d| M::MovXToG { d, s: x });
        let sign = self.gshift_imm(ShiftOp::LShr, f, 16);
        let sign = self.gbin_imm(GOp::And, sign, 0x8000);
        let a = self.gbin_imm(GOp::And, f, 0x7fff_ffff);
        let is_nan = self.gcmp_imm(ICond::Ugt, a, 0x7f80_0000);
        let qnan = self.gconst(0x7e00);
        let inf = self.gconst(0x7c00);
        let big = self.gsel(is_nan, qnan, inf);
        // Subnormal results: let the float adder round.
        let af = self.vreg(Class::X);
        self.code.push(M::MovGToX { d: af, s: a });
        let half = self.vreg(Class::X);
        self.code.push(M::XConst { d: half, bits: 0.5f32.to_bits() });
        let sum = self.vreg(Class::X);
        self.code.push(M::XBin { op: XOp::Add, d: sum, a: af, b: half });
        let sum = self.g2(|d| M::MovXToG { d, s: sum });
        let den = self.gbin_imm(GOp::Add, sum, -0x3f00_0000);
        // Normal results: rebias and round to nearest even.
        let odd = self.gshift_imm(ShiftOp::LShr, a, 13);
        let odd = self.gbin_imm(GOp::And, odd, 1);
        let n = self.gbin_imm(GOp::Add, a, 0xc800_0fff_u32 as i32);
        let n = self.g2(|d| M::GBin { op: GOp::Add, d, a: n, b: odd });
        let n = self.gshift_imm(ShiftOp::LShr, n, 13);
        let is_big = self.gcmp_imm(ICond::Uge, a, 0x4780_0000);
        let is_den = self.gcmp_imm(ICond::Ult, a, 0x3880_0000);
        let r = self.gsel(is_den, den, n);
        let r = self.gsel(is_big, big, r);
        self.g2(|d| M::GBin { op: GOp::Or, d, a: r, b: sign })
    }

    /// f16 bits (low 16 of `h`) → f32 into `out`: Cranelift's
    /// `f16_bits_to_f32`, step for step.
    fn f16_to_f32(&mut self, h: VReg, out: VReg) {
        use MInst as M;
        let m = self.gbin_imm(GOp::And, h, 0x7fff);
        let o = self.gshift_imm(ShiftOp::Shl, m, 13);
        let exp = self.gbin_imm(GOp::And, o, 0x0f80_0000);
        let o = self.gbin_imm(GOp::Add, o, (127 - 15) << 23);
        let o_inf = self.gbin_imm(GOp::Add, o, (128 - 16) << 23);
        let o_den = self.gbin_imm(GOp::Add, o, 1 << 23);
        let o_den_f = self.vreg(Class::X);
        self.code.push(M::MovGToX { d: o_den_f, s: o_den });
        let magic = self.vreg(Class::X);
        self.code.push(M::XConst { d: magic, bits: 113 << 23 });
        let diff = self.vreg(Class::X);
        self.code.push(M::XBin { op: XOp::Sub, d: diff, a: o_den_f, b: magic });
        let o_den = self.g2(|d| M::MovXToG { d, s: diff });
        let is_inf = self.gcmp_imm(ICond::Eq, exp, 0x0f80_0000);
        let is_den = self.gcmp_imm(ICond::Eq, exp, 0);
        let r = self.gsel(is_den, o_den, o);
        let r = self.gsel(is_inf, o_inf, r);
        let sign = self.gbin_imm(GOp::And, h, 0x8000);
        let sign = self.gshift_imm(ShiftOp::Shl, sign, 16);
        let r = self.g2(|d| M::GBin { op: GOp::Or, d, a: r, b: sign });
        self.code.push(M::MovGToX { d: out, s: r });
    }

    // ── images and derivatives (phase 7) ──────────────────────────────
    //
    // Everything here mirrors Cranelift's lowering: the
    // same table slots, argument orders and out-buffer layout, so both
    // backends call the runtime identically.

    fn require_stage(&self, s: ShaderStage, what: &str) -> Result<(), BackendError> {
        if self.func.stage != s {
            return unsupported(format!("{what} in {:?}", self.func.stage));
        }
        Ok(())
    }

    /// Descriptor binding of an image or sampled-image value.
    fn binding_of(&self, v: &Value) -> Result<i32, BackendError> {
        let (_, b) = self
            .images
            .get(&v.id)
            .copied()
            .ok_or_else(|| BackendError::Unsupported(format!("image value {:?} is not a descriptor", v.id)))?;
        i32::try_from(b).ok().filter(|&b| b < 1 << 20).ok_or_else(|| BackendError::Unsupported("binding".into()))
    }

    /// At least `n` lanes of `v`, all of class `c`.
    fn lanes_class(&self, v: &Value, c: Class, n: usize, what: &str) -> Result<Vec<VReg>, BackendError> {
        let l = self.lanes_of(v)?;
        if l.len() < n || l.iter().any(|&r| self.class_of(r) != c) {
            return unsupported(format!("{what}: operand shape"));
        }
        Ok(l)
    }

    fn call_helper(&mut self, table: VReg, fn_off: i32, args: Vec<CArg>, ret: Option<VReg>) {
        self.helper_bytes = self.helper_bytes.max(helper_area_bytes(args.len()));
        self.code.push(MInst::CallHelper { table, fn_off, args, ret });
    }

    /// Result lanes read back from the helper out buffer.
    fn out_buf_result(&mut self, inst: &Inst, n: usize) -> Result<(), BackendError> {
        let res = Self::result(inst)?;
        let d = self.define(res)?;
        if d.len() != n {
            return unsupported(format!("{} result shape", op_name(&inst.op)));
        }
        self.load_lanes(Ptr { base: Base::Helper, off: 0 }, &d);
        Ok(())
    }

    fn image_op(&mut self, inst: &Inst) -> Result<(), BackendError> {
        use MInst as M;
        let name = op_name(&inst.op);
        // Fragment descriptors: the uniforms argument, helpers at fixed
        // slots, binding b's (tex, samp) pointers at 80 + 16b.
        let frag_desc = |b: i32| 80 + 16 * b;
        // Compute storage images: ImageDesc* at IMG_TABLE_DESC_BASE + 8b.
        let comp_desc = |b: i32| IMG_TABLE_DESC_BASE + 8 * b;
        match &inst.op {
            Op::ImageHandle { set, binding } => {
                let res = Self::result(inst)?;
                self.images.insert(res.id, (*set, *binding));
            }
            Op::CombineSampledImage { image, .. } => {
                let res = Self::result(inst)?;
                let h = self
                    .images
                    .get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Unsupported("CombineSampledImage image operand".into()))?;
                self.images.insert(res.id, h);
            }
            Op::Derivative { value, site, axis } => {
                self.require_stage(ShaderStage::Fragment, &name)?;
                let v = self.lanes_class(value, Class::X, 1, &name)?;
                if v.len() != 1 {
                    return unsupported("Derivative of a vector");
                }
                let site = i32::try_from(*site).map_err(|_| BackendError::Unsupported("Derivative site".into()))?;
                let res = Self::result(inst)?;
                let d = self.define(res)?;
                if d.len() != 1 || self.class_of(d[0]) != Class::X {
                    return unsupported("Derivative result");
                }
                let table = self.arg(Arg::FsUniforms);
                self.call_helper(table, 72, vec![CArg::V(v[0]), CArg::Imm(site), CArg::Imm(*axis as i32)], Some(d[0]));
            }
            Op::ImageSampleImplicitLod { sampled_image, coord }
            | Op::ImageSampleExplicitLod { sampled_image, coord, .. } => {
                self.require_stage(ShaderStage::Fragment, &name)?;
                let b = self.binding_of(sampled_image)?;
                let c = self.lanes_class(coord, Class::X, 2, &name)?;
                let is_cube = matches!(sampled_image.ty, Type::SampledImage(ImageDimensionality::Cube));
                let is_array = !is_cube && c.len() >= 3;
                let lod = match &inst.op {
                    Op::ImageSampleExplicitLod { lod, .. } => Some(self.lanes_class(lod, Class::X, 1, &name)?[0]),
                    _ => None,
                };
                let fn_off = match (is_cube, is_array, lod.is_some()) {
                    (true, _, true) => 56,
                    (_, true, true) => 48,
                    (true, _, false) => 32,
                    (_, true, false) => 24,
                    (_, _, true) => 16,
                    _ => 0,
                };
                let mut args = vec![
                    CArg::TableLoad(frag_desc(b)),
                    CArg::TableLoad(frag_desc(b) + 8),
                    CArg::V(c[0]),
                    CArg::V(c[1]),
                ];
                if is_cube || is_array {
                    args.push(CArg::V(c[2]));
                }
                // 2D + lod: the lod takes the third slot.
                args.extend(lod.map(CArg::V));
                args.push(CArg::OutBuf);
                let table = self.arg(Arg::FsUniforms);
                self.call_helper(table, fn_off, args, None);
                self.out_buf_result(inst, 4)?;
            }
            Op::ImageSampleDref { sampled_image, coord, dref } => {
                self.require_stage(ShaderStage::Fragment, &name)?;
                let b = self.binding_of(sampled_image)?;
                let c = self.lanes_class(coord, Class::X, 2, &name)?;
                let r = self.lanes_class(dref, Class::X, 1, &name)?;
                let args = vec![
                    CArg::TableLoad(frag_desc(b)),
                    CArg::TableLoad(frag_desc(b) + 8),
                    CArg::V(c[0]),
                    CArg::V(c[1]),
                    CArg::V(r[0]),
                    CArg::OutBuf,
                ];
                let table = self.arg(Arg::FsUniforms);
                self.call_helper(table, 64, args, None);
                self.out_buf_result(inst, 1)?;
            }
            Op::ImageGather { sampled_image, coord, component } => {
                self.require_stage(ShaderStage::Fragment, &name)?;
                let b = self.binding_of(sampled_image)?;
                let c = self.lanes_class(coord, Class::X, 2, &name)?;
                let k = self.lanes_class(component, Class::G, 1, &name)?;
                let args = vec![
                    CArg::TableLoad(frag_desc(b)),
                    CArg::TableLoad(frag_desc(b) + 8),
                    CArg::V(c[0]),
                    CArg::V(c[1]),
                    CArg::V(k[0]),
                    CArg::OutBuf,
                ];
                let table = self.arg(Arg::FsUniforms);
                self.call_helper(table, 40, args, None);
                self.out_buf_result(inst, 4)?;
            }
            Op::ImageFetch { image, coord, lod } => {
                self.require_stage(ShaderStage::Fragment, &name)?;
                let b = self.binding_of(image)?;
                let c = self.lanes_class(coord, Class::G, 2, &name)?;
                let lod = match lod {
                    Some(l) => CArg::V(self.lanes_class(l, Class::G, 1, &name)?[0]),
                    None => CArg::Imm(0),
                };
                let args = vec![CArg::TableLoad(frag_desc(b)), CArg::V(c[0]), CArg::V(c[1]), lod, CArg::OutBuf];
                let table = self.arg(Arg::FsUniforms);
                self.call_helper(table, 8, args, None);
                self.out_buf_result(inst, 4)?;
            }
            Op::SampledImageQuerySizeLod { image, .. } => {
                self.require_stage(ShaderStage::Fragment, &name)?;
                let b = self.binding_of(image)?;
                let res = Self::result(inst)?;
                // Cranelift fills only width and height; a vec3 result
                // would leave a lane undefined there.
                if !matches!(res.ty, Type::Vec2(_)) {
                    return unsupported("SampledImageQuerySizeLod result type");
                }
                let d = self.define(res)?;
                let table = self.arg(Arg::FsUniforms);
                let desc = self.vreg(Class::P);
                self.code.push(M::PLoad { d: desc, addr: Addr { base: Base::V(table), off: frag_desc(b) } });
                for (i, &v) in d.iter().enumerate() {
                    self.code.push(M::GLoad { d: v, addr: Addr { base: Base::V(desc), off: 8 + 4 * i as i32 } });
                }
            }
            Op::ImageQuerySize(image) => {
                self.require_stage(ShaderStage::Compute, &name)?;
                let b = self.binding_of(image)?;
                let res = Self::result(inst)?;
                if !matches!(res.ty, Type::Vec2(_) | Type::Vec3(_)) {
                    return unsupported("ImageQuerySize result type");
                }
                let d = self.define(res)?;
                let table = self.arg(Arg::CsUniforms);
                let desc = self.vreg(Class::P);
                self.code.push(M::PLoad { d: desc, addr: Addr { base: Base::V(table), off: comp_desc(b) } });
                for (&v, off) in d.iter().zip([8, 12, 24]) {
                    self.code.push(M::GLoad { d: v, addr: Addr { base: Base::V(desc), off } });
                }
            }
            Op::ImageTexelPointer { image, coord } => {
                self.require_stage(ShaderStage::Compute, &name)?;
                let b = self.binding_of(image)?;
                let c = self.lanes_class(coord, Class::G, 2, &name)?;
                let res = Self::result(inst)?;
                let table = self.arg(Arg::CsUniforms);
                let desc = self.vreg(Class::P);
                self.code.push(M::PLoad { d: desc, addr: Addr { base: Base::V(table), off: comp_desc(b) } });
                let mut addr = self.vreg(Class::P);
                self.code.push(M::PLoad { d: addr, addr: Addr { base: Base::V(desc), off: 0 } });
                // data + z * slice_bytes (@28) + y * stride_bytes (@16) + 4x,
                // the products in 32 bits as Cranelift does.
                let mut add = |l: &mut Self, idx: VReg, field: Option<i32>| {
                    let idx = match field {
                        Some(off) => {
                            let f = l.vreg(Class::G);
                            l.code.push(M::GLoad { d: f, addr: Addr { base: Base::V(desc), off } });
                            let p = l.vreg(Class::G);
                            l.code.push(M::GBin { op: GOp::Mul, d: p, a: idx, b: f });
                            p
                        }
                        None => idx,
                    };
                    let d = l.vreg(Class::P);
                    l.code.push(M::PtrAdd { d, base: Base::V(addr), idx, stride: if field.is_some() { 1 } else { 4 } });
                    addr = d;
                };
                if c.len() >= 3 {
                    add(self, c[2], Some(28));
                }
                add(self, c[1], Some(16));
                add(self, c[0], None);
                self.ptrs.insert(res.id, Ptr { base: Base::V(addr), off: 0 });
            }
            Op::ImageRead { image, coord }
            | Op::ImageWrite { image, coord, .. }
            | Op::ImageReadLod { image, coord, .. }
            | Op::ImageWriteLod { image, coord, .. } => {
                self.require_stage(ShaderStage::Compute, &name)?;
                let b = self.binding_of(image)?;
                let c = self.lanes_class(coord, Class::G, 2, &name)?;
                let is_3d = c.len() >= 3;
                let (texel, lod) = match &inst.op {
                    Op::ImageWrite { texel, .. } => (Some(texel), None),
                    Op::ImageWriteLod { texel, lod, .. } => (Some(texel), Some(lod)),
                    Op::ImageReadLod { lod, .. } => (None, Some(lod)),
                    _ => (None, None),
                };
                let fn_off = if lod.is_some() { 32 } else { 0 }
                    + match (is_3d, texel.is_some()) {
                        (false, false) => 0,
                        (false, true) => 8,
                        (true, false) => 16,
                        (true, true) => 24,
                    };
                let mut args = vec![CArg::TableLoad(comp_desc(b)), CArg::V(c[0]), CArg::V(c[1])];
                if is_3d {
                    args.push(CArg::V(c[2]));
                }
                if let Some(l) = lod {
                    args.push(CArg::V(self.lanes_class(l, Class::G, 1, &name)?[0]));
                }
                args.push(CArg::OutBuf);
                if let Some(t) = texel {
                    let t = self.lanes_of(t)?;
                    if t.len() < 4 {
                        return unsupported("ImageWrite texel must be a vec4");
                    }
                    for (i, &s) in t.iter().take(4).enumerate() {
                        let addr = Addr { base: Base::Helper, off: 4 * i as i32 };
                        self.code.push(match self.class_of(s) {
                            Class::X => M::XStore { addr, s },
                            _ => M::GStore { addr, s },
                        });
                    }
                }
                let table = self.arg(Arg::CsUniforms);
                self.call_helper(table, fn_off, args, None);
                if texel.is_none() {
                    self.out_buf_result(inst, 4)?;
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn gconst(&mut self, imm: i32) -> VReg {
        let d = self.vreg(Class::G);
        self.code.push(MInst::GConst { d, imm });
        d
    }

    fn atomic(&mut self, inst: &Inst, ptr: &Value, value: &Value, op: AtomicOp) -> Result<(), BackendError> {
        let res = Self::result(inst)?;
        let p = self.ptr_of(ptr)?;
        let v = self.lanes_of(value)?;
        let d = self.define(res)?;
        if v.len() != 1 || d.len() != 1 || self.class_of(v[0]) != Class::G || self.class_of(d[0]) != Class::G {
            return unsupported(format!("{} type", op_name(&inst.op)));
        }
        self.code.push(MInst::AtomicRmw { op, d: d[0], addr: Addr { base: p.base, off: p.off }, v: v[0] });
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
            defs.is_empty() || inst.has_effects() || defs.iter().any(|v| used[v.0 as usize])
        });
        if vc.insts.len() == before {
            break;
        }
    }
}

/// Move each compare whose only use is a later Br or GSelect in the same
/// block to just before that use, so the emitter can fuse the two (flags
/// straight into jcc/cmovcc). Safe because a block body never redefines a
/// vreg: phi copies sit at the very end of a block, after its last use.
pub(crate) fn sink_compares(vc: &mut VCode) {
    let mut use_count = vec![0u32; vc.classes.len()];
    let mut tmp = Vec::new();
    for inst in &vc.insts {
        tmp.clear();
        inst.uses(&mut tmp);
        for v in &tmp {
            use_count[v.0 as usize] += 1;
        }
    }
    let mut i = 0;
    while i < vc.insts.len() {
        let d = match &vc.insts[i] {
            MInst::GCmp { d, .. } | MInst::GCmpI { d, .. } | MInst::FCmp { d, .. }
                if use_count[d.0 as usize] == 1 =>
            {
                *d
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let mut j = i + 1;
        let mut target = None;
        while j < vc.insts.len() {
            match &vc.insts[j] {
                MInst::Br { c, .. } | MInst::GSelect { c, .. } if *c == d => {
                    target = Some(j);
                    break;
                }
                MInst::Label(_) | MInst::Jmp(_) | MInst::Br { .. } | MInst::CaseEq { .. } | MInst::Ret => break,
                _ => j += 1,
            }
        }
        if let Some(j) = target {
            if j > i + 1 {
                let c = vc.insts.remove(i);
                vc.insts.insert(j - 1, c);
                continue; // re-examine the instruction now at i
            }
        }
        i += 1;
    }
}

impl ICond {
    /// The condition with the operands swapped (a < b  ==  b > a).
    fn swapped(self) -> ICond {
        use ICond::*;
        match self {
            Eq => Eq,
            Ne => Ne,
            Ult => Ugt,
            Ugt => Ult,
            Ule => Uge,
            Uge => Ule,
            Slt => Sgt,
            Sgt => Slt,
            Sle => Sge,
            Sge => Sle,
        }
    }
}

/// Turn integer ops with a constant operand into their immediate forms;
/// the constants themselves then usually die in DCE.
pub(crate) fn fold_immediates(vc: &mut VCode) {
    let mut defs = vec![0u32; vc.classes.len()];
    let mut consts: HashMap<VReg, i32> = HashMap::new();
    let mut tmp = Vec::new();
    for inst in &vc.insts {
        tmp.clear();
        inst.defs(&mut tmp);
        for v in &tmp {
            defs[v.0 as usize] += 1;
        }
        if let MInst::GConst { d, imm } = inst {
            consts.insert(*d, *imm);
        }
    }
    consts.retain(|v, _| defs[v.0 as usize] == 1);
    let k = |v: &VReg| consts.get(v).copied();
    for inst in vc.insts.iter_mut() {
        let new = match *inst {
            MInst::GBin { op, d, a, b } => match (k(&a), k(&b)) {
                (_, Some(imm)) => Some(MInst::GBinI { op, d, a, imm }),
                (Some(imm), None) if op.commutative() => Some(MInst::GBinI { op, d, a: b, imm }),
                _ => None,
            },
            MInst::GShift { op, d, a, b } => k(&b).map(|imm| MInst::GShiftI { op, d, a, imm: (imm & 31) as u8 }),
            MInst::GCmp { cc, d, a, b } => match (k(&a), k(&b)) {
                (_, Some(imm)) => Some(MInst::GCmpI { cc, d, a, imm }),
                (Some(imm), None) => Some(MInst::GCmpI { cc: cc.swapped(), d, a: b, imm }),
                _ => None,
            },
            _ => None,
        };
        if let Some(n) = new {
            *inst = n;
        }
    }
}

/// Lane count of an f32 vector type worth packing.
fn packable(ty: &Type) -> Option<usize> {
    match ty {
        Type::Vec2(VecElement::F32) => Some(2),
        Type::Vec3(VecElement::F32) => Some(3),
        Type::Vec4(VecElement::F32) => Some(4),
        _ => None,
    }
}
