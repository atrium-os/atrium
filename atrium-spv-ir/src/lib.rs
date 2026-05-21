//! atrium-spv-ir — SSA IR for the tier-2 software Vulkan renderer.
//!
//! This crate defines the IR that sits between
//! `atrium-spv-frontend` (SPIR-V parser + SSA constructor +
//! structured-CFG recoverer) and the production backends
//! (`atrium-spv-backend-bespoke`, `atrium-spv-backend-cranelift`).
//! The test-only `atrium-spv-tests::interpreter` consumes the
//! same IR as the differential-test oracle.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §5 — IR design
//! - [`docs/spec/tier2-shader-codegen-constraints.md`] §B — IR
//!   invariants that every pass must preserve
//!
//! # Status
//!
//! **Phase 0 v0b skeleton.** Types only, no methods, no
//! validation, no construction helpers, no Display. The
//! frontend lands the constructors (phase 1); the validator
//! lands alongside the first backend (phase 2/3); helpers
//! get added when the frontend / backends concretely need
//! them. Resist building speculative API surface.
//!
//! # Shape at a glance
//!
//! ```text
//!   Module
//!     entry_points: [EntryPoint]
//!     functions:    [Function]
//!     uniforms:     [UniformBinding]
//!     push_constants_size: u32
//!     vertex_inputs: [VertexInput]    // vertex stage only
//!     varyings:     [Varying]         // between stages
//!
//!   Function
//!     name: String
//!     stage: ShaderStage
//!     entry_block: BlockId
//!     blocks: { BlockId -> Block }
//!     params: [Value]
//!     return_type: Type
//!
//!   Block
//!     id: BlockId
//!     kind: BlockKind                 // Linear / IfHeader / LoopHeader / SwitchHeader / Merge
//!     insts: [Inst]                   // terminator is the last element
//!
//!   Inst
//!     op: Op                          // ~80 variants; the bulk of the IR
//!     result: Option<Value>
//!     source_spirv_offset: u32        // for the PC-map sidecar (constraint A2)
//!
//!   Value = ValueId + Type            // dense per-function SSA ids
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;

// ── Identifiers ─────────────────────────────────────────────────

/// Dense per-function SSA value identifier.
///
/// Values are unique within a `Function`. The frontend assigns
/// these sequentially during SSA construction. Pass rewrites
/// preserve uniqueness — when an op is replaced, the
/// replacement gets a fresh id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueId(pub u32);

/// Dense per-function basic-block identifier.
///
/// `BlockId(0)` is conventionally the entry block by frontend
/// construction, but no consumer should rely on that — read
/// `Function::entry_block` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

// ── Types ───────────────────────────────────────────────────────

/// The IR's type system.
///
/// Numeric types match SPIR-V's directly. Vector types stay
/// first-class through to instruction selection (constraint
/// B2) — passes do not lower them to per-lane scalars early.
/// Pointer types carry a storage class indicating which flat
/// block they index into (constraint B5).
///
/// Notably:
/// - `Bool` exists as a distinct type for readability but the
///   bespoke + Cranelift backends treat it identically to
///   `I32` per constraint B4 (0/1 convention). The interpreter
///   may use a native bool internally.
/// - `Vec3` is stored in vec4 lanes on the backends; the high
///   lane is undefined (constraint B3). Don't dereference it.
/// - No `Mat*` type today; SPIR-V matrices are lowered to
///   N vec4s by the frontend. Add `Mat` as a first-class type
///   if codegen-side patterns prove it's worth the complexity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    /// Function return type only.
    Void,
    /// 0/1 with i32 backing per constraint B4.
    Bool,
    /// Signed 32-bit integer.
    I32,
    /// Unsigned 32-bit integer.
    U32,
    /// Signed 64-bit integer.
    I64,
    /// Unsigned 64-bit integer.
    U64,
    /// IEEE 754 binary32.
    F32,
    /// IEEE 754 binary64. Rarely used; backends may reject.
    F64,
    /// Two-lane vector.
    Vec2(VecElement),
    /// Three-lane vector. High lane undefined per constraint B3.
    Vec3(VecElement),
    /// Four-lane vector.
    Vec4(VecElement),
    /// 4×4 matrix, column-major (SPIR-V's
    /// `OpTypeMatrix 4 (OpTypeVector elem 4)`). Backends
    /// lower a `Mat4` value into four column vec4s; the
    /// matrix never has its own register file.
    Mat4(VecElement),
    /// Typed offset into a flat storage block. Box keeps Type
    /// finite-sized.
    Pointer(StorageClass, Box<Type>),
    /// Opaque image handle. Lowered to a runtime descriptor by
    /// the backend; never directly addressed.
    Image(ImageDimensionality),
    /// Opaque sampler handle. Same as `Image`.
    Sampler,
    /// Combined image+sampler. SPIR-V's
    /// `OpTypeSampledImage`.
    SampledImage(ImageDimensionality),
}

