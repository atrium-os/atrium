//! SPIR-V validator — pre-execution gate for third-party shader uploads.
//!
//! Per `docs/spec/aqueduct-gpu.md` §11 ("Universal sandbox, no escape
//! valve"), every shader must pass static validation before reaching
//! the GPU. This module is the host-side gate that runs at
//! `OP_GPU_SHADER_UPLOAD` time (and at atrium-pkg install time, by
//! linking the same crate from the package tool).
//!
//! ## Scope of this skeleton
//!
//! Phase 2.0 lands the **structural checks** — the cheap, deterministic
//! gates that don't require dataflow analysis:
//!
//! 1. **Magic number + version**: must be SPIR-V, version ≤ 1.6.
//! 2. **Forbidden capabilities**: explicit allowlist; reject
//!    `PhysicalStorageBufferAddresses`, `RayTracing*` (deferred to
//!    Phase 2.2), and anything not on the list.
//! 3. **Forbidden extensions**: reject `SPV_KHR_physical_storage_buffer`,
//!    `SPV_EXT_descriptor_indexing` (until descriptor-buffer story
//!    lands), etc.
//! 4. **Bounded module size**: hard cap so a malformed module can't
//!    DoS the parser.
//!
//! ## Out of scope here (lands in follow-on commits)
//!
//! - Full bounded-loop analysis (constant-bounded OpLoopMerge bodies).
//!   Phase 2.1.
//! - Descriptor-layout cross-check against pipeline declarations.
//!   Phase 2.3.
//! - Cycle detection / IR validation beyond what spirv-val does.
//!   Phase 2.4 — likely lift `spirv-val` from upstream as a build dep
//!   rather than reimplement.
//!
//! ## Why custom validator
//!
//! `spirv-val` from SPIRV-Tools is the obvious thing to link, and we
//! will, eventually. But the structural checks here are tiny, fast,
//! and policy-specific (the forbidden-capability allowlist is an
//! Atrium decision). They run first and reject 99% of attacks at low
//! cost; `spirv-val` follows for the rest. Implementing the cheap
//! gate ourselves means the trust surface is in-house Rust, not a
//! C dependency.
//!
//! ## Threat model assumed by callers
//!
//! - SPIR-V bytes arrive from an **untrusted source** (third-party
//!   app inside a guest VM). Parser must not panic, not OOM, not run
//!   in unbounded time.
//! - A "valid" verdict from this module is necessary but not
//!   sufficient — see follow-on phases. A "reject" verdict is
//!   final: the shader will not be loaded.

#![warn(missing_docs)]

use std::fmt;

/// SPIR-V module magic number (little-endian on most platforms;
/// the spec mandates this exact value in the first word).
pub const SPIRV_MAGIC: u32 = 0x07230203;

/// Maximum SPIR-V module size we accept (16 MiB). Anything larger
/// is rejected at the bytecount check before parsing begins.
pub const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;

/// SPIR-V instruction opcodes the validator cares about. Subset of
/// the full spec; everything else parses but isn't inspected.
#[allow(missing_docs)]
mod op {
    pub const OP_CAPABILITY:   u16 = 17;
    pub const OP_EXTENSION:    u16 = 10;
    pub const OP_ENTRY_POINT:  u16 = 15;
    pub const OP_TYPE_POINTER: u16 = 32;
    pub const OP_LOOP_MERGE:   u16 = 246;
    pub const OP_FUNCTION:     u16 = 54;
    pub const OP_VARIABLE:     u16 = 59;
    pub const OP_DECORATE:     u16 = 71;
}

/// SPIR-V `ExecutionModel` codes (spec §3.3) → required capability.
/// Returns the capability code an entry point with this model
/// must declare. `None` for models Atrium doesn't honour (e.g.
/// Kernel — OpenCL-style, outside our Vulkan-shaped sandbox).
fn required_capability_for_model(model: u32) -> Option<u32> {
    match model {
        0 => Some(1), // Vertex                   → Shader (1)
        1 => Some(3), // TessellationControl      → Tessellation (3)
        2 => Some(3), // TessellationEvaluation   → Tessellation (3)
        3 => Some(2), // Geometry                 → Geometry (2)
        4 => Some(1), // Fragment                 → Shader (1)
        5 => Some(1), // GLCompute                → Shader (1)
        // 6 Kernel — explicitly NOT honoured. Returning None makes
        // the cross-check reject any module declaring it.
        _ => None,
    }
}

/// Human-readable execution-model name for diagnostics.
fn execution_model_name(model: u32) -> &'static str {
    match model {
        0 => "Vertex",
        1 => "TessellationControl",
        2 => "TessellationEvaluation",
        3 => "Geometry",
        4 => "Fragment",
        5 => "GLCompute",
        6 => "Kernel",
        _ => "<unknown>",
    }
}

/// SPIR-V `Decoration` codes we care about (spec §3.20).
#[allow(missing_docs)]
mod decoration {
    pub const BINDING:        u32 = 33;
    pub const DESCRIPTOR_SET: u32 = 34;
}

/// SPIR-V `StorageClass` codes that require descriptor binding
/// (spec §3.7). A variable in any of these classes is a "resource"
/// — the driver looks it up via a descriptor set / binding pair.
/// Without `DescriptorSet + Binding` decorations the driver picks
/// arbitrary slots, opening descriptor-confusion attacks.
///
/// Resource classes:
///   0  UniformConstant (samplers, images, opaque types)
///   2  Uniform (UBO)
///   10 AtomicCounter
///   11 Image (legacy GL_ARB_shader_image_load_store)
///   12 StorageBuffer (SSBO, since SPV 1.3)
///
/// Note: `PushConstant` (9) is NOT in this list — push constants
/// are accessed via a pipeline-layout-level push-constant block,
/// not a descriptor. `Function` (7), `Workgroup` (4), `Private` (6),
/// `Input` (1), `Output` (3) are also excluded — they're not
/// descriptor-backed.
fn resource_storage_class(sc: u32) -> bool {
    matches!(sc, 0 | 2 | 10 | 11 | 12)
}

/// SPIR-V storage classes the validator REJECTS. Defense-in-depth
/// against buffer-device-address: even if the module didn't declare
/// the capability, declaring an `OpTypePointer` with the
/// `PhysicalStorageBuffer` storage class is the giveaway.
fn forbidden_storage_classes() -> &'static [(u32, &'static str)] {
    &[
        (5349, "PhysicalStorageBuffer storage class implies buffer-device-address"),
    ]
}

/// Maximum total instructions per module. Prevents DoS where a
/// validator pass becomes a sink for arbitrary compute. 256K
/// instructions is generous — Mesa-compiled glslang output for a
/// large fragment shader typically lands well below 10K.
pub const MAX_INSTRUCTIONS_PER_MODULE: usize = 256 * 1024;

/// Maximum number of `OpLoopMerge` instructions per module.
/// Heuristic upper bound on loop nesting; legitimate compute
/// shaders virtually never exceed 64 loops total. This is the
/// Phase 2.1 cheap gate for runaway-loop DoS — Phase 2.2 will
/// add proper CFG-based bounded-loop analysis.
pub const MAX_LOOPS_PER_MODULE: usize = 64;

/// Maximum function-definitions per module. A shader with hundreds
/// of functions is either machine-generated for an attack or so
/// pathological it shouldn't pass review anyway.
pub const MAX_FUNCTIONS_PER_MODULE: usize = 1024;

