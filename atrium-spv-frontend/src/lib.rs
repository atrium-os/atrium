//! atrium-spv-frontend — SPIR-V → atrium-spv-ir.
//!
//! Parses SPIR-V via `rspirv::dr`, walks the module, and
//! produces an [`atrium_spv_ir::Module`] suitable for both
//! production backends to consume.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §6.1 — crate layout
//! - [`docs/spec/tier2-shader-codegen-constraints.md`] §A —
//!   frontend invariants (reject unstructured CFGs early,
//!   preserve SPIR-V source offsets, validate capabilities,
//!   one-shot structured-CFG recovery)
//!
//! # Phase status
//!
//! **Phase 1 v1.** Implements the narrow path the
//! phase-0 v0c interpreter handles: single-block fragment
//! shaders that store a constant `vec4<f32>` to an Output
//! variable and return. Sufficient to validate the
//! frontend-side architecture + integrate with the
//! Cranelift backend's first end-to-end demo. Opcode
//! coverage widens iteratively from here.
//!
//! Unsupported constructs return
//! [`FrontendError::Unsupported`] with a human-readable
//! description, which `atrium-spv-compile` translates to
//! `VK_ERROR_INVALID_SHADER_NV` for the app.
//!
//! # Architecture
//!
//! ```text
//!     SPIR-V bytes
//!          │
//!          ▼  rspirv::dr::Loader
//!     rspirv::dr::Module
//!          │
//!          ▼  Frontend::translate (this crate)
//!     atrium_spv_ir::Module
//! ```
//!
//! The frontend itself is split into:
//!
//! - [`types`]: SPIR-V type ids → [`atrium_spv_ir::Type`]
//! - [`constants`]: SPIR-V constant ids → IR constant ops
//! - [`interface`]: walk entry-points + variables to
//!   populate `Module::entry_points` + `Module::uniforms`
//!   + per-stage interface
//! - [`functions`]: SPIR-V function bodies → IR functions
//! - [`cfg`]: structured-CFG recovery (stub in v1; full
//!   implementation in v2)
//! - [`error`]: the [`FrontendError`] enum

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cfg;
pub mod constants;
pub mod error;
pub mod functions;
pub mod interface;
pub mod offsets;
pub mod types;

pub use error::FrontendError;
pub use offsets::OffsetTable;

use atrium_spv_ir::Module;
use rspirv::dr;

/// Top-level translation entry point.
///
/// Takes SPIR-V bytes, produces an
/// [`atrium_spv_ir::Module`]. Validates SPIR-V structural
/// invariants along the way; rejects unstructured CFGs
/// per constraint A1.
/// Per-SpecId override values supplied by the host (the
/// SPIR-V-side mirror of `VkSpecializationInfo`).  Each entry
/// maps the SPIR-V `SpecId` decoration's integer constant to
/// the raw 32-bit override value.  Boolean spec constants
/// interpret a non-zero override as `true`; i32 / u32 / f32
/// spec constants use the bit pattern directly (so an f32
/// override is `f32::to_bits(v)`).  Spec constants without an
/// override entry retain their SPIR-V-declared default.
pub type SpecOverrides = std::collections::HashMap<u32, u32>;

/// Translate SPIR-V to the IR using the SPIR-V-declared
/// default values for every spec constant.  Convenience
/// wrapper around [`translate_with_spec_overrides`].
pub fn translate(spirv: &[u8]) -> Result<Module, FrontendError> {
    translate_with_spec_overrides(spirv, &SpecOverrides::default())
}