/// Scalar element type for vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VecElement {
    /// 32-bit float lane.
    F32,
    /// 64-bit float lane (rare; backends may reject).
    F64,
    /// Signed 32-bit integer lane.
    I32,
    /// Unsigned 32-bit integer lane.
    U32,
}

/// Storage class for a pointer.
///
/// Determines which flat block the pointer indexes into. All
/// resolved to byte offsets by the frontend (constraint B5);
/// no runtime pointer arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    /// Per-vertex input attributes (vertex stage).
    Input,
    /// Inter-stage varyings (vertex out → fragment in).
    Output,
    /// Uniform descriptor block.
    Uniform,
    /// Read-only descriptor binding for "opaque" handles
    /// (images, samplers, sampled-images). SPIR-V's
    /// `UniformConstant` — these don't carry block data;
    /// the pointer is a descriptor reference resolved by
    /// the runtime, not a loadable memory address.
    UniformConstant,
    /// Storage descriptor block (read-write).
    StorageBuffer,
    /// Push-constant block (capped at 128 bytes).
    PushConstant,
    /// Function-local scratch.
    Function,
    /// Shader-private (per-invocation persistent).
    Private,
    /// Compute-shader workgroup-shared.
    Workgroup,
}

/// Image dimensionality. Maps to SPIR-V's `Dim` operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDimensionality {
    /// 1D texture.
    Dim1D,
    /// 2D texture (the common case).
    Dim2D,
    /// 3D texture.
    Dim3D,
    /// Cube map.
    Cube,
    /// 2D rect (no mipmaps, non-normalised coords).
    Rect,
    /// Buffer texture (1D, linear layout).
    Buffer,
}

// ── Values ──────────────────────────────────────────────────────

/// An SSA value: an id paired with its type.
///
/// `Value` is `Clone + Copy` (Type is small enough to copy);
/// passes pass it by value freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    /// SSA identifier; unique within the owning Function.
    pub id: ValueId,
    /// The value's type.
    pub ty: Type,
}

// ── Instructions ────────────────────────────────────────────────

/// An IR instruction.
///
/// Every instruction carries the SPIR-V byte offset that
/// produced it (constraint A2) for PC-map sidecar generation
/// and crash triage. Passes that synthesise new instructions
/// inherit the offset of the original they're replacing.
#[derive(Debug, Clone)]
pub struct Inst {
    /// The operation and its operands.
    pub op: Op,
    /// Where the result lands. `None` for void-typed ops
    /// (stores, branches, returns, etc.).
    pub result: Option<Value>,
    /// Byte offset into the source SPIR-V module that
    /// produced this instruction. Preserved through rewrites.
    pub source_spirv_offset: u32,
}

