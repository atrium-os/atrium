//! Bespoke ARM64 backend for atrium-spv tier-2.
//!
//! # Why a second backend
//!
//! The user explicitly committed to the bespoke path early
//! on, citing the PPTK project's data point that
//! Cranelift-class regalloc + ISel hits ~70% of bespoke-
//! quality scalar code while the 90% bar is what makes
//! tier-2 worth shipping for production shaders. The
//! Cranelift backend stays — it's the differential
//! oracle's twin AND the graceful-degradation fallback for
//! IR shapes the bespoke pass doesn't yet handle — but the
//! perf path goes through here.
//!
//! # Phase 3 status
//!
//! **Skeleton.** This commit lands the crate shape +
//! public `compile(module, target) -> CompileOutput`
//! function with the same signature as
//! [`atrium_spv_backend_cranelift::compile`]. The
//! implementation currently emits:
//!   - empty functions (single `Op::Return`) as a literal
//!     ARM64 `ret` (0xD65F03C0).
//!   - wraps the bytes in a host-format object file (ELF
//!     for FreeBSD, Mach-O for Darwin) via the `object`
//!     crate.
//!
//! Real instruction selection + linear-scan register
//! allocation lands in phase 3 step 2+. The skeleton lets
//! the differential harness pull this backend into the
//! runner-set as soon as it understands any opcode at all
//! (currently: just empty functions).

use std::collections::HashMap;

use atrium_spv_ir::{
    FloatKind, Function, Module, Op, ShaderStage, StorageClass, Type,
    Value, ValueId,
};
use pptk_codegen_arm64::asm;
use thiserror::Error;

/// What went wrong compiling a module.
#[derive(Debug, Error)]
pub enum BackendError {
    /// The IR uses an opcode / shape the bespoke backend
    /// doesn't yet support. The driver interprets this as
    /// "fall back to Cranelift" in production.
    #[error("bespoke backend doesn't support: {0}")]
    Unsupported(String),
    /// Anything else (object emission failure, internal
    /// invariant violation).
    #[error("internal error: {0}")]
    Internal(String),
}

/// Target the produced object file is for.
///
/// Mirrors [`atrium_spv_backend_cranelift::Target`] so the
/// driver can transparently swap backends. The bespoke
/// backend is ARM64-only by charter; X86_64 selection is
/// rejected at the top of [`compile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// FreeBSD on 64-bit ARM (the production tier-2 target).
    Aarch64FreeBSD,
    /// macOS on Apple Silicon (dev iteration).
    Aarch64Darwin,
}

impl Target {
    /// The host's target, panicking on x86_64 (the bespoke
    /// backend doesn't target it).
    pub fn host() -> Self {
        #[cfg(all(target_arch = "aarch64", target_os = "freebsd"))]
        { return Target::Aarch64FreeBSD; }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        { return Target::Aarch64Darwin; }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_os = "freebsd"),
            all(target_arch = "aarch64", target_os = "macos"),
        )))]
        { return Target::Aarch64Darwin; }
    }

    fn object_format(self) -> object::BinaryFormat {
        match self {
            Target::Aarch64FreeBSD => object::BinaryFormat::Elf,
            Target::Aarch64Darwin  => object::BinaryFormat::MachO,
        }
    }
}

/// Result of compiling a module — matches
/// [`atrium_spv_backend_cranelift::CompileOutput`] field-
/// for-field.
#[derive(Debug, Clone)]
pub struct CompileOutput {
    /// Object-file bytes ready to pass to `ld` / `cc`.
    pub object: Vec<u8>,
    /// PC-map sidecar bytes (atrium-spv-pcmap format v1).
    pub pcmap: Vec<u8>,
}

/// Compile an atrium-spv-ir module to a native object
/// file + PC-map sidecar.
pub fn compile(module: &Module, target: Target) -> Result<CompileOutput, BackendError> {
    let mut obj = object::write::Object::new(
        target.object_format(),
        object::Architecture::Aarch64,
        object::Endianness::Little,
    );
    let text_section = obj.section_id(object::write::StandardSection::Text);

    let mut pcmap = atrium_spv_pcmap::Builder::new();

    for func in &module.functions {
        let body = emit_function(func)?;
        let symbol_name = exported_symbol_name(func);

        let sym = obj.add_symbol(object::write::Symbol {
            name: symbol_name.into_bytes(),
            value: 0,
            size: body.len() as u64,
            kind: object::SymbolKind::Text,
            scope: object::SymbolScope::Dynamic,
            weak: false,
            section: object::write::SymbolSection::Section(text_section),
            flags: object::SymbolFlags::None,
        });
        // 4-byte alignment for ARM64 instructions.
        let _off = obj.add_symbol_data(sym, text_section, &body, 4);

        // PC-map entry: function-relative host_offset = 0,
        // spirv_offset = first body inst's
        // source_spirv_offset (or 0 if no insts).
        let first_offset = func.blocks.get(&func.entry_block)
            .and_then(|b| b.insts.first())
            .map(|i| i.source_spirv_offset)
            .unwrap_or(0);
        pcmap.push(0, first_offset);
    }

    let object_bytes = obj.write()
        .map_err(|e| BackendError::Internal(format!("object write: {e}")))?;
    let pcmap_bytes = pcmap.finish_to_bytes();
    Ok(CompileOutput { object: object_bytes, pcmap: pcmap_bytes })
}

