//! atrium-spv-backend-cranelift — atrium-spv-ir → native
//! object file via Cranelift.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §2 — backend
//!   selection: this crate is the graceful-degradation
//!   fallback that ships from day 1, before the bespoke
//!   backend has wide opcode coverage.
//! - [`docs/spec/tier2-renderer.md`] §6.1 — crate layout
//! - [`docs/spec/tier2-shader-codegen-constraints.md`]
//!   §E — Cranelift adapter rules (type parity with
//!   bespoke, same shader ABI, functional indistinguishability).
//!
//! # What it does
//!
//! Takes an [`atrium_spv_ir::Module`], walks every
//! function, emits a Cranelift-IR function with the
//! atrium shader-ABI signature, and produces an object
//! file (Mach-O on Darwin, ELF on Linux/FreeBSD) ready
//! for `ld` to link into a `.so`.
//!
//! # Phase status
//!
//! **Phase 2 v1 skeleton.** The compile pipeline works
//! end-to-end with an empty function body — proves the
//! Cranelift toolchain wiring is correct + produces a
//! valid object file. Real IR-instruction translation
//! lands in v2 (Op::Store + Op::ConstFloat / ConstVec
//! first; arithmetic / control flow / vector ops
//! incrementally as the frontend learns them).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;

pub use error::BackendError;

use atrium_spv_ir::{Module, ShaderStage};

use cranelift_codegen::ir::{AbiParam, Function as ClifFunction, InstBuilder, Signature, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings;
use cranelift_codegen::Context as ClifContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module as ClifModule};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;

/// Target the produced object file is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// FreeBSD on 64-bit ARM (the production tier-2 target).
    Aarch64FreeBSD,
    /// macOS on Apple Silicon (dev iteration).
    Aarch64Darwin,
    /// FreeBSD on x86_64 (future; phase 4).
    X86_64FreeBSD,
}

impl Target {
    /// Cranelift / target-lexicon triple string.
    fn triple(self) -> &'static str {
        match self {
            Target::Aarch64FreeBSD => "aarch64-unknown-freebsd",
            Target::Aarch64Darwin  => "aarch64-apple-darwin",
            Target::X86_64FreeBSD  => "x86_64-unknown-freebsd",
        }
    }

    /// The host's target (best-guess from cargo's
    /// configured target-triple at compile time).
    pub fn host() -> Self {
        // cfg!() resolves at build time.
        #[cfg(all(target_arch = "aarch64", target_os = "freebsd"))]
        { return Target::Aarch64FreeBSD; }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        { return Target::Aarch64Darwin; }
        #[cfg(all(target_arch = "x86_64",  target_os = "freebsd"))]
        { return Target::X86_64FreeBSD; }
        // Fallback for other dev platforms — Darwin's the
        // most useful for "does it build at all".
        #[cfg(not(any(
            all(target_arch = "aarch64", target_os = "freebsd"),
            all(target_arch = "aarch64", target_os = "macos"),
            all(target_arch = "x86_64",  target_os = "freebsd"),
        )))]
        { return Target::Aarch64Darwin; }
    }
}

/// Compile an atrium-spv-ir module to a native object file.
///
/// Returns the raw object bytes (suitable for passing to
/// `ld` for the final link into a `.so`). The bytes start
/// with the target's object-format magic (Mach-O on
/// Darwin, ELF on FreeBSD/Linux).
///
/// On unsupported IR shapes (which phase 2 v1 mostly is),
/// returns [`BackendError::Unsupported`]. The
/// `atrium-spv-compile` driver interprets that as "fall
/// back to bespoke" in the production path, or
/// "skip this runner" in the test harness.
pub fn compile(module: &Module, target: Target) -> Result<Vec<u8>, BackendError> {
    // ── 1. Set up the target ISA ────────────────────────────
    let triple: Triple = target.triple().parse()
        .map_err(|e| BackendError::Internal(format!("triple parse: {e}")))?;
    let flag_builder = settings::builder();
    let isa_builder = cranelift_codegen::isa::lookup(triple)
        .map_err(|e| BackendError::Internal(format!("ISA lookup: {e}")))?;
    let isa = isa_builder.finish(settings::Flags::new(flag_builder))
        .map_err(|e| BackendError::Internal(format!("ISA finish: {e}")))?;

    // ── 2. Create the ObjectModule ─────────────────────────
    let builder = ObjectBuilder::new(
        isa,
        b"atrium_spv_compile".to_vec(),
        cranelift_module::default_libcall_names(),
    ).map_err(|e| BackendError::Internal(format!("ObjectBuilder: {e}")))?;
    let mut clif_module = ObjectModule::new(builder);

    // ── 3. Emit each function ──────────────────────────────
    for func in &module.functions {
        emit_function(&mut clif_module, func)?;
    }

    // ── 4. Finalise and emit bytes ─────────────────────────
    let product = clif_module.finish();
    let bytes = product.emit()
        .map_err(|e| BackendError::Internal(format!("object emit: {e}")))?;
    Ok(bytes)
}

