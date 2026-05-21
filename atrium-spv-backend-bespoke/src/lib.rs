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

/// Result of [`compile_blob`] — the JIT-emit counterpart
/// of [`CompileOutput`].
#[derive(Debug, Clone)]
pub struct BlobOutput {
    /// Serialised `atrium-spv-blob` container: a flat
    /// position-independent code blob plus a per-stage
    /// entry-offset table. The loader `mmap`s this
    /// `PROT_EXEC` directly — no `cc`, no `dlopen`.
    pub blob: Vec<u8>,
    /// PC-map sidecar bytes (atrium-spv-pcmap format v1),
    /// identical to [`CompileOutput::pcmap`].
    pub pcmap: Vec<u8>,
}

/// Compile an atrium-spv-ir module straight to a flat
/// executable blob — no ELF object, no linker.
///
/// This is the fast path: the bespoke backend already
/// emits self-contained PIC machine code (branches patched
/// in-backend, constants materialised inline, the fragment
/// ABI entirely register/pointer), so there is nothing for
/// `cc` to do — `compile_blob` just concatenates the
/// per-function bodies and records where each stage's
/// entry point lands. It is strictly *less* work than
/// [`compile`], which wraps the same bytes in an
/// `object::write::Object` so `cc -shared` can re-derive a
/// `.so` from them.
///
/// `target` is accepted for API symmetry with [`compile`]
/// but the emitted code is OS-agnostic — a flat AAPCS64
/// blob is identical whether the eventual host is FreeBSD
/// or Darwin; only the architecture matters.
pub fn compile_blob(module: &Module, target: Target)
    -> Result<BlobOutput, BackendError>
{
    let _ = target; // aarch64 either way; see doc comment.
    let mut code: Vec<u8> = Vec::new();
    let mut pcmap = atrium_spv_pcmap::Builder::new();
    let mut entries = atrium_spv_blob::EntryOffsets::default();

    for func in &module.functions {
        let (body, pc_entries) = emit_function(func)?;
        // Each body is a whole number of 4-byte ARM64
        // instructions, so concatenating keeps every
        // function — and therefore every entry offset —
        // 4-byte aligned without explicit padding.
        let off = code.len() as u32;
        for (rel_host, spirv) in pc_entries {
            pcmap.push(off + rel_host, spirv);
        }
        match func.stage {
            ShaderStage::Vertex   => entries.vs = Some(off),
            ShaderStage::Fragment => entries.fs = Some(off),
            ShaderStage::Compute  => entries.cs = Some(off),
        }
        code.extend_from_slice(&body);
    }

    let blob = atrium_spv_blob::ShaderBlob {
        arch: atrium_spv_blob::ARCH_AARCH64,
        code,
        entries,
    };
    Ok(BlobOutput {
        blob: blob.to_bytes(),
        pcmap: pcmap.finish_to_bytes(),
    })
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
    // Compute is enabled with the AAPCS64 9-param signature
    // (uniforms@X0, push_constants@X1, out_buffer@X2,
    // wg_id[xyz]@W3..W5, local_id[xy]@W6..W7, local_id[z]
    // at [SP+0]). Storage class mapping for Compute and the
    // Op::LoadBuiltin codegen for the wg/lid builtins are
    // wired below; ops the bespoke compute path can't yet
    // emit (OpCompositeExtract on uvec3, scalar StorageBuffer
    // stores, etc.) still return BackendError::Unsupported
    // and atrium-spv-compile falls back to Cranelift for
    // that shader -- the foundation gets us closer
    // incrementally without breaking existing behaviour.

    let mut a = asm::Asm::new();
    // PC-map entries: (body-relative host byte offset,
    // source SPIR-V offset) — one per IR inst, pushed in
    // codegen-walk order. `a` only ever grows, so the host
    // offsets come out monotone non-decreasing.
    let mut pcmap_entries: Vec<(u32, u32)> = Vec::new();
    // scalars[id] = Vreg holding the live f32 (S-reg view).
    let mut scalars: HashMap<ValueId, asm::Vreg> = HashMap::new();
    let mut vectors: HashMap<ValueId, Vec<Value>> = HashMap::new();
    // Mat4 values: deferred lazy load.  Op::Load of a Mat4
    // does NOT materialise the 16 lane scalars (would burn
    // 16 V-regs across a long live range covering the rest
    // of the function and exhaust the 24-V-reg pool when
    // combined with vec3 inputs + constant + 4
    // accumulators).  Instead we just remember the (base
    // X-reg, byte offset) of the matrix; Op::MatrixTimesVector
    // loads each column lane on demand into a single
    // recycled temp V-reg.  16 ldr_w / fmov pairs + 4
    // accumulator V-regs + 1 temp V-reg = 5 V-regs
    // simultaneous, comfortably under the budget.
    let mut matrices_ptr: HashMap<ValueId, (asm::Xreg, i32)> = HashMap::new();
    // NEON-packed vec4 values: the whole vector in one
    // Q-register, driven with `.4s` ops. A vec4 ValueId is
    // in *either* `vectors` (per-lane) or `packed`, never
    // both — the NEON-pack classifier (below) decides.
    let mut packed: HashMap<ValueId, asm::Vreg> = HashMap::new();
    // IR Value (image / sampler / sampled-image handle) →
    // the SPIR-V `(DescriptorSet, Binding)` it represents.
    // Set by `Op::ImageHandle`; propagated unchanged by
    // `Op::CombineSampledImage`; consumed by image-sample
    // codegen to compute the v1-ABI descriptor-table offset
    // (matches the side-table the Cranelift backend uses).
    let mut image_handles: HashMap<ValueId, (u32, u32)> = HashMap::new();
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

    // Stage-specific "primary output pointer" register
    // (the AAPCS64 slot the shader writes its main result
    // through). Fragment: X4 holds `out_color`. Vertex:
    // X6 holds `out_position` (per the vertex ABI in
    // docs/spec/tier2-renderer.md §4.1). Scratch X9/W9
    // for constant materialisation + fmov bridging.
    let x_out = match func.stage {
        ShaderStage::Fragment => asm::Xreg(4),
        ShaderStage::Vertex   => asm::Xreg(6),
        // Compute's primary writable pointer is the SSBO
        // out_buffer at X2 (3rd AAPCS64 arg, after uniforms
        // + push_constants).  Op::Store through a
        // StorageBuffer pointer routes here.
        ShaderStage::Compute  => asm::Xreg(2),
    };
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

    // Flat index of each result value's defining inst —
    // Phi-move coalescing uses it to locate a Phi arm's
    // producer and check the Phi value isn't read past it.
    let mut value_def_flat_idx: HashMap<ValueId, usize> = HashMap::new();
    for (idx, inst) in flat_insts.iter().enumerate() {
        if let Some(r) = inst.result.as_ref() {
            value_def_flat_idx.insert(r.id, idx);
        }
    }

    // Predecessor map: which blocks branch into each block.
    // Phi-move coalescing uses it to recognise the
    // body→continue shape — a Phi arm's value is produced
    // in the loop body but the arm's `from` is the
    // continue block (the header's immediate predecessor).
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for bid in &block_order {
        let block = func.blocks.get(bid).unwrap();
        match block.insts.last().map(|i| &i.op) {
            Some(Op::Branch(t)) => preds.entry(*t).or_default().push(*bid),
            Some(Op::BranchCond { t_block, f_block, .. }) => {
                preds.entry(*t_block).or_default().push(*bid);
                preds.entry(*f_block).or_default().push(*bid);
            }
            Some(Op::Switch { cases, default, .. }) => {
                preds.entry(*default).or_default().push(*bid);
                for (_, t) in cases {
                    preds.entry(*t).or_default().push(*bid);
                }
            }
            _ => {}
        }
    }

    // ── Immediate-fold pre-pass (opt #3) ──────────────────────
    //
    // A small integer constant used *only* as an add/sub
    // operand doesn't need its own W-reg — it can ride in
    // the instruction's imm12 field (`add w,w,#1` instead
    // of `mov w,#1` + `add w,w,w`). This both shrinks the
    // prologue and, more importantly, frees a W-reg for the
    // whole function — easing the pressure that opt #5
    // (spilling) addresses.
    //
    // A ConstInt is foldable when its value fits imm12
    // (0..=4095) and every use is a foldable add/sub
    // position: either operand of `IAdd` (commutative) or
    // the RHS of `ISub`. Any other use — Phi arm, IMul,
    // Store, a second constant operand — disqualifies it.
    let mut const_int_value: HashMap<ValueId, i64> = HashMap::new();
    for inst in &flat_insts {
        if let Op::ConstInt { value, .. } = &inst.op {
            if let Some(r) = inst.result.as_ref() {
                const_int_value.insert(r.id, *value);
            }
        }
    }
    let mut fold_const: std::collections::HashSet<ValueId> =
        const_int_value.iter()
            .filter(|(_, &v)| (0..=4095).contains(&v))
            .map(|(&id, _)| id)
            .collect();
    for inst in &flat_insts {
        match &inst.op {
            Op::IAdd(l, r) => {
                // A candidate operand here is a foldable
                // use — unless *both* operands are
                // constants (can't fold both into one
                // instruction), in which case neither
                // folds.
                if const_int_value.contains_key(&l.id)
                    && const_int_value.contains_key(&r.id)
                {
                    fold_const.remove(&l.id);
                    fold_const.remove(&r.id);
                }
            }
            Op::ISub(l, r) => {
                // Only the RHS rides in `sub_imm`. An LHS
                // constant is a non-foldable use.
                fold_const.remove(&l.id);
                if const_int_value.contains_key(&l.id)
                    && const_int_value.contains_key(&r.id)
                {
                    fold_const.remove(&r.id);
                }
            }
            other => {
                // Any other op that reads a candidate uses
                // it non-foldably.
                fold_const.retain(|cid| !op_reads(other, *cid));
            }
        }
    }

    // ── NEON-pack classifier (phase 1) ────────────────────────
    //
    // A vec4 value can live in a single Q-register and be
    // driven with `.4s` ops instead of the four-scalar
    // lane-walk — but only if its entire def-use subgraph is
    // "pack-friendly": produced by a `ConstVec` or a vec×vec
    // FP binop, and consumed only by vec×vec FP binops or a
    // whole-vector `Store`. Anything lane-addressed (Shuffle,
    // Extract, Dot, Phi, Select, Composite*, Insert,
    // AccessChain) forces the per-lane representation. The
    // two representations don't mix mid-graph yet (no
    // packed↔lanes bridge — a later phase), so one tainted
    // value disqualifies its whole connected component.
    //
    // Implementation: seed `disqualified` from every vec4
    // value touched by a non-pack-friendly op, then
    // fixed-point propagate across vec×vec FP binop cliques
    // (result + both operands share a fate).
    let is_vec4 = |ty: &Type| matches!(ty, Type::Vec4(_));
    // Every vec4-typed SSA value (each is some inst's result).
    let mut vec4_ids: std::collections::HashSet<ValueId> =
        std::collections::HashSet::new();
    for inst in &flat_insts {
        if let Some(r) = inst.result.as_ref() {
            if is_vec4(&r.ty) { vec4_ids.insert(r.id); }
        }
    }
    // A vec×vec FP binop is the only pack-friendly
    // *propagating* producer (ConstVec is a leaf, Store a
    // sink). Returns the two operand ids when it matches.
    let vecvec_fp_binop = |op: &Op| -> Option<(ValueId, ValueId)> {
        match op {
            Op::FAdd(l, r) | Op::FSub(l, r)
            | Op::FMul(l, r) | Op::FDiv(l, r)
                if is_vec4(&l.ty) && is_vec4(&r.ty) =>
                Some((l.id, r.id)),
            _ => None,
        }
    };
    // True scalar constants — the only ConstVec elements we
    // pack directly. A ConstVec whose lanes are *computed*
    // scalars (a CompositeConstruct of extracted/derived
    // values) must stay per-lane: its element S-regs alias
    // a per-lane subgraph, and assembling them into a fresh
    // Q-register can clobber a still-live aliased reg. Such
    // a ConstVec is treated as non-friendly below.
    let mut const_float_ids: std::collections::HashSet<ValueId> =
        std::collections::HashSet::new();
    for inst in &flat_insts {
        if matches!(&inst.op, Op::ConstFloat { .. }) {
            if let Some(r) = inst.result.as_ref() {
                const_float_ids.insert(r.id);
            }
        }
    }
    let pure_const_vec = |op: &Op| -> bool {
        matches!(op, Op::ConstVec(els)
            if els.iter().all(|e| const_float_ids.contains(&e.id)))
    };
    // A vec4 Phi is a *propagating* clique like a vec×vec
    // FP binop — its result and every arm value share a
    // fate. Returns (result_id, arm_ids) when it matches.
    let vec4_phi = |inst: &atrium_spv_ir::Inst| -> Option<(ValueId, Vec<ValueId>)> {
        match (&inst.op, inst.result.as_ref()) {
            (Op::Phi(arms), Some(r)) if is_vec4(&r.ty) =>
                Some((r.id, arms.iter().map(|a| a.value.id).collect())),
            _ => None,
        }
    };
    let mut disqualified: std::collections::HashSet<ValueId> =
        std::collections::HashSet::new();
    for inst in &flat_insts {
        let friendly = pure_const_vec(&inst.op)
            || vecvec_fp_binop(&inst.op).is_some()
            || vec4_phi(inst).is_some()
            || matches!(&inst.op,
                Op::Store { value, .. } if is_vec4(&value.ty));
        if friendly { continue; }
        // Non-friendly op: every vec4 it defines or reads is
        // tainted.
        if let Some(r) = inst.result.as_ref() {
            if is_vec4(&r.ty) { disqualified.insert(r.id); }
        }
        for vid in &vec4_ids {
            if op_reads(&inst.op, *vid) { disqualified.insert(*vid); }
        }
    }
    // Fixed-point: a vec×vec FP binop's result and both
    // operands must agree — taint all if any. A vec4 Phi
    // propagates the same way across {result, all arms}.
    loop {
        let mut changed = false;
        for inst in &flat_insts {
            if let Some((l, r)) = vecvec_fp_binop(&inst.op) {
                let res = inst.result.as_ref().map(|x| x.id);
                let any = disqualified.contains(&l)
                    || disqualified.contains(&r)
                    || res.is_some_and(|x| disqualified.contains(&x));
                if any {
                    changed |= disqualified.insert(l);
                    changed |= disqualified.insert(r);
                    if let Some(x) = res {
                        changed |= disqualified.insert(x);
                    }
                }
            }
            if let Some((res, arms)) = vec4_phi(inst) {
                let any = disqualified.contains(&res)
                    || arms.iter().any(|a| disqualified.contains(a));
                if any {
                    changed |= disqualified.insert(res);
                    for a in arms {
                        changed |= disqualified.insert(a);
                    }
                }
            }
        }
        if !changed { break; }
    }
    let packed_ids: std::collections::HashSet<ValueId> =
        vec4_ids.difference(&disqualified).copied().collect();

    // ── ConstVec literal-pool pre-pass ────────────────────────
    //
    // Every pack-friendly `ConstVec` (vec4 of genuine
    // `ConstFloat`s, classified as packed) gets a deduped
    // slot in a per-function literal pool laid out after
    // the final `ret`. At emit time the ConstVec lowers to
    // a single `ldr q, [pc-rel]` placeholder; after the
    // function body is fully emitted (post per-block loop,
    // before branch fixup) the pool is laid out 16-byte
    // aligned and each placeholder's `imm19` is patched
    // with the resolved PC-relative instruction delta.
    //
    // This is the obvious analogue of what `clang -O2` does
    // for vec constants — replacing a 12+-insn per-lane
    // build (`movz/movk` + `fmov s,w` + `ins v.s[k]`) with
    // one PIC load.
    let mut const_float_bits: HashMap<ValueId, u32> = HashMap::new();
    for inst in &flat_insts {
        if let Op::ConstFloat { value, kind: _ } = &inst.op {
            if let Some(r) = inst.result.as_ref() {
                const_float_bits.insert(r.id, (*value as f32).to_bits());
            }
        }
    }
    let mut pool_slots: Vec<[u32; 4]> = Vec::new();
    let mut constvec_pool_slot: HashMap<ValueId, usize> = HashMap::new();
    for inst in &flat_insts {
        if let (Op::ConstVec(els), Some(r)) = (&inst.op, inst.result.as_ref()) {
            if !packed_ids.contains(&r.id) { continue; }
            if els.len() != 4 { continue; }
            let mut lanes = [0u32; 4];
            let mut all_const = true;
            for (i, el) in els.iter().enumerate() {
                match const_float_bits.get(&el.id) {
                    Some(b) => lanes[i] = *b,
                    None => { all_const = false; break; }
                }
            }
            if !all_const { continue; }
            let slot = pool_slots.iter().position(|s| s == &lanes)
                .unwrap_or_else(|| {
                    pool_slots.push(lanes);
                    pool_slots.len() - 1
                });
            constvec_pool_slot.insert(r.id, slot);
        }
    }
    // Placeholders to patch once the pool's start offset is
    // known: (asm byte offset of the `ldr q`, slot index,
    // destination Q-reg).
    let mut pool_patches: Vec<(usize, usize, asm::Vreg)> = Vec::new();

    // ConstFloats whose every consumer is a pool-eligible
    // ConstVec can be skipped entirely: their bit pattern
    // lives in the pool, never in an S-reg. Without this,
    // each pool ConstVec would still drag 4 dead `movz/movk
    // + fmov s,w` ops through the prologue.
    let mut dead_const_floats: std::collections::HashSet<ValueId> =
        std::collections::HashSet::new();
    'cf: for inst in &flat_insts {
        if !matches!(&inst.op, Op::ConstFloat { .. }) { continue; }
        let Some(r) = inst.result.as_ref() else { continue; };
        let cid = r.id;
        let mut any_use = false;
        for u in &flat_insts {
            if !op_reads(&u.op, cid) { continue; }
            any_use = true;
            // Only pool-eligible ConstVecs are "transparent"
            // consumers — any other reader keeps the
            // ConstFloat alive.
            let pool_consumer = matches!(&u.op, Op::ConstVec(_))
                && u.result.as_ref()
                    .is_some_and(|ur| constvec_pool_slot.contains_key(&ur.id));
            if !pool_consumer { continue 'cf; }
        }
        if any_use { dead_const_floats.insert(cid); }
    }

    // ── Linear-scan register allocator ─────────────────────────
    //
    // Free pool of V-regs. Two tiers:
    //   * V16..V31 — caller-saved per AAPCS64, no prologue
    //     cost; the preferred tier (handed out first).
    //   * V8..V15  — callee-saved (the low 64 bits must be
    //     preserved). Using one forces an `stp d`/`ldp d`
    //     prologue+epilogue, exactly like the W19..W28
    //     integer overflow tier — so they're the overflow
    //     tier, popped only once V16..V31 is exhausted.
    // `pop` takes from the end, so the vec is ordered
    // [V15..V8, V31..V16] → V16 first, V8..V15 last.
    // At each inst i, before defining any new value, expire
    // scalars whose last_use < i and return their V-regs to
    // the pool. Then allocate from the pool for new defs.
    let mut free_pool: Vec<u8> = (8..16).rev().collect();
    free_pool.extend((16..32).rev());
    let mut owners: HashMap<u8, ValueId> = HashMap::new();
    // Set once any V8..V15 reg is handed out — drives the
    // callee-saved FP `stp d`/`ldp d` prologue+epilogue.
    let mut used_callee_saved_v = false;

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
    // ── Phi-move coalescing ───────────────────────────────────
    //
    // `coalesce_into[v] = D` means "when the instruction
    // producing value `v` is emitted, write its result
    // straight into the Phi register `D` instead of a
    // fresh register" — so the phi-move `fmov D, <v's reg>`
    // becomes an identity and is dropped. These `fmov`s sit
    // directly on a loop's carried dependency chain, so
    // removing them is the actual runtime lever (opt #4).
    //
    // Safe to coalesce a Phi arm `(src, from)` into the
    // Phi's register `D` when, all together:
    //   * `src` is produced by a scalar binary op P in
    //     block `from` (so P goes through a coalesce-aware
    //     emit path);
    //   * `src` is single-use — only the phi-move reads it
    //     (`use_counts[src] == 1`);
    //   * the Phi's own value is not read *after* P, using
    //     the pre-extension `raw_last_use` (P may read it —
    //     ARM reads operands before writing the dest, so
    //     `add D,D,#1` is fine — but nothing later may).
    let mut coalesce_into: HashMap<ValueId, PhiDest> = HashMap::new();
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
                    if n < 16 { used_callee_saved_v = true; }
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
                // A vecN-f32 Phi is N per-lane scalar Phis
                // travelling together. Allocate one V-reg
                // per lane (never expired, like a scalar
                // Phi dest), register the result as a
                // vector of synthetic per-lane scalar
                // Values, and carry the lane regs in
                // PhiDest::Vec. Phi-move emission copies
                // lane-by-lane. No coalescing yet (vec-Phi
                // coalescing is a later phase).
                Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_)
                    if packed_ids.contains(&result.id) =>
                {
                    // NEON-packed vec4 Phi: one never-expired
                    // Q-reg. Phi-moves are `mov v.16b`; a
                    // coalesced .4s binop arm writes the
                    // Q-reg in place.
                    let n = free_pool.pop().ok_or_else(||
                        BackendError::Unsupported(
                            "out of V-regs allocating packed vec Phi dest".into()))?;
                    if n < 16 { used_callee_saved_v = true; }
                    let q = asm::Vreg(n);
                    packed.insert(result.id, q);
                    PhiDest::Packed(q)
                }
                Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                    let lane_count = match &result.ty {
                        Type::Vec2(_) => 2usize,
                        Type::Vec3(_) => 3usize,
                        Type::Vec4(_) => 4usize,
                        _ => unreachable!(),
                    };
                    let mut lane_regs = Vec::with_capacity(lane_count);
                    let mut lane_vals = Vec::with_capacity(lane_count);
                    for _ in 0..lane_count {
                        let n = free_pool.pop().ok_or_else(||
                            BackendError::Unsupported(
                                "out of V-regs allocating vec Phi dest".into()))?;
                        if n < 16 { used_callee_saved_v = true; }
                        let v = asm::Vreg(n);
                        let synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        scalars.insert(synth, v);
                        lane_regs.push(v);
                        lane_vals.push(Value { id: synth, ty: Type::F32 });
                    }
                    vectors.insert(result.id, lane_vals);
                    PhiDest::Vec(lane_regs)
                }
                other => return Err(BackendError::Unsupported(format!(
                    "Phi of type {other:?} not supported"))),
            };
            phi_target_blocks.insert(*bid);
            for arm in arms {
                phi_moves.entry((arm.from, *bid))
                    .or_default()
                    .push((dest.clone(), arm.value.id));

                // Decide coalescing for this arm.
                let src = arm.value.id;
                let Some(&p_idx) = value_def_flat_idx.get(&src) else {
                    continue;
                };
                let p = flat_insts[p_idx];
                // P must dominate the phi-move site (the
                // terminator of `arm.from`). Two provably
                // safe shapes:
                //   1. P is in `arm.from` itself — the
                //      phi-move comes right after P in the
                //      same straight-line block (induction
                //      variables: `i_next` in the continue
                //      block).
                //   2. P is in a block B whose sole job is
                //      to fall through to `arm.from`: B's
                //      terminator is `Branch(arm.from)` and
                //      B is `arm.from`'s only predecessor
                //      (loop accumulators: `a_next` is
                //      produced in the body, the arm's
                //      `from` is the continue block).
                let p_in = |b: &BlockId| -> bool {
                    block_flat_start.get(b).zip(block_term_idx.get(b))
                        .is_some_and(|(&s, &e)| s <= p_idx && p_idx <= e)
                };
                let case1 = p_in(&arm.from);
                let case2 = !case1
                    && preds.get(&arm.from)
                        .filter(|v| v.len() == 1)
                        .is_some_and(|v| {
                            let b = v[0];
                            p_in(&b)
                                && matches!(
                                    func.blocks.get(&b)
                                        .and_then(|bl| bl.insts.last())
                                        .map(|i| &i.op),
                                    Some(Op::Branch(t)) if *t == arm.from)
                        });
                if !(case1 || case2) { continue; }
                // P must be a binary op whose result type
                // matches the Phi register class, and it
                // must go through a coalesce-aware emit path
                // (emit_fp_binop_poly / emit_int_binop). A
                // Vec Phi coalesces an FP binop whose result
                // is the same-shaped vector: emit_fp_binop_poly
                // writes each lane straight into the Phi's
                // per-lane reg.
                let op_ok = match &dest {
                    PhiDest::Float(_) => matches!(&p.op,
                        Op::FAdd(..) | Op::FSub(..)
                        | Op::FMul(..) | Op::FDiv(..)),
                    PhiDest::Int(_) => matches!(&p.op,
                        Op::IAdd(..) | Op::ISub(..) | Op::IMul(..)
                        | Op::SDiv(..) | Op::UDiv(..)
                        | Op::SMod(..) | Op::UMod(..)
                        | Op::BitAnd(..) | Op::BitOr(..) | Op::BitXor(..)
                        | Op::Shl(..) | Op::LShr(..) | Op::AShr(..)),
                    PhiDest::Vec(_) => matches!(&p.op,
                        Op::FAdd(..) | Op::FSub(..)
                        | Op::FMul(..) | Op::FDiv(..)),
                    PhiDest::Packed(_) => matches!(&p.op,
                        Op::FAdd(..) | Op::FSub(..)
                        | Op::FMul(..) | Op::FDiv(..)),
                };
                if !op_ok { continue; }
                // `src` single-use (only the phi-move reads it).
                if use_counts.get(&src).copied() != Some(1) { continue; }
                // The Phi value must not be read by anything
                // that executes *between* P and the
                // back-edge phi-move. Reads BEFORE P (this
                // iteration's Phi value, e.g. the header
                // compare or body uses ahead of P) are fine
                // — P hasn't overwritten the register yet.
                // Reads AFTER the loop (the merge block) are
                // fine too — the register holds the final
                // back-edge value, which is exactly what the
                // Phi resolves to on exit. So scan only the
                // half-open flat span (P, back-edge].
                let back_edge_idx = block_term_idx.get(&arm.from)
                    .copied().unwrap_or(p_idx);
                let phi_id = result.id;
                let read_between = (p_idx + 1..=back_edge_idx)
                    .any(|fi| op_reads(&flat_insts[fi].op, phi_id));
                if read_between { continue; }
                coalesce_into.insert(src, dest.clone());
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
    // Both register pools can dip into callee-saved
    // registers under pressure: the int pool into X19..X28,
    // the V-reg pool into V8..V15. Per AAPCS64 those must
    // be saved/restored across the call. We don't know yet
    // which (if any) get used, so reserve a fixed
    // PROLOGUE_INSTS-instruction region (NOPs for now) and
    // patch it after body emission. A fixed size keeps
    // every body offset — block_asm_offset, branch patch
    // sites — consistent regardless of whether the prologue
    // ends up real or NOPs.
    //
    // Layout: 5 slots for the integer pairs (X19/X20 …
    // X27/X28) then 4 slots for the FP pairs (D8/D9 …
    // D14/D15). The two halves are independent — either,
    // both, or neither may be real; unused slots stay NOPs.
    const PROLOGUE_INT_INSTS: usize = 5;
    const PROLOGUE_FP_INSTS: usize = 4;
    const PROLOGUE_INSTS: usize = PROLOGUE_INT_INSTS + PROLOGUE_FP_INSTS;
    let prologue_off = a.len();
    for _ in 0..PROLOGUE_INSTS { a.emit(asm::nop()); }
    // Epilogue placeholder offsets — one per `ret`. Each is
    // a PROLOGUE_INSTS-NOP region emitted just before its
    // ret. Mirror-image layout: 4 FP slots then 5 int slots
    // (restores run in reverse of the prologue's saves).
    let mut epilogue_offs: Vec<usize> = Vec::new();

    let mut flat_i: usize = 0;
    for (block_pos, bid) in block_order.iter().enumerate() {
        let block = func.blocks.get(bid).unwrap();
        block_asm_offset.insert(*bid, a.len());
        // The block emitted immediately after this one, if
        // any — branch-to-next-block elision drops an
        // unconditional `b` whose target is this block.
        let next_block: Option<BlockId> =
            block_order.get(block_pos + 1).copied();
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

        // Phi-move coalescing target for this instruction's
        // result, if any (see the Phi pre-pass). When set,
        // the producing binop writes straight into the Phi
        // register and the corresponding phi-move is
        // dropped below. Split into the V-reg / W-reg forms
        // the two binop emit helpers expect.
        let coalesce_dest: Option<PhiDest> = inst.result.as_ref()
            .and_then(|r| coalesce_into.get(&r.id).cloned());
        // The FP binop helper takes the whole `PhiDest` (it
        // handles both `Float` scalar coalescing and `Vec`
        // per-lane coalescing); the int helper still wants
        // the bare W-reg.
        let coalesce_w: Option<asm::Wreg> = match &coalesce_dest {
            Some(PhiDest::Int(w)) => Some(*w),
            _ => None,
        };

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
                // Folded constants (opt #3) ride in an
                // add/sub imm12 field — no W-reg, no
                // materialisation.
                if fold_const.contains(&result.id) {
                    // nothing to emit
                } else if let Ok(w) = alloc_int_w(&mut int_pool, result.id) {
                    materialise_u32_into_w(&mut a, w, *value as i32 as u32);
                    ints.insert(result.id, w);
                }
            }
            Op::ConstNull => {
                // Treat as an orphan — no W-reg.
            }
            Op::IAdd(l, r) => emit_int_addsub(
                &mut a, &mut ints, &mut int_pool, coalesce_w,
                &fold_const, &const_int_value, inst, l, r, false)?,
            Op::ISub(l, r) => emit_int_addsub(
                &mut a, &mut ints, &mut int_pool, coalesce_w,
                &fold_const, &const_int_value, inst, l, r, true)?,
            Op::IMul(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::mul_w)?,
            Op::SDiv(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::sdiv_w)?,
            Op::UDiv(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::udiv_w)?,
            // Bitwise + shifts.
            Op::BitAnd(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::and_w)?,
            Op::BitOr(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::orr_w)?,
            Op::BitXor(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::eor_w)?,
            Op::Shl(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::lslv_w)?,
            Op::LShr(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::lsrv_w)?,
            Op::AShr(l, r) => emit_int_binop(
                &mut a, &mut ints, &mut int_pool, coalesce_w, inst, l, r, asm::asrv_w)?,
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
                let d_v = alloc_vreg(&mut free_pool, &mut owners, &mut used_callee_saved_v,result.id)?;
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
                let d_v = alloc_vreg(&mut free_pool, &mut owners, &mut used_callee_saved_v,result.id)?;
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
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstFloat without result".into()))?;
                // Dead — only used by pool-eligible ConstVecs,
                // which read the literal pool instead. Skip
                // the materialise to drop the prologue cost.
                if dead_const_floats.contains(&result.id) { continue; }
                let bits = (*value as f32).to_bits();
                let v = alloc_vreg(&mut free_pool, &mut owners, &mut used_callee_saved_v,result.id)?;
                materialise_u32_into_w(&mut a, w_tmp, bits);
                a.emit(asm::fmov_s_from_w(v, w_tmp));
                scalars.insert(result.id, v);
            }
            Op::ConstVec(elements) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstVec without result".into()))?;
                if packed_ids.contains(&result.id) {
                    let q = alloc_vreg(&mut free_pool, &mut owners,
                        &mut used_callee_saved_v, result.id)?;
                    if let Some(&slot) = constvec_pool_slot.get(&result.id) {
                        // Literal-pool path: one `ldr q,
                        // [pc-rel]` placeholder; imm19 gets
                        // patched after the pool layout is
                        // known. No per-lane S-regs needed —
                        // their ConstFloat producers are
                        // skipped by `dead_const_floats`.
                        let off = a.len();
                        a.emit(asm::ldr_q_literal(q, 0));
                        pool_patches.push((off, slot, q));
                    } else {
                        // Fallback (mixed-shape ConstVec):
                        // assemble lanes via per-lane `ins`.
                        for (lane_i, el) in elements.iter().enumerate() {
                            let s = *scalars.get(&el.id).ok_or_else(||
                                BackendError::Internal(format!(
                                    "ConstVec lane {:?} not in scalars",
                                    el.id)))?;
                            a.emit(asm::ins_v_s(q, lane_i as u8, s, 0));
                        }
                    }
                    packed.insert(result.id, q);
                } else {
                    // Per-lane representation: each lane
                    // must already exist either in
                    // `scalars` (f32 lane → V-reg) or in
                    // `ints` (i32/u32 lane → W-reg). The
                    // latter is what an `ivec2` coord for
                    // `OpImageFetch` looks like.
                    for el in elements {
                        let in_scalars = scalars.contains_key(&el.id);
                        let in_ints    = ints.contains_key(&el.id);
                        if !(in_scalars || in_ints) {
                            return Err(BackendError::Unsupported(format!(
                                "ConstVec lane {:?} not in scalars or ints",
                                el.id)));
                        }
                    }
                    vectors.insert(result.id, elements.clone());
                }
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
                        let v = alloc_vreg(&mut free_pool, &mut owners, &mut used_callee_saved_v,synth)?;
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
                let acc = alloc_vreg(&mut free_pool, &mut owners, &mut used_callee_saved_v,result.id)?;
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
                    let tmp = alloc_vreg(&mut free_pool, &mut owners, &mut used_callee_saved_v,tmp_synth)?;
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
            // OpMatrixTimesVector — column-major:
            //   result[i] = Σ_j matrix[j][i] * vector[j]
            // Phase 4 v1 emits the per-lane scalar chain
            // (4 fmul + 12 fmul/fadd pairs).  Lanes are
            // loaded on demand into a single recycled temp
            // V-reg so the matrix doesn't burn 16 V-regs
            // of live range covering the rest of the
            // function.  4 accumulator V-regs (one per
            // result lane) + 1 col-load temp + 1 mul temp
            // = 6 V-regs simultaneously, well under budget.
            //
            // Phase 4 v2 will extend the NEON-pack
            // classifier to recognise this chain and emit
            // `fmul.4s / fmla.4s` instead.
            Op::MatrixTimesVector { matrix, vector } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "MatrixTimesVector without result".into()))?;
                let (mat_param, mat_off) = matrices_ptr.get(&matrix.id)
                    .copied()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "MatrixTimesVector matrix {:?} not in matrices_ptr",
                        matrix.id)))?;
                let vec_lanes = vectors.get(&vector.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "MatrixTimesVector vector {:?} not in vectors",
                        vector.id)))?;
                // v1: hard-coded 4-column Mat4 × 4-lane Vec4.
                // mat2/mat3 wait for a real shader that needs them.
                if vec_lanes.len() != 4 {
                    return Err(BackendError::Unsupported(format!(
                        "MatrixTimesVector: phase 4 v1 supports vec4 only \
                         ({}-lane given)", vec_lanes.len())));
                }
                let n_lanes = 4usize;
                let n_cols  = 4usize;
                // Helper to materialise one Mat4 lane into a
                // temp V-reg, do the work, then free the reg.
                // Closure-style would borrow `&mut a`, which
                // makes the borrow checker unhappy mid-loop,
                // so inline the load + free.
                let load_mat_lane = |a: &mut asm::Asm,
                                    free_pool: &mut Vec<u8>,
                                    owners: &mut HashMap<u8, ValueId>,
                                    used_csv: &mut bool,
                                    next_synth_id: &mut u32,
                                    col: usize, lane: usize|
                    -> Result<(asm::Vreg, u8), BackendError>
                {
                    let synth = ValueId(*next_synth_id);
                    *next_synth_id += 1;
                    let v = alloc_vreg(free_pool, owners, used_csv, synth)?;
                    let off = mat_off
                        .saturating_add((col * 16) as i32)
                        .saturating_add((lane * 4) as i32);
                    emit_load_f32_offset(a, w_tmp, mat_param, off, v)?;
                    Ok((v, v.0))
                };

                // 1) Initial broadcast: out[i] = col[0][i] * vec[0].
                //    Allocate 4 accumulators; load col[0][i] into a
                //    temp, do fmul into the acc, free the temp.
                let v0 = *scalars.get(&vec_lanes[0].id)
                    .ok_or_else(|| BackendError::Internal(format!(
                        "MatrixTimesVector vec[0] {:?} missing",
                        vec_lanes[0].id)))?;
                let mut out_lanes: Vec<Value> = Vec::with_capacity(n_lanes);
                for i in 0..n_lanes {
                    let acc_id = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let acc = alloc_vreg(
                        &mut free_pool, &mut owners,
                        &mut used_callee_saved_v, acc_id)?;
                    let (mat_v, mat_n) = load_mat_lane(
                        &mut a, &mut free_pool, &mut owners,
                        &mut used_callee_saved_v, &mut next_synth_id,
                        0, i)?;
                    a.emit(asm::fmul_s(acc, mat_v, v0));
                    // Free mat lane V-reg; its synth_id will
                    // never expire from last_use (not in map),
                    // but we don't need the data anymore.
                    free_pool.push(mat_n);
                    owners.remove(&mat_n);
                    scalars.insert(acc_id, acc);
                    out_lanes.push(Value { id: acc_id, ty: Type::F32 });
                }
                // 2) Accumulate remaining columns: out[i] += col[j][i] * vec[j].
                for j in 1..n_cols {
                    let vj = *scalars.get(&vec_lanes[j].id)
                        .ok_or_else(|| BackendError::Internal(format!(
                            "MatrixTimesVector vec[{j}] {:?} missing",
                            vec_lanes[j].id)))?;
                    for i in 0..n_lanes {
                        let acc = *scalars.get(&out_lanes[i].id)
                            .ok_or_else(|| BackendError::Internal(format!(
                                "MatrixTimesVector acc lane {i} missing")))?;
                        let (mat_v, mat_n) = load_mat_lane(
                            &mut a, &mut free_pool, &mut owners,
                            &mut used_callee_saved_v, &mut next_synth_id,
                            j, i)?;
                        // tmp = mat_v * vj; acc += tmp.
                        let tmp_synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        let tmp = alloc_vreg(
                            &mut free_pool, &mut owners,
                            &mut used_callee_saved_v, tmp_synth)?;
                        a.emit(asm::fmul_s(tmp, mat_v, vj));
                        a.emit(asm::fadd_s(acc, acc, tmp));
                        // Free both temps.
                        free_pool.push(tmp.0);
                        owners.remove(&tmp.0);
                        free_pool.push(mat_n);
                        owners.remove(&mat_n);
                    }
                }
                vectors.insert(result.id, out_lanes);
            }
            Op::FAdd(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors, &mut packed, &packed_ids,
                &mut free_pool, &mut owners, &mut used_callee_saved_v,&mut next_synth_id,
                coalesce_dest.as_ref(), inst, a_v, b_v, asm::fadd_s, asm::fadd_v_4s)?,
            Op::FSub(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors, &mut packed, &packed_ids,
                &mut free_pool, &mut owners, &mut used_callee_saved_v,&mut next_synth_id,
                coalesce_dest.as_ref(), inst, a_v, b_v, asm::fsub_s, asm::fsub_v_4s)?,
            Op::FMul(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors, &mut packed, &packed_ids,
                &mut free_pool, &mut owners, &mut used_callee_saved_v,&mut next_synth_id,
                coalesce_dest.as_ref(), inst, a_v, b_v, asm::fmul_s, asm::fmul_v_4s)?,
            Op::FDiv(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors, &mut packed, &packed_ids,
                &mut free_pool, &mut owners, &mut used_callee_saved_v,&mut next_synth_id,
                coalesce_dest.as_ref(), inst, a_v, b_v, asm::fdiv_s, asm::fdiv_v_4s)?,
            Op::Store { ptr, value } => {
                // Accept any writable storage class -- the
                // resolve_or_make_pointer storage-class table
                // is the source of truth for what's writable.
                // It returns (param_xreg, byte_offset); the
                // byte_offset honours any prior OpAccessChain
                // (compute SSBO writes flow through this
                // path, as do fragment/vertex Output stores).
                let (ptr_param, base_off) =
                    resolve_or_make_pointer(ptr, &mut pointers, func.stage)?;
                let base_off_u16: u16 = u16::try_from(base_off)
                    .map_err(|_| BackendError::Unsupported(format!(
                        "Op::Store byte offset {base_off} out of u16 range")))?;
                // NEON-packed value: one 128-bit store of
                // the whole Q-register.
                if let Some(&q) = packed.get(&value.id) {
                    a.emit(asm::str_q_offset(q, ptr_param, base_off_u16));
                    continue;
                }
                if let Some(lanes) = vectors.get(&value.id) {
                    if lanes.len() > 4 {
                        return Err(BackendError::Unsupported(format!(
                            "Store of {}-lane vector not supported", lanes.len())));
                    }
                    for (lane_i, lane) in lanes.iter().enumerate() {
                        let sreg = *scalars.get(&lane.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "lane {:?} not in scalars", lane.id)))?;
                        a.emit(asm::fmov_w_from_s(w_tmp, sreg));
                        let offset_bytes = base_off_u16 + (lane_i as u16) * 4;
                        a.emit(asm::str_w_offset(w_tmp, ptr_param, offset_bytes));
                    }
                    continue;
                }
                // Scalar int store (u32/i32 SSBO write).
                if let Some(&w) = ints.get(&value.id) {
                    a.emit(asm::str_w_offset(w, ptr_param, base_off_u16));
                    continue;
                }
                // Scalar f32 store.
                if let Some(&sreg) = scalars.get(&value.id) {
                    a.emit(asm::fmov_w_from_s(w_tmp, sreg));
                    a.emit(asm::str_w_offset(w_tmp, ptr_param, base_off_u16));
                    continue;
                }
                return Err(BackendError::Unsupported(format!(
                    "Op::Store value {:?} not in packed/vectors/ints/scalars",
                    value.id)));
            }
            Op::AccessChain { base, byte_offset } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "AccessChain without result".into()))?;
                let (param, base_off) =
                    resolve_or_make_pointer(base, &mut pointers, func.stage)?;
                let new_off = base_off.saturating_add(*byte_offset as i32);
                pointers.insert(result.id, (param, new_off));
            }
            Op::Load(ptr) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("Load without result".into()))?;
                let (param, off) =
                    resolve_or_make_pointer(ptr, &mut pointers, func.stage)?;
                let pointee = match &ptr.ty {
                    Type::Pointer(_, inner) => (**inner).clone(),
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load ptr {other:?} is not a Pointer type"))),
                };
                match &pointee {
                    Type::F32 => {
                        let v = alloc_vreg(
                            &mut free_pool, &mut owners, &mut used_callee_saved_v,result.id)?;
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
                                &mut free_pool, &mut owners, &mut used_callee_saved_v,synthetic_id)?;
                            emit_load_f32_offset(&mut a, w_tmp, param, lane_off, v)?;
                            scalars.insert(synthetic_id, v);
                            lanes.push(Value {
                                id: synthetic_id, ty: Type::F32,
                            });
                        }
                        vectors.insert(result.id, lanes);
                    }
                    Type::Mat4(_) => {
                        // Deferred: just record (param, off);
                        // MatrixTimesVector loads on demand
                        // to keep the V-reg working set
                        // small.  See `matrices_ptr` decl.
                        matrices_ptr.insert(result.id, (param, off));
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
                            &mut free_pool, &mut owners, &mut used_callee_saved_v,result.id)?;
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
                                &mut free_pool, &mut owners, &mut used_callee_saved_v,synth)?;
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
                        // Coalesced arm: the producing binop
                        // already wrote straight into `dest`,
                        // so the move would be an identity —
                        // skip it (opt #4).
                        if coalesce_into.get(src_id) == Some(dest) {
                            continue;
                        }
                        match dest {
                            PhiDest::Float(dv) => {
                                let src = *scalars.get(src_id).ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi f32 source {:?} not in scalars",
                                        src_id)))?;
                                // Use the 128-bit vector move
                                // (`mov vd.16b, vn.16b`, an
                                // `ORR` alias) rather than the
                                // scalar `fmov s`. Both copy
                                // the f32 (it lives in the low
                                // 32 bits; we only ever read
                                // the S-reg view), but the
                                // `ORR`-form vector move is
                                // eliminated at register rename
                                // on the target cores, whereas
                                // `fmov s` goes down the FP
                                // pipe with real latency — and
                                // these phi-moves sit on the
                                // loop-carried critical path.
                                // (Disasm-confirmed: this is
                                // exactly the one instruction
                                // Cranelift picks differently,
                                // and the heavy4 gap to
                                // Cranelift narrows to it.)
                                a.emit(asm::mov_v_16b(*dv, src));
                            }
                            PhiDest::Int(dw) => {
                                let src = *ints.get(src_id).ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi int source {:?} not in ints",
                                        src_id)))?;
                                a.emit(asm::mov_w(*dw, src));
                            }
                            PhiDest::Packed(dq) => {
                                // NEON-packed vec4 Phi: the
                                // source value's whole vector
                                // lives in one Q-reg too. One
                                // `mov v.16b` carries 128
                                // bits — rename-eliminated on
                                // target cores, like the
                                // scalar Float case.
                                let src = *packed.get(src_id).ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi packed source {:?} not in packed",
                                        src_id)))?;
                                a.emit(asm::mov_v_16b(*dq, src));
                            }
                            PhiDest::Vec(lane_regs) => {
                                // The arm's source is a vector
                                // value — copy each lane's
                                // S-reg into the matching Phi
                                // lane reg. Same `mov vd.16b`
                                // per lane as the scalar Float
                                // case.
                                let src_lanes = vectors.get(src_id)
                                    .cloned().ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi vec source {:?} not in vectors",
                                        src_id)))?;
                                if src_lanes.len() != lane_regs.len() {
                                    return Err(BackendError::Internal(format!(
                                        "Phi vec arm lane mismatch: {} dest \
                                         vs {} src", lane_regs.len(),
                                        src_lanes.len())));
                                }
                                for (dv, lane) in lane_regs.iter()
                                    .zip(src_lanes.iter())
                                {
                                    let src = *scalars.get(&lane.id)
                                        .ok_or_else(||
                                        BackendError::Internal(format!(
                                            "Phi vec lane {:?} not in scalars",
                                            lane.id)))?;
                                    a.emit(asm::mov_v_16b(*dv, src));
                                }
                            }
                        }
                    }
                }
                // Branch-to-next-block elision: if `target`
                // is the very next block in emission order,
                // the `b` would jump to the following
                // instruction — just fall through.
                if next_block != Some(*target) {
                    let patch_off = a.len();
                    a.emit(asm::b(0));
                    branch_relocs.push((patch_off, *target));
                }
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
                // Jump to default — elided if default is
                // the next block in emission order.
                if next_block != Some(*default) {
                    let patch_d = a.len();
                    a.emit(asm::b(0));
                    branch_relocs.push((patch_d, *default));
                }
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
                // Unconditional branch to f_block — elided
                // when f_block is the next block emitted
                // (the conditional `b.<cond>` above already
                // handles the taken edge; control just
                // falls through to f_block).
                if next_block != Some(*f_block) {
                    let patch_f = a.len();
                    a.emit(asm::b(0));
                    branch_relocs.push((patch_f, *f_block));
                }
            }
            // ── Image / sampler ────────────────────────────
            //
            // ImageHandle / CombineSampledImage emit zero
            // native instructions — they're metadata-only,
            // tracking the (set, binding) so the eventual
            // ImageSample call site can compute the v1-ABI
            // descriptor-table offset.
            Op::ImageHandle { set, binding } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageHandle without result".into()))?;
                image_handles.insert(result.id, (*set, *binding));
            }
            Op::CombineSampledImage { image, sampler: _ } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "CombineSampledImage without result".into()))?;
                let h = image_handles.get(&image.id).copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "CombineSampledImage image operand {:?} not an \
                         ImageHandle", image.id)))?;
                image_handles.insert(result.id, h);
            }
            // ImageSampleImplicitLod: emit the v1-ABI call
            // sequence into atrium_tex_sample_2d. The shader
            // ABI keeps `uniforms` in X1; the v1 descriptor
            // table puts the helper fn-ptr at [X1, #0] and
            // the descriptor pair for binding B at
            // [X1, #UNIFORMS_DESC_BASE + B*16 + {0,8}].
            //
            //   sub  sp, sp, #32         ; reserve 16 for
            //                            ; out_rgba + 16 for
            //                            ; saved X4 (the call
            //                            ; is AAPCS64 — X4 is
            //                            ; caller-saved and
            //                            ; X4 is the shader's
            //                            ; out_color pointer
            //                            ; which Store needs
            //                            ; later).
            //   str  x4, [sp, #16]
            //   ldr  x9,  [x1, #0]       ; helper fn ptr
            //   ldr  x10, [x1, #16+B*16] ; tex_desc*
            //   ldr  x11, [x1, #...+8]   ; samp_desc*
            //   <move u, v into V2,V1 then into V0,V1 in
            //    parallel-copy-safe order>
            //   mov  x0, x10
            //   mov  x1, x11             ; clobbers uniforms
            //                            ; (only out_color is
            //                            ; needed after)
            //   mov  x2, sp              ; out_rgba ptr
            //   blr  x9
            //   ldr  w9, [sp, #0..#12]   ; lane bytes
            //     + fmov_s_from_w into each lane V-reg
            //   ldr  x4, [sp, #16]       ; restore out_color
            //   add  sp, sp, #32
            //
            // v1 limitation — V-regs are caller-saved per
            // AAPCS64 (V16..V31; V8..V15 lower 64 bits are
            // callee-saved) and the call clobbers them. The
            // test shader here doesn't have other live V-reg
            // values across the call so this works; a real
            // shader with values live across an ImageSample
            // needs proper save/restore (a later phase).
            Op::ImageSampleImplicitLod { sampled_image, coord } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageSampleImplicitLod without result".into()))?;
                let (_, binding) = image_handles.get(&sampled_image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageSampleImplicitLod sampled_image {:?} not an \
                         ImageHandle", sampled_image.id)))?;
                let coord_lanes = vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageSampleImplicitLod coord {:?} not a vector",
                        coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "ImageSampleImplicitLod 2D coord must have ≥2 lanes, \
                         got {}", coord_lanes.len())));
                }
                let u_v = *scalars.get(&coord_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageSampleImplicitLod coord lane 0 {:?} not in scalars",
                        coord_lanes[0].id)))?;
                let v_v = *scalars.get(&coord_lanes[1].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageSampleImplicitLod coord lane 1 {:?} not in scalars",
                        coord_lanes[1].id)))?;

                // Snapshot every currently-owned V-reg
                // *before* allocating the result lanes —
                // these are the values live across the `blr`
                // (caller-saved V16..V31 + V8..V15 upper-64
                // bits are clobbered per AAPCS64). We spill
                // each as a full 128-bit Q-reg so packed
                // vec4 values (from the NEON arc) survive
                // intact, even though most live values only
                // need 32 bits. Reload after the call.
                //
                // Conservatively saves coord lanes too even
                // though they're consumed by this very op;
                // their last_use would expire at the next
                // inst anyway, so the extra ldr_q is just
                // dead bytes — not incorrect.
                let mut live_vregs: Vec<u8> = owners.keys().copied().collect();
                live_vregs.sort();

                // Allocate four result-lane V-regs *after*
                // the snapshot. They're caller-saved (V16..
                // V31) but not live until we write them with
                // the post-call ldr/fmov, so the clobber by
                // `blr` is harmless and we don't need to
                // spill them.
                let mut lane_regs: Vec<asm::Vreg> = Vec::with_capacity(4);
                let mut lane_vals: Vec<Value> = Vec::with_capacity(4);
                for _ in 0..4 {
                    let synth = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let r = alloc_vreg(&mut free_pool, &mut owners,
                        &mut used_callee_saved_v, synth)?;
                    scalars.insert(synth, r);
                    lane_regs.push(r);
                    lane_vals.push(Value { id: synth, ty: Type::F32 });
                }

                let sp = asm::Xreg(31);
                let x0 = asm::Xreg(0);
                let x1 = asm::Xreg(1);
                let x2 = asm::Xreg(2);
                let x9 = asm::Xreg(9);
                let x10 = asm::Xreg(10);
                let x11 = asm::Xreg(11);
                let v0 = asm::Vreg(0);
                let v1 = asm::Vreg(1);
                let v2 = asm::Vreg(2); // parallel-copy temp
                let lr = asm::Xreg(30);
                let desc_off: u16 = 16 + (binding as u16) * 16;

                // Stack layout (16-byte aligned; each region
                // is 16 bytes):
                //   [sp +  0..16]: out_rgba result slot
                //   [sp + 16..24]: saved X4 (out_color)
                //   [sp + 24..32]: saved X30 (link register)
                //   [sp + 32..32+N*16]: saved V-regs (Q view)
                // SP stays 16-byte aligned (32 + N*16 is a
                // multiple of 16 for any N). LR save is
                // critical: `blr` clobbers X30, and the
                // function's eventual `ret` reads it; without
                // it the function returns to a stale LR and
                // segfaults at the caller boundary. The
                // bespoke backend's existing prologue doesn't
                // save LR because non-image shaders make no
                // function calls — image sample is the first
                // op that does.
                let n_spill = live_vregs.len() as u16;
                let frame_bytes: u16 = 32 + n_spill * 16;
                a.emit(asm::sub_imm_x(sp, sp, frame_bytes));
                a.emit(asm::str_x_offset(x_out, sp, 16));
                a.emit(asm::str_x_offset(lr, sp, 24));
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::str_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }

                // Load descriptor pointers + helper.
                a.emit(asm::ldr_x_offset(x9, x1, 0));
                a.emit(asm::ldr_x_offset(x10, x1, desc_off));
                a.emit(asm::ldr_x_offset(x11, x1, desc_off + 8));

                // u, v → V0, V1, parallel-copy safe via V2
                // (a fresh caller-saved scratch outside our
                // pool). Equivalent to:
                //   tmp = u; v1 = v; v0 = tmp
                a.emit(asm::mov_v_16b(v2, u_v));
                a.emit(asm::mov_v_16b(v1, v_v));
                a.emit(asm::mov_v_16b(v0, v2));

                // Pointer args + call. Note: `mov` from SP
                // is a different ARM64 alias than reg→reg
                // (the plain `MOV` alias is `ORR <Xd>, XZR,
                // <Xm>`, which treats reg 31 as XZR not SP).
                // For `x2 = sp` we use `add x2, sp, #0` —
                // the canonical SP-friendly MOV alias.
                a.emit(asm::mov_x(x0, x10));
                a.emit(asm::mov_x(x1, x11));
                a.emit(asm::add_imm_x(x2, sp, 0));
                a.emit(asm::blr_x(x9));

                // Read result lanes back out of the stack
                // slot (`ldr w9, [sp, #off]; fmov s_lane, w9`).
                for (i, lane) in lane_regs.iter().enumerate() {
                    a.emit(asm::ldr_w_offset(w_tmp, sp, (i as u16) * 4));
                    a.emit(asm::fmov_s_from_w(*lane, w_tmp));
                }

                // Reload spilled V-regs (clobbered by `blr`).
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::ldr_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }

                // Restore X4 (out_color) + X30 (LR); drop stack.
                a.emit(asm::ldr_x_offset(x_out, sp, 16));
                a.emit(asm::ldr_x_offset(lr, sp, 24));
                a.emit(asm::add_imm_x(sp, sp, frame_bytes));

                vectors.insert(result.id, lane_vals);
            }
            // ImageFetch — same v1-ABI call shape as
            // ImageSampleImplicitLod, but: helper fn ptr
            // lives at `uniforms[8]` (the *fetch* slot,
            // not sample); no sampler descriptor; args
            // are (tex_desc*, x:i32, y:i32, lod:i32,
            // out_rgba*) so W1/W2/W3 carry the ivec2 coord
            // + lod and X4 carries out_rgba (AAPCS64
            // assigns the 5th int/ptr arg to X4 — same
            // slot that holds the shader's out_color, so
            // it gets saved/restored across the call as
            // before).
            Op::ImageFetch { image, coord, lod } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageFetch without result".into()))?;
                let (_, binding) = image_handles.get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageFetch image {:?} not an ImageHandle",
                        image.id)))?;
                let coord_lanes = vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageFetch coord {:?} not a vector",
                        coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "ImageFetch 2D coord must have ≥2 lanes, got {}",
                        coord_lanes.len())));
                }
                let x_w = *ints.get(&coord_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageFetch coord lane 0 {:?} not in ints",
                        coord_lanes[0].id)))?;
                let y_w = *ints.get(&coord_lanes[1].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageFetch coord lane 1 {:?} not in ints",
                        coord_lanes[1].id)))?;
                let lod_w_opt: Option<asm::Wreg> = match lod {
                    Some(lv) => Some(*ints.get(&lv.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "ImageFetch lod {:?} not in ints",
                            lv.id)))?),
                    None => None,
                };

                // Snapshot owned V-regs to spill, then
                // allocate 4 result-lane V-regs (same
                // mechanism as ImageSampleImplicitLod).
                let mut live_vregs: Vec<u8> = owners.keys().copied().collect();
                live_vregs.sort();
                let mut lane_regs: Vec<asm::Vreg> = Vec::with_capacity(4);
                let mut lane_vals: Vec<Value> = Vec::with_capacity(4);
                for _ in 0..4 {
                    let synth = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let r = alloc_vreg(&mut free_pool, &mut owners,
                        &mut used_callee_saved_v, synth)?;
                    scalars.insert(synth, r);
                    lane_regs.push(r);
                    lane_vals.push(Value { id: synth, ty: Type::F32 });
                }

                let sp = asm::Xreg(31);
                let x0 = asm::Xreg(0);
                let x1 = asm::Xreg(1);
                let x4 = asm::Xreg(4);  // becomes out_rgba ptr
                let x9 = asm::Xreg(9);
                let x10 = asm::Xreg(10);
                let lr = asm::Xreg(30);
                let w1 = asm::Wreg(1);
                let w2 = asm::Wreg(2);
                let w3 = asm::Wreg(3);
                let desc_off: u16 = 16 + (binding as u16) * 16;

                let n_spill = live_vregs.len() as u16;
                let frame_bytes: u16 = 32 + n_spill * 16;
                a.emit(asm::sub_imm_x(sp, sp, frame_bytes));
                a.emit(asm::str_x_offset(x_out, sp, 16));
                a.emit(asm::str_x_offset(lr, sp, 24));
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::str_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }

                // Load helper fn ptr + tex_desc pointer
                // from the uniforms buffer at the v1
                // offsets. helper slot is +8 (fetch);
                // tex_desc at the descriptor table base
                // for this binding.
                a.emit(asm::ldr_x_offset(x9, x1, 8));
                a.emit(asm::ldr_x_offset(x10, x1, desc_off));

                // Set up args. The ivec2 coord lanes are
                // already in W-regs; copy them into W1/W2.
                // Lod: explicit W-reg if supplied, else 0.
                a.emit(asm::mov_x(x0, x10));
                a.emit(asm::mov_w(w1, x_w));
                a.emit(asm::mov_w(w2, y_w));
                match lod_w_opt {
                    Some(lw) => a.emit(asm::mov_w(w3, lw)),
                    None     => a.emit(asm::movz_w(w3, 0, 0)),
                }
                a.emit(asm::add_imm_x(x4, sp, 0));
                a.emit(asm::blr_x(x9));

                // Read result lanes back out of the stack
                // slot.
                for (i, lane) in lane_regs.iter().enumerate() {
                    a.emit(asm::ldr_w_offset(w_tmp, sp, (i as u16) * 4));
                    a.emit(asm::fmov_s_from_w(*lane, w_tmp));
                }

                // Reload spilled V-regs.
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::ldr_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }

                // Restore X4 (out_color) + X30 (LR); drop stack.
                a.emit(asm::ldr_x_offset(x_out, sp, 16));
                a.emit(asm::ldr_x_offset(lr, sp, 24));
                a.emit(asm::add_imm_x(sp, sp, frame_bytes));

                vectors.insert(result.id, lane_vals);
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "op {other:?} not supported")));
            }
        }
    }
    } // end of per-block loop

    // ── ConstVec literal pool ─────────────────────────────────
    //
    // Lay out the deduped pack-friendly ConstVec literals
    // right here — after the final block's `ret`, so the
    // pool is never executed — and patch each `ldr q`
    // placeholder's imm19 with the resolved PC-relative
    // instruction delta. Pool entries are 16-byte aligned
    // (the LDR-literal natural alignment); pad with NOPs.
    if !pool_slots.is_empty() {
        while a.len() % 16 != 0 {
            a.emit(0xd503_201f); // nop
        }
        let pool_start = a.len();
        for slot in &pool_slots {
            for lane in slot {
                a.emit(*lane);
            }
        }
        for (patch_off, slot, rt) in &pool_patches {
            let slot_byte = pool_start + slot * 16;
            let delta_bytes = slot_byte as i64 - *patch_off as i64;
            if delta_bytes % 4 != 0 {
                return Err(BackendError::Internal(
                    "ldr-q-literal delta not 4-aligned".into()));
            }
            let delta_insts = (delta_bytes / 4) as i32;
            if !(-(1 << 18)..(1 << 18)).contains(&delta_insts) {
                return Err(BackendError::Unsupported(format!(
                    "ldr-q-literal delta {delta_insts} exceeds imm19 range")));
            }
            a.patch(*patch_off, asm::ldr_q_literal(*rt, delta_insts));
        }
    }

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
    // Each pool that dipped into callee-saved registers
    // gets its reserved NOP slots patched with the AAPCS64
    // save/restore sequence. The two halves are
    // independent — either, both, or neither runs; unused
    // slots stay NOPs (zero cost beyond the reserved
    // space). When a half *is* used we save its whole
    // register bank unconditionally (conservative but
    // correct; a used-set-precise prologue is a later
    // optimisation).
    //
    // Prologue order: 5 int pairs (slots 0..5) then 4 FP
    // pairs (slots 5..9), SP dropping 16 per `stp`.
    // Epilogue order is the mirror: 4 FP pairs (slots 0..4)
    // then 5 int pairs (slots 4..9), each `ldp` bumping SP
    // back up — restores unwind in reverse of the saves
    // regardless of which halves are active, since NOP
    // slots don't touch SP.
    let int_pairs = [
        (asm::Xreg(19), asm::Xreg(20)),
        (asm::Xreg(21), asm::Xreg(22)),
        (asm::Xreg(23), asm::Xreg(24)),
        (asm::Xreg(25), asm::Xreg(26)),
        (asm::Xreg(27), asm::Xreg(28)),
    ];
    let fp_pairs = [
        (asm::Vreg(8),  asm::Vreg(9)),
        (asm::Vreg(10), asm::Vreg(11)),
        (asm::Vreg(12), asm::Vreg(13)),
        (asm::Vreg(14), asm::Vreg(15)),
    ];
    if int_pool.used_callee_saved {
        // Prologue int slots: 0..5.
        for (i, (a_reg, b_reg)) in int_pairs.iter().enumerate() {
            a.patch(prologue_off + i * 4,
                    asm::stp_x_pre(*a_reg, *b_reg, asm::Xreg(31), -16));
        }
        // Epilogue int slots: 4..9, reverse pair order.
        for &ep in &epilogue_offs {
            for (i, (a_reg, b_reg)) in int_pairs.iter().rev().enumerate() {
                a.patch(ep + (PROLOGUE_FP_INSTS + i) * 4,
                        asm::ldp_x_post(*a_reg, *b_reg, asm::Xreg(31), 16));
            }
        }
    }
    if used_callee_saved_v {
        // Prologue FP slots: 5..9.
        for (i, (a_reg, b_reg)) in fp_pairs.iter().enumerate() {
            a.patch(prologue_off + (PROLOGUE_INT_INSTS + i) * 4,
                    asm::stp_d_pre(*a_reg, *b_reg, asm::Xreg(31), -16));
        }
        // Epilogue FP slots: 0..4, reverse pair order.
        for &ep in &epilogue_offs {
            for (i, (a_reg, b_reg)) in fp_pairs.iter().rev().enumerate() {
                a.patch(ep + i * 4,
                        asm::ldp_d_post(*a_reg, *b_reg, asm::Xreg(31), 16));
            }
        }
    }

    Ok((a.into_bytes(), pcmap_entries))
}

