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

/// Byte offset of the `atrium_barrier` function-pointer slot
/// within the compute image-table.  Sourced from
/// [`atrium_spv_runtime::IMG_TABLE_BARRIER_OFFSET`]; narrowed
/// to `u16` because the bespoke backend's ARM64 immediate
/// encodings take `u16` displacements.  See Arc 150.
const IMG_TABLE_BARRIER_OFFSET_U16: u16 =
    atrium_spv_runtime::IMG_TABLE_BARRIER_OFFSET as u16;

/// Byte offset of the first image-descriptor slot within the
/// compute image-table.  Sourced from
/// [`atrium_spv_runtime::IMG_TABLE_DESC_BASE`]; narrowed to
/// `u16` for the same reason as the barrier offset above.
const IMG_TABLE_DESC_BASE_U16: u16 =
    atrium_spv_runtime::IMG_TABLE_DESC_BASE as u16;
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
        let (body, pc_entries) = emit_function_auto(func)?;
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
    // `target` selects ARM64 either way for the bodies, but the
    // span thunk's incoming stack-arg offsets are ABI-specific
    // (Apple packs; AAPCS64/FreeBSD uses 8-byte slots).
    let mut code: Vec<u8> = Vec::new();
    let mut pcmap = atrium_spv_pcmap::Builder::new();
    let mut entries = atrium_spv_blob::EntryOffsets::default();

    for func in &module.functions {
        let (body, pc_entries) = emit_function_auto(func)?;
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
        let fs_main_len = body.len();
        code.extend_from_slice(&body);
        // P2.2b: append the batched-fragment span thunk right after
        // the scalar fs_main body so its `bl atrium_fs_main` is a
        // fixed negative offset (no relocation).  Emitted only for
        // the supported FS subset; `None` otherwise.
        if func.stage == ShaderStage::Fragment {
            if let Some(thunk) = emit_fragment_span_thunk(func, fs_main_len, target) {
                entries.fs_span = Some(off + fs_main_len as u32);
                code.extend_from_slice(&thunk);
            }
        }
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
/// Fast path first; on V-reg exhaustion retry with scalar
/// spilling. SSA immutability makes slots write-once (stored at
/// definition), so EVICTION emits no code — only reloads (`ldr d`)
/// and def-stores (`str d`) cost instructions, and only shaders
/// that actually exhaust the file pay them.
fn emit_function_auto(
    func: &Function,
) -> Result<(Vec<u8>, Vec<(u32, u32)>), BackendError> {
    // Testing lever: force spill mode so SMALL kernels (the
    // whole differential suite) exercise the spill machinery.
    if std::env::var("ATRIUM_SPV_FORCE_SPILL").is_ok() {
        return emit_function(func, true);
    }
    match emit_function(func, false) {
        Err(BackendError::Unsupported(m))
            if m.contains("ran out of V-regs")
                || m.contains("ran out of W-regs")
                || m.contains("Bool W-regs")
                || m.contains("allocating Phi dest")
                || m.contains("allocating Bool Phi dest") =>
            emit_function(func, true),
        r => r,
    }
}

fn emit_function(
    func: &Function,
    spill_mode: bool,
) -> Result<(Vec<u8>, Vec<(u32, u32)>), BackendError> {
    // Split conditional edges into Phi-bearing blocks first; the
    // rest of codegen then only ever places phi-moves on
    // unconditional edges.
    let split;
    let (func, stub_placements): (&Function, Vec<(BlockId, BlockId)>) =
        match split_critical_edges(func) {
            Some((f, p)) => {
                split = f;
                (&split, p)
            }
            None => (func, Vec::new()),
        };
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
    // Int-element vector Loads (e.g. a push-constant uint4) are
    // DEFERRED like Mat4: record (base reg, byte offset) and let
    // each VectorExtract `ldr w` its lane directly — zero W-regs
    // held between load and use (materialised lanes pinned the
    // 15-reg W file; measured on Orbis's terrain kernel).
    let mut int_vec_ptr: HashMap<ValueId, (asm::Xreg, i32)> = HashMap::new();
    // Deferred dynamic pointers (spill mode): PtrOffsetDynamic
    // emits NOTHING; consumers materialise base+index<<k into the
    // X9 scratch at the access. Addresses are one shift+add —
    // cheaper to recompute than to keep resident (they pinned
    // most of the W file on big kernels, and as X-views in the
    // `pointers` map they were invisible to int eviction).
    let mut deferred_ptr: HashMap<ValueId, (asm::Xreg, i32, ValueId, u8)> =
        HashMap::new();
    // Compute uvec3 builtins, lowered once per KIND: spirv-opt
    // emits a LoadBuiltin per use site, and each lowering kept 3
    // immortal W-lanes (synth ids have no last_use) — several
    // uses exhausted the 15-reg W file. Cache the lane Values;
    // repeat loads alias the same registers. (Also a fast-path
    // win: no recompute.)
    let mut builtin_lane_cache: HashMap<u8, Vec<Value>> = HashMap::new();
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
    // in a W-reg as i32 0/1 per constraint B4.  Pool is
    // W10..W12 (3 slots, separate from the int pool which
    // starts at W13).  Linear-scan-style ownership with
    // expiration on last_use < i; alloc pops from
    // bool_free, expire pushes back.
    let mut bools: HashMap<ValueId, asm::Wreg> = HashMap::new();
    // Bool Phi dests that overflowed the W pool into S-regs
    // (V-class). Bools are written at phi-move edges
    // (`fmov s, w` / `fmov s, s`) and read at only a handful
    // of consumer sites, which materialise them into the W9
    // scratch via `fmov w, s` — no memory traffic, and the
    // contended W pool stays for ints (32 V-regs vs 15 W).
    let mut bool_in_v: HashMap<ValueId, asm::Vreg> = HashMap::new();
    // Spill-resident bool Phi dests: ValueId -> slot index.
    let mut bool_in_slot: HashMap<ValueId, u32> = HashMap::new();
    let mut bool_owners: HashMap<u8, ValueId> = HashMap::new();
    let mut bool_free: Vec<u8> = vec![12, 11, 10]; // pop hands out W10 first
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
    // Edge stubs sit right AFTER their from-block in flat order —
    // sorted-by-id would dump them at the end and extend every
    // phi-move source's flat live range to the whole function.
    for (stub, from) in &stub_placements {
        block_order.retain(|b| b != stub);
        if let Some(pos) = block_order.iter().position(|b| b == from) {
            block_order.insert(pos + 1, *stub);
        }
    }

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
    let (mut last_use, use_counts) = compute_last_use_flat(
        &flat_insts, &block_term_idx, &block_flat_start);

    // ── Scalar spill state (active only in spill_mode) ────────
    // spilled    = values currently slot-only (not in a reg).
    // spill_slot = ValueId → slot index (8 bytes/slot, write-
    //              once at def: SSA values never change).
    let mut spilled: std::collections::HashSet<ValueId> =
        std::collections::HashSet::new();
    let mut spilled_w: std::collections::HashSet<ValueId> =
        std::collections::HashSet::new();
    let mut spill_slot: HashMap<ValueId, u32> = HashMap::new();
    let mut spill_next: u32 = 0;
    // Dead values' slots recycle through a free list — the live-
    // slot peak is what bounds the frame, not total defs.
    let mut spill_free: Vec<u32> = Vec::new();

    const SPILL_SLOT_CAP: u32 = 1000; // 8000 B / two sub-imm12 insts

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
            | Op::FMin(l, r) | Op::FMax(l, r)
                if is_vec4(&l.ty) && is_vec4(&r.ty) =>
                Some((l.id, r.id)),
            _ => None,
        }
    };
    // Unary FP ops over a vec4: result and operand share
    // the same NEON-packed fate.  Returns the operand id
    // when it matches.
    let vec_fp_unop = |op: &Op| -> Option<ValueId> {
        match op {
            Op::FAbs(x) | Op::FSqrt(x) | Op::FNeg(x)
            | Op::FFloor(x) | Op::FCeil(x) | Op::FTrunc(x)
                if is_vec4(&x.ty) =>
                Some(x.id),
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
            || vec_fp_unop(&inst.op).is_some()
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
            if let Some(operand) = vec_fp_unop(&inst.op) {
                let res = inst.result.as_ref().map(|x| x.id);
                let any = disqualified.contains(&operand)
                    || res.is_some_and(|x| disqualified.contains(&x));
                if any {
                    changed |= disqualified.insert(operand);
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
    //   * V0..V7   — also caller-saved per AAPCS64, normally
    //     used for FP/SIMD argument passing.  The compute
    //     calling convention passes no V-reg args (uniforms
    //     and pointers ride X0..X2; wg/lid ride W3..W7), so
    //     V0..V7 are free for our use with no prologue cost.
    // `pop` takes from the end, so the vec is ordered
    // [V15..V8, V7..V0, V31..V16] → V16..V31 first,
    // V0..V7 next, V8..V15 last (callee-saved overflow tier).
    // At each inst i, before defining any new value, expire
    // scalars whose last_use < i and return their V-regs to
    // the pool. Then allocate from the pool for new defs.
    let mut free_pool: Vec<u8> = (8..16).rev().collect();
    free_pool.extend((0..8).rev());
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

    // ── Interval-based Phi-dest allocation ────────────────────
    //
    // A Phi dest is loop-carried: every predecessor edge's
    // phi-move WRITES it (the back-edge write is the latest
    // event in flat order) and the loop body reads it. Its
    // live interval is therefore
    //   [min over arms of from-block's terminator,
    //    max(arm terminators, last_use of the result)].
    // Phis of DISJOINT intervals (sequential loops) share
    // registers — without this, every loop in the function
    // permanently pins its own dest regs and big kernels
    // exhaust the 15-reg W pool (seen with Orbis's terrain
    // path tracer: 3+ sequential march loops).
    //
    // Within its interval a dest is pinned (never expired);
    // after it, ownership is registered with the regular
    // linear scan (owner = last phi on the reg, last_use
    // extended to the interval-union end), so the reg also
    // returns to the general pool for non-phi values.
    let mut phi_recs: Vec<(usize, usize, BlockId, &atrium_spv_ir::Inst)> =
        Vec::new();
    for bid in &block_order {
        let block = func.blocks.get(bid).unwrap();
        for inst in &block.insts {
            let arms = match &inst.op {
                Op::Phi(arms) => arms,
                _ => break, // Phis must lead the block.
            };
            let result = inst.result.as_ref().ok_or_else(||
                BackendError::Internal("Phi without result".into()))?;
            let mut start = usize::MAX;
            let mut end = 0usize;
            for arm in arms {
                let t = block_term_idx.get(&arm.from).copied().unwrap_or(0);
                start = start.min(t);
                end = end.max(t);
            }
            end = end.max(last_use.get(&result.id).copied().unwrap_or(0));
            phi_recs.push((start, end, *bid, inst));
        }
    }
    phi_recs.sort_by_key(|r| r.0);

    // (end, regs) of in-interval phis; class 0 = V, 1 = W.
    let mut phi_active: Vec<(usize, Vec<(u8, u8)>)> = Vec::new();
    let mut phi_avail_v: Vec<u8> = Vec::new();
    let mut phi_avail_w: Vec<u8> = Vec::new();
    // reg -> (last owner value, union end) for the post-pass
    // ownership handoff to the regular linear scan.
    let mut phi_meta_v: HashMap<u8, (ValueId, usize)> = HashMap::new();
    let mut phi_meta_w: HashMap<u8, (ValueId, usize)> = HashMap::new();

    // Spill-mode phi caps: leave the working set evictable regs.
    const PHI_V_CAP: usize = 9999;
    let mut phi_v_resident: usize = 0;
    let mut phi_slot_floats: Vec<ValueId> = Vec::new();
    for (start, end, bid, inst) in &phi_recs {
        let (start, end) = (*start, *end);
        let arms = match &inst.op {
            Op::Phi(arms) => arms,
            _ => unreachable!("phi_recs holds only Op::Phi"),
        };
        let result = inst.result.as_ref().expect("checked in collection");
        let bid = bid; // &BlockId, body below uses *bid

        // Release actives whose interval ended before this
        // phi starts — their regs become preferred slots.
        let share_disabled =
            std::env::var("ATRIUM_SPV_NO_PHI_SHARE").is_ok();
        phi_active.retain(|(aend, regs)| {
            if *aend < start && !share_disabled {
                for (class, n) in regs {
                    if *class == 0 { phi_avail_v.push(*n); }
                    else { phi_avail_w.push(*n); }
                }
                false
            } else {
                true
            }
        });
        let mut regs_this: Vec<(u8, u8)> = Vec::new();

        let dest = match &result.ty {
                Type::F32 => {
                    // Cap reg-resident phi dests in spill mode so
                    // the working set keeps evictable registers;
                    // overflow goes to spill-resident slots.
                    let reg = if !spill_mode
                        || phi_v_resident < PHI_V_CAP
                    {
                        phi_avail_v.pop().or_else(|| free_pool.pop())
                    } else { None };
                    if let Some(n) = reg {
                        if n < 16 { used_callee_saved_v = true; }
                        phi_v_resident += 1;
                        let m = phi_meta_v.entry(n)
                            .or_insert((result.id, end));
                        *m = (result.id, m.1.max(end));
                        regs_this.push((0, n));
                        let v = asm::Vreg(n);
                        scalars.insert(result.id, v);
                        PhiDest::Float(v)
                    } else if spill_mode {
                        let slot = spill_next;
                        spill_next += 1;
                        if slot >= SPILL_SLOT_CAP {
                            return Err(BackendError::Unsupported(
                                "spill area exhausted (phi)".into()));
                        }
                        spill_slot.insert(result.id, slot);
                        spilled.insert(result.id);
                        phi_slot_floats.push(result.id);
                        PhiDest::FloatSpill(slot)
                    } else {
                        return Err(BackendError::Unsupported(
                            "out of V-regs allocating Phi dest".into()));
                    }
                }
                Type::I32 | Type::U32 => {
                    let n = phi_avail_w.pop()
                        .or_else(|| int_pool.free.pop())
                        .ok_or_else(|| BackendError::Unsupported(
                            "out of int W-regs allocating Phi dest".into()))?;
                    if n >= 19 { int_pool.used_callee_saved = true; }
                    let m = phi_meta_w.entry(n).or_insert((result.id, end));
                    *m = (result.id, m.1.max(end));
                    regs_this.push((1, n));
                    let w = asm::Wreg(n);
                    ints.insert(result.id, w);
                    PhiDest::Int(w)
                }
                // Bool Phi: same W-reg discipline as Int (bools
                // are 0/1-valued i32s per constraint B4), but the
                // dest lives in the `bools` map — that's where
                // Select / BranchCond resolve their conditions.
                Type::Bool => {
                    // In spill mode, prefer the V side outright:
                    // bool phi dests are PINNED (regs baked into
                    // the move tables, unevictable), and the
                    // 15-reg W file can't afford ~20 pinned bools
                    // once ints also need eviction headroom. The
                    // 32-reg V file + scalar spilling absorbs
                    // them; fast path keeps W-first (no spilling,
                    // W pressure is the cheaper move there).
                    let w_first = if spill_mode {
                        None
                    } else {
                        phi_avail_w.pop()
                            .or_else(|| int_pool.free.pop())
                    };
                    if let Some(n) = w_first {
                        if n >= 19 { int_pool.used_callee_saved = true; }
                        let m = phi_meta_w.entry(n).or_insert((result.id, end));
                        *m = (result.id, m.1.max(end));
                        regs_this.push((1, n));
                        let w = asm::Wreg(n);
                        bools.insert(result.id, w);
                        PhiDest::Bool(w)
                    } else {
                        // W exhausted: V-class next (fmov w<->s,
                        // no memory), then spill-resident slots
                        // once the V cap is hit too.
                        let reg = if !spill_mode
                            || phi_v_resident < PHI_V_CAP
                        {
                            phi_avail_v.pop().or_else(|| free_pool.pop())
                        } else { None };
                        if let Some(n) = reg {
                            if n < 16 { used_callee_saved_v = true; }
                            phi_v_resident += 1;
                            let m = phi_meta_v.entry(n)
                                .or_insert((result.id, end));
                            *m = (result.id, m.1.max(end));
                            regs_this.push((0, n));
                            let v = asm::Vreg(n);
                            bool_in_v.insert(result.id, v);
                            PhiDest::BoolV(v)
                        } else if spill_mode {
                            let slot = spill_next;
                            spill_next += 1;
                            if slot >= SPILL_SLOT_CAP {
                                return Err(BackendError::Unsupported(
                                    "spill area exhausted (phi)".into()));
                            }
                            spill_slot.insert(result.id, slot);
                            bool_in_slot.insert(result.id, slot);
                            PhiDest::BoolSpill(slot)
                        } else {
                            return Err(BackendError::Unsupported(
                                "out of W- AND V-regs allocating \
                                 Bool Phi dest".into()));
                        }
                    }
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
                        let n = phi_avail_v.pop()
                            .or_else(|| free_pool.pop())
                            .ok_or_else(|| BackendError::Unsupported(
                                "out of V-regs allocating vec Phi dest".into()))?;
                        if n < 16 { used_callee_saved_v = true; }
                        let v = asm::Vreg(n);
                        let synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        let m = phi_meta_v.entry(n).or_insert((synth, end));
                        *m = (synth, m.1.max(end));
                        last_use.insert(synth, end);
                        regs_this.push((0, n));
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
                    // Bool producers are compares/logic, not the
                    // coalesce-aware binop emitters: never coalesce.
                    PhiDest::Bool(_) | PhiDest::BoolV(_)
                    | PhiDest::FloatSpill(_) | PhiDest::BoolSpill(_) => false,
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
            phi_active.push((end, regs_this));
    }

    // Hand expired-interval phi regs to the regular linear
    // scan: owner = the last phi that used the reg, with its
    // last_use extended to the union end, so `expire` returns
    // the reg to the free pool exactly when the last interval
    // on it closes. (Packed Q-reg phi dests stay pinned for
    // the function — rare, and the packed map has no expiry.)
    for (n, (owner, end)) in &phi_meta_v {
        owners.insert(*n, *owner);
        let e = last_use.entry(*owner).or_insert(*end);
        *e = (*e).max(*end);
    }
    for (n, (owner, end)) in &phi_meta_w {
        int_pool.owners.insert(*n, *owner);
        let e = last_use.entry(*owner).or_insert(*end);
        *e = (*e).max(*end);
    }
    // Phi dest regs are baked into phi_moves — never evict a
    // value owned by a phi (covers float/vec-lane/boolV dests).
    let phi_pinned: std::collections::HashSet<ValueId> =
        phi_meta_v.values().chain(phi_meta_w.values())
            .map(|(o, _)| *o).collect();

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
    /// Two slots for the spill-area `sub sp` / `add sp` pair
    /// (each imm12 <= 4095; two cover 8190 B = 1023 slots; NOPs
    /// when unused).
    const PROLOGUE_SPILL_INSTS: usize = 2;
    const PROLOGUE_INSTS: usize =
        PROLOGUE_INT_INSTS + PROLOGUE_FP_INSTS + PROLOGUE_SPILL_INSTS;
    let prologue_off = a.len();
    for _ in 0..PROLOGUE_INSTS { a.emit(asm::nop()); }
    // Epilogue placeholder offsets — one per `ret`. Each is
    // a PROLOGUE_INSTS-NOP region emitted just before its
    // ret. Mirror-image layout: 4 FP slots then 5 int slots
    // (restores run in reverse of the prologue's saves).
    let mut epilogue_offs: Vec<usize> = Vec::new();
    // Byte offsets of `ldr_w wreg, [sp, #0]` instructions
    // that load LocalInvocationId.z (the 9th AAPCS64 arg).
    // Patched at the end of body emission with the actual
    // frame-byte shift the prologue introduced -- the SP-
    // relative offset of the lid.z slot is +frame_bytes
    // *after* the prologue's `stp ..., [sp, #-16]!`
    // instructions push SP down.  Each entry: (byte offset
    // of the ldr_w_offset inst, dest W-reg).
    let mut lid_z_load_patches: Vec<(usize, asm::Wreg)> = Vec::new();

    // Multi-binding SSBO base registers that an image-helper
    // `blr` would clobber (X12..X17 are caller-saved): each
    // entry is (base reg, descriptor-table binding).  After
    // an ImageRead/ImageWrite call the lowering re-loads
    // these from X2 (which is itself saved/restored across
    // the call), so a shader can both read SSBOs and touch
    // storage images.
    let mut ssbo_base_reloads: Vec<(asm::Xreg, u32)> = Vec::new();

    // Multi-binding SSBO prologue.  When a compute shader
    // declares >1 StorageBuffer, X2 is no longer a direct
    // SSBO pointer -- it points to a descriptor table where
    // entry B holds the pointer for binding B (8 bytes each,
    // single descriptor set assumed).  Pre-load each
    // binding's pointer into X16/X17 (AAPCS64 IP0/IP1 --
    // caller-saved and unused by the regalloc int pool
    // X19..X28) and pre-populate `pointers` so subsequent
    // resolve_or_make_pointer calls return the cached reg.
    //
    // Single-SSBO compute shaders keep the legacy X2-direct
    // mapping so the host (atrium-vk-icd / tier2 backend)
    // doesn't have to build a table for the common case.
    if func.stage == ShaderStage::Compute && func.ssbo_bindings.len() >= 2 {
        let mut sorted: Vec<(u32, (u32, u32))> = func.ssbo_bindings.iter()
            .map(|(vid, sb)| (*vid, *sb)).collect();
        sorted.sort_by_key(|(_, (_, binding))| *binding);
        // Caller-saved scratch regs that don't overlap the
        // regalloc int pool (X19..X28) or the AAPCS64 arg
        // regs (X0..X7) or platform-reserved X18.  X16/X17
        // are AAPCS64 IP0/IP1 (intra-procedure-call scratch);
        // X12..X15 are general caller-saved temps.  X9..X11
        // are also caller-saved but are touched contextually
        // by the image-sample helper, so left out of this
        // pool.  Six slots = practical ceiling for the
        // pre-materialize strategy; shaders needing more
        // bindings would need the per-access indirect-load
        // model.
        const SSBO_BASE_REGS: [u8; 6] = [16, 17, 13, 14, 15, 12];
        if sorted.len() > SSBO_BASE_REGS.len() {
            return Err(BackendError::Unsupported(format!(
                "bespoke compute supports at most {} SSBO bindings (got {})",
                SSBO_BASE_REGS.len(), sorted.len())));
        }
        // Reserve the SSBO base regs from IntPool: those
        // physical registers (W13..W17 are the same as
        // X13..X17) overlap the int regalloc's primary
        // caller-saved tier.  Without this reservation the
        // body's regalloc would hand them out for arithmetic
        // and overwrite the SSBO pointers we just loaded.
        for &n in &SSBO_BASE_REGS[..sorted.len()] {
            int_pool.free.retain(|&r| r != n);
        }
        for (i, (vid, (_set, binding))) in sorted.iter().enumerate() {
            let dst = asm::Xreg(SSBO_BASE_REGS[i]);
            a.emit(asm::ldr_x_offset(dst, asm::Xreg(2), (binding * 8) as u16));
            pointers.insert(ValueId(*vid), (dst, 0));
            ssbo_base_reloads.push((dst, *binding));
        }
    }

    // Workgroup-shared memory prologue.  When the compute
    // shader declares StorageClass::Workgroup variables, the
    // 10th cs_main argument (`workgroup_buf`, AAPCS64 stack
    // slot SP+8) points at a per-workgroup scratch buffer.
    // Load it once into X8 (AAPCS64 indirect-result reg,
    // unused by the compute calling convention) and pre-
    // populate `pointers` so every Workgroup OpVariable
    // resolves to (X8, var_offset).  The SP+8 offset shifts
    // by frame_bytes after the prologue pushes callee-saved
    // pairs, so the load is a placeholder patched at the end.
    let mut workgroup_buf_load_patch: Option<usize> = None;
    if func.workgroup_size > 0 && !func.workgroup_var_offset.is_empty() {
        let off = a.len();
        // Placeholder: ldr X8, [SP, #0] -- patched to
        // [SP, #(frame_bytes + 8)] post-body.
        a.emit(asm::ldr_x_offset(asm::Xreg(8), asm::Xreg(31), 0));
        workgroup_buf_load_patch = Some(off);
        for (&vid, &voff) in &func.workgroup_var_offset {
            pointers.insert(vid, (asm::Xreg(8), voff as i32));
        }
    }

    // Vertex Location-decorated Output variables: route to
    // X7 (out_varyings) + per-variable byte offset assigned
    // by the frontend in Location order.  Same shape as the
    // cranelift prologue's VS varying routing.  Without
    // this, `resolve_pointer_param`'s generic
    // (Vertex, Output) -> X6 fallback sends varying writes
    // into out_position, clobbering gl_Position and
    // dropping the varying.
    if func.stage == ShaderStage::Vertex
        && !func.output_varying_byte_offset.is_empty()
    {
        for (&vid, &voff) in &func.output_varying_byte_offset {
            pointers.insert(vid, (asm::Xreg(7), voff as i32));
        }
    }

    // Fragment Location-decorated outputs (MRT): route each
    // colour output to X4 (out_color) + the per-Location
    // byte offset.  A single Location=0 output gets offset 0
    // -- identical to the generic (Fragment, Output) -> X4
    // fallback, so single-attachment shaders are unchanged.
    // Multi-attachment shaders write Location=1 at X4+16,
    // etc.; the daemon sizes out_color to cover every
    // Location and scatters each slot to its attachment.
    if func.stage == ShaderStage::Fragment
        && !func.output_varying_byte_offset.is_empty()
    {
        for (&vid, &voff) in &func.output_varying_byte_offset {
            pointers.insert(vid, (asm::Xreg(4), voff as i32));
        }
    }

    // gl_FragDepth: route stores to the FragDepth-decorated
    // Output variable to X5 (out_depth) instead of X4
    // (out_color), so the depth value doesn't clobber colour
    // attachment 0.
    if let Some(vid) = func.frag_depth_output {
        pointers.insert(vid, (asm::Xreg(5), 0));
    }

    // Location-decorated `Input`-storage vars (VS attrs and
    // FS varyings): both stages take the input buffer as
    // X0 (in_attributes for VS, in_varyings for FS).
    // Without per-Location offsets here, every Input load
    // resolves to X0+0 -- two Inputs at Locations 0 and 1
    // would read the same bytes.
    if !func.input_varying_byte_offset.is_empty()
        && matches!(func.stage,
            ShaderStage::Vertex | ShaderStage::Fragment)
    {
        for (&vid, &voff) in &func.input_varying_byte_offset {
            pointers.insert(vid, (asm::Xreg(0), voff as i32));
        }
    }

    // Storage-image prologue.  A compute shader that does
    // OpImageRead / OpImageWrite calls into the runtime
    // (atrium_img_read_2d / atrium_img_write_2d) via the v1
    // descriptor-table ABI.  Those calls clobber the
    // caller-saved registers, so the image-table base (which
    // arrives in X0) is stashed once into X19 -- a callee-
    // saved register the runtime helper preserves.  Reserving
    // X19 forces the callee-saved int prologue (stp/ldp),
    // which saves the caller's X19 before our `mov x19, x0`.
    let uses_storage_image = func.blocks.values().any(|b|
        b.insts.iter().any(|i| matches!(&i.op,
            Op::ImageRead { .. } | Op::ImageWrite { .. }
            | Op::ImageReadLod { .. } | Op::ImageWriteLod { .. }
            | Op::ImageTexelPointer { .. }
            | Op::ImageQuerySize(_))));
    // Op::Barrier also calls through the X19-anchored image
    // table (specifically [X19, #IMG_TABLE_BARRIER_OFFSET]), so
    // any compute shader with a barrier needs the same prologue
    // -- stash X0 into X19 once at function entry so the
    // post-call image-table base survives across barrier calls
    // even when the shader doesn't use storage images.  Arc 150.
    let uses_barrier = func.blocks.values().any(|b|
        b.insts.iter().any(|i| matches!(&i.op, Op::Barrier)));
    if uses_storage_image || uses_barrier {
        int_pool.free.retain(|&r| r != 19);
        int_pool.used_callee_saved = true;
        a.emit(asm::mov_x(asm::Xreg(19), asm::Xreg(0)));
    }

    // Constant values by id — phi-move source fallback: constant
    // arms (incl. OpUndef folded to ConstNull by the frontend)
    // are never materialised into pool registers; the move
    // materialises the immediate at the edge instead.
    let mut phi_const_val: HashMap<ValueId, u32> = HashMap::new();
    for inst in &flat_insts {
        if let Some(r) = inst.result.as_ref() {
            match &inst.op {
                Op::ConstInt { value, .. } =>
                    { phi_const_val.insert(r.id, *value as u32); }
                Op::ConstNull =>
                    { phi_const_val.insert(r.id, 0); }
                Op::ConstFloat { value, kind: FloatKind::F32 } =>
                    { phi_const_val.insert(r.id,
                        (*value as f32).to_bits()); }
                _ => {}
            }
        }
    }

    // Deferred def-store (spill_mode): the PREVIOUS inst's f32
    // results are stored to their slots at the head of the NEXT
    // iteration — arms `continue` freely, and terminators define
    // nothing, so a def is always followed by another inst in
    // the same block (stores stay dominated by the def).
    let mut prev_def_inst: Option<&atrium_spv_ir::Inst> = None;
    let mut flat_i: usize = 0;
    for (block_pos, bid) in block_order.iter().enumerate() {
        let block = func.blocks.get(bid).unwrap();
        block_asm_offset.insert(*bid, a.len());
        // Spill-resident float phis: any reg-cached copy goes
        // stale at block boundaries (the slot is rewritten by
        // predecessor terminators' phi-moves) — drop the cache.
        if spill_mode && !phi_slot_floats.is_empty() {
            for pid in &phi_slot_floats {
                if let Some(v) = scalars.remove(pid) {
                    owners.remove(&v.0);
                    free_pool.push(v.0);
                    spilled.insert(*pid);
                }
            }
        }
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
        if spill_mode {
            // Recycle slots of values that died (skip ids with no
            // last_use entry — synthetic lanes — they'd read as
            // immortal-dead).
            {
                let mut dead_slots: Vec<(ValueId, u32)> = spill_slot
                    .iter()
                    .filter(|(sid, _)| match last_use.get(sid) {
                        Some(&lu) => lu < i,
                        // Synthetic lane: alive while ANY
                        // containing vector is (lanes alias
                        // across shuffles/composites).
                        None => !vectors.iter().any(|(vid, lanes)| {
                            lanes.iter().any(|l| l.id == **sid)
                                && last_use.get(vid)
                                    .is_some_and(|&v| v >= i)
                        }),
                    })
                    .map(|(sid, slot)| (*sid, *slot))
                    .collect();
                dead_slots.sort_unstable_by_key(|(_, slot)| *slot);
                for (sid, slot) in dead_slots {
                    spill_slot.remove(&sid);
                    spilled.remove(&sid);
                    spilled_w.remove(&sid);
                    spill_free.push(slot);
                }
            }
            if let Some(prev) = prev_def_inst.take() {
                // (sid, effective last_use): synthetic lane ids
                // are absent from last_use — they inherit the
                // parent vector's liveness, else they'd be
                // skipped as dead and stay unevictable forever.
                let mut to_store: Vec<(ValueId, usize)> = Vec::new();
                if let Some(r) = prev.result.as_ref() {
                    let r_lu = last_use.get(&r.id).copied().unwrap_or(0);
                    match &r.ty {
                        Type::F32 | Type::I32 | Type::U32 =>
                            to_store.push((r.id, r_lu)),
                        Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                            if let Some(lanes) = vectors.get(&r.id) {
                                to_store.extend(lanes.iter().map(|l| (
                                    l.id,
                                    last_use.get(&l.id).copied()
                                        .unwrap_or(r_lu),
                                )));
                            }
                        }
                        _ => {}
                    }
                }
                for (sid, eff_lu) in to_store {
                    // Already-dead defs don't need a slot.
                    if eff_lu < i {
                        continue;
                    }
                    let in_v = scalars.get(&sid).copied();
                    let in_w = if in_v.is_none() {
                        ints.get(&sid).copied()
                    } else { None };
                    if in_v.is_none() && in_w.is_none() { continue; }

                    let slot = *spill_slot.entry(sid)
                        .or_insert_with(|| {
                            spill_free.pop().unwrap_or_else(|| {
                                let sl = spill_next;
                                spill_next += 1;
                                sl
                            })
                        });
                    if slot >= SPILL_SLOT_CAP {
                        return Err(BackendError::Unsupported(
                            "spill area exhausted (1000 slots)".into()));
                    }
                    if let Some(v) = in_v {
                        a.emit(asm::str_d_offset(
                            v, asm::Xreg(31), (slot as u16) * 8));
                    } else if let Some(w) = in_w {
                        a.emit(asm::str_w_offset(
                            w, asm::Xreg(31), (slot as u16) * 8));
                    }
                }
            }
            prev_def_inst = Some(inst);
        }
        pcmap_entries.push((a.len() as u32, inst.source_spirv_offset));
        // Expire scalars whose last_use < i. Their V-regs
        // return to the free pool. Drain into a temp Vec
        // first to dodge the borrow checker.
        let mut dead: Vec<u8> = owners.iter()
            .filter_map(|(n, id)|
                if last_use.get(id).copied().unwrap_or(usize::MAX) < i {
                    Some(*n)
                } else { None })
            .collect();
        // Deterministic pool order: HashMap iteration must not
        // leak into allocation order (content-addressed shaders
        // need bit-identical codegen).
        dead.sort_unstable();
        for n in dead {
            if n == 30 && std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok() {
                eprintln!("V30 EXPIRED at i={i} owner={:?} lu={:?}",
                    owners.get(&n), owners.get(&n)
                        .and_then(|id| last_use.get(id)));
            }
            owners.remove(&n);
            free_pool.push(n);
        }
        // Vec-lane synth liveness:  Op::Load(vec4) and the
        // per-lane FAdd/FSub/FMul/FDiv/FAbs/FSqrt/FMin/FMax/
        // FRINT* emits all allocate per-lane V-regs under
        // synthetic ValueIds that aren't in the IR-driven
        // last_use map.  Without explicit reclamation they
        // live forever -- a long vec chain (Load -> FAdd ->
        // FDiv -> FSqrt -> FMax -> FMin -> Store) accumulates
        // 6 * 4 = 24 V-regs, which exhausts the 24-slot pool.
        //
        // Tie synth-lane V-regs to their TOP Value's last_use.
        // When the top expires, the lanes are unreachable
        // (only `vectors[top]` could route to them, and that
        // entry is removed here too).
        //
        // Safe now that OpVectorExtract copies via mov_v_16b
        // / mov_w instead of aliasing the source lane's
        // V-reg.  Without the copy, freeing a source vec's
        // synth-lane V-reg here would invalidate any extract
        // result that aliased the same V-reg (the matrix
        // tests were the canary -- pre-copy, this expire
        // corrupted MatrixTimesVector accumulators).
        let mut dead_vecs: Vec<ValueId> = vectors.iter()
            .filter_map(|(top, _)|
                if last_use.get(top).copied().unwrap_or(usize::MAX) < i {
                    Some(*top)
                } else { None })
            .collect();
        dead_vecs.sort_unstable_by_key(|v| v.0);
        for top in dead_vecs {
            if let Some(lanes) = vectors.remove(&top) {
                for lane in lanes {
                    if let Some(&vreg) = scalars.get(&lane.id) {
                        if owners.get(&vreg.0) == Some(&lane.id) {
                            // Constants are routinely shared
                            // across multiple ConstVecs (e.g.
                            // vec4(0) and vec3(0) both pin
                            // ConstFloat 0.0).  Two guards
                            // prevent premature reclamation:
                            //   (1) any OTHER still-live entry
                            //       in `vectors` references the
                            //       lane.id; OR
                            //   (2) the lane's own last_use is
                            //       still in the future (a
                            //       downstream ConstVec hasn't
                            //       emitted yet — Arc 42
                            //       ProjDref pattern: c_one is
                            //       consumed by both `uvq` and
                            //       a later pixel-composite
                            //       vec4, and when uvq dies the
                            //       pixel ConstVec isn't yet in
                            //       `vectors`).
                            let still_referenced = vectors.values()
                                .any(|ls| ls.iter().any(|v| v.id == lane.id));
                            // Missing last_use = SYNTHETIC lane:
                            // reachable only through `vectors`,
                            // which guard (1) just checked — treat
                            // as dead, not immortal. (unwrap_or
                            // MAX pinned every dead chain's lane
                            // regs forever; the 24-V-reg example
                            // in the comment above was never
                            // actually fixed for synth ids.)
                            let lane_alive = last_use.get(&lane.id)
                                .is_some_and(|&lu| lu >= i);
                            if !(still_referenced || lane_alive) {
                                owners.remove(&vreg.0);
                                free_pool.push(vreg.0);
                                scalars.remove(&lane.id);
                            }
                        }
                    }
                }
            }
        }
        // Same expiry pass for the integer W-reg pool.
        int_pool.expire(i, &last_use);
        // And for the bool W-pool: free slots whose owner's
        // last_use is in the past.
        let mut dead_bools: Vec<u8> = bool_owners.iter()
            .filter_map(|(n, id)|
                if last_use.get(id).copied().unwrap_or(usize::MAX) < i {
                    Some(*n)
                } else { None })
            .collect();
        dead_bools.sort_unstable();
        for n in dead_bools {
            if let Some(id) = bool_owners.remove(&n) {
                bools.remove(&id);
                bool_free.push(n);
            }
        }

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

        // ── spill mode: int reloads + W-pool headroom relief ──
        // Reload spilled ints this inst reads, then evict
        // far-future slot-valid ints until the pool has enough
        // headroom for any arm's internal allocations (the int
        // emit helpers allocate without spill awareness).
        if spill_mode {
            if !spilled_w.is_empty() {
                let mut need: Vec<ValueId> = spilled_w.iter()
                    .filter(|id| op_reads(&inst.op, **id))
                    .copied().collect();
                need.sort_by_key(|v| v.0);
                for (vid, lanes) in &vectors {
                    if op_reads(&inst.op, *vid) {
                        need.extend(lanes.iter()
                            .map(|l| l.id)
                            .filter(|lid| spilled_w.contains(lid)));
                    }
                }
                // A deferred pointer's index is read wherever the
                // POINTER is read (the consumer re-materialises
                // the address from it).
                for (pid, (_, _, idx_id, _)) in &deferred_ptr {
                    if op_reads(&inst.op, *pid)
                        && spilled_w.contains(idx_id)
                    {
                        need.push(*idx_id);
                    }
                }
                for id in need {
                    let slot = *spill_slot.get(&id).ok_or_else(||
                        BackendError::Internal(format!(
                            "int reload of {id:?} without slot")))?;
                    if int_pool.free.is_empty()
                        && !wevict_one(&mut int_pool, &mut ints,
                            &mut spilled_w, &spill_slot, &vectors, &deferred_ptr,
                            &last_use, &phi_pinned, &inst.op)
                    {
                        return Err(BackendError::Unsupported(
                            "W pressure with no spillable victim".into()));
                    }
                    let w = int_pool.alloc(id)?;
                    a.emit(asm::ldr_w_offset(
                        w, asm::Xreg(31), (slot as u16) * 8));
                    ints.insert(id, w);
                    spilled_w.remove(&id);
                }
            }
            const W_HEADROOM: usize = 8;
            while int_pool.free.len() < W_HEADROOM {
                if !wevict_one(&mut int_pool, &mut ints,
                    &mut spilled_w, &spill_slot, &vectors, &deferred_ptr,
                    &last_use, &phi_pinned, &inst.op)
                {
                    if std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok()
                        && int_pool.free.len() < 8
                    {
                        let mut d = String::new();
                        for (n, oid) in &int_pool.owners {
                            d.push_str(&format!(
                                "W{n}:{:?}p{}s{}i{}r{}; ", oid,
                                phi_pinned.contains(oid) as u8,
                                spill_slot.contains_key(oid) as u8,
                                (ints.get(oid) == Some(&asm::Wreg(*n))) as u8,
                                op_reads(&inst.op, *oid) as u8));
                        }
                        eprintln!("W-STALL i={i} free={} op={:?} [{d}]",
                            int_pool.free.len(), inst.op);
                    }
                    break; // nothing evictable; arms may still fit
                }
            }
            // Same for the V file: the FP emit helpers allocate
            // internally without spill awareness, so keep enough
            // headroom for any arm (vec ops want up to 8 lanes).
            const V_HEADROOM: usize = 10;
            while free_pool.len() < V_HEADROOM {
                if !vevict_one(&mut free_pool, &mut owners,
                    &mut scalars, &mut spilled, &spill_slot,
                    &vectors, &last_use, &phi_pinned, &inst.op)
                {
                    break;
                }
            }
        }

        // ── spill mode: reload this inst's spilled operands ──
        // (directly read values + lanes of read vectors). Runs
        // after expire so freed regs are preferred; `ldr`
        // preserves NZCV, so compare→branch fusion is safe.
        if spill_mode && !spilled.is_empty() {
            let mut need: Vec<ValueId> = spilled.iter()
                .filter(|id| op_reads(&inst.op, **id))
                .copied()
                .collect();
            need.sort_by_key(|v| v.0);
            for (vid, lanes) in &vectors {
                if op_reads(&inst.op, *vid) {
                    need.extend(lanes.iter()
                        .map(|l| l.id)
                        .filter(|lid| spilled.contains(lid)));
                }
            }
            for id in need {
                vread_impl(&mut a, &mut free_pool, &mut owners,
                    &mut used_callee_saved_v, &mut scalars,
                    &mut spilled, &spill_slot, &vectors, &last_use,
                    &phi_pinned, spill_mode, &inst.op, id)?;
            }
        }

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
                } else if use_counts.get(&result.id).copied().unwrap_or(0) == 0 {
                    // Orphan ConstInt -- typically an AccessChain
                    // index pre-materialised by the frontend's
                    // entry-block prelude that the codegen
                    // resolves inline.  Without this skip the
                    // orphan pins a W-reg forever (its last_use
                    // is None, so linear-scan never reclaims).
                } else {
                    let w = alloc_int_w(&mut int_pool, result.id)?;
                    materialise_u32_into_w(&mut a, w, *value as i32 as u32);
                    ints.insert(result.id, w);
                    // OpConstantTrue / OpConstantFalse (and the
                    // SpecConstant variants) lower to a ConstInt
                    // with result.ty == Bool.  Bool consumers
                    // (Select, BranchCond) read the `bools` map,
                    // so also register the W-reg there.
                    if matches!(result.ty, Type::Bool) {
                        bools.insert(result.id, w);
                    }
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
            Op::Clz(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("Clz without result".into()))?;
                let s_w = *ints.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Clz operand {:?} not in ints", s.id)))?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                a.emit(asm::clz_w(d_w, s_w));
                ints.insert(result.id, d_w);
            }
            Op::Rbit(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("Rbit without result".into()))?;
                let s_w = *ints.get(&s.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Rbit operand {:?} not in ints", s.id)))?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                a.emit(asm::rbit_w(d_w, s_w));
                ints.insert(result.id, d_w);
            }
            Op::PackHalf2x16(v) => {
                // vec2<f32> -> u32.  fcvt each lane to f16
                // (low 16 of a V-reg, fmov bridges to a W),
                // then result = lane0 | (lane1 << 16).
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("PackHalf2x16 without result".into()))?;
                let lanes = vectors.get(&v.id).cloned().ok_or_else(||
                    BackendError::Internal(format!(
                        "PackHalf2x16 operand {:?} not a vector", v.id)))?;
                if lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "PackHalf2x16 needs a vec2, got {} lanes", lanes.len())));
                }
                let s0 = *scalars.get(&lanes[0].id).ok_or_else(||
                    BackendError::Internal("PackHalf2x16 lane 0 missing".into()))?;
                let s1 = *scalars.get(&lanes[1].id).ok_or_else(||
                    BackendError::Internal("PackHalf2x16 lane 1 missing".into()))?;
                let synth = ValueId(next_synth_id);
                next_synth_id += 1;
                let tv = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
                let d_w = alloc_int_w(&mut int_pool, result.id)?;
                // lane 0 -> d_w (fcvt zeroes the upper 16).
                a.emit(asm::fcvt_h_from_s(tv, s0));
                a.emit(asm::fmov_w_from_s(d_w, tv));
                // lane 1 -> w_tmp, shift up, or in.
                a.emit(asm::fcvt_h_from_s(tv, s1));
                a.emit(asm::fmov_w_from_s(w_tmp, tv));
                a.emit(asm::lsl_imm_w(w_tmp, w_tmp, 16));
                a.emit(asm::orr_w(d_w, d_w, w_tmp));
                owners.remove(&tv.0);
                free_pool.push(tv.0);
                ints.insert(result.id, d_w);
            }
            Op::UnpackHalf2x16(v) => {
                // u32 -> vec2<f32>.  Bridge low / high 16
                // bits into a V-reg, fcvt each from f16.
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("UnpackHalf2x16 without result".into()))?;
                let s_w = *ints.get(&v.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "UnpackHalf2x16 operand {:?} not in ints", v.id)))?;
                let synth0 = ValueId(next_synth_id); next_synth_id += 1;
                let v0 = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth0)?;
                let synth1 = ValueId(next_synth_id); next_synth_id += 1;
                let v1 = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth1)?;
                let synth_t = ValueId(next_synth_id); next_synth_id += 1;
                let tv = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth_t)?;
                // lane 0: fcvt reads the low 16 of the bridge.
                a.emit(asm::fmov_s_from_w(tv, s_w));
                a.emit(asm::fcvt_s_from_h(v0, tv));
                // lane 1: high 16 shifted down first.
                a.emit(asm::lsr_imm_w(w_tmp, s_w, 16));
                a.emit(asm::fmov_s_from_w(tv, w_tmp));
                a.emit(asm::fcvt_s_from_h(v1, tv));
                owners.remove(&tv.0);
                free_pool.push(tv.0);
                scalars.insert(synth0, v0);
                scalars.insert(synth1, v1);
                vectors.insert(result.id, vec![
                    Value { id: synth0, ty: Type::F32 },
                    Value { id: synth1, ty: Type::F32 },
                ]);
            }
            // Integer comparisons → Bool W-reg.
            Op::IEq(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r,
                asm::Cond::Eq)?,
            Op::INe(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Ne)?,
            Op::SLt(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Lt)?,
            Op::SLe(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Le)?,
            Op::SGt(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Gt)?,
            Op::SGe(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Ge)?,
            Op::ULt(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Cc)?,
            Op::ULe(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Ls)?,
            Op::UGt(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, l, r, asm::Cond::Hi)?,
            Op::UGe(l, r) => emit_icmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &ints, &mut bools, &mut bool_owners, &mut bool_free,
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
                let d_v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                a.emit(asm::scvtf_s_from_w(d_v, s_w));
                scalars.insert(result.id, d_v);
            }
            Op::ConvertUToF(s) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConvertUToF without result".into()))?;
                // Accept either an int W-reg (regular u32) or
                // a bool W-reg (i32 0/1 from a float compare).
                // Both live in W-regs and ucvtf treats them
                // identically.  This enables synthesised
                // lowerings like FSign / FStep which feed
                // float-compare results into FConvert.
                let from_bools = bools.get(&s.id).copied();
                let from_bool_v = if ints.get(&s.id).is_none()
                    && from_bools.is_none()
                {
                    bool_in_v.get(&s.id).copied()
                } else {
                    None
                };
                let from_bool_slot = if ints.get(&s.id).is_none()
                    && from_bools.is_none() && from_bool_v.is_none()
                {
                    bool_in_slot.get(&s.id).copied()
                } else {
                    None
                };
                if let Some(v) = from_bool_v {
                    a.emit(asm::fmov_w_from_s(w_tmp, v));
                } else if let Some(slot) = from_bool_slot {
                    a.emit(asm::ldr_w_offset(w_tmp,
                        asm::Xreg(31), (slot as u16) * 8));
                }
                let s_w = ints.get(&s.id).copied()
                    .or(from_bools)
                    .or(from_bool_v.map(|_| w_tmp))
                    .or(from_bool_slot.map(|_| w_tmp))
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ConvertUToF operand {:?} not in ints, bools, \
                         bool_in_v, or bool_in_slot",
                        s.id)))?;
                let d_v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                a.emit(asm::ucvtf_s_from_w(d_v, s_w));
                scalars.insert(result.id, d_v);
                // Eager-free the bool W-reg if the bool was
                // the source.  ConvertUToF is the canonical
                // last-and-only consumer of synthesised
                // FSign/FStep bool intermediates -- without
                // this targeted recycle, those bools pile up
                // in the 3-slot W10..W12 pool and exhaust it
                // (e.g. 2 sign + 2 step = 6 bools, but only
                // 3 slots).  Return the slot to bool_free
                // so subsequent allocs reuse it.
                // Free the bool's W-pool reg only when this
                // conversion is its SOLE consumer — the bool may
                // also feed a phi arm or a later Select (latent
                // bug exposed by spill-mode shaders: the phi-move
                // found its source freed).
                if from_bools.is_some()
                    && use_counts.get(&s.id).copied() == Some(1)
                {
                    if let Some(w_freed) = bools.remove(&s.id) {
                        bool_owners.remove(&w_freed.0);
                        bool_free.push(w_freed.0);
                    }
                }
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
            Op::Bitcast(s, target_ty) => {
                // Pure reinterpret between i32/u32 and f32.
                // Three cases:
                //   - int -> f32:  fmov_s_from_w  (W-reg -> V-reg S-lane)
                //   - f32 -> int:  fmov_w_from_s  (V-reg S-lane -> W-reg)
                //   - int -> int:  alias the same W-reg (i32 <-> u32
                //                  is a no-op at the hardware level).
                // The frontend translates SPIR-V OpBitcast for every
                // {int,float} <-> {int,float} pairing, including the
                // identity-shaped int<->int from `bitcast(u32, i32)`
                // that real shaders emit when reinterpreting a signed
                // index as an unsigned one (or vice versa).
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Bitcast without result".into()))?;
                let to_float = matches!(target_ty, Type::F32);
                let src_in_ints = ints.contains_key(&s.id);
                let src_in_scalars = scalars.contains_key(&s.id);
                match (to_float, src_in_ints, src_in_scalars) {
                    (true, true, _) => {
                        let s_w = *ints.get(&s.id).unwrap();
                        let d_v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                        a.emit(asm::fmov_s_from_w(d_v, s_w));
                        scalars.insert(result.id, d_v);
                    }
                    (false, _, true) => {
                        let s_v = *scalars.get(&s.id).unwrap();
                        let d_w = alloc_int_w(&mut int_pool, result.id)?;
                        a.emit(asm::fmov_w_from_s(d_w, s_v));
                        ints.insert(result.id, d_w);
                    }
                    (false, true, false) => {
                        // int -> int: copy into a fresh W-reg.
                        // Aliasing (sharing the source's Wreg
                        // under the result's ValueId) looks
                        // attractive but breaks once the
                        // source's last_use triggers the
                        // int_pool to reclaim the Wreg --
                        // any later use of the result then
                        // reads a clobbered register.  A
                        // single `mov w_dst, w_src` keeps the
                        // result's lifetime independent.
                        let s_w = *ints.get(&s.id).unwrap();
                        let d_w = alloc_int_w(&mut int_pool, result.id)?;
                        a.emit(asm::mov_w(d_w, s_w));
                        ints.insert(result.id, d_w);
                    }
                    (true, false, true) => {
                        // f32 -> f32 (degenerate but legal):
                        // copy via fmov rather than alias for
                        // the same liveness-correctness reason.
                        let s_v = *scalars.get(&s.id).unwrap();
                        let d_v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                        a.emit(asm::fmov_s(d_v, s_v));
                        scalars.insert(result.id, d_v);
                    }
                    _ => return Err(BackendError::Internal(format!(
                        "Bitcast: operand {:?} not in ints or scalars",
                        s.id))),
                }
            }
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstFloat without result".into()))?;
                if std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok() {
                    eprintln!("CF {:?} v={} dead={} uc={:?}",
                        result.id, value,
                        dead_const_floats.contains(&result.id),
                        use_counts.get(&result.id));
                }
                // Dead — only used by pool-eligible ConstVecs,
                // which read the literal pool instead. Skip
                // the materialise to drop the prologue cost.
                if dead_const_floats.contains(&result.id) { continue; }
                // Orphan ConstFloat (no consumer reaches it,
                // e.g. a pre-materialised entry-block constant
                // whose only "use" was inlined by codegen): a
                // last_use=None pins the V-reg forever, so
                // skip materialisation.  Mirrors the orphan-
                // ConstInt skip a few arms up.
                if use_counts.get(&result.id).copied().unwrap_or(0) == 0 {
                    continue;
                }
                let bits = (*value as f32).to_bits();
                let v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                materialise_u32_into_w(&mut a, w_tmp, bits);
                a.emit(asm::fmov_s_from_w(v, w_tmp));
                scalars.insert(result.id, v);
            }
            Op::ConstVec(elements) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConstVec without result".into()))?;
                if packed_ids.contains(&result.id) {
                    let q = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
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
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Eq)?,
            Op::FOrdNe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ne)?,
            Op::FOrdLt(a_v, b_v) => emit_fcmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Mi)?,
            Op::FOrdLe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ls)?,
            Op::FOrdGt(a_v, b_v) => emit_fcmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Gt)?,
            Op::FOrdGe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ge)?,
            // FUnordNe (a != b, true when either is NaN):
            // after `fcmp`, Z is set only on an ordered-equal
            // result, so `NE` (Z==0) is exactly "unordered or
            // not-equal".  This is the lowering OpIsNan rides
            // (IsNan(x) == FUnordNe(x, x)).
            Op::FUnordNe(a_v, b_v) => emit_fcmp_to_bool(
                &mut a,
                &mut BoolOverflow {
                    bool_in_slot: &mut bool_in_slot,
                    spill_next: &mut spill_next,
                    spill_free: &mut spill_free,
                    spill_mode,
                }, &scalars, &mut bools, &mut bool_owners, &mut bool_free,
                &mut fused_branch, fuse_eligible, inst, a_v, b_v, asm::Cond::Ne)?,
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
                        let v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
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
            // OpVectorExtract: copy lane `index`'s register
            // value into a FRESH register owned by `result.id`.
            //
            // Earlier this op aliased the lane's V-reg into
            // the result's scalars/ints entry, which was
            // ~free but had a structural problem: the source
            // vec's synth-lane V-regs and the result's V-reg
            // shared physical registers but different owner
            // ValueIds.  Any V-pool expire driven by the
            // source vec's last_use would free the V-reg
            // while the extract result was still live --
            // corrupting downstream reads.
            //
            // Fix: copy via `mov v.16b` (f32) or `mov w` (i32).
            // Both forms are ORR-aliases that target cores
            // rename-eliminate (zero-latency on Apple
            // Firestorm/Avalanche, Cortex-A715, etc.) -- so
            // the apparent +1 instruction is approximately
            // free at runtime, but the result NOW owns a
            // distinct V-reg and the source vec's lanes can
            // expire independently.  This unblocks the
            // vec-lane synth-liveness expire pass that
            // closes the V-pool exhaustion seen on long vec
            // chains (5+ ops sharing Load-synth lanes).
            Op::VectorExtract { vector, index } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "VectorExtract without result".into()))?;
                if let Some(&(param, base_off)) =
                    int_vec_ptr.get(&vector.id)
                {
                    // Deferred int-vector load: `ldr w` the lane
                    // straight from its home (push constants /
                    // uniforms are immutable for the dispatch).
                    let lane_off = base_off
                        .saturating_add((*index as usize * 4) as i32);
                    if lane_off < 0 || lane_off > u16::MAX as i32 {
                        return Err(BackendError::Unsupported(format!(
                            "int vec lane offset {lane_off} out of range")));
                    }
                    let d = int_pool.alloc(result.id)?;
                    a.emit(asm::ldr_w_offset(d, param, lane_off as u16));
                    ints.insert(result.id, d);
                    continue;
                }
                let lanes = vectors.get(&vector.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "VectorExtract source {:?} not a vec", vector.id)))?;
                let lane = lanes.get(*index as usize).ok_or_else(||
                    BackendError::Unsupported(format!(
                        "VectorExtract index {index} out of range \
                         ({} lanes)", lanes.len())))?;
                if let Some(s) = scalars.get(&lane.id).copied() {
                    let d = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                    a.emit(asm::mov_v_16b(d, s));
                    scalars.insert(result.id, d);
                } else if let Some(w) = ints.get(&lane.id).copied() {
                    let d = int_pool.alloc(result.id)?;
                    a.emit(asm::mov_w(d, w));
                    ints.insert(result.id, d);
                } else {
                    return Err(BackendError::Internal(format!(
                        "VectorExtract lane {:?} not in scalars or ints; \
                         spw={} spv={} slot={:?} vec={:?} parents={:?}",
                        lane.id,
                        spilled_w.contains(&lane.id),
                        spilled.contains(&lane.id),
                        spill_slot.get(&lane.id),
                        vector.id,
                        vectors.iter().filter(|(_, ls)| ls.iter()
                            .any(|l| l.id == lane.id))
                            .map(|(v, _)| *v).collect::<Vec<_>>())
                        + &format!(" i={i} curop={:?}", inst.op)));
                }
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
                let acc = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
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
                    let tmp = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, tmp_synth)?;
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
                    let acc = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, acc_id)?;
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
                        let tmp = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, tmp_synth)?;
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
            Op::Fma(a_v, b_v, c_v) => emit_fma(
                &mut a, &mut scalars, &mut free_pool, &mut owners,
                &mut used_callee_saved_v, coalesce_dest.as_ref(), inst, a_v, b_v, c_v)?,
            Op::Store { ptr, value } => {
                // Accept any writable storage class -- the
                // resolve_or_make_pointer storage-class table
                // is the source of truth for what's writable.
                // It returns (param_xreg, byte_offset); the
                // byte_offset honours any prior OpAccessChain
                // (compute SSBO writes flow through this
                // path, as do fragment/vertex Output stores).
                let (ptr_param, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, ptr, &mut pointers, func.stage)?;
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
                        // Direct FP store — NO W9 staging: when
                        // ptr_param is the X9 deferred-pointer
                        // scratch, `fmov w9, s` destroyed the
                        // base after the first lane.
                        let offset_bytes = base_off_u16 + (lane_i as u16) * 4;
                        a.emit(asm::str_s_offset(sreg, ptr_param, offset_bytes));
                    }
                    continue;
                }
                // Scalar int store (u32/i32 SSBO write).
                if let Some(&w) = ints.get(&value.id) {
                    a.emit(asm::str_w_offset(w, ptr_param, base_off_u16));
                    continue;
                }
                // Scalar f32 store (direct FP store — see the
                // lane-store comment re the X9 scratch).
                if let Some(&sreg) = scalars.get(&value.id) {
                    a.emit(asm::str_s_offset(sreg, ptr_param, base_off_u16));
                    continue;
                }
                return Err(BackendError::Unsupported(format!(
                    "Op::Store value {:?} not in packed/vectors/ints/scalars",
                    value.id)));
            }
            // GLSL.std.450 math (scalar f32 + NEON vec4f + per-lane vec).
            Op::FFloor(x) | Op::FCeil(x) | Op::FTrunc(x) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("FRINT* without result".into()))?;
                let pick_v4s = |op: &Op, d, s| match op {
                    Op::FFloor(_) => asm::frintm_v_4s(d, s),
                    Op::FCeil(_)  => asm::frintp_v_4s(d, s),
                    Op::FTrunc(_) => asm::frintz_v_4s(d, s),
                    _ => unreachable!(),
                };
                let pick_s = |op: &Op, d, s| match op {
                    Op::FFloor(_) => asm::frintm_s(d, s),
                    Op::FCeil(_)  => asm::frintp_s(d, s),
                    Op::FTrunc(_) => asm::frintz_s(d, s),
                    _ => unreachable!(),
                };
                if packed_ids.contains(&result.id) {
                    let v_src = *packed.get(&x.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "FRINT* packed source {:?} not packed", x.id)))?;
                    let v_dst = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                    a.emit(pick_v4s(&inst.op, v_dst, v_src));
                    packed.insert(result.id, v_dst);
                } else if matches!(result.ty, Type::F32) {
                    let v_src = *scalars.get(&x.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "FRINT* source {:?} not in scalars", x.id)))?;
                    let v_dst = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                    a.emit(pick_s(&inst.op, v_dst, v_src));
                    scalars.insert(result.id, v_dst);
                } else if let Some(lanes) = vectors.get(&x.id).cloned() {
                    let mut out_lanes = Vec::with_capacity(lanes.len());
                    for lane in &lanes {
                        let src = *scalars.get(&lane.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "FRINT* lane source {:?} not in scalars",
                                lane.id)))?;
                        let synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        let v_dst = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
                        a.emit(pick_s(&inst.op, v_dst, src));
                        scalars.insert(synth, v_dst);
                        out_lanes.push(Value { id: synth, ty: lane.ty.clone() });
                    }
                    vectors.insert(result.id, out_lanes);
                } else {
                    return Err(BackendError::Unsupported(format!(
                        "FRINT* on {:?} -- value not in scalars/packed/vectors",
                        result.ty)));
                }
            }
            Op::FAbs(x) | Op::FSqrt(x) | Op::FNeg(x) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("F-unary without result".into()))?;
                // Packed vec4 (single Q-reg) path.
                if packed_ids.contains(&result.id) {
                    let v_src = *packed.get(&x.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "F-unary packed source {:?} not packed", x.id)))?;
                    let v_dst = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                    let enc = match &inst.op {
                        Op::FAbs(_)  => asm::fabs_v_4s(v_dst, v_src),
                        Op::FSqrt(_) => asm::fsqrt_v_4s(v_dst, v_src),
                        Op::FNeg(_)  => asm::fneg_v_4s(v_dst, v_src),
                        _ => unreachable!(),
                    };
                    a.emit(enc);
                    packed.insert(result.id, v_dst);
                } else if matches!(result.ty, Type::F32) {
                    let v_src = *scalars.get(&x.id).ok_or_else(||
                        BackendError::Internal(format!(
                            "F-unary source {:?} not in scalars", x.id)))?;
                    let v_dst = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                    let enc = match &inst.op {
                        Op::FAbs(_)  => asm::fabs_s(v_dst, v_src),
                        Op::FSqrt(_) => asm::fsqrt_s(v_dst, v_src),
                        Op::FNeg(_)  => asm::fneg_s(v_dst, v_src),
                        _ => unreachable!(),
                    };
                    a.emit(enc);
                    scalars.insert(result.id, v_dst);
                } else if let Some(lanes) = vectors.get(&x.id).cloned() {
                    // Per-lane vec path (loaded vec4 not in
                    // the packed clique): synthesise lane
                    // result Values, emit one scalar inst
                    // per lane.
                    let mut out_lanes = Vec::with_capacity(lanes.len());
                    for lane in &lanes {
                        let src = *scalars.get(&lane.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "F-unary lane source {:?} not in scalars",
                                lane.id)))?;
                        let synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        let v_dst = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
                        let enc = match &inst.op {
                            Op::FAbs(_)  => asm::fabs_s(v_dst, src),
                            Op::FSqrt(_) => asm::fsqrt_s(v_dst, src),
                            Op::FNeg(_)  => asm::fneg_s(v_dst, src),
                            _ => unreachable!(),
                        };
                        a.emit(enc);
                        scalars.insert(synth, v_dst);
                        out_lanes.push(Value {
                            id: synth, ty: lane.ty.clone(),
                        });
                    }
                    vectors.insert(result.id, out_lanes);
                } else {
                    return Err(BackendError::Unsupported(format!(
                        "F-unary on {:?} -- value not in scalars, packed, \
                         or vectors map", result.ty)));
                }
            }
            Op::FMin(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors, &mut packed, &packed_ids,
                &mut free_pool, &mut owners, &mut used_callee_saved_v, &mut next_synth_id,
                coalesce_dest.as_ref(), inst, a_v, b_v, asm::fmin_s, asm::fmin_v_4s)?,
            Op::FMax(a_v, b_v) => emit_fp_binop_poly(
                &mut a, &mut scalars, &mut vectors, &mut packed, &packed_ids,
                &mut free_pool, &mut owners, &mut used_callee_saved_v, &mut next_synth_id,
                coalesce_dest.as_ref(), inst, a_v, b_v, asm::fmax_s, asm::fmax_v_4s)?,
            Op::AccessChain { base, byte_offset } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "AccessChain without result".into()))?;
                let (param, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, base, &mut pointers, func.stage)?;
                let new_off = base_off.saturating_add(*byte_offset as i32);
                pointers.insert(result.id, (param, new_off));
            }
            Op::AtomicLoad(ptr) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "AtomicLoad without result".into()))?;
                let (x_base, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, ptr, &mut pointers, func.stage)?;
                if base_off < 0 || base_off > u16::MAX as i32 {
                    return Err(BackendError::Unsupported(format!(
                        "AtomicLoad ptr offset {base_off} outside imm12 range")));
                }
                let w_dst = int_pool.alloc(result.id)?;
                a.emit(asm::ldr_w_offset(w_dst, x_base, base_off as u16));
                ints.insert(result.id, w_dst);
            }
            Op::AtomicStore { ptr, value } => {
                let (x_base, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, ptr, &mut pointers, func.stage)?;
                if base_off < 0 || base_off > u16::MAX as i32 {
                    return Err(BackendError::Unsupported(format!(
                        "AtomicStore ptr offset {base_off} outside imm12 range")));
                }
                let w_val = *ints.get(&value.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "AtomicStore value {:?} not in ints", value.id)))?;
                a.emit(asm::str_w_offset(w_val, x_base, base_off as u16));
            }
            Op::AtomicCompareExchange { ptr, expected, desired } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "AtomicCompareExchange without result".into()))?;
                let (x_base, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, ptr, &mut pointers, func.stage)?;
                if base_off < 0 {
                    return Err(BackendError::Unsupported(format!(
                        "AtomicCompareExchange ptr offset {base_off} negative")));
                }
                let w_exp = *ints.get(&expected.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "AtomicCompareExchange expected {:?} not in ints",
                        expected.id)))?;
                let w_des = *ints.get(&desired.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "AtomicCompareExchange desired {:?} not in ints",
                        desired.id)))?;
                // Lower with CASAL Ws, Wt, [Xn] (LSE):
                //   tmp = *Xn; if tmp == Ws { *Xn = Wt }; Ws = tmp.
                // Caveat: CASAL overwrites Ws with the original
                // value -- if w_exp is still live downstream we
                // must not clobber it.  Copy expected into W9
                // scratch, then mov the result into a fresh
                // int-pool reg for the IR result.
                let w_scratch = asm::Wreg(9);
                let addr_x = if base_off == 0 {
                    x_base
                } else {
                    // We need W9 for both the address computation
                    // AND the cas scratch.  Compute address first
                    // into a int-pool temp, then reuse W9 for cas.
                    let temp = int_pool.alloc(ValueId(u32::MAX - inst.result.as_ref().map(|r| r.id.0).unwrap_or(0)))?;
                    materialise_u32_into_w(&mut a, temp, base_off as u32);
                    let temp_x = asm::Xreg(temp.0);
                    a.emit(asm::add_x(temp_x, x_base, temp_x));
                    // We'll free temp at the end manually.
                    temp_x
                };
                a.emit(asm::mov_w(w_scratch, w_exp));
                a.emit(asm::casal_w(w_scratch, w_des, addr_x));
                let w_old = int_pool.alloc(result.id)?;
                a.emit(asm::mov_w(w_old, w_scratch));
                ints.insert(result.id, w_old);
                // (If we allocated a temp for address, it's now
                // dead -- but the int_pool's linear-scan expiry
                // won't reclaim it since the synth ValueId isn't
                // in last_use.  Free explicitly.)
                if base_off != 0 {
                    int_pool.free(asm::Wreg(addr_x.0));
                }
            }
            Op::AtomicIAdd { ptr, value }
            | Op::AtomicAnd { ptr, value }
            | Op::AtomicOr  { ptr, value }
            | Op::AtomicXor { ptr, value }
            | Op::AtomicSMin { ptr, value }
            | Op::AtomicSMax { ptr, value }
            | Op::AtomicUMin { ptr, value }
            | Op::AtomicUMax { ptr, value }
            | Op::AtomicExchange { ptr, value } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Atomic op without result".into()))?;
                let (x_base, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, ptr, &mut pointers, func.stage)?;
                if base_off < 0 {
                    return Err(BackendError::Unsupported(format!(
                        "Atomic ptr offset {base_off} negative")));
                }
                let w_value = *ints.get(&value.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Atomic value {:?} not in ints", value.id)))?;
                // LSE atomic instructions take a register-only
                // address operand: build Xn = x_base + base_off
                // (or just reuse x_base when base_off==0).
                let addr_x = if base_off == 0 {
                    x_base
                } else {
                    // Use Xreg(9) as scratch -- same scratch
                    // ConstFloat materialisation uses.  Lifetime
                    // is the single LSE instruction below; no
                    // overlap with W9-as-int-scratch since
                    // atomic codegen here never touches the int
                    // arithmetic path.
                    let scratch_x = asm::Xreg(9);
                    materialise_u32_into_w(&mut a, asm::Wreg(9), base_off as u32);
                    a.emit(asm::add_x(scratch_x, x_base, scratch_x));
                    scratch_x
                };
                let w_old = int_pool.alloc(result.id)?;
                // For AtomicAnd we need to invert the operand
                // because ARM's LDCLR does *X &= ~Rs.  Use the
                // scratch W9 as the inverted value holder.
                // Note: when base_off != 0 above we've already
                // overwritten W9 with the offset and then used
                // it; the add_x above sets X9 (the same reg).
                // For And we still need W9 for the inversion --
                // safe because by this point the address is in
                // X9 (used only by the LDCLR below); but we'd
                // overwrite X9 if we MVN to it.  Use a
                // different temporary: the W_value source then
                // MVN through W9 only if there's no conflict.
                // Simpler: do MVN before computing addr_x so
                // the scratch is reusable.  But for clarity,
                // when AND requires inversion AND base_off!=0,
                // we just bail to load-op-store (rare case).
                let is_and = matches!(&inst.op, Op::AtomicAnd { .. });
                if is_and && base_off != 0 {
                    // Conflict: scratch W9 is taken by address.
                    // Fall back to non-atomic seq (legacy path).
                    let w_new = asm::Wreg(9);
                    a.emit(asm::ldr_w_offset(w_old, x_base, base_off as u16));
                    a.emit(asm::and_w(w_new, w_old, w_value));
                    a.emit(asm::str_w_offset(w_new, x_base, base_off as u16));
                    ints.insert(result.id, w_old);
                    continue;
                }
                match &inst.op {
                    Op::AtomicIAdd { .. } =>
                        a.emit(asm::ldaddal_w(w_value, w_old, addr_x)),
                    Op::AtomicOr   { .. } =>
                        a.emit(asm::ldsetal_w(w_value, w_old, addr_x)),
                    Op::AtomicXor  { .. } =>
                        a.emit(asm::ldeoral_w(w_value, w_old, addr_x)),
                    Op::AtomicAnd  { .. } => {
                        // LDCLR does &= ~Rs; we want &= Rs, so
                        // invert first into W9 (the scratch).
                        let inv = asm::Wreg(9);
                        a.emit(asm::mvn_w(inv, w_value));
                        a.emit(asm::ldclral_w(inv, w_old, addr_x));
                    }
                    Op::AtomicSMin { .. } =>
                        a.emit(asm::ldsminal_w(w_value, w_old, addr_x)),
                    Op::AtomicSMax { .. } =>
                        a.emit(asm::ldsmaxal_w(w_value, w_old, addr_x)),
                    Op::AtomicUMin { .. } =>
                        a.emit(asm::lduminal_w(w_value, w_old, addr_x)),
                    Op::AtomicUMax { .. } =>
                        a.emit(asm::ldumaxal_w(w_value, w_old, addr_x)),
                    Op::AtomicExchange { .. } =>
                        a.emit(asm::swpal_w(w_value, w_old, addr_x)),
                    _ => unreachable!(),
                }
                ints.insert(result.id, w_old);
            }
            Op::PtrOffsetDynamic { base, index, stride } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "PtrOffsetDynamic without result".into()))?;
                let (x_base, base_off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, base, &mut pointers, func.stage)?;
                let w_index = *ints.get(&index.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "PtrOffsetDynamic index {:?} not in ints",
                        index.id)))?;
                // Stride must be a power of two for the
                // shift-and-add lowering; non-pow2 strides
                // would need a madd via a materialised
                // constant, queued as follow-up.
                if !stride.is_power_of_two() {
                    return Err(BackendError::Unsupported(format!(
                        "PtrOffsetDynamic stride {stride} is not a power of \
                         two (madd-based lowering not implemented yet)")));
                }
                let log2 = stride.trailing_zeros() as u8;
                if log2 > 63 {
                    return Err(BackendError::Unsupported(format!(
                        "PtrOffsetDynamic stride 2^{log2} too large")));
                }
                if spill_mode {
                    // Defer: record the recipe; consumers emit
                    // lsl+add into X9 at the access. Keep the
                    // index alive (and reloadable) until the
                    // pointer's LAST READER — pointers are
                    // deliberately absent from last_use (they
                    // held no registers pre-spilling), so scan
                    // for the last consumer explicitly. Without
                    // this the index expired at this inst and
                    // its stale `ints` entry aliased whatever
                    // value the register was re-handed to
                    // (observed: out_pixels stored AT the pixel
                    // VALUE as an address — SIGSEGV).
                    deferred_ptr.insert(result.id,
                        (x_base, base_off, index.id, log2));
                    let ptr_lu = flat_insts.iter().enumerate().rev()
                        .find(|(_, fi)| op_reads(&fi.op, result.id))
                        .map(|(j, _)| j)
                        .unwrap_or(i);
                    let e = last_use.entry(index.id).or_insert(ptr_lu);
                    *e = (*e).max(ptr_lu);
                    continue;
                }
                // Allocate a fresh int reg for the resulting
                // address.  Use the IntPool so the lifetime
                // tracks the result Value the normal way.
                let dst_w = int_pool.alloc(result.id)?;
                let dst_x = asm::Xreg(dst_w.0);
                let idx_x = asm::Xreg(w_index.0);
                if log2 == 0 {
                    // stride == 1: just add.
                    a.emit(asm::add_x(dst_x, x_base, idx_x));
                } else {
                    // stride == 2^log2: shift then add.
                    a.emit(asm::lsl_imm_x(dst_x, idx_x, log2));
                    a.emit(asm::add_x(dst_x, x_base, dst_x));
                }
                // Result pointer: base = dst_x, byte_off
                // carries the constant prefix the
                // AccessChain accumulated (the load/store
                // imm12 will fold it).
                pointers.insert(result.id, (dst_x, base_off));
            }
            Op::LoadBuiltin(kind) => {
                use atrium_spv_ir::BuiltinKind as BK;
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "LoadBuiltin without result".into()))?;
                // VS/FS scalar-integer builtins arrive in fixed AAPCS64
                // argument W-registers (ints in W0-W7, allocated separately
                // from the V-reg float args). Capture into the int pool here,
                // before the body reuses those registers:
                //   VS:  VertexIndex=W4, InstanceIndex=W5; Base*=const 0
                //   FS:  FrontFacing=W6, PrimitiveId=W7
                // (FragCoord — V0-V3 floats — is not yet handled here, so a
                // FragCoord FS still falls back to cranelift.)
                let vs_fs_int: Option<Option<u8>> = match (func.stage, *kind) {
                    (ShaderStage::Vertex, BK::VertexIndex) => Some(Some(4)),
                    (ShaderStage::Vertex, BK::InstanceIndex) => Some(Some(5)),
                    (ShaderStage::Vertex, BK::BaseVertex)
                    | (ShaderStage::Vertex, BK::BaseInstance) => Some(None),
                    (ShaderStage::Fragment, BK::FrontFacing) => Some(Some(6)),
                    (ShaderStage::Fragment, BK::PrimitiveId) => Some(Some(7)),
                    _ => None,
                };
                let bk_key: u8 = match kind {
                    BK::WorkgroupId => 0,
                    BK::LocalInvocationId => 1,
                    BK::GlobalInvocationId => 2,
                    BK::WorkgroupSize => 3,
                    _ => 255,
                };
                if bk_key != 255 {
                    if let Some(lanes) = builtin_lane_cache.get(&bk_key) {
                        vectors.insert(result.id, lanes.clone());
                        continue;
                    }
                }
                let run_compute_match = if let Some(src) = vs_fs_int {
                    let w = int_pool.alloc(result.id)?;
                    match src {
                        Some(reg) => a.emit(asm::mov_w(w, asm::Wreg(reg))),
                        None => a.emit(asm::movz_w(w, 0, 0)), // Base* = 0
                    }
                    ints.insert(result.id, w);
                    false
                } else if !matches!(func.stage, ShaderStage::Compute) {
                    return Err(BackendError::Unsupported(format!(
                        "LoadBuiltin({kind:?}) not supported in bespoke for \
                         stage {:?}", func.stage)));
                } else {
                    true
                };
                // Materialise uvec3 lanes via int W-regs in
                // the int_pool.  Source registers per the
                // Compute AAPCS64 sig:
                //   WorkgroupId       -> W3, W4, W5
                //   LocalInvocationId -> W6, W7, [SP+0]
                //   GlobalInvocationId -> wg[i]*LocalSize[i] + lid[i]
                let load_lane_from_w = |a: &mut asm::Asm,
                                        ints: &mut HashMap<ValueId, asm::Wreg>,
                                        int_pool: &mut IntPool,
                                        next_synth_id: &mut u32,
                                        src_w: u8|
                    -> Result<Value, BackendError>
                {
                    let synth = ValueId(*next_synth_id);
                    *next_synth_id += 1;
                    let w = int_pool.alloc(synth)?;
                    a.emit(asm::mov_w(w, asm::Wreg(src_w)));
                    ints.insert(synth, w);
                    Ok(Value { id: synth, ty: Type::U32 })
                };
                // Load lid.z (the 9th AAPCS64 arg, [SP+0] at
                // function entry).  Records the patch site
                // so the actual stack offset can be filled in
                // once we know the prologue's frame shift.
                let load_lid_z = |a: &mut asm::Asm,
                                   ints: &mut HashMap<ValueId, asm::Wreg>,
                                   int_pool: &mut IntPool,
                                   next_synth_id: &mut u32,
                                   patches: &mut Vec<(usize, asm::Wreg)>|
                    -> Result<Value, BackendError>
                {
                    let synth = ValueId(*next_synth_id);
                    *next_synth_id += 1;
                    let w = int_pool.alloc(synth)?;
                    let off = a.len();
                    // Placeholder offset 0 -- patched at end
                    // of emit_function to (frame_bytes >> 0).
                    a.emit(asm::ldr_w_offset(w, asm::Xreg(31), 0));
                    patches.push((off, w));
                    ints.insert(synth, w);
                    Ok(Value { id: synth, ty: Type::U32 })
                };
                if run_compute_match {
                match kind {
                    BK::WorkgroupId => {
                        let lx = load_lane_from_w(
                            &mut a, &mut ints, &mut int_pool, &mut next_synth_id, 3)?;
                        let ly = load_lane_from_w(
                            &mut a, &mut ints, &mut int_pool, &mut next_synth_id, 4)?;
                        let lz = load_lane_from_w(
                            &mut a, &mut ints, &mut int_pool, &mut next_synth_id, 5)?;
                        vectors.insert(result.id, vec![lx, ly, lz]);
                    }
                    BK::LocalInvocationId => {
                        let lx = load_lane_from_w(
                            &mut a, &mut ints, &mut int_pool, &mut next_synth_id, 6)?;
                        let ly = load_lane_from_w(
                            &mut a, &mut ints, &mut int_pool, &mut next_synth_id, 7)?;
                        // lid.z is the 9th AAPCS64 arg --
                        // [SP+0] at function entry, but
                        // shifts up by frame_bytes if the
                        // prologue's stp_x_pre / stp_d_pre
                        // pushed SP down for callee-saved
                        // saves.  Defer the offset to a
                        // post-body patch.
                        let lz = load_lid_z(
                            &mut a, &mut ints, &mut int_pool,
                            &mut next_synth_id, &mut lid_z_load_patches)?;
                        vectors.insert(result.id, vec![lx, ly, lz]);
                    }
                    BK::GlobalInvocationId => {
                        // GID[i] = WorkgroupID[i] * LocalSize[i] + LocalInvocationID[i].
                        // LocalSize comes from func.local_size (the SPIR-V
                        // OpExecutionMode literal); default (1,1,1) folds
                        // the multiply away.  Lane i=2 reads lid.z from
                        // the stack slot at [SP+0].
                        let ls = func.local_size.unwrap_or((1, 1, 1));
                        let ls_arr = [ls.0, ls.1, ls.2];
                        let mut lanes = Vec::with_capacity(3);
                        for i in 0..3 {
                            let synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_dst = int_pool.alloc(synth)?;
                            let w_wg = asm::Wreg(3 + i as u8);

                            // Load lid[i] into a scratch W-reg.
                            // Lanes 0,1 live in W6/W7; lane 2
                            // lives at [SP+0] (the 9th AAPCS64
                            // arg).
                            let scratch_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_lid = int_pool.alloc(scratch_synth)?;
                            if i < 2 {
                                a.emit(asm::mov_w(w_lid, asm::Wreg(6 + i as u8)));
                            } else {
                                // lid.z stack load -- record
                                // a patch site so the offset
                                // gets filled in after the
                                // prologue's frame size is
                                // known.
                                let off = a.len();
                                a.emit(asm::ldr_w_offset(w_lid, asm::Xreg(31), 0));
                                lid_z_load_patches.push((off, w_lid));
                            }

                            if ls_arr[i] == 1 {
                                // GID[i] = WG[i] + LID[i].
                                a.emit(asm::add_w(w_dst, w_wg, w_lid));
                            } else {
                                // scaled = WG[i] * LocalSize[i]
                                // GID[i] = scaled + LID[i]
                                let scaled_synth = ValueId(next_synth_id);
                                next_synth_id += 1;
                                let w_scaled = int_pool.alloc(scaled_synth)?;

                                // Materialise LocalSize[i] as
                                // a u32 constant in a temp W-reg
                                // (movz + optional movk for >16 bits).
                                let ls_synth = ValueId(next_synth_id);
                                next_synth_id += 1;
                                let w_ls = int_pool.alloc(ls_synth)?;
                                let lo = (ls_arr[i] & 0xFFFF) as u16;
                                let hi = ((ls_arr[i] >> 16) & 0xFFFF) as u16;
                                a.emit(asm::movz_w(w_ls, lo, 0));
                                if hi != 0 {
                                    a.emit(asm::movk_w(w_ls, hi, 16));
                                }
                                a.emit(asm::mul_w(w_scaled, w_wg, w_ls));
                                a.emit(asm::add_w(w_dst, w_scaled, w_lid));
                                // The LocalSize constant and the
                                // intermediate scaled WG product
                                // are both dead the instant the
                                // add reads them.  Manually
                                // return their W-regs to the
                                // pool -- their synth ValueIds
                                // aren't in the IR's last_use
                                // map, so the linear-scan expire
                                // pass would never reclaim them.
                                int_pool.free(w_ls);
                                int_pool.free(w_scaled);
                            }
                            // Same reasoning for w_lid: a
                            // scratch consumed by the final
                            // add_w.  Free it back to the pool.
                            int_pool.free(w_lid);

                            ints.insert(synth, w_dst);
                            lanes.push(Value { id: synth, ty: Type::U32 });
                        }
                        vectors.insert(result.id, lanes);
                    }
                    BK::WorkgroupSize => {
                        // uvec3 of the SPIR-V LocalSize
                        // literal -- a compile-time constant.
                        // Materialise each lane as a u32 in
                        // its own W-reg, mirroring how the
                        // other uvec3 builtins land in
                        // `vectors`.
                        let ls = func.local_size.unwrap_or((1, 1, 1));
                        let ls_arr = [ls.0, ls.1, ls.2];
                        let mut lanes = Vec::with_capacity(3);
                        for v in ls_arr {
                            let synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w = int_pool.alloc(synth)?;
                            a.emit(asm::movz_w(w, (v & 0xFFFF) as u16, 0));
                            if (v >> 16) != 0 {
                                a.emit(asm::movk_w(
                                    w, ((v >> 16) & 0xFFFF) as u16, 16));
                            }
                            ints.insert(synth, w);
                            lanes.push(Value { id: synth, ty: Type::U32 });
                        }
                        vectors.insert(result.id, lanes);
                    }
                    BK::LocalInvocationIndex => {
                        // index = lz * (sx*sy) + ly * sx + lx
                        // Folds:
                        //  - sx*sy == 1: index = lx
                        //  - sy == 1 && sx*sy != 1: index = lz*sxsy + lx
                        //    (no ly term since LocalSize.y=1 -> ly=0)
                        //  - sx == 1: similar simplifications
                        //  General: full 5-op sequence.
                        let ls = func.local_size.unwrap_or((1, 1, 1));
                        let sx = ls.0;
                        let sy = ls.1;
                        let sxsy = sx.saturating_mul(sy);
                        let w_dst = int_pool.alloc(result.id)?;
                        if sx == 1 && sy == 1 {
                            // index = lz only (since lx=0,
                            // ly=0).  Materialise lz into w_dst
                            // directly.  Use the same lid.z
                            // load patch as LocalInvocationId.
                            let off = a.len();
                            a.emit(asm::ldr_w_offset(w_dst, asm::Xreg(31), 0));
                            lid_z_load_patches.push((off, w_dst));
                        } else if sxsy == 1 {
                            // Unreachable since sxsy==1 implies
                            // sx==1 && sy==1 (handled above) --
                            // kept defensively.
                            a.emit(asm::mov_w(w_dst, asm::Wreg(6)));
                        } else if sy == 1 {
                            // index = lz * sx + lx
                            // (no ly term since ly is always 0).
                            // Load lz, multiply by sx, add lx.
                            let lz_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_lz = int_pool.alloc(lz_synth)?;
                            let off = a.len();
                            a.emit(asm::ldr_w_offset(w_lz, asm::Xreg(31), 0));
                            lid_z_load_patches.push((off, w_lz));
                            // Materialise sx into a scratch.
                            let sx_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_sx = int_pool.alloc(sx_synth)?;
                            let lo = (sx & 0xFFFF) as u16;
                            let hi = ((sx >> 16) & 0xFFFF) as u16;
                            a.emit(asm::movz_w(w_sx, lo, 0));
                            if hi != 0 { a.emit(asm::movk_w(w_sx, hi, 16)); }
                            // w_dst = w_lz * w_sx
                            a.emit(asm::mul_w(w_dst, w_lz, w_sx));
                            // w_dst += W6 (lx)
                            a.emit(asm::add_w(w_dst, w_dst, asm::Wreg(6)));
                            int_pool.free(w_lz);
                            int_pool.free(w_sx);
                        } else {
                            // General case: lz*sxsy + ly*sx + lx.
                            let lz_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_lz = int_pool.alloc(lz_synth)?;
                            let off = a.len();
                            a.emit(asm::ldr_w_offset(w_lz, asm::Xreg(31), 0));
                            lid_z_load_patches.push((off, w_lz));
                            // Materialise sxsy.
                            let sxsy_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_sxsy = int_pool.alloc(sxsy_synth)?;
                            a.emit(asm::movz_w(w_sxsy, (sxsy & 0xFFFF) as u16, 0));
                            if (sxsy >> 16) != 0 {
                                a.emit(asm::movk_w(
                                    w_sxsy, ((sxsy >> 16) & 0xFFFF) as u16, 16));
                            }
                            // w_dst = lz * sxsy
                            a.emit(asm::mul_w(w_dst, w_lz, w_sxsy));
                            int_pool.free(w_lz);
                            int_pool.free(w_sxsy);
                            // Materialise sx.
                            let sx_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_sx = int_pool.alloc(sx_synth)?;
                            a.emit(asm::movz_w(w_sx, (sx & 0xFFFF) as u16, 0));
                            if (sx >> 16) != 0 {
                                a.emit(asm::movk_w(
                                    w_sx, ((sx >> 16) & 0xFFFF) as u16, 16));
                            }
                            // ly_scaled = ly * sx
                            let lys_synth = ValueId(next_synth_id);
                            next_synth_id += 1;
                            let w_lys = int_pool.alloc(lys_synth)?;
                            a.emit(asm::mul_w(w_lys, asm::Wreg(7), w_sx));
                            int_pool.free(w_sx);
                            // w_dst += ly_scaled
                            a.emit(asm::add_w(w_dst, w_dst, w_lys));
                            int_pool.free(w_lys);
                            // w_dst += lx (W6)
                            a.emit(asm::add_w(w_dst, w_dst, asm::Wreg(6)));
                        }
                        ints.insert(result.id, w_dst);
                    }
                    other => return Err(BackendError::Unsupported(format!(
                        "LoadBuiltin({other:?}) not supported in compute \
                         bespoke path"))),
                }
                } // run_compute_match
                if bk_key != 255 {
                    if let Some(lanes) = vectors.get(&result.id) {
                        builtin_lane_cache.insert(bk_key, lanes.clone());
                    }
                }
            }
            Op::Load(ptr) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("Load without result".into()))?;
                let (param, off) =
                    resolve_ptr_spill(&mut a, &deferred_ptr, &ints, &phi_const_val, &inst.op, ptr, &mut pointers, func.stage)?;
                let pointee = match &ptr.ty {
                    Type::Pointer(_, inner) => (**inner).clone(),
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load ptr {other:?} is not a Pointer type"))),
                };
                match &pointee {
                    Type::F32 => {
                        let v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
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
                        // into vectors[result.id]. Lane class
                        // follows the ELEMENT type: f32 lanes
                        // ride S-regs; i32/u32 lanes (e.g. a
                        // push-constant uint4) ride W-regs so
                        // int consumers (compares/arith via
                        // VectorExtract) resolve them — the
                        // same poison the Cranelift backend
                        // had with F32-hardcoded lanes.
                        let elem = match &pointee {
                            Type::Vec2(e) | Type::Vec3(e)
                            | Type::Vec4(e) => *e,
                            _ => unreachable!(),
                        };
                        let int_lanes = matches!(
                            elem,
                            atrium_spv_ir::VecElement::I32
                                | atrium_spv_ir::VecElement::U32);
                        if int_lanes {
                            int_vec_ptr.insert(result.id, (param, off));
                        } else {
                            let mut lanes = Vec::with_capacity(lane_count);
                            for lane_i in 0..lane_count {
                                let lane_off = off.saturating_add((lane_i * 4) as i32);
                                // Synthetic ValueId from the
                                // dedicated high-range counter
                                // — collision-free with any IR
                                // ValueId the frontend assigns.
                                let synthetic_id = ValueId(next_synth_id);
                                next_synth_id += 1;
                                let v = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synthetic_id)?;
                                emit_load_f32_offset(&mut a, w_tmp, param, lane_off, v)?;
                                scalars.insert(synthetic_id, v);
                                lanes.push(Value {
                                    id: synthetic_id, ty: Type::F32,
                                });
                            }
                            vectors.insert(result.id, lanes);
                        }
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
                let w_cond = match bools.get(&cond.id) {
                    Some(w) => *w,
                    None => match bool_in_v.get(&cond.id) {
                        // V-overflowed bool: materialise into
                        // the W9 scratch (consumed immediately
                        // by the cmp below).
                        Some(v) => {
                            a.emit(asm::fmov_w_from_s(w_tmp, *v));
                            w_tmp
                        }
                        None => match bool_in_slot.get(&cond.id) {
                            Some(slot) => {
                                a.emit(asm::ldr_w_offset(w_tmp,
                                    asm::Xreg(31), (*slot as u16) * 8));
                                w_tmp
                            }
                            None => return Err(BackendError::Internal(
                                format!("Select cond {:?} not in \
                                         bools/bool_in_v/slot", cond.id))),
                        },
                    },
                };
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
                        let s_d = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, result.id)?;
                        a.emit(asm::fcsel_s(s_d, s_t, s_f, asm::Cond::Ne));
                        scalars.insert(result.id, s_d);
                    }
                    Type::I32 | Type::U32 => {
                        let w_t = *ints.get(&t_val.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "int Select t {:?} not in ints", t_val.id)))?;
                        let w_f = *ints.get(&f_val.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "int Select f {:?} not in ints", f_val.id)))?;
                        let w_d = alloc_int_w(&mut int_pool, result.id)?;
                        a.emit(asm::csel_w(w_d, w_t, w_f, asm::Cond::Ne));
                        ints.insert(result.id, w_d);
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
                            let sd = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
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
                    if spill_mode {
                        // Reload spilled phi-move sources first
                        // (the move emission reads regs).
                        let needed: Vec<ValueId> = moves.iter()
                            .flat_map(|(dest, src_id)| match dest {
                                PhiDest::Float(_)
                                | PhiDest::FloatSpill(_) => vec![*src_id],
                                PhiDest::Vec(_) => vectors
                                    .get(src_id)
                                    .map(|ls| ls.iter()
                                        .map(|l| l.id).collect())
                                    .unwrap_or_default(),
                                _ => Vec::new(),
                            })
                            .filter(|id| spilled.contains(id))
                            .collect();
                        // Int/Bool move sources from the W side.
                        let needed_w: Vec<ValueId> = moves.iter()
                            .filter_map(|(dest, src_id)| match dest {
                                PhiDest::Int(_)
                                | PhiDest::Bool(_)
                                | PhiDest::BoolV(_) => Some(*src_id),
                                _ => None,
                            })
                            .filter(|id| spilled_w.contains(id))
                            .collect();
                        for id in needed_w {
                            let slot = *spill_slot.get(&id)
                                .ok_or_else(|| BackendError::Internal(
                                    format!("int reload of {id:?} \
                                             without slot")))?;
                            if int_pool.free.is_empty()
                                && !wevict_one(&mut int_pool,
                                    &mut ints, &mut spilled_w,
                                    &spill_slot, &vectors, &deferred_ptr, &last_use,
                                    &phi_pinned, &inst.op)
                            {
                                return Err(BackendError::Unsupported(
                                    "W pressure with no spillable \
                                     victim".into()));
                            }
                            let w = int_pool.alloc(id)?;
                            a.emit(asm::ldr_w_offset(
                                w, asm::Xreg(31), (slot as u16) * 8));
                            ints.insert(id, w);
                            spilled_w.remove(&id);
                        }
                        for id in needed {
                            vread_impl(&mut a, &mut free_pool,
                                &mut owners, &mut used_callee_saved_v,
                                &mut scalars, &mut spilled,
                                &spill_slot, &vectors, &last_use,
                                &phi_pinned, spill_mode, &inst.op,
                                id)?;
                        }
                    }
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
                                if scalars.get(src_id).is_none() {
                                    if let Some(c) = phi_const_val.get(src_id) {
                                        materialise_u32_into_w(
                                            &mut a, w_tmp, *c);
                                        a.emit(asm::fmov_s_from_w(
                                            *dv, w_tmp));
                                        continue;
                                    }
                                }
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
                                // Peephole: drop identity move
                                // (same src + dst reg).  These
                                // can survive the coalesce pass
                                // when a Phi arm is force-
                                // allocated into the same V-reg
                                // the source already lives in.
                                if dv.0 != src.0 {
                                    a.emit(asm::mov_v_16b(*dv, src));
                                }
                            }
                            PhiDest::Int(dw) => {
                                if let Some(src) = ints.get(src_id) {
                                    if dw.0 != src.0 {
                                        a.emit(asm::mov_w(*dw, *src));
                                    }
                                } else if let Some(c) =
                                    phi_const_val.get(src_id)
                                {
                                    materialise_u32_into_w(&mut a, *dw, *c);
                                } else {
                                    return Err(BackendError::Internal(
                                        format!("Phi int source {:?} not \
                                                 in ints", src_id)));
                                }
                            }
                            PhiDest::Bool(dw) => {
                                if let Some(src) = bools.get(src_id)
                                    .or_else(|| ints.get(src_id))
                                {
                                    if dw.0 != src.0 {
                                        a.emit(asm::mov_w(*dw, *src));
                                    }
                                } else if let Some(sv) =
                                    bool_in_v.get(src_id)
                                {
                                    a.emit(asm::fmov_w_from_s(*dw, *sv));
                                } else if let Some(ss) =
                                    bool_in_slot.get(src_id)
                                {
                                    a.emit(asm::ldr_w_offset(*dw,
                                        asm::Xreg(31), (*ss as u16) * 8));
                                } else if let Some(c) =
                                    phi_const_val.get(src_id)
                                {
                                    materialise_u32_into_w(&mut a, *dw, *c);
                                } else {
                                    return Err(BackendError::Internal(
                                        format!("Phi bool source {:?} not \
                                                 in bools/ints/bool_in_v",
                                                src_id)));
                                }
                            }
                            PhiDest::FloatSpill(slot) => {
                                if scalars.get(src_id).is_none() {
                                    if let Some(c) = phi_const_val.get(src_id) {
                                        materialise_u32_into_w(
                                            &mut a, w_tmp, *c);
                                        a.emit(asm::str_w_offset(w_tmp,
                                            asm::Xreg(31),
                                            (*slot as u16) * 8));
                                        continue;
                                    }
                                }
                                let src = *scalars.get(src_id).ok_or_else(||
                                    BackendError::Internal(format!(
                                        "Phi f32-spill source {:?} not in \
                                         scalars", src_id)))?;
                                a.emit(asm::str_d_offset(
                                    src, asm::Xreg(31), (*slot as u16) * 8));
                            }
                            PhiDest::BoolSpill(slot) => {
                                if let Some(sw) = bools.get(src_id)
                                    .or_else(|| ints.get(src_id))
                                {
                                    a.emit(asm::str_w_offset(*sw,
                                        asm::Xreg(31), (*slot as u16) * 8));
                                } else if let Some(sv) =
                                    bool_in_v.get(src_id)
                                {
                                    a.emit(asm::fmov_w_from_s(w_tmp, *sv));
                                    a.emit(asm::str_w_offset(w_tmp,
                                        asm::Xreg(31), (*slot as u16) * 8));
                                } else if let Some(ss) =
                                    bool_in_slot.get(src_id)
                                {
                                    a.emit(asm::ldr_w_offset(w_tmp,
                                        asm::Xreg(31), (*ss as u16) * 8));
                                    a.emit(asm::str_w_offset(w_tmp,
                                        asm::Xreg(31), (*slot as u16) * 8));
                                } else if let Some(c) =
                                    phi_const_val.get(src_id)
                                {
                                    materialise_u32_into_w(&mut a, w_tmp, *c);
                                    a.emit(asm::str_w_offset(w_tmp,
                                        asm::Xreg(31), (*slot as u16) * 8));
                                } else {
                                    return Err(BackendError::Internal(
                                        format!("Phi bool-spill source \
                                                 {:?} unresolved", src_id)));
                                }
                            }
                            PhiDest::BoolV(dv) => {
                                if let Some(sw) = bools.get(src_id)
                                    .or_else(|| ints.get(src_id))
                                {
                                    a.emit(asm::fmov_s_from_w(*dv, *sw));
                                } else if let Some(sv) =
                                    bool_in_v.get(src_id)
                                {
                                    if dv.0 != sv.0 {
                                        a.emit(asm::fmov_s(*dv, *sv));
                                    }
                                } else if let Some(ss) =
                                    bool_in_slot.get(src_id)
                                {
                                    a.emit(asm::ldr_w_offset(w_tmp,
                                        asm::Xreg(31), (*ss as u16) * 8));
                                    a.emit(asm::fmov_s_from_w(*dv, w_tmp));
                                } else if let Some(c) =
                                    phi_const_val.get(src_id)
                                {
                                    materialise_u32_into_w(&mut a, w_tmp, *c);
                                    a.emit(asm::fmov_s_from_w(*dv, w_tmp));
                                } else {
                                    return Err(BackendError::Internal(
                                        format!("Phi boolV source {:?} not \
                                                 in bools/ints/bool_in_v",
                                                src_id)));
                                }
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
                                if dq.0 != src.0 {
                                    a.emit(asm::mov_v_16b(*dq, src));
                                }
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
                                    if dv.0 != src.0 {
                                        a.emit(asm::mov_v_16b(*dv, src));
                                    }
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
                        let w_bool = match bools.get(&cond.id) {
                            Some(w) => *w,
                            None => match bool_in_v.get(&cond.id) {
                                Some(v) => {
                                    a.emit(asm::fmov_w_from_s(w_tmp, *v));
                                    w_tmp
                                }
                                None => match bool_in_slot.get(&cond.id) {
                                    Some(slot) => {
                                        a.emit(asm::ldr_w_offset(w_tmp,
                                            asm::Xreg(31),
                                            (*slot as u16) * 8));
                                        w_tmp
                                    }
                                    None => return Err(
                                        BackendError::Unsupported(format!(
                                            "BranchCond cond {:?} is not \
                                             a known Bool W-reg", cond.id))),
                                },
                            },
                        };
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
            //
            // ImageSampleExplicitLod shares this arm but
            // routes through `atrium_tex_sample_2d_lod`
            // (helper @ #16) and passes the LOD scalar in V2.
            // Mip selection happens inside the helper via
            // `TexDesc.mip_descs[lod]` (Arc 29).
            // Shadow / depth-comparison sample.  The bespoke
            // emitter doesn't implement the dref call shape
            // (extra f32 arg + scalar result); return
            // Unsupported so the compiler falls back to the
            // cranelift backend, which lowers it through the
            // `atrium_tex_sample_2d_dref` helper (#64).
            Op::ImageSampleDref { .. } => {
                return Err(BackendError::Unsupported(
                    "ImageSampleDref (shadow sample) -- handled by cranelift".into()));
            }
            Op::Derivative { .. } => {
                // Screen-space derivatives need the 2x2-quad
                // re-execution + helper call; the bespoke
                // emitter doesn't implement the indirect call
                // shape, so fall back to cranelift.
                return Err(BackendError::Unsupported(
                    "Derivative (dFdx/dFdy/fwidth) -- handled by cranelift".into()));
            }
            Op::ImageSampleImplicitLod { sampled_image, coord }
            | Op::ImageSampleExplicitLod { sampled_image, coord, .. } => {
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
                // Three-lane coord paths:
                //   samplerCube         -> sample_cube (#32)
                //   sampler2DArray (2D) -> sample_2d_array (#24)
                // Disambiguated by sampled_image.ty's
                // ImageDimensionality (Cube vs Dim2D).
                let is_cube = matches!(sampled_image.ty,
                    Type::SampledImage(atrium_spv_ir::ImageDimensionality::Cube));
                let is_array = !is_cube && coord_lanes.len() >= 3;
                let third_v: Option<asm::Vreg> =
                    if is_cube || is_array {
                        Some(*scalars.get(&coord_lanes[2].id).ok_or_else(||
                            BackendError::Internal(format!(
                                "ImageSample 3-lane coord lane 2 {:?} \
                                 not in scalars", coord_lanes[2].id)))?)
                    } else { None };
                let layer_v = third_v;  // legacy name kept for the
                                        // helper_off match below
                let _ = layer_v;

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
                    let r = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
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
                let v2 = asm::Vreg(2); // third coord / LOD / layer slot
                let v3 = asm::Vreg(3); // LOD slot for array+lod / cube+lod
                let v4 = asm::Vreg(4); // u  hold across parallel copy
                let v5 = asm::Vreg(5); // v  hold across parallel copy
                let v6 = asm::Vreg(6); // third-coord hold (array+lod / cube+lod)
                let v7 = asm::Vreg(7); // lod hold (array+lod / cube+lod)
                let lr = asm::Xreg(30);
                // Helper-table layout:
                //   #0  sample_2d            (ImplicitLod, 2D)
                //   #8  fetch_2d
                //   #16 sample_2d_lod        (ExplicitLod, 2D)
                //   #24 sample_2d_array      (ImplicitLod, 2DArray)
                //   #32 sample_cube          (ImplicitLod, Cube)
                //   #40 gather_2d
                //   #48 sample_2d_array_lod  (ExplicitLod, 2DArray)
                //   #56 sample_cube_lod      (ExplicitLod, Cube)
                let explicit_lod_v: Option<asm::Vreg> = match &inst.op {
                    Op::ImageSampleExplicitLod { lod, .. } => {
                        let lv = *scalars.get(&lod.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "ImageSampleExplicitLod lod {:?} not \
                                 in scalars", lod.id)))?;
                        Some(lv)
                    }
                    _ => None,
                };
                let helper_off: u16 = match (
                    is_cube, is_array, explicit_lod_v.is_some(),
                ) {
                    (true,  _, true)  => 56, // cube  + lod
                    (_, true,  true)  => 48, // array + lod
                    (true,  _, false) => 32, // cube
                    (_, true,  false) => 24, // array
                    (_,    _, true)   => 16, // 2D    + lod
                    _                 => 0,  // 2D
                };
                // For single-extra paths (one of {ExplicitLod, Array,
                // Cube}), the third float arg lands in V2.  For the
                // two-extra paths (Array+Lod, Cube+Lod), V2 carries
                // the third coord and V3 carries the LOD.
                let two_extra = explicit_lod_v.is_some() && (is_array || is_cube);
                let extra_v: Option<asm::Vreg> =
                    if two_extra { third_v } else { explicit_lod_v.or(third_v) };
                let desc_off: u16 = 80 + (binding as u16) * 16;

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

                // Load descriptor pointers + helper.  For
                // ExplicitLod, `helper_off` is #16, pointing
                // at `atrium_tex_sample_2d_lod`.
                a.emit(asm::ldr_x_offset(x9, x1, helper_off));
                a.emit(asm::ldr_x_offset(x10, x1, desc_off));
                a.emit(asm::ldr_x_offset(x11, x1, desc_off + 8));

                // Parallel-copy of (u, v[, third[, lod]]) into
                // (V0, V1[, V2[, V3]]).  Sources may live in
                // any V-reg (including the dest set), so we
                // first hold every source in V4..V7 (always
                // safe scratch: caller-saved and either not
                // owned, or already spilled by the dump above)
                // and then assign down.
                a.emit(asm::mov_v_16b(v4, u_v));
                a.emit(asm::mov_v_16b(v5, v_v));
                if two_extra {
                    a.emit(asm::mov_v_16b(v6, third_v.unwrap()));
                    a.emit(asm::mov_v_16b(v7, explicit_lod_v.unwrap()));
                } else if let Some(ev) = extra_v {
                    a.emit(asm::mov_v_16b(v6, ev));
                }
                a.emit(asm::mov_v_16b(v0, v4));
                a.emit(asm::mov_v_16b(v1, v5));
                if two_extra {
                    a.emit(asm::mov_v_16b(v2, v6));
                    a.emit(asm::mov_v_16b(v3, v7));
                } else if extra_v.is_some() {
                    a.emit(asm::mov_v_16b(v2, v6));
                }

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
            // OpImageGather (Arc 32) -- 2x2 footprint fetch
            // of one channel.  Helper @ #40
            // (`atrium_tex_gather_2d`); signature is
            // (tex, samp, u, v, component:i32, out_rgba).
            // AAPCS64 register schedule:
            //   X0=tex, X1=samp, V0=u, V1=v, W2=component,
            //   X3=out_rgba slot.
            // The output pointer shifts from X2 (sample) to
            // X3 because `component` consumes the W2 int
            // slot; everything else (V-reg spill window,
            // 4-lane result load) follows the sample arm.
            Op::ImageGather { sampled_image, coord, component } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageGather without result".into()))?;
                let (_, binding) = image_handles.get(&sampled_image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageGather sampled_image {:?} not an \
                         ImageHandle", sampled_image.id)))?;
                let coord_lanes = vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageGather coord {:?} not a vector",
                        coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "ImageGather 2D coord must have ≥2 lanes, \
                         got {}", coord_lanes.len())));
                }
                let u_v = *scalars.get(&coord_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageGather coord lane 0 {:?} not in scalars",
                        coord_lanes[0].id)))?;
                let v_v = *scalars.get(&coord_lanes[1].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageGather coord lane 1 {:?} not in scalars",
                        coord_lanes[1].id)))?;
                let comp_w = *ints.get(&component.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageGather component {:?} not in ints",
                        component.id)))?;

                // Spill V-regs + allocate 4 result lanes,
                // same pattern as ImageSampleImplicitLod.
                let mut live_vregs: Vec<u8> = owners.keys().copied().collect();
                live_vregs.sort();
                let mut lane_regs: Vec<asm::Vreg> = Vec::with_capacity(4);
                let mut lane_vals: Vec<Value> = Vec::with_capacity(4);
                for _ in 0..4 {
                    let synth = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let r = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
                    scalars.insert(synth, r);
                    lane_regs.push(r);
                    lane_vals.push(Value { id: synth, ty: Type::F32 });
                }
                let sp = asm::Xreg(31);
                let x0 = asm::Xreg(0);
                let x1 = asm::Xreg(1);
                let x3 = asm::Xreg(3);
                let x9 = asm::Xreg(9);
                let x10 = asm::Xreg(10);
                let x11 = asm::Xreg(11);
                let v0 = asm::Vreg(0);
                let v1 = asm::Vreg(1);
                let v2 = asm::Vreg(2); // u/v parallel-copy temp
                let w2 = asm::Wreg(2);
                let lr = asm::Xreg(30);
                let desc_off: u16 = 80 + (binding as u16) * 16;
                let n_spill = live_vregs.len() as u16;
                let frame_bytes: u16 = 32 + n_spill * 16;
                a.emit(asm::sub_imm_x(sp, sp, frame_bytes));
                a.emit(asm::str_x_offset(x_out, sp, 16));
                a.emit(asm::str_x_offset(lr, sp, 24));
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::str_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }
                // Helper + descriptor pointer loads (gather_2d
                // helper @ #40).
                a.emit(asm::ldr_x_offset(x9, asm::Xreg(1), 40));
                a.emit(asm::ldr_x_offset(x10, asm::Xreg(1), desc_off));
                a.emit(asm::ldr_x_offset(x11, asm::Xreg(1), desc_off + 8));
                // u, v -> V0, V1 (parallel-safe via V2).
                a.emit(asm::mov_v_16b(v2, u_v));
                a.emit(asm::mov_v_16b(v1, v_v));
                a.emit(asm::mov_v_16b(v0, v2));
                // Tex + samp ptrs + component + rgba slot.
                a.emit(asm::mov_x(x0, x10));
                a.emit(asm::mov_x(x1, x11));
                a.emit(asm::mov_w(w2, comp_w));
                a.emit(asm::add_imm_x(x3, sp, 0));
                a.emit(asm::blr_x(x9));
                // Load 4 result lanes back.
                for (i, lane) in lane_regs.iter().enumerate() {
                    a.emit(asm::ldr_w_offset(w_tmp, sp, (i as u16) * 4));
                    a.emit(asm::fmov_s_from_w(*lane, w_tmp));
                }
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::ldr_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }
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
                    let r = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
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
                let desc_off: u16 = 80 + (binding as u16) * 16;

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
            // OpImageTexelPointer — produce a raw byte pointer
            // to a storage-image texel so a subsequent atomic
            // op can read-modify-write it in place.  No helper
            // call: the address is computed inline from the
            // X19-anchored ImageDesc (data @0, stride @16,
            // slice_bytes @28):
            //   texel_addr = data + z*slice_bytes
            //                     + y*stride_bytes + x*4
            // A 2-lane coord is a 2D image (z term skipped);
            // a 3-lane coord is an image3D.  The result Value
            // is registered in `pointers` as (Xreg, 0); the
            // LSE atomic arm then resolves it via
            // resolve_or_make_pointer with no relocation.
            Op::ImageTexelPointer { image, coord } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageTexelPointer without result".into()))?;
                let (_, binding) = image_handles.get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageTexelPointer image {:?} not an ImageHandle",
                        image.id)))?;
                let coord_lanes = vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageTexelPointer coord {:?} not a vector",
                        coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "ImageTexelPointer coord must have ≥2 lanes, \
                         got {}", coord_lanes.len())));
                }
                let x_w = *ints.get(&coord_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageTexelPointer coord lane 0 {:?} not in ints",
                        coord_lanes[0].id)))?;
                let y_w = *ints.get(&coord_lanes[1].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "ImageTexelPointer coord lane 1 {:?} not in ints",
                        coord_lanes[1].id)))?;
                // 3-lane coord -> image3D: also fold in
                // z*slice_bytes.
                let z_w = if coord_lanes.len() >= 3 {
                    Some(*ints.get(&coord_lanes[2].id).ok_or_else(||
                        BackendError::Internal(format!(
                            "ImageTexelPointer coord lane 2 {:?} not \
                             in ints", coord_lanes[2].id)))?)
                } else { None };
                // ImageDesc* lives at [X19, #32 + binding*8]
                // (the helper header is 32 B: 2D r/w + 3D r/w).
                let desc_off: u16 = IMG_TABLE_DESC_BASE_U16 + (binding as u16) * 8;
                let dst_w = int_pool.alloc(result.id)?;
                let dst_x = asm::Xreg(dst_w.0);
                let x9 = asm::Xreg(9);
                let w9 = asm::Wreg(9);
                let w10 = asm::Wreg(10);
                let x10 = asm::Xreg(10);
                // x9 = ImageDesc*
                a.emit(asm::ldr_x_offset(x9, asm::Xreg(19), desc_off));
                // dst = data pointer (ImageDesc.data @ #0)
                a.emit(asm::ldr_x_offset(dst_x, x9, 0));
                // image3D: dst += z * slice_bytes (field @28).
                if let Some(z_w) = z_w {
                    a.emit(asm::ldr_w_offset(w10, x9, 28));
                    a.emit(asm::mul_w(w10, z_w, w10));
                    a.emit(asm::add_x(dst_x, dst_x, x10));
                }
                // w10 = stride_bytes (ImageDesc.stride_bytes @ #16)
                a.emit(asm::ldr_w_offset(w10, x9, 16));
                // w10 = y * stride_bytes
                a.emit(asm::mul_w(w10, y_w, w10));
                // w9 = x * 4 (one rgba8/r32 texel is 4 bytes)
                a.emit(asm::lsl_imm_w(w9, x_w, 2));
                // w10 = y*stride + x*4 (32-bit op zero-extends X10)
                a.emit(asm::add_w(w10, w10, w9));
                // dst += offset
                a.emit(asm::add_x(dst_x, dst_x, x10));
                pointers.insert(result.id, (dst_x, 0));
            }
            // OpImageQuerySize — read width / height [/ depth]
            // off the ImageDesc at [X19, #32+B*8].  Returns a
            // uvec2 (image2D) or uvec3 (image3D); the lane
            // count comes from the result Type variant.
            Op::ImageQuerySize(image) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageQuerySize without result".into()))?;
                let lane_count = match &result.ty {
                    Type::Vec2(_) => 2,
                    Type::Vec3(_) => 3,
                    other => return Err(BackendError::Unsupported(format!(
                        "ImageQuerySize result must be vec2/vec3, got {other:?}"))),
                };
                let (_, binding) = image_handles.get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageQuerySize image {:?} not an ImageHandle",
                        image.id)))?;
                let desc_off: u16 = IMG_TABLE_DESC_BASE_U16 + (binding as u16) * 8;
                let x9 = asm::Xreg(9);
                a.emit(asm::ldr_x_offset(x9, asm::Xreg(19), desc_off));
                // ImageDesc field offsets: width @8, height @12,
                // depth @24.
                let field_offs: [u16; 3] = [8, 12, 24];
                let mut lanes: Vec<Value> = Vec::with_capacity(lane_count);
                for i in 0..lane_count {
                    let synth = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let w = int_pool.alloc(synth)?;
                    a.emit(asm::ldr_w_offset(w, x9, field_offs[i]));
                    ints.insert(synth, w);
                    lanes.push(Value { id: synth, ty: Type::U32 });
                }
                vectors.insert(result.id, lanes);
            }
            // OpImageQuerySizeLod on a sampled image -- reads
            // TexDesc.width/height off the X1-anchored
            // uniforms table.  LOD operand is captured for
            // liveness but ignored at codegen (Tier-2 v1 is
            // single-mip from the sampler's perspective).
            Op::SampledImageQuerySizeLod { image, lod: _ } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "SampledImageQuerySizeLod without result".into()))?;
                let lane_count = match &result.ty {
                    Type::Vec2(_) => 2,
                    Type::Vec3(_) => 3,
                    other => return Err(BackendError::Unsupported(format!(
                        "SampledImageQuerySizeLod result must be \
                         vec2/vec3, got {other:?}"))),
                };
                let (_, binding) = image_handles.get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "SampledImageQuerySizeLod image {:?} not an \
                         ImageHandle", image.id)))?;
                // Sampled-image descriptor table sits at X1
                // anchored, with slot pitch 16 (tex+samp) and
                // base offset UNIFORMS_DESC_BASE (= 64).
                let desc_off: u16 = 80 + (binding as u16) * 16;
                let x9 = asm::Xreg(9);
                a.emit(asm::ldr_x_offset(x9, asm::Xreg(1), desc_off));
                // TexDesc field offsets: width @8, height @12.
                let field_offs: [u16; 3] = [8, 12, 0];
                let mut lanes: Vec<Value> = Vec::with_capacity(lane_count);
                for i in 0..lane_count.min(2) {
                    let synth = ValueId(next_synth_id);
                    next_synth_id += 1;
                    let w = int_pool.alloc(synth)?;
                    a.emit(asm::ldr_w_offset(w, x9, field_offs[i]));
                    ints.insert(synth, w);
                    lanes.push(Value { id: synth, ty: Type::U32 });
                }
                vectors.insert(result.id, lanes);
            }
            // OpImageRead / OpImageWrite — storage-image
            // access in a compute shader.  v1-ABI call via
            // the X19-anchored image descriptor table:
            //   helper ptr   at [X19, #0]  (read_2d)
            //                  [X19, #8]  (write_2d)
            //                  [X19, #16] (read_3d)
            //                  [X19, #24] (write_3d)
            //                  [X19, #32] (read_2d_lod)
            //                  [X19, #40] (write_2d_lod)
            //                  [X19, #48] (read_3d_lod)
            //                  [X19, #56] (write_3d_lod)
            //   ImageDesc*   at [X19, #72 + binding*8]
            // 2D helper signature: (ImageDesc*, x, y, rgba*)
            //   -> X0=desc, W1=x, W2=y, X3=rgba stack slot.
            // 3D helper signature: (ImageDesc*, x, y, z, rgba*)
            //   -> X0=desc, W1=x, W2=y, W3=z, X4=rgba slot.
            // 2D Lod helper: (ImageDesc*, x, y, lod, rgba*)
            //   -> X0=desc, W1=x, W2=y, W3=lod, X4=rgba.
            // 3D Lod helper: (ImageDesc*, x, y, z, lod, rgba*)
            //   -> X0=desc, W1=x, W2=y, W3=z, W4=lod, X5=rgba.
            // The 2-vs-3 routing is by coord-lane count; the
            // Lod-vs-base routing is by op variant.
            // The call clobbers caller-saved regs, so live
            // V-regs and live caller-saved int W-regs (W13..
            // W17) are spilled across it; X19 (image table)
            // and X2 (SSBO out_buffer) are callee-preserved /
            // explicitly saved.
            Op::ImageRead { image, coord }
            | Op::ImageWrite { image, coord, .. }
            | Op::ImageReadLod { image, coord, .. }
            | Op::ImageWriteLod { image, coord, .. } => {
                let is_write = matches!(&inst.op,
                    Op::ImageWrite { .. } | Op::ImageWriteLod { .. });
                let is_lod = matches!(&inst.op,
                    Op::ImageReadLod { .. } | Op::ImageWriteLod { .. });
                let (_, binding) = image_handles.get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "Image op image {:?} not an ImageHandle",
                        image.id)))?;
                let coord_lanes = vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "Image op coord {:?} not a vector", coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "Image op coord must have ≥2 lanes, got {}",
                        coord_lanes.len())));
                }
                let is_3d = coord_lanes.len() >= 3;
                let x_w = *ints.get(&coord_lanes[0].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Image op coord lane 0 {:?} not in ints",
                        coord_lanes[0].id)))?;
                let y_w = *ints.get(&coord_lanes[1].id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Image op coord lane 1 {:?} not in ints",
                        coord_lanes[1].id)))?;
                let z_w: Option<asm::Wreg> = if is_3d {
                    Some(*ints.get(&coord_lanes[2].id).ok_or_else(||
                        BackendError::Internal(format!(
                            "Image op coord lane 2 {:?} not in ints",
                            coord_lanes[2].id)))?)
                } else { None };
                // Lod scalar lives in `ints` (i32).  Resolve
                // ahead of the call so its W-reg is captured
                // before we spill caller-saved ints across
                // the blr.
                let lod_w: Option<asm::Wreg> = match &inst.op {
                    Op::ImageReadLod { lod, .. }
                    | Op::ImageWriteLod { lod, .. } => Some(
                        *ints.get(&lod.id).ok_or_else(||
                            BackendError::Internal(format!(
                                "Image Lod op lod {:?} not in ints",
                                lod.id)))?),
                    _ => None,
                };
                // For a write, grab the 4 texel lane V-regs
                // up front (they get spilled below, but we
                // need to copy them into the rgba stack slot
                // *before* the call).
                let texel_lanes: Option<[asm::Vreg; 4]> = if is_write {
                    let tv = match &inst.op {
                        Op::ImageWrite { texel, .. } => texel,
                        Op::ImageWriteLod { texel, .. } => texel,
                        _ => unreachable!(),
                    };
                    let lanes = vectors.get(&tv.id).cloned().ok_or_else(||
                        BackendError::Internal(format!(
                            "ImageWrite texel {:?} not a vector", tv.id)))?;
                    if lanes.len() < 4 {
                        return Err(BackendError::Unsupported(format!(
                            "ImageWrite texel must be vec4, got {} lanes",
                            lanes.len())));
                    }
                    let mut regs = [asm::Vreg(0); 4];
                    for (i, r) in regs.iter_mut().enumerate() {
                        // Integer-texel ImageWrite (e.g. R32_UINT
                        // storage images, where Slang lowers
                        // `img[uv] = 99u` to a uint4 with i32 lanes)
                        // lands the lanes in `ints`, not `scalars`.
                        // Bespoke's current spill/call shape moves
                        // V-regs into the rgba stack slot — wiring
                        // up the int->V-reg copy is a larger refactor.
                        // Fall back to cranelift via Unsupported so
                        // the upper layers retry with the other
                        // backend.
                        *r = *scalars.get(&lanes[i].id).ok_or_else(||
                            BackendError::Unsupported(format!(
                                "ImageWrite integer-texel lane {i} \
                                 (need cranelift fallback)")))?;
                    }
                    Some(regs)
                } else { None };

                // Spill sets: all owned V-regs, plus owned
                // caller-saved int W-regs (W13..W17).
                let mut live_vregs: Vec<u8> = owners.keys().copied().collect();
                live_vregs.sort();
                let mut live_iregs: Vec<u8> = int_pool.owners.keys()
                    .copied().filter(|&n| (13..=17).contains(&n)).collect();
                live_iregs.sort();

                // Result lanes (read only): 4 fresh V-regs.
                let mut lane_regs: Vec<asm::Vreg> = Vec::with_capacity(4);
                let mut lane_vals: Vec<Value> = Vec::with_capacity(4);
                if !is_write {
                    for _ in 0..4 {
                        let synth = ValueId(next_synth_id);
                        next_synth_id += 1;
                        let r = valloc_impl(&mut free_pool, &mut owners, &mut used_callee_saved_v, &mut scalars, &mut spilled, &spill_slot, &vectors, &last_use, &phi_pinned, spill_mode, &inst.op, synth)?;
                        scalars.insert(synth, r);
                        lane_regs.push(r);
                        lane_vals.push(Value { id: synth, ty: Type::F32 });
                    }
                }

                let sp = asm::Xreg(31);
                let x0 = asm::Xreg(0);
                let x2 = asm::Xreg(2);
                let x3 = asm::Xreg(3);
                let x4 = asm::Xreg(4);
                let x5 = asm::Xreg(5);
                let x9 = asm::Xreg(9);
                let x10 = asm::Xreg(10);
                let x19 = asm::Xreg(19);
                let lr = asm::Xreg(30);
                let w1 = asm::Wreg(1);
                let w2 = asm::Wreg(2);
                let w3 = asm::Wreg(3);
                let w4 = asm::Wreg(4);
                // image table layout (Arc 26):
                //   #0..32  base helpers (no Lod)
                //   #32..64 _lod variants
                //   #64+B*8 descriptor pointers
                // Within each block: 2D-read, 2D-write,
                // 3D-read, 3D-write at +0/+8/+16/+24.
                let helper_off: u16 = {
                    let block_base: u16 = if is_lod { 32 } else { 0 };
                    let within: u16 = match (is_3d, is_write) {
                        (false, false) => 0,
                        (false, true)  => 8,
                        (true,  false) => 16,
                        (true,  true)  => 24,
                    };
                    block_base + within
                };
                let desc_off: u16 = IMG_TABLE_DESC_BASE_U16 + (binding as u16) * 8;
                // rgba pointer register: X3 (2D), X4 (3D or
                // 2D Lod), X5 (3D Lod).  Each extra arg in
                // the helper signature shifts the rgba slot
                // up by one register.
                let rgba_reg = match (is_3d, is_lod) {
                    (false, false) => x3,
                    (false, true)  | (true, false) => x4,
                    (true,  true)  => x5,
                };

                // Frame: [0..16] rgba scratch, [16] x_out
                // save, [24] LR save, [32..] V spills, then
                // int spills.
                let n_v = live_vregs.len() as u16;
                let n_i = live_iregs.len() as u16;
                let iregs_base: u16 = 32 + n_v * 16;
                let raw = iregs_base + n_i * 8;
                let frame_bytes: u16 = (raw + 15) & !15;
                a.emit(asm::sub_imm_x(sp, sp, frame_bytes));
                a.emit(asm::str_x_offset(x_out, sp, 16));
                a.emit(asm::str_x_offset(lr, sp, 24));
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::str_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }
                for (i, n) in live_iregs.iter().enumerate() {
                    a.emit(asm::str_w_offset(
                        asm::Wreg(*n), sp, iregs_base + (i as u16) * 4));
                }
                // For a write, pack the texel lanes into the
                // rgba scratch slot now (before spills clobber
                // nothing -- the texel V-regs are read here).
                if let Some(tregs) = texel_lanes {
                    for (i, r) in tregs.iter().enumerate() {
                        a.emit(asm::fmov_w_from_s(w_tmp, *r));
                        a.emit(asm::str_w_offset(w_tmp, sp, (i as u16) * 4));
                    }
                }

                // Load helper fn ptr + ImageDesc ptr from the
                // X19-anchored image table.
                a.emit(asm::ldr_x_offset(x9, x19, helper_off));
                a.emit(asm::ldr_x_offset(x10, x19, desc_off));
                a.emit(asm::mov_x(x0, x10));
                a.emit(asm::mov_w(w1, x_w));
                a.emit(asm::mov_w(w2, y_w));
                // The integer-arg register sequence after
                // (desc, x, y) is conditional:
                //   2D no-lod:  (rgba in X3)
                //   2D lod:     W3=lod, rgba in X4
                //   3D no-lod:  W3=z,   rgba in X4
                //   3D lod:     W3=z, W4=lod, rgba in X5
                if let Some(z_w) = z_w {
                    a.emit(asm::mov_w(w3, z_w));
                    if let Some(lod_w) = lod_w {
                        a.emit(asm::mov_w(w4, lod_w));
                    }
                } else if let Some(lod_w) = lod_w {
                    a.emit(asm::mov_w(w3, lod_w));
                }
                a.emit(asm::add_imm_x(rgba_reg, sp, 0)); // rgba scratch
                a.emit(asm::blr_x(x9));

                // Read result lanes back (read op only).
                for (i, lane) in lane_regs.iter().enumerate() {
                    a.emit(asm::ldr_w_offset(w_tmp, sp, (i as u16) * 4));
                    a.emit(asm::fmov_s_from_w(*lane, w_tmp));
                }
                // Reload spills.
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::ldr_q_offset(
                        asm::Vreg(*n), sp, 32 + (i as u16) * 16));
                }
                for (i, n) in live_iregs.iter().enumerate() {
                    a.emit(asm::ldr_w_offset(
                        asm::Wreg(*n), sp, iregs_base + (i as u16) * 4));
                }
                a.emit(asm::ldr_x_offset(x_out, sp, 16));
                a.emit(asm::ldr_x_offset(lr, sp, 24));
                a.emit(asm::add_imm_x(sp, sp, frame_bytes));
                // Re-establish the multi-binding SSBO base
                // pointers: the helper `blr` clobbered the
                // caller-saved X12..X17 they live in, but X2
                // (the descriptor-table base) was just
                // restored, so re-load each binding from it.
                for &(base_reg, binding) in &ssbo_base_reloads {
                    a.emit(asm::ldr_x_offset(
                        base_reg, x2, (binding * 8) as u16));
                }

                if !is_write {
                    let result = inst.result.as_ref().ok_or_else(||
                        BackendError::Internal(
                            "ImageRead without result".into()))?;
                    vectors.insert(result.id, lane_vals);
                }
            }
            Op::Barrier => {
                // Workgroup-scope control barrier.  Call
                // through the image-table barrier slot:
                //
                //   ldr  x9, [x19, #IMG_TABLE_BARRIER_OFFSET]
                //   blr  x9
                //
                // Same spill discipline as the ImageRead/
                // ImageWrite arm: stash live V-regs + live
                // caller-saved int W-regs (W13..W17) across
                // the call; LR also gets a slot.  The barrier
                // takes no args and returns nothing, so the
                // frame is just the spill scratch.  Arc 150.
                let mut live_vregs: Vec<u8> = owners.keys().copied().collect();
                live_vregs.sort();
                let mut live_iregs: Vec<u8> = int_pool.owners.keys()
                    .copied().filter(|&n| (13..=17).contains(&n)).collect();
                live_iregs.sort();

                let sp = asm::Xreg(31);
                let x9 = asm::Xreg(9);
                let x19 = asm::Xreg(19);
                let lr = asm::Xreg(30);

                let n_v = live_vregs.len() as u16;
                let n_i = live_iregs.len() as u16;
                // Frame: [0]   LR save (8 B + 8 pad to 16)
                //        [16..]  V-reg spills (16 B each)
                //        [16 + n_v*16 ..] int spills (4 B each)
                let iregs_base: u16 = 16 + n_v * 16;
                let raw = iregs_base + n_i * 4;
                let frame_bytes: u16 = (raw + 15) & !15;
                a.emit(asm::sub_imm_x(sp, sp, frame_bytes));
                a.emit(asm::str_x_offset(lr, sp, 0));
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::str_q_offset(
                        asm::Vreg(*n), sp, 16 + (i as u16) * 16));
                }
                for (i, n) in live_iregs.iter().enumerate() {
                    a.emit(asm::str_w_offset(
                        asm::Wreg(*n), sp, iregs_base + (i as u16) * 4));
                }
                // ldr x9, [x19, #64] = IMG_TABLE_BARRIER_OFFSET
                a.emit(asm::ldr_x_offset(x9, x19, IMG_TABLE_BARRIER_OFFSET_U16));
                a.emit(asm::blr_x(x9));
                // Restore.
                for (i, n) in live_vregs.iter().enumerate() {
                    a.emit(asm::ldr_q_offset(
                        asm::Vreg(*n), sp, 16 + (i as u16) * 16));
                }
                for (i, n) in live_iregs.iter().enumerate() {
                    a.emit(asm::ldr_w_offset(
                        asm::Wreg(*n), sp, iregs_base + (i as u16) * 4));
                }
                a.emit(asm::ldr_x_offset(lr, sp, 0));
                a.emit(asm::add_imm_x(sp, sp, frame_bytes));
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
                a.patch(
                    ep + (PROLOGUE_SPILL_INSTS + PROLOGUE_FP_INSTS + i) * 4,
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
                a.patch(ep + (PROLOGUE_SPILL_INSTS + i) * 4,
                        asm::ldp_d_post(*a_reg, *b_reg, asm::Xreg(31), 16));
            }
        }
    }

    // Spill area: one `sub sp` at the prologue tail / `add sp`
    // at each epilogue head (NOPs when no slots were used).
    let spill_bytes_total: u16 =
        (((spill_next as usize) * 8 + 15) & !15) as u16;
    if spill_bytes_total > 0 {
        // Split across the two prologue slots (each imm12 <= 4095,
        // and 16-aligned halves keep SP aligned at every step).
        let hi = (spill_bytes_total / 2) & !15;
        let lo = spill_bytes_total - hi;
        let base = prologue_off
            + (PROLOGUE_INT_INSTS + PROLOGUE_FP_INSTS) * 4;
        if hi > 0 {
            a.patch(base, asm::sub_imm_x(
                asm::Xreg(31), asm::Xreg(31), hi));
        }
        a.patch(base + 4, asm::sub_imm_x(
            asm::Xreg(31), asm::Xreg(31), lo));
        for &ep in &epilogue_offs {
            if hi > 0 {
                a.patch(ep, asm::add_imm_x(
                    asm::Xreg(31), asm::Xreg(31), hi));
            }
            a.patch(ep + 4, asm::add_imm_x(
                asm::Xreg(31), asm::Xreg(31), lo));
        }
    }

    // lid.z stack-load patches.  The prologue's stp_x_pre /
    // stp_d_pre instructions each push SP down by 16 bytes;
    // the original [SP+0] slot ends up at [SP + frame_bytes]
    // by the time the body runs.  Compute the shift and
    // rewrite the placeholder ldr_w offsets.
    if !lid_z_load_patches.is_empty() || workgroup_buf_load_patch.is_some() {
        let int_pairs_count = if int_pool.used_callee_saved {
            PROLOGUE_INT_INSTS
        } else { 0 };
        let fp_pairs_count = if used_callee_saved_v {
            PROLOGUE_FP_INSTS
        } else { 0 };
        let frame_bytes = (int_pairs_count + fp_pairs_count) as u16 * 16
            + spill_bytes_total;
        for (off, wreg) in &lid_z_load_patches {
            a.patch(*off, asm::ldr_w_offset(*wreg, asm::Xreg(31), frame_bytes));
        }
        // workgroup_buf is the 10th arg, at SP+8 (lid.z is the
        // 9th, at SP+0); both shift up by frame_bytes once the
        // prologue pushes callee-saved pairs.
        if let Some(off) = workgroup_buf_load_patch {
            a.patch(off, asm::ldr_x_offset(
                asm::Xreg(8), asm::Xreg(31), frame_bytes + 8));
        }
    }

    // Drop unused prologue NOP slots.  When a callee-saved
    // tier was not touched by the body, its 5 (int) or 4
    // (FP) prologue slots are NOPs that nothing branches
    // into and that hold no useful instructions.  The
    // prologue sits at the function start, so dropping any
    // contiguous prefix region shifts source + target of
    // every PC-relative reference (branches, ldr-literal
    // pool loads) uniformly, leaving the encoded relative
    // delta unchanged.  Only requirement: fix up the pcmap
    // entries (absolute byte offsets recorded during emit).
    //
    // Epilogue NOPs at each ret site are NOT dropped: they
    // sit mid-function, and removing them would invalidate
    // any PC-relative ref that bridged the dropped range
    // (e.g. a ldr-q-literal pointing forward to the post-
    // function literal pool).  Reclaiming those bytes would
    // need a relocation pass that re-encodes every patch
    // site -- queued as follow-up if it matters.
    //
    // Layout: int slots 0..5 (20 bytes), FP slots 5..9
    // (16 bytes).  Drop FP slots first so the int-slot
    // offset doesn't need shifting.
    let drop_fp_prologue =
        !used_callee_saved_v && PROLOGUE_FP_INSTS > 0;
    let drop_int_prologue =
        !int_pool.used_callee_saved && PROLOGUE_INT_INSTS > 0;
    let drop_spill_prologue =
        spill_bytes_total == 0 && PROLOGUE_SPILL_INSTS > 0;
    if drop_fp_prologue || drop_int_prologue || drop_spill_prologue {
        let mut bytes = a.into_bytes();
        let spill_range = (
            prologue_off + (PROLOGUE_INT_INSTS + PROLOGUE_FP_INSTS) * 4,
            prologue_off + PROLOGUE_INSTS * 4,
        );
        let fp_range = (
            prologue_off + PROLOGUE_INT_INSTS * 4,
            prologue_off + (PROLOGUE_INT_INSTS + PROLOGUE_FP_INSTS) * 4,
        );
        let int_range = (
            prologue_off,
            prologue_off + PROLOGUE_INT_INSTS * 4,
        );
        // Drain higher range first so the lower range's
        // coordinates stay valid.
        if drop_spill_prologue {
            bytes.drain(spill_range.0..spill_range.1);
        }
        if drop_fp_prologue {
            bytes.drain(fp_range.0..fp_range.1);
        }
        if drop_int_prologue {
            bytes.drain(int_range.0..int_range.1);
        }
        // Shift pcmap entries that point past the dropped
        // region(s).  Body emission begins after the
        // prologue, so every pcmap offset is >= the FP
        // range's end.
        let total_drop =
            (if drop_fp_prologue { PROLOGUE_FP_INSTS } else { 0 }
             + if drop_int_prologue { PROLOGUE_INT_INSTS } else { 0 }
             + if drop_spill_prologue { PROLOGUE_SPILL_INSTS } else { 0 })
            * 4;
        for (off, _) in pcmap_entries.iter_mut() {
            *off -= total_drop as u32;
        }
        return Ok((bytes, pcmap_entries));
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
/// Spill-aware pointer resolution: deferred dynamic pointers
/// materialise into the X9 scratch here (one lsl+add); everything
/// else falls through to [`resolve_or_make_pointer`]. The X9
/// result must be consumed before the next deferred resolve.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn resolve_ptr_spill(
    a: &mut asm::Asm,
    deferred_ptr: &HashMap<ValueId, (asm::Xreg, i32, ValueId, u8)>,
    ints: &HashMap<ValueId, asm::Wreg>,
    const_vals: &HashMap<ValueId, u32>,
    cur_op: &Op,
    v: &Value,
    pointers: &mut HashMap<ValueId, (asm::Xreg, i32)>,
    stage: ShaderStage,
) -> Result<(asm::Xreg, i32), BackendError> {
    if let Some(&(x_base, base_off, index_id, log2)) =
        deferred_ptr.get(&v.id)
    {
        let x9 = asm::Xreg(9);
        let idx_x = if let Some(w_index) = ints.get(&index_id) {
            asm::Xreg(w_index.0)
        } else if let Some(c) = const_vals.get(&index_id) {
            // Folded/orphan constant index: materialise into
            // the W9 scratch (= X9's W view) — the lsl/add
            // below consumes it immediately.
            materialise_u32_into_w(a, asm::Wreg(9), *c);
            x9
        } else {
            return Err(BackendError::Internal(format!(
                "deferred ptr index {index_id:?} not in ints \
                 or constants; consumer={cur_op:?} ptr={:?}", v.id)));
        };
        if log2 == 0 {
            a.emit(asm::add_x(x9, x_base, idx_x));
        } else {
            a.emit(asm::lsl_imm_x(x9, idx_x, log2));
            a.emit(asm::add_x(x9, x_base, x9));
        }
        return Ok((x9, base_off));
    }
    resolve_or_make_pointer(v, pointers, stage)
}

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
    let _ = w_tmp;
    // Direct FP load: one inst, and crucially NO W9 staging —
    // when `param` is the X9 deferred-pointer scratch, `ldr w9,
    // [x9, ...]` destroyed the base after the first lane of a
    // vec load (observed as a wild address = the loaded float's
    // bit pattern).
    a.emit(asm::ldr_s_offset(dst, param, off as u16));
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
    /// Bool Phi: i32-backed (constraint B4) in a W-reg, with the
    /// dest registered in the `bools` map so Select/BranchCond
    /// consumers resolve it.
    Bool(asm::Wreg),
    /// Bool Phi overflowed to an S-reg (W pool exhausted):
    /// 0/1 carried in the low 32 bits of a V-reg. Written by
    /// `fmov s, w` phi-moves; consumers materialise through
    /// the W9 scratch.
    BoolV(asm::Vreg),
    /// Spill-resident float Phi (spill mode, reg caps hit): the
    /// dest IS a stack slot. Phi-moves `str d` into it; reads
    /// reload via the regular spill machinery, with the cached
    /// copy invalidated at block boundaries (slots only mutate
    /// at predecessor terminators).
    FloatSpill(u32),
    /// Spill-resident bool Phi: phi-moves `str w` the 0/1 into
    /// the slot; the bool consumer sites `ldr w` through the W9
    /// scratch (never cached).
    BoolSpill(u32),
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
        // Note: W8 and W9 look free per AAPCS64 but the
        // bespoke codegen uses W9 as a global scratch
        // (`w_tmp`) for ConstFloat materialisation etc., so
        // they MUST stay out of the regalloc pool.
        let mut free: Vec<u8> = (19..29).rev().collect();
        free.extend((13..18).rev());
        Self { free, owners: HashMap::new(), used_callee_saved: false }
    }

    fn alloc(&mut self, owner: ValueId) -> Result<asm::Wreg, BackendError> {
        let n = self.free.pop().ok_or_else(|| BackendError::Unsupported(
            "int linear-scan RA ran out of W-regs (W13..W17 + W19..W28); \
             retried with scalar spilling by compile()".into()))?;
        if n >= 19 { self.used_callee_saved = true; }
        self.owners.insert(n, owner);
        Ok(asm::Wreg(n))
    }

    /// Manually free a W-reg the caller knows is dead --
    /// useful for codegen-synthesised intermediates that
    /// don't appear in the IR's last_use map, so the
    /// linear-scan expire pass won't reclaim them.
    fn free(&mut self, reg: asm::Wreg) {
        if self.owners.remove(&reg.0).is_some() {
            self.free.push(reg.0);
        }
    }

    /// Return W-regs whose owner's last_use < `before`.
    fn expire(&mut self, before: usize,
              last_use: &HashMap<ValueId, usize>) {
        let mut dead: Vec<u8> = self.owners.iter()
            .filter_map(|(n, id)|
                if last_use.get(id).copied().unwrap_or(usize::MAX) < before {
                    Some(*n)
                } else { None })
            .collect();
        dead.sort_unstable();
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

/// Bool-pool overflow context (spill mode): when W10..W12 are all
/// live, a materialised compare result lands in a stack slot via
/// the W9 scratch; every bool consumer site has a `bool_in_slot`
/// fallback.
struct BoolOverflow<'a> {
    bool_in_slot: &'a mut HashMap<ValueId, u32>,
    spill_next: &'a mut u32,
    spill_free: &'a mut Vec<u32>,
    spill_mode: bool,
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
    ov: &mut BoolOverflow,
    ints: &HashMap<ValueId, asm::Wreg>,
    bools: &mut HashMap<ValueId, asm::Wreg>,
    bool_owners: &mut HashMap<u8, ValueId>,
    bool_free: &mut Vec<u8>,
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
            "icmp lhs {:?} (ty {:?}) not in ints; op={:?}",
            lhs.id, lhs.ty, inst.op)))?;
    let r = *ints.get(&rhs.id).ok_or_else(||
        BackendError::Internal(format!(
            "icmp rhs {:?} (ty {:?}) not in ints; op={:?}",
            rhs.id, rhs.ty, inst.op)))?;
    if fuse_eligible {
        a.emit(asm::cmp_w(l, r));
        *fused_branch = Some((result.id, cond));
        return Ok(());
    }
    if bool_free.is_empty() && ov.spill_mode {
        // Slot-resident bool: cset through the W9 scratch.
        let slot = ov.spill_free.pop().unwrap_or_else(|| {
            let sl = *ov.spill_next;
            *ov.spill_next += 1;
            sl
        });
        a.emit(asm::cmp_w(l, r));
        a.emit(asm::cset_w(asm::Wreg(9), cond));
        a.emit(asm::str_w_offset(
            asm::Wreg(9), asm::Xreg(31), (slot as u16) * 8));
        ov.bool_in_slot.insert(result.id, slot);
        return Ok(());
    }
    let n = bool_free.pop().ok_or_else(|| BackendError::Unsupported(
        "ran out of Bool W-regs (W10..W12 exhausted)".into()))?;
    let w_bool = asm::Wreg(n);
    a.emit(asm::cmp_w(l, r));
    a.emit(asm::cset_w(w_bool, cond));
    bools.insert(result.id, w_bool);
    bool_owners.insert(n, result.id);
    Ok(())
}

