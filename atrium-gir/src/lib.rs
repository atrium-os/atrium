//! atrium-gir — the Atrium GPU IR.
//!
//! GIR is the target-independent level of the shader compiler
//! (docs/spec/atrium-gpu-ir.md). It sits between the SPIR-V frontend IR
//! (`atrium-spv-ir`, shared with the CPU backends) and the per-architecture
//! machine layers (`atrium-rdna4-compiler` first).
//!
//! Shape:
//! * SSA values, each with a [`Ty`]. Arithmetic is scalar; vector types only
//!   appear as the data of loads, stores, [`Op::Construct`] and
//!   [`Op::Extract`].
//! * A function body is a tree of [`Node`]s: straight-line [`Node::Inst`]s,
//!   [`Node::If`] and [`Node::Loop`]. Every reconvergence point is explicit.
//!   Values merged by an `if` or carried by a loop are region results and
//!   loop parameters ([`Node::If::results`], [`Node::Loop::params`]), fed by
//!   [`Op::Yield`], [`Op::Continue`] and [`Op::Break`].
//! * Memory is addressed by [`Space`] plus a byte offset value; there are no
//!   raw pointers.
//!
//! The crate holds the data model, a [`verify`] pass, the [`build`]er from
//! the frontend IR, and an [`interp`]reter with real subgroup semantics used
//! as an oracle.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

pub mod build;
pub mod interp;
pub mod passes;
pub mod verify;

/// An SSA value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ValueId(pub u32);

/// Scalar types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scalar {
    /// One bit per invocation.
    Bool,
    /// 32-bit integer; signedness lives in the ops.
    I32,
    /// 32-bit IEEE float.
    F32,
}

/// Value types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    /// A scalar.
    S(Scalar),
    /// A vector of 2–4 scalars (memory data only).
    V(Scalar, u8),
}

impl Ty {
    /// `bool`.
    pub const BOOL: Ty = Ty::S(Scalar::Bool);
    /// `i32`.
    pub const I32: Ty = Ty::S(Scalar::I32);
    /// `f32`.
    pub const F32: Ty = Ty::S(Scalar::F32);

    /// Number of components (1 for scalars).
    pub fn comps(self) -> usize {
        match self {
            Ty::S(_) => 1,
            Ty::V(_, n) => n as usize,
        }
    }

    /// Element scalar type.
    pub fn scalar(self) -> Scalar {
        match self {
            Ty::S(s) | Ty::V(s, _) => s,
        }
    }

    /// Size in memory, in bytes (`bool` is stored as 4 bytes).
    pub fn bytes(self) -> u32 {
        4 * self.comps() as u32
    }
}

/// Where a memory op points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Space {
    /// A storage buffer, by descriptor set and binding.
    Buffer {
        /// Descriptor set.
        set: u32,
        /// Binding.
        binding: u32,
    },
    /// A per-invocation variable (index into [`Function::privates`]).
    Private(u32),
    /// Workgroup-shared memory (LDS on AMD, SLM on Intel).
    Workgroup,
    /// The push-constant block (read-only).
    Push,
}

/// Unary ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnOp {
    /// Integer negate.
    INeg,
    /// Bitwise (or, on `bool`, logical) not.
    Not,
    /// Float negate.
    FNeg,
    /// Float absolute value.
    FAbs,
    /// Float square root.
    FSqrt,
    /// Round toward −∞.
    FFloor,
    /// Round toward +∞.
    FCeil,
    /// Round toward zero.
    FTrunc,
    /// Count leading zeros.
    Clz,
    /// Reverse bits.
    BitReverse,
}

/// Binary ops. On `bool`, `And`/`Or`/`Xor` are logical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    /// Wrapping add.
    IAdd,
    /// Wrapping subtract.
    ISub,
    /// Wrapping multiply (low 32 bits).
    IMul,
    /// Unsigned divide.
    UDiv,
    /// Signed divide.
    SDiv,
    /// Unsigned remainder.
    UMod,
    /// Signed remainder (sign of the dividend).
    SRem,
    /// Bitwise and.
    And,
    /// Bitwise or.
    Or,
    /// Bitwise xor.
    Xor,
    /// Shift left (shift amount taken modulo 32).
    Shl,
    /// Logical shift right.
    LShr,
    /// Arithmetic shift right.
    AShr,
    /// Float add.
    FAdd,
    /// Float subtract.
    FSub,
    /// Float multiply.
    FMul,
    /// Float divide.
    FDiv,
    /// Float minimum.
    FMin,
    /// Float maximum.
    FMax,
}