/// Maximum value accepted in an `OpLoopMerge` `MaxIterations`
/// literal. Loops promising more iterations than this are rejected
/// at validate-time; the GPU timeout would catch them anyway, but
/// rejecting upfront is faster than letting them run to TDR.
///
/// 1<<24 = 16,777,216 — generous for any legitimate compute kernel,
/// tight enough to prevent "max u32" annotations slipping through.
pub const MAX_LOOP_ITERATIONS: u32 = 1 << 24;


/// Validator verdict.
#[derive(Debug)]
pub enum ValidatorError {
    /// Module is smaller than the SPIR-V header (20 bytes).
    TooShort {
        /// Bytes received.
        bytes: usize,
    },
    /// Module exceeds [`MAX_MODULE_BYTES`].
    TooLong {
        /// Bytes received.
        bytes: usize,
        /// Limit enforced.
        cap: usize,
    },
    /// First word didn't match [`SPIRV_MAGIC`].
    BadMagic {
        /// What we saw in the first word.
        got: u32,
    },
    /// SPIR-V version word higher than what we support.
    UnsupportedVersion {
        /// Major.minor packed (e.g. 0x00010600 for 1.6).
        version: u32,
    },
    /// Module declared a forbidden capability.
    ForbiddenCapability {
        /// SPIR-V `Capability` value the module declared.
        capability: u32,
        /// Human-readable reason for the rejection.
        why: &'static str,
    },
    /// Module imported a forbidden extension.
    ForbiddenExtension {
        /// Extension name.
        name: String,
        /// Human-readable reason for the rejection.
        why: &'static str,
    },
    /// Instruction stream is truncated / malformed at the byte level
    /// (word count would read past end-of-module, etc.).
    Truncated {
        /// Word offset where parsing failed.
        word_offset: usize,
    },
    /// Instruction declared zero word-count (would loop forever in
    /// the parser).
    ZeroWordCount {
        /// Word offset of the offending instruction.
        word_offset: usize,
    },
    /// Module exceeds [`MAX_INSTRUCTIONS_PER_MODULE`].
    TooManyInstructions {
        /// Count we hit before bailing.
        count: usize,
        /// Limit enforced.
        cap: usize,
    },
    /// Module exceeds [`MAX_LOOPS_PER_MODULE`].
    TooManyLoops {
        /// Count we hit before bailing.
        count: usize,
        /// Limit enforced.
        cap: usize,
    },
    /// Module exceeds [`MAX_FUNCTIONS_PER_MODULE`].
    TooManyFunctions {
        /// Count we hit before bailing.
        count: usize,
        /// Limit enforced.
        cap: usize,
    },
    /// Module declared an `OpTypePointer` with a forbidden storage
    /// class (buffer-device-address byte-pattern even without the
    /// capability declaration).
    ForbiddenStorageClass {
        /// SPIR-V storage class value.
        storage_class: u32,
        /// Human-readable reason.
        why: &'static str,
    },
    /// `OpLoopMerge` lacks the `MaxIterations` LoopControl bit.
    /// Phase 2.2 requires every loop to carry a producer-supplied
    /// iteration bound — without it the validator cannot prove
    /// termination.
    UnboundedLoop {
        /// Word offset of the offending OpLoopMerge.
        word_offset: usize,
    },
    /// `OpLoopMerge` declared `MaxIterations` exceeding
    /// [`MAX_LOOP_ITERATIONS`].
    LoopIterationsExceedCap {
        /// The literal annotation found.
        annotated: u32,
        /// Limit enforced.
        cap: u32,
    },
    /// A resource-class `OpVariable` is missing required
    /// `DescriptorSet` and/or `Binding` decorations. Without both,
    /// the driver picks slots non-deterministically — a descriptor-
    /// confusion attack vector.
    UnboundResourceVariable {
        /// SPIR-V result-id of the offending variable.
        variable_id: u32,
        /// Storage class (e.g. 12 = StorageBuffer).
        storage_class: u32,
        /// Whether DescriptorSet was missing.
        missing_set: bool,
        /// Whether Binding was missing.
        missing_binding: bool,
    },
    /// `OpEntryPoint` declared an execution model whose required
    /// `Capability` was not declared by any `OpCapability`. Without
    /// the capability declaration, the driver behaviour for this
    /// entry point is undefined.
    EntryPointMissingCapability {
        /// Execution model code (0 = Vertex, 4 = Fragment, etc.).
        execution_model: u32,
        /// Human-readable execution-model name.
        model_name: &'static str,
        /// Required SPIR-V `Capability` code.
        required_capability: u32,
    },
    /// `OpEntryPoint` declared an execution model Atrium doesn't
    /// honour. Currently this is Kernel (6) — OpenCL-style execution
    /// outside the Vulkan-shaped sandbox.
    ForbiddenExecutionModel {
        /// Execution model code.
        execution_model: u32,
        /// Human-readable name.
        model_name: &'static str,
    },
}

impl fmt::Display for ValidatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidatorError::TooShort { bytes } =>
                write!(f, "SPIR-V module too short ({bytes} bytes; need ≥20)"),
            ValidatorError::TooLong { bytes, cap } =>
                write!(f, "SPIR-V module too long ({bytes} bytes; cap {cap})"),
            ValidatorError::BadMagic { got } =>
                write!(f, "bad SPIR-V magic {got:#010x} (expected {SPIRV_MAGIC:#010x})"),
            ValidatorError::UnsupportedVersion { version } =>
                write!(f, "unsupported SPIR-V version {:#010x}", version),
            ValidatorError::ForbiddenCapability { capability, why } =>
                write!(f, "forbidden SPIR-V capability {capability}: {why}"),
            ValidatorError::ForbiddenExtension { name, why } =>
                write!(f, "forbidden SPIR-V extension {name:?}: {why}"),
            ValidatorError::Truncated { word_offset } =>
                write!(f, "SPIR-V instruction at word {word_offset} extends past end of module"),
            ValidatorError::ZeroWordCount { word_offset } =>
                write!(f, "SPIR-V instruction at word {word_offset} has zero word count"),
            ValidatorError::TooManyInstructions { count, cap } =>
                write!(f, "SPIR-V module has too many instructions ({count}; cap {cap})"),
            ValidatorError::TooManyLoops { count, cap } =>
                write!(f, "SPIR-V module has too many OpLoopMerge instructions ({count}; cap {cap})"),
            ValidatorError::TooManyFunctions { count, cap } =>
                write!(f, "SPIR-V module has too many OpFunction definitions ({count}; cap {cap})"),
            ValidatorError::ForbiddenStorageClass { storage_class, why } =>
                write!(f, "forbidden SPIR-V storage class {storage_class}: {why}"),
            ValidatorError::UnboundedLoop { word_offset } =>
                write!(f, "OpLoopMerge at word {word_offset} lacks MaxIterations annotation \
                          (rebuild with bounded-loop emission enabled, e.g. glslang -Os)"),
            ValidatorError::LoopIterationsExceedCap { annotated, cap } =>
                write!(f, "OpLoopMerge declared MaxIterations={annotated} exceeding cap {cap}"),
            ValidatorError::UnboundResourceVariable {
                variable_id, storage_class, missing_set, missing_binding,
            } => {
                let what = match (missing_set, missing_binding) {
                    (true, true)  => "DescriptorSet and Binding",
                    (true, false) => "DescriptorSet",
                    (false, true) => "Binding",
                    (false, false) => "(no decoration missing — bug?)",
                };
                write!(f, "resource OpVariable id={variable_id} \
                          (StorageClass={storage_class}) missing {what} decoration")
            }
            ValidatorError::EntryPointMissingCapability {
                execution_model, model_name, required_capability,
            } => write!(f,
                "OpEntryPoint with execution model {model_name} ({execution_model}) \
                 requires Capability {required_capability} which was not declared"
            ),
            ValidatorError::ForbiddenExecutionModel { execution_model, model_name } =>
                write!(f, "OpEntryPoint declared forbidden execution model \
                          {model_name} ({execution_model})"),
        }
    }
}
impl std::error::Error for ValidatorError {}