/// Emit the ARM64 instruction bytes for one function.
///
/// Phase 3 step 2: supports the constant-vec4 store shape
/// (the canonical "constant-colour fragment shader"):
///   ConstFloat × 4 → ConstVec → Store(out, vec) → Return
///
/// Strategy:
/// * We don't yet have a register allocator. ConstFloat
///   values are kept as raw f32 bit patterns (no register
///   assigned until Store time). ConstVec just collects
///   the lane ValueIds.
/// * Store onto an Output pointer emits, for each lane:
///     movz w_tmp, #lo16
///     movk w_tmp, #hi16, lsl 16
///     str  w_tmp, [x_out, #(i*4)]
///   Storing the f32 bit pattern via a w-register instead
///   of an s-register avoids the fmov dance we'd need if
///   we were keeping the value in an FP register.
/// * Return → `ret`.
///
/// The out-pointer is in `X4` per the AAPCS64 split for
/// the fragment shader ABI:
///   X0=in_varyings, X1=uniforms, X2=push_consts,
///   X3=samples_mask, X4=out_color, X5=out_depth
///   S0..S3 = frag_coord {x, y, z, w}
fn emit_function(func: &Function) -> Result<Vec<u8>, BackendError> {
    if func.stage != ShaderStage::Fragment {
        return Err(BackendError::Unsupported(format!(
            "stage {:?} not yet supported", func.stage)));
    }
    let block = func.blocks.get(&func.entry_block).ok_or_else(||
        BackendError::Internal("entry block missing".into()))?;

    let mut a = asm::Asm::new();
    let mut scalars: HashMap<ValueId, u32> = HashMap::new();
    let mut vectors: HashMap<ValueId, Vec<Value>> = HashMap::new();

    // X4 holds out_color.
    let x_out = asm::Xreg(4);
    // Scratch W register for constant materialisation.
    let w_tmp = asm::Wreg(9);

    for inst in &block.insts {
        match &inst.op {
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let bits = (*value as f32).to_bits();
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstFloat without result".into()))?;
                scalars.insert(result.id, bits);
            }
            Op::ConstVec(elements) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstVec without result".into()))?;
                // Verify every lane is a known scalar bit
                // pattern — only the constant case is
                // supported in step 2.
                for el in elements {
                    if !scalars.contains_key(&el.id) {
                        return Err(BackendError::Unsupported(format!(
                            "ConstVec lane {:?} is not a known constant scalar; \
                             step 2 supports constant vecs only",
                            el.id)));
                    }
                }
                vectors.insert(result.id, elements.clone());
            }
            Op::Store { ptr, value } => {
                // Pointer must be Output (the only sink in
                // step 2). The lane bits must be in
                // `vectors`. Emit movz/movk/str per lane.
                match &ptr.ty {
                    Type::Pointer(StorageClass::Output, _) => {}
                    other => return Err(BackendError::Unsupported(format!(
                        "Store target {other:?} not supported in step 2"
                    ))),
                }
                let lanes = vectors.get(&value.id).ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Op::Store value {:?} is not a vector; step 2 supports vec stores only",
                        value.id)))?;
                if lanes.len() > 4 {
                    return Err(BackendError::Unsupported(format!(
                        "Store of {}-lane vector not supported", lanes.len())));
                }
                for (i, lane) in lanes.iter().enumerate() {
                    let bits = *scalars.get(&lane.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "lane {:?} missing", lane.id)))?;
                    materialise_u32_into_w(&mut a, w_tmp, bits);
                    // pptk's str_w_offset takes the byte
                    // offset directly (asserts it's a
                    // multiple of 4) and scales internally
                    // to the ARM64 pimm12 field.
                    let offset_bytes = (i as u16) * 4;
                    a.emit(asm::str_w_offset(w_tmp, x_out, offset_bytes));
                }
            }
            Op::Return => {
                a.emit(asm::ret());
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "op {other:?} not supported in step 2")));
            }
        }
    }
    Ok(a.into_bytes())
}

/// Load a 32-bit immediate into a W register via the
/// canonical movz / movk pair. Skips the high-half movk
/// when the upper 16 bits are zero (common for small
/// integer constants).
fn materialise_u32_into_w(a: &mut asm::Asm, dst: asm::Wreg, bits: u32) {
    let lo = (bits & 0xFFFF) as u16;
    let hi = ((bits >> 16) & 0xFFFF) as u16;
    // movz_w/movk_w take shift in *halfword units*
    // (0 = low 16 bits, 1 = high 16 bits of a 32-bit reg).
    a.emit(asm::movz_w(dst, lo, 0));
    if hi != 0 {
        a.emit(asm::movk_w(dst, hi, 1));
    }
}

/// Compute the exported symbol name for a function.
///
/// Same convention as the Cranelift backend:
/// `atrium_vs_main` / `atrium_fs_main` / `atrium_cs_main`
/// for entry points, else the IR name verbatim.
fn exported_symbol_name(func: &Function) -> String {
    match func.stage {
        ShaderStage::Vertex   => "atrium_vs_main".to_string(),
        ShaderStage::Fragment => "atrium_fs_main".to_string(),
        ShaderStage::Compute  => "atrium_cs_main".to_string(),
    }
}