/// All IR operations.
///
/// Grouped by category in the order they're typically
/// emitted by the frontend:
///
/// 1. Constants
/// 2. Arithmetic (integer + float, scalar + vector)
/// 3. Bitwise / shift
/// 4. Comparison (producing Bool)
/// 5. Vector ops (shuffle / extract / insert / dot)
/// 6. Memory (load / store / access chain)
/// 7. Type conversions
/// 8. Selection (`OpSelect`)
/// 9. Atomic operations
/// 10. Image sampling / fetching
/// 11. Fragment derivatives
/// 12. Terminators (branch / switch / return / discard)
/// 13. Phi nodes (start of merge blocks)
///
/// The variant set covers SPIR-V's GLSL 4.50 core subset;
/// less-common ops (extended-instruction-set math functions,
/// quad operations, subgroup ops) are missing and will be
/// added by phase 9.
#[derive(Debug, Clone)]
pub enum Op {
    // ── Constants ──────────────────────────────────────────────

    /// Integer constant.
    ConstInt {
        /// The value, sign-extended to i64 for any
        /// width ≤ i64.
        value: i64,
        /// Width + signedness.
        kind: IntKind,
    },
    /// Floating-point constant.
    ConstFloat {
        /// The value (always stored as f64 for precision;
        /// emitted as f32 if the IR type is F32).
        value: f64,
        /// Width.
        kind: FloatKind,
    },
    /// Vector constant — composed from element values.
    ConstVec(Vec<Value>),
    /// Zero / null value of any type.
    ConstNull,

    // ── Integer arithmetic ─────────────────────────────────────

    /// `result = a + b` (integer; signedness from operand type).
    IAdd(Value, Value),
    /// `result = a - b`.
    ISub(Value, Value),
    /// `result = a * b`.
    IMul(Value, Value),
    /// Unsigned division.
    UDiv(Value, Value),
    /// Signed division.
    SDiv(Value, Value),
    /// Unsigned remainder.
    UMod(Value, Value),
    /// Signed remainder (sign follows divisor per SPIR-V).
    SMod(Value, Value),
    /// `result = -a`.
    INeg(Value),

    // ── Float arithmetic ───────────────────────────────────────

    /// `result = a + b` (float).
    FAdd(Value, Value),
    /// `result = a - b`.
    FSub(Value, Value),
    /// `result = a * b`.
    FMul(Value, Value),
    /// `result = a / b`.
    FDiv(Value, Value),
    /// Float remainder.
    FRem(Value, Value),
    /// `result = -a`.
    FNeg(Value),
    /// `result = matrix * vector` — SPIR-V
    /// `OpMatrixTimesVector`, column-major: each result
    /// lane `i` is `Σ matrix[j][i] * vector[j]`. The
    /// backend lowers this to 4 vec×scalar broadcasts
    /// (`matrix.column[j] * vector[j]`) followed by 3
    /// vec+vec adds — every op below it already exists
    /// + is tested. The IR carries the op so the
    /// frontend's `OpAccessChain` into a struct's mat4
    /// member resolves cleanly through `Type::Mat4`.
    MatrixTimesVector {
        /// `Mat4`-typed Value (or pointer to one, in
        /// which case the backend reads the four columns
        /// at byte_offset 0, 16, 32, 48 of the matrix's
        /// storage).
        matrix: Value,
        /// Vec4-typed Value.
        vector: Value,
    },

    // ── Bitwise / shift ────────────────────────────────────────

    /// Bitwise AND.
    BitAnd(Value, Value),
    /// Bitwise OR.
    BitOr(Value, Value),
    /// Bitwise XOR.
    BitXor(Value, Value),
    /// Bitwise NOT.
    BitNot(Value),
    /// Left shift.
    Shl(Value, Value),
    /// Logical right shift (unsigned).
    LShr(Value, Value),
    /// Arithmetic right shift (signed).
    AShr(Value, Value),

