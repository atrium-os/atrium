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

use std::collections::HashMap;

use atrium_spv_ir::{
    FloatKind, Module, Op, ShaderStage, StorageClass, Type, ValueId,
};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Function as ClifFunction, InstBuilder, MemFlags, Signature,
    StackSlotData, StackSlotKind, UserFuncName, Value as ClifValue,
};
use cranelift_codegen::ir::types as clif_types;
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

/// Result of compiling a module.
///
/// `object` is the raw object-file bytes ready to pass
/// to `ld` (production) or `cc -shared`/`-dynamiclib`
/// (test harness) for the final `.so` link.
///
/// `pcmap` is the serialized [`atrium_spv_pcmap`] sidecar.
/// Per spec §10.1, the Cranelift backend produces a
/// function-granularity map (one entry per function start)
/// rather than per-instruction. Cranelift's emission API
/// doesn't expose per-instruction host PCs at the
/// granularity our IR's `source_spirv_offset` field
/// targets. The bespoke backend (phase 3+) emits the
/// fine-grained map by virtue of controlling every
/// instruction byte directly.
///
/// For shaders compiled via this backend, crash triage
/// can attribute a faulting host PC to the containing
/// function via `dlsym` + symbol table, then use the
/// pcmap to find the SPIR-V offset where that function
/// started. Better than a raw native PC; worse than the
/// instruction-level map the bespoke backend will give us.
#[derive(Debug, Clone)]
pub struct CompileOutput {
    /// Object-file bytes.
    pub object: Vec<u8>,
    /// PC-map sidecar bytes (atrium-spv-pcmap format v1).
    pub pcmap: Vec<u8>,
}

/// Result of [`compile_blob`] — the JIT-emit counterpart
/// of [`CompileOutput`], matching
/// [`atrium_spv_backend_bespoke::BlobOutput`] field-for-
/// field so `atrium-spv-compile` can handle both backends'
/// blob output uniformly.
#[derive(Debug, Clone)]
pub struct BlobOutput {
    /// Serialised `atrium-spv-blob` container — a flat
    /// position-independent code blob the loader `mmap`s
    /// `PROT_EXEC` directly. No `cc`, no `dlopen`.
    pub blob: Vec<u8>,
    /// PC-map sidecar bytes (atrium-spv-pcmap format v1).
    pub pcmap: Vec<u8>,
}

/// Compile an atrium-spv-ir module to a native object
/// file + PC-map sidecar.
///
/// On unsupported IR shapes, returns
/// [`BackendError::Unsupported`]. The `atrium-spv-compile`
/// driver interprets that as "fall back to bespoke" in
/// the production path, or "skip this runner" in the
/// test harness.
pub fn compile(module: &Module, target: Target) -> Result<CompileOutput, BackendError> {
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

    // ── 3. Emit each function + build the pcmap ────────────
    //
    // Cranelift's emission API doesn't surface per-host-
    // instruction source-loc → PC mapping cleanly at this
    // version, so the pcmap is function-granularity only.
    // For each atrium-spv-ir function we record one entry
    // whose host_offset is 0 (function-relative — the
    // daemon resolves which function via dlsym first) and
    // whose spirv_offset is the first IR instruction's
    // source_spirv_offset.
    //
    // Today every IR Inst has source_spirv_offset = 0
    // (the frontend phase 1 v1 placeholder, to be wired
    // properly in phase 1 v2 via an rspirv Consumer
    // adapter). So the pcmap entries all show 0; the
    // shape is correct + the data lands when the frontend
    // starts emitting real offsets.
    let mut pcmap = atrium_spv_pcmap::Builder::new();
    for func in &module.functions {
        emit_function(&mut clif_module, func)?;
        // Add a pcmap entry for this function's start.
        // host_offset stays at 0 — function-relative;
        // production crash handlers resolve which function
        // via dlsym first and consult the pcmap second.
        // First *real* source SPIR-V offset for this fn:
        // skip the entry-block prelude of hoisted constants
        // (source_spirv_offset == 0) and pick the first
        // instruction translated from real SPIR-V.
        let first_spirv_offset = module.functions
            .iter()
            .find(|f| f.name == func.name)
            .and_then(|f| f.blocks.get(&f.entry_block))
            .and_then(|b| b.insts.iter()
                .map(|i| i.source_spirv_offset)
                .find(|o| *o != 0))
            .unwrap_or(0);
        // For multi-function modules every entry currently
        // has host_offset=0 (we don't have real per-
        // function host PCs from Cranelift). The pcmap
        // format tolerates duplicate host_offsets per
        // atrium_spv_pcmap::PcMap's `lookup` contract;
        // for phase 2 v4 we accept that lookup returns
        // the last-recorded entry. Future work: switch
        // to one pcmap per .so symbol when the production
        // dispatcher needs per-function distinction.
        pcmap.push(0, first_spirv_offset);
    }

    // ── 4. Finalise and emit bytes ─────────────────────────
    let product = clif_module.finish();
    let object = product.emit()
        .map_err(|e| BackendError::Internal(format!("object emit: {e}")))?;
    let pcmap = pcmap.finish_to_bytes();
    Ok(CompileOutput { object, pcmap })
}

/// Compile an atrium-spv-ir module straight to a flat
/// executable blob — the JIT-emit path's Cranelift entry,
/// matching [`atrium_spv_backend_bespoke::compile_blob`].
///
/// Cranelift's aarch64 lowering materialises shader
/// constants inline (no literal pool / `.rodata`) and
/// shaders are single-function, so the emitted `.text` for
/// every shader in the corpus has **zero relocations** —
/// it's already self-contained PIC code, exactly like the
/// bespoke backend's output. `compile_blob` builds the
/// object as usual, then re-parses it (`object` read API,
/// ELF/Mach-O-agnostic) to lift out the flat `.text` bytes
/// and the per-stage entry-symbol offsets.
///
/// If the object ever *does* carry a relocation, this
/// returns [`BackendError::Internal`] rather than silently
/// producing broken code — a loud signal to implement a
/// reloc table in the blob format. (`atrium-spv-compile`
/// has no `cc` fallback anymore; that's deliberate.)
pub fn compile_blob(module: &Module, target: Target)
    -> Result<BlobOutput, BackendError>
{
    use object::{Object, ObjectSection, ObjectSymbol};

    let CompileOutput { object, pcmap } = compile(module, target)?;

    let obj = object::File::parse(&*object).map_err(|e|
        BackendError::Internal(format!("re-parsing emitted object: {e}")))?;

    // The single text section holds all the code.
    let text = obj.sections()
        .find(|s| s.kind() == object::SectionKind::Text)
        .ok_or_else(|| BackendError::Internal(
            "emitted object has no text section".into()))?;
    // Self-contained-PIC invariant: no relocations. If this
    // ever trips, the blob format needs a reloc table —
    // fail loud rather than emit code that jumps nowhere.
    if text.relocations().next().is_some() {
        return Err(BackendError::Internal(
            "Cranelift object carries relocations — the flat-blob path \
             needs a relocation table (open question 1 in the RUNBOOK \
             JIT-emit design)".into()));
    }
    let text_addr = text.address();
    let code = text.data()
        .map_err(|e| BackendError::Internal(format!(
            "reading text section: {e}")))?
        .to_vec();

    // Entry offsets from the exported symbols. Mach-O
    // prefixes symbol names with `_`; strip it.
    let mut entries = atrium_spv_blob::EntryOffsets::default();
    for sym in obj.symbols() {
        let Ok(name) = sym.name() else { continue };
        let name = name.strip_prefix('_').unwrap_or(name);
        // In a relocatable object the text section's
        // address is 0, but subtract it anyway to be exact.
        let off = (sym.address() - text_addr) as u32;
        match name {
            "atrium_vs_main" => entries.vs = Some(off),
            "atrium_fs_main" => entries.fs = Some(off),
            "atrium_cs_main" => entries.cs = Some(off),
            _ => {}
        }
    }
    if entries.vs.is_none() && entries.fs.is_none()
        && entries.cs.is_none()
    {
        return Err(BackendError::Internal(
            "emitted object exports no atrium_(vs|fs|cs)_main symbol".into()));
    }

    let blob = atrium_spv_blob::ShaderBlob {
        arch: atrium_spv_blob::ARCH_AARCH64,
        code,
        entries,
    };
    Ok(BlobOutput { blob: blob.to_bytes(), pcmap })
}