/// Highest SPIR-V version the validator accepts. Bumped as we
/// audit newer features. Format: `0x00<major><minor>00`.
pub const MAX_SUPPORTED_VERSION: u32 = 0x0001_0600; // 1.6

/// SPIR-V Capability codes the validator REJECTS. Sourced from
/// the spec `SPIR-V Capability` enumeration. The allowlist
/// approach (default-deny) is preferable in principle but the
/// SPIR-V capability list is huge; this maintains a deny-list of
/// the ones with documented sandbox-escape implications and
/// allows everything else, with the explicit assumption that
/// spirv-val will catch the long tail when we link it in
/// Phase 2.4.
fn forbidden_capabilities() -> &'static [(u32, &'static str)] {
    &[
        // PhysicalStorageBufferAddresses (5347) — VK_KHR_buffer_device_address.
        // Sandbox §11: raw GPU pointers bypass descriptor-bounded
        // isolation. Explicitly forbidden by Atrium policy.
        (5347, "buffer-device-address (PhysicalStorageBufferAddresses) \
                bypasses descriptor-bounded isolation"),

        // RayTracingKHR (4479) — full ray-tracing PIPELINE support (raygen/
        // closest-hit/miss stages + shader binding table). Still deferred: the
        // SBT and the extra pipeline stages are a much larger surface than the
        // wire's closed vocabulary exposes.
        (4479, "ray-tracing-pipeline capability not supported by aqueduct-gpu wire"),
        // RayQueryKHR (4477) — inline ray-tracing in a compute shader. ALLOWED:
        // the acceleration structure is a descriptor-bound resource (VK_DESCRIPTOR
        // _TYPE_ACCELERATION_STRUCTURE_KHR), traversal is fixed-function HW, and
        // the shader only sees committed-hit scalars (t, primitiveIndex,
        // instanceId, barycentrics) — no raw device pointers, so it stays inside
        // the descriptor-bounded model. (buffer-device-address / 5347 remains
        // forbidden; ray_query does not require it in-shader.)
        // RayTracingNV (5340) — vendor extension predecessor.
        (5340, "ray-tracing-NV vendor extension not supported"),

        // MeshShadingNV (5266) / MeshShadingEXT (5283) — bespoke
        // geometry-amplification path. Functional equivalent
        // available via compute + indirect draw; the wire's closed
        // vocabulary doesn't expose mesh-shader-specific stages, so
        // shaders using these can't be hooked up regardless.
        (5266, "mesh-shading-NV not supported by aqueduct-gpu wire"),
        (5283, "mesh-shading-EXT not supported by aqueduct-gpu wire"),

        // CooperativeMatrixNV (5357) / KHR (6022) — tensor cores
        // exposed via foreign matrix types. Deferred — auditing
        // bounds-checking story.
        (5357, "cooperative-matrix-NV deferred"),
        (6022, "cooperative-matrix-KHR deferred"),

        // Kernel (6) — OpenCL-shaped execution. Atrium's wire is
        // Vulkan-shaped; kernel-mode entry points and their
        // associated address spaces are outside the sandbox's
        // descriptor-bounded model.
        (6, "Kernel capability (OpenCL-style) not supported by Atrium's Vulkan-shaped sandbox"),
    ]
}

/// Extension strings the validator REJECTS. SPIR-V extensions are
/// opt-ins; an extension declared in the module enables additional
/// instructions / decorations that may bypass sandbox primitives.
fn forbidden_extensions() -> &'static [(&'static str, &'static str)] {
    &[
        ("SPV_KHR_physical_storage_buffer",
         "physical-storage-buffer extension implies buffer-device-address"),
        ("SPV_EXT_physical_storage_buffer",
         "physical-storage-buffer (EXT) implies buffer-device-address"),
        ("SPV_KHR_ray_tracing",
         "ray-tracing-pipeline extension not supported by aqueduct-gpu wire"),
        // SPV_KHR_ray_query ALLOWED — see the RayQueryKHR (4477) note above.
        ("SPV_NV_ray_tracing",
         "ray-tracing-NV extension not supported"),
        ("SPV_NV_mesh_shader",
         "mesh-shader-NV extension not supported by aqueduct-gpu wire"),
        ("SPV_EXT_mesh_shader",
         "mesh-shader-EXT extension not supported by aqueduct-gpu wire"),
    ]
}