    // ── Comparison ─────────────────────────────────────────────
    //
    // Integer comparisons produce Bool (i32 0/1 per B4).
    // Float comparisons specify ordered vs unordered NaN
    // semantics matching SPIR-V's FOrd*/FUnord* split.

    /// Integer equality.
    IEq(Value, Value),
    /// Integer inequality.
    INe(Value, Value),
    /// Unsigned less-than.
    ULt(Value, Value),
    /// Unsigned less-or-equal.
    ULe(Value, Value),
    /// Unsigned greater-than.
    UGt(Value, Value),
    /// Unsigned greater-or-equal.
    UGe(Value, Value),
    /// Signed less-than.
    SLt(Value, Value),
    /// Signed less-or-equal.
    SLe(Value, Value),
    /// Signed greater-than.
    SGt(Value, Value),
    /// Signed greater-or-equal.
    SGe(Value, Value),
    /// Float ordered equality (false if either operand NaN).
    FOrdEq(Value, Value),
    /// Float ordered inequality.
    FOrdNe(Value, Value),
    /// Float ordered less-than.
    FOrdLt(Value, Value),
    /// Float ordered less-or-equal.
    FOrdLe(Value, Value),
    /// Float ordered greater-than.
    FOrdGt(Value, Value),
    /// Float ordered greater-or-equal.
    FOrdGe(Value, Value),
    /// Float unordered equality (true if either operand NaN).
    FUnordEq(Value, Value),
    /// Float unordered inequality.
    FUnordNe(Value, Value),
    /// Float unordered less-than.
    FUnordLt(Value, Value),
    /// Float unordered less-or-equal.
    FUnordLe(Value, Value),
    /// Float unordered greater-than.
    FUnordGt(Value, Value),
    /// Float unordered greater-or-equal.
    FUnordGe(Value, Value),

    // ── Vector ops ─────────────────────────────────────────────

    /// `OpVectorShuffle`: produce a new vector from
    /// components of two source vectors. `components[i]`
    /// indexes into `src1 ++ src2` (so for two 4-lane
    /// sources, valid indices are 0..8).
    VectorShuffle {
        /// First source vector.
        src1: Value,
        /// Second source vector.
        src2: Value,
        /// Per-lane source indices.
        components: Vec<u32>,
    },
    /// Extract a scalar from a vector lane.
    VectorExtract {
        /// The vector.
        vector: Value,
        /// Lane index.
        index: u32,
    },
    /// Replace one lane of a vector with a scalar.
    VectorInsert {
        /// The original vector.
        vector: Value,
        /// The scalar to insert.
        scalar: Value,
        /// Lane index to replace.
        index: u32,
    },
    /// Dot product of two vectors (sum of per-lane products).
    Dot(Value, Value),

    // ── Memory ─────────────────────────────────────────────────

    /// Load a value through a pointer.
    Load(Value),
    /// Load the value of a stage built-in (e.g.
    /// `gl_LocalInvocationID`).  The frontend recognises an
    /// OpLoad whose source variable has a `BuiltIn`
    /// decoration and emits this op instead of `Op::Load`;
    /// the backend produces the value from the appropriate
    /// stage-ABI parameters rather than from memory.
    LoadBuiltin(BuiltinKind),
    /// Store a value through a pointer.
    Store {
        /// Pointer to store into.
        ptr: Value,
        /// Value to store.
        value: Value,
    },
    /// SPIR-V's `OpAccessChain`: produce a new pointer by
    /// adding a constant byte offset to `base`. Per
    /// constraint B5 the frontend resolves all index
    /// chains to a single byte offset at translate time;
    /// the backend never walks indices at runtime.
    ///
    /// The result Value's type is `Pointer(<base's storage
    /// class>, <pointee leaf type>)`.
    AccessChain {
        /// Base pointer.
        base: Value,
        /// Byte offset from `base` to the leaf pointee.
        byte_offset: u32,
    },

    // ── Type conversions ───────────────────────────────────────