/// Comparisons (result `bool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    /// Integer (or `bool`) equal.
    IEq,
    /// Integer (or `bool`) not equal.
    INe,
    /// Unsigned less than.
    ULt,
    /// Unsigned less or equal.
    ULe,
    /// Signed less than.
    SLt,
    /// Signed less or equal.
    SLe,
    /// Ordered float equal.
    FOEq,
    /// Ordered float not equal.
    FONe,
    /// Ordered float less than.
    FOLt,
    /// Ordered float less or equal.
    FOLe,
    /// Unordered float equal.
    FUEq,
    /// Unordered float not equal.
    FUNe,
    /// Unordered float less than.
    FULt,
    /// Unordered float less or equal.
    FULe,
}

/// Conversions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConvOp {
    /// Signed int → float.
    SToF,
    /// Unsigned int → float.
    UToF,
    /// Float → signed int (toward zero).
    FToS,
    /// Float → unsigned int (toward zero).
    FToU,
    /// `bool` → `i32` (0 or 1).
    BoolToI,
}

/// Atomic read-modify-write ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AtomicOp {
    /// Add.
    Add,
    /// And.
    And,
    /// Or.
    Or,
    /// Xor.
    Xor,
    /// Signed minimum.
    SMin,
    /// Signed maximum.
    SMax,
    /// Unsigned minimum.
    UMin,
    /// Unsigned maximum.
    UMax,
    /// Exchange.
    Exchange,
}

/// Shader inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// `gl_GlobalInvocationID` (`vec3<i32>`).
    GlobalId,
    /// `gl_LocalInvocationID` (`vec3<i32>`).
    LocalId,
    /// `gl_WorkGroupID` (`vec3<i32>`).
    WorkgroupId,
    /// `gl_LocalInvocationIndex` (`i32`).
    LocalIndex,
    /// `gl_SubgroupSize` (`i32`): [`SUBGROUP_SIZE`].
    SubgroupSize,
    /// `gl_SubgroupInvocationID` (`i32`): the local index modulo the
    /// subgroup size.
    SubgroupLocalId,
    /// `gl_SubgroupID` (`i32`): the local index divided by the subgroup
    /// size.
    SubgroupId,
    /// `gl_NumSubgroups` (`i32`).
    NumSubgroups,
}

/// Subgroup size. Subgroups are consecutive runs of this many invocations
/// in local-index order (the last one may be partial). Fixed at 32 for
/// now: RDNA wave32. A per-target value comes with the first wave64 or
/// SIMD16 target.
pub const SUBGROUP_SIZE: u32 = 32;

/// Arithmetic of a subgroup reduction or scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupArith {
    /// Wrapping add (identity 0).
    IAdd,
    /// Float add (identity 0.0).
    FAdd,
    /// Wrapping multiply (identity 1).
    IMul,
    /// Float multiply (identity 1.0).
    FMul,
    /// Signed minimum (identity `i32::MAX`).
    SMin,
    /// Unsigned minimum (identity `u32::MAX`).
    UMin,
    /// Float minimum (identity +inf).
    FMin,
    /// Signed maximum (identity `i32::MIN`).
    SMax,
    /// Unsigned maximum (identity 0).
    UMax,
    /// Float maximum (identity -inf).
    FMax,
    /// Bitwise and (identity all ones).
    And,
    /// Bitwise or (identity 0).
    Or,
    /// Bitwise xor (identity 0).
    Xor,
}

impl GroupArith {
    /// The identity element's bits.
    pub fn identity(self) -> u32 {
        match self {
            GroupArith::IAdd | GroupArith::UMax | GroupArith::Or | GroupArith::Xor => 0,
            GroupArith::FAdd => 0.0f32.to_bits(),
            GroupArith::IMul => 1,
            GroupArith::FMul => 1.0f32.to_bits(),
            GroupArith::SMin => i32::MAX as u32,
            GroupArith::UMin | GroupArith::And => u32::MAX,
            GroupArith::FMin => f32::INFINITY.to_bits(),
            GroupArith::SMax => i32::MIN as u32,
            GroupArith::FMax => f32::NEG_INFINITY.to_bits(),
        }
    }