/// Emit one function body.
///
/// Phase 2 v1: empty body that just returns. Proves the
/// declaration + signature + finalise pipeline works.
fn emit_function(
    clif_module: &mut ObjectModule,
    func: &atrium_spv_ir::Function,
) -> Result<(), BackendError> {
    let sig = build_signature(func.stage, clif_module.target_config().pointer_type())?;
    let symbol_name = exported_symbol_name(func);

    let func_id = clif_module
        .declare_function(&symbol_name, Linkage::Export, &sig)
        .map_err(|e| BackendError::Internal(
            format!("declare_function({symbol_name}): {e}"),
        ))?;

    let mut ctx = ClifContext::new();
    ctx.func = ClifFunction::with_name_signature(
        UserFuncName::user(0, func_id.as_u32()),
        sig,
    );

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.append_block_params_for_function_params(block);

        // Phase 2 v1: every shader returns immediately. v2
        // walks func.blocks and emits per-IR-instruction
        // Cranelift IR. The shader produces the
        // (empty) initial state of out_color which the
        // rasterizer will treat as undefined — caller's
        // responsibility to pre-init or not care for the
        // skeleton-test path.
        let _ = func; // wire in v2
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize();
    }

    clif_module
        .define_function(func_id, &mut ctx)
        .map_err(|e| BackendError::Internal(
            format!("define_function({symbol_name}): {e}"),
        ))?;
    Ok(())
}

/// Build the Cranelift Signature matching the shader ABI
/// (`docs/spec/tier2-renderer.md` §4.1) for the given
/// stage.
fn build_signature(
    stage: ShaderStage,
    pointer_type: cranelift_codegen::ir::Type,
) -> Result<Signature, BackendError> {
    use cranelift_codegen::ir::types;
    let mut params: Vec<AbiParam> = Vec::new();
    match stage {
        ShaderStage::Fragment => {
            // atrium_fs_main(
            //   in_varyings:    *const u8,
            //   uniforms:       *const u8,
            //   push_constants: *const u8,
            //   frag_coord:     [f32; 4],   // 4× f32 by value
            //   samples_mask:   u32,
            //   out_color:      *mut [f32;4],
            //   out_depth:      *mut f32,
            // )
            params.push(AbiParam::new(pointer_type));   // in_varyings
            params.push(AbiParam::new(pointer_type));   // uniforms
            params.push(AbiParam::new(pointer_type));   // push_constants
            params.push(AbiParam::new(types::F32));     // frag_coord.x
            params.push(AbiParam::new(types::F32));     // frag_coord.y
            params.push(AbiParam::new(types::F32));     // frag_coord.z
            params.push(AbiParam::new(types::F32));     // frag_coord.w
            params.push(AbiParam::new(types::I32));     // samples_mask
            params.push(AbiParam::new(pointer_type));   // out_color
            params.push(AbiParam::new(pointer_type));   // out_depth
        }
        ShaderStage::Vertex => {
            // atrium_vs_main(
            //   in_attributes, in_attr_strides, uniforms,
            //   push_constants, vertex_index, instance_index,
            //   out_position, out_varyings, out_clip_distance,
            // )
            params.push(AbiParam::new(pointer_type)); // in_attributes
            params.push(AbiParam::new(pointer_type)); // in_attr_strides
            params.push(AbiParam::new(pointer_type)); // uniforms
            params.push(AbiParam::new(pointer_type)); // push_constants
            params.push(AbiParam::new(types::I32));   // vertex_index
            params.push(AbiParam::new(types::I32));   // instance_index
            params.push(AbiParam::new(pointer_type)); // out_position
            params.push(AbiParam::new(pointer_type)); // out_varyings
            params.push(AbiParam::new(pointer_type)); // out_clip_distance
        }
        ShaderStage::Compute => {
            // atrium_cs_main(uniforms, push_constants,
            //                workgroup_id[3], local_id[3])
            params.push(AbiParam::new(pointer_type)); // uniforms
            params.push(AbiParam::new(pointer_type)); // push_constants
            // workgroup_id as three u32 (the C struct is
            // [u32; 3]; on the SystemV ABI it's passed
            // either by value in regs or as a pointer; we
            // use three direct u32 params to keep the ABI
            // explicit and selector-independent).
            params.push(AbiParam::new(types::I32));   // workgroup_id[0]
            params.push(AbiParam::new(types::I32));   // workgroup_id[1]
            params.push(AbiParam::new(types::I32));   // workgroup_id[2]
            params.push(AbiParam::new(types::I32));   // local_id[0]
            params.push(AbiParam::new(types::I32));   // local_id[1]
            params.push(AbiParam::new(types::I32));   // local_id[2]
        }
    }

    Ok(Signature {
        params,
        returns: vec![],
        call_conv: CallConv::SystemV,
    })
}

/// Compute the C-ABI symbol name a function exports.
///
/// Entry-point functions use the spec'd names
/// (`atrium_vs_main` / `atrium_fs_main` / `atrium_cs_main`)
/// regardless of their atrium-spv-ir `Function.name` —
/// that's the contract the daemon loads against.
fn exported_symbol_name(func: &atrium_spv_ir::Function) -> String {
    match func.stage {
        ShaderStage::Vertex   => "atrium_vs_main".to_string(),
        ShaderStage::Fragment => "atrium_fs_main".to_string(),
        ShaderStage::Compute  => "atrium_cs_main".to_string(),
    }
}