/// Validate a SPIR-V module. Returns `Ok(())` if all structural and
/// policy checks pass; `Err` with a specific diagnostic otherwise.
///
/// Caller is responsible for emitting the rejection as
/// `OP_GPU_VALIDATION_ERR` / failed `ShaderUploadResponse`.
pub fn validate_spirv(bytes: &[u8]) -> Result<(), ValidatorError> {
    if bytes.len() > MAX_MODULE_BYTES {
        return Err(ValidatorError::TooLong { bytes: bytes.len(), cap: MAX_MODULE_BYTES });
    }
    if bytes.len() < 20 {
        return Err(ValidatorError::TooShort { bytes: bytes.len() });
    }
    if bytes.len() % 4 != 0 {
        // Non-word-aligned modules are malformed; treat as truncated.
        return Err(ValidatorError::Truncated { word_offset: bytes.len() / 4 });
    }

    // SPIR-V words are little-endian per spec (in practice).
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    // ── Header ────────────────────────────────────────────────────
    if words[0] != SPIRV_MAGIC {
        return Err(ValidatorError::BadMagic { got: words[0] });
    }
    let version = words[1];
    if version > MAX_SUPPORTED_VERSION {
        return Err(ValidatorError::UnsupportedVersion { version });
    }
    // words[2] = generator, words[3] = id bound, words[4] = schema
    // (reserved). Skip — none affect sandbox correctness.

    // ── Instruction walk ─────────────────────────────────────────
    let mut idx = 5usize;
    let caps = forbidden_capabilities();
    let exts = forbidden_extensions();
    let storage_classes = forbidden_storage_classes();

    let mut instruction_count = 0usize;
    let mut loop_count = 0usize;
    let mut function_count = 0usize;

    // Descriptor-binding cross-check state. Decorations and variables
    // can appear in any order in the binary, so we collect both
    // sides in one pass and check after the walk.
    //   resource_vars: variable_id → storage_class
    //   has_set / has_binding: variable_ids that carry the decoration
    let mut resource_vars: Vec<(u32, u32)> = Vec::new();
    let mut has_set:     std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut has_binding: std::collections::HashSet<u32> = std::collections::HashSet::new();

    // Entry-point / capability cross-check state. OpCapability and
    // OpEntryPoint both appear in the early part of a module, but
    // can be interleaved in any order. Collect for post-pass check.
    let mut declared_caps:   std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut entry_models:    Vec<u32> = Vec::new();

    while idx < words.len() {
        let word0 = words[idx];
        let word_count = (word0 >> 16) as usize;
        let opcode = (word0 & 0xFFFF) as u16;

        if word_count == 0 {
            return Err(ValidatorError::ZeroWordCount { word_offset: idx });
        }
        if idx + word_count > words.len() {
            return Err(ValidatorError::Truncated { word_offset: idx });
        }

        instruction_count += 1;
        if instruction_count > MAX_INSTRUCTIONS_PER_MODULE {
            return Err(ValidatorError::TooManyInstructions {
                count: instruction_count, cap: MAX_INSTRUCTIONS_PER_MODULE,
            });
        }

        match opcode {
            op::OP_CAPABILITY => {
                // OpCapability has exactly one operand word.
                if word_count >= 2 {
                    let cap = words[idx + 1];
                    for (forbid, why) in caps {
                        if cap == *forbid {
                            return Err(ValidatorError::ForbiddenCapability {
                                capability: cap, why,
                            });
                        }
                    }
                    declared_caps.insert(cap);
                }
            }
            op::OP_ENTRY_POINT => {
                // OpEntryPoint layout:
                //   word 1: execution_model
                //   word 2: function_id
                //   word 3+: literal name, then interface ids
                if word_count >= 3 {
                    entry_models.push(words[idx + 1]);
                }
            }
            op::OP_EXTENSION => {
                // OpExtension carries a literal string in remaining
                // words; null-terminated UTF-8 packed little-endian.
                let name_words = &words[idx + 1..idx + word_count];
                let name = read_literal_string(name_words);
                for (forbid, why) in exts {
                    if name == *forbid {
                        return Err(ValidatorError::ForbiddenExtension {
                            name, why,
                        });
                    }
                }
            }
            op::OP_TYPE_POINTER => {
                // OpTypePointer layout: result_id, storage_class,
                // pointed_to_type. word_count = 4.
                if word_count >= 3 {
                    let sc = words[idx + 2];
                    for (forbid, why) in storage_classes {
                        if sc == *forbid {
                            return Err(ValidatorError::ForbiddenStorageClass {
                                storage_class: sc, why,
                            });
                        }
                    }
                }
            }
            op::OP_LOOP_MERGE => {
                loop_count += 1;
                if loop_count > MAX_LOOPS_PER_MODULE {
                    return Err(ValidatorError::TooManyLoops {
                        count: loop_count, cap: MAX_LOOPS_PER_MODULE,
                    });
                }
                // OpLoopMerge layout (SPIR-V §3.32.17):
                //   word 1: merge_block_id
                //   word 2: continue_target_id
                //   word 3: loop_control mask
                //   word 4+: literal operands selected by mask bits,
                //            in the order they're listed in the spec.
                //
                // Per SPIR-V spec §3.23 LoopControl, the bits that
                // carry a literal operand (lowest first):
                //   0x008 DependencyLength      → 1 lit  (SPV 1.1+)
                //   0x010 MinIterations         → 1 lit  (SPV 1.4+)
                //   0x020 MaxIterations         → 1 lit  (SPV 1.4+)
                //   0x040 IterationMultiple     → 1 lit  (SPV 1.4+)
                //   0x080 PeelCount             → 1 lit  (SPV 1.4+)
                //   0x100 PartialCount          → 1 lit  (SPV 1.4+)
                //
                // Bits 0x01 / 0x02 / 0x04 (Unroll, DontUnroll,
                // DependencyInfinite) are flag-only.
                //
                // Validator-strict requires at least one literal-
                // bearing bit; the toolchain's `annotate` step adds
                // `MaxIterations` (0x20) when slangc emits none.
                //
                // Historical note: prior versions of this validator
                // mis-encoded the literal-bearing bits one position
                // off (0x10..0x200 instead of 0x08..0x100). Fixed
                // 2026-05-12 after inspect surfaced the discrepancy.
                if word_count < 4 {
                    return Err(ValidatorError::Truncated { word_offset: idx });
                }
                let loop_control_mask = words[idx + 3];

                // STRICT MODE (locked 2026-05-12): require at least
                // one literal-bearing LoopControl bit. Unroll alone
                // is no longer trusted — the toolchain's annotate
                // step (aqueduct-gpu-host/src/shader_annotate.rs)
                // injects a real MaxIterations literal on every loop,
                // so the validator can demand a literal uniformly.
                //
                // Why strict beats the earlier Unroll-permitting
                // policy:
                //   - Single-rule trust chain: "every loop has a
                //     literal bound". No "backend must refuse to
                //     fallback on Unroll" obligation.
                //   - The annotate step preserves Unroll in the
                //     mask, so drivers still unroll [unroll] loops.
                //     The literal is just additional evidence.
                //
                // Bits that carry literals (SPIR-V spec §3.23):
                //   0x008 DependencyLength
                //   0x010 MinIterations
                //   0x020 MaxIterations       <- the one annotate emits
                //   0x040 IterationMultiple
                //   0x080 PeelCount
                //   0x100 PartialCount
                const LITERAL_BITS: u32 =
                    0x008 | 0x010 | 0x020 | 0x040 | 0x080 | 0x100;
                if loop_control_mask & LITERAL_BITS == 0 {
                    return Err(ValidatorError::UnboundedLoop { word_offset: idx });
                }

                // For each set bit-with-literal, consume one operand
                // word in mask-bit order. Check that MaxIterations
                // (if present) doesn't exceed our cap.
                let mut lit_idx = idx + 4;
                let lit_end = idx + word_count;
                let take_lit = |li: &mut usize| -> Option<u32> {
                    if *li < lit_end {
                        let v = words[*li];
                        *li += 1;
                        Some(v)
                    } else { None }
                };

                let handle_bit = |bit: u32, li: &mut usize|
                    -> Result<(), ValidatorError> {
                    if loop_control_mask & bit != 0 {
                        let lit = take_lit(li).ok_or(
                            ValidatorError::Truncated { word_offset: idx })?;
                        if lit > MAX_LOOP_ITERATIONS {
                            return Err(ValidatorError::LoopIterationsExceedCap {
                                annotated: lit, cap: MAX_LOOP_ITERATIONS,
                            });
                        }
                    }
                    Ok(())
                };
                // Iterate in SPIR-V spec bit order (lowest first).
                handle_bit(0x008, &mut lit_idx)?; // DependencyLength
                handle_bit(0x010, &mut lit_idx)?; // MinIterations
                handle_bit(0x020, &mut lit_idx)?; // MaxIterations
                handle_bit(0x040, &mut lit_idx)?; // IterationMultiple
                handle_bit(0x080, &mut lit_idx)?; // PeelCount
                handle_bit(0x100, &mut lit_idx)?; // PartialCount
            }
            op::OP_FUNCTION => {
                function_count += 1;
                if function_count > MAX_FUNCTIONS_PER_MODULE {
                    return Err(ValidatorError::TooManyFunctions {
                        count: function_count, cap: MAX_FUNCTIONS_PER_MODULE,
                    });
                }
            }
            op::OP_VARIABLE => {
                // OpVariable layout: result_type_id, result_id,
                // storage_class. word_count >= 4.
                if word_count >= 4 {
                    let var_id        = words[idx + 2];
                    let storage_class = words[idx + 3];
                    if resource_storage_class(storage_class) {
                        resource_vars.push((var_id, storage_class));
                    }
                }
            }
            op::OP_DECORATE => {
                // OpDecorate layout: target_id, decoration_code, [literals].
                if word_count >= 3 {
                    let target = words[idx + 1];
                    let deco   = words[idx + 2];
                    match deco {
                        decoration::DESCRIPTOR_SET => { has_set.insert(target); }
                        decoration::BINDING        => { has_binding.insert(target); }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        idx += word_count;
    }

    // ── Post-pass: entry-point / capability cross-check ──────────
    // Every OpEntryPoint's required Capability must have been
    // declared via an OpCapability somewhere in the module. Kernel
    // model is rejected outright as not part of Atrium's Vulkan-
    // shaped sandbox.
    for model in &entry_models {
        match required_capability_for_model(*model) {
            None => {
                return Err(ValidatorError::ForbiddenExecutionModel {
                    execution_model: *model,
                    model_name: execution_model_name(*model),
                });
            }
            Some(required_cap) => {
                if !declared_caps.contains(&required_cap) {
                    return Err(ValidatorError::EntryPointMissingCapability {
                        execution_model: *model,
                        model_name: execution_model_name(*model),
                        required_capability: required_cap,
                    });
                }
            }
        }
    }

    // ── Post-pass: descriptor-binding coverage ────────────────────
    // Every resource OpVariable must carry BOTH DescriptorSet and
    // Binding decorations. SPIR-V allows decorations in any order
    // relative to the variable they target, so this runs after the
    // single forward walk has collected both sides.
    for (var_id, storage_class) in &resource_vars {
        let missing_set     = !has_set.contains(var_id);
        let missing_binding = !has_binding.contains(var_id);
        if missing_set || missing_binding {
            return Err(ValidatorError::UnboundResourceVariable {
                variable_id:    *var_id,
                storage_class:  *storage_class,
                missing_set,
                missing_binding,
            });
        }
    }

    Ok(())
}

/// Decode a SPIR-V literal string: bytes packed into u32 words,
/// little-endian per byte position within a word, NUL-terminated.
fn read_literal_string(words: &[u32]) -> String {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words {
        bytes.push((w & 0xff) as u8);
        bytes.push(((w >> 8) & 0xff) as u8);
        bytes.push(((w >> 16) & 0xff) as u8);
        bytes.push(((w >> 24) & 0xff) as u8);
    }
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes.truncate(nul);
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid SPIR-V module: just header, no instructions.
    fn minimal_header(version: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20);
        bytes.extend_from_slice(&SPIRV_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // generator
        bytes.extend_from_slice(&1u32.to_le_bytes()); // bound
        bytes.extend_from_slice(&0u32.to_le_bytes()); // schema
        bytes
    }

    /// Build header + one OpCapability instruction.
    fn header_with_capability(cap: u32) -> Vec<u8> {
        let mut bytes = minimal_header(0x0001_0000);
        // Word 0: word_count=2 in high 16, opcode=OpCapability(17) in low 16.
        let inst0 = (2u32 << 16) | 17u32;
        bytes.extend_from_slice(&inst0.to_le_bytes());
        bytes.extend_from_slice(&cap.to_le_bytes());
        bytes
    }

    /// Build header + OpExtension with given name.
    fn header_with_extension(name: &str) -> Vec<u8> {
        let mut bytes = minimal_header(0x0001_0000);
        let mut name_bytes = name.as_bytes().to_vec();
        name_bytes.push(0); // NUL
        while name_bytes.len() % 4 != 0 { name_bytes.push(0); }
        let word_count = 1 + name_bytes.len() / 4;
        let inst0 = ((word_count as u32) << 16) | 10u32; // OpExtension = 10
        bytes.extend_from_slice(&inst0.to_le_bytes());
        bytes.extend_from_slice(&name_bytes);
        bytes
    }

    #[test]
    fn accepts_minimal_module() {
        let m = minimal_header(0x0001_0000);
        validate_spirv(&m).expect("minimal SPIR-V header should validate");
    }

    #[test]
    fn rejects_too_short() {
        let m = vec![0u8; 10];
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::TooShort { .. })));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut m = minimal_header(0x0001_0000);
        m[0] ^= 0xFF;
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::BadMagic { .. })));
    }

    #[test]
    fn rejects_unsupported_version() {
        let m = minimal_header(0x0001_FF00); // 1.255 — future
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::UnsupportedVersion { .. })));
    }

    #[test]
    fn rejects_non_word_aligned() {
        let mut m = minimal_header(0x0001_0000);
        m.push(0);
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::Truncated { .. })));
    }

    #[test]
    fn rejects_too_long() {
        let mut m = minimal_header(0x0001_0000);
        m.resize(MAX_MODULE_BYTES + 4, 0);
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::TooLong { .. })));
    }

    #[test]
    fn accepts_allowed_capability() {
        // Capability 1 = Shader (the basic one).
        let m = header_with_capability(1);
        validate_spirv(&m).expect("Shader capability is allowed");
    }

    #[test]
    fn rejects_buffer_device_address_capability() {
        // 5347 = PhysicalStorageBufferAddresses.
        let m = header_with_capability(5347);
        match validate_spirv(&m) {
            Err(ValidatorError::ForbiddenCapability { capability, .. }) => {
                assert_eq!(capability, 5347);
            }
            other => panic!("expected ForbiddenCapability, got {other:?}"),
        }
    }

    #[test]
    fn rejects_ray_tracing_capability() {
        let m = header_with_capability(4479); // RayTracingKHR
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::ForbiddenCapability { .. })));
    }

    #[test]
    fn rejects_physical_storage_buffer_extension() {
        let m = header_with_extension("SPV_KHR_physical_storage_buffer");
        match validate_spirv(&m) {
            Err(ValidatorError::ForbiddenExtension { name, .. }) => {
                assert_eq!(name, "SPV_KHR_physical_storage_buffer");
            }
            other => panic!("expected ForbiddenExtension, got {other:?}"),
        }
    }

    #[test]
    fn rejects_zero_word_count_instruction() {
        let mut m = minimal_header(0x0001_0000);
        m.extend_from_slice(&0u32.to_le_bytes()); // word0 = 0 ⇒ word_count = 0
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::ZeroWordCount { .. })));
    }

    #[test]
    fn rejects_truncated_instruction() {
        let mut m = minimal_header(0x0001_0000);
        // Claim word_count=4 but only provide 1 word total.
        let inst0 = (4u32 << 16) | 17u32; // OpCapability
        m.extend_from_slice(&inst0.to_le_bytes());
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::Truncated { .. })));
    }

    /// Append one instruction with given opcode and operand words.
    fn push_inst(out: &mut Vec<u8>, opcode: u16, operands: &[u32]) {
        let wc = 1 + operands.len();
        let word0 = ((wc as u32) << 16) | (opcode as u32);
        out.extend_from_slice(&word0.to_le_bytes());
        for o in operands {
            out.extend_from_slice(&o.to_le_bytes());
        }
    }

    #[test]
    fn rejects_physical_storage_buffer_storage_class() {
        // OpTypePointer = 32, operands: result_id, storage_class,
        // pointed_to. storage class 5349 = PhysicalStorageBuffer.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 32, &[1, 5349, 2]);
        match validate_spirv(&m) {
            Err(ValidatorError::ForbiddenStorageClass { storage_class, .. }) => {
                assert_eq!(storage_class, 5349);
            }
            other => panic!("expected ForbiddenStorageClass, got {other:?}"),
        }
    }

    #[test]
    fn accepts_allowed_storage_class() {
        // 0 = UniformConstant — allowed.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 32, &[1, 0, 2]);
        validate_spirv(&m).expect("UniformConstant storage class is allowed");
    }

    /// Helper: encode a bounded OpLoopMerge with `MaxIterations`
    /// annotation (bit 0x20 per SPIR-V spec §3.23) and literal
    /// `max` iterations.
    fn bounded_loop_merge(max: u32) -> [u32; 5] {
        [1, 2, 0x20, max, 0]
    }

    #[test]
    fn rejects_too_many_loops() {
        // Build MAX_LOOPS_PER_MODULE + 1 BOUNDED loops, so the loop-
        // count cap fires before the unbounded-loop check.
        let mut m = minimal_header(0x0001_0000);
        let operands = bounded_loop_merge(100);
        for _ in 0..(MAX_LOOPS_PER_MODULE + 1) {
            push_inst(&mut m, 246, &operands);
        }
        match validate_spirv(&m) {
            Err(ValidatorError::TooManyLoops { count, cap }) => {
                assert_eq!(cap, MAX_LOOPS_PER_MODULE);
                assert_eq!(count, MAX_LOOPS_PER_MODULE + 1);
            }
            other => panic!("expected TooManyLoops, got {other:?}"),
        }
    }

    #[test]
    fn accepts_loops_under_cap() {
        let mut m = minimal_header(0x0001_0000);
        let operands = bounded_loop_merge(100);
        for _ in 0..MAX_LOOPS_PER_MODULE {
            push_inst(&mut m, 246, &operands);
        }
        validate_spirv(&m).expect("at-cap bounded loops should validate");
    }

    #[test]
    fn rejects_unroll_alone_in_strict_mode() {
        // STRICT MODE: Unroll without a literal bound is rejected.
        // The toolchain's annotate step adds the literal before this
        // ever reaches the validator. See shader_annotate.rs.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &[1, 2, 0x01]); // LoopControl = Unroll
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::UnboundedLoop { .. })));
    }

    #[test]
    fn accepts_unroll_paired_with_max_iterations() {
        // Annotated form: Unroll (0x01) preserved (driver unrolls),
        // plus MaxIterations (0x20) literal (validator trusts the bound).
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &[1, 2, 0x01 | 0x20, 256, 0]);
        validate_spirv(&m).expect("Unroll + MaxIterations should validate");
    }

    #[test]
    fn rejects_dont_unroll_alone() {
        // LoopControl = 0x02 (DontUnroll, no literal). Same posture
        // as Unroll-alone: strict mode rejects until annotated.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &[1, 2, 0x02]);
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::UnboundedLoop { .. })));
    }

    #[test]
    fn accepts_dont_unroll_paired_with_max_iterations() {
        // 0x02 (DontUnroll) | 0x20 (MaxIterations) + literal.
        // Runtime loop is fine when the compiler also promises a bound.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &[1, 2, 0x02 | 0x20, 1024, 0]);
        validate_spirv(&m).expect("DontUnroll + MaxIterations should validate");
    }

    #[test]
    fn rejects_unbounded_loop() {
        // OpLoopMerge with loop_control=0 — no MinIterations/MaxIterations.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &[1, 2, 0]);
        match validate_spirv(&m) {
            Err(ValidatorError::UnboundedLoop { .. }) => {}
            other => panic!("expected UnboundedLoop, got {other:?}"),
        }
    }

    #[test]
    fn accepts_loop_with_min_iterations_annotation() {
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &bounded_loop_merge(1024));
        validate_spirv(&m).expect("loop with MinIterations should validate");
    }

    #[test]
    fn accepts_loop_with_max_iterations_annotation() {
        // 0x20 = MaxIterations per SPIR-V spec §3.23.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &[1, 2, 0x20, 1024, 0]);
        validate_spirv(&m).expect("loop with MaxIterations should validate");
    }

    #[test]
    fn rejects_loop_iterations_above_cap() {
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 246, &bounded_loop_merge(MAX_LOOP_ITERATIONS + 1));
        match validate_spirv(&m) {
            Err(ValidatorError::LoopIterationsExceedCap { annotated, cap }) => {
                assert_eq!(annotated, MAX_LOOP_ITERATIONS + 1);
                assert_eq!(cap, MAX_LOOP_ITERATIONS);
            }
            other => panic!("expected LoopIterationsExceedCap, got {other:?}"),
        }
    }

    #[test]
    fn handles_multiple_loop_control_literals_in_order() {
        // Set DependencyLength (0x008) + MinIterations (0x010), both
        // with literals. Validator must consume them in spec-bit order.
        let mut m = minimal_header(0x0001_0000);
        // operands: merge, continue, mask, dep_len_lit, min_iter_lit, padding
        push_inst(&mut m, 246, &[1, 2, 0x008 | 0x010, 4, 256, 0]);
        validate_spirv(&m).expect("ordered loop-control literals should parse");
    }

    #[test]
    fn rejects_too_many_functions() {
        // OpFunction = 54. Build MAX_FUNCTIONS_PER_MODULE + 1 of them.
        let mut m = minimal_header(0x0001_0000);
        for _ in 0..(MAX_FUNCTIONS_PER_MODULE + 1) {
            // OpFunction: result_type, result_id, function_control,
            // function_type. word_count = 5.
            push_inst(&mut m, 54, &[1, 2, 0, 3]);
        }
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::TooManyFunctions { .. })));
    }

    #[test]
    fn rejects_too_many_instructions() {
        // OpNop = 0 (one-word instruction). Build > cap.
        let mut m = minimal_header(0x0001_0000);
        for _ in 0..(MAX_INSTRUCTIONS_PER_MODULE + 1) {
            push_inst(&mut m, 0, &[]);
        }
        assert!(matches!(validate_spirv(&m), Err(ValidatorError::TooManyInstructions { .. })));
    }

    /// Real slangc 2026.8 output. Captured corpus: compute kernel
    /// with `[unroll(8)] for (uint i = 0; i < 8; ++i)`. The
    /// resulting OpLoopMerge carries `LoopControl = Unroll (0x01)`.
    /// Pinning this in source means CI catches it if a future
    /// validator change accidentally regresses the slangc path.
    const SLANGC_UNROLL_LOOP_SPV: &[u32] = &[
        0x07230203, 0x00010300, 0x00280000, 0x00000035, 0x00000000, 0x00020011, 0x00000001, 0x0003000e,
        0x00000000, 0x00000001, 0x0006000f, 0x00000005, 0x00000002, 0x6e69616d, 0x00000000, 0x0000002a,
        0x00060010, 0x00000002, 0x00000011, 0x00000001, 0x00000001, 0x00000001, 0x00030003, 0x0000000b,
        0x00000001, 0x00030005, 0x00000007, 0x00000069, 0x00030005, 0x00000007, 0x00000069, 0x00030005,
        0x00000008, 0x006d7573, 0x00030005, 0x00000008, 0x006d7573, 0x00030005, 0x0000001d, 0x006d7573,
        0x00030005, 0x00000021, 0x00000069, 0x00070005, 0x00000030, 0x74535752, 0x74637572, 0x64657275,
        0x66667542, 0x00007265, 0x00060006, 0x00000030, 0x00000000, 0x656d5f5f, 0x7265626d, 0x00000030,
        0x00040005, 0x00000033, 0x5f74756f, 0x00667562, 0x00040005, 0x00000002, 0x6e69616d, 0x00000000,
        0x00040047, 0x0000002a, 0x0000000b, 0x0000001c, 0x00040047, 0x00000031, 0x00000006, 0x00000004,
        0x00030047, 0x00000030, 0x00000003, 0x00050048, 0x00000030, 0x00000000, 0x00000023, 0x00000000,
        0x00040047, 0x00000033, 0x00000021, 0x00000000, 0x00040047, 0x00000033, 0x00000022, 0x00000000,
        0x00020013, 0x00000001, 0x00030021, 0x00000003, 0x00000001, 0x00040015, 0x00000005, 0x00000020,
        0x00000000, 0x00040020, 0x00000006, 0x00000007, 0x00000005, 0x0004002b, 0x00000005, 0x00000013,
        0x00000000, 0x00020014, 0x00000017, 0x0004002b, 0x00000005, 0x00000019, 0x00000008, 0x0004002b,
        0x00000005, 0x00000022, 0x00000001, 0x00040017, 0x00000027, 0x00000005, 0x00000003, 0x00040020,
        0x00000029, 0x00000001, 0x00000027, 0x00040015, 0x0000002c, 0x00000020, 0x00000001, 0x0004002b,
        0x0000002c, 0x0000002d, 0x00000000, 0x00040020, 0x0000002e, 0x00000002, 0x00000005, 0x0003001d,
        0x00000031, 0x00000005, 0x0003001e, 0x00000030, 0x00000031, 0x00040020, 0x00000032, 0x00000002,
        0x00000030, 0x0004003b, 0x00000029, 0x0000002a, 0x00000001, 0x0004003b, 0x00000032, 0x00000033,
        0x00000002, 0x00050036, 0x00000001, 0x00000002, 0x00000000, 0x00000003, 0x000200f8, 0x00000004,
        0x0004003b, 0x00000006, 0x00000007, 0x00000007, 0x0004003b, 0x00000006, 0x00000008, 0x00000007,
        0x0003003e, 0x00000007, 0x00000013, 0x0003003e, 0x00000008, 0x00000013, 0x000200f9, 0x00000009,
        0x000200f8, 0x00000009, 0x000400f6, 0x00000012, 0x00000010, 0x00000001, 0x000200f9, 0x0000000a,
        0x000200f8, 0x0000000a, 0x000200f9, 0x0000000b, 0x000200f8, 0x0000000b, 0x000200f9, 0x0000000c,
        0x000200f8, 0x0000000c, 0x0004003d, 0x00000005, 0x00000016, 0x00000007, 0x000500b0, 0x00000017,
        0x00000018, 0x00000016, 0x00000019, 0x000300f7, 0x0000000d, 0x00000000, 0x000400fa, 0x00000018,
        0x0000000d, 0x00000011, 0x000200f8, 0x00000011, 0x000200f9, 0x00000012, 0x000200f8, 0x0000000d,
        0x0004003d, 0x00000005, 0x0000001b, 0x00000007, 0x0004003d, 0x00000005, 0x0000001c, 0x00000008,
        0x00050080, 0x00000005, 0x0000001d, 0x0000001c, 0x0000001b, 0x000200f9, 0x0000000e, 0x000200f8,
        0x0000000e, 0x000200f9, 0x0000000f, 0x000200f8, 0x0000000f, 0x0004003d, 0x00000005, 0x00000020,
        0x00000007, 0x00050080, 0x00000005, 0x00000021, 0x00000020, 0x00000022, 0x0003003e, 0x00000007,
        0x00000021, 0x0003003e, 0x00000008, 0x0000001d, 0x000200f9, 0x00000010, 0x000200f8, 0x00000010,
        0x000200f9, 0x00000009, 0x000200f8, 0x00000012, 0x0004003d, 0x00000027, 0x00000028, 0x0000002a,
        0x00050051, 0x00000005, 0x0000002b, 0x00000028, 0x00000000, 0x00060041, 0x0000002e, 0x0000002f,
        0x00000033, 0x0000002d, 0x0000002b, 0x0004003d, 0x00000005, 0x00000034, 0x00000008, 0x0003003e,
        0x0000002f, 0x00000034, 0x000100fd, 0x00010038,
    ];

    #[test]
    fn rejects_raw_slangc_output_in_strict_mode() {
        // Real slangc 2026.8 emits LoopControl = Unroll (0x01) with
        // no literal. Strict mode rejects this. The toolchain's
        // annotate step (shader_annotate.rs) is responsible for
        // injecting MaxIterations before the bytes reach the
        // validator. CI pins this so any future relaxation is a
        // deliberate, reviewed change.
        let mut bytes = Vec::with_capacity(SLANGC_UNROLL_LOOP_SPV.len() * 4);
        for w in SLANGC_UNROLL_LOOP_SPV {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        assert_eq!(bytes.len(), 1136, "fixture should match captured slangc output");
        assert!(matches!(validate_spirv(&bytes),
                         Err(ValidatorError::UnboundedLoop { .. })),
                "strict mode must reject raw slangc Unroll-only output");
    }

    /// Build header + an OpVariable for a resource storage class
    /// with a result-id, optionally followed by Decorate instructions
    /// targeting it. Supplies `result_type=99` since we don't model
    /// the type-pointer chain.
    fn resource_variable_module(
        storage_class: u32,
        decorate_set: bool,
        decorate_binding: bool,
    ) -> Vec<u8> {
        let mut m = minimal_header(0x0001_0000);
        // OpVariable: result_type=99, result_id=42, storage_class.
        push_inst(&mut m, 59, &[99, 42, storage_class]);
        if decorate_set {
            // OpDecorate target=42 DescriptorSet=34 literal=0
            push_inst(&mut m, 71, &[42, 34, 0]);
        }
        if decorate_binding {
            // OpDecorate target=42 Binding=33 literal=0
            push_inst(&mut m, 71, &[42, 33, 0]);
        }
        m
    }

    #[test]
    fn rejects_uniform_variable_missing_descriptor_set() {
        // storage_class=2 Uniform, only Binding decorated.
        let m = resource_variable_module(2, false, true);
        match validate_spirv(&m) {
            Err(ValidatorError::UnboundResourceVariable {
                variable_id, storage_class, missing_set, missing_binding,
            }) => {
                assert_eq!(variable_id, 42);
                assert_eq!(storage_class, 2);
                assert!(missing_set);
                assert!(!missing_binding);
            }
            other => panic!("expected UnboundResourceVariable, got {other:?}"),
        }
    }

    #[test]
    fn rejects_storage_buffer_missing_binding() {
        // storage_class=12 StorageBuffer, only DescriptorSet decorated.
        let m = resource_variable_module(12, true, false);
        match validate_spirv(&m) {
            Err(ValidatorError::UnboundResourceVariable {
                missing_set, missing_binding, ..
            }) => {
                assert!(!missing_set);
                assert!(missing_binding);
            }
            other => panic!("expected UnboundResourceVariable, got {other:?}"),
        }
    }

    #[test]
    fn rejects_uniform_constant_missing_both_decorations() {
        // storage_class=0 UniformConstant, neither decoration.
        let m = resource_variable_module(0, false, false);
        match validate_spirv(&m) {
            Err(ValidatorError::UnboundResourceVariable {
                missing_set, missing_binding, ..
            }) => {
                assert!(missing_set);
                assert!(missing_binding);
            }
            other => panic!("expected UnboundResourceVariable, got {other:?}"),
        }
    }

    #[test]
    fn accepts_resource_variable_with_both_decorations() {
        for sc in [0u32, 2, 10, 11, 12] {
            let m = resource_variable_module(sc, true, true);
            validate_spirv(&m).unwrap_or_else(|e| panic!(
                "storage_class {sc} with both decorations should validate; got {e}"
            ));
        }
    }

    #[test]
    fn accepts_non_resource_variable_without_decorations() {
        // storage_class=7 Function — local variable, not a resource.
        // No decorations required.
        let m = resource_variable_module(7, false, false);
        validate_spirv(&m).expect("Function-class variable doesn't need decorations");

        // storage_class=9 PushConstant — accessed via pipeline layout,
        // not a descriptor. No decorations required either.
        let m = resource_variable_module(9, false, false);
        validate_spirv(&m).expect("PushConstant doesn't need DescriptorSet/Binding");
    }

    #[test]
    fn decorations_can_precede_variable() {
        // Real-world SPIR-V often emits all OpDecorate instructions
        // before the OpVariable they target. Test the two-pass logic.
        let mut m = minimal_header(0x0001_0000);
        // Decorations first.
        push_inst(&mut m, 71, &[42, 34, 0]); // DescriptorSet
        push_inst(&mut m, 71, &[42, 33, 0]); // Binding
        // OpVariable last.
        push_inst(&mut m, 59, &[99, 42, 12]); // StorageBuffer
        validate_spirv(&m).expect("decoration-before-variable order should validate");
    }

    /// Helper: build a module with an OpEntryPoint and optionally
    /// preceding OpCapability declarations. `name` becomes the
    /// entry-point name (length must be ≤ 4 to fit in one extra word).
    fn module_with_entry_point(
        capabilities: &[u32],
        execution_model: u32,
        name: &str,
    ) -> Vec<u8> {
        let mut m = minimal_header(0x0001_0000);
        for cap in capabilities {
            push_inst(&mut m, 17, &[*cap]);  // OpCapability
        }
        // OpEntryPoint: execution_model, function_id=99, name words.
        // SPIR-V literal string: NUL-terminated, packed LE into u32 words.
        let mut bytes = name.as_bytes().to_vec();
        bytes.push(0);
        while bytes.len() % 4 != 0 { bytes.push(0); }
        let n_words = bytes.len() / 4;
        let mut name_words = Vec::new();
        for chunk in bytes.chunks_exact(4) {
            name_words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        let mut operands = vec![execution_model, 99];
        operands.extend_from_slice(&name_words);
        let _ = n_words;
        push_inst(&mut m, 15, &operands); // OpEntryPoint
        m
    }

    #[test]
    fn rejects_vertex_entry_point_without_shader_capability() {
        // No capabilities declared, but a Vertex entry point.
        let m = module_with_entry_point(&[], 0, "main");
        match validate_spirv(&m) {
            Err(ValidatorError::EntryPointMissingCapability {
                execution_model, required_capability, ..
            }) => {
                assert_eq!(execution_model, 0);
                assert_eq!(required_capability, 1); // Shader
            }
            other => panic!("expected EntryPointMissingCapability, got {other:?}"),
        }
    }

    #[test]
    fn rejects_geometry_entry_point_without_geometry_capability() {
        // Only Shader (1) declared, but a Geometry (3) entry point.
        let m = module_with_entry_point(&[1], 3, "main");
        match validate_spirv(&m) {
            Err(ValidatorError::EntryPointMissingCapability {
                execution_model, required_capability, ..
            }) => {
                assert_eq!(execution_model, 3);
                assert_eq!(required_capability, 2); // Geometry
            }
            other => panic!("expected EntryPointMissingCapability, got {other:?}"),
        }
    }

    #[test]
    fn accepts_compute_entry_point_with_shader_capability() {
        // GLCompute (5) needs Shader (1).
        let m = module_with_entry_point(&[1], 5, "main");
        validate_spirv(&m).expect("GLCompute + Shader cap should validate");
    }

    #[test]
    fn rejects_kernel_execution_model() {
        // Kernel mode (6) is forbidden by Atrium's sandbox policy.
        // The forbidden-cap denylist also catches Kernel capability
        // independently, but Kernel model can be declared without
        // the cap (which is illegal SPIR-V anyway). We reject the
        // model at the cross-check post-pass.
        //
        // Note: declaring Kernel cap (6) would be caught by the
        // forbidden-capability check FIRST. So this test uses
        // Shader (1) cap with Kernel (6) model to exercise the
        // execution-model-only path.
        let m = module_with_entry_point(&[1], 6, "main");
        match validate_spirv(&m) {
            Err(ValidatorError::ForbiddenExecutionModel { execution_model, .. }) => {
                assert_eq!(execution_model, 6);
            }
            other => panic!("expected ForbiddenExecutionModel, got {other:?}"),
        }
    }

    #[test]
    fn rejects_kernel_capability_directly() {
        // Kernel (6) capability is on the forbidden-cap denylist.
        // This fires BEFORE the entry-point post-pass so the error
        // variant is ForbiddenCapability, not ForbiddenExecutionModel.
        let mut m = minimal_header(0x0001_0000);
        push_inst(&mut m, 17, &[6]); // OpCapability Kernel
        match validate_spirv(&m) {
            Err(ValidatorError::ForbiddenCapability { capability: 6, .. }) => {}
            other => panic!("expected ForbiddenCapability(6), got {other:?}"),
        }
    }

    #[test]
    fn accepts_module_with_no_entry_point() {
        // Library shaders / shader fragments may have no entry point;
        // they'd be linked into a real module before validation.
        // The cross-check only applies when an entry point exists.
        let m = minimal_header(0x0001_0000);
        validate_spirv(&m).expect("module with no entry point should validate");
    }

    #[test]
    fn validates_annotated_slangc_output() {
        // Round-trip: raw slangc → annotate → validate. This is the
        // pipeline atrium-pkg's install hook and bundles/*/build.sh
        // run. Proves the toolchain composition is sound.
        let mut bytes = Vec::with_capacity(SLANGC_UNROLL_LOOP_SPV.len() * 4);
        for w in SLANGC_UNROLL_LOOP_SPV {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let report = crate::shader_annotate::annotate_loop_merges(&bytes, 65536)
            .expect("annotate should accept real slangc output");
        assert_eq!(report.patched, 1, "real corpus has one OpLoopMerge");
        validate_spirv(&report.bytes).expect(
            "annotated slangc output should pass strict-mode validator"
        );
    }

    #[test]
    fn fuzz_random_bytes_never_panics() {
        // Quick fuzz: 1000 random byte buffers of varying lengths.
        // Validator must reject all (or accidentally accept if the
        // bytes happen to form a valid module), but never panic.
        for seed in 0..1000u64 {
            let mut bytes = Vec::with_capacity(256);
            let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15);
            let n = (x as usize) % 256;
            for _ in 0..n {
                x = x.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
                bytes.push((x >> 56) as u8);
            }
            let _ = validate_spirv(&bytes); // must not panic
        }
    }

    /// Phase 2.4 — long-tail validator cross-check against upstream
    /// SPIRV-Tools. Gated behind the `spirv-tools-cross-check` cargo
    /// feature so default `cargo test` doesn't require libSPIRV-Tools.
    ///
    /// Run with:
    ///   cargo test --features spirv-tools-cross-check
    ///
    /// The contract we're enforcing: SPIR-V the hand validator
    /// ACCEPTS must also be accepted by upstream SPIRV-Tools. If
    /// spirv-tools rejects something we accept, it's a real bug —
    /// either our test fixture is malformed or our validator is
    /// missing a structural check.
    ///
    /// We don't test the inverse direction (things we reject that
    /// spirv-tools accepts): our rules are stricter on purpose
    /// (forbidden capabilities, bounded loops, etc.), and we expect
    /// spirv-tools to accept what we deliberately ban.
    #[cfg(feature = "spirv-tools-cross-check")]
    #[test]
    fn slangc_unroll_loop_passes_spirv_tools_validator() {
        use spirv_tools::val::{self, Validator};
        use spirv_tools::TargetEnv;

        // SLANGC_UNROLL_LOOP_SPV is a real slangc 2026.8 compute
        // shader. Our hand validator accepts it after annotation;
        // upstream spirv-tools must also accept it (it's
        // structurally well-formed SPIR-V, just with an Unroll
        // LoopControl that we strict-mode-reject pre-annotation).
        let validator = val::create(Some(TargetEnv::Vulkan_1_2));
        let result = validator.validate(
            SLANGC_UNROLL_LOOP_SPV, None,
        );
        assert!(
            result.is_ok(),
            "spirv-tools rejected a fixture our validator accepts: {:?}",
            result.err(),
        );
    }
}