    /// True for the float ops.
    pub fn is_float(self) -> bool {
        matches!(self, GroupArith::FAdd | GroupArith::FMul | GroupArith::FMin | GroupArith::FMax)
    }

    /// `a op b` on raw bits.
    pub fn apply(self, a: u32, b: u32) -> u32 {
        let (x, y) = (f32::from_bits(a), f32::from_bits(b));
        match self {
            GroupArith::IAdd => a.wrapping_add(b),
            GroupArith::FAdd => (x + y).to_bits(),
            GroupArith::IMul => a.wrapping_mul(b),
            GroupArith::FMul => (x * y).to_bits(),
            GroupArith::SMin => (a as i32).min(b as i32) as u32,
            GroupArith::UMin => a.min(b),
            GroupArith::FMin => x.min(y).to_bits(),
            GroupArith::SMax => (a as i32).max(b as i32) as u32,
            GroupArith::UMax => a.max(b),
            GroupArith::FMax => x.max(y).to_bits(),
            GroupArith::And => a & b,
            GroupArith::Or => a | b,
            GroupArith::Xor => a ^ b,
        }
    }
}

/// Subgroup operations. Each acts on the *active* invocations of the
/// executing invocation's subgroup (those in the current control-flow
/// mask). Data operands are scalar `i32` or `f32` unless noted; the
/// builder scalarises vectors and turns `bool` data into `i32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubgroupOp {
    /// `() -> bool`: true in the lowest active invocation.
    Elect,
    /// `(bool) -> bool`: true in every active invocation.
    All,
    /// `(bool) -> bool`: true in some active invocation.
    Any,
    /// `(x) -> bool`: `x` has the same bits in every active invocation.
    AllEqual,
    /// `(bool) -> i32`: bit `i` set when invocation `i` is active and true.
    Ballot,
    /// `(x, id) -> x` from invocation `id` (dynamically uniform, active).
    Broadcast,
    /// `(x) -> x` from the lowest active invocation.
    BroadcastFirst,
    /// `(x, id) -> x` from invocation `id`.
    Shuffle,
    /// `(x, m) -> x` from invocation `self ^ m`.
    ShuffleXor,
    /// `(x, d) -> x` from invocation `self - d`.
    ShuffleUp,
    /// `(x, d) -> x` from invocation `self + d`.
    ShuffleDown,
    /// `(x) -> x`: reduction over the active invocations in id order.
    Reduce(GroupArith),
    /// `(x) -> x`: inclusive prefix in id order.
    InclusiveScan(GroupArith),
    /// `(x) -> x`: exclusive prefix in id order (identity in the lowest).
    ExclusiveScan(GroupArith),
}

/// An instruction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Op {
    /// A constant: raw bits of a scalar (`bool`: 0 or 1).
    Const(u32),
    /// Unary op.
    Un(UnOp, ValueId),
    /// Binary op.
    Bin(BinOp, ValueId, ValueId),
    /// Fused multiply-add `a * b + c` (f32).
    Fma(ValueId, ValueId, ValueId),
    /// Comparison.
    Cmp(CmpOp, ValueId, ValueId),
    /// `cond ? t : f`.
    Select(ValueId, ValueId, ValueId),
    /// Conversion.
    Conv(ConvOp, ValueId),
    /// Reinterpret the bits as the result type (same component count).
    Bitcast(ValueId),
    /// Build a vector from scalars.
    Construct(Vec<ValueId>),
    /// Component of a vector.
    Extract(ValueId, u8),
    /// Shader input.
    Builtin(Builtin),
    /// Load the result type from `space` at byte offset `offset` (an `i32`).
    Load(Space, ValueId),
    /// Store `value` to `space` at byte offset `offset`.
    Store(Space, ValueId, ValueId),
    /// Atomic RMW on a 32-bit word; the result is the old value.
    Atomic(AtomicOp, Space, ValueId, ValueId),
    /// Workgroup execution and memory barrier.
    Barrier,
    /// Subgroup operation. Depends on which invocations are active, so it
    /// is never merged or moved across control flow; unused ones are
    /// removed.
    Subgroup(SubgroupOp, Vec<ValueId>),
    /// End an `if` arm, giving the arm's values for [`Node::If::results`].
    Yield(Vec<ValueId>),
    /// Next iteration of the innermost loop with these parameter values.
    Continue(Vec<ValueId>),
    /// Leave the innermost loop with these result values.
    Break(Vec<ValueId>),
    /// End the invocation.
    Return,
}

