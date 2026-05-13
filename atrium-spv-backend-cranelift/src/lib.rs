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

use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{
    AbiParam, Function as ClifFunction, InstBuilder, MemFlags, Signature, UserFuncName,
    Value as ClifValue,
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
        let first_spirv_offset = module.functions
            .iter()
            .find(|f| f.name == func.name)
            .and_then(|f| f.blocks.get(&f.entry_block))
            .and_then(|b| b.insts.first())
            .map(|i| i.source_spirv_offset)
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

        let mut translator = FnTranslator {
            stage: func.stage,
            entry_block: entry,
            // Cache the param-index → Cranelift value
            // mapping once; ip-walk reads it for ptr-arg
            // resolution.
            params: builder.func.dfg.block_params(entry).to_vec(),
            scalars: HashMap::new(),
            vectors: HashMap::new(),
            pointers: HashMap::new(),
        };

        // Single-block IR per phase 1 v1.
        let block = func.blocks.get(&func.entry_block).ok_or_else(||
            BackendError::Internal("entry block missing".to_string()))?;
        for inst in &block.insts {
            translator.emit_inst(inst, &mut builder)?;
        }

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
}

impl FnTranslator {
    fn emit_inst(
        &mut self,
        inst: &atrium_spv_ir::Inst,
        builder: &mut FunctionBuilder,
    ) -> Result<(), BackendError> {
        match &inst.op {
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
                // Resolve pointer: only Output / Input /
                // Uniform / PushConstant pointers from
                // the SPIR-V are valid in phase 2 v2;
                // they reroute to the corresponding
                // function parameter.
                let ptr_param = self.resolve_pointer_param(&ptr.ty)?;
                // Resolve value: it must be a vector for
                // the constant-color case (vec4<f32>).
                let lanes = self.vectors.get(&value.id).cloned().ok_or_else(||
                    BackendError::Unsupported(format!(
                        "Op::Store value is not a vector in scalars/vectors maps; \
                         only vec stores supported in phase 2 v2 (value id {:?})",
                        value.id,
                    )))?;
                // Emit one store per lane at consecutive
                // 4-byte offsets. Per constraint B3 we
                // write all lanes (including vec3's high
                // lane is undefined and we don't have
                // vec3 in phase 2 v2 anyway).
                let bytes_per_lane = 4;
                for (i, lane) in lanes.iter().enumerate() {
                    let offset = (i as i32) * (bytes_per_lane as i32);
                    builder.ins().store(MemFlags::new(), *lane, ptr_param, offset);
                }
                Ok(())
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
                    other => return Err(BackendError::Unsupported(format!(
                        "Op::Load of pointee type {other:?} not supported",
                    ))),
                }
                Ok(())
            }
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
        let param = self.resolve_pointer_param(&v.ty)?;
        self.pointers.insert(v.id, (param, 0));
        Ok((param, 0))
    }

    /// Map a storage-class pointer type to its Cranelift
    /// function-parameter value. The shader-ABI per stage
    /// puts the per-storage pointer at a fixed param
    /// index.
    fn resolve_pointer_param(&self, ty: &Type) -> Result<ClifValue, BackendError> {
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
            (ShaderStage::Vertex, StorageClass::Output) => Ok(self.params[7]),
            // Compute (params: uniforms, push, workgroup_id*3, local_id*3)
            (ShaderStage::Compute, StorageClass::Uniform) => Ok(self.params[0]),
            (ShaderStage::Compute, StorageClass::PushConstant) => Ok(self.params[1]),
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