/// Emit one function body.
///
/// Phase 2 v2: walks each block's instructions and emits
/// matching Cranelift IR for the supported ops
/// (ConstFloat / ConstVec / Store-to-Output / Return).
/// Unsupported ops return [`BackendError::Unsupported`];
/// the production driver falls through to the bespoke
/// backend on that signal.
fn emit_function(
    clif_module: &mut ObjectModule,
    func: &atrium_spv_ir::Function,
) -> Result<(), BackendError> {
    let pointer_type = clif_module.target_config().pointer_type();
    let sig = build_signature(func.stage, pointer_type)?;
    let symbol_name = exported_symbol_name(func);

    let func_id = clif_module
        .declare_function(&symbol_name, Linkage::Export, &sig)
        .map_err(|e| BackendError::Internal(
            format!("declare_function({symbol_name}): {e}"),
        ))?;

    let mut ctx = ClifContext::new();
    ctx.func = ClifFunction::with_name_signature(
        UserFuncName::user(0, func_id.as_u32()),
        sig.clone(),
    );

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.append_block_params_for_function_params(entry);

        // Pre-create one Cranelift block per IR block (so
        // terminators in any block can reference each
        // other freely). The entry IR block reuses the
        // Cranelift entry block (which carries the
        // function params).
        let mut block_map: HashMap<atrium_spv_ir::BlockId,
                                   cranelift_codegen::ir::Block> = HashMap::new();
        block_map.insert(func.entry_block, entry);
        for (id, _) in &func.blocks {
            if *id == func.entry_block { continue; }
            block_map.insert(*id, builder.create_block());
        }

        let mut translator = FnTranslator {
            stage: func.stage,
            local_size: func.local_size,
            ssbo_var_ptr: HashMap::new(),
            entry_block: entry,
            // Cache the param-index → Cranelift value
            // mapping once; ip-walk reads it for ptr-arg
            // resolution.
            params: builder.func.dfg.block_params(entry).to_vec(),
            scalars: HashMap::new(),
            vectors: HashMap::new(),
            pointers: HashMap::new(),
            block_map,
            image_handles: HashMap::new(),
            matrices: HashMap::new(),
        };

        // Multi-binding compute SSBO prologue.  When
        // ssbo_bindings declares 2+ StorageBuffer variables,
        // params[2] is a descriptor-table base (an array of
        // u64 pointers).  Load each binding's pointer into a
        // fresh SSA value at function entry, keyed by the
        // variable's IR ValueId; `resolve_pointer_param`
        // consults `ssbo_var_ptr` before falling back to the
        // legacy params[2]-direct path.
        if func.stage == ShaderStage::Compute && func.ssbo_bindings.len() >= 2 {
            builder.switch_to_block(entry);
            let table_base = translator.params[2];
            for (&vid, &(_set, binding)) in &func.ssbo_bindings {
                let p = builder.ins().load(
                    pointer_type,
                    cranelift_codegen::ir::MemFlags::new(),
                    table_base,
                    (binding as i32) * 8,
                );
                translator.ssbo_var_ptr.insert(vid, p);
            }
        }

        // Walk every IR block in id order, switching the
        // builder to the Cranelift block for each. The
        // terminator (Branch / BranchCond / Return) is
        // emitted here directly so it can pass Phi-arm
        // block-args to its target(s).
        let mut block_ids: Vec<atrium_spv_ir::BlockId> =
            func.blocks.keys().copied().collect();
        block_ids.sort_by_key(|b| b.0);

        // First pass: declare Cranelift block params for
        // every leading Op::Phi in non-entry blocks. Done
        // before the second pass emits branches so target
        // block params already exist when we generate the
        // `jump` / `brif` call sites.
        for id in &block_ids {
            if *id == func.entry_block { continue; }
            let block = func.blocks.get(id).unwrap();
            let cl_block = *translator.block_map.get(id).unwrap();
            for inst in &block.insts {
                let result = match &inst.op {
                    Op::Phi(_) => match inst.result.as_ref() {
                        Some(r) => r,
                        None => continue,
                    },
                    _ => break, // Phi must lead its block
                };
                declare_phi_block_param(
                    cl_block, result, &mut builder, &mut translator,
                )?;
            }
        }

        // Second pass: emit instructions + branches.
        for id in &block_ids {
            let block = func.blocks.get(id).ok_or_else(||
                BackendError::Internal(format!("missing block {id:?}")))?;
            let cl_block = *translator.block_map.get(id).ok_or_else(||
                BackendError::Internal(format!("no cl block for {id:?}")))?;
            if *id != func.entry_block {
                builder.switch_to_block(cl_block);
            }
            for inst in &block.insts {
                translator.emit_inst(inst, &mut builder)?;
                // Terminator: emit the right Cranelift
                // control-flow op AFTER emit_inst (which
                // is a no-op for these). Collect any
                // phi-args for the target block(s).
                match &inst.op {
                    Op::Branch(target) => {
                        let args = collect_phi_args(
                            *target, *id, func, &translator)?;
                        let cl_target = *translator.block_map
                            .get(target).unwrap();
                        let bargs: Vec<cranelift_codegen::ir::BlockArg> =
                            args.into_iter().map(Into::into).collect();
                        builder.ins().jump(cl_target, &bargs);
                    }
                    Op::Switch { selector, cases, default } => {
                        // Lower to a chain of icmp + brif:
                        //   for each (lit, target):
                        //     cmp = icmp eq selector, lit
                        //     brif cmp, target, next_fall
                        //   final fall_through: jump default
                        let sv = translator.scalars
                            .get(&selector.id).copied()
                            .ok_or_else(|| BackendError::Internal(format!(
                                "Switch selector {:?} not in scalars",
                                selector.id)))?;
                        for (case_idx, (lit, target)) in cases.iter().enumerate() {
                            let cmp = builder.ins().icmp_imm(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                sv, *lit);
                            let case_args = collect_phi_args(
                                *target, *id, func, &translator)?;
                            let case_bargs: Vec<cranelift_codegen::ir::BlockArg> =
                                case_args.into_iter().map(Into::into).collect();
                            let cl_target = *translator.block_map
                                .get(target).unwrap();
                            if case_idx + 1 == cases.len() {
                                // Last case: false → default.
                                let def_args = collect_phi_args(
                                    *default, *id, func, &translator)?;
                                let def_bargs: Vec<cranelift_codegen::ir::BlockArg> =
                                    def_args.into_iter().map(Into::into).collect();
                                let cl_def = *translator.block_map
                                    .get(default).unwrap();
                                builder.ins().brif(
                                    cmp, cl_target, &case_bargs,
                                    cl_def, &def_bargs);
                            } else {
                                // More cases: false → fresh
                                // fall-through Cranelift block.
                                let fall = builder.create_block();
                                builder.ins().brif(
                                    cmp, cl_target, &case_bargs, fall, &[]);
                                builder.switch_to_block(fall);
                            }
                        }
                        if cases.is_empty() {
                            // No cases: unconditional jump
                            // to default.
                            let def_args = collect_phi_args(
                                *default, *id, func, &translator)?;
                            let def_bargs: Vec<cranelift_codegen::ir::BlockArg> =
                                def_args.into_iter().map(Into::into).collect();
                            let cl_def = *translator.block_map
                                .get(default).unwrap();
                            builder.ins().jump(cl_def, &def_bargs);
                        }
                    }
                    Op::BranchCond { cond, t_block, f_block } => {
                        let cv = translator.scalars.get(&cond.id)
                            .copied()
                            .ok_or_else(|| BackendError::Internal(format!(
                                "BranchCond cond {:?} not in scalars",
                                cond.id)))?;
                        let t_args = collect_phi_args(
                            *t_block, *id, func, &translator)?;
                        let f_args = collect_phi_args(
                            *f_block, *id, func, &translator)?;
                        let cl_t = *translator.block_map
                            .get(t_block).unwrap();
                        let cl_f = *translator.block_map
                            .get(f_block).unwrap();
                        let t_bargs: Vec<cranelift_codegen::ir::BlockArg> =
                            t_args.into_iter().map(Into::into).collect();
                        let f_bargs: Vec<cranelift_codegen::ir::BlockArg> =
                            f_args.into_iter().map(Into::into).collect();
                        builder.ins().brif(cv, cl_t, &t_bargs, cl_f, &f_bargs);
                    }
                    _ => {}
                }
            }
        }

        builder.seal_all_blocks();
        builder.finalize();
    }

    clif_module
        .define_function(func_id, &mut ctx)
        .map_err(|e| BackendError::Internal(
            format!("define_function({symbol_name}): {e:?}"),
        ))?;
    Ok(())
}