impl Op {
    /// True for ops that end a region.
    pub fn is_terminator(&self) -> bool {
        matches!(self, Op::Yield(_) | Op::Continue(_) | Op::Break(_) | Op::Return)
    }

    /// Values this op reads.
    pub fn operands(&self) -> Vec<ValueId> {
        match self {
            Op::Const(_) | Op::Builtin(_) | Op::Barrier | Op::Return => vec![],
            Op::Un(_, a) | Op::Conv(_, a) | Op::Bitcast(a) | Op::Extract(a, _) | Op::Load(_, a) => vec![*a],
            Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::Store(_, a, b) | Op::Atomic(_, _, a, b) => vec![*a, *b],
            Op::Fma(a, b, c) | Op::Select(a, b, c) => vec![*a, *b, *c],
            Op::Construct(v) | Op::Yield(v) | Op::Continue(v) | Op::Break(v) | Op::Subgroup(_, v) => v.clone(),
        }
    }
}

impl Op {
    /// Mutable references to the values this op reads.
    pub fn operands_mut(&mut self) -> Vec<&mut ValueId> {
        match self {
            Op::Const(_) | Op::Builtin(_) | Op::Barrier | Op::Return => vec![],
            Op::Un(_, a) | Op::Conv(_, a) | Op::Bitcast(a) | Op::Extract(a, _) | Op::Load(_, a) => vec![a],
            Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::Store(_, a, b) | Op::Atomic(_, _, a, b) => vec![a, b],
            Op::Fma(a, b, c) | Op::Select(a, b, c) => vec![a, b, c],
            Op::Construct(v) | Op::Yield(v) | Op::Continue(v) | Op::Break(v) | Op::Subgroup(_, v) => v.iter_mut().collect(),
        }
    }

    /// True for ops without side effects, which may be removed when unused
    /// or merged when identical.
    pub fn is_pure(&self) -> bool {
        matches!(self, Op::Const(_) | Op::Un(..) | Op::Bin(..) | Op::Fma(..) | Op::Cmp(..) | Op::Select(..)
            | Op::Conv(..) | Op::Bitcast(_) | Op::Construct(_) | Op::Extract(..) | Op::Builtin(_))
    }
}

/// One instruction and its result.
#[derive(Debug, Clone, PartialEq)]
pub struct Inst {
    /// Result value, if the op produces one.
    pub result: Option<ValueId>,
    /// The op.
    pub op: Op,
}

/// A region: a sequence of nodes.
pub type Region = Vec<Node>;

/// A node of the region tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// One instruction.
    Inst(Inst),
    /// Two-way branch that reconverges after the node.
    If {
        /// Condition (`bool`).
        cond: ValueId,
        /// Taken when `cond` is true.
        then: Region,
        /// Taken when `cond` is false.
        els: Region,
        /// Values defined after the node, fed by each arm's `Yield`.
        results: Vec<ValueId>,
    },
    /// A loop. The body runs until every invocation has left by `Break`
    /// or `Return`. Falling off the end of the body is `Continue` with no
    /// values (only allowed when there are no parameters).
    Loop {
        /// Initial values of `params`.
        inits: Vec<ValueId>,
        /// Values defined at the top of the body.
        params: Vec<ValueId>,
        /// The body.
        body: Region,
        /// Values defined after the node, fed by `Break`.
        results: Vec<ValueId>,
    },
}

/// True if control never leaves `r` by falling off its end: it ends in
/// `continue`, `break` or `return`, or in an `if` whose arms both never fall.
/// (`yield` falls through to the `if`'s merge.)
pub fn never_falls(r: &Region) -> bool {
    match r.last() {
        Some(Node::Inst(i)) => matches!(i.op, Op::Continue(_) | Op::Break(_) | Op::Return),
        Some(Node::If { then, els, .. }) => never_falls(then) && never_falls(els),
        _ => false,
    }
}

/// A compute kernel in GIR.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// Entry-point name.
    pub name: String,
    /// Workgroup size.
    pub local_size: [u32; 3],
    /// Type of every value, indexed by `ValueId.0`.
    pub types: Vec<Ty>,
    /// Size in bytes of each private variable.
    pub privates: Vec<u32>,
    /// Workgroup-shared memory size in bytes.
    pub workgroup_bytes: u32,
    /// Storage buffers the kernel uses, as (set, binding).
    pub buffers: Vec<(u32, u32)>,
    /// Size of the push-constant block in bytes (0 if none).
    pub push_bytes: u32,
    /// The body.
    pub body: Region,
}

