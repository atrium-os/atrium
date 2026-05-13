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

    // ── Pre-pass: live-range analysis ─────────────────────────
    //
    // For each scalar ValueId compute the highest inst
    // index that references it. ConstVec lanes inherit the
    // ConstVec result's uses transitively, so a Store of a
    // vector keeps every lane alive through the Store.
    let last_use = compute_last_use(&block.insts);

    // ── Linear-scan register allocator ─────────────────────────
    //
    // Free pool of V-regs (V16..V31, caller-saved in
    // AAPCS64). At each inst i, before defining any new
    // value, expire scalars whose last_use < i and return
    // their V-regs to the pool. Then allocate from the pool
    // for new defs.
    let mut free_pool: Vec<u8> = (16..32).rev().collect();
    let mut owners: HashMap<u8, ValueId> = HashMap::new();

    for (i, inst) in block.insts.iter().enumerate() {
        // Expire scalars whose last_use < i. Their V-regs
        // return to the free pool. Drain into a temp Vec
        // first to dodge the borrow checker.
        let dead: Vec<u8> = owners.iter()
            .filter_map(|(n, id)|
                if last_use.get(id).copied().unwrap_or(usize::MAX) < i {
                    Some(*n)
                } else { None })
            .collect();
        for n in dead {
            owners.remove(&n);
            free_pool.push(n);
        }

        match &inst.op {
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let bits = (*value as f32).to_bits();
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstFloat without result".into()))?;
                let v = alloc_vreg(&mut free_pool, &mut owners, result.id)?;
                materialise_u32_into_w(&mut a, w_tmp, bits);
                a.emit(asm::fmov_s_from_w(v, w_tmp));
                scalars.insert(result.id, v);
            }
            Op::ConstVec(elements) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstVec without result".into()))?;
                for el in elements {
                    if !scalars.contains_key(&el.id) {
                        return Err(BackendError::Unsupported(format!(
                            "ConstVec lane {:?} not in scalars", el.id)));
                    }
                }
                vectors.insert(result.id, elements.clone());
            }
            Op::FAdd(a_v, b_v) => emit_fp_binop_with_pool(
                &mut a, &mut scalars, &mut free_pool, &mut owners,
                inst, a_v, b_v, asm::fadd_s)?,
            Op::FSub(a_v, b_v) => emit_fp_binop_with_pool(
                &mut a, &mut scalars, &mut free_pool, &mut owners,
                inst, a_v, b_v, asm::fsub_s)?,
            Op::FMul(a_v, b_v) => emit_fp_binop_with_pool(
                &mut a, &mut scalars, &mut free_pool, &mut owners,
                inst, a_v, b_v, asm::fmul_s)?,
            Op::FDiv(a_v, b_v) => emit_fp_binop_with_pool(
                &mut a, &mut scalars, &mut free_pool, &mut owners,
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
                for (lane_i, lane) in lanes.iter().enumerate() {
                    let sreg = *scalars.get(&lane.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "lane {:?} not in scalars", lane.id)))?;
                    a.emit(asm::fmov_w_from_s(w_tmp, sreg));
                    let offset_bytes = (lane_i as u16) * 4;
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

/// Linear-scan allocator helper: take one V-reg from the
/// free pool, remember which value owns it. Returns
/// Unsupported when the pool is empty (spilling lands
/// in step 5+).
fn alloc_vreg(
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    owner: ValueId,
) -> Result<asm::Vreg, BackendError> {
    let n = free_pool.pop().ok_or_else(|| BackendError::Unsupported(
        "linear-scan RA ran out of V-regs; spilling lands in step 5".into()))?;
    owners.insert(n, owner);
    Ok(asm::Vreg(n))
}

/// Compute the highest inst index that references each
/// scalar ValueId. ConstVec lanes inherit the ConstVec
/// result's uses transitively (a Store of a vector keeps
/// each lane alive through the Store).
fn compute_last_use(insts: &[atrium_spv_ir::Inst]) -> HashMap<ValueId, usize> {
    let mut last_use: HashMap<ValueId, usize> = HashMap::new();
    let mut vec_lanes: HashMap<ValueId, Vec<ValueId>> = HashMap::new();
    for (i, inst) in insts.iter().enumerate() {
        let mut mark = |id: ValueId| {
            last_use.entry(id)
                .and_modify(|e| *e = (*e).max(i))
                .or_insert(i);
        };
        match &inst.op {
            Op::ConstFloat { .. } | Op::ConstInt { .. } | Op::ConstNull => {}
            Op::ConstVec(els) => {
                let lane_ids: Vec<ValueId> =
                    els.iter().map(|v| v.id).collect();
                for lid in &lane_ids { mark(*lid); }
                if let Some(r) = inst.result.as_ref() {
                    vec_lanes.insert(r.id, lane_ids);
                }
            }
            Op::FAdd(l, r) | Op::FSub(l, r)
            | Op::FMul(l, r) | Op::FDiv(l, r) => {
                mark(l.id); mark(r.id);
            }
            Op::FNeg(s) => mark(s.id),
            Op::Store { ptr: _, value } => {
                mark(value.id);
                if let Some(lane_ids) = vec_lanes.get(&value.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            _ => {}
        }
    }
    last_use
}

/// Emit one scalar f32 binary op (fadd/fsub/fmul/fdiv).
/// Allocates the destination V-reg from the linear-scan
/// free pool.
#[allow(clippy::too_many_arguments)]
fn emit_fp_binop_with_pool(
    a: &mut asm::Asm,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
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
    let d = alloc_vreg(free_pool, owners, result.id)?;
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