/// Declare Cranelift block param(s) for one Op::Phi
/// result. Scalars get one block param; vectors get one
/// per lane. Stored in translator.scalars / .vectors so
/// downstream uses look them up exactly like any other
/// computed Value.
fn declare_phi_block_param(
    cl_block: cranelift_codegen::ir::Block,
    result: &atrium_spv_ir::Value,
    builder: &mut FunctionBuilder,
    translator: &mut FnTranslator,
) -> Result<(), BackendError> {
    match &result.ty {
        Type::F32 => {
            let p = builder.append_block_param(cl_block, clif_types::F32);
            translator.scalars.insert(result.id, p);
        }
        Type::I32 | Type::U32 | Type::Bool => {
            let p = builder.append_block_param(cl_block, clif_types::I32);
            translator.scalars.insert(result.id, p);
        }
        Type::I64 | Type::U64 => {
            let p = builder.append_block_param(cl_block, clif_types::I64);
            translator.scalars.insert(result.id, p);
        }
        Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
            let count = match &result.ty {
                Type::Vec2(_) => 2,
                Type::Vec3(_) => 3,
                Type::Vec4(_) => 4,
                _ => unreachable!(),
            };
            let mut lanes = Vec::with_capacity(count);
            for _ in 0..count {
                lanes.push(builder.append_block_param(cl_block, clif_types::F32));
            }
            translator.vectors.insert(result.id, lanes);
        }
        other => return Err(BackendError::Unsupported(format!(
            "Phi result of type {other:?} not supported",
        ))),
    }
    Ok(())
}

/// Collect the Cranelift values to pass as block-args
/// when branching from `source` to `target`. Reads each
/// leading Op::Phi in `target` and picks the arm with
/// `from == source`.
fn collect_phi_args(
    target: atrium_spv_ir::BlockId,
    source: atrium_spv_ir::BlockId,
    func: &atrium_spv_ir::Function,
    translator: &FnTranslator,
) -> Result<Vec<ClifValue>, BackendError> {
    let block = func.blocks.get(&target).ok_or_else(||
        BackendError::Internal(format!(
            "collect_phi_args: target block {target:?} missing",
        )))?;
    let mut args: Vec<ClifValue> = Vec::new();
    for inst in &block.insts {
        let arms = match &inst.op {
            Op::Phi(arms) => arms,
            _ => break, // phis lead the block
        };
        let arm = arms.iter().find(|a| a.from == source).ok_or_else(||
            BackendError::Internal(format!(
                "Op::Phi in block {target:?} has no arm from {source:?}",
            )))?;
        match &arm.value.ty {
            Type::F32 | Type::I32 | Type::U32 | Type::I64 | Type::U64
            | Type::Bool => {
                let v = translator.scalars.get(&arm.value.id).copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "Phi arm scalar {:?} not lowered yet",
                        arm.value.id)))?;
                args.push(v);
            }
            Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                let lanes = translator.vectors.get(&arm.value.id)
                    .cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "Phi arm vector {:?} not lowered yet",
                        arm.value.id)))?;
                args.extend(lanes);
            }
            other => return Err(BackendError::Unsupported(format!(
                "Phi arm of type {other:?} not supported",
            ))),
        }
    }
    Ok(args)
}

/// Per-function translation state.
///
/// `scalars` and `vectors` are the IR ValueId → Cranelift
/// value maps. Scalars hold one ClifValue; vectors hold
/// up to 4 ClifValues (one per lane). We don't yet use
/// Cranelift's first-class SIMD types — keeping per-lane
/// scalars makes the OpStore lane-walking emission
/// trivial and lets the backend run on any target without
/// SIMD ISel concerns.
struct FnTranslator {
    stage: ShaderStage,
    /// Compute `LocalSize` (workgroup size) from the SPIR-V
    /// `OpExecutionMode`, threaded through by the frontend.
    /// `None` for non-compute and for compute functions that
    /// left LocalSize implicit; the backend folds this into
    /// `gl_GlobalInvocationID` codegen (multiplying
    /// WorkgroupId by LocalSize before adding LocalInvocationID).
    local_size: Option<(u32, u32, u32)>,
    /// Multi-binding compute: variable ValueId -> Cranelift
    /// pointer value loaded from the descriptor table.  Empty
    /// when the shader is single-binding (legacy params[2]
    /// path).  Populated by the entry-block prologue when
    /// `func.ssbo_bindings.len() >= 2`.
    ssbo_var_ptr: HashMap<u32, ClifValue>,
    #[allow(dead_code)]
    entry_block: cranelift_codegen::ir::Block,
    params: Vec<ClifValue>,
    scalars: HashMap<ValueId, ClifValue>,
    vectors: HashMap<ValueId, Vec<ClifValue>>,
    /// IR Value (Pointer-typed) → (base Cranelift pointer
    /// value from `params`, byte offset). Populated by
    /// `Op::AccessChain` and on first reference to a bare
    /// Variable. `Op::Load` / `Op::Store` consult this map
    /// to find what `mem.load`/`mem.store` should target.
    pointers: HashMap<ValueId, (ClifValue, i32)>,
    /// IR BlockId → Cranelift block. Pre-populated for
    /// every IR block so terminators can branch freely.
    block_map: HashMap<atrium_spv_ir::BlockId,
                       cranelift_codegen::ir::Block>,
    /// IR Value (image / sampler / sampled-image handle) →
    /// the SPIR-V `(DescriptorSet, Binding)` it represents.
    /// Set by `Op::ImageHandle`; propagated unchanged by
    /// `Op::CombineSampledImage`; consumed by `Op::ImageSample*`
    /// + `Op::ImageFetch` to compute the descriptor-table
    /// offset.
    image_handles: HashMap<ValueId, (u32, u32)>,
    /// IR Value (`Mat4`-typed) → its four column vec4s,
    /// each stored as 4 lane `ClifValue`s. Populated by
    /// `Op::Load` of a Mat4; consumed by
    /// `Op::MatrixTimesVector`.
    matrices: HashMap<ValueId, Vec<Vec<ClifValue>>>,
}