    /// Signed int → float.
    ConvertSToF(Value),
    /// Float → signed int (truncation toward zero per SPIR-V).
    ConvertFToS(Value),
    /// Unsigned int → float.
    ConvertUToF(Value),
    /// Float → unsigned int (truncation toward zero).
    ConvertFToU(Value),
    /// Sign-extend or truncate to a different int width
    /// keeping signedness.
    SConvert(Value, IntKind),
    /// Zero-extend or truncate keeping unsigned semantics.
    UConvert(Value, IntKind),
    /// Float-to-float width change (f32 ↔ f64).
    FConvert(Value, FloatKind),
    /// Reinterpret bits as a different type (same size).
    Bitcast(Value, Type),

    // ── Selection ──────────────────────────────────────────────

    /// SPIR-V's `OpSelect`: `cond ? t_val : f_val`.
    /// `cond` is Bool; `t_val` and `f_val` share a type.
    Select {
        /// Selector (Bool).
        cond: Value,
        /// Selected if cond is true.
        t_val: Value,
        /// Selected if cond is false.
        f_val: Value,
    },

    // ── Atomic ─────────────────────────────────────────────────
    //
    // All atomics MUST lower to LSE on ARM64 per constraint
    // C6 (no LDXR/STXR fallback). On x86_64 they use the
    // standard LOCK-prefixed forms.

    /// Atomic load through pointer.
    AtomicLoad(Value),
    /// Atomic store through pointer.
    AtomicStore {
        /// Pointer to store into.
        ptr: Value,
        /// Value to store.
        value: Value,
    },
    /// `*ptr += value`, returns old value.
    AtomicIAdd {
        /// Pointer.
        ptr: Value,
        /// Increment.
        value: Value,
    },
    /// Bitwise AND in place.
    AtomicAnd {
        /// Pointer.
        ptr: Value,
        /// Mask.
        value: Value,
    },
    /// Bitwise OR in place.
    AtomicOr {
        /// Pointer.
        ptr: Value,
        /// Mask.
        value: Value,
    },
    /// Bitwise XOR in place.
    AtomicXor {
        /// Pointer.
        ptr: Value,
        /// Mask.
        value: Value,
    },
    /// Atomic compare-and-swap. Returns previous value.
    AtomicCompareExchange {
        /// Pointer.
        ptr: Value,
        /// Value to compare against.
        expected: Value,
        /// Value to write if comparison succeeds.
        desired: Value,
    },
    /// Atomic unconditional swap. Returns previous value.
    AtomicExchange {
        /// Pointer.
        ptr: Value,
        /// New value.
        value: Value,
    },

    // ── Image / sampler ────────────────────────────────────────
    //
    // These lower to calls into atrium-spv-runtime. The
    // backend emits the call sequence; the runtime kernel
    // does the actual filtered sample.

    /// A descriptor-bound handle to an image / sampler /
    /// sampled-image. Produced by the frontend in place of
    /// `Op::Load` when the load targets a `UniformConstant`
    /// variable of image/sampler type — *not* a memory
    /// load, but the binding's `(set, binding)` resolved
    /// at translate time. Consumed by `CombineSampledImage`
    /// and `ImageSample*` / `ImageFetch`.
    ImageHandle {
        /// SPIR-V `DescriptorSet` decoration value.
        set:     u32,
        /// SPIR-V `Binding` decoration value.
        binding: u32,
    },
    /// Combine an image binding and a sampler binding into
    /// a sampled-image value (SPIR-V `OpSampledImage`).
    /// Both operands are descriptor references — produced
    /// by `Op::Load` through an image/sampler variable —
    /// and the result is a `Type::SampledImage(dim)` that
    /// feeds `ImageSample*`. No native instructions are
    /// emitted: a backend just tracks the pair so it can
    /// resolve both descriptors at the sample call site.
    CombineSampledImage {
        /// Image binding (descriptor reference).
        image: Value,
        /// Sampler binding (descriptor reference).
        sampler: Value,
    },
    /// Sample an image with implicit LOD computation from
    /// fragment derivatives (fragment shader only).
    ImageSampleImplicitLod {
        /// Combined image+sampler.
        sampled_image: Value,
        /// UV (or UVW for 3D).
        coord: Value,
    },
    /// Sample an image with explicit LOD.
    ImageSampleExplicitLod {
        /// Combined image+sampler.
        sampled_image: Value,
        /// UV.
        coord: Value,
        /// Mip level.
        lod: Value,
    },
    /// Direct unfiltered texel fetch by integer coords.
    ImageFetch {
        /// Image (without sampler).
        image: Value,
        /// Integer coords.
        coord: Value,
        /// Mip level (or 0).
        lod: Option<Value>,
    },