/// Materialise the (base X-reg, byte offset) pointer
/// repr for a pointer-typed Value. If the Value already
/// has a repr (set by a prior AccessChain), return it.
/// Otherwise it must be a Variable: derive the base
/// register from its storage class per the *stage's*
/// AAPCS64 split — fragment + vertex have different
/// register assignments (see docs/spec/tier2-renderer.md
/// §4.1):
///
///   Fragment:
///     X0 in_varyings, X1 uniforms, X2 push_constants,
///     X4 out_color, X5 out_depth.
///   Vertex:
///     X0 in_attributes, X1 in_attr_strides, X2 uniforms,
///     X3 push_constants, W4 vertex_index, W5 instance_index,
///     X6 out_position, X7 out_varyings, X8 out_clip_distance.
///
/// v1 maps Vertex Output → X6 (out_position) on the
/// assumption the shader only writes gl_Position; mirrors
/// the Cranelift backend's v1 mapping. Richer dispatch
/// (BuiltIn vs Location) lands in a later phase.
fn resolve_or_make_pointer(
    v: &Value,
    pointers: &mut HashMap<ValueId, (asm::Xreg, i32)>,
    stage: ShaderStage,
) -> Result<(asm::Xreg, i32), BackendError> {
    if let Some(p) = pointers.get(&v.id) { return Ok(*p); }
    let storage = match &v.ty {
        Type::Pointer(sc, _) => sc,
        other => return Err(BackendError::Unsupported(format!(
            "pointer Value is not Pointer-typed: {other:?}"))),
    };
    let param = match (stage, storage) {
        (ShaderStage::Fragment, StorageClass::Input)        => asm::Xreg(0),
        (ShaderStage::Fragment, StorageClass::Uniform)      => asm::Xreg(1),
        (ShaderStage::Fragment, StorageClass::PushConstant) => asm::Xreg(2),
        (ShaderStage::Fragment, StorageClass::Output)       => asm::Xreg(4),

        (ShaderStage::Vertex,   StorageClass::Input)        => asm::Xreg(0),
        (ShaderStage::Vertex,   StorageClass::Uniform)      => asm::Xreg(2),
        (ShaderStage::Vertex,   StorageClass::PushConstant) => asm::Xreg(3),
        (ShaderStage::Vertex,   StorageClass::Output)       => asm::Xreg(6),

        // Compute: AAPCS64 9-param signature.  uniforms@X0,
        // push_constants@X1, out_buffer@X2 = the SSBO the
        // shader writes through via StorageBuffer pointers.
        (ShaderStage::Compute,  StorageClass::Uniform)       => asm::Xreg(0),
        (ShaderStage::Compute,  StorageClass::PushConstant)  => asm::Xreg(1),
        (ShaderStage::Compute,  StorageClass::StorageBuffer) => asm::Xreg(2),

        (stage, other) => return Err(BackendError::Unsupported(format!(
            "stage={stage:?} storage class={other:?} not mapped to an \
             ABI register"))),
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

/// Destination register(s) for a Phi node.
///
/// * `Float` — an f32 Phi: one V-reg, moved with the
///   vector `mov vd.16b` (rename-eliminated; see the
///   heavy4 fix).
/// * `Int` — an i32/u32 Phi: one W-reg, moved with `mov w`.
/// * `Vec` — a vecN-f32 Phi: N V-regs, one per lane,
///   moved per-lane (the backend stores a vector as a
///   list of per-lane scalar values, so a vec Phi is just
///   N scalar Phis travelling together). Not `Copy`
///   because of the `Vec`; `Clone` + the handful of
///   `.clone()`s at the build sites is the cost.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PhiDest {
    Float(asm::Vreg),
    Int(asm::Wreg),
    Vec(Vec<asm::Vreg>),
    /// NEON-packed vec4 Phi: the whole vector in a single
    /// Q-register. Phi-move is one `mov v.16b`; a packed
    /// vec×vec FP-binop arm coalesces by writing the .4s
    /// result straight into this Q-reg.
    Packed(asm::Vreg),
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
///
/// `coalesce` — when `Some(d)` this op's result is a Phi
/// arm being coalesced into the Phi register `d`: compute
/// straight into `d` instead of allocating a fresh W-reg,
/// so the phi-move drops to an identity (see opt #4).
#[allow(clippy::too_many_arguments)]
fn emit_int_binop(
    a: &mut asm::Asm,
    ints: &mut HashMap<ValueId, asm::Wreg>,
    int_pool: &mut IntPool,
    coalesce: Option<asm::Wreg>,
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
    // ARM reads both source operands before writing the
    // destination, so coalescing into a register that is
    // also a source (`add D, D, r`) is correct.
    let d = match coalesce {
        Some(d) => d,
        None => alloc_int_w(int_pool, result.id)?,
    };
    a.emit(make_inst(d, l, r));
    ints.insert(result.id, d);
    Ok(())
}

/// Emit an integer add or sub, folding a small constant
/// operand into the `imm12` field when the immediate-fold
/// pre-pass marked it foldable (opt #3) — `add w,w,#1`
/// instead of materialising the constant into a W-reg.
/// `coalesce` behaves as in [`emit_int_binop`].
#[allow(clippy::too_many_arguments)]
fn emit_int_addsub(
    a: &mut asm::Asm,
    ints: &mut HashMap<ValueId, asm::Wreg>,
    int_pool: &mut IntPool,
    coalesce: Option<asm::Wreg>,
    fold_const: &std::collections::HashSet<ValueId>,
    const_int_value: &HashMap<ValueId, i64>,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    is_sub: bool,
) -> Result<(), BackendError> {
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("int add/sub without result".into()))?;
    // Foldable constant operand? `sub` only folds its RHS
    // (`sub w,w,#imm`); `add` is commutative so either.
    let folded: Option<(asm::Wreg, u16)> = if fold_const.contains(&rhs.id) {
        let rn = *ints.get(&lhs.id).ok_or_else(||
            BackendError::Internal(format!(
                "int add/sub reg operand {:?} not in ints", lhs.id)))?;
        Some((rn, const_int_value[&rhs.id] as u16))
    } else if !is_sub && fold_const.contains(&lhs.id) {
        let rn = *ints.get(&rhs.id).ok_or_else(||
            BackendError::Internal(format!(
                "int add reg operand {:?} not in ints", rhs.id)))?;
        Some((rn, const_int_value[&lhs.id] as u16))
    } else {
        None
    };
    let d = match coalesce {
        Some(d) => d,
        None => alloc_int_w(int_pool, result.id)?,
    };
    match folded {
        Some((rn, imm)) => a.emit(if is_sub {
            asm::sub_imm_w(d, rn, imm)
        } else {
            asm::add_imm_w(d, rn, imm)
        }),
        None => {
            let l = *ints.get(&lhs.id).ok_or_else(||
                BackendError::Internal(format!(
                    "int add/sub lhs {:?} not in ints", lhs.id)))?;
            let r = *ints.get(&rhs.id).ok_or_else(||
                BackendError::Internal(format!(
                    "int add/sub rhs {:?} not in ints", rhs.id)))?;
            a.emit(if is_sub { asm::sub_w(d, l, r) }
                   else { asm::add_w(d, l, r) });
        }
    }
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
    used_callee_saved_v: &mut bool,
    owner: ValueId,
) -> Result<asm::Vreg, BackendError> {
    let n = free_pool.pop().ok_or_else(|| BackendError::Unsupported(
        "linear-scan RA ran out of V-regs (V16..V31 + V8..V15); \
         true spilling lands in a later widening".into()))?;
    // V8..V15 are callee-saved — first use forces the FP
    // prologue/epilogue.
    if n < 16 { *used_callee_saved_v = true; }
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
/// Does instruction `op` read `id` as one of its operands?
///
/// Used by Phi-move coalescing to scan the flat span
/// between a Phi arm's producer and the back-edge for any
/// read of the Phi value. Conservative: any op shape not
/// explicitly enumerated returns `true` ("assume it reads
/// it" → block the coalesce), so an unhandled op can never
/// make coalescing *un*safe — only miss an opportunity.
fn op_reads(op: &Op, id: ValueId) -> bool {
    use Op::*;
    match op {
        ConstInt { .. } | ConstFloat { .. } | ConstNull
        | Branch(_) | Return | Discard => false,
        ConstVec(els) => els.iter().any(|v| v.id == id),
        IAdd(l, r) | ISub(l, r) | IMul(l, r) | UDiv(l, r) | SDiv(l, r)
        | UMod(l, r) | SMod(l, r) | FAdd(l, r) | FSub(l, r)
        | FMul(l, r) | FDiv(l, r) | FRem(l, r)
        | BitAnd(l, r) | BitOr(l, r) | BitXor(l, r)
        | Shl(l, r) | LShr(l, r) | AShr(l, r)
        | IEq(l, r) | INe(l, r)
        | ULt(l, r) | ULe(l, r) | UGt(l, r) | UGe(l, r)
        | SLt(l, r) | SLe(l, r) | SGt(l, r) | SGe(l, r)
        | FOrdEq(l, r) | FOrdNe(l, r) | FOrdLt(l, r)
        | FOrdLe(l, r) | FOrdGt(l, r) | FOrdGe(l, r)
        | FUnordEq(l, r) | FUnordNe(l, r) | FUnordLt(l, r)
        | FUnordLe(l, r) | FUnordGt(l, r) | FUnordGe(l, r)
        | Dot(l, r) =>
            l.id == id || r.id == id,
        INeg(s) | FNeg(s) | BitNot(s)
        | ConvertSToF(s) | ConvertFToS(s)
        | ConvertUToF(s) | ConvertFToU(s)
        | SConvert(s, _) | UConvert(s, _) | FConvert(s, _)
        | Bitcast(s, _) | Load(s)
        | DPdx(s) | DPdy(s) | Fwidth(s) | ReturnValue(s) =>
            s.id == id,
        VectorShuffle { src1, src2, .. } =>
            src1.id == id || src2.id == id,
        VectorExtract { vector, .. } => vector.id == id,
        VectorInsert { vector, scalar, .. } =>
            vector.id == id || scalar.id == id,
        MatrixTimesVector { matrix, vector } =>
            matrix.id == id || vector.id == id,
        AccessChain { base, .. } => base.id == id,
        Store { ptr, value } => ptr.id == id || value.id == id,
        Select { cond, t_val, f_val } =>
            cond.id == id || t_val.id == id || f_val.id == id,
        BranchCond { cond, .. } => cond.id == id,
        Switch { selector, .. } => selector.id == id,
        Phi(arms) => arms.iter().any(|a| a.value.id == id),
        // Atomics, image ops, and anything else: conservative.
        _ => true,
    }
}

/// Returns `(last_use, use_counts)`:
/// * `last_use` — loop-extension applied (the value the
///   linear-scan expiry consults).
/// * `use_counts` — raw operand-occurrence tally; NOT
///   perturbed by the loop extension, so `use_counts[v]
///   == 1` is a reliable "single use" signal for the
///   compare→branch fusion and Phi-coalescing checks.
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
            Op::MatrixTimesVector { matrix, vector } => {
                // Mat4 lane scalars are backend-synth IDs allocated
                // inside emit_function and never appear in this
                // pre-pass — they stay alive forever via the
                // unwrap_or(MAX) fallback in the expiry test.  But
                // the vector lane Values DO exist in the IR (e.g. a
                // ConstFloat fed through a ConstVec), and without
                // this propagation the linear-scan reclaims their
                // V-regs right after ConstVec — the Mat4 Load then
                // re-uses those regs and clobbers the lane data the
                // MatrixTimesVector still needs to read.
                mark(matrix.id); mark(vector.id);
                if let Some(lanes) = vec_lanes.get(&vector.id).cloned() {
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
            // Image sample / fetch: the coord operand (a
            // vec) is read at this op's index; propagate to
            // its per-lane scalars so the lane V-regs don't
            // expire early. Without this, a ConstFloat that
            // only feeds a `texture()` call's coord
            // (e.g. `c_quarter` shared by both `uv`'s lanes)
            // would have its `last_use` set only to the
            // *ConstVec*'s index — leaving the regalloc free
            // to reuse its V-reg from the *next* inst on,
            // even though `ImageSample` still needs to read
            // it. (The bug `texture_sample_tinted` was
            // staged to catch.)
            Op::ImageSampleImplicitLod { sampled_image, coord } => {
                mark(sampled_image.id);
                mark(coord.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageSampleExplicitLod { sampled_image, coord, lod } => {
                mark(sampled_image.id);
                mark(coord.id);
                mark(lod.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageFetch { image, coord, lod } => {
                mark(image.id);
                mark(coord.id);
                if let Some(l) = lod { mark(l.id); }
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::CombineSampledImage { image, sampler } => {
                mark(image.id);
                mark(sampler.id);
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
            // A vector phi arm (e.g. a `ConstVec` initial
            // value on the entry edge) doesn't own a reg
            // itself — its per-lane scalar values do. The
            // vec-Phi phi-move reads each lane reg at the
            // predecessor's terminator, so each lane's live
            // range must reach there too; without this the
            // linear-scan recycles a lane's constant reg
            // before the entry→header phi-move copies it.
            if let Some(lanes) = vec_lanes.get(&value_id).cloned() {
                for lid in lanes {
                    *use_counts.entry(lid).or_insert(0) += 1;
                    last_use.entry(lid)
                        .and_modify(|e| *e = (*e).max(term))
                        .or_insert(term);
                }
            }
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
/// Result is stored in `scalars` (scalar shape), `packed`
/// (NEON-packed vec4 — one `.4s` op), or `vectors` (per-
/// lane vec shape).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn emit_fp_binop_poly(
    a: &mut asm::Asm,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    vectors: &mut HashMap<ValueId, Vec<Value>>,
    packed: &mut HashMap<ValueId, asm::Vreg>,
    packed_ids: &std::collections::HashSet<ValueId>,
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    used_callee_saved_v: &mut bool,
    next_synth_id: &mut u32,
    coalesce: Option<&PhiDest>,
    inst: &atrium_spv_ir::Inst,
    lhs: &Value,
    rhs: &Value,
    make_inst: fn(asm::Vreg, asm::Vreg, asm::Vreg) -> u32,
    make_inst_v4s: fn(asm::Vreg, asm::Vreg, asm::Vreg) -> u32,
) -> Result<(), BackendError> {
    let mut fresh_synth = || {
        let id = ValueId(*next_synth_id);
        *next_synth_id += 1;
        id
    };
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("fp binop without result".into()))?;

    // NEON-packed vec4 path: the classifier proved this
    // result and both operands live in single Q-registers,
    // so the whole binop is one `.4s` instruction. If this
    // result is a coalesced packed-Phi arm, write straight
    // into the Phi's Q-reg (the vec analogue of opt #4 +
    // the vec-Phi phase-2 coalescing).
    if packed_ids.contains(&result.id) {
        let l = *packed.get(&lhs.id).ok_or_else(||
            BackendError::Internal(format!(
                "packed fp binop lhs {:?} not packed", lhs.id)))?;
        let r = *packed.get(&rhs.id).ok_or_else(||
            BackendError::Internal(format!(
                "packed fp binop rhs {:?} not packed", rhs.id)))?;
        let d = match coalesce {
            Some(PhiDest::Packed(q)) => *q,
            _ => alloc_vreg(free_pool, owners, used_callee_saved_v,
                result.id)?,
        };
        a.emit(make_inst_v4s(d, l, r));
        packed.insert(result.id, d);
        return Ok(());
    }

    // Scalar coalesce target (Float Phi) and per-lane
    // coalesce targets (Vec Phi). A Vec Phi's lane regs are
    // never-expired and distinct per lane, so writing lane
    // `li`'s result straight into `coalesce_lanes[li]` is
    // safe: `make_inst` reads both operands before writing
    // the dest, and the only register the dest can alias is
    // that same lane's own operand.
    let coalesce_scalar: Option<asm::Vreg> = match coalesce {
        Some(PhiDest::Float(v)) => Some(*v),
        _ => None,
    };
    let coalesce_lanes: Option<&[asm::Vreg]> = match coalesce {
        Some(PhiDest::Vec(lr)) => Some(lr.as_slice()),
        _ => None,
    };

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
            // Phi-move coalescing: write straight into the
            // Phi register when this scalar result is a
            // single-use Phi arm (see opt #4). ARM reads
            // both operands before writing the dest, so
            // coalescing into a source register is correct.
            let d = match coalesce_scalar {
                Some(d) => d,
                None => alloc_vreg(free_pool, owners, used_callee_saved_v,result.id)?,
            };
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
                let d = match coalesce_lanes {
                    Some(lr) => lr[li],
                    None => alloc_vreg(free_pool, owners, used_callee_saved_v,synth)?,
                };
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
                let d = match coalesce_lanes {
                    Some(lr) => lr[li],
                    None => alloc_vreg(free_pool, owners, used_callee_saved_v,synth)?,
                };
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
                let d = match coalesce_lanes {
                    Some(lr) => lr[li],
                    None => alloc_vreg(free_pool, owners, used_callee_saved_v,synth)?,
                };
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
