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
    /// Pointer into a storage-image texel, produced by
    /// `OpImageTexelPointer`. The pointee is one image
    /// texel; the only legal consumers are atomic ops.
    Image,
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
    /// Pack a vec2 of f32 into a u32 as two f16 halves
    /// (`packHalf2x16`): lane 0 → low 16 bits, lane 1 → high
    /// 16.  f16 is internal to the lowering — it never
    /// appears as an IR type.
    PackHalf2x16(Value),
    /// Unpack a u32 into a vec2 of f32, treating the low /
    /// high 16 bits as two f16 values (`unpackHalf2x16`).
    UnpackHalf2x16(Value),
    /// Count leading zeros (32-bit).  CLZ on ARM64; cls/clz
    /// on Cranelift.  Returns 32 if input is 0.
    Clz(Value),
    /// Bit-reverse a 32-bit integer.  RBIT on ARM64;
    /// bitreverse on Cranelift.
    Rbit(Value),
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
    /// Dynamic pointer offset.  Produces `base + index *
    /// stride`, used by the frontend when an SPIR-V
    /// OpAccessChain index isn't a compile-time constant
    /// (e.g. `ssbo.data[gid.x]`).  The frontend emits this
    /// AFTER any constant-prefix `AccessChain` that walked
    /// through enclosing struct members.
    ///
    /// `base` must be a Pointer-typed Value (a Variable or
    /// the result of an AccessChain); the result has the
    /// same Pointer type.  `index` is a u32/i32 Value;
    /// `stride` is the element size in bytes (typically 4
    /// for `u32`/`f32`, 16 for `vec4<f32>`).
    PtrOffsetDynamic {
        /// Base pointer (post any constant AccessChain).
        base: Value,
        /// Dynamic 32-bit integer index.
        index: Value,
        /// Element size in bytes.  Backends pick between
        /// shift-by-log2 (power-of-two stride) and madd
        /// (general case).
        stride: u32,
    },

    // ── Type conversions ───────────────────────────────────────

    /// `floor(x)` (GLSL.std.450 Floor) — round toward -inf.
    FFloor(Value),
    /// `ceil(x)`  (GLSL.std.450 Ceil)  — round toward +inf.
    FCeil(Value),
    /// `trunc(x)` (GLSL.std.450 Trunc) — round toward zero.
    FTrunc(Value),
    /// `abs(x)` for f32 scalars and vectors (GLSL.std.450 FAbs).
    FAbs(Value),
    /// `sqrt(x)` for f32 scalars and vectors (GLSL.std.450 Sqrt).
    FSqrt(Value),
    /// `min(a, b)` for f32 scalars/vectors (GLSL.std.450 FMin).
    FMin(Value, Value),
    /// `max(a, b)` for f32 scalars/vectors (GLSL.std.450 FMax).
    FMax(Value, Value),

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
    /// `*ptr = min(*ptr, value)` (signed); returns old value.
    AtomicSMin {
        /// Pointer.
        ptr: Value,
        /// Comparand.
        value: Value,
    },
    /// `*ptr = max(*ptr, value)` (signed); returns old value.
    AtomicSMax {
        /// Pointer.
        ptr: Value,
        /// Comparand.
        value: Value,
    },
    /// `*ptr = min(*ptr, value)` (unsigned); returns old value.
    AtomicUMin {
        /// Pointer.
        ptr: Value,
        /// Comparand.
        value: Value,
    },
    /// `*ptr = max(*ptr, value)` (unsigned); returns old value.
    AtomicUMax {
        /// Pointer.
        ptr: Value,
        /// Comparand.
        value: Value,
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
    /// Screen-space derivative (`OpDPdx`/`OpDPdy`/`OpFwidth`
    /// + Fine/Coarse variants).  Computed via 2x2-quad
    /// re-execution in the rasterizer: a runtime helper
    /// records `value` per quad lane in a probe pass and
    /// returns the lane-difference in the final pass.
    /// `site` is a per-op unique id (the result ValueId)
    /// keying the quad's per-site operand store; `axis` is
    /// 0 = dFdx, 1 = dFdy, 2 = fwidth.  Result is f32 (vec
    /// derivatives are lowered component-wise by the
    /// frontend).
    Derivative {
        /// The value whose screen-space derivative is taken.
        value: Value,
        /// Per-op site id (keys the quad operand store).
        site: u32,
        /// 0 = dFdx, 1 = dFdy, 2 = fwidth.
        axis: u8,
    },
    /// Depth-comparison ("shadow") sample
    /// (`OpImageSampleDref*`).  Samples the depth texture
    /// bound at `sampled_image`, compares the sampled
    /// value against `dref` using the SAMPLER's runtime
    /// `compareOp`, and returns the (PCF-filtered) pass
    /// fraction in [0, 1] as an f32.  The compare op +
    /// filtering live in the runtime helper because they're
    /// sampler state the shader compiler can't see.
    ImageSampleDref {
        /// Combined depth-image + comparison sampler.
        sampled_image: Value,
        /// UV.
        coord: Value,
        /// Depth reference to compare against.
        dref: Value,
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
    /// Read a texel from a storage image (`OpImageRead`).
    /// Unfiltered, no sampler — the texel address is pure
    /// integer arithmetic.  Result is always a vec4 (the
    /// shader may extract a narrower vector).
    ImageRead {
        /// Storage-image descriptor handle.
        image: Value,
        /// Integer coords (ivec2 / uvec2 for a 2D image).
        coord: Value,
    },
    /// Write a texel to a storage image (`OpImageWrite`).
    /// No result.  `texel` is a vec4; narrower image
    /// formats drop the unused lanes in the runtime helper.
    ImageWrite {
        /// Storage-image descriptor handle.
        image: Value,
        /// Integer coords.
        coord: Value,
        /// vec4 texel value to store.
        texel: Value,
    },
    /// Query the dimensions of a sampled image at a given
    /// mip level (`OpImageQuerySizeLod`).  Result is a uvec2
    /// for 2D images (width, height).  Distinct from
    /// [`Self::ImageQuerySize`] which queries storage images
    /// via the X19-anchored ImageDesc table; this op reads
    /// off the X1-anchored TexDesc table.  In v1 the LOD
    /// operand is captured but ignored at codegen (we read
    /// the base TexDesc's width/height); real multi-mip
    /// query would indirect through `mip_descs[lod]`.
    SampledImageQuerySizeLod {
        /// SampledImage (or Image) value to query.
        image: Value,
        /// Mip level (i32).  Captured but ignored in v1.
        lod:   Value,
    },
    /// Gather a 2×2 footprint of one channel
    /// (`OpImageGather`).  Returns a `vec4` whose elements
    /// are the chosen channel from each of the four texels
    /// around the sample coordinate (per GLSL ordering:
    /// (0,1), (1,1), (1,0), (0,0)).  `component` is the
    /// channel index 0..3 (RGBA).
    ImageGather {
        /// SampledImage value.
        sampled_image: Value,
        /// Sample coordinate (vec2 for 2D).
        coord: Value,
        /// i32 channel selector (0=R, 1=G, 2=B, 3=A).
        component: Value,
    },
    /// Read a texel from a specific mip level of a storage
    /// image (`OpImageRead` with `Image-Operands::Lod`).
    /// Same shape as [`Self::ImageRead`] but with an extra
    /// `lod: Value` scalar selecting the mip level.  Lowers
    /// to the runtime's `atrium_img_read_2d_lod` /
    /// `atrium_img_read_3d_lod` helpers (selected by coord
    /// lane count) on both backends.
    ImageReadLod {
        /// Storage-image handle (`Op::ImageHandle`).
        image: Value,
        /// Integer-coord vector (ivec2 for 2D, ivec3 for 3D).
        coord: Value,
        /// i32 mip level (0 = base).
        lod:   Value,
    },
    /// Write a texel to a specific mip level of a storage
    /// image (`OpImageWrite` with `Image-Operands::Lod`).
    /// Same shape as [`Self::ImageWrite`] plus a `lod`
    /// scalar; lowers to the `_lod` helpers.
    ImageWriteLod {
        /// Storage-image handle (`Op::ImageHandle`).
        image: Value,
        /// Integer-coord vector (ivec2 for 2D, ivec3 for 3D).
        coord: Value,
        /// vec4 texel value to write.
        texel: Value,
        /// i32 mip level (0 = base).
        lod:   Value,
    },
    /// Compute a pointer to a single texel of a storage
    /// image (`OpImageTexelPointer`).  The result is a
    /// pointer Value that `Atomic*` ops then operate on --
    /// this is how `imageAtomicAdd` / `imageAtomicExchange`
    /// etc. are expressed.  The texel is treated as a 32-bit
    /// integer cell (the only width SPIR-V allows for image
    /// atomics).
    ImageTexelPointer {
        /// Storage-image descriptor handle.
        image: Value,
        /// Integer coords (ivec2 / uvec2 for a 2D image).
        coord: Value,
    },
    /// Query the dimensions of a storage image
    /// (`OpImageQuerySize`).  Result is a uvec2 / uvec3
    /// holding `(width, height [, depth])`, read directly
    /// from the `ImageDesc`.  No helper call.
    ImageQuerySize(Value),

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

    // ── Synchronisation ────────────────────────────────────────

    /// Workgroup-scope control barrier (SPIR-V's
    /// `OpControlBarrier` with `Scope::Workgroup` execution
    /// scope, and any memory scope / semantics).  Every
    /// invocation in the workgroup must reach this point before
    /// any moves past it.
    ///
    /// Memory scope and semantics from the SPIR-V operands are
    /// intentionally not carried in the IR: Atrium's tier-2
    /// dispatcher implements all four meaningful combinations
    /// (Workgroup / Subgroup × Acquire / Release / AcqRel) as
    /// a single synchronous wait at this point, because the
    /// only synchronisation primitive the dispatcher has is
    /// the per-workgroup `std::sync::Barrier`.  Atomic ops
    /// within the workgroup carry their own ordering via
    /// `Op::AtomicIAdd` etc.
    ///
    /// `OpControlBarrier` with non-Workgroup execution scope
    /// (e.g. Device or Subgroup) is rejected at frontend
    /// translation time: tier-2 has no Device-scope parallel
    /// dispatcher, and at subgroupSize=1 a Subgroup-scope
    /// barrier is trivially satisfied by every invocation.
    /// The frontend short-circuits Subgroup-scope to
    /// translate-as-nothing.
    ///
    /// `OpMemoryBarrier` (no execution scope, just memory
    /// fences) is currently translated as a no-op: the only
    /// memory the dispatcher actually parallelises across is
    /// workgroup-shared memory, which Op::Barrier already
    /// covers.  Add a separate Op::MemoryBarrier in the
    /// future if we add device-shared memory paths that need
    /// cache fences without a barrier.
    Barrier,
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
    /// Compute-shader workgroup size (`LocalSize` SPIR-V
    /// execution mode).  `None` for non-compute functions
    /// and for compute functions that left LocalSize at its
    /// implicit default; backends should treat `None` as
    /// `(1, 1, 1)`.  Used by Op::LoadBuiltin to compute
    /// `gl_GlobalInvocationID = WorkgroupId * LocalSize +
    /// LocalInvocationID` at codegen time.
    pub local_size: Option<(u32, u32, u32)>,
    /// Per-variable descriptor bindings: ValueId of a
    /// Variable in this function -> (set, binding) from the
    /// SPIR-V `DescriptorSet` / `Binding` decorations.
    ///
    /// Today populated for StorageBuffer variables only (the
    /// multi-binding compute arc).  Uniform/UniformConstant
    /// bindings are flattened into `Module::uniforms` with
    /// pre-resolved offsets and don't need per-variable
    /// lookup at codegen.  Image/sampler descriptors are
    /// resolved via `Op::ImageHandle { set, binding }` which
    /// carries the binding inline.
    pub ssbo_bindings: HashMap<u32, (u32, u32)>,
    /// Total byte size of the per-workgroup scratch buffer
    /// this compute function needs, computed at frontend time
    /// from the sum of its `StorageClass::Workgroup` OpVariable
    /// sizes (aligned).  Zero if the shader declares no
    /// workgroup-shared memory.  The host dispatcher
    /// allocates a buffer of this size per worker thread and
    /// passes its base pointer as the `workgroup_buf` ABI
    /// slot (10th cs_main argument, at SP+8).
    pub workgroup_size: u32,
    /// Per-variable byte offsets inside the workgroup
    /// scratch buffer.  Indexed by the variable's IR
    /// ValueId; codegen resolves Workgroup-storage OpVariable
    /// to `(workgroup_buf_ptr, offset)`.
    pub workgroup_var_offset: HashMap<ValueId, u32>,
    /// Per-variable byte offsets inside the VS `out_varyings`
    /// buffer.  Indexed by the variable's IR ValueId; codegen
    /// resolves Location-decorated `Output`-storage
    /// OpVariable to `(out_varyings_ptr, offset)`.  BuiltIn
    /// outputs (e.g. `gl_Position`) are NOT in this map --
    /// they stay on the legacy `out_position` (params[6])
    /// path through the BuiltIn translation.  Empty for FS
    /// / CS shaders.
    pub output_varying_byte_offset: HashMap<ValueId, u32>,
    /// Per-variable byte offsets inside the VS
    /// `in_attributes` / FS `in_varyings` buffer.  Indexed
    /// by the variable's IR ValueId; codegen resolves
    /// Location-decorated `Input`-storage OpVariable to
    /// `(in_buffer_ptr, offset)`.  Without this map a shader
    /// with multiple Inputs would read all of them from
    /// offset 0 (the symptom: a VS reading `in_pos` at
    /// Location 0 and `in_color` at Location 1 both read
    /// from the position bytes).
    pub input_varying_byte_offset: HashMap<ValueId, u32>,
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
    /// `gl_WorkGroupSize` (Compute only).  uvec3.  Equal to
    /// the SPIR-V OpExecutionMode LocalSize -- so this is
    /// effectively a compile-time constant the backends fold
    /// in from `func.local_size`.
    WorkgroupSize,
    /// `gl_LocalInvocationIndex` (Compute only).  uint.
    /// Linearised index of the invocation within its
    /// workgroup, formed as
    ///   lid.z * (LocalSize.x * LocalSize.y)
    /// + lid.y *  LocalSize.x
    /// + lid.x
    /// Backends fold the LocalSize constants in -- the
    /// formula collapses to a single mov when LocalSize is
    /// (N, 1, 1).
    LocalInvocationIndex,
    /// `gl_VertexIndex` (Vertex only).  uint.
    VertexIndex,
    /// `gl_InstanceIndex` (Vertex only).  uint.
    InstanceIndex,
    /// `gl_FrontFacing` (Fragment only).  Bool, materialised as
    /// an i32 (1 = front-facing, 0 = back) so it feeds OpSelect
    /// and integer comparisons directly.  The rasterizer derives
    /// it per-triangle from the screen-space winding vs the
    /// pipeline's `VkFrontFace` and passes it as the FS entry's
    /// trailing `front_facing` parameter.
    FrontFacing,
    /// `gl_PrimitiveID` (Fragment only).  uint -- the 0-based
    /// index of the primitive (triangle / line / point) the
    /// fragment belongs to within the draw.  The rasterizer
    /// supplies it as the FS entry's trailing `primitive_id`
    /// parameter.
    PrimitiveId,
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
pub const TIER2_SHADER_ABI_VERSION: u32 = 2;
//
// History:
//   v1 -- original tier-2 ABI (Arc 1-149).
//   v2 -- Arc 150 carves an 8-byte slot at byte 64 of the
//         compute image-table for the `atrium_barrier`
//         function pointer.  Image-descriptor base shifts
//         from 64 to 72.  Existing cached .so files have the
//         old offset baked in; bumping the ABI version sends
//         them into a different cache directory, forcing
//         transparent recompile on first run.

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