    // ── Fragment derivatives ───────────────────────────────────
    //
    // Computed by the rasterizer across 2×2 pixel quads;
    // the backend emits a call to the runtime that reads
    // from neighbouring quad pixels' varyings.

    /// Partial derivative w.r.t. screen X.
    DPdx(Value),
    /// Partial derivative w.r.t. screen Y.
    DPdy(Value),
    /// `abs(dPdx) + abs(dPdy)` — common LOD heuristic.
    Fwidth(Value),

    // ── Terminators ────────────────────────────────────────────
    //
    // Always the last instruction in their block. The IR
    // validator (phase 2) enforces this.

    /// Unconditional branch.
    Branch(BlockId),
    /// Conditional branch.
    BranchCond {
        /// Selector (Bool).
        cond: Value,
        /// Taken if cond is true.
        t_block: BlockId,
        /// Taken if cond is false.
        f_block: BlockId,
    },
    /// Multi-way branch.
    Switch {
        /// Selector (integer).
        selector: Value,
        /// (case-value, target) pairs.
        cases: Vec<(i64, BlockId)>,
        /// Fallback for unmatched values.
        default: BlockId,
    },
    /// Return from a void function.
    Return,
    /// Return a value.
    ReturnValue(Value),
    /// Discard the current fragment (fragment shader only).
    Discard,

    // ── Phi ────────────────────────────────────────────────────

    /// SSA phi node. Always at the start of a merge block;
    /// the IR validator (phase 2) enforces that.
    Phi(Vec<PhiArm>),
}

/// Width + signedness for integer constants and conversions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntKind {
    /// Signed 32-bit.
    I32,
    /// Unsigned 32-bit.
    U32,
    /// Signed 64-bit.
    I64,
    /// Unsigned 64-bit.
    U64,
}

/// Width for float constants and conversions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatKind {
    /// 32-bit.
    F32,
    /// 64-bit.
    F64,
}

/// One arm of a phi node.
#[derive(Debug, Clone)]
pub struct PhiArm {
    /// Predecessor block.
    pub from: BlockId,
    /// Value as observed from that predecessor.
    pub value: Value,
}

// ── Blocks ──────────────────────────────────────────────────────

/// A basic block.
///
/// Blocks are typed by their role in the structured CFG.
/// The frontend recovers this from SPIR-V's
/// `OpSelectionMerge` / `OpLoopMerge` markers (constraint
/// A4). The backend uses block kind to emit structured
/// control flow without falling back to general CFG
/// reconstruction.
#[derive(Debug, Clone)]
pub struct Block {
    /// Block identifier.
    pub id: BlockId,
    /// Role in the structured CFG.
    pub kind: BlockKind,
    /// Instructions in source order. The last entry is
    /// always a terminator (`Branch`, `BranchCond`,
    /// `Switch`, `Return`, `ReturnValue`, or `Discard`).
    /// Phi nodes, when present, come first (before
    /// non-phi instructions). IR validator enforces.
    pub insts: Vec<Inst>,
}

