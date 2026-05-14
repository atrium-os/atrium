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
    BlockId, FloatKind, Function, Module, Op, ShaderStage, StorageClass, Type,
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
        let (body, pc_entries) = emit_function(func)?;
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
        let off = obj.add_symbol_data(sym, text_section, &body, 4);

        // PC-map: one entry per lowered IR instruction.
        // `emit_function` records each inst's body-relative
        // byte offset paired with its source SPIR-V offset;
        // shift by this function's offset within the text
        // section so host_offsets are section-relative. The
        // section grows per function so offsets stay
        // monotone non-decreasing across the whole module.
        for (rel_host, spirv) in pc_entries {
            pcmap.push(off as u32 + rel_host, spirv);
        }
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
///
/// Returns the ARM64 body bytes plus the PC-map entries
/// `(body_relative_host_offset, source_spirv_offset)` —
/// one per lowered IR instruction, recorded in codegen-
/// walk order so their host offsets are already monotone
/// non-decreasing.
fn emit_function(
    func: &Function,
) -> Result<(Vec<u8>, Vec<(u32, u32)>), BackendError> {
    if func.stage != ShaderStage::Fragment {
        return Err(BackendError::Unsupported(format!(
            "stage {:?} not yet supported", func.stage)));
    }

    let mut a = asm::Asm::new();
    // PC-map entries: (body-relative host byte offset,
    // source SPIR-V offset) — one per IR inst, pushed in
    // codegen-walk order. `a` only ever grows, so the host
    // offsets come out monotone non-decreasing.
    let mut pcmap_entries: Vec<(u32, u32)> = Vec::new();
    // scalars[id] = Vreg holding the live f32 (S-reg view).
    let mut scalars: HashMap<ValueId, asm::Vreg> = HashMap::new();
    let mut vectors: HashMap<ValueId, Vec<Value>> = HashMap::new();
    // Pointer-typed Values: (base X-reg, byte offset).
    let mut pointers: HashMap<ValueId, (asm::Xreg, i32)> = HashMap::new();
    // Bool-typed Values (results of FOrd*/FUnord*): held
    // in a W-reg as i32 0/1 per constraint B4. Allocated
    // from a tiny separate pool W10..W15 — bools are rare
    // and short-lived, so no reuse needed yet.
    let mut bools: HashMap<ValueId, asm::Wreg> = HashMap::new();
    let mut next_bool_w: u8 = 10;
    // Set by a comparison whose result feeds only the
    // block's BranchCond terminator (compare→branch
    // fusion): `(comparison result id, condition code)`.
    // The BranchCond arm consumes it on the very next
    // instruction — flags survive intact because the
    // comparison sits immediately before the terminator.
    let mut fused_branch: Option<(ValueId, asm::Cond)> = None;
    // Integer scalar Values (i32/u32): held in W-regs
    // drawn from a separate linear-scan pool W13..W17
    // (5 slots, caller-saved per AAPCS64 → no prologue).
    // Loops with i32 induction variables push past a
    // bump allocator's budget, so the int pool recycles
    // dead W-regs the same way the f32 V-pool does.
    let mut ints: HashMap<ValueId, asm::Wreg> = HashMap::new();
    let mut int_pool = IntPool::new();

    // X4 holds out_color. Scratch X9/W9 for constant
    // materialisation + fmov bridging.
    let x_out = asm::Xreg(4);
    let w_tmp = asm::Wreg(9);

    // Synthetic-id counter for per-lane Values that need a
    // ValueId but don't exist in the IR (vec-Load lane
    // scalars, vec-arithmetic-result lane scalars).
    // Starts well above any realistic IR ValueId to avoid
    // colliding with scalars-map entries for real IR
    // values — small backend-side multipliers were
    // colliding with the dense low IR ValueIds the
    // pre-allocation pass assigns to function results.
    let mut next_synth_id: u32 = 1_000_000;

    // ── Block layout ──────────────────────────────────────────
    //
    // Walk blocks in BlockId order (matches the frontend's
    // assignment). Build a FLAT list of insts across all
    // blocks so live-range analysis can span block
    // boundaries — without flat indices, a scalar defined
    // in entry block and used in a successor would look
    // dead at the block boundary and get its V-reg
    // recycled.
    let mut block_order: Vec<BlockId> =
        func.blocks.keys().copied().collect();
    block_order.sort_by_key(|b| b.0);

    // Flat-index → (BlockId, intra-block-index). Inverse
    // map: per-block-start flat index. Also the flat
    // index of each block's *terminator* — needed so Phi
    // arm sources used on a back-edge get a live range
    // that reaches the predecessor's branch, not just the
    // (earlier-in-flat-order) Phi.
    let mut flat_insts: Vec<&atrium_spv_ir::Inst> = Vec::new();
    let mut block_flat_start: HashMap<BlockId, usize> = HashMap::new();
    let mut block_term_idx: HashMap<BlockId, usize> = HashMap::new();
    for bid in &block_order {
        let block = func.blocks.get(bid).unwrap();
        block_flat_start.insert(*bid, flat_insts.len());
        for inst in &block.insts {
            flat_insts.push(inst);
        }
        // Terminator = last inst pushed for this block.
        if !flat_insts.is_empty() {
            block_term_idx.insert(*bid, flat_insts.len() - 1);
        }
    }

    // ── Pre-pass: live-range analysis on the flat stream ───
    let (last_use, use_counts) = compute_last_use_flat(
        &flat_insts, &block_term_idx, &block_flat_start);

    // ── Linear-scan register allocator ─────────────────────────
    //
    // Free pool of V-regs (V16..V31, caller-saved in
    // AAPCS64). At each inst i, before defining any new
    // value, expire scalars whose last_use < i and return
    // their V-regs to the pool. Then allocate from the pool
    // for new defs.
    let mut free_pool: Vec<u8> = (16..32).rev().collect();
    let mut owners: HashMap<u8, ValueId> = HashMap::new();

    // ── Pre-pass: discover Phi nodes ──────────────────────────
    //
    // For each Op::Phi at the top of a block, allocate one
    // destination register and build a per-edge move list:
    // phi_moves[(from_bid, to_bid)] = Vec<(PhiDest,
    // source_value_id)> — the predecessor block's branch
    // emits the matching register move per entry just
    // before its terminator. f32 Phis live in V-regs
    // (fmov_s move); i32/u32 Phis live in W-regs (mov_w
    // move) — loops with i32 induction variables hit the
    // latter.
    let mut phi_moves: HashMap<(BlockId, BlockId),
                               Vec<(PhiDest, ValueId)>> = HashMap::new();
    // Used by Op::BranchCond to detect a Phi-bearing
    // target (critical-edge splitting lands later).
    let mut phi_target_blocks: std::collections::HashSet<BlockId> =
        std::collections::HashSet::new();
    for bid in &block_order {
        let block = func.blocks.get(bid).unwrap();
        for inst in &block.insts {
            let arms = match &inst.op {
                Op::Phi(arms) => arms,
                _ => break, // Phis must lead the block.
            };
            let result = inst.result.as_ref().ok_or_else(||
                BackendError::Internal("Phi without result".into()))?;
            // Phi destination registers are loop-carried:
            // a loop back-edge writes them via the phi-
            // move just before re-entering the header.
            // They must therefore live for the *entire*
            // function — NOT be subject to linear-scan
            // expiry — so we pop them straight off the
            // free pool without registering an owner
            // (expire only reclaims owned regs).
            let dest = match &result.ty {
                Type::F32 => {
                    let n = free_pool.pop().ok_or_else(||
                        BackendError::Unsupported(
                            "out of V-regs allocating Phi dest".into()))?;
                    let v = asm::Vreg(n);
                    scalars.insert(result.id, v);
                    PhiDest::Float(v)
                }
                Type::I32 | Type::U32 => {
                    let n = int_pool.free.pop().ok_or_else(||
                        BackendError::Unsupported(
                            "out of int W-regs allocating Phi dest".into()))?;
                    let w = asm::Wreg(n);
                    ints.insert(result.id, w);
                    PhiDest::Int(w)
                }
                other => return Err(BackendError::Unsupported(format!(
                    "Phi of type {other:?} not supported"))),
            };
            phi_target_blocks.insert(*bid);
            for arm in arms {
                phi_moves.entry((arm.from, *bid))
                    .or_default()
                    .push((dest, arm.value.id));
            }
        }
    }

    // Record where each block starts in the asm byte
    // stream so branches can be patched after the full
    // function is emitted.
    let mut block_asm_offset: HashMap<BlockId, usize> = HashMap::new();
    // Pending `b imm26` relocations: (asm_byte_offset of
    // the placeholder instruction, target BlockId).
    let mut branch_relocs: Vec<(usize, BlockId)> = Vec::new();
    // Pending `b.cond imm19` relocations, each carrying its
    // own condition code: `(patch byte offset, target,
    // cond)`. BranchCond pushes `Ne` (test a materialised
    // bool) or, with compare→branch fusion, the comparison's
    // own condition code; Switch pushes `Eq`.
    let mut cond_branch_relocs: Vec<(usize, BlockId, asm::Cond)> = Vec::new();

    // ── Prologue placeholder ──────────────────────────────────
    //
    // The int pool can dip into callee-saved registers
    // (X19..X28) when a shader's integer pressure exceeds
    // the 5 caller-saved slots — loops do this. Per AAPCS64
    // those must be saved/restored across the call. We
    // don't know yet whether any get used, so reserve a
    // fixed 5-instruction prologue region (NOPs for now)
    // and patch it after body emission. Reserving a fixed
    // size keeps every body offset — block_asm_offset,
    // branch patch sites — consistent regardless of
    // whether the prologue ends up real or NOPs.
    const PROLOGUE_INSTS: usize = 5;
    let prologue_off = a.len();
    for _ in 0..PROLOGUE_INSTS { a.emit(asm::nop()); }
    // Epilogue placeholder offsets — one per `ret`. Each
    // is a 5-NOP region emitted just before its ret;
    // patched to ldp's iff the prologue is real.
    let mut epilogue_offs: Vec<usize> = Vec::new();

    let mut flat_i: usize = 0;
    for bid in &block_order {
        let block = func.blocks.get(bid).unwrap();
        block_asm_offset.insert(*bid, a.len());
    for (block_inst_idx, inst) in block.insts.iter().enumerate() {
        let i = flat_i;
        flat_i += 1;
        let _ = block_inst_idx;
        // Record the PC-map entry for this IR inst: the
        // current body byte offset (this inst's first
        // native byte) → its source SPIR-V offset. Done
        // before emission so the offset points at the
        // start of the inst's code. Ops that lower to zero
        // native bytes (Phi, ConstVec, VectorShuffle …)
        // share the next inst's offset — a duplicate
        // host_offset, which the pcmap format allows.
        pcmap_entries.push((a.len() as u32, inst.source_spirv_offset));
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
        // Same expiry pass for the integer W-reg pool.
        int_pool.expire(i, &last_use);

        // ── compare→branch fusion eligibility ───────────────
        //
        // True when this instruction is a comparison whose
        // result feeds *only* this block's BranchCond
        // terminator, and the comparison sits immediately
        // before that terminator. Both conditions matter:
        //   * immediately-before → nothing between the
        //     `cmp` and the `b.cond` clobbers NZCV;
        //   * sole use → no OpSelect / later block needs the
        //     materialised i32 bool, so skipping `cset` is
        //     safe.
        // Sole-use is checked via `use_counts` (the raw
        // operand tally) rather than `last_use`: the
        // loop-liveness extension pushes a loop-header
        // comparison's `last_use` out to the back-edge, so
        // `last_use == terminator` would spuriously fail for
        // exactly the loop-exit branches this optimisation
        // most wants to fuse.
        // The comparison arms read this; a fused comparison
        // emits just `cmp`/`fcmp` and stashes the condition
        // in `fused_branch` for the BranchCond arm.
        let fuse_eligible: bool = inst.result.as_ref().is_some_and(|res| {
            block_inst_idx + 2 == block.insts.len()
                && matches!(
                    &block.insts[block_inst_idx + 1].op,
                    Op::BranchCond { cond, .. } if cond.id == res.id)
                && use_counts.get(&res.id).copied() == Some(1)
        });

        match &inst.op {
            Op::ConstInt { value, kind: _ } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstInt without result".into()))?;
                // If the constant is referenced by an int
                // arithmetic op, it needs a W-reg.
                // Orphan ConstInts (e.g. AccessChain index
                // resolutions) are consumed by the frontend
                // before the backend sees them — but a
                // genuinely-unused ConstInt left in the
                // stream would still get a reg here. With
                // linear-scan the reg is reclaimed at the
                // next inst (last_use < i), so this is
                // cheap; on pool exhaustion we skip
                // codegen (assume orphan).
                if let Ok(w) = alloc_int_w(&mut int_pool, result.id) {
                    materialise_u32_into_w(&mut a, w, *value as i32 as u32);
                    ints.insert(result.id, w);
                }
            }
            Op::ConstNull => {
                // Treat as an orphan — no W-reg.
            }
            Op::IAdd(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::add_w)?,
            Op::ISub(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::sub_w)?,
            Op::IMul(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::mul_w)?,
            Op::SDiv(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::sdiv_w)?,
            Op::UDiv(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::udiv_w)?,
            // Bitwise + shifts.
            Op::BitAnd(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::and_w)?,
            Op::BitOr(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::orr_w)?,
            Op::BitXor(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::eor_w)?,
            Op::Shl(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::lslv_w)?,
            Op::LShr(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::lsrv_w)?,
            Op::AShr(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, inst, l, r, asm::asrv_w)?,
            Op::BitNot(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("BitNot without result".into()))?;
                let s_w = *ints.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "BitNot operand {:?} not in ints", s.id)))?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                a.emit(asm::mvn_w(d_w, s_w));
                ints.insert(result.id, d_w);
            }
            // Integer comparisons → Bool W-reg.
            Op::IEq(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Eq)?,
            Op::INe(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Ne)?,
            Op::SLt(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Lt)?,
            Op::SLe(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Le)?,
            Op::SGt(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Gt)?,
            Op::SGe(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Ge)?,
            Op::ULt(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Cc)?,
            Op::ULe(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Ls)?,
            Op::UGt(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Hi)?,
            Op::UGe(l, r) => emit_icmp_to_bool(
                &mut a, &ints, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Cs)?,
            Op::INeg(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("INeg without result".into()))?;
                let s_w = *ints.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "INeg operand {:?} not in ints", s.id)))?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                a.emit(asm::neg_w(d_w, s_w));
                ints.insert(result.id, d_w);
            }
            Op::ConvertSToF(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConvertSToF without result".into()))?;
                let s_w = *ints.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertSToF operand {:?} not in ints", s.id)))?;
                let d_v = alloc_vreg(&mut free_pool, &mut owners, result.id)?;
                a.emit(asm::scvtf_s_from_w(d_v, s_w));
                scalars.insert(result.id, d_v);
            }
            Op::ConvertUToF(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConvertUToF without result".into()))?;
                let s_w = *ints.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertUToF operand {:?} not in ints", s.id)))?;
                let d_v = alloc_vreg(&mut free_pool, &mut owners, result.id)?;
                a.emit(asm::ucvtf_s_from_w(d_v, s_w));
                scalars.insert(result.id, d_v);
            }
            Op::ConvertFToS(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConvertFToS without result".into()))?;
                let s_v = *scalars.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertFToS operand {:?} not in scalars", s.id)))?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                a.emit(asm::fcvtzs_w_from_s(d_w, s_v));
                ints.insert(result.id, d_w);
            }
            Op::ConvertFToU(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConvertFToU without result".into()))?;
                let s_v = *scalars.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertFToU operand {:?} not in scalars", s.id)))?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                a.emit(asm::fcvtzu_w_from_s(d_w, s_v));
                ints.insert(result.id, d_w);
            }
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
            // Float compares → Bool (i32 0/1 in a W-reg).
            Op::FOrdEq(a_v, b_v) => emit_fcmp_to_bool(
                &mut a, &scalars, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Eq)?,
            Op::FOrdNe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a, &scalars, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ne)?,
            Op::FOrdLt(a_v, b_v) => emit_fcmp_to_bool(
                &mut a, &scalars, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Mi)?,
            Op::FOrdLe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a, &scalars, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ls)?,
            Op::FOrdGt(a_v, b_v) => emit_fcmp_to_bool(
                &mut a, &scalars, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Gt)?,
            Op::FOrdGe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a, &scalars, &mut bools, &mut next_bool_w,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ge)?,
            // OpVectorShuffle: produce a new vector by
            // picking per-output-lane indices into
            // src1 ++ src2. With our per-lane S-reg
            // storage this is pure aliasing — each output
            // lane reuses an existing source S-reg, so no
            // new V-regs are allocated (modulo 0xFFFFFFFF
            // "Undefined" slots which materialise a 0.0).
            Op::VectorShuffle { src1, src2, components } => {
                let s1 = vectors.get(&src1.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "VectorShuffle src1 {:?} not a vec", src1.id)))?;
                let s2 = vectors.get(&src2.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "VectorShuffle src2 {:?} not a vec", src2.id)))?;
                let combined = s1.len() + s2.len();
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("VectorShuffle without result".into()))?;
                let mut out_lanes = Vec::with_capacity(components.len());
                for c in components {
                    if *c == 0xFFFF_FFFF {
                        // Undefined → zero. Materialise a
                        // fresh S-reg holding 0.0 (movz w,0
                        // + fmov_s_from_w) for B3 well-
                        // definedness.
                        let synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        let v = alloc_vreg(&mut free_pool, &mut owners, synth)?;
                        a.emit(asm::movz_w(w_tmp, 0, 0));
                        a.emit(asm::fmov_s_from_w(v, w_tmp));
                        scalars.insert(synth, v);
                        out_lanes.push(Value { id: synth, ty: Type::F32 });
                        continue;
                    }
                    let idx = *c as usize;
                    if idx >= combined {
                        return Err(BackendError::Unsupported(format!(
                            "VectorShuffle component {idx} out of range \
                             (combined len {combined})")));
                    }
                    let src_lane = if idx < s1.len() {
                        s1[idx].clone()
                    } else {
                        s2[idx - s1.len()].clone()
                    };
                    // Alias: reuse the existing lane Value
                    // (and its V-reg). No fmov needed.
                    out_lanes.push(src_lane);
                }
                vectors.insert(result.id, out_lanes);
            }
            // OpVectorExtract: alias lane `index`'s S-reg
            // Value into the scalar result. No instruction
            // emitted — pure aliasing, like VectorShuffle.
            Op::VectorExtract { vector, index } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "VectorExtract without result".into()))?;
                let lanes = vectors.get(&vector.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "VectorExtract source {:?} not a vec", vector.id)))?;
                let lane = lanes.get(*index as usize).ok_or_else(||
                    BackendError::Unsupported(format!(
                        "VectorExtract index {index} out of range \
                         ({} lanes)", lanes.len())))?;
                let s = *scalars.get(&lane.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "VectorExtract lane {:?} not in scalars", lane.id)))?;
                scalars.insert(result.id, s);
            }
            // OpDot: scalar = Σ a_i * b_i. Lowers to one
            // fmul_s for the first lane + (fmul_s + fadd_s)
            // per additional lane.
            Op::Dot(l, r) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("Dot without result".into()))?;
                let l_lanes = vectors.get(&l.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Dot lhs {:?} not a vec", l.id)))?;
                let r_lanes = vectors.get(&r.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Dot rhs {:?} not a vec", r.id)))?;
                if l_lanes.len() != r_lanes.len() {
                    return Err(BackendError::Unsupported(format!(
                        "Dot mismatched lanes: {} vs {}",
                        l_lanes.len(), r_lanes.len())));
                }
                // Accumulator V-reg owned by the final result id.
                let acc = alloc_vreg(&mut free_pool, &mut owners, result.id)?;
                // First lane: acc = l[0] * r[0].
                let l0 = *scalars.get(&l_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Dot lane 0 lhs {:?} missing", l_lanes[0].id)))?;
                let r0 = *scalars.get(&r_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Dot lane 0 rhs {:?} missing", r_lanes[0].id)))?;
                a.emit(asm::fmul_s(acc, l0, r0));
                // Remaining lanes: tmp = l[i] * r[i]; acc += tmp.
                for i in 1..l_lanes.len() {
                    let li = *scalars.get(&l_lanes[i].id).ok_or_else(||
                        BackendError::Internal(format!(
                            "Dot lane {i} lhs missing")))?;
                    let ri = *scalars.get(&r_lanes[i].id).ok_or_else(||
                        BackendError::Internal(format!(
                            "Dot lane {i} rhs missing")))?;
                    let tmp_synth = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let tmp = alloc_vreg(&mut free_pool, &mut owners, tmp_synth)?;
                    a.emit(asm::fmul_s(tmp, li, ri));
                    a.emit(asm::fadd_s(acc, acc, tmp));
                    // tmp dies immediately; manually return
                    // its V-reg to the pool to avoid burning
                    // 3 regs for a 4-lane Dot.
                    free_pool.push(tmp.0);
                    owners.remove(&tmp.0);
                }
                scalars.insert(result.id, acc);
            }
            Op::FAdd(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors,
                &mut free_pool, &mut owners, &mut next_synth_id,
                inst, a_v, b_v, asm::fadd_s)?,
            Op::FSub(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors,
                &mut free_pool, &mut owners, &mut next_synth_id,
                inst, a_v, b_v, asm::fsub_s)?,
            Op::FMul(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors,
                &mut free_pool, &mut owners, &mut next_synth_id,
                inst, a_v, b_v, asm::fmul_s)?,
            Op::FDiv(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors,
                &mut free_pool, &mut owners, &mut next_synth_id,
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
            Op::AccessChain { base, byte_offset } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "AccessChain without result".into()))?;
                let (param, base_off) =
                    resolve_or_make_pointer(base, &mut pointers)?;
                let new_off = base_off.saturating_add(*byte_offset as i32);
                pointers.insert(result.id, (param, new_off));
            }
            Op::Load(ptr) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("Load without result".into()))?;
                let (param, off) =
                    resolve_or_make_pointer(ptr, &mut pointers)?;
                let pointee = match &ptr.ty {
                    Type::Pointer(_, inner) => (**inner).clone(),
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load ptr {other:?} is not a Pointer type"))),
                };
                match &pointee {
                    Type::F32 => {
                        let v = alloc_vreg(
                            &mut free_pool, &mut owners, result.id)?;
                        emit_load_f32_offset(&mut a, w_tmp, param, off, v)?;
                        scalars.insert(result.id, v);
                    }
                    Type::I32 | Type::U32 => {
                        // Load a 32-bit int straight into
                        // the int W-pool: `ldr w_dst,
                        // [param, #off]`.
                        if off < 0 || off > u16::MAX as i32 {
                            return Err(BackendError::Unsupported(format!(
                                "int Load offset {off} out of range")));
                        }
                        let w = alloc_int_w(&mut int_pool, result.id)?;
                        a.emit(asm::ldr_w_offset(w, param, off as u16));
                        ints.insert(result.id, w);
                    }
                    Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                        let lane_count = match &pointee {
                            Type::Vec2(_) => 2usize,
                            Type::Vec3(_) => 3usize,
                            Type::Vec4(_) => 4usize,
                            _ => unreachable!(),
                        };
                        // Synthesise lane Values + collect
                        // into vectors[result.id]. Each lane
                        // gets a fresh S-reg + ldr+fmov.
                        let mut lanes = Vec::with_capacity(lane_count);
                        for lane_i in 0..lane_count {
                            let lane_off = off.saturating_add((lane_i * 4) as i32);
                            // Synthetic ValueId from the
                            // dedicated high-range counter
                            // — collision-free with any IR
                            // ValueId the frontend assigns.
                            let synthetic_id = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let v = alloc_vreg(
                                &mut free_pool, &mut owners, synthetic_id)?;
                            emit_load_f32_offset(&mut a, w_tmp, param, lane_off, v)?;
                            scalars.insert(synthetic_id, v);
                            lanes.push(Value {
                                id: synthetic_id, ty: Type::F32,
                            });
                        }
                        vectors.insert(result.id, lanes);
                    }
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load of {other:?} not supported"))),
                }
            }
            // OpSelect: cond ? t : f. cond is Bool W-reg.
            // Supports:
            //   - scalar cond + scalar f32 t/f
            //   - scalar cond + vec f32 t/f  (lane-walk)
            Op::Select { cond, t_val, f_val } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Select without result".into()))?;
                let w_cond = *bools.get(&cond.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Select cond {:?} not in bools", cond.id)))?;
                // Set NZCV once; both scalar and vec paths
                // reuse the same cmp+fcsel sequence per
                // lane / per scalar.
                a.emit(asm::cmp_imm_w(w_cond, 0));
                match &result.ty {
                    Type::F32 => {
                        let s_t = *scalars.get(&t_val.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "Select t {:?} not in scalars", t_val.id)))?;
                        let s_f = *scalars.get(&f_val.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "Select f {:?} not in scalars", f_val.id)))?;
                        let s_d = alloc_vreg(
                            &mut free_pool, &mut owners, result.id)?;
                        a.emit(asm::fcsel_s(s_d, s_t, s_f, asm::Cond::Ne));
                        scalars.insert(result.id, s_d);
                    }
                    Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                        let t_lanes = vectors.get(&t_val.id).cloned().ok_or_else(||
                            BackendError::Internal(format!(
                                "vec Select t {:?} not in vectors", t_val.id)))?;
                        let f_lanes = vectors.get(&f_val.id).cloned().ok_or_else(||
                            BackendError::Internal(format!(
                                "vec Select f {:?} not in vectors", f_val.id)))?;
                        if t_lanes.len() != f_lanes.len() {
                            return Err(BackendError::Unsupported(format!(
                                "vec Select mismatched lanes: {} vs {}",
                                t_lanes.len(), f_lanes.len())));
                        }
                        let mut out_lanes = Vec::with_capacity(t_lanes.len());
                        for (li, (tl, fl)) in t_lanes.iter().zip(f_lanes.iter()).enumerate() {
                            let st = *scalars.get(&tl.id).ok_or_else(||
                                BackendError::Internal(format!(
                                    "vec Select t lane {li} {:?} missing", tl.id)))?;
                            let sf = *scalars.get(&fl.id).ok_or_else(||
                                BackendError::Internal(format!(
                                    "vec Select f lane {li} {:?} missing", fl.id)))?;
                            let synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let sd = alloc_vreg(
                                &mut free_pool, &mut owners, synth)?;
                            a.emit(asm::fcsel_s(sd, st, sf, asm::Cond::Ne));
                            scalars.insert(synth, sd);
                            out_lanes.push(Value { id: synth, ty: tl.ty.clone() });
                        }
                        vectors.insert(result.id, out_lanes);
                    }
                    other => return Err(BackendError::Unsupported(format!(
                        "Select of type {other:?} not supported"))),
                }
            }
            Op::Phi(_) => {
                // Phi result V-reg was pre-allocated; the
                // per-edge move emits land at the
                // predecessor's branch. Nothing to do here.
            }
            Op::Return => {
                // Epilogue placeholder (5 NOPs) before
                // every ret — patched to ldp's iff the
                // prologue is real.
                epilogue_offs.push(a.len());
                for _ in 0..PROLOGUE_INSTS { a.emit(asm::nop()); }
                a.emit(asm::ret());
            }
            Op::Branch(target) => {
                // Phi-edge moves: if `target` has incoming
                // Phis, emit the matching register move
                // (fmov_s for f32, mov_w for i32/u32) for
                // each, just before the branch.
                if let Some(moves) = phi_moves.get(&(*bid, *target)) {
                    for (dest, src_id) in moves {
                        match dest {
                            PhiDest::Float(dv) => {
                                let src = *scalars.get(src_id).ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi f32 source {:?} not in scalars",
                                        src_id)))?;
                                a.emit(asm::fmov_s(*dv, src));
                            }
                            PhiDest::Int(dw) => {
                                let src = *ints.get(src_id).ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi int source {:?} not in ints",
                                        src_id)))?;
                                a.emit(asm::mov_w(*dw, src));
                            }
                        }
                    }
                }
                let patch_off = a.len();
                a.emit(asm::b(0));
                branch_relocs.push((patch_off, *target));
            }
            Op::Switch { selector, cases, default } => {
                // Lower to a chain of cmp_imm_w + b.cond.EQ
                // + final unconditional jump to default.
                // Reject Phi-bearing target as with
                // BranchCond.
                if phi_target_blocks.contains(default)
                    || cases.iter().any(|(_, t)|
                        phi_target_blocks.contains(t))
                {
                    return Err(BackendError::Unsupported(
                        "Switch into Phi-bearing block; \
                         critical-edge splitting lands in v13".into()));
                }
                let w_sel = *ints.get(&selector.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Switch selector {:?} not in ints",
                        selector.id)))?;
                for (lit, target) in cases {
                    if *lit < 0 || *lit > 4095 {
                        return Err(BackendError::Unsupported(format!(
                            "Switch case literal {lit} outside \
                             cmp_imm_w 12-bit range (use a wide-
                             literal lowering path in v13+)")));
                    }
                    a.emit(asm::cmp_imm_w(w_sel, *lit as u16));
                    let patch_t = a.len();
                    a.emit(asm::b_cond(asm::Cond::Eq, 0));
                    cond_branch_relocs.push((patch_t, *target, asm::Cond::Eq));
                }
                // Fall-through to default.
                let patch_d = a.len();
                a.emit(asm::b(0));
                branch_relocs.push((patch_d, *default));
            }
            Op::BranchCond { cond, t_block, f_block } => {
                // BranchCond targeting a Phi-bearing block
                // requires critical-edge splitting (so the
                // moves only run on one outgoing edge). v11
                // defers to keep scope tight; the if/else
                // pattern doesn't hit this because then/else
                // branches are unconditional. Loops do hit
                // it — v12 lands the splitter.
                if phi_target_blocks.contains(t_block)
                    || phi_target_blocks.contains(f_block)
                {
                    return Err(BackendError::Unsupported(
                        "BranchCond into Phi-bearing block; \
                         critical-edge splitting lands in v12".into()));
                }
                // Compare→branch fusion: if the immediately
                // preceding comparison stashed its condition
                // for this exact cond value, NZCV is still
                // live from its `cmp`/`fcmp` — emit the
                // conditional branch directly, no bool
                // materialised, no `cmp w,#0`.
                let (patch_t, branch_cond) = match fused_branch.take()
                    .filter(|(fid, _)| *fid == cond.id)
                {
                    Some((_, fcond)) => {
                        let patch_t = a.len();
                        a.emit(asm::b_cond(fcond, 0));
                        (patch_t, fcond)
                    }
                    None => {
                        let w_bool = *bools.get(&cond.id).ok_or_else(||
                            BackendError::Unsupported(format!(
                                "BranchCond cond {:?} is not a known Bool W-reg",
                                cond.id)))?;
                        // cmp w_bool, #0  → flags reflect zero-ness
                        a.emit(asm::cmp_imm_w(w_bool, 0));
                        // b.ne t_block (taken if w_bool != 0).
                        let patch_t = a.len();
                        a.emit(asm::b_cond(asm::Cond::Ne, 0));
                        (patch_t, asm::Cond::Ne)
                    }
                };
                cond_branch_relocs.push((patch_t, *t_block, branch_cond));
                // Unconditional fallthrough to f_block.
                let patch_f = a.len();
                a.emit(asm::b(0));
                branch_relocs.push((patch_f, *f_block));
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "op {other:?} not supported")));
            }
        }
    }
    } // end of per-block loop

    // ── Branch fixup ──────────────────────────────────────────
    //
    // Walk each pending `b imm26` placeholder and patch it
    // with the resolved instruction-relative offset.
    // imm26 is the signed instruction-count delta from
    // the branch's PC to the target block's first
    // instruction.
    for (patch_off, target_bid) in &branch_relocs {
        let target_byte = *block_asm_offset.get(target_bid).ok_or_else(||
            BackendError::Internal(format!(
                "branch target {target_bid:?} has no asm offset")))?;
        let delta_bytes = target_byte as i64 - *patch_off as i64;
        if delta_bytes % 4 != 0 {
            return Err(BackendError::Internal(
                "branch delta not 4-aligned".into()));
        }
        let delta_insts = (delta_bytes / 4) as i32;
        // imm26 is 26 signed bits → range ±2^25 insts.
        if !(-(1 << 25)..(1 << 25)).contains(&delta_insts) {
            return Err(BackendError::Unsupported(format!(
                "branch delta {delta_insts} exceeds imm26 range")));
        }
        a.patch(*patch_off, asm::b(delta_insts));
    }

    // Conditional-branch fixup (imm19, ±2^18 insts). Each
    // reloc carries its own condition code — `Ne` for a
    // bool-test BranchCond, the comparison's code for a
    // fused BranchCond, `Eq` for a Switch case.
    for (patch_off, target_bid, cond) in &cond_branch_relocs {
        let target_byte = *block_asm_offset.get(target_bid).ok_or_else(||
            BackendError::Internal(format!(
                "cond branch target {target_bid:?} has no asm offset")))?;
        let delta_bytes = target_byte as i64 - *patch_off as i64;
        if delta_bytes % 4 != 0 {
            return Err(BackendError::Internal(
                "cond branch delta not 4-aligned".into()));
        }
        let delta_insts = (delta_bytes / 4) as i32;
        if !(-(1 << 18)..(1 << 18)).contains(&delta_insts) {
            return Err(BackendError::Unsupported(format!(
                "cond-branch delta {delta_insts} exceeds imm19 range")));
        }
        a.patch(*patch_off, asm::b_cond(*cond, delta_insts));
    }

    // ── Prologue / epilogue fixup ─────────────────────────────
    //
    // If the int pool dipped into callee-saved registers
    // (W19..W28 — only happens under heavy integer
    // pressure, e.g. loops), patch the reserved 5-NOP
    // prologue + every 5-NOP epilogue with the AAPCS64
    // save/restore sequence. We save all of X19..X28 (5
    // pairs) unconditionally when *any* are used —
    // conservative but correct; a used-set-precise
    // prologue is a later optimisation. If no callee-saved
    // reg was touched the placeholders stay NOPs (zero
    // cost beyond 5+5 NOP slots).
    if int_pool.used_callee_saved {
        // Prologue: 5× pre-indexed stp, each dropping SP
        // by 16. Order: x19/x20 highest address … x27/x28
        // lowest.
        let save_pairs = [
            (asm::Xreg(19), asm::Xreg(20)),
            (asm::Xreg(21), asm::Xreg(22)),
            (asm::Xreg(23), asm::Xreg(24)),
            (asm::Xreg(25), asm::Xreg(26)),
            (asm::Xreg(27), asm::Xreg(28)),
        ];
        for (i, (a_reg, b_reg)) in save_pairs.iter().enumerate() {
            a.patch(prologue_off + i * 4,
                    asm::stp_x_pre(*a_reg, *b_reg, asm::Xreg(31), -16));
        }
        // Epilogue: reverse-order post-indexed ldp, each
        // bumping SP back up by 16.
        for &ep in &epilogue_offs {
            for (i, (a_reg, b_reg)) in save_pairs.iter().rev().enumerate() {
                a.patch(ep + i * 4,
                        asm::ldp_x_post(*a_reg, *b_reg, asm::Xreg(31), 16));
            }
        }
    }

    Ok((a.into_bytes(), pcmap_entries))
}