impl FnTranslator {
    fn emit_inst(
        &mut self,
        inst: &atrium_spv_ir::Inst,
        builder: &mut FunctionBuilder,
    ) -> Result<(), BackendError> {
        match &inst.op {
            Op::ConstInt { value, kind } => {
                use atrium_spv_ir::IntKind;
                let (clif_ty, narrowed) = match kind {
                    IntKind::I32 | IntKind::U32 =>
                        (clif_types::I32, (*value as i32) as i64),
                    IntKind::I64 | IntKind::U64 =>
                        (clif_types::I64, *value),
                };
                let v = builder.ins().iconst(clif_ty, narrowed);
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConstInt without result Value".to_string()))?;
                self.scalars.insert(result.id, v);
                Ok(())
            }
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let v = builder.ins().f32const(*value as f32);
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConstFloat without result Value".to_string()))?;
                self.scalars.insert(result.id, v);
                Ok(())
            }
            Op::ConstFloat { value: _, kind: FloatKind::F64 } => {
                Err(BackendError::Unsupported(
                    "F64 constants not supported in phase 2 v2".to_string()))
            }
            Op::ConstVec(elements) => {
                let mut lanes = Vec::with_capacity(elements.len());
                for e in elements {
                    let v = self.scalars.get(&e.id).copied().ok_or_else(||
                        BackendError::Internal(format!(
                            "ConstVec references undefined scalar {:?}", e.id,
                        )))?;
                    lanes.push(v);
                }
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ConstVec without result Value".to_string()))?;
                self.vectors.insert(result.id, lanes);
                Ok(())
            }
            Op::Store { ptr, value } => {
                // Resolve pointer: any prior OpAccessChain has
                // landed (param, byte_offset) in self.pointers,
                // so use resolve_or_make_pointer to honour the
                // chain's offset rather than always writing at
                // the base.  Bare-Variable pointers (no chain)
                // fall through to offset 0.
                let (ptr_param, base_off) = self.resolve_or_make_pointer(ptr)?;
                // Vector value: emit one store per lane at
                // consecutive 4-byte offsets.
                if let Some(lanes) = self.vectors.get(&value.id).cloned() {
                    let bytes_per_lane = 4;
                    for (i, lane) in lanes.iter().enumerate() {
                        let lane_off = base_off
                            .saturating_add((i as i32) * (bytes_per_lane as i32));
                        builder.ins().store(MemFlags::new(), *lane, ptr_param, lane_off);
                    }
                    return Ok(());
                }
                // Scalar value: one store sized to the pointee
                // type (f32 / i32 / u32 / bool all land as a
                // 4-byte little-endian store via Cranelift's
                // type-sized i.store insn).
                if let Some(s) = self.scalars.get(&value.id).copied() {
                    builder.ins().store(MemFlags::new(), s, ptr_param, base_off);
                    return Ok(());
                }
                Err(BackendError::Unsupported(format!(
                    "Op::Store value {:?} not in vectors or scalars maps",
                    value.id,
                )))
            }
            // OpAccessChain: produce a (param, offset)
            // pointer repr by adding the IR's resolved
            // byte_offset to the base pointer's existing
            // offset. The frontend has already resolved all
            // chain indices to a single byte_offset
            // (constraint B5).
            Op::AccessChain { base, byte_offset } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "AccessChain without result Value".to_string()))?;
                let (param, base_off) = self.resolve_or_make_pointer(base)?;
                let new_off = base_off.saturating_add(*byte_offset as i32);
                self.pointers.insert(result.id, (param, new_off));
                Ok(())
            }
            Op::AtomicIAdd { ptr, value }
            | Op::AtomicAnd { ptr, value }
            | Op::AtomicOr  { ptr, value }
            | Op::AtomicXor { ptr, value }
            | Op::AtomicExchange { ptr, value } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Atomic op without result".to_string()))?;
                let (ptr_v, ptr_off) = self.resolve_or_make_pointer(ptr)?;
                let val = *self.scalars.get(&value.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "Atomic value {:?} not in scalars",
                        value.id)))?;
                let mflags = cranelift_codegen::ir::MemFlags::new();
                let old = builder.ins().load(
                    clif_types::I32, mflags, ptr_v, ptr_off);
                let new_val = match &inst.op {
                    Op::AtomicIAdd { .. } => builder.ins().iadd(old, val),
                    Op::AtomicAnd  { .. } => builder.ins().band(old, val),
                    Op::AtomicOr   { .. } => builder.ins().bor(old, val),
                    Op::AtomicXor  { .. } => builder.ins().bxor(old, val),
                    Op::AtomicExchange { .. } => val, // write addend as-is
                    _ => unreachable!(),
                };
                builder.ins().store(mflags, new_val, ptr_v, ptr_off);
                // SPIR-V returns the OLD value.
                self.scalars.insert(result.id, old);
                Ok(())
            }
            Op::PtrOffsetDynamic { base, index, stride } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "PtrOffsetDynamic without result".to_string()))?;
                let (base_ptr, base_off) = self.resolve_or_make_pointer(base)?;
                let idx = *self.scalars.get(&index.id).ok_or_else(||
                    BackendError::Internal(format!(
                        "PtrOffsetDynamic index {:?} not in scalars",
                        index.id)))?;
                // Widen the 32-bit index to 64-bit so the
                // address arithmetic doesn't truncate.
                // Cranelift's pointer type on aarch64 is I64.
                let idx_ext = builder.ins().uextend(clif_types::I64, idx);
                // Scale by stride.  Use shift for power-of-
                // two; imul otherwise (matches bespoke's
                // shift-or-madd plan).
                let scaled = if stride.is_power_of_two() {
                    let log2 = stride.trailing_zeros() as i64;
                    if log2 == 0 {
                        idx_ext
                    } else {
                        builder.ins().ishl_imm(idx_ext, log2)
                    }
                } else {
                    let s = builder.ins().iconst(
                        clif_types::I64, *stride as i64);
                    builder.ins().imul(idx_ext, s)
                };
                let addr = builder.ins().iadd(base_ptr, scaled);
                // Stash as a pointer with the residual
                // constant offset; downstream Load/Store
                // applies base_off as the load-offset
                // operand.
                self.pointers.insert(result.id, (addr, base_off));
                Ok(())
            }
            // Op::LoadBuiltin: pull a stage built-in value
            // (WorkgroupId, LocalInvocationId, GlobalInvocationId,
            // VertexIndex, InstanceIndex) from the appropriate
            // stage-ABI parameter slot rather than from memory.
            // Param layout per build_signature:
            //   Vertex   params: in_attributes, in_attr_strides,
            //                    uniforms, push_constants,
            //                    vertex_index, instance_index, ...
            //   Compute  params: uniforms, push_constants, out_buffer,
            //                    workgroup_id[0..2], local_id[0..2]
            Op::LoadBuiltin(kind) => {
                use atrium_spv_ir::BuiltinKind as BK;
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "LoadBuiltin without result Value".to_string()))?;
                match (self.stage, kind) {
                    (ShaderStage::Compute, BK::WorkgroupId) => {
                        // Vec3<uint> from params[3..6].
                        self.vectors.insert(result.id, vec![
                            self.params[3], self.params[4], self.params[5],
                        ]);
                    }
                    (ShaderStage::Compute, BK::LocalInvocationId) => {
                        // Vec3<uint> from params[6..9].
                        self.vectors.insert(result.id, vec![
                            self.params[6], self.params[7], self.params[8],
                        ]);
                    }
                    (ShaderStage::Compute, BK::GlobalInvocationId) => {
                        // gl_GlobalInvocationID =
                        //   WorkgroupID * WorkGroupSize + LocalInvocationID
                        //
                        // WorkGroupSize comes from the SPIR-V
                        // OpExecutionMode LocalSize, which the
                        // frontend stamps onto func.local_size and
                        // we cached on the FnTranslator. Missing
                        // (non-compute, or compute that left
                        // LocalSize implicit) -> default (1,1,1)
                        // which folds the multiply away.
                        let ls = self.local_size.unwrap_or((1, 1, 1));
                        let ls_arr = [ls.0, ls.1, ls.2];
                        let lanes = (0..3).map(|i| {
                            let wg = self.params[3 + i];
                            let li = self.params[6 + i];
                            if ls_arr[i] == 1 {
                                builder.ins().iadd(wg, li)
                            } else {
                                let ls_c = builder.ins().iconst(
                                    clif_types::I32, ls_arr[i] as i64);
                                let scaled = builder.ins().imul(wg, ls_c);
                                builder.ins().iadd(scaled, li)
                            }
                        }).collect();
                        self.vectors.insert(result.id, lanes);
                    }
                    (ShaderStage::Vertex, BK::VertexIndex) => {
                        // Scalar uint from params[4].
                        self.scalars.insert(result.id, self.params[4]);
                    }
                    (ShaderStage::Vertex, BK::InstanceIndex) => {
                        // Scalar uint from params[5].
                        self.scalars.insert(result.id, self.params[5]);
                    }
                    (stage, kind) => {
                        return Err(BackendError::Unsupported(format!(
                            "LoadBuiltin({kind:?}) not supported for stage {stage:?}",
                        )));
                    }
                }
                Ok(())
            }
            // OpLoad: read a leaf value through a pointer.
            // The result type is the Pointer's pointee
            // recorded on the result Value.
            Op::Load(ptr) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Load without result Value".to_string()))?;
                let (param, off) = self.resolve_or_make_pointer(ptr)?;
                let pointee = match &ptr.ty {
                    Type::Pointer(_, inner) => (**inner).clone(),
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load pointer Value is not Pointer-typed: {other:?}",
                    ))),
                };
                match &pointee {
                    Type::F32 => {
                        let v = builder.ins().load(
                            clif_types::F32, MemFlags::new(), param, off);
                        self.scalars.insert(result.id, v);
                    }
                    Type::I32 | Type::U32 | Type::Bool => {
                        let v = builder.ins().load(
                            clif_types::I32, MemFlags::new(), param, off);
                        self.scalars.insert(result.id, v);
                    }
                    Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                        let (lane_ty, lane_count) = match &pointee {
                            Type::Vec2(_) => (clif_types::F32, 2usize),
                            Type::Vec3(_) => (clif_types::F32, 3usize),
                            Type::Vec4(_) => (clif_types::F32, 4usize),
                            _ => unreachable!(),
                        };
                        let mut lanes = Vec::with_capacity(lane_count);
                        for i in 0..lane_count {
                            let lane_off = off.saturating_add((i * 4) as i32);
                            let v = builder.ins().load(
                                lane_ty, MemFlags::new(), param, lane_off);
                            lanes.push(v);
                        }
                        self.vectors.insert(result.id, lanes);
                    }
                    Type::Mat4(_) => {
                        // Materialise the matrix as 4 column
                        // vec4s stored in the matrices side
                        // table. 16-byte column stride per
                        // SPIR-V's canonical std140-ish
                        // layout (also matches the v1
                        // host-side packing convention).
                        let mut columns = Vec::with_capacity(4);
                        for c in 0..4usize {
                            let col_base = off.saturating_add((c * 16) as i32);
                            let mut lanes = Vec::with_capacity(4);
                            for i in 0..4usize {
                                let lane_off = col_base
                                    .saturating_add((i * 4) as i32);
                                let v = builder.ins().load(
                                    clif_types::F32, MemFlags::new(),
                                    param, lane_off);
                                lanes.push(v);
                            }
                            columns.push(lanes);
                        }
                        self.matrices.insert(result.id, columns);
                    }
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load of pointee type {other:?} not supported",
                    ))),
                }
                Ok(())
            }
            // Op::Branch / Op::BranchCond / Op::Switch are
            // handled at the caller level in `emit_function`
            // so it can collect Phi-args from the target
            // block(s) before emitting `jump` / `brif` /
            // br-table chain.
            Op::Branch(_) | Op::BranchCond { .. }
            | Op::Switch { .. } => Ok(()),
            // Op::Phi is materialised as a Cranelift block
            // param at block entry; the Inst itself is a
            // no-op at this point.
            Op::Phi(_) => Ok(()),
            Op::Return => {
                builder.ins().return_(&[]);
                Ok(())
            }
            Op::ReturnValue(_) => {
                Err(BackendError::Unsupported(
                    "ReturnValue not supported in phase 2 v2 (void shaders only)"
                        .to_string()))
            }
            // ── Float arithmetic (phase 2 v6) ───────────
            //
            // Scalar f32 binops: look up both operands
            // from the scalars map, emit the Cranelift
            // arithmetic instruction, cache the result.
            // Vec arithmetic is the next widening; the
            // bin/unop helper would walk both vectors
            // lane-by-lane.
            // Float arithmetic is polymorphic by operand
            // type — SPIR-V's OpFAdd works on scalar f32
            // AND vec[2,3,4]<f32>. The dispatch helper
            // checks whether the operands live in
            // `scalars` (lane count 1) or `vectors`
            // (lane count >= 2) and walks lanes
            // accordingly.
            // Integer arithmetic. All scalar; vec int
            // ops would lane-walk like float, but we
            // don't support vec ints yet.
            Op::IAdd(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().iadd(x, y)),
            Op::ISub(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().isub(x, y)),
            Op::IMul(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().imul(x, y)),
            Op::SDiv(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().sdiv(x, y)),
            Op::UDiv(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().udiv(x, y)),
            Op::SMod(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().srem(x, y)),
            Op::UMod(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().urem(x, y)),
            Op::INeg(a) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("INeg without result".into()))?;
                let av = self.scalars.get(&a.id).copied().ok_or_else(||
                    BackendError::Internal(format!(
                        "INeg operand {:?} not in scalars", a.id)))?;
                let v = builder.ins().ineg(av);
                self.scalars.insert(result.id, v);
                Ok(())
            }
            // Bitwise + shifts. Shift amounts in Cranelift
            // are taken modulo the type width; our SPIR-V
            // shift ops have the same semantics for in-range
            // amounts (out-of-range is undefined in SPIR-V).
            Op::BitAnd(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().band(x, y)),
            Op::BitOr(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().bor(x, y)),
            Op::BitXor(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().bxor(x, y)),
            Op::BitNot(a) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("BitNot without result".into()))?;
                let av = self.scalars.get(&a.id).copied().ok_or_else(||
                    BackendError::Internal(format!(
                        "BitNot operand {:?} not in scalars", a.id)))?;
                let v = builder.ins().bnot(av);
                self.scalars.insert(result.id, v);
                Ok(())
            }
            Op::Shl(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().ishl(x, y)),
            Op::LShr(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().ushr(x, y)),
            Op::AShr(a, b) => self.emit_int_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().sshr(x, y)),
            // Int↔float conversions. Cranelift's
            // fcvt_to_sint/fcvt_to_uint trap on NaN; SPIR-V
            // says NaN→0 (saturated). Use the saturating
            // variants to match.
            Op::ConvertSToF(a) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConvertSToF without result".into()))?;
                let av = self.scalars.get(&a.id).copied().ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertSToF operand {:?} not in scalars", a.id)))?;
                let v = builder.ins().fcvt_from_sint(clif_types::F32, av);
                self.scalars.insert(result.id, v);
                Ok(())
            }
            Op::ConvertUToF(a) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConvertUToF without result".into()))?;
                let av = self.scalars.get(&a.id).copied().ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertUToF operand {:?} not in scalars", a.id)))?;
                let v = builder.ins().fcvt_from_uint(clif_types::F32, av);
                self.scalars.insert(result.id, v);
                Ok(())
            }
            Op::ConvertFToS(a) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConvertFToS without result".into()))?;
                let av = self.scalars.get(&a.id).copied().ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertFToS operand {:?} not in scalars", a.id)))?;
                let v = builder.ins().fcvt_to_sint_sat(clif_types::I32, av);
                self.scalars.insert(result.id, v);
                Ok(())
            }
            Op::ConvertFToU(a) => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal("ConvertFToU without result".into()))?;
                let av = self.scalars.get(&a.id).copied().ok_or_else(||
                    BackendError::Internal(format!(
                        "ConvertFToU operand {:?} not in scalars", a.id)))?;
                let v = builder.ins().fcvt_to_uint_sat(clif_types::I32, av);
                self.scalars.insert(result.id, v);
                Ok(())
            }
            // Integer comparisons → Bool (i32 0/1 per B4).
            Op::IEq(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::Equal),
            Op::INe(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::NotEqual),
            Op::SLt(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::SignedLessThan),
            Op::SLe(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::SignedLessThanOrEqual),
            Op::SGt(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::SignedGreaterThan),
            Op::SGe(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::SignedGreaterThanOrEqual),
            Op::ULt(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::UnsignedLessThan),
            Op::ULe(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::UnsignedLessThanOrEqual),
            Op::UGt(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::UnsignedGreaterThan),
            Op::UGe(a, b) => self.emit_icmp(
                builder, &inst.result, a, b, IntCC::UnsignedGreaterThanOrEqual),
            Op::FAdd(a, b) => self.emit_float_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().fadd(x, y),
            ),
            Op::FSub(a, b) => self.emit_float_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().fsub(x, y),
            ),
            Op::FMul(a, b) => self.emit_float_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().fmul(x, y),
            ),
            Op::FDiv(a, b) => self.emit_float_binop(
                builder, &inst.result, a, b, |b, x, y| b.ins().fdiv(x, y),
            ),
            Op::FNeg(a) => self.emit_float_unop(
                builder, &inst.result, a, |b, x| b.ins().fneg(x),
            ),
            // Float comparisons (constraint B4: result
            // is i32 0/1). Cranelift's fcmp produces an
            // I8 boolean; we uextend to I32 to match our
            // Bool storage convention.
            Op::FOrdEq(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::Equal),
            Op::FOrdNe(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::OrderedNotEqual),
            Op::FOrdLt(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::LessThan),
            Op::FOrdLe(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::LessThanOrEqual),
            Op::FOrdGt(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::GreaterThan),
            Op::FOrdGe(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::GreaterThanOrEqual),
            Op::FUnordEq(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::UnorderedOrEqual),
            Op::FUnordNe(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::NotEqual),
            Op::FUnordLt(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::UnorderedOrLessThan),
            Op::FUnordLe(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::UnorderedOrLessThanOrEqual),
            Op::FUnordGt(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::UnorderedOrGreaterThan),
            Op::FUnordGe(a, b) => self.emit_fcmp(builder, &inst.result, a, b, FloatCC::UnorderedOrGreaterThanOrEqual),

            // OpSelect: cond ? t : f. Cranelift's select
            // takes any integer ctrl; non-zero selects t.
            // Per constraint B4, Bool is i32 0/1, so a
            // direct pass through works.
            Op::Select { cond, t_val, f_val } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Select without result Value".to_string()))?;
                let cv = self.scalars.get(&cond.id).copied().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Select cond id {:?} not in scalars (per-lane vec \
                         cond not supported yet)", cond.id,
                    )))?;
                // Vec select: lane-walk if both t_val and
                // f_val are vectors.
                if let (Some(t_lanes), Some(f_lanes)) =
                    (self.vectors.get(&t_val.id).cloned(),
                     self.vectors.get(&f_val.id).cloned())
                {
                    if t_lanes.len() != f_lanes.len() {
                        return Err(BackendError::Unsupported(format!(
                            "Select vec lane-count mismatch: {} vs {}",
                            t_lanes.len(), f_lanes.len(),
                        )));
                    }
                    let mut out_lanes = Vec::with_capacity(t_lanes.len());
                    for (tl, fl) in t_lanes.iter().zip(f_lanes.iter()) {
                        out_lanes.push(builder.ins().select(cv, *tl, *fl));
                    }
                    self.vectors.insert(result.id, out_lanes);
                    return Ok(());
                }
                // Scalar select.
                let tv = self.scalars.get(&t_val.id).copied().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Select t_val id {:?} not in scalars", t_val.id,
                    )))?;
                let fv = self.scalars.get(&f_val.id).copied().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Select f_val id {:?} not in scalars", f_val.id,
                    )))?;
                let v = builder.ins().select(cv, tv, fv);
                self.scalars.insert(result.id, v);
                Ok(())
            }

            // Op::VectorShuffle: gather lanes from
            // src1 ++ src2 by per-output-lane index.
            // With our per-lane scalar storage this is
            // just a permutation — no Cranelift
            // instruction needed.
            Op::VectorShuffle { src1, src2, components } => {
                let s1_lanes = self.vectors.get(&src1.id).cloned()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "VectorShuffle src1 id {:?} not in vectors", src1.id,
                    )))?;
                let s2_lanes = self.vectors.get(&src2.id).cloned()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "VectorShuffle src2 id {:?} not in vectors", src2.id,
                    )))?;
                let combined_len = s1_lanes.len() + s2_lanes.len();
                let mut out_lanes = Vec::with_capacity(components.len());
                for c in components {
                    let idx = *c as usize;
                    // 0xFFFFFFFF is SPIR-V's "Undefined"
                    // sentinel. The renderer spec § B3
                    // mandates writing all lanes even when
                    // their content is undefined; we
                    // synthesise a zero constant for the
                    // slot so the output is still
                    // well-defined SSA.
                    if *c == 0xFFFF_FFFF {
                        let z = builder.ins().f32const(0.0f32);
                        out_lanes.push(z);
                        continue;
                    }
                    if idx >= combined_len {
                        return Err(BackendError::Unsupported(format!(
                            "VectorShuffle component {idx} out of range \
                             (combined source length {combined_len})",
                        )));
                    }
                    let lane = if idx < s1_lanes.len() {
                        s1_lanes[idx]
                    } else {
                        s2_lanes[idx - s1_lanes.len()]
                    };
                    out_lanes.push(lane);
                }
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "VectorShuffle without result Value".to_string()))?;
                self.vectors.insert(result.id, out_lanes);
                Ok(())
            }

            // Op::VectorExtract: pull lane `index` out of
            // a vector into a scalar. Pure indexing with
            // our per-lane storage.
            Op::VectorExtract { vector, index } => {
                let lanes = self.vectors.get(&vector.id).cloned()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "VectorExtract source {:?} not in vectors",
                        vector.id)))?;
                let lane = lanes.get(*index as usize).copied()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "VectorExtract index {index} out of range \
                         (vector has {} lanes)", lanes.len())))?;
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "VectorExtract without result Value".to_string()))?;
                self.scalars.insert(result.id, lane);
                Ok(())
            }

            // Op::Dot: per-lane fmul, then tree-reduce
            // with fadd. Result is a scalar.
            Op::Dot(a, b) => {
                let a_lanes = self.vectors.get(&a.id).cloned()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "Dot lhs id {:?} not in vectors", a.id,
                    )))?;
                let b_lanes = self.vectors.get(&b.id).cloned()
                    .ok_or_else(|| BackendError::Unsupported(format!(
                        "Dot rhs id {:?} not in vectors", b.id,
                    )))?;
                if a_lanes.len() != b_lanes.len() {
                    return Err(BackendError::Unsupported(format!(
                        "Dot with mismatched lane counts: {} vs {}",
                        a_lanes.len(), b_lanes.len(),
                    )));
                }
                if a_lanes.is_empty() {
                    return Err(BackendError::Unsupported(
                        "Dot on zero-lane vectors".to_string(),
                    ));
                }
                let mut acc = builder.ins().fmul(a_lanes[0], b_lanes[0]);
                for (la, lb) in a_lanes.iter().zip(b_lanes.iter()).skip(1) {
                    let prod = builder.ins().fmul(*la, *lb);
                    acc = builder.ins().fadd(acc, prod);
                }
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "Dot without result Value".to_string()))?;
                self.scalars.insert(result.id, acc);
                Ok(())
            }
            // ── Image / sampler ────────────────────────────
            //
            // ImageHandle and CombineSampledImage emit zero
            // native instructions — they're metadata moves
            // tracking which descriptor slot a Value points
            // at, so the eventual ImageSample call site can
            // compute the right uniforms offset.
            Op::ImageHandle { set, binding } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageHandle without result Value".to_string()))?;
                self.image_handles.insert(result.id, (*set, *binding));
                Ok(())
            }
            Op::CombineSampledImage { image, sampler: _ } => {
                // v1 keeps sampler config inside each
                // TextureBinding (one descriptor slot per
                // combined sampler+image), so we propagate
                // the image operand's (set, binding) and
                // ignore the sampler operand's — the
                // descriptor slot at this binding carries
                // both `tex_desc*` and `samp_desc*` already.
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "CombineSampledImage without result Value".to_string()))?;
                let h = self.image_handles.get(&image.id).copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "CombineSampledImage image operand {:?} not an \
                         ImageHandle", image.id)))?;
                self.image_handles.insert(result.id, h);
                Ok(())
            }
            // ImageSampleImplicitLod: emit the four-instruction
            // call-via-pointer sequence the v1 descriptor ABI
            // specifies (atrium-spv-runtime
            // `UNIFORMS_HELPERS_BASE` / `UNIFORMS_DESC_BASE`):
            //   x_fn   = *(uniforms + 0)
            //   x_tex  = *(uniforms + 16 + B*16 + 0)
            //   x_samp = *(uniforms + 16 + B*16 + 8)
            //   blr  x_fn  (call atrium_tex_sample_2d via fn-ptr)
            // plus a 16-byte stack slot for the out_rgba
            // pixel, which we then load lane-by-lane.
            Op::ImageSampleImplicitLod { sampled_image, coord } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageSampleImplicitLod without result".to_string()))?;
                let (_, binding) = self.image_handles.get(&sampled_image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageSampleImplicitLod sampled_image {:?} not an \
                         ImageHandle", sampled_image.id)))?;
                let coord_lanes = self.vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageSampleImplicitLod coord {:?} not a vector",
                        coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "ImageSampleImplicitLod 2D coord must have ≥2 lanes, \
                         got {}", coord_lanes.len())));
                }
                let u = coord_lanes[0];
                let v = coord_lanes[1];

                let pointer_type = builder.func.dfg
                    .value_type(self.params[1]); // uniforms ptr
                let uniforms = self.params[1];

                // Load helper fn pointer + descriptor slot
                // pointers from the uniforms buffer at the
                // v1-ABI offsets.
                let desc_off: i32 = 16 + (binding as i32) * 16;
                let fn_ptr = builder.ins().load(
                    pointer_type, MemFlags::new(), uniforms, 0);
                let tex_ptr = builder.ins().load(
                    pointer_type, MemFlags::new(), uniforms, desc_off);
                let samp_ptr = builder.ins().load(
                    pointer_type, MemFlags::new(), uniforms, desc_off + 8);

                // 16-byte stack slot for the out_rgba pixel
                // (4 f32 lanes, 4-byte aligned is the f32
                // requirement; 16 is over-aligned but
                // matches the Q-register convention the
                // bespoke backend uses).
                let slot = builder.create_sized_stack_slot(
                    StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
                let out_ptr = builder.ins().stack_addr(pointer_type, slot, 0);

                // void atrium_tex_sample_2d(
                //   const TexDesc*, const SamplerDesc*,
                //   f32 u, f32 v, f32 *out_rgba);
                let mut call_sig = Signature::new(CallConv::SystemV);
                call_sig.params.push(AbiParam::new(pointer_type));
                call_sig.params.push(AbiParam::new(pointer_type));
                call_sig.params.push(AbiParam::new(clif_types::F32));
                call_sig.params.push(AbiParam::new(clif_types::F32));
                call_sig.params.push(AbiParam::new(pointer_type));
                let sig_ref = builder.import_signature(call_sig);
                builder.ins().call_indirect(
                    sig_ref, fn_ptr,
                    &[tex_ptr, samp_ptr, u, v, out_ptr]);

                // Read the four pixel lanes back out as f32s.
                let mut lanes = Vec::with_capacity(4);
                for i in 0..4i32 {
                    let l = builder.ins().load(
                        clif_types::F32, MemFlags::new(), out_ptr, i * 4);
                    lanes.push(l);
                }
                self.vectors.insert(result.id, lanes);
                Ok(())
            }
            // ImageFetch — unfiltered integer-coord texel
            // load. v1 ABI: helper fn pointer at
            // uniforms[8] (the fetch slot), tex_desc* at
            // the descriptor slot for this binding;
            // sampler ignored. Args by AAPCS64 / SystemV:
            //   atrium_tex_fetch_2d(
            //     const TexDesc*,
            //     int32_t x, int32_t y, int32_t lod,
            //     float *out_rgba);
            // OpMatrixTimesVector — column-major: each
            // result lane `i = Σ matrix[j][i] * vector[j]`.
            // Lowered to 4 vec×scalar broadcasts + 3 vec
            // adds, all using existing FMul/FAdd Cranelift
            // ops. The matrix value lives in `matrices`
            // (4 column vec4s); the vector in `vectors`
            // (4 f32 lanes).
            Op::MatrixTimesVector { matrix, vector } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "MatrixTimesVector without result".to_string()))?;
                let columns = self.matrices.get(&matrix.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "MatrixTimesVector matrix {:?} not in matrices",
                        matrix.id)))?;
                let vec_lanes = self.vectors.get(&vector.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "MatrixTimesVector vector {:?} not in vectors",
                        vector.id)))?;
                if columns.len() != vec_lanes.len() {
                    return Err(BackendError::Unsupported(format!(
                        "MatrixTimesVector: {} columns × {}-lane vector",
                        columns.len(), vec_lanes.len())));
                }
                let n_lanes = columns[0].len();
                let mut out_lanes: Vec<ClifValue> = (0..n_lanes)
                    .map(|i| {
                        let m = columns[0][i];
                        builder.ins().fmul(m, vec_lanes[0])
                    })
                    .collect();
                for j in 1..columns.len() {
                    let vj = vec_lanes[j];
                    for i in 0..n_lanes {
                        let prod = builder.ins().fmul(columns[j][i], vj);
                        out_lanes[i] = builder.ins().fadd(out_lanes[i], prod);
                    }
                }
                self.vectors.insert(result.id, out_lanes);
                Ok(())
            }
            Op::ImageFetch { image, coord, lod } => {
                let result = inst.result.as_ref().ok_or_else(||
                    BackendError::Internal(
                        "ImageFetch without result".to_string()))?;
                let (_, binding) = self.image_handles.get(&image.id)
                    .copied()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageFetch image {:?} not an ImageHandle",
                        image.id)))?;
                let coord_lanes = self.vectors.get(&coord.id).cloned()
                    .ok_or_else(|| BackendError::Internal(format!(
                        "ImageFetch coord {:?} not a vector", coord.id)))?;
                if coord_lanes.len() < 2 {
                    return Err(BackendError::Unsupported(format!(
                        "ImageFetch 2D coord must have ≥2 lanes, \
                         got {}", coord_lanes.len())));
                }
                let x = coord_lanes[0];
                let y = coord_lanes[1];
                // Optional explicit lod operand; v1 passes
                // 0 if absent. The runtime ignores it for
                // now (mip-0 only).
                let lod_val = match lod {
                    Some(lv) => *self.scalars.get(&lv.id)
                        .ok_or_else(|| BackendError::Internal(format!(
                            "ImageFetch lod {:?} not in scalars", lv.id)))?,
                    None => builder.ins().iconst(clif_types::I32, 0),
                };

                let pointer_type = builder.func.dfg
                    .value_type(self.params[1]);
                let uniforms = self.params[1];

                let desc_off: i32 = 16 + (binding as i32) * 16;
                let fn_ptr = builder.ins().load(
                    pointer_type, MemFlags::new(), uniforms, 8); // fetch slot
                let tex_ptr = builder.ins().load(
                    pointer_type, MemFlags::new(), uniforms, desc_off);

                let slot = builder.create_sized_stack_slot(
                    StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
                let out_ptr = builder.ins().stack_addr(pointer_type, slot, 0);

                let mut call_sig = Signature::new(CallConv::SystemV);
                call_sig.params.push(AbiParam::new(pointer_type));
                call_sig.params.push(AbiParam::new(clif_types::I32));
                call_sig.params.push(AbiParam::new(clif_types::I32));
                call_sig.params.push(AbiParam::new(clif_types::I32));
                call_sig.params.push(AbiParam::new(pointer_type));
                let sig_ref = builder.import_signature(call_sig);
                builder.ins().call_indirect(
                    sig_ref, fn_ptr,
                    &[tex_ptr, x, y, lod_val, out_ptr]);

                let mut lanes = Vec::with_capacity(4);
                for i in 0..4i32 {
                    let l = builder.ins().load(
                        clif_types::F32, MemFlags::new(), out_ptr, i * 4);
                    lanes.push(l);
                }
                self.vectors.insert(result.id, lanes);
                Ok(())
            }
            other => Err(BackendError::Unsupported(format!(
                "Op {other:?} not supported in phase 2 v6",
            ))),
        }
    }

    /// Helper for f32 binary arithmetic that may be
    /// scalar or vector. SPIR-V's OpFAdd / OpFSub /
    /// OpFMul / OpFDiv are polymorphic by operand type;
    /// we dispatch on whether both operands live in the
    /// vectors map (lane-walk) or both in scalars (single
    /// op). Mixed-shape operands aren't allowed by SPIR-V.
    fn emit_float_binop(
        &mut self,
        builder: &mut FunctionBuilder,
        result: &Option<atrium_spv_ir::Value>,
        a: &atrium_spv_ir::Value,
        b: &atrium_spv_ir::Value,
        emit: impl Fn(&mut FunctionBuilder, ClifValue, ClifValue) -> ClifValue,
    ) -> Result<(), BackendError> {
        let result = result.as_ref().ok_or_else(||
            BackendError::Internal(
                "binop without result Value".to_string()))?;

        // Vec → vec.
        if let (Some(a_lanes), Some(b_lanes)) =
            (self.vectors.get(&a.id).cloned(), self.vectors.get(&b.id).cloned())
        {
            if a_lanes.len() != b_lanes.len() {
                return Err(BackendError::Unsupported(format!(
                    "vec binop with mismatched lane counts: {} vs {}",
                    a_lanes.len(), b_lanes.len(),
                )));
            }
            let mut out_lanes = Vec::with_capacity(a_lanes.len());
            for (la, lb) in a_lanes.iter().zip(b_lanes.iter()) {
                out_lanes.push(emit(builder, *la, *lb));
            }
            self.vectors.insert(result.id, out_lanes);
            return Ok(());
        }

        // Vec × scalar (broadcast). OpVectorTimesScalar
        // lowers through here: SPIR-V puts the vec
        // first, scalar second.
        if let (Some(a_lanes), Some(b_scalar)) =
            (self.vectors.get(&a.id).cloned(), self.scalars.get(&b.id).copied())
        {
            let mut out_lanes = Vec::with_capacity(a_lanes.len());
            for la in a_lanes.iter() {
                out_lanes.push(emit(builder, *la, b_scalar));
            }
            self.vectors.insert(result.id, out_lanes);
            return Ok(());
        }

        // Scalar × vec (broadcast, commutative for our
        // supported ops). Symmetric case for safety.
        if let (Some(a_scalar), Some(b_lanes)) =
            (self.scalars.get(&a.id).copied(), self.vectors.get(&b.id).cloned())
        {
            let mut out_lanes = Vec::with_capacity(b_lanes.len());
            for lb in b_lanes.iter() {
                out_lanes.push(emit(builder, a_scalar, *lb));
            }
            self.vectors.insert(result.id, out_lanes);
            return Ok(());
        }

        // Scalar.
        let av = self.scalars.get(&a.id).copied().ok_or_else(||
            BackendError::Unsupported(format!(
                "float binop lhs id {:?} not in scalars or vectors", a.id,
            )))?;
        let bv = self.scalars.get(&b.id).copied().ok_or_else(||
            BackendError::Unsupported(format!(
                "float binop rhs id {:?} not in scalars or vectors", b.id,
            )))?;
        let v = emit(builder, av, bv);
        self.scalars.insert(result.id, v);
        Ok(())
    }

    /// Emit a float-comparison instruction. Polymorphic
    /// by operand shape (scalar/vec). Result is Bool
    /// (constraint B4 == i32 0/1). Cranelift's fcmp
    /// produces an I8 boolean which we uextend to I32.
    /// For vec inputs, emits N fcmps and stores the
    /// uextend'd bools as a vec<i32>.
    /// Emit a scalar integer binary op (iadd / isub /
    /// imul / sdiv / etc.). Operands looked up in
    /// self.scalars; result written there.
    fn emit_int_binop(
        &mut self,
        builder: &mut FunctionBuilder,
        result: &Option<atrium_spv_ir::Value>,
        a: &atrium_spv_ir::Value,
        b: &atrium_spv_ir::Value,
        emit: impl FnOnce(&mut FunctionBuilder, ClifValue, ClifValue) -> ClifValue,
    ) -> Result<(), BackendError> {
        let result = result.as_ref().ok_or_else(||
            BackendError::Internal("int binop without result".into()))?;
        let av = self.scalars.get(&a.id).copied().ok_or_else(||
            BackendError::Internal(format!(
                "int binop lhs {:?} not in scalars", a.id)))?;
        let bv = self.scalars.get(&b.id).copied().ok_or_else(||
            BackendError::Internal(format!(
                "int binop rhs {:?} not in scalars", b.id)))?;
        let v = emit(builder, av, bv);
        self.scalars.insert(result.id, v);
        Ok(())
    }

    /// Emit a scalar integer comparison; Cranelift's icmp
    /// returns I8 boolean which we uextend to I32 per
    /// constraint B4.
    fn emit_icmp(
        &mut self,
        builder: &mut FunctionBuilder,
        result: &Option<atrium_spv_ir::Value>,
        a: &atrium_spv_ir::Value,
        b: &atrium_spv_ir::Value,
        cc: IntCC,
    ) -> Result<(), BackendError> {
        let result = result.as_ref().ok_or_else(||
            BackendError::Internal("icmp without result".into()))?;
        let av = self.scalars.get(&a.id).copied().ok_or_else(||
            BackendError::Internal(format!(
                "icmp lhs {:?} not in scalars", a.id)))?;
        let bv = self.scalars.get(&b.id).copied().ok_or_else(||
            BackendError::Internal(format!(
                "icmp rhs {:?} not in scalars", b.id)))?;
        let bool_v = builder.ins().icmp(cc, av, bv);
        let i32_v = builder.ins().uextend(clif_types::I32, bool_v);
        self.scalars.insert(result.id, i32_v);
        Ok(())
    }

    fn emit_fcmp(
        &mut self,
        builder: &mut FunctionBuilder,
        result: &Option<atrium_spv_ir::Value>,
        a: &atrium_spv_ir::Value,
        b: &atrium_spv_ir::Value,
        cc: FloatCC,
    ) -> Result<(), BackendError> {
        let result = result.as_ref().ok_or_else(||
            BackendError::Internal(
                "fcmp without result Value".to_string()))?;

        // Vec × vec.
        if let (Some(a_lanes), Some(b_lanes)) =
            (self.vectors.get(&a.id).cloned(), self.vectors.get(&b.id).cloned())
        {
            if a_lanes.len() != b_lanes.len() {
                return Err(BackendError::Unsupported(format!(
                    "vec fcmp with mismatched lane counts: {} vs {}",
                    a_lanes.len(), b_lanes.len(),
                )));
            }
            let mut out_lanes = Vec::with_capacity(a_lanes.len());
            for (la, lb) in a_lanes.iter().zip(b_lanes.iter()) {
                let bool_v = builder.ins().fcmp(cc, *la, *lb);
                let i32_v = builder.ins().uextend(clif_types::I32, bool_v);
                out_lanes.push(i32_v);
            }
            self.vectors.insert(result.id, out_lanes);
            return Ok(());
        }

        // Scalar.
        let av = self.scalars.get(&a.id).copied().ok_or_else(||
            BackendError::Unsupported(format!(
                "fcmp lhs id {:?} not in scalars or vectors", a.id,
            )))?;
        let bv = self.scalars.get(&b.id).copied().ok_or_else(||
            BackendError::Unsupported(format!(
                "fcmp rhs id {:?} not in scalars or vectors", b.id,
            )))?;
        let bool_v = builder.ins().fcmp(cc, av, bv);
        let i32_v = builder.ins().uextend(clif_types::I32, bool_v);
        self.scalars.insert(result.id, i32_v);
        Ok(())
    }

    /// Same shape as [`Self::emit_float_binop`] for unary
    /// ops (FNeg).
    fn emit_float_unop(
        &mut self,
        builder: &mut FunctionBuilder,
        result: &Option<atrium_spv_ir::Value>,
        a: &atrium_spv_ir::Value,
        emit: impl Fn(&mut FunctionBuilder, ClifValue) -> ClifValue,
    ) -> Result<(), BackendError> {
        let result = result.as_ref().ok_or_else(||
            BackendError::Internal(
                "unop without result Value".to_string()))?;
        if let Some(a_lanes) = self.vectors.get(&a.id).cloned() {
            let mut out_lanes = Vec::with_capacity(a_lanes.len());
            for la in a_lanes.iter() {
                out_lanes.push(emit(builder, *la));
            }
            self.vectors.insert(result.id, out_lanes);
            return Ok(());
        }
        let av = self.scalars.get(&a.id).copied().ok_or_else(||
            BackendError::Unsupported(format!(
                "float unop operand id {:?} not in scalars or vectors", a.id,
            )))?;
        let v = emit(builder, av);
        self.scalars.insert(result.id, v);
        Ok(())
    }

    /// Look up or materialise the (base param, byte
    /// offset) repr for a pointer-typed IR Value.
    ///
    /// If the Value already has a repr in `self.pointers`
    /// (set by a prior OpAccessChain), return it.
    /// Otherwise the Value is a bare Variable: derive the
    /// param from its storage class and seed offset=0.
    fn resolve_or_make_pointer(
        &mut self,
        v: &atrium_spv_ir::Value,
    ) -> Result<(ClifValue, i32), BackendError> {
        if let Some(p) = self.pointers.get(&v.id) { return Ok(*p); }
        let param = self.resolve_pointer_param(&v.ty, v.id.0)?;
        self.pointers.insert(v.id, (param, 0));
        Ok((param, 0))
    }

    /// Map a storage-class pointer type to its Cranelift
    /// function-parameter value. The shader-ABI per stage
    /// puts the per-storage pointer at a fixed param
    /// index.  `var_id` is the IR ValueId of the underlying
    /// Variable; for multi-binding compute (where SSBO
    /// pointers were pre-loaded by the entry-block prologue
    /// into `ssbo_var_ptr`) this picks the right per-binding
    /// loaded pointer instead of the params[2] direct path.
    fn resolve_pointer_param(&self, ty: &Type, var_id: u32) -> Result<ClifValue, BackendError> {
        // Multi-binding compute SSBO short-circuit.
        if matches!(self.stage, ShaderStage::Compute) {
            if let Some(p) = self.ssbo_var_ptr.get(&var_id) {
                return Ok(*p);
            }
        }
        let storage = match ty {
            Type::Pointer(sc, _) => sc,
            other => return Err(BackendError::Unsupported(format!(
                "Store target is not a Pointer type: {other:?}",
            ))),
        };
        match (self.stage, storage) {
            // Fragment-stage param order (build_signature):
            //   0 in_varyings, 1 uniforms, 2 push_constants,
            //   3..7 frag_coord (4 f32), 7 samples_mask,
            //   8 out_color, 9 out_depth.
            // Wait — that's 10 params total. samples_mask
            // is at index 7, out_color at 8, out_depth at 9.
            // build_signature puts frag_coord across indices
            // 3, 4, 5, 6; then samples_mask at 7; out_color
            // at 8; out_depth at 9. Match that here.
            (ShaderStage::Fragment, StorageClass::Output) => {
                Ok(self.params[8])
            }
            (ShaderStage::Fragment, StorageClass::Input) => {
                // in_varyings (parameter 0)
                Ok(self.params[0])
            }
            (ShaderStage::Fragment, StorageClass::Uniform) => {
                Ok(self.params[1])
            }
            (ShaderStage::Fragment, StorageClass::PushConstant) => {
                Ok(self.params[2])
            }
            // Vertex-stage param order (build_signature):
            //   0 in_attributes, 1 in_attr_strides, 2 uniforms,
            //   3 push_constants, 4 vertex_index, 5 instance_index,
            //   6 out_position, 7 out_varyings, 8 out_clip_distance.
            (ShaderStage::Vertex, StorageClass::Input) => Ok(self.params[0]),
            (ShaderStage::Vertex, StorageClass::Uniform) => Ok(self.params[2]),
            (ShaderStage::Vertex, StorageClass::PushConstant) => Ok(self.params[3]),
            // v1 maps Vertex Output → out_position (param
            // 6) on the assumption the shader only writes
            // gl_Position. A real shader with both
            // gl_Position AND Location-decorated varyings
            // needs richer dispatch (look at the variable's
            // BuiltIn vs Location decoration to choose
            // out_position vs out_varyings); landing that
            // is queued for vertex phase 4+.
            (ShaderStage::Vertex, StorageClass::Output) => Ok(self.params[6]),
            // Compute (params: uniforms, push, out_buffer,
            //          workgroup_id*3, local_id*3)
            (ShaderStage::Compute, StorageClass::Uniform) => Ok(self.params[0]),
            (ShaderStage::Compute, StorageClass::PushConstant) => Ok(self.params[1]),
            (ShaderStage::Compute, StorageClass::StorageBuffer) => Ok(self.params[2]),
            (stage, sc) => Err(BackendError::Unsupported(format!(
                "no param mapping for stage={stage:?}, storage={sc:?}",
            ))),
        }
    }
}

// Silence the unused-import warning until we use clif_types
// in a future phase.
#[allow(dead_code)]
fn _types_placeholder() -> cranelift_codegen::ir::Type { clif_types::F32 }

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
            //                out_buffer,
            //                workgroup_id[3], local_id[3])
            params.push(AbiParam::new(pointer_type)); // uniforms
            params.push(AbiParam::new(pointer_type)); // push_constants
            params.push(AbiParam::new(pointer_type)); // out_buffer (SSBO)
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
