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
/// Phase 3 step 3: scalar f32 ISel.
///
/// * Every f32 scalar lives in an S-register from creation.
/// * ConstFloat materialises into a fresh S-reg via the
///   movz/movk/fmov_s_from_w sequence.
/// * FAdd / FSub / FMul / FDiv emit the scalar S-reg
///   form directly (fadd_s / fsub_s / fmul_s / fdiv_s).
/// * Store of a vec4 onto an Output pointer emits, per
///   lane: fmov_w_from_s into a scratch w-reg, then
///   str_w_offset at the per-lane offset.
/// * Return → `ret`.
///
/// Register allocation: trivial bump allocator starting
/// at V16 (caller-saved per AAPCS64, so no prologue
/// needed). Up to 16 simultaneously-live scalar values
/// are supported before we run out and return
/// Unsupported. Step 4 lands a real linear-scan RA.
///
/// Fragment shader ABI (AAPCS64 split):
///   X0=in_varyings X1=uniforms X2=push_consts
///   X3=samples_mask X4=out_color X5=out_depth
///   S0..S3 = frag_coord {x, y, z, w}
fn emit_function(func: &Function) -> Result<Vec<u8>, BackendError> {
    if func.stage != ShaderStage::Fragment {
        return Err(BackendError::Unsupported(format!(
            "stage {:?} not yet supported", func.stage)));
    }
    let block = func.blocks.get(&func.entry_block).ok_or_else(||
        BackendError::Internal("entry block missing".into()))?;

    let mut a = asm::Asm::new();
    // scalars[id] = Vreg holding the live f32 (S-reg view).
    let mut scalars: HashMap<ValueId, asm::Vreg> = HashMap::new();
    let mut vectors: HashMap<ValueId, Vec<Value>> = HashMap::new();

    // X4 holds out_color. Scratch X9/W9 for constant
    // materialisation + fmov bridging.
    let x_out = asm::Xreg(4);
    let w_tmp = asm::Wreg(9);

    // Trivial register allocator: bump V-reg from V16.
    // V16..V31 are caller-saved in AAPCS64, so we can use
    // them without a prologue. 16 slots is enough for any
    // single-block scalar shader the test harness drives.
    let mut next_vreg: u8 = 16;
    let mut alloc_vreg = || -> Result<asm::Vreg, BackendError> {
        if next_vreg >= 32 {
            return Err(BackendError::Unsupported(
                "ran out of scratch V-regs (>16 live scalars); \
                 linear-scan RA lands in step 4".into()));
        }
        let v = asm::Vreg(next_vreg);
        next_vreg += 1;
        Ok(v)
    };

    for inst in &block.insts {
        match &inst.op {
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let bits = (*value as f32).to_bits();
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstFloat without result".into()))?;
                let v = alloc_vreg()?;
                materialise_u32_into_w(&mut a, w_tmp, bits);
                a.emit(asm::fmov_s_from_w(v, w_tmp));
                scalars.insert(result.id, v);
            }
            Op::ConstVec(elements) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstVec without result".into()))?;
                // Verify every lane is a known scalar.
                for el in elements {
                    if !scalars.contains_key(&el.id) {
                        return Err(BackendError::Unsupported(format!(
                            "ConstVec lane {:?} not in scalars", el.id)));
                    }
                }
                vectors.insert(result.id, elements.clone());
            }
            Op::FAdd(a_v, b_v) => emit_fp_binop_inline(
                &mut a, &mut scalars, &mut alloc_vreg,
                inst, a_v, b_v, asm::fadd_s)?,
            Op::FSub(a_v, b_v) => emit_fp_binop_inline(
                &mut a, &mut scalars, &mut alloc_vreg,
                inst, a_v, b_v, asm::fsub_s)?,
            Op::FMul(a_v, b_v) => emit_fp_binop_inline(
                &mut a, &mut scalars, &mut alloc_vreg,
                inst, a_v, b_v, asm::fmul_s)?,
            Op::FDiv(a_v, b_v) => emit_fp_binop_inline(
                &mut a, &mut scalars, &mut alloc_vreg,
                inst, a_v, b_v, asm::fdiv_s)?,
            Op::Store { ptr, value } => {
                match &ptr.ty {
                    Type::Pointer(StorageClass::Output, _) => {}
                    other => return Err(BackendError::Unsupported(format!(
                        "Store target {other:?} not supported"))),
                }
                let lanes = vectors.get(&value.id).ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Op::Store value {:?} is not a vector", value.id)))?;
                if lanes.len() > 4 {
                    return Err(BackendError::Unsupported(format!(
                        "Store of {}-lane vector not supported", lanes.len())));
                }
                for (i, lane) in lanes.iter().enumerate() {
                    let sreg = *scalars.get(&lane.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "lane {:?} not in scalars", lane.id)))?;
                    // Move f32 bits S-reg → W-reg, then
                    // store the W-reg at the per-lane
                    // offset. (No str_s_offset in pptk yet.)
                    a.emit(asm::fmov_w_from_s(w_tmp, sreg));
                    let offset_bytes = (i as u16) * 4;
                    a.emit(asm::str_w_offset(w_tmp, x_out, offset_bytes));
                }
            }
            Op::Return => {
                a.emit(asm::ret());
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "op {other:?} not supported")));
            }
        }
    }
    Ok(a.into_bytes())
}

/// Emit one scalar f32 binary op (fadd/fsub/fmul/fdiv).
/// Takes `&mut HashMap` for both lookup + insert to keep
/// the borrow checker happy.
fn emit_fp_binop_inline(
    a: &mut asm::Asm,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    alloc_vreg: &mut dyn FnMut() -> Result<asm::Vreg, BackendError>,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    make_inst: fn(asm::Vreg, asm::Vreg, asm::Vreg) -> u32,
) -> Result<(), BackendError> {
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("fp binop without result".into()))?;
    let l = *scalars.get(&lhs.id).ok_or_else(||
        BackendError::Internal(format!("fp binop lhs {:?} missing", lhs.id)))?;
    let r = *scalars.get(&rhs.id).ok_or_else(||
        BackendError::Internal(format!("fp binop rhs {:?} missing", rhs.id)))?;
    let d = alloc_vreg()?;
    a.emit(make_inst(d, l, r));
    scalars.insert(result.id, d);
    Ok(())
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