/// A block's role in the structured CFG.
///
/// `Linear` is a straight-line block; the four `Header`
/// variants mark the start of a structured construct and
/// carry the merge / continue targets the backend uses to
/// emit the equivalent control flow.
#[derive(Debug, Clone)]
pub enum BlockKind {
    /// Straight-line block ending in an unconditional
    /// branch or return.
    Linear,
    /// Start of an if/else construct. The terminator is
    /// `BranchCond { t, f }`; control reconverges at
    /// `merge`.
    IfHeader {
        /// Merge block where both arms reconverge.
        merge: BlockId,
    },
    /// Start of a loop. The header always branches into
    /// the loop body; the loop exits to `merge`; the
    /// back-edge from the body lands at `continue_`.
    LoopHeader {
        /// Block reached when the loop exits.
        merge: BlockId,
        /// Block where back-edges land (the continue target).
        continue_: BlockId,
    },
    /// Start of a switch. The terminator is `Switch`;
    /// control reconverges at `merge`.
    SwitchHeader {
        /// Merge block where all cases (including default)
        /// reconverge.
        merge: BlockId,
    },
    /// A merge point where multiple predecessors join.
    /// Phi nodes (if any) sit at the top of these blocks.
    Merge,
}

// ── Functions ───────────────────────────────────────────────────

/// A single function.
///
/// Top-level functions correspond to SPIR-V function
/// definitions. The entry-point function for a stage
/// (vertex / fragment / compute) is named in the
/// containing `Module::entry_points`.
#[derive(Debug, Clone)]
pub struct Function {
    /// Function name. Entry-point functions are named per
    /// the SPIR-V's `OpEntryPoint` operand and become
    /// exported `atrium_*_main` symbols in the compiled
    /// `.so`.
    pub name: String,
    /// Which shader stage this function belongs to.
    /// Non-entry functions inherit their caller's stage;
    /// for now we don't allow cross-stage function calls
    /// (each shader is a self-contained graph).
    pub stage: ShaderStage,
    /// Function parameters (in declaration order).
    pub params: Vec<Value>,
    /// Function return type.
    pub return_type: Type,
    /// Entry block id.
    pub entry_block: BlockId,
    /// Block storage, keyed by id.
    pub blocks: HashMap<BlockId, Block>,
}

/// Shader stage.
///
/// Tier-2 v1 supports three stages; tessellation +
/// geometry + mesh are out of scope per the renderer
/// spec's non-goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderStage {
    /// Vertex stage (per-vertex shader execution).
    Vertex,
    /// Fragment stage (per-pixel shader execution).
    Fragment,
    /// Compute stage (per-workgroup-invocation).
    Compute,
}

/// SPIR-V stage built-ins recognised by atrium-spv-ir.
///
/// The frontend identifies these by their `Decoration::BuiltIn`
/// annotation on a SPIR-V `OpVariable`; uses of such variables
/// lower to `Op::LoadBuiltin(kind)` instead of going through
/// memory.  Each backend maps a builtin onto whichever stage-
/// ABI parameter carries that value (e.g. WorkgroupId on
/// Compute → params[3..6]).
///
/// Vector-typed builtins (WorkgroupId, LocalInvocationId,
/// GlobalInvocationId) load as a 3-lane `uint` vector;
/// scalar builtins (VertexIndex, InstanceIndex) load as a
/// single `uint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinKind {
    /// `gl_WorkgroupID` (Compute only).  uvec3.
    WorkgroupId,
    /// `gl_LocalInvocationID` (Compute only).  uvec3.
    LocalInvocationId,
    /// `gl_GlobalInvocationID` (Compute only).  uvec3.
    /// Equal to `WorkgroupId * gl_WorkGroupSize + LocalInvocationID`.
    GlobalInvocationId,
    /// `gl_VertexIndex` (Vertex only).  uint.
    VertexIndex,
    /// `gl_InstanceIndex` (Vertex only).  uint.
    InstanceIndex,
}

// ── Module ──────────────────────────────────────────────────────