/// Materialise the (base X-reg, byte offset) pointer
/// repr for a pointer-typed Value. If the Value already
/// has a repr (set by a prior AccessChain), return it.
/// Otherwise it must be a Variable: derive the base
/// register from its storage class per the fragment-
/// shader AAPCS64 split.
fn resolve_or_make_pointer(
    v: &Value,
    pointers: &mut HashMap<ValueId, (asm::Xreg, i32)>,
) -> Result<(asm::Xreg, i32), BackendError> {
    if let Some(p) = pointers.get(&v.id) { return Ok(*p); }
    let storage = match &v.ty {
        Type::Pointer(sc, _) => sc,
        other => return Err(BackendError::Unsupported(format!(
            "pointer Value is not Pointer-typed: {other:?}"))),
    };
    let param = match storage {
        StorageClass::Input        => asm::Xreg(0), // in_varyings
        StorageClass::Uniform      => asm::Xreg(1),
        StorageClass::PushConstant => asm::Xreg(2),
        StorageClass::Output       => asm::Xreg(4),
        other => return Err(BackendError::Unsupported(format!(
            "storage class {other:?} not mapped to an ABI register"))),
    };
    let repr = (param, 0);
    pointers.insert(v.id, repr);
    Ok(repr)
}

/// Emit `ldr w_tmp, [param, #off]; fmov s_dst, w_tmp` —
/// load an f32 from `[param + off]` into an S-reg via a
/// W-reg bridge (pptk has no `ldr_s_offset` yet).
fn emit_load_f32_offset(
    a: &mut asm::Asm,
    w_tmp: asm::Wreg,
    param: asm::Xreg,
    off: i32,
    dst: asm::Vreg,
) -> Result<(), BackendError> {
    if off < 0 {
        return Err(BackendError::Unsupported(format!(
            "negative load offset {off} not supported")));
    }
    if off > u16::MAX as i32 {
        return Err(BackendError::Unsupported(format!(
            "load offset {off} exceeds u16 — large offsets need
             a different addressing mode")));
    }
    a.emit(asm::ldr_w_offset(w_tmp, param, off as u16));
    a.emit(asm::fmov_s_from_w(dst, w_tmp));
    Ok(())
}

/// Bump-allocate one W-reg from the int pool (W13..W17).
/// Destination register for a Phi node. f32 Phis land in
/// a V-reg (moved with `fmov_s`); i32/u32 Phis land in a
/// W-reg (moved with `mov_w`).
#[derive(Debug, Clone, Copy)]
enum PhiDest {
    Float(asm::Vreg),
    Int(asm::Wreg),
}

/// Linear-scan allocator for the integer W-reg pool
/// (W13..W17). Mirrors the f32 V-reg pool: pop on alloc,
/// return on expiry. `owner` is the ValueId that gets the
/// reg — `expire` consults `last_use` to know when to
/// reclaim.
struct IntPool {
    free: Vec<u8>,
    owners: HashMap<u8, ValueId>,
    /// Set once any callee-saved reg (W19..W28) is handed
    /// out — drives whether emit_function patches in the
    /// stp/ldp prologue + epilogue.
    used_callee_saved: bool,
}

impl IntPool {
    fn new() -> Self {
        // Pop order: W13..W17 (caller-saved, free) first,
        // then W19..W28 (callee-saved — using these forces
        // a prologue, so they're the overflow tier). Vec
        // end is W13 so `pop` hands out caller-saved first.
        let mut free: Vec<u8> = (19..29).rev().collect();
        free.extend((13..18).rev());
        Self { free, owners: HashMap::new(), used_callee_saved: false }
    }

    fn alloc(&mut self, owner: ValueId) -> Result<asm::Wreg, BackendError> {
        let n = self.free.pop().ok_or_else(|| BackendError::Unsupported(
            "int linear-scan RA ran out of W-regs (W13..W17 + W19..W28); \
             spilling lands in a later widening".into()))?;
        if n >= 19 { self.used_callee_saved = true; }
        self.owners.insert(n, owner);
        Ok(asm::Wreg(n))
    }

    /// Return W-regs whose owner's last_use < `before`.
    fn expire(&mut self, before: usize,
              last_use: &HashMap<ValueId, usize>) {
        let dead: Vec<u8> = self.owners.iter()
            .filter_map(|(n, id)|
                if last_use.get(id).copied().unwrap_or(usize::MAX) < before {
                    Some(*n)
                } else { None })
            .collect();
        for n in dead {
            self.owners.remove(&n);
            self.free.push(n);
        }
    }
}

/// Convenience: alloc one int W-reg for `owner`.
fn alloc_int_w(
    pool: &mut IntPool,
    owner: ValueId,
) -> Result<asm::Wreg, BackendError> {
    pool.alloc(owner)
}

/// Emit one scalar i32 binary op (add/sub/mul/sdiv/udiv).
fn emit_int_binop(
    a: &mut asm::Asm,
    ints: &mut HashMap<ValueId, asm::Wreg>,
    int_pool: &mut IntPool,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    make_inst: fn(asm::Wreg, asm::Wreg, asm::Wreg) -> u32,
) -> Result<(), BackendError> {
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("int binop without result".into()))?;
    let l = *ints.get(&lhs.id).ok_or_else(||
        BackendError::Internal(format!(
            "int binop lhs {:?} not in ints", lhs.id)))?;
    let r = *ints.get(&rhs.id).ok_or_else(||
        BackendError::Internal(format!(
            "int binop rhs {:?} not in ints", rhs.id)))?;
    let d = alloc_int_w(int_pool, result.id)?;
    a.emit(make_inst(d, l, r));
    ints.insert(result.id, d);
    Ok(())
}

/// Emit an integer comparison.
///
/// Two lowerings:
///
/// * **fused** (`fuse_eligible`) — the comparison result
///   feeds *only* the block's `BranchCond` terminator and
///   sits immediately before it, so NZCV survives intact
///   from the `cmp` to the `b.cond`. Emit just `cmp_w` and
///   record the condition in `fused_branch`; the
///   `BranchCond` arm consumes it and emits a single
///   `b.<cond>`. No Bool W-reg is materialised.
/// * **materialised** — emit `cmp_w + cset_w` into a
///   fresh bool W-pool register (the value is used by an
///   `OpSelect`, a later block, etc.).
#[allow(clippy::too_many_arguments)]
fn emit_icmp_to_bool(
    a: &mut asm::Asm,
    ints: &HashMap<ValueId, asm::Wreg>,
    bools: &mut HashMap<ValueId, asm::Wreg>,
    next_bool_w: &mut u8,
    fused_branch: &mut Option<(ValueId, asm::Cond)>,
    fuse_eligible: bool,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    cond: asm::Cond,
) -> Result<(), BackendError> {
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("icmp without result".into()))?;
    let l = *ints.get(&lhs.id).ok_or_else(||
        BackendError::Internal(format!(
            "icmp lhs {:?} not in ints", lhs.id)))?;
    let r = *ints.get(&rhs.id).ok_or_else(||
        BackendError::Internal(format!(
            "icmp rhs {:?} not in ints", rhs.id)))?;
    if fuse_eligible {
        a.emit(asm::cmp_w(l, r));
        *fused_branch = Some((result.id, cond));
        return Ok(());
    }
    if *next_bool_w >= 13 {
        // W10..W12 are bool pool (3 slots); W13..W17 are
        // the int pool. The ranges must not overlap.
        return Err(BackendError::Unsupported(
            "ran out of Bool W-regs (W10..W12 exhausted)".into()));
    }
    let w_bool = asm::Wreg(*next_bool_w);
    *next_bool_w += 1;
    a.emit(asm::cmp_w(l, r));
    a.emit(asm::cset_w(w_bool, cond));
    bools.insert(result.id, w_bool);
    Ok(())
}

/// Emit a float comparison. Same two lowerings as
/// [`emit_icmp_to_bool`]: fused (`fcmp_s` only, condition
/// recorded in `fused_branch`) or materialised
/// (`fcmp_s + cset_w` into a bool W-pool register).
#[allow(clippy::too_many_arguments)]
fn emit_fcmp_to_bool(
    a: &mut asm::Asm,
    scalars: &HashMap<ValueId, asm::Vreg>,
    bools: &mut HashMap<ValueId, asm::Wreg>,
    next_bool_w: &mut u8,
    fused_branch: &mut Option<(ValueId, asm::Cond)>,
    fuse_eligible: bool,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    cond: asm::Cond,
) -> Result<(), BackendError> {
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("fcmp without result".into()))?;
    let l = *scalars.get(&lhs.id).ok_or_else(||
        BackendError::Internal(format!("fcmp lhs {:?} missing", lhs.id)))?;
    let r = *scalars.get(&rhs.id).ok_or_else(||
        BackendError::Internal(format!("fcmp rhs {:?} missing", rhs.id)))?;
    if fuse_eligible {
        a.emit(asm::fcmp_s(l, r));
        *fused_branch = Some((result.id, cond));
        return Ok(());
    }
    if *next_bool_w >= 13 {
        // W10..W12 are bool pool (3 slots); the int pool
        // starts at W13 so the ranges must not overlap.
        return Err(BackendError::Unsupported(
            "ran out of Bool W-regs (W10..W12 exhausted)".into()));
    }
    let w_bool = asm::Wreg(*next_bool_w);
    *next_bool_w += 1;
    a.emit(asm::fcmp_s(l, r));
    a.emit(asm::cset_w(w_bool, cond));
    bools.insert(result.id, w_bool);
    Ok(())
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

/// Compute, per scalar ValueId: the highest inst index
/// that references it (`last_use`) and how many operand
/// occurrences reference it (`use_counts`). ConstVec lanes
/// inherit the ConstVec result's uses transitively (a
/// Store of a vector keeps each lane alive through the
/// Store).
///
/// `use_counts` is the *raw* operand-occurrence tally — it
/// is NOT perturbed by the loop-liveness extension below,
/// so `use_counts[v] == 1` is a reliable "single use"
/// signal even for a value used inside a loop. Compare→
/// branch fusion relies on this.
fn compute_last_use_flat(
    insts: &[&atrium_spv_ir::Inst],
    block_term_idx: &HashMap<BlockId, usize>,
    block_flat_start: &HashMap<BlockId, usize>,
) -> (HashMap<ValueId, usize>, HashMap<ValueId, u32>) {
    let mut last_use: HashMap<ValueId, usize> = HashMap::new();
    let mut use_counts: HashMap<ValueId, u32> = HashMap::new();
    let mut vec_lanes: HashMap<ValueId, Vec<ValueId>> = HashMap::new();
    // Phi arm uses are deferred: a Phi arm `(value, from)`
    // is *read* by the phi-move emitted at `from`'s
    // terminator — which on a back-edge is LATER in flat
    // order than the Phi itself. Collect them and apply
    // after the per-inst sweep, marking each at its
    // predecessor block's terminator index.
    let mut phi_arm_uses: Vec<(ValueId, BlockId)> = Vec::new();
    for (i, inst) in insts.iter().enumerate() {
        let mut mark = |id: ValueId| {
            *use_counts.entry(id).or_insert(0) += 1;
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
                // If either operand is a vec, its lanes
                // need to be alive too (per-lane scalars
                // are read at this op's index).
                if let Some(lanes) = vec_lanes.get(&l.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
                if let Some(lanes) = vec_lanes.get(&r.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::FNeg(s) => mark(s.id),
            // Integer arithmetic / bitwise / shifts /
            // comparisons — operands are scalar W-reg
            // values. Without these arms an int operand's
            // live range is never recorded, so the
            // linear-scan reclaims its W-reg the moment a
            // stale seed (e.g. a Load's `or_insert`) falls
            // behind `i` — which is exactly what made a
            // loop-invariant `n` get clobbered mid-loop.
            Op::IAdd(l, r) | Op::ISub(l, r) | Op::IMul(l, r)
            | Op::SDiv(l, r) | Op::UDiv(l, r)
            | Op::SMod(l, r) | Op::UMod(l, r)
            | Op::BitAnd(l, r) | Op::BitOr(l, r) | Op::BitXor(l, r)
            | Op::Shl(l, r) | Op::LShr(l, r) | Op::AShr(l, r)
            | Op::IEq(l, r) | Op::INe(l, r)
            | Op::SLt(l, r) | Op::SLe(l, r)
            | Op::SGt(l, r) | Op::SGe(l, r)
            | Op::ULt(l, r) | Op::ULe(l, r)
            | Op::UGt(l, r) | Op::UGe(l, r) => {
                mark(l.id); mark(r.id);
            }
            Op::INeg(s) | Op::BitNot(s)
            | Op::ConvertSToF(s) | Op::ConvertUToF(s)
            | Op::ConvertFToS(s) | Op::ConvertFToU(s) => mark(s.id),
            Op::Dot(l, r) => {
                mark(l.id); mark(r.id);
                if let Some(lanes) = vec_lanes.get(&l.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
                if let Some(lanes) = vec_lanes.get(&r.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::VectorShuffle { src1, src2, .. } => {
                mark(src1.id); mark(src2.id);
                // Shuffle aliases lanes — they must stay
                // live forever past this op since the
                // shuffle result reuses their V-regs.
                if let Some(lanes) = vec_lanes.get(&src1.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
                if let Some(lanes) = vec_lanes.get(&src2.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::VectorExtract { vector, .. } => {
                mark(vector.id);
                // The extracted lane's S-reg is aliased
                // into the result; keep all source lanes
                // live through the extract.
                if let Some(lanes) = vec_lanes.get(&vector.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::FOrdEq(l, r) | Op::FOrdNe(l, r)
            | Op::FOrdLt(l, r) | Op::FOrdLe(l, r)
            | Op::FOrdGt(l, r) | Op::FOrdGe(l, r) => {
                mark(l.id); mark(r.id);
            }
            Op::BranchCond { cond, .. } => mark(cond.id),
            Op::Switch { selector, .. } => mark(selector.id),
            Op::Select { cond, t_val, f_val } => {
                mark(cond.id); mark(t_val.id); mark(f_val.id);
                // Vec operands' lane scalars need to stay
                // alive across the Select — without this
                // the per-lane fcsel_s reads scalars[lane]
                // for V-regs that may have already been
                // recycled by the linear-scan.
                if let Some(lanes) = vec_lanes.get(&t_val.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
                if let Some(lanes) = vec_lanes.get(&f_val.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::Phi(arms) => {
                // Defer: each arm's value is read by the
                // phi-move at `arm.from`'s terminator, not
                // here. Marking at the Phi's own index
                // would give a back-edge value a live
                // range that ENDS before it's even
                // defined — the linear-scan would then
                // recycle its reg, colliding with the
                // value computed right after it (the
                // classic "i_next and acc_next share a
                // reg → infinite loop" bug).
                for arm in arms {
                    phi_arm_uses.push((arm.value.id, arm.from));
                }
            }
            Op::AccessChain { base: _, byte_offset: _ } => {
                // Base is a pointer Value, not a scalar —
                // no V-reg liveness implication.
            }
            Op::Load(_) => {
                // The result may produce a vec4; if so,
                // its synthetic lane scalars get their
                // last_use set when we walk the Store.
                // The result itself is a fresh scalar
                // value (handled by alloc_vreg + Store).
                // For a scalar Load result, last_use will
                // be populated by whatever later op
                // references it (FAdd / Store etc.).
                if let Some(r) = inst.result.as_ref() {
                    // Seed last_use so a Load result
                    // unused later still has a defined
                    // last_use of its own def.
                    last_use.entry(r.id).or_insert(i);
                }
            }
            Op::Store { ptr: _, value } => {
                mark(value.id);
                if let Some(lane_ids) = vec_lanes.get(&value.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            _ => {}
        }
    }

    // Apply deferred Phi arm uses. Each arm value is read
    // by the phi-move at its predecessor block's
    // terminator — extend its live range there so the
    // linear-scan doesn't recycle the reg before the
    // back-edge writes it.
    for (value_id, from) in phi_arm_uses {
        // Count the phi-arm read too, so `use_counts` stays
        // a true total — a value that is both a comparison
        // result and a Phi arm source must not look
        // single-use to the fusion check.
        *use_counts.entry(value_id).or_insert(0) += 1;
        if let Some(&term) = block_term_idx.get(&from) {
            last_use.entry(value_id)
                .and_modify(|e| *e = (*e).max(term))
                .or_insert(term);
        }
    }

    // ── Loop liveness extension ───────────────────────────────
    //
    // Flat-order linear-scan can't see that a loop body
    // re-executes: a loop-invariant value (e.g. the
    // iteration count `n`, loaded in the preheader, read
    // in the loop header's compare) has a flat last_use
    // *inside* the loop — earlier than the back-edge — so
    // the allocator would recycle its register mid-loop
    // and a value computed in the loop tail would clobber
    // it. Fix: identify each loop by its back-edge (a
    // `Branch` whose target block starts at a LOWER flat
    // index than the branch itself) and extend every
    // value whose last_use lands inside the loop's flat
    // span to the loop's end (the back-edge index).
    // Iterated to a fixpoint to handle nested loops.
    let mut loops: Vec<(usize, usize)> = Vec::new(); // (start, end)
    for (i, inst) in insts.iter().enumerate() {
        if let Op::Branch(target) = &inst.op {
            if let Some(&start) = block_flat_start.get(target) {
                if start <= i {
                    loops.push((start, i));
                }
            }
        }
    }
    loop {
        let mut changed = false;
        for &(start, end) in &loops {
            for lu in last_use.values_mut() {
                if *lu >= start && *lu < end {
                    *lu = end;
                    changed = true;
                }
            }
        }
        if !changed { break; }
    }

    (last_use, use_counts)
}

/// Emit one polymorphic float binary op (fadd/fsub/fmul/
/// fdiv). Dispatches on operand shape:
///
/// * scalar × scalar → one S-reg op.
/// * vec   × vec     → per-lane S-reg ops (lane count
///                     must match).
/// * vec   × scalar  → broadcast scalar to every lane.
/// * scalar × vec    → same, symmetric.
///
/// Result is stored in `scalars` (scalar shape) or
/// `vectors` (vec shape).
#[allow(clippy::too_many_arguments)]
fn emit_fp_binop_poly(
    a: &mut asm::Asm,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    vectors: &mut HashMap<ValueId, Vec<Value>>,
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    next_synth_id: &mut u32,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    make_inst: fn(asm::Vreg, asm::Vreg, asm::Vreg) -> u32,
) -> Result<(), BackendError> {
    let mut fresh_synth = || {
        let id = ValueId(*next_synth_id);
        *next_synth_id += 1;
        id
    };
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("fp binop without result".into()))?;

    let lhs_is_vec = vectors.contains_key(&lhs.id);
    let rhs_is_vec = vectors.contains_key(&rhs.id);

    match (lhs_is_vec, rhs_is_vec) {
        (false, false) => {
            // Scalar × scalar.
            let l = *scalars.get(&lhs.id).ok_or_else(||
                BackendError::Internal(format!(
                    "fp binop lhs {:?} missing", lhs.id)))?;
            let r = *scalars.get(&rhs.id).ok_or_else(||
                BackendError::Internal(format!(
                    "fp binop rhs {:?} missing", rhs.id)))?;
            let d = alloc_vreg(free_pool, owners, result.id)?;
            a.emit(make_inst(d, l, r));
            scalars.insert(result.id, d);
        }
        (true, true) => {
            // Vec × vec lane-walk.
            let l_lanes = vectors.get(&lhs.id).unwrap().clone();
            let r_lanes = vectors.get(&rhs.id).unwrap().clone();
            if l_lanes.len() != r_lanes.len() {
                return Err(BackendError::Unsupported(format!(
                    "vec fp binop mismatched lanes: {} vs {}",
                    l_lanes.len(), r_lanes.len())));
            }
            let mut out_lanes = Vec::with_capacity(l_lanes.len());
            for (li, (ll, rl)) in l_lanes.iter().zip(r_lanes.iter()).enumerate() {
                let l = *scalars.get(&ll.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "vec fp binop lane {li} lhs {:?} missing",
                        ll.id)))?;
                let r = *scalars.get(&rl.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "vec fp binop lane {li} rhs {:?} missing",
                        rl.id)))?;
                // Synthetic per-lane result ValueId.
                let synth = fresh_synth();
                let d = alloc_vreg(free_pool, owners, synth)?;
                a.emit(make_inst(d, l, r));
                scalars.insert(synth, d);
                out_lanes.push(Value {
                    id: synth, ty: ll.ty.clone(),
                });
            }
            vectors.insert(result.id, out_lanes);
        }
        (true, false) => {
            // Vec × scalar broadcast.
            let l_lanes = vectors.get(&lhs.id).unwrap().clone();
            let r = *scalars.get(&rhs.id).ok_or_else(||
                BackendError::Internal(format!(
                    "vec×scalar fp binop scalar {:?} missing", rhs.id)))?;
            let mut out_lanes = Vec::with_capacity(l_lanes.len());
            for (li, ll) in l_lanes.iter().enumerate() {
                let l = *scalars.get(&ll.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "vec×scalar lane {li} lhs missing")))?;
                let synth = fresh_synth();
                let d = alloc_vreg(free_pool, owners, synth)?;
                a.emit(make_inst(d, l, r));
                scalars.insert(synth, d);
                out_lanes.push(Value { id: synth, ty: ll.ty.clone() });
            }
            vectors.insert(result.id, out_lanes);
        }
        (false, true) => {
            // Scalar × vec broadcast (symmetric).
            let r_lanes = vectors.get(&rhs.id).unwrap().clone();
            let l = *scalars.get(&lhs.id).ok_or_else(||
                BackendError::Internal(format!(
                    "scalar×vec fp binop scalar {:?} missing", lhs.id)))?;
            let mut out_lanes = Vec::with_capacity(r_lanes.len());
            for (li, rl) in r_lanes.iter().enumerate() {
                let r = *scalars.get(&rl.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "scalar×vec lane {li} rhs missing")))?;
                let synth = fresh_synth();
                let d = alloc_vreg(free_pool, owners, synth)?;
                a.emit(make_inst(d, l, r));
                scalars.insert(synth, d);
                out_lanes.push(Value { id: synth, ty: rl.ty.clone() });
            }
            vectors.insert(result.id, out_lanes);
        }
    }
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