impl Function {
    /// Type of a value.
    pub fn ty(&self, v: ValueId) -> Ty {
        self.types[v.0 as usize]
    }

    /// Allocate a new value of type `ty`.
    pub fn new_value(&mut self, ty: Ty) -> ValueId {
        self.types.push(ty);
        ValueId(self.types.len() as u32 - 1)
    }
}

// ── Printing ────────────────────────────────────────────────────

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = |s: Scalar| match s {
            Scalar::Bool => "bool",
            Scalar::I32 => "i32",
            Scalar::F32 => "f32",
        };
        match self {
            Ty::S(x) => write!(f, "{}", s(*x)),
            Ty::V(x, n) => write!(f, "vec{n}<{}>", s(*x)),
        }
    }
}

fn list(v: &[ValueId]) -> String {
    v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
}

impl Function {
    fn print_region(&self, r: &Region, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        for n in r {
            match n {
                Node::Inst(i) => {
                    out.push_str(&pad);
                    if let Some(v) = i.result {
                        out.push_str(&format!("{v}: {} = ", self.ty(v)));
                    }
                    let body = match &i.op {
                        Op::Const(b) => match i.result.map(|v| self.ty(v)) {
                            Some(Ty::S(Scalar::F32)) => format!("const {:?}", f32::from_bits(*b)),
                            _ => format!("const {}", *b as i32),
                        },
                        Op::Un(o, a) => format!("{o:?} {a}"),
                        Op::Bin(o, a, b) => format!("{o:?} {a}, {b}"),
                        Op::Fma(a, b, c) => format!("fma {a}, {b}, {c}"),
                        Op::Cmp(o, a, b) => format!("cmp.{o:?} {a}, {b}"),
                        Op::Select(c, a, b) => format!("select {c}, {a}, {b}"),
                        Op::Conv(o, a) => format!("{o:?} {a}"),
                        Op::Bitcast(a) => format!("bitcast {a}"),
                        Op::Construct(v) => format!("construct {}", list(v)),
                        Op::Extract(a, i) => format!("extract {a}[{i}]"),
                        Op::Builtin(b) => format!("builtin {b:?}"),
                        Op::Load(s, o) => format!("load {s:?} + {o}"),
                        Op::Store(s, o, v) => format!("store {s:?} + {o}, {v}"),
                        Op::Atomic(a, s, o, v) => format!("atomic.{a:?} {s:?} + {o}, {v}"),
                        Op::Barrier => "barrier".into(),
                        Op::Subgroup(o, v) => format!("subgroup.{o:?} {}", list(v)),
                        Op::Yield(v) => format!("yield {}", list(v)),
                        Op::Continue(v) => format!("continue {}", list(v)),
                        Op::Break(v) => format!("break {}", list(v)),
                        Op::Return => "return".into(),
                    };
                    out.push_str(&body);
                    out.push('\n');
                }
                Node::If { cond, then, els, results } => {
                    let res = if results.is_empty() { String::new() } else { format!("{} = ", list(results)) };
                    out.push_str(&format!("{pad}{res}if {cond} {{\n"));
                    self.print_region(then, depth + 1, out);
                    out.push_str(&format!("{pad}}} else {{\n"));
                    self.print_region(els, depth + 1, out);
                    out.push_str(&format!("{pad}}}\n"));
                }
                Node::Loop { inits, params, body, results } => {
                    let res = if results.is_empty() { String::new() } else { format!("{} = ", list(results)) };
                    out.push_str(&format!("{pad}{res}loop ({}) = ({}) {{\n", list(params), list(inits)));
                    self.print_region(body, depth + 1, out);
                    out.push_str(&format!("{pad}}}\n"));
                }
            }
        }
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut s = format!(
            "kernel {} local_size({}, {}, {}) buffers {:?} push {}B privates {:?} workgroup {}B\n",
            self.name, self.local_size[0], self.local_size[1], self.local_size[2],
            self.buffers, self.push_bytes, self.privates, self.workgroup_bytes
        );
        self.print_region(&self.body, 1, &mut s);
        f.write_str(&s)
    }
}