/// Translate SPIR-V to the IR, applying host-supplied
/// spec-constant overrides.  `overrides` maps a SPIR-V
/// `SpecId` (the integer literal of the `Decoration::SpecId`
/// annotation on an `OpSpecConstant*`) to its replacement
/// 32-bit value.  Unmatched spec constants keep their
/// declared defaults.
pub fn translate_with_spec_overrides(
    spirv: &[u8],
    overrides: &SpecOverrides,
) -> Result<Module, FrontendError> {
    // ── 0. Build the byte-offset table for source-position
    //       preservation through the IR (constraint A2).
    let offsets = OffsetTable::build(spirv)?;

    // ── 1. Parse via rspirv. ────────────────────────────────
    let mut loader = dr::Loader::new();
    rspirv::binary::parse_bytes(spirv, &mut loader)
        .map_err(|e| FrontendError::ParseFailed(format!("{e:?}")))?;
    let rspirv_module = loader.module();

    // ── 2. Validate declared capabilities ───────────────────
    //
    // Per constraint A3, reject capabilities outside our
    // supported set. Phase 1 v1 only handles Shader (the
    // baseline Vulkan capability).
    for cap_inst in &rspirv_module.capabilities {
        if let Some(rspirv::dr::Operand::Capability(cap)) = cap_inst.operands.first() {
            use rspirv::spirv::Capability as C;
            let accepted = matches!(*cap,
                C::Shader
                // Subgroup ops lower trivially at subgroupSize=1.
                | C::GroupNonUniform
                | C::GroupNonUniformVote
                | C::GroupNonUniformArithmetic
                | C::GroupNonUniformBallot
                | C::GroupNonUniformShuffle
                | C::GroupNonUniformShuffleRelative
                | C::GroupNonUniformClustered
                | C::GroupNonUniformQuad
                // textureQueryLevels / textureSize etc.
                | C::ImageQuery
                // dFdxFine / dFdyCoarse etc. -- lowered to
                // zero (no quad dispatch), Arc 33.
                | C::DerivativeControl
                // OpDemoteToHelperInvocation / OpIsHelper-
                // InvocationEXT, both no-ops since Tier-2's
                // serial dispatcher has no helper concept
                // (Arc 65).
                | C::DemoteToHelperInvocation
                // Geometry / Tessellation are the SPIR-V-mandated
                // gate for reading `gl_PrimitiveId` in a fragment
                // shader (the builtin's "valid via" capability
                // list).  We don't implement geometry /
                // tessellation *stages* -- an OpEntryPoint with
                // those execution models still fails at stage
                // handling -- but accepting the capability lets a
                // fragment shader that reads gl_PrimitiveId
                // (which glslang emits with OpCapability Geometry)
                // through to the rasterizer, which supplies the
                // per-primitive index.
                | C::Geometry
                | C::Tessellation);
            if !accepted {
                return Err(FrontendError::Unsupported(format!(
                    "capability {cap:?} not supported in phase-1 v1",
                )));
            }
        }
    }

    // ── 3. Index types + constants + variables ──────────────
    let type_ctx = types::TypeContext::build(&rspirv_module)?;
    // Resolve VkSpecializationInfo-style overrides: walk
    // OpDecorate * SpecId N to build a result_id -> override
    // map, then feed it into the constants pass so each
    // OpSpecConstant* sees the host-supplied value (if any)
    // in place of its SPIR-V-declared default.
    let mut spec_id_overrides: std::collections::HashMap<rspirv::spirv::Word, u32>
        = std::collections::HashMap::new();
    for inst in &rspirv_module.annotations {
        if inst.class.opcode != rspirv::spirv::Op::Decorate { continue; }
        let target = match inst.operands.first() {
            Some(rspirv::dr::Operand::IdRef(id)) => *id,
            _ => continue,
        };
        let kind = match inst.operands.get(1) {
            Some(rspirv::dr::Operand::Decoration(d)) => *d,
            _ => continue,
        };
        if kind != rspirv::spirv::Decoration::SpecId { continue; }
        let spec_id = match inst.operands.get(2) {
            Some(rspirv::dr::Operand::LiteralBit32(v)) => *v,
            _ => continue,
        };
        if let Some(value) = overrides.get(&spec_id).copied() {
            spec_id_overrides.insert(target, value);
        }
    }
    let const_ctx = constants::ConstantContext::build_with_spec_overrides(
        &rspirv_module, &type_ctx, &spec_id_overrides)?;
    let iface_ctx = interface::InterfaceContext::build_with_constants(
        &rspirv_module, &type_ctx, Some(&const_ctx),
    )?;

    // Compute the OffsetTable index of each function's
    // OpFunction instruction. Module instructions appear
    // in this source-byte order:
    //   capabilities, extensions, ext_inst_imports,
    //   memory_model, entry_points, execution_modes,
    //   debug_string_source, debug_names,
    //   debug_module_processed, annotations,
    //   types_global_values,
    //   then each function (def, parameters, blocks
    //     (label + instructions), end).
    let pre_function_count =
          rspirv_module.capabilities.len()
        + rspirv_module.extensions.len()
        + rspirv_module.ext_inst_imports.len()
        + rspirv_module.memory_model.iter().count()
        + rspirv_module.entry_points.len()
        + rspirv_module.execution_modes.len()
        + rspirv_module.debug_string_source.len()
        + rspirv_module.debug_names.len()
        + rspirv_module.debug_module_processed.len()
        + rspirv_module.annotations.len()
        + rspirv_module.types_global_values.len();
    let mut function_start_indices = Vec::with_capacity(rspirv_module.functions.len());
    let mut running = pre_function_count;
    for func in &rspirv_module.functions {
        function_start_indices.push(running);
        // OpFunction itself + parameters + each block's
        // (OpLabel + instructions) + OpFunctionEnd.
        running += 1; // OpFunction
        running += func.parameters.len();
        for block in &func.blocks {
            running += block.label.iter().count();
            running += block.instructions.len();
        }
        running += func.end.iter().count();
    }

    // ── 4. Translate functions ──────────────────────────────
    let functions = functions::translate_all(
        &rspirv_module,
        &type_ctx,
        &const_ctx,
        &iface_ctx,
        &offsets,
        &function_start_indices,
    )?;

    // ── 5. Assemble the Module ──────────────────────────────
    //
    // Patch entry_point.function_index now that the function
    // vector is laid out. The interface pass left them as
    // 0 placeholders.
    let entry_points = functions::patch_entry_point_indices(
        &rspirv_module, &iface_ctx,
    );
    let module = Module {
        functions,
        entry_points,
        uniforms: iface_ctx.uniforms,
        push_constants_size: iface_ctx.push_constants_size,
        vertex_inputs: iface_ctx.vertex_inputs,
        varyings: iface_ctx.varyings,
    };
    Ok(module)
}