/// A compiled SPIR-V module in atrium-spv-ir form.
///
/// One `Module` corresponds to one `vkCreateShaderModule`
/// call. A typical module has 1 entry-point function plus a
/// handful of helper functions (e.g. inlined math); some
/// engines emit larger modules with multiple entry points
/// (e.g. compute + vertex in one binary).
#[derive(Debug, Clone)]
pub struct Module {
    /// Functions defined in this module.
    pub functions: Vec<Function>,
    /// Entry points referencing functions by index into
    /// `functions`.
    pub entry_points: Vec<EntryPoint>,
    /// Uniform-buffer layout (flattened across descriptor
    /// sets — per constraint B5 every uniform is resolved
    /// to a flat byte offset at translate time).
    pub uniforms: Vec<UniformBinding>,
    /// Total size of the push-constant block in bytes.
    /// Capped at 128 by `PhysicalDeviceLimits::maxPushConstantsSize`.
    pub push_constants_size: u32,
    /// Per-vertex inputs (vertex stage only).
    pub vertex_inputs: Vec<VertexInput>,
    /// Inter-stage varyings.
    pub varyings: Vec<Varying>,
}

/// One declared entry point.
#[derive(Debug, Clone)]
pub struct EntryPoint {
    /// Stage.
    pub stage: ShaderStage,
    /// Index into `Module::functions`.
    pub function_index: usize,
    /// Name as declared in SPIR-V (becomes the exported
    /// `atrium_<stage>_main` symbol).
    pub name: String,
}

/// One uniform-buffer binding.
#[derive(Debug, Clone)]
pub struct UniformBinding {
    /// Descriptor set.
    pub set: u32,
    /// Binding within the set.
    pub binding: u32,
    /// Flat byte offset into the daemon-side concatenated
    /// uniforms block at runtime. Frontend computes this
    /// from the descriptor set layout.
    pub offset: u32,
    /// Type of the bound resource.
    pub ty: Type,
}

/// One per-vertex input.
#[derive(Debug, Clone)]
pub struct VertexInput {
    /// `layout(location = N)` from GLSL.
    pub location: u32,
    /// Flat byte offset into the per-vertex attribute
    /// block.
    pub offset: u32,
    /// Type (one of the scalar / vector types).
    pub ty: Type,
}

/// One inter-stage varying.
#[derive(Debug, Clone)]
pub struct Varying {
    /// `layout(location = N)`.
    pub location: u32,
    /// Flat byte offset into the per-pixel varying block
    /// after interpolation.
    pub offset: u32,
    /// Type (scalar / vector).
    pub ty: Type,
}

// ── Shader-ABI version ──────────────────────────────────────────

/// Shader ABI version, propagated to the
/// `ATRIUM_SHADER_METADATA::abi_version` field at compile
/// time and checked by the daemon at dlopen time. See
/// `docs/spec/tier2-renderer.md` §4.3.
///
/// Bump when:
/// - Any of the `atrium_*_main` function signatures change.
/// - The `AtriumShaderMetadata` struct layout changes.
/// - The atrium-spv-runtime API changes.
/// - The uniform / varying / vertex-attr layout rules
///   change.
///
/// Old cached `.so`s become unreachable under the new
/// version directory (constraint F3); next launch
/// recompiles transparently.
pub const TIER2_SHADER_ABI_VERSION: u32 = 1;

// ── Doc-only re-exports ────────────────────────────────────────
//
// The crate is types-only at phase-0 v0b. Methods land
// alongside their consumers:
//
// - Module::validate()  — alongside the IR validator
//   (phase 2, used by every backend boundary)
// - Module::display()   — alongside debugging tools
//   (lands when we need to read IR dumps)
// - Module::builder()   — alongside the frontend
//   (phase 1; small helpers for SSA-construction)
//
// Resist adding these speculatively. Build them when the
// first consumer concretely needs them.