/// Emit a float comparison. Same two lowerings as
/// [`emit_icmp_to_bool`]: fused (`fcmp_s` only, condition
/// recorded in `fused_branch`) or materialised
/// (`fcmp_s + cset_w` into a bool W-pool register).
#[allow(clippy::too_many_arguments)]
fn emit_fcmp_to_bool(
    a: &mut asm::Asm,
    ov: &mut BoolOverflow,
    scalars: &HashMap<ValueId, asm::Vreg>,
    bools: &mut HashMap<ValueId, asm::Wreg>,
    bool_owners: &mut HashMap<u8, ValueId>,
    bool_free: &mut Vec<u8>,
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
    if bool_free.is_empty() && ov.spill_mode {
        let slot = ov.spill_free.pop().unwrap_or_else(|| {
            let sl = *ov.spill_next;
            *ov.spill_next += 1;
            sl
        });
        a.emit(asm::fcmp_s(l, r));
        a.emit(asm::cset_w(asm::Wreg(9), cond));
        a.emit(asm::str_w_offset(
            asm::Wreg(9), asm::Xreg(31), (slot as u16) * 8));
        ov.bool_in_slot.insert(result.id, slot);
        return Ok(());
    }
    let n = bool_free.pop().ok_or_else(|| BackendError::Unsupported(
        "ran out of Bool W-regs (W10..W12 exhausted)".into()))?;
    let w_bool = asm::Wreg(n);
    a.emit(asm::fcmp_s(l, r));
    a.emit(asm::cset_w(w_bool, cond));
    bools.insert(result.id, w_bool);
    bool_owners.insert(n, result.id);
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

/// Critical-edge splitting: a conditional terminator edge into a
/// Phi-bearing block can't carry phi-moves (they'd execute on the
/// OTHER edge too). Insert a stub block on each such edge — the
/// stub's lone `Branch(target)` is where the moves land. Returns
/// the transformed function + (stub, from) placement pairs so the
/// caller can keep stubs ADJACENT to their from-block in flat
/// order (a stub at the end would balloon every phi source's
/// live range to the whole function).
fn split_critical_edges(
    func: &Function,
) -> Option<(Function, Vec<(BlockId, BlockId)>)> {
    let has_phis = |b: &atrium_spv_ir::Block| {
        b.insts.first().is_some_and(|i| matches!(&i.op, Op::Phi(_)))
    };
    let mut next_id = func.blocks.keys().map(|b| b.0).max().unwrap_or(0) + 1;
    let mut f = func.clone();
    let mut placements: Vec<(BlockId, BlockId)> = Vec::new();
    let mut new_blocks: Vec<atrium_spv_ir::Block> = Vec::new();
    // (target, original_from, stub) phi-arm rewrites to apply.
    let mut arm_rewrites: Vec<(BlockId, BlockId, BlockId)> = Vec::new();

    let from_ids: Vec<BlockId> = f.blocks.keys().copied().collect();
    for from in from_ids {
        // Collect this terminator's conditional-edge targets.
        let (term_off, targets): (u32, Vec<BlockId>) = {
            let b = f.blocks.get(&from).unwrap();
            match b.insts.last().map(|i| (&i.op, i.source_spirv_offset)) {
                Some((Op::BranchCond { t_block, f_block, .. }, off)) =>
                    (off, vec![*t_block, *f_block]),
                Some((Op::Switch { cases, default, .. }, off)) => {
                    let mut v: Vec<BlockId> =
                        cases.iter().map(|(_, t)| *t).collect();
                    v.push(*default);
                    (off, v)
                }
                _ => continue,
            }
        };
        let mut stub_for: HashMap<BlockId, BlockId> = HashMap::new();
        for target in targets {
            if stub_for.contains_key(&target) { continue; }
            let needs = f.blocks.get(&target).is_some_and(has_phis);
            if !needs { continue; }
            let stub = BlockId(next_id);
            next_id += 1;
            new_blocks.push(atrium_spv_ir::Block {
                id: stub,
                kind: atrium_spv_ir::BlockKind::Linear,
                insts: vec![atrium_spv_ir::Inst {
                    op: Op::Branch(target),
                    result: None,
                    source_spirv_offset: term_off,
                }],
            });
            stub_for.insert(target, stub);
            placements.push((stub, from));
            arm_rewrites.push((target, from, stub));
        }
        if stub_for.is_empty() { continue; }
        // Rewrite the terminator's edges to the stubs.
        let b = f.blocks.get_mut(&from).unwrap();
        match &mut b.insts.last_mut().unwrap().op {
            Op::BranchCond { t_block, f_block, .. } => {
                if let Some(st) = stub_for.get(t_block) { *t_block = *st; }
                if let Some(st) = stub_for.get(f_block) { *f_block = *st; }
            }
            Op::Switch { cases, default, .. } => {
                for (_, t) in cases.iter_mut() {
                    if let Some(st) = stub_for.get(t) { *t = *st; }
                }
                if let Some(st) = stub_for.get(default) { *default = *st; }
            }
            _ => unreachable!(),
        }
    }
    if new_blocks.is_empty() {
        return None;
    }
    for (target, from, stub) in arm_rewrites {
        let tb = f.blocks.get_mut(&target).unwrap();
        for inst in tb.insts.iter_mut() {
            let Op::Phi(arms) = &mut inst.op else { break };
            for arm in arms.iter_mut() {
                if arm.from == from {
                    arm.from = stub;
                }
            }
        }
    }
    for nb in new_blocks {
        f.blocks.insert(nb.id, nb);
    }
    Some((f, placements))
}

/// Spill-aware V-reg allocation. Fast path = alloc_vreg; on an
/// empty pool in spill mode, EVICT the in-register scalar with
/// the farthest last_use whose slot is already valid (write-once
/// at def), excluding values the current inst reads (directly or
/// through a read vector's lanes) and pinned phi dests. Eviction
/// emits no code.
#[allow(clippy::too_many_arguments)]
fn valloc_impl(
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    used_callee_saved_v: &mut bool,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    spilled: &mut std::collections::HashSet<ValueId>,
    spill_slot: &HashMap<ValueId, u32>,
    vectors: &HashMap<ValueId, Vec<Value>>,
    last_use: &HashMap<ValueId, usize>,
    phi_pinned: &std::collections::HashSet<ValueId>,
    spill_mode: bool,
    cur_op: &Op,
    owner: ValueId,
) -> Result<asm::Vreg, BackendError> {
    if let Some(n) = free_pool.pop() {
        if n < 16 { *used_callee_saved_v = true; }
        if n == 30 && std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok() {
            eprintln!("V30 ALLOC -> {owner:?}");
        }
        owners.insert(n, owner);
        return Ok(asm::Vreg(n));
    }
    if spill_mode {
        let read_now = |id: ValueId| -> bool {
            if op_reads(cur_op, id) { return true; }
            vectors.iter().any(|(vid, lanes)|
                op_reads(cur_op, *vid)
                    && lanes.iter().any(|l| l.id == id))
        };
        // Deterministic choice: tie-break by id then reg, so
        // identical SPIR-V always yields identical machine code
        // (HashMap iteration order must never leak into output).
        let victim = owners.iter()
            .filter(|(n, id)| {
                scalars.get(id) == Some(&asm::Vreg(**n))
                    && spill_slot.contains_key(id)
                    && !phi_pinned.contains(id)
                    && !read_now(**id)
            })
            .max_by_key(|(n, id)|
                (last_use.get(id).copied().unwrap_or(0), id.0, **n))
            .map(|(n, id)| (*n, *id));
        if let Some((n, victim_id)) = victim {
            if std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok()
                && (n == 30 || victim_id.0 == 118 || owner.0 == 118)
            {
                eprintln!("VEVICT V{n}: victim={victim_id:?} -> {owner:?}");
            }
            owners.remove(&n);
            scalars.remove(&victim_id);
            spilled.insert(victim_id);
            owners.insert(n, owner);
            return Ok(asm::Vreg(n));
        }
        return Err(BackendError::Unsupported(
            "V pressure with no spillable victim".into()));
    }
    Err(BackendError::Unsupported(
        "linear-scan RA ran out of V-regs (V16..V31 + V8..V15); \
         compile() retries with scalar spilling".into()))
}

/// Evict ONE in-register int: the slot-valid value with the
/// farthest last_use, excluding pinned phi dests and values the
/// current inst reads. Emits no code (slots are write-once at
/// def). Returns false when nothing is evictable.
#[allow(clippy::too_many_arguments)]
fn wevict_one(
    int_pool: &mut IntPool,
    ints: &mut HashMap<ValueId, asm::Wreg>,
    spilled_w: &mut std::collections::HashSet<ValueId>,
    spill_slot: &HashMap<ValueId, u32>,
    vectors: &HashMap<ValueId, Vec<Value>>,
    deferred_ptr: &HashMap<ValueId, (asm::Xreg, i32, ValueId, u8)>,
    last_use: &HashMap<ValueId, usize>,
    phi_pinned: &std::collections::HashSet<ValueId>,
    cur_op: &Op,
) -> bool {
    // Exclusions beyond direct reads: (a) int lanes read THROUGH
    // their vector's id (uvec3 builtins keep W-reg lanes); (b) a
    // deferred pointer's INDEX is read wherever the POINTER is —
    // the consumer re-materialises base+index<<k from it.
    let read_now = |id: ValueId| -> bool {
        if op_reads(cur_op, id) { return true; }
        if vectors.iter().any(|(vid, lanes)|
            op_reads(cur_op, *vid)
                && lanes.iter().any(|l| l.id == id))
        {
            return true;
        }
        deferred_ptr.iter().any(|(pid, (_, _, idx, _))|
            *idx == id && op_reads(cur_op, *pid))
    };
    let victim = int_pool.owners.iter()
        .filter(|(n, id)| {
            ints.get(id) == Some(&asm::Wreg(**n))
                && spill_slot.contains_key(id)
                && !phi_pinned.contains(id)
                && !read_now(**id)
        })
        .max_by_key(|(n, id)|
            (last_use.get(id).copied().unwrap_or(0), id.0, **n))
        .map(|(n, id)| (*n, *id));
    if let Some((n, victim_id)) = victim {
        int_pool.owners.remove(&n);
        int_pool.free.push(n);
        ints.remove(&victim_id);
        spilled_w.insert(victim_id);
        true
    } else {
        false
    }
}

/// Evict ONE in-register f32 scalar (mirror of [`wevict_one`]):
/// slot-valid, farthest last_use, not read by the current inst
/// (directly or as a lane of a read vector), not a pinned phi.
/// Emits no code.
#[allow(clippy::too_many_arguments)]
fn vevict_one(
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    spilled: &mut std::collections::HashSet<ValueId>,
    spill_slot: &HashMap<ValueId, u32>,
    vectors: &HashMap<ValueId, Vec<Value>>,
    last_use: &HashMap<ValueId, usize>,
    phi_pinned: &std::collections::HashSet<ValueId>,
    cur_op: &Op,
) -> bool {
    let read_now = |id: ValueId| -> bool {
        if op_reads(cur_op, id) { return true; }
        vectors.iter().any(|(vid, lanes)|
            op_reads(cur_op, *vid)
                && lanes.iter().any(|l| l.id == id))
    };
    let victim = owners.iter()
        .filter(|(n, id)| {
            scalars.get(id) == Some(&asm::Vreg(**n))
                && spill_slot.contains_key(id)
                && !phi_pinned.contains(id)
                && !read_now(**id)
        })
        .max_by_key(|(_, id)| last_use.get(id).copied().unwrap_or(0))
        .map(|(n, id)| (*n, *id));
    if let Some((n, victim_id)) = victim {
        owners.remove(&n);
        scalars.remove(&victim_id);
        spilled.insert(victim_id);
        free_pool.push(n);
        true
    } else {
        false
    }
}

/// Resolve a value to a V-reg, reloading from its spill slot if
/// evicted (`ldr d` — preserves NZCV, safe between a compare and
/// its fused branch).
#[allow(clippy::too_many_arguments)]
fn vread_impl(
    a: &mut asm::Asm,
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    used_callee_saved_v: &mut bool,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    spilled: &mut std::collections::HashSet<ValueId>,
    spill_slot: &HashMap<ValueId, u32>,
    vectors: &HashMap<ValueId, Vec<Value>>,
    last_use: &HashMap<ValueId, usize>,
    phi_pinned: &std::collections::HashSet<ValueId>,
    spill_mode: bool,
    cur_op: &Op,
    id: ValueId,
) -> Result<asm::Vreg, BackendError> {
    if let Some(v) = scalars.get(&id) {
        return Ok(*v);
    }
    let slot = *spill_slot.get(&id).ok_or_else(||
        BackendError::Internal(format!(
            "reload of {id:?} without a spill slot")))?;
    let v = valloc_impl(free_pool, owners, used_callee_saved_v,
        scalars, spilled, spill_slot, vectors, last_use,
        phi_pinned, spill_mode, cur_op, id)?;
    if std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok()
        && (v.0 == 30 || id.0 == 118)
    {
        eprintln!("VREAD {id:?} -> V{} (slot {slot})", v.0);
    }
    a.emit(asm::ldr_d_offset(v, asm::Xreg(31), (slot as u16) * 8));
    scalars.insert(id, v);
    spilled.remove(&id);
    Ok(v)
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
        | Clz(s) | Rbit(s)
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
        // Reads nothing (builtins ride ABI registers; handles
        // carry literal set/binding; Barrier is pure sync).
        LoadBuiltin(_) | ImageHandle { .. } | Barrier => false,
        FFloor(s) | FCeil(s) | FTrunc(s) | FAbs(s) | FSqrt(s)
        | PackHalf2x16(s) | UnpackHalf2x16(s)
        | AtomicLoad(s) | ImageQuerySize(s)
        | Derivative { value: s, .. } =>
            s.id == id,
        FMin(l, r) | FMax(l, r) => l.id == id || r.id == id,
        Fma(x, y, z) => x.id == id || y.id == id || z.id == id,
        PtrOffsetDynamic { base, index, .. } =>
            base.id == id || index.id == id,
        AtomicStore { ptr, value } | AtomicIAdd { ptr, value }
        | AtomicAnd { ptr, value } | AtomicOr { ptr, value }
        | AtomicXor { ptr, value } | AtomicSMin { ptr, value }
        | AtomicSMax { ptr, value } | AtomicUMin { ptr, value }
        | AtomicUMax { ptr, value } | AtomicExchange { ptr, value } =>
            ptr.id == id || value.id == id,
        AtomicCompareExchange { ptr, expected, desired } =>
            ptr.id == id || expected.id == id || desired.id == id,
        CombineSampledImage { image, sampler } =>
            image.id == id || sampler.id == id,
        ImageRead { image, coord } =>
            image.id == id || coord.id == id,
        ImageWrite { image, coord, texel } =>
            image.id == id || coord.id == id || texel.id == id,
        ImageReadLod { image, coord, lod } =>
            image.id == id || coord.id == id || lod.id == id,
        ImageWriteLod { image, coord, texel, lod } =>
            image.id == id || coord.id == id
                || texel.id == id || lod.id == id,
        ImageSampleImplicitLod { sampled_image, coord } =>
            sampled_image.id == id || coord.id == id,
        ImageSampleExplicitLod { sampled_image, coord, lod } =>
            sampled_image.id == id || coord.id == id || lod.id == id,
        ImageSampleDref { sampled_image, coord, dref } =>
            sampled_image.id == id || coord.id == id || dref.id == id,
        ImageGather { sampled_image, coord, component } =>
            sampled_image.id == id || coord.id == id
                || component.id == id,
        SampledImageQuerySizeLod { image, lod } =>
            image.id == id || lod.id == id,
        // Future variants: conservative (safe for coalescing —
        // pessimistic for spill eviction, which then just finds
        // fewer victims at that inst).
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
            Op::FFloor(x) | Op::FCeil(x) | Op::FTrunc(x) => {
                mark(x.id);
                if let Some(lanes) = vec_lanes.get(&x.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::FAbs(x) | Op::FSqrt(x) => {
                mark(x.id);
                // Propagate lane-liveness for vector operands
                // -- per-lane emit reads each element's V-reg
                // at this inst's index, so the lanes must
                // stay alive too.  Without this, the linear-
                // scan reclaims element V-regs after the
                // ConstVec inst that owns them and reassigns
                // to later defs, producing stale data.
                if let Some(lanes) = vec_lanes.get(&x.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
            Op::FMin(l, r) | Op::FMax(l, r) => {
                mark(l.id); mark(r.id);
                if let Some(lanes) = vec_lanes.get(&l.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
                if let Some(lanes) = vec_lanes.get(&r.id).cloned() {
                    for lid in &lanes { mark(*lid); }
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
            // FMA reads three scalars (the fused-mul operands + the addend);
            // the FMul it replaced is gone, so its operands must stay live to
            // here. Scalar-only (the fusion pass never produces a vec FMA).
            Op::Fma(x, y, z) => { mark(x.id); mark(y.id); mark(z.id); }
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
            | Op::Clz(s) | Op::Rbit(s)
            | Op::ConvertSToF(s) | Op::ConvertUToF(s)
            | Op::ConvertFToS(s) | Op::ConvertFToU(s) => mark(s.id),
            Op::Bitcast(s, _) => mark(s.id),
            Op::UnpackHalf2x16(s) => mark(s.id),
            Op::PackHalf2x16(s) => {
                mark(s.id);
                // vec2 operand -- keep both lane scalars live.
                if let Some(lanes) = vec_lanes.get(&s.id).cloned() {
                    for lid in &lanes { mark(*lid); }
                }
            }
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
            | Op::FOrdGt(l, r) | Op::FOrdGe(l, r)
            | Op::FUnordEq(l, r) | Op::FUnordNe(l, r)
            | Op::FUnordLt(l, r) | Op::FUnordLe(l, r)
            | Op::FUnordGt(l, r) | Op::FUnordGe(l, r) => {
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
            Op::PtrOffsetDynamic { base: _, index, stride: _ } => {
                // index is an int scalar -- mark it so the
                // IntPool keeps the W-reg live until this op.
                mark(index.id);
            }
            Op::AtomicIAdd { ptr: _, value }
            | Op::AtomicAnd { ptr: _, value }
            | Op::AtomicOr  { ptr: _, value }
            | Op::AtomicXor { ptr: _, value }
            | Op::AtomicSMin { ptr: _, value }
            | Op::AtomicSMax { ptr: _, value }
            | Op::AtomicUMin { ptr: _, value }
            | Op::AtomicUMax { ptr: _, value }
            | Op::AtomicExchange { ptr: _, value }
            | Op::AtomicStore { ptr: _, value } => {
                // value is an int scalar; mark for IntPool.
                mark(value.id);
            }
            Op::AtomicCompareExchange { ptr: _, expected, desired } => {
                mark(expected.id);
                mark(desired.id);
            }
            Op::AtomicLoad(_) => {
                // No int operand to mark; result is fresh.
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
            Op::ImageGather { sampled_image, coord, component } => {
                mark(sampled_image.id);
                mark(coord.id);
                mark(component.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::SampledImageQuerySizeLod { image, lod } => {
                mark(image.id);
                mark(lod.id);
            }
            Op::ImageFetch { image, coord, lod } => {
                mark(image.id);
                mark(coord.id);
                if let Some(l) = lod { mark(l.id); }
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageRead { image, coord } => {
                mark(image.id);
                mark(coord.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageWrite { image, coord, texel } => {
                mark(image.id);
                mark(coord.id);
                mark(texel.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
                if let Some(lane_ids) = vec_lanes.get(&texel.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageReadLod { image, coord, lod } => {
                mark(image.id);
                mark(coord.id);
                mark(lod.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageWriteLod { image, coord, texel, lod } => {
                mark(image.id);
                mark(coord.id);
                mark(texel.id);
                mark(lod.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
                if let Some(lane_ids) = vec_lanes.get(&texel.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageTexelPointer { image, coord } => {
                mark(image.id);
                mark(coord.id);
                if let Some(lane_ids) = vec_lanes.get(&coord.id).cloned() {
                    for lid in &lane_ids { mark(*lid); }
                }
            }
            Op::ImageQuerySize(image) => {
                mark(image.id);
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

    // ── Vector-membership liveness ────────────────────────────
    //
    // A lane value is READ wherever any vector containing it is
    // read: codegen lane-walks resolve the lane's own register at
    // the consuming inst. The per-inst sweep above marks lanes at
    // the VECTOR-BUILDING inst only — a vector consumed later
    // than the build left its lanes under-live (observed: a
    // saturate's 0-splat lane expired between the splat and the
    // FMax lane-walk; the stale register read whatever value
    // took it). Propagate vec last_use down to members in
    // reverse def order (handles chains); two passes for safety.
    let mut members: HashMap<ValueId, Vec<ValueId>> = HashMap::new();
    for inst in insts.iter() {
        let Some(r) = inst.result.as_ref() else { continue };
        match &inst.op {
            Op::ConstVec(els) =>
                { members.insert(r.id, els.iter().map(|v| v.id).collect()); }
            Op::VectorShuffle { src1, src2, .. } =>
                { members.insert(r.id, vec![src1.id, src2.id]); }
            Op::VectorInsert { vector, scalar, .. } =>
                { members.insert(r.id, vec![vector.id, scalar.id]); }
            Op::Select { cond: _, t_val, f_val }
                if matches!(r.ty,
                    Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_)) =>
                { members.insert(r.id, vec![t_val.id, f_val.id]); }
            Op::Phi(arms)
                if matches!(r.ty,
                    Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_)) =>
                { members.insert(r.id,
                    arms.iter().map(|a| a.value.id).collect()); }
            _ => {}
        }
    }
    for _ in 0..2 {
        for inst in insts.iter().rev() {
            let Some(r) = inst.result.as_ref() else { continue };
            let Some(ms) = members.get(&r.id) else { continue };
            let Some(&vl) = last_use.get(&r.id) else { continue };
            for m in ms {
                let e = last_use.entry(*m).or_insert(vl);
                *e = (*e).max(vl);
            }
        }
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
/// Emit a scalar fused multiply-add: `Fma(a, b, c)` → `fmadd Sd, Sa, Sb, Sc`
/// (Sd = c + a*b, one rounding). The fusion pass only produces scalar-F32
/// FMAs, so all three operands live in `scalars`. ARM reads all three sources
/// before writing Sd, so coalescing the dest into a source register (a Phi
/// accumulator) is safe.
#[allow(clippy::too_many_arguments)]
fn emit_fma(
    a: &mut asm::Asm,
    scalars: &mut HashMap<ValueId, asm::Vreg>,
    free_pool: &mut Vec<u8>,
    owners: &mut HashMap<u8, ValueId>,
    used_callee_saved_v: &mut bool,
    coalesce: Option<&PhiDest>,
    inst: &atrium_spv_ir::Inst,
    av: &Value,
    bv: &Value,
    cv: &Value,
) -> Result<(), BackendError> {
    let result = inst.result.as_ref().ok_or_else(||
        BackendError::Internal("fma without result".into()))?;
    let ra = *scalars.get(&av.id).ok_or_else(||
        BackendError::Internal(format!("fma a {:?} missing", av.id)))?;
    let rb = *scalars.get(&bv.id).ok_or_else(||
        BackendError::Internal(format!("fma b {:?} missing", bv.id)))?;
    let rc = *scalars.get(&cv.id).ok_or_else(||
        BackendError::Internal(format!("fma c {:?} missing", cv.id)))?;
    let d = match coalesce {
        Some(PhiDest::Float(v)) => *v,
        _ => alloc_vreg(free_pool, owners, used_callee_saved_v, result.id)?,
    };
    a.emit(asm::fmadd_s(d, ra, rb, rc));
    scalars.insert(result.id, d);
    Ok(())
}

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
            if std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok() {
                eprintln!("FPBIN {:?}: l={:?}@V{} r={:?}@V{} -> {:?}@V{}",
                    std::mem::discriminant(&inst.op),
                    lhs.id, l.0, rhs.id, r.0, result.id, d.0);
            }
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
                if std::env::var("ATRIUM_SPV_RA_DEBUG").is_ok() {
                    eprintln!("VECLANE {li}: l={:?}@V{} r={:?}@V{} -> V{}",
                        ll.id, l.0, rl.id, r.0, d.0);
                }
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

/// P2.2b — emit the batched-fragment span thunk
/// (`atrium_fs_main_span`).  Hand-written ARM64 that shades up to
/// `lane_count` pixels in one call by looping over the lanes and
/// `BL`-ing the *already-emitted* scalar `atrium_fs_main` per
/// covered lane (call-per-lane).  Reuses the existing body verbatim
/// — only the per-lane argument marshalling + the FFI crossing are
/// amortized across the span.
///
/// Returns the thunk bytes, to be appended to the SAME combined
/// fragment body so the `BL` needs no relocation: `atrium_fs_main`
/// sits at body offset 0 and the thunk at `fs_main_len`, so the
/// `BL`'s imm26 is `-((fs_main_len + bl_local)/4)` — computable
/// here.  `None` for shapes outside the supported subset (non-MRT,
/// non-image-sampling, non-derivative fragment shaders), in which
/// case the caller leaves `entries.fs_span` absent.
///
/// AAPCS64 span ABI (matches `atrium_spv_loader::FsSpanMain`): regs
/// x0=varyings_soa, w1=varying_stride, x2=uniforms, x3=push,
/// x4..x7=frag_{x,y,z,w} ptrs; stack `[sp + frame + k*8]`:
/// k0=coverage_mask, k1=samples_mask, k2=out_color_soa,
/// k3=out_depth, k4=front_facing, k5=primitive_id, k6=lane_count.
///
/// Frame = 144 B.  Running pointers live in callee-saved regs
/// (preserved across the inner `BL` by `fs_main`'s own
/// prologue/epilogue): x19=lane, x20=lane_count, x21=varyings,
/// x22=out_color, x23=out_depth, x24..x27=frag ptrs, x28=mask.
/// Shared args are spilled to the frame and reloaded per active
/// lane.
fn emit_fragment_span_thunk(
    func: &Function, fs_main_len: usize, target: Target,
) -> Option<Vec<u8>> {
    use asm::{Xreg, Wreg, Vreg, Cond, SP, XZR};
    if func.stage != ShaderStage::Fragment { return None; }
    // Incoming stack-arg byte offsets (relative to the post-prologue
    // SP, i.e. +144 frame).  The first five (mask/smask/out_color/
    // out_depth/ff) agree across ABIs, but the trailing u32s differ:
    // Apple's ARM64 ABI PACKS stack args to natural size (4-byte
    // slots), while AAPCS64 (FreeBSD) rounds each to an 8-byte slot.
    let darwin = matches!(target, Target::Aarch64Darwin);
    let pid_in: u16        = if darwin { 144 + 36 } else { 144 + 40 }; // 180 vs 184
    let lane_count_in: u16 = if darwin { 144 + 40 } else { 144 + 48 }; // 184 vs 192
    // MRT (a colour output at a non-zero byte offset) needs a wider
    // per-lane colour slot than 16 B — out of scope for v1.
    if func.output_varying_byte_offset.values().any(|&o| o != 0) {
        return None;
    }
    // Image sampling + derivatives read helper/descriptor pointers
    // from the uniforms buffer via emit_function's fixed register
    // conventions; the call-per-lane thunk hands fs_main the same
    // x1=uniforms, so those WOULD work, but to stay byte-aligned
    // with the cranelift span's gated subset (and avoid the quad /
    // per-pixel-LOD paths the rasterizer special-cases) we skip
    // them for v1.
    let skip = func.blocks.values().any(|b|
        b.insts.iter().any(|i| matches!(i.op,
            Op::ImageHandle { .. } | Op::Derivative { .. })));
    if skip { return None; }

    let mut a = asm::Asm::new();
    // ── Prologue: alloc 144 B frame, save x29/x30 + x19..x28. ──
    a.emit(asm::stp_x_pre(Xreg(29), Xreg(30), SP, -144));
    for (r, off) in [(19u8, 16u16), (20, 24), (21, 32), (22, 40), (23, 48),
                     (24, 56), (25, 64), (26, 72), (27, 80), (28, 88)] {
        a.emit(asm::str_x_offset(Xreg(r), SP, off));
    }
    // Spill shared args into the frame (sp+96..144).
    a.emit(asm::str_x_offset(Xreg(2), SP, 96));  // uniforms  -> [sp+96]
    a.emit(asm::str_x_offset(Xreg(3), SP, 104)); // push      -> [sp+104]
    a.emit(asm::str_x_offset(Xreg(1), SP, 112)); // stride(w) -> [sp+112]
    a.emit(asm::ldr_w_offset(Wreg(9), SP, 152)); // smask @ incoming [sp+152]
    a.emit(asm::str_x_offset(Xreg(9), SP, 120)); //           -> [sp+120]
    a.emit(asm::ldr_w_offset(Wreg(9), SP, 176)); // ff    @ incoming [sp+176]
    a.emit(asm::str_x_offset(Xreg(9), SP, 128)); //           -> [sp+128]
    a.emit(asm::ldr_w_offset(Wreg(9), SP, pid_in)); // pid @ incoming
    a.emit(asm::str_x_offset(Xreg(9), SP, 136)); //           -> [sp+136]
    // Init running pointers from the reg/stack args.
    a.emit(asm::mov_x(Xreg(21), Xreg(0)));            // varyings
    a.emit(asm::ldr_x_offset(Xreg(22), SP, 160));     // out_color_soa @ [sp+160]
    a.emit(asm::ldr_x_offset(Xreg(23), SP, 168));     // out_depth     @ [sp+168]
    a.emit(asm::mov_x(Xreg(24), Xreg(4)));            // fx
    a.emit(asm::mov_x(Xreg(25), Xreg(5)));            // fy
    a.emit(asm::mov_x(Xreg(26), Xreg(6)));            // fz
    a.emit(asm::mov_x(Xreg(27), Xreg(7)));            // fw
    a.emit(asm::ldr_x_offset(Xreg(28), SP, 144));     // mask @ [sp+144]
    a.emit(asm::mov_x(Xreg(19), XZR));                // lane = 0
    a.emit(asm::ldr_w_offset(Wreg(20), SP, lane_count_in)); // lane_count

    // ── Loop. ──
    let loop_off = a.len();
    a.emit(asm::cmp_x(Xreg(19), Xreg(20)));
    let bge_off = a.len();
    a.emit(asm::b_cond(Cond::Ge, 0));                 // -> end (patched)
    a.emit(asm::movz_x(Xreg(10), 1, 0));
    a.emit(asm::and_x(Xreg(9), Xreg(28), Xreg(10)));  // mask & 1
    let cbz_off = a.len();
    a.emit(asm::cbz_x(Xreg(9), 0));                   // -> advance (patched)
    // Active lane: marshal fs_main args, BL.
    a.emit(asm::mov_x(Xreg(0), Xreg(21)));            // x0 = varyings
    a.emit(asm::ldr_x_offset(Xreg(1), SP, 96));       // x1 = uniforms
    a.emit(asm::ldr_x_offset(Xreg(2), SP, 104));      // x2 = push
    a.emit(asm::ldr_w_offset(Wreg(9), Xreg(24), 0)); a.emit(asm::fmov_s_from_w(Vreg(0), Wreg(9)));
    a.emit(asm::ldr_w_offset(Wreg(9), Xreg(25), 0)); a.emit(asm::fmov_s_from_w(Vreg(1), Wreg(9)));
    a.emit(asm::ldr_w_offset(Wreg(9), Xreg(26), 0)); a.emit(asm::fmov_s_from_w(Vreg(2), Wreg(9)));
    a.emit(asm::ldr_w_offset(Wreg(9), Xreg(27), 0)); a.emit(asm::fmov_s_from_w(Vreg(3), Wreg(9)));
    a.emit(asm::ldr_w_offset(Wreg(3), SP, 120));      // w3 = samples_mask @ [sp+120]
    a.emit(asm::mov_x(Xreg(4), Xreg(22)));            // x4 = out_color
    a.emit(asm::mov_x(Xreg(5), Xreg(23)));            // x5 = out_depth
    a.emit(asm::ldr_w_offset(Wreg(6), SP, 128));      // w6 = front_facing @ [sp+128]
    a.emit(asm::ldr_w_offset(Wreg(7), SP, 136));      // w7 = primitive_id @ [sp+136]
    let bl_off = a.len();
    a.emit(asm::bl(0));                               // -> fs_main (patched)
    // Advance (every lane, covered or not).
    let advance_off = a.len();
    a.emit(asm::ldr_w_offset(Wreg(9), SP, 112));      // stride @ [sp+112]
    a.emit(asm::add_x(Xreg(21), Xreg(21), Xreg(9)));  // varyings += stride
    a.emit(asm::add_imm_x(Xreg(22), Xreg(22), 16));   // out_color += 16
    a.emit(asm::add_imm_x(Xreg(23), Xreg(23), 4));    // out_depth += 4
    a.emit(asm::add_imm_x(Xreg(24), Xreg(24), 4));
    a.emit(asm::add_imm_x(Xreg(25), Xreg(25), 4));
    a.emit(asm::add_imm_x(Xreg(26), Xreg(26), 4));
    a.emit(asm::add_imm_x(Xreg(27), Xreg(27), 4));
    a.emit(asm::lsr_imm_x(Xreg(28), Xreg(28), 1));    // mask >>= 1
    a.emit(asm::add_imm_x(Xreg(19), Xreg(19), 1));    // lane++
    let b_off = a.len();
    a.emit(asm::b(0));                                // -> loop (patched)

    // ── Epilogue. ──
    let end_off = a.len();
    for (r, off) in [(19u8, 16u16), (20, 24), (21, 32), (22, 40), (23, 48),
                     (24, 56), (25, 64), (26, 72), (27, 80), (28, 88)] {
        a.emit(asm::ldr_x_offset(Xreg(r), SP, off));
    }
    a.emit(asm::ldp_x_post(Xreg(29), Xreg(30), SP, 144));
    a.emit(asm::ret());

    // ── Resolve branches (imm in instruction units = bytes/4). ──
    a.patch(bge_off, asm::b_cond(Cond::Ge, ((end_off - bge_off) / 4) as i32));
    a.patch(cbz_off, asm::cbz_x(Xreg(9), ((advance_off - cbz_off) / 4) as i32));
    a.patch(b_off, asm::b(((loop_off as i64 - b_off as i64) / 4) as i32));
    // BL → fs_main at combined-body offset 0; the thunk's bl sits at
    // blob offset (fs_main_len + bl_off), so the relative jump is
    // negative and independent of where the body lands in the blob.
    let bl_rel = -(((fs_main_len + bl_off) / 4) as i64) as i32;
    a.patch(bl_off, asm::bl(bl_rel));

    Some(a.into_bytes())
}
