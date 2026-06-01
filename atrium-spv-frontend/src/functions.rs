//! Function body translation.
//!
//! Walks each SPIR-V function's instructions and emits
//! the corresponding atrium-spv-ir [`Function`] structure.
//! Maintains an `id_map` from SPIR-V id → IR
//! [`ValueId`] so all SSA references stay consistent.

use std::collections::HashMap;

use atrium_spv_ir::{
    Block, BlockId, BlockKind, Function, Inst, Op, ShaderStage, Type, Value,
    ValueId, VecElement,
};
use rspirv::dr::{Function as SpvFunction, Instruction, Module, Operand};
use rspirv::spirv::{Op as SpvOp, Word};

use crate::cfg;
use crate::constants::{ConstantContext, ConstantKind};
use crate::error::FrontendError;
use crate::interface::InterfaceContext;
use crate::offsets::OffsetTable;
use crate::types::TypeContext;

/// Translate every function in the module.
///
/// Each function becomes one [`Function`] in the returned
/// `Vec`. Order matches the SPIR-V module's function order
/// (so an entry-point's `function_index` is a direct lookup
/// against this Vec).
///
/// `offsets` + `function_start_indices` thread the source-
/// SPIR-V byte offset of each instruction through to the
/// IR via Inst::source_spirv_offset (constraint A2).
pub fn translate_all(
    module: &Module,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    offsets: &OffsetTable,
    function_start_indices: &[usize],
) -> Result<Vec<Function>, FrontendError> {
    let mut out = Vec::with_capacity(module.functions.len());
    for (idx, func) in module.functions.iter().enumerate() {
        let start = function_start_indices.get(idx).copied().unwrap_or(0);
        let translated = translate_one(func, types, constants, iface, offsets, start)?;
        out.push(translated);
    }
    // Patch entry_point.function_index now that we've laid
    // out the function vector. The interface pass left them
    // as 0 placeholders.
    // We can't mutate `iface` (it's borrowed read-only),
    // so we patch in the top-level translate() after this
    // returns — but to do that the caller needs the
    // (function_id → vec_index) mapping. Build + return it
    // via a side channel.
    //
    // Actually we DO get the mapping for free: the SPIR-V
    // function order matches our output order, and
    // iface.entry_function_ids maps SPIR-V function id →
    // entry-point index. So in lib.rs we'll patch by
    // walking module.functions in order and looking up.
    //
    // Doing it here would tangle ownership; defer.
    Ok(out)
}

fn translate_one(
    spv: &SpvFunction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    offsets: &OffsetTable,
    fn_start_index: usize,
) -> Result<Function, FrontendError> {
    // Classify all blocks up front; this also validates the
    // SPIR-V is structured (Op{Branch,Switch}Conditional
    // must follow Op{Selection,Loop}Merge).
    let classification = cfg::classify(spv)?;

    let def = spv.def.as_ref().ok_or_else(|| FrontendError::Malformed(
        "function missing OpFunction def".to_string()))?;
    let fn_id = def.result_id.ok_or_else(|| FrontendError::Malformed(
        "OpFunction missing result id".to_string()))?;
    let return_ty_id = def.result_type.ok_or_else(|| FrontendError::Malformed(
        "OpFunction missing result type".to_string()))?;
    let return_type = types.get(return_ty_id)?.clone();

    let stage = iface.entry_function_ids.get(&fn_id)
        .and_then(|i| iface.entry_points.get(*i))
        .map(|ep| ep.stage)
        .unwrap_or(ShaderStage::Fragment); // helpers inherit; arbitrary for now

    let name = iface.entry_function_ids.get(&fn_id)
        .and_then(|i| iface.entry_points.get(*i))
        .map(|ep| ep.name.clone())
        .unwrap_or_else(|| format!("fn_{fn_id}"));

    // ── Walk blocks ─────────────────────────────────────────
    //
    // Build a SPIR-V-label-id → IR BlockId map first so
    // terminators emitted in this pass can reference the
    // right target.
    let mut id_map: HashMap<Word, Value> = HashMap::new();
    let mut next_value_id: u32 = 0;
    let mut blocks: HashMap<BlockId, Block> = HashMap::new();
    // SPIR-V id → pointee SPIR-V type id for every pointer
    // produced by an OpAccessChain.  Lets a *chained*
    // AccessChain (base = a prior AccessChain result, e.g.
    // `instances[slot].member`) recover where its base points
    // so it can keep walking struct members.
    let mut ptr_pointee: HashMap<Word, Word> = HashMap::new();

    let mut label_to_block_id: HashMap<Word, BlockId> = HashMap::new();
    for (i, spv_block) in spv.blocks.iter().enumerate() {
        let label_id = spv_block.label.as_ref().and_then(|l| l.result_id)
            .ok_or_else(|| FrontendError::Malformed(
                "block without OpLabel".to_string()))?;
        label_to_block_id.insert(label_id, BlockId(i as u32));
    }

    // OffsetTable index starts at the first instruction
    // after OpFunction + parameters; we increment as we
    // walk every instruction (including OpLabels) so the
    // table stays aligned with the source stream.
    let mut spv_inst_index = fn_start_index
        + 1
        + spv.parameters.len();

    // Pre-allocate IR Values for every function-local
    // result_id, so forward references (e.g. a loop
    // header's OpPhi arm referring to a value computed
    // in the continue-block on the back-edge) can be
    // resolved during translation without the defining
    // instruction having been visited yet.
    for spv_block in &spv.blocks {
        for inst in &spv_block.instructions {
            let Some(rid) = inst.result_id else { continue };
            if id_map.contains_key(&rid) { continue; }
            let ty = match inst.result_type {
                Some(tid) => types.get(tid).cloned().unwrap_or(Type::Void),
                None => Type::Void,
            };
            let _ = alloc_or_get_result(
                rid, ty, &mut id_map, &mut next_value_id);
        }
    }

    // Pre-materialise every constant in the entry block so
    // SSA references from later blocks remain dominated.
    // Without this hoist, a constant first referenced
    // inside one branch can be re-used from a sibling
    // branch, violating Cranelift's dominance verifier.
    let mut entry_prelude: Vec<Inst> = Vec::new();
    {
        // Collect ids first to avoid borrowing `constants`
        // while we recurse into resolve_value.
        let const_ids: Vec<Word> = constants.iter().map(|(id, _)| *id).collect();
        for cid in const_ids {
            // Use offset 0 — these instructions don't map
            // to any specific source SPIR-V byte. (The
            // ConstFloat / ConstVec emissions don't carry
            // user-visible offsets anyway.)
            let _ = resolve_value(
                cid, types, constants, &mut id_map,
                &mut next_value_id, &mut entry_prelude, 0,
            );
            // Errors here (e.g. a constant whose type isn't
            // representable) are ignored — the constant
            // will fail later if actually referenced, with
            // a more localised diagnostic.
        }
    }

    for (i, spv_block) in spv.blocks.iter().enumerate() {
        let label_id = spv_block.label.as_ref().and_then(|l| l.result_id)
            .ok_or_else(|| FrontendError::Malformed(
                "block without OpLabel".to_string()))?;
        let block_id = BlockId(i as u32);
        // Step past the OpLabel itself.
        if spv_block.label.is_some() { spv_inst_index += 1; }

        // For the entry block, seed insts with the pre-
        // materialised constants so all later SSA refs are
        // dominated.
        let mut insts: Vec<Inst> = if i == 0 {
            std::mem::take(&mut entry_prelude)
        } else {
            Vec::new()
        };
        let block_kind = classification.get(label_id)
            .cloned()
            .unwrap_or(BlockKind::Linear);

        for spv_inst in &spv_block.instructions {
            let source_offset = offsets.get(spv_inst_index);
            translate_inst_with_cfg(
                spv_inst,
                types,
                constants,
                iface,
                &label_to_block_id,
                &mut id_map,
                &mut next_value_id,
                &mut insts,
                &mut ptr_pointee,
                source_offset,
            )?;
            spv_inst_index += 1;
        }

        blocks.insert(block_id, Block {
            id: block_id,
            kind: block_kind,
            insts,
        });
    }

    let entry_block_id = label_to_block_id
        .get(&spv.blocks.first()
             .and_then(|b| b.label.as_ref().and_then(|l| l.result_id))
             .unwrap_or(0))
        .copied()
        .unwrap_or(BlockId(0));

    let local_size = iface.local_sizes.get(&fn_id).copied();

    // Surface (set, binding) for any SSBO Variables this
    // function references.  Variables get a stable ValueId
    // through `resolve_variable`; we re-walk the same
    // mapping here so the backend can look up the binding
    // by ValueId without needing the SPIR-V Word.
    let mut ssbo_bindings: HashMap<u32, (u32, u32)> = HashMap::new();
    let mut workgroup_var_offset: HashMap<ValueId, u32> = HashMap::new();
    let mut output_varying_byte_offset: HashMap<ValueId, u32> = HashMap::new();
    let mut input_varying_byte_offset: HashMap<ValueId, u32> = HashMap::new();
    let mut frag_depth_output: Option<ValueId> = None;
    for (spv_var_id, value) in &id_map {
        // Only true SSBOs land in ssbo_bindings.  Storage
        // images and samplers share the (set, binding) map
        // (`iface.var_binding`) but are NOT descriptor-table
        // slots from the cs_main ABI's point of view -- the
        // image-table goes through X0 (uniforms), not X2
        // (ssbo).  Filtering by `iface.storage_buffer_vars`
        // avoids cranelift's "multi-binding prologue" mis-
        // firing when a shader has 1 SSBO + 1 storage image.
        if iface.storage_buffer_vars.contains(spv_var_id) {
            if let Some(&(set, binding)) = iface.var_binding.get(spv_var_id) {
                ssbo_bindings.insert(value.id.0, (set, binding));
            }
        }
        if let Some(&off) = iface.workgroup_var_offset.get(spv_var_id) {
            workgroup_var_offset.insert(value.id, off);
        }
        if let Some(&off) = iface.output_varying_byte_offset.get(spv_var_id) {
            output_varying_byte_offset.insert(value.id, off);
        }
        if let Some(&off) = iface.input_varying_byte_offset.get(spv_var_id) {
            input_varying_byte_offset.insert(value.id, off);
        }
        if iface.frag_depth_var == Some(*spv_var_id) {
            frag_depth_output = Some(value.id);
        }
    }
    // Workgroup vars that were referenced (via OpLoad/OpStore
    // or OpAccessChain through resolve_variable) are in id_map.
    // Vars declared but never used by this entry point still
    // need a slot reserved so the buffer-size matches what the
    // frontend computed; copy any missing entries by allocating
    // fresh ValueIds for them.  (Single-block compute shaders
    // typically use every declared workgroup var, so this is
    // rare; left in for spec-correctness.)
    let workgroup_size = iface.workgroup_size;

    Ok(Function {
        name,
        stage,
        params: Vec::new(), // no params in v1 narrow scope
        return_type,
        entry_block: entry_block_id,
        blocks,
        local_size,
        ssbo_bindings,
        workgroup_size,
        workgroup_var_offset,
        output_varying_byte_offset,
        input_varying_byte_offset,
        frag_depth_output,
    })
}

/// CFG-aware shim around [`translate_inst`]. Handles the
/// terminator + merge-marker opcodes that need the
/// SPIR-V-label → IR BlockId map; delegates everything
/// else to the existing per-instruction translator.
#[allow(clippy::too_many_arguments)]
fn translate_inst_with_cfg(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    label_to_block_id: &HashMap<Word, BlockId>,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    ptr_pointee: &mut HashMap<Word, Word>,
    source_spirv_offset: u32,
) -> Result<(), FrontendError> {
    match spv_inst.class.opcode {
        // Structured-CFG markers — their merge / continue
        // operands are already encoded in BlockKind, so
        // they emit no IR instruction.
        SpvOp::SelectionMerge | SpvOp::LoopMerge => Ok(()),
        SpvOp::Branch => {
            let target_label = expect_id(&spv_inst.operands, 0)?;
            let target = label_to_block_id.get(&target_label)
                .copied()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "OpBranch target label {target_label} not in this function",
                )))?;
            insts.push(Inst {
                op: Op::Branch(target),
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::Switch => {
            // Operands: selector, default, (lit, target)+.
            let sel_id = expect_id(&spv_inst.operands, 0)?;
            let default_label = expect_id(&spv_inst.operands, 1)?;
            let selector = resolve_value(
                sel_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let default = label_to_block_id.get(&default_label).copied()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "OpSwitch default target {default_label} not in this function",
                )))?;
            let mut cases: Vec<(i64, BlockId)> = Vec::new();
            let mut i = 2;
            while i + 1 < spv_inst.operands.len() {
                // SPIR-V allows multi-word case literals
                // for >32-bit selectors. v5 supports only
                // 32-bit selectors; literal sits in one
                // LiteralBit32.
                let lit = match &spv_inst.operands[i] {
                    Operand::LiteralBit32(v) => *v as i32 as i64,
                    other => return Err(FrontendError::Malformed(format!(
                        "OpSwitch case literal: expected LiteralBit32, got {other:?}",
                    ))),
                };
                let target_label = match &spv_inst.operands[i + 1] {
                    Operand::IdRef(id) => *id,
                    other => return Err(FrontendError::Malformed(format!(
                        "OpSwitch case target: expected IdRef, got {other:?}",
                    ))),
                };
                let target = label_to_block_id.get(&target_label).copied()
                    .ok_or_else(|| FrontendError::Malformed(format!(
                        "OpSwitch case target {target_label} not in this function",
                    )))?;
                cases.push((lit, target));
                i += 2;
            }
            insts.push(Inst {
                op: Op::Switch { selector, cases, default },
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::BranchConditional => {
            let cond_id = expect_id(&spv_inst.operands, 0)?;
            let t_label = expect_id(&spv_inst.operands, 1)?;
            let f_label = expect_id(&spv_inst.operands, 2)?;
            let cond = resolve_value(
                cond_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let t_block = label_to_block_id.get(&t_label).copied()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "OpBranchConditional true target {t_label} not in this function",
                )))?;
            let f_block = label_to_block_id.get(&f_label).copied()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "OpBranchConditional false target {f_label} not in this function",
                )))?;
            insts.push(Inst {
                op: Op::BranchCond { cond, t_block, f_block },
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }
        _ => translate_inst(
            spv_inst, types, constants, iface, label_to_block_id,
            id_map, next_value_id, insts, ptr_pointee, source_spirv_offset,
        ),
    }
}

/// Translate one SPIR-V instruction. Pushes zero or more
/// [`Inst`]s onto `insts` — constants that need
/// materialising prefix any non-constant use.
///
/// `source_spirv_offset` is the byte offset of this
/// instruction in the source SPIR-V; preserved on every
/// emitted IR Inst per constraint A2.
#[allow(clippy::too_many_arguments)]
fn translate_inst(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    label_to_block_id: &HashMap<Word, BlockId>,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    ptr_pointee: &mut HashMap<Word, Word>,
    source_spirv_offset: u32,
) -> Result<(), FrontendError> {

    match spv_inst.class.opcode {
        // Block label — handled by block-walking, no IR emit.
        SpvOp::Label | SpvOp::Nop => Ok(()),

        SpvOp::Store => {
            let ptr_id = expect_id(&spv_inst.operands, 0)?;
            let value_id = expect_id(&spv_inst.operands, 1)?;
            // The pointer operand is usually a variable;
            // try the variable path first.
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => resolve_value(
                    ptr_id, types, constants, id_map, next_value_id, insts,
                    source_spirv_offset,
                )?,
            };
            let value_value = resolve_value(
                value_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            insts.push(Inst {
                op: Op::Store { ptr: ptr_value, value: value_value },
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }

        // Arc 60: OpUnreachable / OpKill / OpTerminateInvocation
        // all act as block terminators that don't yield a
        // value.  Lower as Op::Return (no value) so the block
        // has a valid terminator and the caller sees a clean
        // exit.
        //   OpUnreachable -- compiler asserts this block is
        //   not reached at run time; treat as Return.
        //   OpKill / OpTerminateInvocation -- fragment shader
        //   discards.  v1 Tier-2 doesn't have a separate
        //   discard pipeline path; mapping to Return drops the
        //   shader's pending writes (caller-visible buffer
        //   contains whatever was written prior).
        // Arc 65: OpDemoteToHelperInvocation acts like Kill but
        // is *not* a block terminator -- the shader keeps
        // executing afterwards, with the side effects of any
        // subsequent stores quietly dropped.  Tier-2 has no
        // helper-invocation pipeline state, so this op is just
        // a no-op (the rest of the function body runs normally;
        // subsequent stores still happen, but Tier-2 doesn't
        // currently distinguish helper writes).  No IR
        // emission needed.
        SpvOp::DemoteToHelperInvocation => Ok(()),
        // OpIsHelperInvocation returns false: no helpers in
        // Tier-2's serial dispatcher.  Lower as ConstInt 0
        // (Bool i32-backed).
        SpvOp::IsHelperInvocationEXT => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "IsHelperInvocation without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "IsHelperInvocation without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstInt {
                    value: 0,
                    kind: atrium_spv_ir::IntKind::I32,
                },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::Return
        | SpvOp::Unreachable
        | SpvOp::Kill
        | SpvOp::TerminateInvocation => {
            insts.push(Inst {
                op: Op::Return,
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }

        SpvOp::ReturnValue => {
            let val_id = expect_id(&spv_inst.operands, 0)?;
            let val = resolve_value(
                val_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            insts.push(Inst {
                op: Op::ReturnValue(val),
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }

        SpvOp::FunctionEnd => Ok(()),

        // ── Float arithmetic (phase 1 v3) ───────────────
        //
        // OpFAdd / OpFSub / OpFMul / OpFDiv / OpFNegate
        // all share the same shape: result_id +
        // result_type + N source-value operands.
        // Scalar f32 only in v3; vec arithmetic comes
        // next.

        // Integer arithmetic.
        SpvOp::IAdd => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::IAdd(a, b),
        ),
        SpvOp::ISub => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::ISub(a, b),
        ),
        SpvOp::IMul => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::IMul(a, b),
        ),
        SpvOp::SDiv => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::SDiv(a, b),
        ),
        SpvOp::UDiv => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::UDiv(a, b),
        ),
        // Arc 49: OpSRem -- signed integer truncated remainder.
        // SPIR-V distinguishes SRem (same sign as dividend, via
        // truncated division) from SMod (same sign as divisor,
        // via floored division).  We already had SMod via
        // Op::SMod; SRem lowers at the frontend as
        //   x - y * (x sdiv y)
        // using the existing Op::SDiv (truncated) + IMul + ISub.
        SpvOp::SRem => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("SRem without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("SRem without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let y_id = expect_id(&spv_inst.operands, 1)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let y = resolve_value(y_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let q = push_i32(Op::SDiv(x.clone(), y.clone()),
                source_spirv_offset, insts, next_value_id);
            let prod = push_i32(Op::IMul(y, q),
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ISub(x, prod),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        // Arc 50: OpSMod -- signed floored remainder.  Same sign
        // as the divisor.  Lower at the frontend with the
        // standard sign-adjust on top of SRem, since the
        // bespoke backend's Op::SMod path is incomplete.
        //
        //   r = x - y * (x sdiv y)                  -- SRem
        //   adjust = (sign(r) != sign(y)) && (r != 0)
        //   SMod = adjust ? r + y : r
        SpvOp::SMod => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("SMod without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("SMod without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let y_id = expect_id(&spv_inst.operands, 1)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let y = resolve_value(y_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // r = x - y * (x sdiv y)
            let q = push_i32(Op::SDiv(x.clone(), y.clone()),
                source_spirv_offset, insts, next_value_id);
            let prod = push_i32(Op::IMul(y.clone(), q),
                source_spirv_offset, insts, next_value_id);
            let r = push_i32(Op::ISub(x, prod),
                source_spirv_offset, insts, next_value_id);
            // Compute the adjust condition via bit-tricks
            // (avoids producing Bool intermediates that the
            // bespoke `bools` map / `ints` map would split):
            //
            //   sign_xor   = r ^ y                 -- top bit set iff signs differ
            //   diff_bit   = sign_xor >> 31 (LShr) -- 0 or 1
            //   neg_r      = -r
            //   nonzero_or = r | neg_r             -- top bit set iff r != 0
            //   nz_bit     = nonzero_or >> 31      -- 0 or 1
            //   cond_int   = diff_bit & nz_bit     -- 0 or 1
            //   cond       = (cond_int != 0)       -- Bool, lands in `bools`
            let c31 = push_ci(31, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let sign_xor = push_i32(Op::BitXor(r.clone(), y.clone()),
                source_spirv_offset, insts, next_value_id);
            let diff_bit = push_i32(Op::LShr(sign_xor, c31.clone()),
                source_spirv_offset, insts, next_value_id);
            let neg_r = push_i32(Op::INeg(r.clone()),
                source_spirv_offset, insts, next_value_id);
            let nonzero_or = push_i32(Op::BitOr(r.clone(), neg_r),
                source_spirv_offset, insts, next_value_id);
            let nz_bit = push_i32(Op::LShr(nonzero_or, c31),
                source_spirv_offset, insts, next_value_id);
            let cond_int = push_i32(Op::BitAnd(diff_bit, nz_bit),
                source_spirv_offset, insts, next_value_id);
            let zero = push_ci(0, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let cond = push_bool(Op::INe(cond_int, zero),
                source_spirv_offset, insts, next_value_id);
            // adjusted = r + y; result = cond ? adjusted : r.
            let adjusted = push_i32(Op::IAdd(r.clone(), y),
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::Select {
                    cond, t_val: adjusted, f_val: r,
                },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::UMod => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::UMod(a, b),
        ),
        SpvOp::SNegate => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a| Op::INeg(a),
        ),
        // Integer comparisons.
        SpvOp::IEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::IEq(a, b),
        ),
        SpvOp::INotEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::INe(a, b),
        ),
        SpvOp::SLessThan => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::SLt(a, b),
        ),
        SpvOp::SLessThanEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::SLe(a, b),
        ),
        SpvOp::SGreaterThan => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::SGt(a, b),
        ),
        SpvOp::SGreaterThanEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::SGe(a, b),
        ),
        SpvOp::ULessThan => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::ULt(a, b),
        ),
        SpvOp::ULessThanEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::ULe(a, b),
        ),
        SpvOp::UGreaterThan => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::UGt(a, b),
        ),
        SpvOp::UGreaterThanEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::UGe(a, b),
        ),
        // Bitwise + shifts.
        SpvOp::BitwiseAnd => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::BitAnd(a, b)),
        SpvOp::BitwiseOr => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::BitOr(a, b)),
        SpvOp::BitwiseXor => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::BitXor(a, b)),
        SpvOp::Not => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            Op::BitNot),

        // Arc 46: Logical{And,Or,Not,Equal,NotEqual} + Any/All.
        // Bools are i32-backed (0 or 1) per constraint B4.
        // The downstream Bool consumers (Op::Select, Op::Branch
        // conditions) require the value to live in the bespoke
        // backend's `bools` map, which only happens for the
        // dedicated compare ops (FOrd*, IEqual, INotEqual, ...).
        // So the lowering pattern is:
        //
        //   LogicalAnd(a, b)     -> INotEqual(BitAnd(a, b), 0)
        //   LogicalOr(a, b)      -> INotEqual(BitOr(a, b), 0)
        //   LogicalEqual(a, b)   -> IEqual(a, b)
        //   LogicalNotEqual(a, b)-> INotEqual(a, b)
        //   LogicalNot(b)        -> IEqual(b, 0)
        //   Any(vec_bool)        -> INotEqual(fold BitOr  across lanes, 0)
        //   All(vec_bool)        -> INotEqual(fold BitAnd across lanes, 0)
        SpvOp::LogicalAnd | SpvOp::LogicalOr => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("LogicalAnd/Or without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("LogicalAnd/Or without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let a_id = expect_id(&spv_inst.operands, 0)?;
            let b_id = expect_id(&spv_inst.operands, 1)?;
            let a = resolve_value(a_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let b = resolve_value(b_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let bit_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let bit_v = Value { id: bit_id, ty: Type::U32 };
            insts.push(Inst {
                op: if matches!(spv_inst.class.opcode, SpvOp::LogicalAnd) {
                    Op::BitAnd(a, b)
                } else {
                    Op::BitOr(a, b)
                },
                result: Some(bit_v.clone()),
                source_spirv_offset,
            });
            let zero = push_ci(0, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::INe(bit_v, zero),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::LogicalEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::IEq(a, b)),
        SpvOp::LogicalNotEqual => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::INe(a, b)),
        SpvOp::LogicalNot => {
            // NOT b  ≡  b == 0.
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("LogicalNot without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("LogicalNot without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let b_id = expect_id(&spv_inst.operands, 0)?;
            let b = resolve_value(b_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let zero = push_ci(0, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::IEq(b, zero),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::Any | SpvOp::All => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("Any/All without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("Any/All without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let vec_id = expect_id(&spv_inst.operands, 0)?;
            let vec_val = resolve_value(vec_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let n = match &vec_val.ty {
                Type::Vec2(_) => 2,
                Type::Vec3(_) => 3,
                Type::Vec4(_) => 4,
                other => return Err(FrontendError::Unsupported(format!(
                    "Any/All on non-vector type {other:?}"))),
            };
            // Extract every lane as i32, then fold BitOr (Any)
            // or BitAnd (All) left to right.
            let mut acc = push_extract_lane_i32(
                vec_val.clone(), 0,
                source_spirv_offset, insts, next_value_id)?;
            let is_any = matches!(spv_inst.class.opcode, SpvOp::Any);
            for i in 1..n {
                let lane = push_extract_lane_i32(
                    vec_val.clone(), i as u32,
                    source_spirv_offset, insts, next_value_id)?;
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: Type::U32 };
                insts.push(Inst {
                    op: if is_any { Op::BitOr(acc, lane) }
                        else      { Op::BitAnd(acc, lane) },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                acc = v;
            }
            // `acc` is a u32 with the fold result (0 or 1+).
            // Convert to Bool with INotEqual(acc, 0) so the
            // backends' Select / compare-consumer paths see a
            // properly-tagged bool in their `bools` maps.
            let zero = push_ci(0, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::INe(acc, zero),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        // Arc 57: OpSConvert / OpUConvert / OpFConvert --
        // width-changing conversions.  Tier-2 v1 only supports
        // 32-bit scalar/vector types, so for the common case
        // where source and destination types match, these
        // collapse to a zero-cost alias of the source id in
        // the SPIR-V id_map.  Genuine widening / narrowing
        // (e.g. f16->f32 or i64->i32) is gated as unsupported.
        SpvOp::SConvert | SpvOp::UConvert | SpvOp::FConvert => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "[SUF]Convert without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "[SUF]Convert without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let src_id = expect_id(&spv_inst.operands, 0)?;
            let src = resolve_value(src_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            if src.ty != result_ty {
                return Err(FrontendError::Unsupported(format!(
                    "[SUF]Convert {:?} -> {:?} (width change)",
                    src.ty, result_ty)));
            }
            id_map.insert(result_id, src);
            Ok(())
        }
        // Arc 59: debug-info ops -- silently ignored.
        //
        // OpLine / OpNoLine carry source-location hints from
        // glslang's `-g` mode and can appear *between* real
        // instructions inside function blocks.  The other ops
        // (OpName, OpMemberName, OpSource, OpSourceContinued,
        // OpSourceExtension, OpString) are usually module-level
        // and never reach the function translator, but we list
        // them here too for resilience against unusual encoders.
        //
        // The catchall at the end would otherwise reject them
        // with "opcode Line not supported in phase 1 v3".
        SpvOp::Line
        | SpvOp::NoLine
        | SpvOp::Name
        | SpvOp::MemberName
        | SpvOp::Source
        | SpvOp::SourceContinued
        | SpvOp::SourceExtension
        | SpvOp::String
        | SpvOp::ModuleProcessed
        // Arc 75: OpLifetimeStart / OpLifetimeStop are
        // optimizer hints for stack-variable lifetimes.
        // Tier-2 ignores them -- our IR has no equivalent
        // marker and the backends never re-stack-allocate.
        | SpvOp::LifetimeStart
        | SpvOp::LifetimeStop => Ok(()),

        // OpBitcast: reinterpret the bits of a value as the
        // result type (f32 <-> i32/u32).  Maps directly to
        // Op::Bitcast, which both backends already lower.
        SpvOp::Bitcast => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("Bitcast without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("Bitcast without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let src_id = expect_id(&spv_inst.operands, 0)?;
            let src = resolve_value(src_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let result = alloc_or_get_result(
                result_id, result_ty.clone(), id_map, next_value_id);
            insts.push(Inst {
                op: Op::Bitcast(src, result_ty),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        // Arc 55: OpQuantizeToF16 -- round-trip an f32 through
        // f16 precision and back.  Implemented as a bit-mask
        // that drops the bottom 13 mantissa bits of the f32
        // (f32 has 23 mantissa bits; f16 has 10, so 13 are
        // discarded).
        //
        // Limitations:
        //   * Truncating round; IEEE 754 round-to-nearest-even
        //     is not implemented.
        //   * Values outside f16 dynamic range (~|x| > 65504)
        //     are NOT clamped to +/- Inf; they return whatever
        //     truncating the mantissa produces.  Inputs in
        //     range round correctly.
        // Real shaders use QuantizeToF16 mostly for mediump
        // round-trip compatibility, which truncation already
        // captures.
        SpvOp::QuantizeToF16 => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "QuantizeToF16 without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "QuantizeToF16 without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            if !matches!(result_ty, Type::F32) {
                return Err(FrontendError::Unsupported(format!(
                    "QuantizeToF16 result type {result_ty:?} (only F32 scalar)")));
            }
            let src_id = expect_id(&spv_inst.operands, 0)?;
            let src = resolve_value(src_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // bits = bitcast<u32>(src)
            let bits_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let bits = Value { id: bits_id, ty: Type::U32 };
            insts.push(Inst {
                op: Op::Bitcast(src, Type::U32),
                result: Some(bits.clone()),
                source_spirv_offset,
            });
            // masked = bits & 0xFFFFE000   (drop bottom 13 bits)
            let mask = push_ci(0xFFFF_E000_i64,
                atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let masked_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let masked = Value { id: masked_id, ty: Type::U32 };
            insts.push(Inst {
                op: Op::BitAnd(bits, mask),
                result: Some(masked.clone()),
                source_spirv_offset,
            });
            // result = bitcast<f32>(masked)
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::Bitcast(masked, Type::F32),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::ShiftLeftLogical => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::Shl(a, b)),
        SpvOp::ShiftRightLogical => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::LShr(a, b)),
        SpvOp::ShiftRightArithmetic => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::AShr(a, b)),
        SpvOp::BitReverse => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            Op::Rbit),

        // Arc 47: bit-field extract / insert.
        //
        //   BitFieldUExtract(base, offset, count):
        //     (base >> offset) & ((1 << count) - 1)
        //   BitFieldSExtract(base, offset, count):
        //     ((base << (32 - offset - count)) >> (32 - count))  -- arithmetic
        //   BitFieldInsert(base, insert, offset, count):
        //     mask    = ((1 << count) - 1) << offset
        //     (base & ~mask) | ((insert << offset) & mask)
        //
        // All three only support 32-bit operands in v1.
        SpvOp::BitFieldUExtract | SpvOp::BitFieldSExtract => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("BitFieldExtract without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("BitFieldExtract without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let base_id   = expect_id(&spv_inst.operands, 0)?;
            let off_id    = expect_id(&spv_inst.operands, 1)?;
            let count_id  = expect_id(&spv_inst.operands, 2)?;
            let base  = resolve_value(base_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let off   = resolve_value(off_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let count = resolve_value(count_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let signed = matches!(spv_inst.class.opcode, SpvOp::BitFieldSExtract);
            let result = alloc_or_get_result(
                result_id, result_ty.clone(), id_map, next_value_id);
            if signed {
                // shift_left  = 32 - offset - count
                // shift_right = 32 - count
                // tmp = base << shift_left
                // result = tmp >> shift_right  (AShr)
                let c32 = push_ci(32, atrium_spv_ir::IntKind::U32,
                    source_spirv_offset, insts, next_value_id);
                let sub1 = push_i32(Op::ISub(c32.clone(), off),
                    source_spirv_offset, insts, next_value_id);
                let shift_left = push_i32(Op::ISub(sub1, count.clone()),
                    source_spirv_offset, insts, next_value_id);
                let shift_right = push_i32(Op::ISub(c32, count),
                    source_spirv_offset, insts, next_value_id);
                let tmp = push_i32(Op::Shl(base, shift_left),
                    source_spirv_offset, insts, next_value_id);
                insts.push(Inst {
                    op: Op::AShr(tmp, shift_right),
                    result: Some(result),
                    source_spirv_offset,
                });
            } else {
                let shifted = push_i32(Op::LShr(base, off),
                    source_spirv_offset, insts, next_value_id);
                // mask = (1 << count) - 1
                let c1 = push_ci(1, atrium_spv_ir::IntKind::U32,
                    source_spirv_offset, insts, next_value_id);
                let pow = push_i32(Op::Shl(c1.clone(), count),
                    source_spirv_offset, insts, next_value_id);
                let mask = push_i32(Op::ISub(pow, c1),
                    source_spirv_offset, insts, next_value_id);
                insts.push(Inst {
                    op: Op::BitAnd(shifted, mask),
                    result: Some(result),
                    source_spirv_offset,
                });
            }
            Ok(())
        }
        SpvOp::BitFieldInsert => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("BitFieldInsert without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("BitFieldInsert without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let base_id   = expect_id(&spv_inst.operands, 0)?;
            let insert_id = expect_id(&spv_inst.operands, 1)?;
            let off_id    = expect_id(&spv_inst.operands, 2)?;
            let count_id  = expect_id(&spv_inst.operands, 3)?;
            let base   = resolve_value(base_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let insert = resolve_value(insert_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let off    = resolve_value(off_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let count  = resolve_value(count_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // mask_low = (1 << count) - 1
            let c1 = push_ci(1, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let pow = push_i32(Op::Shl(c1.clone(), count),
                source_spirv_offset, insts, next_value_id);
            let mask_low = push_i32(Op::ISub(pow, c1),
                source_spirv_offset, insts, next_value_id);
            // mask = mask_low << offset
            let mask = push_i32(Op::Shl(mask_low, off.clone()),
                source_spirv_offset, insts, next_value_id);
            // not_mask = ~mask
            let not_mask = push_i32(Op::BitNot(mask.clone()),
                source_spirv_offset, insts, next_value_id);
            // cleared = base & not_mask
            let cleared = push_i32(Op::BitAnd(base, not_mask),
                source_spirv_offset, insts, next_value_id);
            // shifted = insert << offset
            let shifted = push_i32(Op::Shl(insert, off),
                source_spirv_offset, insts, next_value_id);
            // placed = shifted & mask
            let placed = push_i32(Op::BitAnd(shifted, mask),
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::BitOr(cleared, placed),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::BitCount => {
            // SWAR popcount on u32:
            //   x = x - ((x >> 1) & 0x55555555)
            //   x = (x & 0x33333333) + ((x >> 2) & 0x33333333)
            //   x = (x + (x >> 4)) & 0x0F0F0F0F
            //   x = (x * 0x01010101) >> 24
            // All ops already lowered on both backends.
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("BitCount without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("BitCount without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // s1 = x >> 1
            let c1 = push_ci32(1, source_spirv_offset, insts, next_value_id);
            let s1 = push_i32(Op::LShr(x.clone(), c1),
                source_spirv_offset, insts, next_value_id);
            let m_5 = push_ci32(0x5555_5555, source_spirv_offset, insts, next_value_id);
            let s1_m = push_i32(Op::BitAnd(s1, m_5),
                source_spirv_offset, insts, next_value_id);
            let x1 = push_i32(Op::ISub(x, s1_m),
                source_spirv_offset, insts, next_value_id);
            // m_3 = 0x33333333
            let c2 = push_ci32(2, source_spirv_offset, insts, next_value_id);
            let m_3a = push_ci32(0x3333_3333, source_spirv_offset, insts, next_value_id);
            let m_3b = push_ci32(0x3333_3333, source_spirv_offset, insts, next_value_id);
            let s2 = push_i32(Op::LShr(x1.clone(), c2),
                source_spirv_offset, insts, next_value_id);
            let s2_m = push_i32(Op::BitAnd(s2, m_3a),
                source_spirv_offset, insts, next_value_id);
            let x1_m = push_i32(Op::BitAnd(x1, m_3b),
                source_spirv_offset, insts, next_value_id);
            let x2 = push_i32(Op::IAdd(x1_m, s2_m),
                source_spirv_offset, insts, next_value_id);
            // x = (x + (x >> 4)) & 0x0F0F0F0F
            let c4 = push_ci32(4, source_spirv_offset, insts, next_value_id);
            let s4 = push_i32(Op::LShr(x2.clone(), c4),
                source_spirv_offset, insts, next_value_id);
            let xs4 = push_i32(Op::IAdd(x2, s4),
                source_spirv_offset, insts, next_value_id);
            let m_f = push_ci32(0x0F0F_0F0F, source_spirv_offset, insts, next_value_id);
            let x3 = push_i32(Op::BitAnd(xs4, m_f),
                source_spirv_offset, insts, next_value_id);
            // x = (x * 0x01010101) >> 24
            let m_1 = push_ci32(0x0101_0101, source_spirv_offset, insts, next_value_id);
            let xm = push_i32(Op::IMul(x3, m_1),
                source_spirv_offset, insts, next_value_id);
            let c24 = push_ci32(24, source_spirv_offset, insts, next_value_id);
            let result = Value {
                id: ValueId(*next_value_id),
                ty: result_ty,
            };
            *next_value_id += 1;
            id_map.insert(result_id, result.clone());
            insts.push(Inst {
                op: Op::LShr(xm, c24),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        // Int↔float conversions.
        SpvOp::ConvertSToF => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            Op::ConvertSToF),
        SpvOp::ConvertUToF => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            Op::ConvertUToF),
        SpvOp::ConvertFToS => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            Op::ConvertFToS),
        SpvOp::ConvertFToU => emit_unop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            Op::ConvertFToU),
        SpvOp::FAdd => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FAdd(a, b),
        ),
        SpvOp::FSub => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FSub(a, b),
        ),
        // OpFMul + OpVectorTimesScalar + OpMatrixTimesScalar
        // all lower to Op::FMul; the backend's
        // emit_float_binop dispatches on (scalar × scalar) /
        // (vec × vec) / (vec × scalar with broadcast) by
        // inspecting the operand storage.  Mat4 × scalar
        // rides the existing per-column scalar-broadcast path
        // since a Mat4 is stored as four column vec4s.
        SpvOp::FMul
        | SpvOp::VectorTimesScalar
        | SpvOp::MatrixTimesScalar => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FMul(a, b),
        ),
        // Arc 48: OpFRem and OpFMod -- core SPIR-V FP remainder
        // operations.  Both lower at the frontend; the backends
        // don't ship a native FRem yet.
        //
        //   FRem(x, y) -- truncated remainder; same sign as x.
        //                 Lower as x - y * trunc(x / y).
        //   FMod(x, y) -- floored  remainder; same sign as y.
        //                 Lower as x - y * floor(x / y).
        SpvOp::FRem | SpvOp::FMod => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("FRem/FMod without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("FRem/FMod without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let is_mod = matches!(spv_inst.class.opcode, SpvOp::FMod);
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let y_id = expect_id(&spv_inst.operands, 1)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let y = resolve_value(y_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // div = x / y
            let div_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let div_v = Value { id: div_id, ty: result_ty.clone() };
            insts.push(Inst {
                op: Op::FDiv(x.clone(), y.clone()),
                result: Some(div_v.clone()),
                source_spirv_offset,
            });
            // rounded = floor(div)  if FMod, trunc(div) if FRem.
            let floor_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let floor_v = Value { id: floor_id, ty: result_ty.clone() };
            insts.push(Inst {
                op: if is_mod { Op::FFloor(div_v) } else { Op::FTrunc(div_v) },
                result: Some(floor_v.clone()),
                source_spirv_offset,
            });
            // mul_v = y * floor_v
            let mul_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let mul_v = Value { id: mul_id, ty: result_ty.clone() };
            insts.push(Inst {
                op: Op::FMul(y, floor_v),
                result: Some(mul_v.clone()),
                source_spirv_offset,
            });
            // result = x - mul_v
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::FSub(x, mul_v),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        SpvOp::FDiv => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FDiv(a, b),
        ),
        SpvOp::Dot => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::Dot(a, b),
        ),

        // OpMatrixTimesVector: matrix on the left, vector
        // on the right (column-major, per SPIR-V's
        // "M *cv v" semantics). Backends lower this into
        // 4 broadcast-mul-adds — every op below is in the
        // existing tested set.
        SpvOp::MatrixTimesVector => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |m, v| Op::MatrixTimesVector { matrix: m, vector: v },
        ),

        // Float comparisons → Bool (i32 0/1). All 12
        // variants map 1:1 to Op::FOrd* / FUnord*.
        SpvOp::FOrdEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FOrdEq(a, b),
        ),
        SpvOp::FOrdNotEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FOrdNe(a, b),
        ),
        SpvOp::FOrdLessThan => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FOrdLt(a, b),
        ),
        SpvOp::FOrdLessThanEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FOrdLe(a, b),
        ),
        SpvOp::FOrdGreaterThan => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FOrdGt(a, b),
        ),
        SpvOp::FOrdGreaterThanEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FOrdGe(a, b),
        ),
        SpvOp::FUnordEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FUnordEq(a, b),
        ),
        SpvOp::FUnordNotEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FUnordNe(a, b),
        ),
        SpvOp::FUnordLessThan => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FUnordLt(a, b),
        ),
        SpvOp::FUnordLessThanEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FUnordLe(a, b),
        ),
        SpvOp::FUnordGreaterThan => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FUnordGt(a, b),
        ),
        SpvOp::FUnordGreaterThanEqual => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FUnordGe(a, b),
        ),

        // OpIsNan(x): x is NaN  ≡  x != x under unordered
        // compare (FUnordNe is true whenever either operand
        // is NaN; with both operands equal to x it isolates
        // exactly the NaN case).
        SpvOp::IsNan => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("IsNan without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("IsNan without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::FUnordNe(x.clone(), x),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        // OpIsInf(x): |x| == +∞.  Synthesised as
        // FOrdEq(FAbs(x), ConstFloat(+inf)) -- the ordered
        // compare is false for NaN, matching the spec
        // (IsInf is false on NaN input).
        SpvOp::IsInf => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("IsInf without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("IsInf without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let abs_x = push_f32(Op::FAbs(x),
                source_spirv_offset, insts, next_value_id);
            let inf = push_cf(f64::INFINITY,
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::FOrdEq(abs_x, inf),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpSelect: cond ? t_val : f_val. cond is Bool
        // (scalar) or vec<Bool> (per-lane). Operands:
        //   0  cond
        //   1  t_val (selected when cond != 0)
        //   2  f_val (selected when cond == 0)
        SpvOp::Select => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "Select without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "Select without result type".to_string()))?;
            let ty = types.get(result_type_id)?.clone();
            let cond_id = expect_id(&spv_inst.operands, 0)?;
            let t_id    = expect_id(&spv_inst.operands, 1)?;
            let f_id    = expect_id(&spv_inst.operands, 2)?;
            let cond = resolve_value(
                cond_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let t_val = resolve_value(
                t_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let f_val = resolve_value(
                f_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::Select { cond, t_val, f_val },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpVectorShuffle: produce a new vector by
        // selecting lanes from two source vectors.
        // Operand layout:
        //   0   src1: IdRef
        //   1   src2: IdRef
        //   2.. components: LiteralBit32 (per-output-lane
        //       index into src1 ++ src2; 0xFFFFFFFF means
        //       "undefined" but we punt on that for v4)
        SpvOp::VectorShuffle => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "VectorShuffle without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "VectorShuffle without result type".to_string()))?;
            let ty = types.get(result_type_id)?.clone();
            let src1_id = expect_id(&spv_inst.operands, 0)?;
            let src2_id = expect_id(&spv_inst.operands, 1)?;
            let src1 = resolve_value(
                src1_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let src2 = resolve_value(
                src2_id, types, constants, id_map, next_value_id, insts,
                source_spirv_offset,
            )?;
            let mut components: Vec<u32> = Vec::with_capacity(
                spv_inst.operands.len().saturating_sub(2),
            );
            for op in &spv_inst.operands[2..] {
                match op {
                    Operand::LiteralBit32(v) => components.push(*v),
                    other => return Err(FrontendError::Malformed(format!(
                        "VectorShuffle component expected LiteralBit32, got {other:?}",
                    ))),
                }
            }
            let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::VectorShuffle { src1, src2, components },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::FNegate => emit_unop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a| Op::FNeg(a),
        ),

        // OpCompositeExtract: pull one scalar lane out of
        // a vector. SPIR-V allows multi-level index chains
        // for nested aggregates; we support the single-
        // index vector case (the only one shaders hit —
        // GLSL `color.r` etc.). Deeper chains → Unsupported.
        // Arc 51: OpVectorExtractDynamic / OpVectorInsertDynamic.
        // Runtime-indexed vector access.  Lower at the frontend
        // via a chain of `Op::Select` between statically-
        // extracted lanes.  For an N-lane vector:
        //
        //   ExtractDynamic(v, idx):
        //     cond_k  = (idx == k)  for k in 0..N-1
        //     result  = cond_0 ? v[0] : (cond_1 ? v[1] : ...)
        //
        //   InsertDynamic(v, val, idx):
        //     new_k   = (idx == k) ? val : v[k]   for each lane
        //     result  = ConstVec(new_0, ..., new_{N-1})
        SpvOp::VectorExtractDynamic => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "VectorExtractDynamic without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "VectorExtractDynamic without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let vec_id = expect_id(&spv_inst.operands, 0)?;
            let idx_id = expect_id(&spv_inst.operands, 1)?;
            let vec_val = resolve_value(vec_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let idx = resolve_value(idx_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let n = match &vec_val.ty {
                Type::Vec2(_) => 2,
                Type::Vec3(_) => 3,
                Type::Vec4(_) => 4,
                other => return Err(FrontendError::Unsupported(format!(
                    "VectorExtractDynamic on non-vector type {other:?}"))),
            };
            // Statically extract each lane.
            let mut lanes: Vec<Value> = Vec::with_capacity(n);
            for i in 0..n {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: result_ty.clone() };
                insts.push(Inst {
                    op: Op::VectorExtract {
                        vector: vec_val.clone(), index: i as u32,
                    },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                lanes.push(v);
            }
            // Fold from the right: acc starts as the last lane,
            // and each iteration picks cond_{k} ? lane[k] : acc.
            let mut acc = lanes.pop().unwrap();
            for k in (0..n - 1).rev() {
                let kc = push_ci(k as i64,
                    atrium_spv_ir::IntKind::U32,
                    source_spirv_offset, insts, next_value_id);
                let cond = push_bool(Op::IEq(idx.clone(), kc),
                    source_spirv_offset, insts, next_value_id);
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let sel = Value { id, ty: result_ty.clone() };
                insts.push(Inst {
                    op: Op::Select {
                        cond,
                        t_val: lanes[k].clone(),
                        f_val: acc,
                    },
                    result: Some(sel.clone()),
                    source_spirv_offset,
                });
                acc = sel;
            }
            // Bind the result_id to acc by emitting an identity
            // FAdd-with-0 / IAdd-with-0 based on result type.
            // Simpler: use Select(true, acc, acc) — but that
            // needs a true const.  Cleanest: copy via a no-op
            // arithmetic identity.  For F32, add 0; for I32/U32,
            // IAdd 0.
            let result = alloc_or_get_result(
                result_id, result_ty.clone(), id_map, next_value_id);
            match result_ty {
                Type::F32 => {
                    let zero = ValueId(*next_value_id);
                    *next_value_id += 1;
                    let zero_v = Value { id: zero, ty: Type::F32 };
                    insts.push(Inst {
                        op: Op::ConstFloat { value: 0.0,
                            kind: atrium_spv_ir::FloatKind::F32 },
                        result: Some(zero_v.clone()),
                        source_spirv_offset,
                    });
                    insts.push(Inst {
                        op: Op::FAdd(acc, zero_v),
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                Type::I32 | Type::U32 => {
                    let zero = push_ci(0,
                        atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    insts.push(Inst {
                        op: Op::IAdd(acc, zero),
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                other => return Err(FrontendError::Unsupported(format!(
                    "VectorExtractDynamic result type {other:?}"))),
            }
            Ok(())
        }
        SpvOp::VectorInsertDynamic => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "VectorInsertDynamic without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "VectorInsertDynamic without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let vec_id = expect_id(&spv_inst.operands, 0)?;
            let val_id = expect_id(&spv_inst.operands, 1)?;
            let idx_id = expect_id(&spv_inst.operands, 2)?;
            let vec_val = resolve_value(vec_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let val = resolve_value(val_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let idx = resolve_value(idx_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let n = match &vec_val.ty {
                Type::Vec2(_) => 2,
                Type::Vec3(_) => 3,
                Type::Vec4(_) => 4,
                other => return Err(FrontendError::Unsupported(format!(
                    "VectorInsertDynamic on non-vector type {other:?}"))),
            };
            // For each lane k: new_lane = (idx == k) ? val : v[k].
            let mut new_lanes: Vec<Value> = Vec::with_capacity(n);
            for k in 0..n {
                let v_lane_id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v_lane = Value { id: v_lane_id, ty: val.ty.clone() };
                insts.push(Inst {
                    op: Op::VectorExtract {
                        vector: vec_val.clone(), index: k as u32,
                    },
                    result: Some(v_lane.clone()),
                    source_spirv_offset,
                });
                let kc = push_ci(k as i64,
                    atrium_spv_ir::IntKind::U32,
                    source_spirv_offset, insts, next_value_id);
                let cond = push_bool(Op::IEq(idx.clone(), kc),
                    source_spirv_offset, insts, next_value_id);
                let sel_id = ValueId(*next_value_id);
                *next_value_id += 1;
                let sel = Value { id: sel_id, ty: val.ty.clone() };
                insts.push(Inst {
                    op: Op::Select {
                        cond,
                        t_val: val.clone(),
                        f_val: v_lane,
                    },
                    result: Some(sel.clone()),
                    source_spirv_offset,
                });
                new_lanes.push(sel);
            }
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstVec(new_lanes),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // Arc 52: OpCompositeInsert + OpCopyObject + OpUndef.
        //
        //   CompositeInsert(value, composite, index):
        //     For vec<N>:  new[k] = (k == index) ? value : composite[k]
        //     result = ConstVec(new[0..N])
        //   Static index, single level (vector inserts only).
        //
        //   CopyObject(src) -> alias the result_id to src; emit
        //   an identity copy via FAdd-0 / IAdd-0 so the bespoke
        //   backend allocates a fresh dest reg.
        //
        //   Undef -> ConstFloat 0.0 / ConstInt 0 / ConstVec
        //   of zeros, depending on result type.
        SpvOp::CompositeInsert => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "CompositeInsert without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "CompositeInsert without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let object_id    = expect_id(&spv_inst.operands, 0)?;
            let composite_id = expect_id(&spv_inst.operands, 1)?;
            let indices: Vec<u32> = spv_inst.operands[2..].iter()
                .filter_map(|o| match o {
                    Operand::LiteralBit32(v) => Some(*v),
                    _ => None,
                })
                .collect();
            if indices.len() != 1 {
                return Err(FrontendError::Unsupported(format!(
                    "CompositeInsert with {} indices; only single-level \
                     vector insert supported", indices.len())));
            }
            let index = indices[0] as usize;
            let object = resolve_value(object_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let composite = resolve_value(composite_id, types, constants,
                id_map, next_value_id, insts, source_spirv_offset)?;
            let n = match &composite.ty {
                Type::Vec2(_) => 2,
                Type::Vec3(_) => 3,
                Type::Vec4(_) => 4,
                other => return Err(FrontendError::Unsupported(format!(
                    "CompositeInsert on non-vector type {other:?}"))),
            };
            if index >= n {
                return Err(FrontendError::Unsupported(format!(
                    "CompositeInsert index {index} out of bounds (n={n})")));
            }
            let mut new_lanes: Vec<Value> = Vec::with_capacity(n);
            for k in 0..n {
                if k == index {
                    new_lanes.push(object.clone());
                } else {
                    let id = ValueId(*next_value_id);
                    *next_value_id += 1;
                    let v = Value { id, ty: object.ty.clone() };
                    insts.push(Inst {
                        op: Op::VectorExtract {
                            vector: composite.clone(), index: k as u32,
                        },
                        result: Some(v.clone()),
                        source_spirv_offset,
                    });
                    new_lanes.push(v);
                }
            }
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstVec(new_lanes),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::CopyObject => {
            // OpCopyObject(src): produce a new SSA Result Id
            // with the same value.  We alias at the SPIR-V id
            // level: no new IR Value, no new instruction.  The
            // backends never see a separate op.
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("CopyObject without result id".into()))?;
            let src_id = expect_id(&spv_inst.operands, 0)?;
            let src = resolve_value(src_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            id_map.insert(result_id, src);
            Ok(())
        }
        SpvOp::Undef => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed("Undef without result id".into()))?;
            let result_ty_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed("Undef without result type".into()))?;
            let result_ty = types.get(result_ty_id)?.clone();
            let result = alloc_or_get_result(
                result_id, result_ty.clone(), id_map, next_value_id);
            match result_ty {
                Type::F32 => {
                    insts.push(Inst {
                        op: Op::ConstFloat { value: 0.0,
                            kind: atrium_spv_ir::FloatKind::F32 },
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                Type::I32 => {
                    insts.push(Inst {
                        op: Op::ConstInt { value: 0,
                            kind: atrium_spv_ir::IntKind::I32 },
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                Type::U32 => {
                    insts.push(Inst {
                        op: Op::ConstInt { value: 0,
                            kind: atrium_spv_ir::IntKind::U32 },
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                Type::Bool => {
                    insts.push(Inst {
                        op: Op::ConstInt { value: 0,
                            kind: atrium_spv_ir::IntKind::I32 },
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                other => return Err(FrontendError::Unsupported(format!(
                    "Undef result type {other:?}"))),
            }
            Ok(())
        }

        SpvOp::CompositeExtract => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "CompositeExtract without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "CompositeExtract without result type".to_string()))?;
            let ty = types.get(result_type_id)?.clone();
            let composite_id = expect_id(&spv_inst.operands, 0)?;
            // Exactly one literal index for the vector case.
            let indices: Vec<u32> = spv_inst.operands[1..].iter()
                .filter_map(|o| match o {
                    Operand::LiteralBit32(v) => Some(*v),
                    _ => None,
                })
                .collect();
            if indices.len() != 1 {
                return Err(FrontendError::Unsupported(format!(
                    "CompositeExtract with {} indices; only single-level \
                     vector extract supported", indices.len())));
            }
            let vector = resolve_value(
                composite_id, types, constants, id_map, next_value_id,
                insts, source_spirv_offset,
            )?;
            let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::VectorExtract { vector, index: indices[0] },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpCompositeConstruct: build a vector (or
        // matrix; matrices not supported yet) from N
        // element Values. Per the IR's ConstVec doc:
        // "elements may be runtime-computed Values, not
        // just constants" — the name has a Const prefix
        // for historical reasons but the semantics match
        // SPIR-V's OpCompositeConstruct exactly.
        SpvOp::CompositeConstruct => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "CompositeConstruct without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "CompositeConstruct without result type".to_string()))?;
            let ty = types.get(result_type_id)?.clone();
            let mut elements = Vec::with_capacity(spv_inst.operands.len());
            for op in &spv_inst.operands {
                let elem_id = match op {
                    Operand::IdRef(id) => *id,
                    other => return Err(FrontendError::Malformed(format!(
                        "CompositeConstruct expected IdRef, got {other:?}",
                    ))),
                };
                let v = resolve_value(
                    elem_id, types, constants, id_map, next_value_id, insts,
                    source_spirv_offset,
                )?;
                elements.push(v);
            }
            let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstVec(elements),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpPhi: pick a value based on which predecessor
        // block transferred control here. Operands are a
        // sequence of (value_id, parent_label_id) pairs.
        // The frontend converts each parent_label_id to
        // the IR BlockId we assigned during block walk.
        SpvOp::Phi => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "Phi without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "Phi without result type".to_string()))?;
            let ty = types.get(result_type_id)?.clone();
            // Walk operands two at a time.
            let mut arms: Vec<atrium_spv_ir::PhiArm> = Vec::new();
            let mut i = 0;
            while i + 1 < spv_inst.operands.len() {
                let val_id = match &spv_inst.operands[i] {
                    Operand::IdRef(id) => *id,
                    other => return Err(FrontendError::Malformed(format!(
                        "Phi arm value: expected IdRef, got {other:?}",
                    ))),
                };
                let parent_label = match &spv_inst.operands[i+1] {
                    Operand::IdRef(id) => *id,
                    other => return Err(FrontendError::Malformed(format!(
                        "Phi arm parent: expected IdRef, got {other:?}",
                    ))),
                };
                let from = label_to_block_id.get(&parent_label).copied()
                    .ok_or_else(|| FrontendError::Malformed(format!(
                        "Phi arm parent label {parent_label} not in this function",
                    )))?;
                let value = resolve_value(
                    val_id, types, constants, id_map,
                    next_value_id, insts, source_spirv_offset,
                )?;
                arms.push(atrium_spv_ir::PhiArm { from, value });
                i += 2;
            }
            let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::Phi(arms),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpAccessChain / OpInBoundsAccessChain: resolve a
        // chain of constant indices to a single byte offset
        // per constraint B5. The result is a Pointer Value
        // carrying the storage class + leaf pointee type.
        SpvOp::AccessChain | SpvOp::InBoundsAccessChain => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "AccessChain without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "AccessChain without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();

            let base_id = expect_id(&spv_inst.operands, 0)?;
            // Two base shapes:
            //   (1) a Variable — pointee comes from
            //       iface.variables.
            //   (2) a *chained* AccessChain result — base is
            //       a prior pointer Value whose pointee type
            //       we recorded in `ptr_pointee`.  This is the
            //       `instances[slot].member` shape: the first
            //       AccessChain lands on the struct element
            //       (dynamic step), the second steps into a
            //       member of that struct.
            // Discriminate on iface.variables membership, NOT
            // on resolve_variable's Option: the result-id
            // pre-pass seeds id_map for *every* SSA result, so
            // resolve_variable returns Some for a chained
            // AccessChain result too.  Only true OpVariables
            // appear in iface.variables.
            let (base, mut current_pointee_id) = if let Some((_storage, pointee)) =
                iface.variables.get(&base_id).copied()
            {
                let var_value = resolve_variable(
                    base_id, types, iface, id_map, next_value_id,
                )?.ok_or_else(|| FrontendError::Malformed(format!(
                    "AccessChain base var {base_id} not resolvable",
                )))?;
                (var_value, pointee)
            } else {
                // Chained AccessChain: base is a prior pointer
                // Value (e.g. `instances[slot].member`).
                let pointee = ptr_pointee.get(&base_id).copied()
                    .ok_or_else(|| FrontendError::Unsupported(format!(
                        "AccessChain base id {base_id} is neither a \
                         Variable nor a tracked pointer (unsupported \
                         chained access pattern)",
                    )))?;
                let base_value = resolve_value(
                    base_id, types, constants, id_map,
                    next_value_id, insts, source_spirv_offset,
                )?;
                (base_value, pointee)
            };

            // Captured dynamic step (if any).  Today we
            // support exactly ONE non-constant index, and it
            // must be the LAST index, stepping into a
            // RuntimeArray member.  This matches the
            // canonical `ssbo.data[i]` shape; richer dynamic
            // chains land as needed.
            let mut byte_offset: u32 = 0;
            let mut dynamic_step: Option<(Word, u32)> = None;
            let operands: Vec<&Operand> = spv_inst.operands.iter().skip(1).collect();
            for (step_i, op) in operands.iter().enumerate() {
                let is_last = step_i == operands.len() - 1;
                let idx_id = match op {
                    Operand::IdRef(id) => *id,
                    other => return Err(FrontendError::Malformed(format!(
                        "AccessChain expected IdRef index, got {other:?}",
                    ))),
                };
                let idx_const_opt = constants.get(idx_id);
                if idx_const_opt.is_none() && is_last {
                    // Dynamic last-index path.  Step through
                    // a RuntimeArray (or sized Array) pointee:
                    // record (element_type, stride) so the
                    // dynamic op can pick the right address
                    // arithmetic.
                    let raw = types.get_raw(current_pointee_id)
                        .map_err(|_| FrontendError::Unsupported(format!(
                            "dynamic AccessChain index but pointee \
                             type {current_pointee_id} is not an array \
                             (no raw type info)",
                        )))?;
                    let elem_type_id = match raw.class.opcode {
                        rspirv::spirv::Op::TypeRuntimeArray
                        | rspirv::spirv::Op::TypeArray => match raw.operands.first() {
                            Some(Operand::IdRef(id)) => *id,
                            other => return Err(FrontendError::Malformed(format!(
                                "Array type missing element id: {other:?}",
                            ))),
                        },
                        other => return Err(FrontendError::Unsupported(format!(
                            "dynamic AccessChain through non-array pointee \
                             (opcode {other:?}) not supported",
                        ))),
                    };
                    // Prefer the array's ArrayStride decoration
                    // (authoritative std430 step, includes
                    // trailing padding and covers aggregate
                    // elements); fall back to the element's
                    // packed leaf size for un-decorated arrays.
                    let stride = types.array_stride(current_pointee_id)
                        .unwrap_or_else(|| crate::interface::ir_type_size_bytes_for(
                            &types.types, elem_type_id));
                    if stride == 0 {
                        return Err(FrontendError::Unsupported(format!(
                            "dynamic AccessChain element type {elem_type_id} \
                             has no IR size and no ArrayStride decoration",
                        )));
                    }
                    dynamic_step = Some((idx_id, stride));
                    // The chain now points at the array element;
                    // record it so a chained AccessChain into a
                    // struct element can keep walking members.
                    current_pointee_id = elem_type_id;
                    break;
                }
                let idx_const = idx_const_opt.ok_or_else(||
                    FrontendError::Unsupported(format!(
                        "AccessChain non-constant index id {idx_id} not in \
                         last position or not stepping into a RuntimeArray",
                    )))?;
                let idx_val: u32 = match &idx_const.kind {
                    ConstantKind::Scalar(Op::ConstInt { value, .. }) =>
                        *value as u32,
                    other => return Err(FrontendError::Unsupported(format!(
                        "AccessChain index must be a scalar int constant, got {other:?}",
                    ))),
                };

                // Step through the pointee.  Three cases:
                //   - Struct: descend via the recorded
                //     member layout.
                //   - RuntimeArray / Array: this is a
                //     constant-index step into an array;
                //     compute idx * element_size and add.
                //   - other: unsupported.
                if let Some(layout) = iface.struct_layouts.get(&current_pointee_id) {
                    let member = layout.get(idx_val as usize).ok_or_else(||
                        FrontendError::Malformed(format!(
                            "AccessChain index {idx_val} out of range for struct \
                             with {} members",
                            layout.len(),
                        )))?;
                    byte_offset = byte_offset.saturating_add(member.byte_offset);
                    current_pointee_id = member.type_id;
                } else if let Ok(raw) = types.get_raw(current_pointee_id) {
                    match raw.class.opcode {
                        rspirv::spirv::Op::TypeRuntimeArray
                        | rspirv::spirv::Op::TypeArray => {
                            let elem_type_id = match raw.operands.first() {
                                Some(Operand::IdRef(id)) => *id,
                                other => return Err(FrontendError::Malformed(format!(
                                    "Array type missing element id: {other:?}",
                                ))),
                            };
                            let stride = types.array_stride(current_pointee_id)
                                .unwrap_or_else(|| crate::interface::ir_type_size_bytes_for(
                                    &types.types, elem_type_id));
                            if stride == 0 {
                                return Err(FrontendError::Unsupported(format!(
                                    "constant AccessChain into Array of \
                                     non-leaf element type {elem_type_id}",
                                )));
                            }
                            byte_offset = byte_offset
                                .saturating_add(idx_val.saturating_mul(stride));
                            current_pointee_id = elem_type_id;
                        }
                        other => return Err(FrontendError::Unsupported(format!(
                            "AccessChain constant step through non-struct \
                             non-array pointee (opcode {other:?}) not supported",
                        ))),
                    }
                } else {
                    return Err(FrontendError::Unsupported(format!(
                        "AccessChain step through pointee {current_pointee_id} \
                         with no struct layout or raw type info",
                    )));
                }
            }

            let result = alloc_or_get_result(result_id, result_ty.clone(),
                id_map, next_value_id);
            if let Some((dyn_idx_id, stride)) = dynamic_step {
                // Two-step emission: constant prefix
                // AccessChain produces an intermediate
                // pointer Value with the same result type;
                // PtrOffsetDynamic adds index*stride to it.
                let prefix_value = Value {
                    id: ValueId(*next_value_id),
                    ty: result_ty.clone(),
                };
                *next_value_id += 1;
                insts.push(Inst {
                    op: Op::AccessChain { base, byte_offset },
                    result: Some(prefix_value.clone()),
                    source_spirv_offset,
                });
                let dyn_idx_val = resolve_value(
                    dyn_idx_id, types, constants, id_map,
                    next_value_id, insts, source_spirv_offset,
                )?;
                insts.push(Inst {
                    op: Op::PtrOffsetDynamic {
                        base: prefix_value,
                        index: dyn_idx_val,
                        stride,
                    },
                    result: Some(result),
                    source_spirv_offset,
                });
            } else {
                insts.push(Inst {
                    op: Op::AccessChain { base, byte_offset },
                    result: Some(result),
                    source_spirv_offset,
                });
            }
            // Record where this pointer lands so a chained
            // AccessChain (`base = result_id`) can keep walking.
            ptr_pointee.insert(result_id, current_pointee_id);
            Ok(())
        }

        // OpLoad: load a leaf value through a pointer. The
        // pointer is either a bare Variable (offset 0) or an
        // AccessChain result.
        //
        // Special case: a Load whose result type is an
        // Image / Sampler / SampledImage isn't a memory
        // load — descriptor-bound resources don't have a
        // loadable byte region. We emit `Op::ImageHandle`
        // carrying the variable's `(set, binding)` instead,
        // so the backend / interpreter can resolve the
        // descriptor at the sample call site.
        SpvOp::Load => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "Load without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "Load without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let ptr_id = expect_id(&spv_inst.operands, 0)?;

            // Image / sampler / sampled-image load → ImageHandle.
            if matches!(&result_ty,
                atrium_spv_ir::Type::Image(_)
                | atrium_spv_ir::Type::Sampler
                | atrium_spv_ir::Type::SampledImage(_))
            {
                let (set, binding) = iface.var_binding.get(&ptr_id)
                    .copied()
                    .ok_or_else(|| FrontendError::Malformed(format!(
                        "Load of image/sampler variable {ptr_id} \
                         missing DescriptorSet+Binding decorations",
                    )))?;
                let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
                insts.push(Inst {
                    op: Op::ImageHandle { set, binding },
                    result: Some(result),
                    source_spirv_offset,
                });
                return Ok(());
            }

            // Built-in variable load: short-circuit to
            // `Op::LoadBuiltin`.  Backends produce the result
            // from stage-ABI parameters rather than a memory
            // load.
            if let Some(kind) = iface.builtin_vars.get(&ptr_id).copied() {
                let result = alloc_or_get_result(
                    result_id, result_ty, id_map, next_value_id);
                insts.push(Inst {
                    op: Op::LoadBuiltin(kind),
                    result: Some(result),
                    source_spirv_offset,
                });
                return Ok(());
            }

            // Try variable first; fall back to id_map (set
            // by a prior AccessChain).
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "Load pointer id {ptr_id} not a variable or AccessChain result",
                    )))?,
            };
            let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::Load(ptr_value),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpSampledImage: bundle an already-loaded image
        // value and an already-loaded sampler value into a
        // SampledImage value (no native instructions; the
        // backend just tracks the pair). The result type
        // must be a Type::SampledImage(dim).
        SpvOp::SampledImage => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "SampledImage without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "SampledImage without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id = expect_id(&spv_inst.operands, 0)?;
            let samp_id = expect_id(&spv_inst.operands, 1)?;
            let image = id_map.get(&img_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "SampledImage image operand id {img_id} not yet defined")))?;
            let sampler = id_map.get(&samp_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "SampledImage sampler operand id {samp_id} not yet defined")))?;
            let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::CombineSampledImage { image, sampler },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageSampleImplicitLod / ExplicitLod: filtered
        // texture sample. The first operand is a
        // SampledImage value (produced by OpSampledImage),
        // the second is the UV coord; ExplicitLod takes a
        // mip-level as operand 2.
        // OpImageSampleDref{Implicit,Explicit}Lod (Arc 40):
        // shadow samplers.  GLSL `texture(sampler2DShadow,
        // vec3(s, t, dref))` lowers in SPIR-V to:
        //   <op> <result_type=f32> <result_id>
        //        <sampled_image> <coord:vec2> <dref:f32>
        //        [<image_operands> ...]
        // We lower it entirely at the frontend as:
        //   r       = ImageSample{Implicit,Explicit}Lod(coord)
        //   r0      = VectorExtract(r, 0)
        //   cond    = FOrdLe(r0, dref)
        //   result  = Select(cond, 1.0, 0.0)
        // No new helpers, no backend changes.  Result type
        // must be F32 (the common shadow case); vec4 results
        // bail Unsupported.
        SpvOp::ImageSampleDrefImplicitLod
        | SpvOp::ImageSampleDrefExplicitLod
        | SpvOp::ImageSampleProjDrefImplicitLod
        | SpvOp::ImageSampleProjDrefExplicitLod => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageSampleDref without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageSampleDref without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            if !matches!(result_ty, Type::F32) {
                return Err(FrontendError::Unsupported(format!(
                    "ImageSampleDref result type {result_ty:?} \
                     (only F32 supported)")));
            }
            let si_id    = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let dref_id  = expect_id(&spv_inst.operands, 2)?;
            let sampled_image = id_map.get(&si_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageSampleDref sampled_image {si_id} not defined")))?;
            let raw_coord = id_map.get(&coord_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageSampleDref coord {coord_id} not defined")))?;
            let dref = id_map.get(&dref_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageSampleDref dref {dref_id} not defined")))?;

            // ProjDref variant: divide coord by its last lane
            // before sampling (same Proj math as Arc 37).
            let is_proj = matches!(spv_inst.class.opcode,
                SpvOp::ImageSampleProjDrefImplicitLod
                | SpvOp::ImageSampleProjDrefExplicitLod);
            let coord = if is_proj {
                let n = match &raw_coord.ty {
                    Type::Vec2(_) => 2,
                    Type::Vec3(_) => 3,
                    Type::Vec4(_) => 4,
                    other => return Err(FrontendError::Unsupported(format!(
                        "ImageSampleProjDref coord must be a vector, got {other:?}"))),
                };
                if n < 2 {
                    return Err(FrontendError::Unsupported(format!(
                        "ImageSampleProjDref coord needs ≥2 lanes, got {n}")));
                }
                let q_val = push_extract_lane(
                    raw_coord.clone(), (n - 1) as u32,
                    source_spirv_offset, insts, next_value_id)?;
                let mut lanes_div: Vec<Value> = Vec::with_capacity(n - 1);
                for i in 0..n - 1 {
                    let lane_i = push_extract_lane(
                        raw_coord.clone(), i as u32,
                        source_spirv_offset, insts, next_value_id)?;
                    let id = ValueId(*next_value_id);
                    *next_value_id += 1;
                    let div = Value { id, ty: Type::F32 };
                    insts.push(Inst {
                        op: Op::FDiv(lane_i, q_val.clone()),
                        result: Some(div.clone()),
                        source_spirv_offset,
                    });
                    lanes_div.push(div);
                }
                let new_ty = match n - 1 {
                    2 => Type::Vec2(VecElement::F32),
                    3 => Type::Vec3(VecElement::F32),
                    other => return Err(FrontendError::Unsupported(format!(
                        "ImageSampleProjDref unsupported divided lane count {other}"))),
                };
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let new_coord = Value { id, ty: new_ty };
                insts.push(Inst {
                    op: Op::ConstVec(lanes_div),
                    result: Some(new_coord.clone()),
                    source_spirv_offset,
                });
                new_coord
            } else {
                raw_coord
            };

            // Emit a first-class `ImageSampleDref` op.  The
            // backend lowers it to a runtime helper
            // (`atrium_tex_sample_2d_dref`) that performs the
            // sample + depth comparison using the SAMPLER's
            // runtime `compareOp` and PCF filtering -- both
            // are sampler state invisible to this compiler,
            // so they can't be synthesised inline here.  The
            // ExplicitLod variants' LOD operand is accepted
            // but the helper samples the base level for now
            // (shadow maps are typically single-mip).
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ImageSampleDref { sampled_image, coord, dref },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        SpvOp::ImageSampleImplicitLod | SpvOp::ImageSampleExplicitLod
        | SpvOp::ImageSampleProjImplicitLod
        | SpvOp::ImageSampleProjExplicitLod => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageSample without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageSample without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let si_id = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let sampled_image = id_map.get(&si_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageSample sampled_image id {si_id} not yet defined")))?;
            let raw_coord = id_map.get(&coord_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageSample coord id {coord_id} not yet defined")))?;
            // Arc 37: ImageSampleProj{Implicit,Explicit}Lod
            // -- projective texturing.  The coord is one lane
            // wider than the non-Proj variant (e.g. vec3 for
            // 2D); the last lane is the projection denominator
            // `q`.  Lower at the frontend by emitting per-lane
            // FDiv + a CompositeConstruct of the divided lanes,
            // then dispatch as a normal sample with the new
            // coord.  No backend changes needed.
            let is_proj = matches!(spv_inst.class.opcode,
                SpvOp::ImageSampleProjImplicitLod
                | SpvOp::ImageSampleProjExplicitLod);
            let coord = if is_proj {
                let n = match &raw_coord.ty {
                    Type::Vec2(_) => 2,
                    Type::Vec3(_) => 3,
                    Type::Vec4(_) => 4,
                    other => return Err(FrontendError::Unsupported(format!(
                        "ImageSampleProj* coord must be a vector, got {other:?}"))),
                };
                if n < 2 {
                    return Err(FrontendError::Unsupported(format!(
                        "ImageSampleProj* coord needs ≥2 lanes, got {n}")));
                }
                // Pull each lane out and divide by lane n-1 (q).
                let mut lanes_div: Vec<Value> = Vec::with_capacity(n - 1);
                // Extract q first; reused as RHS for every divide.
                let q_val = push_extract_lane(
                    raw_coord.clone(), (n - 1) as u32,
                    source_spirv_offset, insts, next_value_id)?;
                for i in 0..n - 1 {
                    let lane_i = push_extract_lane(
                        raw_coord.clone(), i as u32,
                        source_spirv_offset, insts, next_value_id)?;
                    let id = ValueId(*next_value_id);
                    *next_value_id += 1;
                    let div = Value { id, ty: Type::F32 };
                    insts.push(Inst {
                        op: Op::FDiv(lane_i, q_val.clone()),
                        result: Some(div.clone()),
                        source_spirv_offset,
                    });
                    lanes_div.push(div);
                }
                // Build new coord = vec(n-1) of the divided lanes.
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let new_ty = match n - 1 {
                    2 => Type::Vec2(VecElement::F32),
                    3 => Type::Vec3(VecElement::F32),
                    other => return Err(FrontendError::Unsupported(format!(
                        "ImageSampleProj* unsupported divided lane count {other}"))),
                };
                let new_coord = Value { id, ty: new_ty };
                insts.push(Inst {
                    op: Op::ConstVec(lanes_div),
                    result: Some(new_coord.clone()),
                    source_spirv_offset,
                });
                new_coord
            } else {
                raw_coord
            };
            let op = if spv_inst.class.opcode == SpvOp::ImageSampleExplicitLod
                || spv_inst.class.opcode == SpvOp::ImageSampleProjExplicitLod
            {
                // ExplicitLod's LOD arrives via the Image
                // Operands mask at index 2 (mask) + index 3
                // (LOD IdRef), per the SPIR-V spec.  The mask
                // is required (Lod or Grad), so reach past it.
                let lod_id = extract_image_operand_lod(&spv_inst.operands, 2)?
                    .ok_or_else(|| FrontendError::Malformed(
                        "ImageSampleExplicitLod missing Image-Operands::Lod"
                        .to_string()))?;
                let lod = id_map.get(&lod_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "ImageSampleExplicitLod lod id {lod_id} not yet defined")))?;
                Op::ImageSampleExplicitLod { sampled_image, coord, lod }
            } else if let Some(bias_id) =
                extract_image_operand_bias(&spv_inst.operands, 2)?
            {
                // Arc 36: Image-Operands::Bias on implicit-LOD.
                // Our implicit LOD collapses to mip 0 (no quad
                // dispatch), so the bias *is* the effective
                // LOD.  Route through the ExplicitLod path and
                // let the existing `sample_2d_lod` / array /
                // cube helpers handle it.
                let bias = id_map.get(&bias_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "ImageSampleImplicitLod bias id {bias_id} not yet defined")))?;
                Op::ImageSampleExplicitLod { sampled_image, coord, lod: bias }
            } else {
                Op::ImageSampleImplicitLod { sampled_image, coord }
            };
            let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
            insts.push(Inst { op, result: Some(result), source_spirv_offset });
            Ok(())
        }

        // OpImageFetch: unfiltered integer-coord texel
        // load. First operand is an image (not a sampled-
        // image — fetch doesn't use the sampler), second is
        // the integer coord. Optional Lod via the Image
        // Operands mask is read from operand index 2+.
        // OpDPdx / OpDPdy / OpFwidth (+ Fine / Coarse
        // variants) -- pixel-quad derivatives.  Lowered to a
        // first-class `Op::Derivative` per scalar lane, which
        // the backend routes to a runtime helper; the
        // rasterizer's 2x2-quad re-execution path supplies
        // the real lane-difference (Fine/Coarse aren't
        // distinguished -- both use the quad difference).
        SpvOp::DPdx | SpvOp::DPdy | SpvOp::Fwidth
        | SpvOp::DPdxFine | SpvOp::DPdyFine | SpvOp::FwidthFine
        | SpvOp::DPdxCoarse | SpvOp::DPdyCoarse | SpvOp::FwidthCoarse => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "derivative op without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "derivative op without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let axis: u8 = match spv_inst.class.opcode {
                SpvOp::DPdx | SpvOp::DPdxFine | SpvOp::DPdxCoarse => 0,
                SpvOp::DPdy | SpvOp::DPdyFine | SpvOp::DPdyCoarse => 1,
                _ => 2, // Fwidth*
            };
            let x_id = expect_id(&spv_inst.operands, 0)?;
            let x = resolve_value(x_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // Helper: one Derivative op per scalar lane.  The
            // `site` id (the lane's result ValueId) keys the
            // quad operand store, so each lane records/reads
            // its own operand.
            let mut emit_lane = |lane_val: Value,
                                 next_value_id: &mut u32,
                                 insts: &mut Vec<Inst>| -> Value {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let r = Value { id, ty: Type::F32 };
                insts.push(Inst {
                    op: Op::Derivative { value: lane_val, site: id.0 as u32, axis },
                    result: Some(r.clone()),
                    source_spirv_offset,
                });
                r
            };
            match &result_ty {
                Type::F32 => {
                    let result = alloc_or_get_result(
                        result_id, result_ty.clone(), id_map, next_value_id);
                    insts.push(Inst {
                        op: Op::Derivative {
                            value: x, site: result.id.0 as u32, axis },
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                Type::Vec2(_) | Type::Vec3(_) | Type::Vec4(_) => {
                    let n = match &result_ty {
                        Type::Vec2(_) => 2,
                        Type::Vec3(_) => 3,
                        _ => 4,
                    };
                    let mut lanes = Vec::with_capacity(n);
                    for i in 0..n {
                        let xi = push_extract_lane(
                            x.clone(), i as u32,
                            source_spirv_offset, insts, next_value_id)?;
                        lanes.push(emit_lane(xi, next_value_id, insts));
                    }
                    let result = alloc_or_get_result(
                        result_id, result_ty.clone(), id_map, next_value_id);
                    insts.push(Inst {
                        op: Op::ConstVec(lanes),
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                other => return Err(FrontendError::Unsupported(format!(
                    "derivative op with non-float result type {other:?}"))),
            }
            Ok(())
        }

        // OpImageGather: textureGather(sampler, coord,
        // component).  Operands: [sampled_image, coord,
        // component, (image-operands mask...)].
        // We translate the basic 3-operand form (no
        // Image-Operands::ConstOffset/Offsets refinement
        // yet -- those would adjust the gather footprint by
        // constant texel offsets, deferred).
        SpvOp::ImageGather => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageGather without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageGather without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id  = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let comp_id  = expect_id(&spv_inst.operands, 2)?;
            let sampled_image = id_map.get(&img_id).cloned()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "ImageGather image id {img_id} not yet defined")))?;
            let coord = resolve_value(coord_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let component = resolve_value(comp_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ImageGather { sampled_image, coord, component },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // Arc 54: OpImageDrefGather -- shadow gather.
        // textureGather(sampler2DShadow, P, refZ) returns a
        // vec4 whose lanes are the depth-compare results at
        // the four texels of the 2x2 footprint around P.
        //
        // Lower at the frontend by composing Arc 32 (ImageGather)
        // + Arc 40 (Dref compare):
        //   gather = ImageGather { component: 0 }(sampler, P)
        //   for k in 0..4:
        //     cond_k = FOrdLe(gather[k], dref)
        //     out_k  = Select(cond_k, 1.0, 0.0)
        //   result = ConstVec([out_0, out_1, out_2, out_3])
        SpvOp::ImageDrefGather => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageDrefGather without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageDrefGather without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id   = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let dref_id  = expect_id(&spv_inst.operands, 2)?;
            let sampled_image = id_map.get(&img_id).cloned()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "ImageDrefGather image id {img_id} not yet defined")))?;
            let coord = resolve_value(coord_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let dref = resolve_value(dref_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // (1) gather red channel at the 2x2 footprint.
            let component = push_ci(0, atrium_spv_ir::IntKind::I32,
                source_spirv_offset, insts, next_value_id);
            let gather_id = ValueId(*next_value_id);
            *next_value_id += 1;
            let gather_val = Value {
                id: gather_id,
                ty: Type::Vec4(VecElement::F32),
            };
            insts.push(Inst {
                op: Op::ImageGather {
                    sampled_image, coord, component,
                },
                result: Some(gather_val.clone()),
                source_spirv_offset,
            });
            // (2) per-lane compare + select.
            let one_val = {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: Type::F32 };
                insts.push(Inst {
                    op: Op::ConstFloat { value: 1.0,
                        kind: atrium_spv_ir::FloatKind::F32 },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                v
            };
            let zero_val = {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: Type::F32 };
                insts.push(Inst {
                    op: Op::ConstFloat { value: 0.0,
                        kind: atrium_spv_ir::FloatKind::F32 },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                v
            };
            let mut out_lanes: Vec<Value> = Vec::with_capacity(4);
            for k in 0..4 {
                let lane = push_extract_lane(
                    gather_val.clone(), k as u32,
                    source_spirv_offset, insts, next_value_id)?;
                let cond = push_bool(
                    Op::FOrdLe(lane, dref.clone()),
                    source_spirv_offset, insts, next_value_id);
                let sel_id = ValueId(*next_value_id);
                *next_value_id += 1;
                let sel = Value { id: sel_id, ty: Type::F32 };
                insts.push(Inst {
                    op: Op::Select {
                        cond,
                        t_val: one_val.clone(),
                        f_val: zero_val.clone(),
                    },
                    result: Some(sel.clone()),
                    source_spirv_offset,
                });
                out_lanes.push(sel);
            }
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstVec(out_lanes),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        SpvOp::ImageFetch => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageFetch without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageFetch without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let image = id_map.get(&img_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageFetch image id {img_id} not yet defined")))?;
            let coord = id_map.get(&coord_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageFetch coord id {coord_id} not yet defined")))?;
            // ImageFetch operand 2 (if present) is an Image
            // Operands mask; we don't decode `lod` yet (the
            // runtime helper ignores it in v1).  Arc 41 adds
            // ConstOffset / Offset support: if present, lift
            // the ivec2 offset id and emit per-lane IAdd to
            // form a new integer coord before dispatching.
            let lod = None;
            let offset_id = extract_image_operand_offset(&spv_inst.operands, 2)?;
            let coord = if let Some(off_id) = offset_id {
                let off = id_map.get(&off_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "ImageFetch offset id {off_id} not yet defined")))?;
                lane_add_int_vec(coord, off,
                    source_spirv_offset, insts, next_value_id)?
            } else {
                coord
            };
            let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ImageFetch { image, coord, lod },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageRead: unfiltered texel read from a storage
        // image.  Operands: [image, coord, (image-operands
        // mask, args...)].  Result is a vec4.
        // When `Image-Operands::Lod` is set we lift the Lod id
        // out and emit `Op::ImageReadLod` instead; otherwise
        // emit `Op::ImageRead`.
        SpvOp::ImageRead => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageRead without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageRead without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id   = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let image = id_map.get(&img_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageRead image id {img_id} not yet defined")))?;
            let coord = resolve_value(coord_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let lod_id = extract_image_operand_lod(&spv_inst.operands, 2)?;
            // Arc 43: Image-Operands::ConstOffset / Offset.
            let offset_id = extract_image_operand_offset(&spv_inst.operands, 2)?;
            let coord = if let Some(off_id) = offset_id {
                let off = id_map.get(&off_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "ImageRead offset id {off_id} not yet defined")))?;
                lane_add_int_vec(coord, off,
                    source_spirv_offset, insts, next_value_id)?
            } else {
                coord
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            let op = if let Some(lid) = lod_id {
                let lod = resolve_value(lid, types, constants, id_map,
                    next_value_id, insts, source_spirv_offset)?;
                Op::ImageReadLod { image, coord, lod }
            } else {
                Op::ImageRead { image, coord }
            };
            insts.push(Inst { op, result: Some(result), source_spirv_offset });
            Ok(())
        }

        // OpImageWrite: unfiltered texel write to a storage
        // image.  Operands: [image, coord, texel,
        // (image-operands mask, args...)].  No result.
        // Same Lod treatment as ImageRead.
        SpvOp::ImageWrite => {
            let img_id   = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let texel_id = expect_id(&spv_inst.operands, 2)?;
            let image = id_map.get(&img_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageWrite image id {img_id} not yet defined")))?;
            let coord = resolve_value(coord_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let texel = resolve_value(texel_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let lod_id = extract_image_operand_lod(&spv_inst.operands, 3)?;
            // Arc 43: Image-Operands::ConstOffset / Offset.
            let offset_id = extract_image_operand_offset(&spv_inst.operands, 3)?;
            let coord = if let Some(off_id) = offset_id {
                let off = id_map.get(&off_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "ImageWrite offset id {off_id} not yet defined")))?;
                lane_add_int_vec(coord, off,
                    source_spirv_offset, insts, next_value_id)?
            } else {
                coord
            };
            let op = if let Some(lid) = lod_id {
                let lod = resolve_value(lid, types, constants, id_map,
                    next_value_id, insts, source_spirv_offset)?;
                Op::ImageWriteLod { image, coord, texel, lod }
            } else {
                Op::ImageWrite { image, coord, texel }
            };
            insts.push(Inst { op, result: None, source_spirv_offset });
            Ok(())
        }

        // ── Subgroup / cooperative-group ops ──────────────
        //
        // Tier-2 runs each workgroup serially on one CPU
        // thread, so every workgroup contains exactly one
        // subgroup of size 1.  Every `OpGroupNonUniform*`
        // collapses to a trivial expression:
        //
        //   Elect / AllEqual                -> ConstantTrue
        //   All / Any / Broadcast{,First}   -> source value
        //   Shuffle / ShuffleXor / ShuffleUp / Down -> source
        //   Ballot(p)                       -> uvec4(p?1:0,0,0,0)
        //   InverseBallot(b)                -> (b.x & 1) != 0
        //   <op> Reduce / InclusiveScan     -> source value
        //   <op> ExclusiveScan              -> identity element
        //
        // The Execution-scope operand is not validated; the
        // SPIR-V spec requires it to be Subgroup (3), which
        // matches our subgroupSize=1 dispatch.  ClusteredReduce
        // is rejected (no cluster semantics with size 1).

        SpvOp::GroupNonUniformElect | SpvOp::GroupNonUniformAllEqual => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniformElect/AllEqual without result id".into()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniformElect/AllEqual without result type".into()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            // ConstInt 1 with result.ty == Bool registers in
            // both `ints` and `bools` per the bespoke Bitcast/
            // ConstInt fix.
            insts.push(Inst {
                op: Op::ConstInt { value: 1,
                    kind: atrium_spv_ir::IntKind::I32 },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::GroupNonUniformAll
        | SpvOp::GroupNonUniformAny
        | SpvOp::GroupNonUniformBroadcast
        | SpvOp::GroupNonUniformBroadcastFirst
        | SpvOp::GroupNonUniformShuffle
        | SpvOp::GroupNonUniformShuffleXor
        | SpvOp::GroupNonUniformShuffleUp
        | SpvOp::GroupNonUniformShuffleDown => {
            // Source value lives at operand index 1 (after the
            // Execution-scope at index 0).
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniform alias-op without result id".into()))?;
            let src_id = expect_id(&spv_inst.operands, 1)?;
            let src = resolve_value(src_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            id_map.insert(result_id, src);
            Ok(())
        }
        SpvOp::GroupNonUniformBallot => {
            // Ballot(p) at subgroup-size 1 -> uvec4(p?1:0, 0, 0, 0).
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniformBallot without result id".into()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniformBallot without result type".into()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let pred_id = expect_id(&spv_inst.operands, 1)?;
            let pred = resolve_value(pred_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let c_zero_u = push_ci(0, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            let c_one_u  = push_ci(1, atrium_spv_ir::IntKind::U32,
                source_spirv_offset, insts, next_value_id);
            // lane0 = pred ? 1u : 0u
            let lane0 = push_u32(
                Op::Select { cond: pred, t_val: c_one_u.clone(),
                    f_val: c_zero_u.clone() },
                source_spirv_offset, insts, next_value_id);
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstVec(vec![lane0,
                    c_zero_u.clone(), c_zero_u.clone(), c_zero_u]),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }
        SpvOp::GroupNonUniformIAdd
        | SpvOp::GroupNonUniformFAdd
        | SpvOp::GroupNonUniformIMul
        | SpvOp::GroupNonUniformFMul
        | SpvOp::GroupNonUniformSMin
        | SpvOp::GroupNonUniformUMin
        | SpvOp::GroupNonUniformFMin
        | SpvOp::GroupNonUniformSMax
        | SpvOp::GroupNonUniformUMax
        | SpvOp::GroupNonUniformFMax
        | SpvOp::GroupNonUniformBitwiseAnd
        | SpvOp::GroupNonUniformBitwiseOr
        | SpvOp::GroupNonUniformBitwiseXor
        | SpvOp::GroupNonUniformLogicalAnd
        | SpvOp::GroupNonUniformLogicalOr
        | SpvOp::GroupNonUniformLogicalXor => {
            // Operand layout: <scope> <operation> <value> [<cluster>]
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniform reduce/scan without result id".into()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "GroupNonUniform reduce/scan without result type".into()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let group_op = match spv_inst.operands.get(1) {
                Some(Operand::GroupOperation(op)) => *op as u32,
                Some(Operand::LiteralBit32(v)) => *v,
                other => return Err(FrontendError::Malformed(format!(
                    "GroupNonUniform reduce/scan operand[1] expected \
                     group-operation literal, got {other:?}"))),
            };
            let src_id = expect_id(&spv_inst.operands, 2)?;
            let src = resolve_value(src_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            // 0 = Reduce, 1 = InclusiveScan, 2 = ExclusiveScan,
            // 3 = ClusteredReduce.  At subgroupSize=1:
            //   Reduce / InclusiveScan -> source value (alias).
            //   ExclusiveScan          -> identity element.
            match group_op {
                0 | 1 => {
                    id_map.insert(result_id, src);
                }
                2 => {
                    // ExclusiveScan: emit the identity element
                    // appropriate to the result type + opcode.
                    let result = alloc_or_get_result(
                        result_id, result_ty.clone(), id_map,
                        next_value_id);
                    let id_op = identity_for_group_op(
                        spv_inst.class.opcode, &result_ty)?;
                    insts.push(Inst {
                        op: id_op,
                        result: Some(result),
                        source_spirv_offset,
                    });
                }
                3 => return Err(FrontendError::Unsupported(
                    "GroupNonUniform ClusteredReduce not supported \
                     at subgroupSize=1".into())),
                other => return Err(FrontendError::Malformed(format!(
                    "GroupNonUniform unknown group operation {other}"))),
            }
            Ok(())
        }

        // OpImageQuerySize: read width / height [/ depth]
        // off the ImageDesc.  The Image operand is a loaded
        // image (its result is an ImageHandle value already
        // in `id_map`, mirroring ImageRead/ImageWrite).
        SpvOp::ImageQuerySize => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQuerySize without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQuerySize without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id = expect_id(&spv_inst.operands, 0)?;
            let image = id_map.get(&img_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageQuerySize image id {img_id} not yet defined")))?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ImageQuerySize(image),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageQuerySizeLod: textureSize(sampler, lod).
        // Operand 0 is an OpTypeImage value (typically
        // extracted from a sampled image via OpImage, or a
        // direct image load).  Operand 1 is the integer LOD.
        // We emit Op::SampledImageQuerySizeLod which both
        // backends lower to direct width/height reads off
        // the TexDesc; the LOD is captured but ignored in v1
        // (single-mip TexDescs always read the base).
        SpvOp::ImageQuerySizeLod => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQuerySizeLod without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQuerySizeLod without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id = expect_id(&spv_inst.operands, 0)?;
            let lod_id = expect_id(&spv_inst.operands, 1)?;
            let image = id_map.get(&img_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageQuerySizeLod image id {img_id} not yet defined")))?;
            let lod = resolve_value(lod_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::SampledImageQuerySizeLod { image, lod },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageQueryLevels: count of mip levels in a
        // sampled image.  Atrium's Tier-2 sampled-image path
        // is single-mip in v1, so the result is a constant 1
        // regardless of which binding is queried.  No IR op
        // needed; emit a ConstInt and bind it to the result
        // id.  (Storage-image mip levels are queried via
        // OpImageQuerySize on the underlying ImageDesc;
        // sampled-image mip counts live behind the sampler
        // helpers which v1 doesn't yet pipe mip metadata
        // through.)
        SpvOp::ImageQueryLevels => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQueryLevels without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQueryLevels without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            // i32 or u32 result -- both lower the same.
            let kind = match result_ty {
                Type::U32 => atrium_spv_ir::IntKind::U32,
                _ => atrium_spv_ir::IntKind::I32,
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstInt { value: 1, kind },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageQueryLod (Arc 38): textureQueryLod(sampler, uv).
        // Returns a vec2(lod_from_derivatives, clamped_lod).
        // Atrium's Tier-2 has no 2x2 quad dispatch so the
        // derivatives are zero -> the LOD is zero too;
        // clamped LOD is also zero (mip 0 is always in range).
        // Both lanes lower to 0.0f.  No IR op needed.
        SpvOp::ImageQueryLod => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQueryLod without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQueryLod without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            // Synthesize the two f32 zero lanes.
            let mut lanes = Vec::with_capacity(2);
            for _ in 0..2 {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: Type::F32 };
                insts.push(Inst {
                    op: Op::ConstFloat {
                        value: 0.0,
                        kind: atrium_spv_ir::FloatKind::F32,
                    },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                lanes.push(v);
            }
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstVec(lanes),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageQuerySamples (Arc 38): textureSamples(image).
        // Returns the sample count; v1 has no MSAA so the
        // answer is the constant 1.  Same shape as the
        // ImageQueryLevels lowering above.
        SpvOp::ImageQuerySamples => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQuerySamples without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageQuerySamples without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let kind = match result_ty {
                Type::U32 => atrium_spv_ir::IntKind::U32,
                _ => atrium_spv_ir::IntKind::I32,
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ConstInt { value: 1, kind },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpImageTexelPointer: form a pointer to a single
        // storage-image texel so a subsequent atomic op can
        // read-modify-write it.  Operands:
        //   <result_type> <result_id> <image> <coord> <sample>
        // Per the SPIR-V spec the `image` operand is the image
        // *variable* (an OpTypePointer to OpTypeImage), not a
        // loaded image — so unlike ImageRead/Write we must
        // synthesise the `Op::ImageHandle` here from the
        // variable's (set, binding).  (A loaded image already
        // in `id_map` is also accepted for robustness.)
        // `sample` is 0 for non-MSAA images and ignored.
        SpvOp::ImageTexelPointer => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ImageTexelPointer without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ImageTexelPointer without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let img_id   = expect_id(&spv_inst.operands, 0)?;
            let coord_id = expect_id(&spv_inst.operands, 1)?;
            let image = match id_map.get(&img_id).cloned() {
                Some(v) => v,
                None => {
                    let (set, binding) = iface.var_binding.get(&img_id)
                        .copied()
                        .ok_or_else(|| FrontendError::Malformed(format!(
                            "ImageTexelPointer image variable {img_id} \
                             missing DescriptorSet+Binding decorations")))?;
                    // Result type of the synthetic ImageHandle is
                    // the variable's pointee (OpTypeImage) type.
                    let img_ty = iface.variables.get(&img_id)
                        .and_then(|(_, pointee)|
                            types.get(*pointee).ok().cloned())
                        .ok_or_else(|| FrontendError::Malformed(format!(
                            "ImageTexelPointer image variable {img_id} \
                             type not found")))?;
                    let handle = Value {
                        id: ValueId(*next_value_id),
                        ty: img_ty,
                    };
                    *next_value_id += 1;
                    insts.push(Inst {
                        op: Op::ImageHandle { set, binding },
                        result: Some(handle.clone()),
                        source_spirv_offset,
                    });
                    handle
                }
            };
            let coord = resolve_value(coord_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset)?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ImageTexelPointer { image, coord },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpAtomicIAdd: <result_type> <result_id> <pointer>
        //               <memory_scope> <memory_semantics> <value>
        // Memory scope + semantics are parsed but ignored:
        // the Tier-2 serial dispatcher executes invocations
        // one at a time, so the non-atomic load+add+store
        // lowering is semantically equivalent.
        // OpAtomic{IAdd,And,Or,Xor,Exchange}: same operand
        // layout per the SPIR-V spec --
        //   <result_type> <result_id> <pointer>
        //   <memory_scope> <memory_semantics> <value>
        // -- so handled by a shared resolution path, then
        // dispatched to the appropriate IR op constructor.
        // Memory scope + semantics are ignored on the serial
        // dispatcher (see Op::AtomicIAdd comment in spv-ir).
        // OpControlBarrier: synchronisation across all
        // invocations in a workgroup.  Tier-2's dispatcher
        // runs each invocation in its own OS thread for
        // workgroups with > 1 invocation (Arc 150+), so this
        // is no longer a no-op.  We emit Op::Barrier; the
        // backends lower to a call through the per-dispatch
        // image-table barrier slot, which the dispatcher
        // points at `atrium_spv_runtime::atrium_barrier`.
        //
        // The SPIR-V execution-scope operand (operand 0) is
        // a constant id whose value is a Scope enumerant:
        //   Workgroup = 2  -- the common case; emit Op::Barrier
        //   Subgroup  = 3  -- at subgroupSize=1 every barrier
        //                     is trivially satisfied; skip
        //   Device    = 1  -- tier-2 has no device-scope
        //                     dispatcher; reject as unsupported
        // Memory scope and semantics operands are decoded but
        // ignored (Op::Barrier covers every meaningful pair
        // for tier-2's single-threaded-per-lane model).
        SpvOp::ControlBarrier => {
            // OpControlBarrier's first operand is `Scope` (an
            // IdScope grammar kind in rspirv, not the more
            // common IdRef).  `expect_id` rejects IdScope, so
            // we read the operand directly here.  All three
            // operands (execution scope, memory scope, memory
            // semantics) are constant Scope/MemorySemantics ids
            // in well-formed SPIR-V; we only care about the
            // execution scope today (Arc 150).
            let exec_scope_id = match spv_inst.operands.first() {
                Some(Operand::IdScope(id)) => *id,
                Some(Operand::IdRef(id))   => *id, // tolerated
                other => return Err(FrontendError::Malformed(format!(
                    "OpControlBarrier expected IdScope at operand 0, got {other:?}",
                ))),
            };
            let exec_scope = match constants.get(exec_scope_id) {
                Some(c) => match &c.kind {
                    crate::constants::ConstantKind::Scalar(
                        Op::ConstInt { value, .. }
                    ) => *value as u32,
                    _ => return Err(FrontendError::Unsupported(format!(
                        "OpControlBarrier execution-scope id {exec_scope_id} \
                         resolves to non-integer constant"))),
                },
                None => return Err(FrontendError::Unsupported(format!(
                    "OpControlBarrier execution-scope id {exec_scope_id} \
                     not a constant -- non-constant scopes are not \
                     supported in phase 1 v3"))),
            };
            match exec_scope {
                2 => {
                    // Workgroup scope: real barrier.
                    insts.push(Inst {
                        op: Op::Barrier,
                        result: None,
                        source_spirv_offset,
                    });
                    Ok(())
                }
                3 => {
                    // Subgroup scope: at subgroupSize=1 every
                    // lane sees its own value, so no actual
                    // synchronisation needed.
                    Ok(())
                }
                other => Err(FrontendError::Unsupported(format!(
                    "OpControlBarrier execution-scope {other} not \
                     supported (Workgroup=2 and Subgroup=3 only)",
                ))),
            }
        }
        // OpMemoryBarrier: just a memory fence with no
        // execution-scope sync.  Tier-2's dispatcher
        // parallelises only across workgroups (within a
        // workgroup invocations either run serially or
        // share a real Barrier::wait), and atomics carry
        // their own ordering.  No code generation needed.
        SpvOp::MemoryBarrier => {
            Ok(())
        }

        // OpAtomicIIncrement / OpAtomicIDecrement: no value
        // operand -- shorthand for IAdd with implicit +/-1.
        // Synthesise the +1 / -1 constant inline so the IR
        // can reuse the existing Op::AtomicIAdd lowering.
        SpvOp::AtomicIIncrement | SpvOp::AtomicIDecrement => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicI{Increment,Decrement} without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicI{Increment,Decrement} without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let ptr_id = expect_id(&spv_inst.operands, 0)?;
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "AtomicI{{Increment,Decrement}} pointer id {ptr_id} not resolvable",
                    )))?,
            };
            // Materialise the +1 or -1 constant inline.
            // For Decrement we use the wrapping 0xFFFFFFFF
            // (= -1 in two's complement) so the same IAdd
            // path produces the subtraction.
            let const_val: i64 = if spv_inst.class.opcode == SpvOp::AtomicIIncrement {
                1
            } else {
                -1
            };
            let synth_value = {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: result_ty.clone() };
                insts.push(Inst {
                    op: Op::ConstInt {
                        value: const_val,
                        kind: atrium_spv_ir::IntKind::U32,
                    },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                v
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::AtomicIAdd { ptr: ptr_value, value: synth_value },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpAtomicISub: same operand layout as IAdd but the
        // value is subtracted.  We don't add Op::AtomicISub
        // (would duplicate codegen); instead negate the value
        // in the frontend (ISub a, b ≡ IAdd a, -b on two's
        // complement u32) and reuse Op::AtomicIAdd.
        SpvOp::AtomicISub => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicISub without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicISub without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let ptr_id = expect_id(&spv_inst.operands, 0)?;
            let value_id = expect_id(&spv_inst.operands, 3)?;
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "AtomicISub pointer id {ptr_id} not resolvable",
                    )))?,
            };
            let value = resolve_value(
                value_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset,
            )?;
            // Negate the value: synth ISub(0, value).
            let zero = {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: result_ty.clone() };
                insts.push(Inst {
                    op: Op::ConstInt {
                        value: 0,
                        kind: atrium_spv_ir::IntKind::U32,
                    },
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                v
            };
            let neg_value = {
                let id = ValueId(*next_value_id);
                *next_value_id += 1;
                let v = Value { id, ty: result_ty.clone() };
                insts.push(Inst {
                    op: Op::ISub(zero, value),
                    result: Some(v.clone()),
                    source_spirv_offset,
                });
                v
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::AtomicIAdd { ptr: ptr_value, value: neg_value },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpAtomicLoad: <result_type> <result_id> <pointer>
        //               <scope> <semantics>
        // No value operand.  Lowers to a plain load on the
        // serial dispatcher.
        SpvOp::AtomicLoad => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicLoad without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicLoad without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let ptr_id = expect_id(&spv_inst.operands, 0)?;
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "AtomicLoad pointer id {ptr_id} not resolvable",
                    )))?,
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::AtomicLoad(ptr_value),
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpAtomicStore: <pointer> <scope> <semantics> <value>
        // No result.  Lowers to a plain store.
        SpvOp::AtomicStore => {
            let ptr_id = expect_id(&spv_inst.operands, 0)?;
            let value_id = expect_id(&spv_inst.operands, 3)?;
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "AtomicStore pointer id {ptr_id} not resolvable",
                    )))?,
            };
            let value = resolve_value(
                value_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset,
            )?;
            insts.push(Inst {
                op: Op::AtomicStore { ptr: ptr_value, value },
                result: None,
                source_spirv_offset,
            });
            Ok(())
        }

        // OpAtomicCompareExchange:
        //   <result_type> <result_id> <pointer>
        //   <scope> <equal_semantics> <unequal_semantics>
        //   <value> <comparator>
        // Returns the OLD value at the pointer.  If the old
        // value equals `comparator`, writes `value`.
        // Arc 64: AtomicCompareExchangeWeak shares the exact
        // operand layout with AtomicCompareExchange and the
        // exact same semantics on every architecture we
        // target -- the "weak" qualifier permits spurious
        // failure (which we don't simulate; LSE CAS on ARM64
        // and lock cmpxchg on x86_64 are both strong).  Route
        // both opcodes to the same handler.
        SpvOp::AtomicCompareExchange
        | SpvOp::AtomicCompareExchangeWeak => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicCompareExchange without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "AtomicCompareExchange without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let ptr_id      = expect_id(&spv_inst.operands, 0)?;
            // 1..4 = scope, equal_sem, unequal_sem -- ignored.
            let value_id    = expect_id(&spv_inst.operands, 4)?;
            let cmp_id      = expect_id(&spv_inst.operands, 5)?;
            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "AtomicCompareExchange pointer id {ptr_id} not resolvable",
                    )))?,
            };
            let desired = resolve_value(
                value_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset,
            )?;
            let expected = resolve_value(
                cmp_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset,
            )?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::AtomicCompareExchange {
                    ptr: ptr_value, expected, desired,
                },
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        SpvOp::AtomicIAdd
        | SpvOp::AtomicAnd | SpvOp::AtomicOr | SpvOp::AtomicXor
        | SpvOp::AtomicSMin | SpvOp::AtomicSMax
        | SpvOp::AtomicUMin | SpvOp::AtomicUMax
        | SpvOp::AtomicExchange => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "Atomic op without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "Atomic op without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let ptr_id = expect_id(&spv_inst.operands, 0)?;
            // Operands 1 + 2: scope + semantics (id-refs to
            // scalar int constants); skipped.
            let value_id = expect_id(&spv_inst.operands, 3)?;

            let ptr_value = match resolve_variable(
                ptr_id, types, iface, id_map, next_value_id,
            )? {
                Some(v) => v,
                None => id_map.get(&ptr_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "Atomic pointer id {ptr_id} not a Variable \
                         and not in id_map (no prior AccessChain)",
                    )))?,
            };
            let value = resolve_value(
                value_id, types, constants, id_map,
                next_value_id, insts, source_spirv_offset,
            )?;
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            let op = match spv_inst.class.opcode {
                SpvOp::AtomicIAdd     => Op::AtomicIAdd     { ptr: ptr_value, value },
                SpvOp::AtomicAnd      => Op::AtomicAnd      { ptr: ptr_value, value },
                SpvOp::AtomicOr       => Op::AtomicOr       { ptr: ptr_value, value },
                SpvOp::AtomicXor      => Op::AtomicXor      { ptr: ptr_value, value },
                SpvOp::AtomicSMin     => Op::AtomicSMin     { ptr: ptr_value, value },
                SpvOp::AtomicSMax     => Op::AtomicSMax     { ptr: ptr_value, value },
                SpvOp::AtomicUMin     => Op::AtomicUMin     { ptr: ptr_value, value },
                SpvOp::AtomicUMax     => Op::AtomicUMax     { ptr: ptr_value, value },
                SpvOp::AtomicExchange => Op::AtomicExchange { ptr: ptr_value, value },
                _ => unreachable!(),
            };
            insts.push(Inst {
                op,
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        // OpExtInst: <result_type> <result_id> <set_id> <inst_enum> <operands...>
        // We support a subset of GLSL.std.450 math functions
        // -- the inst_enum value identifies which one.
        // Enums per the GLSL.std.450 spec.
        SpvOp::ExtInst => {
            let result_id = spv_inst.result_id.ok_or_else(||
                FrontendError::Malformed(
                    "ExtInst without result id".to_string()))?;
            let result_type_id = spv_inst.result_type.ok_or_else(||
                FrontendError::Malformed(
                    "ExtInst without result type".to_string()))?;
            let result_ty = types.get(result_type_id)?.clone();
            let set_id = expect_id(&spv_inst.operands, 0)?;
            if !iface.glsl_std_450_imports.contains(&set_id) {
                return Err(FrontendError::Unsupported(format!(
                    "ExtInst with set_id {set_id} not GLSL.std.450",
                )));
            }
            let inst_enum = match spv_inst.operands.get(1) {
                Some(Operand::LiteralExtInstInteger(n)) => *n,
                other => return Err(FrontendError::Malformed(format!(
                    "ExtInst missing inst_enum (got {other:?})",
                ))),
            };
            // GLSL.std.450 enums we handle:
            //   4 = FAbs    arg: x
            //   8 = Floor   arg: x
            //   9 = Ceil    arg: x
            //   3 = Trunc   arg: x
            //   31 = Sqrt   arg: x
            //   37 = FMin   args: x, y
            //   40 = FMax   args: x, y
            //   43 = FClamp args: x, lo, hi   (lowers to FMax+FMin)
            //   46 = FMix   args: x, y, a    (lowers to a*y + (1-a)*x;
            //                                  here as x + a*(y-x))
            let op = match inst_enum {
                4 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FAbs(x)
                }
                5 => {
                    // SAbs(x) -- integer absolute value.
                    // Standard branchless idiom:
                    //   m = x >> 31  (arithmetic; all-bits-set if x<0, 0 if x>=0)
                    //   result = (x ^ m) - m
                    // Synthesised via existing IR ops.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty };
                        insts.push(Inst { op, result: Some(v.clone()),
                            source_spirv_offset });
                        v
                    };
                    let c31 = push(
                        Op::ConstInt {
                            value: 31,
                            kind: atrium_spv_ir::IntKind::U32,
                        },
                        Type::U32, insts, next_value_id);
                    let mask = push(
                        Op::AShr(x.clone(), c31), result_ty.clone(),
                        insts, next_value_id);
                    let xored = push(
                        Op::BitXor(x, mask.clone()), result_ty.clone(),
                        insts, next_value_id);
                    Op::ISub(xored, mask)
                }
                8 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FFloor(x)
                }
                9 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FCeil(x)
                }
                3 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FTrunc(x)
                }
                13 => {
                    // Sin(x): range-reduce to [-π/2, π/2] then
                    // evaluate 4-term Horner Taylor polynomial
                    // and apply parity sign.  See
                    // synth_trig_reduce / synth_sin_poly.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let (x_red, sign) = synth_trig_reduce(
                        x, source_spirv_offset, insts, next_value_id);
                    let sin = synth_sin_poly(
                        x_red, source_spirv_offset, insts, next_value_id);
                    Op::FMul(sin, sign)
                }
                15 => {
                    // Tan(x) ≡ sin(x_red) / cos(x_red).  Parity
                    // signs cancel in the quotient, so the
                    // reduction's sign output is discarded.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let (x_red, _sign) = synth_trig_reduce(
                        x, source_spirv_offset, insts, next_value_id);
                    let sin = synth_sin_poly(
                        x_red.clone(), source_spirv_offset, insts, next_value_id);
                    let cos = synth_cos_poly(
                        x_red, source_spirv_offset, insts, next_value_id);
                    Op::FDiv(sin, cos)
                }
                14 => {
                    // Cos(x): range-reduce + 5-term Horner Taylor
                    // polynomial + parity sign.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let (x_red, sign) = synth_trig_reduce(
                        x, source_spirv_offset, insts, next_value_id);
                    let cos = synth_cos_poly(
                        x_red, source_spirv_offset, insts, next_value_id);
                    Op::FMul(cos, sign)
                }
                31 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FSqrt(x)
                }
                29 => {
                    // Exp2(x): synth via Horner Taylor + IEEE-754
                    // exponent reconstruction.  See synth_exp2.
                    // Returns the final value; we wrap as a
                    // no-op FMul by 1.0 to fit the arm-returns-Op
                    // shape (cheap; constant-folded by codegen).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let e = synth_exp2(x, source_spirv_offset, insts, next_value_id);
                    let c_one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    Op::FMul(e, c_one)
                }
                27 => {
                    // Exp(x) = Exp2(x * log2(e)).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c_log2e = push_cf(std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id);
                    let x_scaled = push_f32(Op::FMul(x, c_log2e),
                        source_spirv_offset, insts, next_value_id);
                    let e = synth_exp2(x_scaled, source_spirv_offset, insts, next_value_id);
                    let c_one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    Op::FMul(e, c_one)
                }
                30 => {
                    // Log2(x): mantissa-split + rational approx.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let l = synth_log2(x, source_spirv_offset, insts, next_value_id);
                    let c_one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    Op::FMul(l, c_one)
                }
                28 => {
                    // Log(x) = Log2(x) * ln(2).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let l = synth_log2(x, source_spirv_offset, insts, next_value_id);
                    let c_ln2 = push_cf(std::f64::consts::LN_2,
                        source_spirv_offset, insts, next_value_id);
                    Op::FMul(l, c_ln2)
                }
                18 => {
                    // Atan(x): full real line via reciprocal range reduction.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let a = synth_atan(x, source_spirv_offset, insts, next_value_id);
                    let c_one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    Op::FMul(a, c_one)
                }
                19 => {
                    // Sinh(x) = (Exp(x) - Exp(-x)) / 2
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c_log2e = push_cf(std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id);
                    let xl = push_f32(Op::FMul(x.clone(), c_log2e.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let neg_xl = push_f32(Op::FMul(x, push_cf(-std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id)),
                        source_spirv_offset, insts, next_value_id);
                    let ex = synth_exp2(xl, source_spirv_offset, insts, next_value_id);
                    let enx = synth_exp2(neg_xl, source_spirv_offset, insts, next_value_id);
                    let diff = push_f32(Op::FSub(ex, enx),
                        source_spirv_offset, insts, next_value_id);
                    let c_half = push_cf(0.5, source_spirv_offset, insts, next_value_id);
                    let _ = c_log2e;
                    Op::FMul(diff, c_half)
                }
                20 => {
                    // Cosh(x) = (Exp(x) + Exp(-x)) / 2
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c_log2e = push_cf(std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id);
                    let xl = push_f32(Op::FMul(x.clone(), c_log2e),
                        source_spirv_offset, insts, next_value_id);
                    let c_neg_log2e = push_cf(-std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id);
                    let neg_xl = push_f32(Op::FMul(x, c_neg_log2e),
                        source_spirv_offset, insts, next_value_id);
                    let ex = synth_exp2(xl, source_spirv_offset, insts, next_value_id);
                    let enx = synth_exp2(neg_xl, source_spirv_offset, insts, next_value_id);
                    let sum = push_f32(Op::FAdd(ex, enx),
                        source_spirv_offset, insts, next_value_id);
                    let c_half = push_cf(0.5, source_spirv_offset, insts, next_value_id);
                    Op::FMul(sum, c_half)
                }
                21 => {
                    // Tanh(x) = (Exp(x) - Exp(-x)) / (Exp(x) + Exp(-x))
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c_log2e = push_cf(std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id);
                    let xl = push_f32(Op::FMul(x.clone(), c_log2e),
                        source_spirv_offset, insts, next_value_id);
                    let c_neg_log2e = push_cf(-std::f64::consts::LOG2_E,
                        source_spirv_offset, insts, next_value_id);
                    let neg_xl = push_f32(Op::FMul(x, c_neg_log2e),
                        source_spirv_offset, insts, next_value_id);
                    let ex = synth_exp2(xl, source_spirv_offset, insts, next_value_id);
                    let enx = synth_exp2(neg_xl, source_spirv_offset, insts, next_value_id);
                    let num = push_f32(Op::FSub(ex.clone(), enx.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let den = push_f32(Op::FAdd(ex, enx),
                        source_spirv_offset, insts, next_value_id);
                    Op::FDiv(num, den)
                }
                22 => {
                    // Asinh(x) = Log(x + sqrt(x² + 1)).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let x2 = push_f32(Op::FMul(x.clone(), x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    let x2p1 = push_f32(Op::FAdd(x2, one),
                        source_spirv_offset, insts, next_value_id);
                    let s = push_f32(Op::FSqrt(x2p1),
                        source_spirv_offset, insts, next_value_id);
                    let arg = push_f32(Op::FAdd(x, s),
                        source_spirv_offset, insts, next_value_id);
                    let l = synth_log2(arg, source_spirv_offset, insts, next_value_id);
                    let c_ln2 = push_cf(std::f64::consts::LN_2,
                        source_spirv_offset, insts, next_value_id);
                    Op::FMul(l, c_ln2)
                }
                23 => {
                    // Acosh(x) = Log(x + sqrt(x² - 1)), x ≥ 1.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let x2 = push_f32(Op::FMul(x.clone(), x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    let x2m1 = push_f32(Op::FSub(x2, one),
                        source_spirv_offset, insts, next_value_id);
                    let s = push_f32(Op::FSqrt(x2m1),
                        source_spirv_offset, insts, next_value_id);
                    let arg = push_f32(Op::FAdd(x, s),
                        source_spirv_offset, insts, next_value_id);
                    let l = synth_log2(arg, source_spirv_offset, insts, next_value_id);
                    let c_ln2 = push_cf(std::f64::consts::LN_2,
                        source_spirv_offset, insts, next_value_id);
                    Op::FMul(l, c_ln2)
                }
                24 => {
                    // Atanh(x) = 0.5 * Log((1+x)/(1-x)),  x ∈ (-1, 1).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    let num = push_f32(Op::FAdd(one.clone(), x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let den = push_f32(Op::FSub(one, x),
                        source_spirv_offset, insts, next_value_id);
                    let ratio = push_f32(Op::FDiv(num, den),
                        source_spirv_offset, insts, next_value_id);
                    let l = synth_log2(ratio, source_spirv_offset, insts, next_value_id);
                    let c_half_ln2 = push_cf(0.5 * std::f64::consts::LN_2,
                        source_spirv_offset, insts, next_value_id);
                    Op::FMul(l, c_half_ln2)
                }
                2 => {
                    // RoundEven(x): round-half-to-even (banker's
                    // rounding).  ARM has FRINTN for this in one
                    // instruction but it's not in the IR; we
                    // synthesise the algorithm.
                    //
                    //   a       = x + 0.5
                    //   r       = floor(a)            // round-half-up
                    //   floor_x = floor(x)
                    //   frac    = x - floor_x          // in [0, 1)
                    //   is_tie  = (frac == 0.5)
                    //   r_i     = (i32) r
                    //   adj     = (is_tie ? (r_i & 1) : 0)
                    //   result  = (f32) (r_i - adj)
                    //
                    // The trick: only adjust on exact tie, and
                    // only when r_i is odd -- subtract 1 to
                    // round down to the even neighbour.  Works
                    // symmetrically for negatives (verified by
                    // hand: -2.5 -> -2, -1.5 -> -2, -0.5 -> 0).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let c_half = push_cf(0.5,
                        source_spirv_offset, insts, next_value_id);
                    let a = push(Op::FAdd(x.clone(), c_half.clone()),
                        Type::F32, insts, next_value_id);
                    let r = push(Op::FFloor(a),
                        Type::F32, insts, next_value_id);
                    let floor_x = push(Op::FFloor(x.clone()),
                        Type::F32, insts, next_value_id);
                    let frac = push(Op::FSub(x, floor_x),
                        Type::F32, insts, next_value_id);
                    let is_tie = push(Op::FOrdEq(frac, c_half),
                        Type::Bool, insts, next_value_id);
                    let r_i = push(Op::ConvertFToS(r),
                        Type::I32, insts, next_value_id);
                    let c_one_i = push_ci(1, atrium_spv_ir::IntKind::I32,
                        source_spirv_offset, insts, next_value_id);
                    let c_zero_i = push_ci(0, atrium_spv_ir::IntKind::I32,
                        source_spirv_offset, insts, next_value_id);
                    let r_low_bit = push(Op::BitAnd(r_i.clone(), c_one_i),
                        Type::I32, insts, next_value_id);
                    let adj = push(Op::Select {
                            cond: is_tie,
                            t_val: r_low_bit,
                            f_val: c_zero_i,
                        },
                        Type::I32, insts, next_value_id);
                    let r_adjusted_i = push(Op::ISub(r_i, adj),
                        Type::I32, insts, next_value_id);
                    Op::ConvertSToF(r_adjusted_i)
                }
                55 => {
                    // PackUnorm4x8(v):  vec4 in [0, 1] -> u32.
                    //   per-lane: floor(clamp(c, 0, 1) * 255 + 0.5) & 0xFF
                    //   result = l0 | (l1<<8) | (l2<<16) | (l3<<24)
                    let v_id = expect_id(&spv_inst.operands, 2)?;
                    let v = resolve_value(v_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let lanes: [Value; 4] = [
                        push(Op::VectorExtract { vector: v.clone(), index: 0 },
                            Type::F32, insts, next_value_id),
                        push(Op::VectorExtract { vector: v.clone(), index: 1 },
                            Type::F32, insts, next_value_id),
                        push(Op::VectorExtract { vector: v.clone(), index: 2 },
                            Type::F32, insts, next_value_id),
                        push(Op::VectorExtract { vector: v, index: 3 },
                            Type::F32, insts, next_value_id),
                    ];
                    let c_zero_f = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_one_f = push_cf(1.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_255_f = push_cf(255.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_half = push_cf(0.5,
                        source_spirv_offset, insts, next_value_id);
                    let c_mask_byte = push_ci(0xFF, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let quantise = |
                        lane: Value, zero: &Value, one: &Value,
                        scale: &Value, half: &Value, mask: &Value,
                        insts: &mut Vec<Inst>,
                        next_value_id: &mut u32,
                    | -> Value {
                        let lo = push(Op::FMax(lane, zero.clone()),
                            Type::F32, insts, next_value_id);
                        let cl = push(Op::FMin(lo, one.clone()),
                            Type::F32, insts, next_value_id);
                        let scaled = push(Op::FMul(cl, scale.clone()),
                            Type::F32, insts, next_value_id);
                        let plus_half = push(Op::FAdd(scaled, half.clone()),
                            Type::F32, insts, next_value_id);
                        let floored = push(Op::FFloor(plus_half),
                            Type::F32, insts, next_value_id);
                        let as_u32 = push(Op::ConvertFToU(floored),
                            Type::U32, insts, next_value_id);
                        push(Op::BitAnd(as_u32, mask.clone()),
                            Type::U32, insts, next_value_id)
                    };
                    let q: [Value; 4] = [
                        quantise(lanes[0].clone(), &c_zero_f, &c_one_f,
                            &c_255_f, &c_half, &c_mask_byte,
                            insts, next_value_id),
                        quantise(lanes[1].clone(), &c_zero_f, &c_one_f,
                            &c_255_f, &c_half, &c_mask_byte,
                            insts, next_value_id),
                        quantise(lanes[2].clone(), &c_zero_f, &c_one_f,
                            &c_255_f, &c_half, &c_mask_byte,
                            insts, next_value_id),
                        quantise(lanes[3].clone(), &c_zero_f, &c_one_f,
                            &c_255_f, &c_half, &c_mask_byte,
                            insts, next_value_id),
                    ];
                    let c_8 = push_ci(8, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_24 = push_ci(24, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let s1 = push(Op::Shl(q[1].clone(), c_8),
                        Type::U32, insts, next_value_id);
                    let s2 = push(Op::Shl(q[2].clone(), c_16),
                        Type::U32, insts, next_value_id);
                    let s3 = push(Op::Shl(q[3].clone(), c_24),
                        Type::U32, insts, next_value_id);
                    let a = push(Op::BitOr(q[0].clone(), s1),
                        Type::U32, insts, next_value_id);
                    let b_ = push(Op::BitOr(a, s2),
                        Type::U32, insts, next_value_id);
                    Op::BitOr(b_, s3)
                }
                54 => {
                    // PackSnorm4x8(v):  vec4 in [-1, 1] -> u32.
                    //   per-lane signed quantise (sign-biased
                    //   rounding) to i8, masked to 0xFF.
                    let v_id = expect_id(&spv_inst.operands, 2)?;
                    let v = resolve_value(v_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let lanes: [Value; 4] = [
                        push(Op::VectorExtract { vector: v.clone(), index: 0 },
                            Type::F32, insts, next_value_id),
                        push(Op::VectorExtract { vector: v.clone(), index: 1 },
                            Type::F32, insts, next_value_id),
                        push(Op::VectorExtract { vector: v.clone(), index: 2 },
                            Type::F32, insts, next_value_id),
                        push(Op::VectorExtract { vector: v, index: 3 },
                            Type::F32, insts, next_value_id),
                    ];
                    let c_neg1 = push_cf(-1.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_one_f = push_cf(1.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_zero_f = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_127_f = push_cf(127.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_hp = push_cf(0.5,
                        source_spirv_offset, insts, next_value_id);
                    let c_hn = push_cf(-0.5,
                        source_spirv_offset, insts, next_value_id);
                    let c_mask_byte = push_ci(0xFF, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let quantise = |
                        lane: Value,
                        neg1: &Value, one: &Value, zero: &Value,
                        scale: &Value, hp: &Value, hn: &Value,
                        mask: &Value,
                        insts: &mut Vec<Inst>,
                        next_value_id: &mut u32,
                    | -> Value {
                        let hi = push(Op::FMin(lane, one.clone()),
                            Type::F32, insts, next_value_id);
                        let cl = push(Op::FMax(hi, neg1.clone()),
                            Type::F32, insts, next_value_id);
                        let scaled = push(Op::FMul(cl, scale.clone()),
                            Type::F32, insts, next_value_id);
                        let is_neg = push(
                            Op::FOrdLt(scaled.clone(), zero.clone()),
                            Type::Bool, insts, next_value_id);
                        let bias = push(
                            Op::Select { cond: is_neg,
                                t_val: hn.clone(), f_val: hp.clone() },
                            Type::F32, insts, next_value_id);
                        let biased = push(Op::FAdd(scaled, bias),
                            Type::F32, insts, next_value_id);
                        let as_i = push(Op::ConvertFToS(biased),
                            Type::I32, insts, next_value_id);
                        push(Op::BitAnd(as_i, mask.clone()),
                            Type::U32, insts, next_value_id)
                    };
                    let q: [Value; 4] = [
                        quantise(lanes[0].clone(), &c_neg1, &c_one_f, &c_zero_f,
                            &c_127_f, &c_hp, &c_hn, &c_mask_byte,
                            insts, next_value_id),
                        quantise(lanes[1].clone(), &c_neg1, &c_one_f, &c_zero_f,
                            &c_127_f, &c_hp, &c_hn, &c_mask_byte,
                            insts, next_value_id),
                        quantise(lanes[2].clone(), &c_neg1, &c_one_f, &c_zero_f,
                            &c_127_f, &c_hp, &c_hn, &c_mask_byte,
                            insts, next_value_id),
                        quantise(lanes[3].clone(), &c_neg1, &c_one_f, &c_zero_f,
                            &c_127_f, &c_hp, &c_hn, &c_mask_byte,
                            insts, next_value_id),
                    ];
                    let c_8 = push_ci(8, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_24 = push_ci(24, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let s1 = push(Op::Shl(q[1].clone(), c_8),
                        Type::U32, insts, next_value_id);
                    let s2 = push(Op::Shl(q[2].clone(), c_16),
                        Type::U32, insts, next_value_id);
                    let s3 = push(Op::Shl(q[3].clone(), c_24),
                        Type::U32, insts, next_value_id);
                    let a = push(Op::BitOr(q[0].clone(), s1),
                        Type::U32, insts, next_value_id);
                    let b_ = push(Op::BitOr(a, s2),
                        Type::U32, insts, next_value_id);
                    Op::BitOr(b_, s3)
                }
                63 => {
                    // UnpackUnorm4x8(u): u32 -> vec4 in [0, 1].
                    //   byte_i = (u >> (8*i)) & 0xFF
                    //   lane_i = byte_i / 255.0
                    let u_id = expect_id(&spv_inst.operands, 2)?;
                    let u = resolve_value(u_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let c_mask = push_ci(0xFF, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_8 = push_ci(8, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_24 = push_ci(24, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_255_f = push_cf(255.0,
                        source_spirv_offset, insts, next_value_id);
                    let shifted: [Value; 4] = [
                        u.clone(),
                        push(Op::LShr(u.clone(), c_8),
                            Type::U32, insts, next_value_id),
                        push(Op::LShr(u.clone(), c_16),
                            Type::U32, insts, next_value_id),
                        push(Op::LShr(u, c_24),
                            Type::U32, insts, next_value_id),
                    ];
                    let mut lane_fs: Vec<Value> = Vec::with_capacity(4);
                    for s in &shifted {
                        let masked = push(Op::BitAnd(s.clone(), c_mask.clone()),
                            Type::U32, insts, next_value_id);
                        let as_f = push(Op::ConvertUToF(masked),
                            Type::F32, insts, next_value_id);
                        let lane = push(Op::FDiv(as_f, c_255_f.clone()),
                            Type::F32, insts, next_value_id);
                        lane_fs.push(lane);
                    }
                    Op::ConstVec(lane_fs)
                }
                64 => {
                    // UnpackSnorm4x8(u): u32 -> vec4 in [-1, 1].
                    //   sign-extend each byte by Shl(u, 24-8*i)
                    //   then AShr by 24:
                    //     byte_0 = AShr(Shl(u, 24), 24)
                    //     byte_1 = AShr(Shl(u, 16), 24)
                    //     byte_2 = AShr(Shl(u,  8), 24)
                    //     byte_3 = AShr(u, 24)
                    //   lane_i = max(byte_i / 127.0, -1.0)
                    let u_id = expect_id(&spv_inst.operands, 2)?;
                    let u = resolve_value(u_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let c_8 = push_ci(8, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_24 = push_ci(24, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_127_f = push_cf(127.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_neg1 = push_cf(-1.0,
                        source_spirv_offset, insts, next_value_id);
                    let signed_bytes: [Value; 4] = [
                        {
                            let sh = push(Op::Shl(u.clone(), c_24.clone()),
                                Type::I32, insts, next_value_id);
                            push(Op::AShr(sh, c_24.clone()),
                                Type::I32, insts, next_value_id)
                        },
                        {
                            let sh = push(Op::Shl(u.clone(), c_16),
                                Type::I32, insts, next_value_id);
                            push(Op::AShr(sh, c_24.clone()),
                                Type::I32, insts, next_value_id)
                        },
                        {
                            let sh = push(Op::Shl(u.clone(), c_8),
                                Type::I32, insts, next_value_id);
                            push(Op::AShr(sh, c_24.clone()),
                                Type::I32, insts, next_value_id)
                        },
                        push(Op::AShr(u, c_24),
                            Type::I32, insts, next_value_id),
                    ];
                    let mut lane_fs: Vec<Value> = Vec::with_capacity(4);
                    for s in &signed_bytes {
                        let as_f = push(Op::ConvertSToF(s.clone()),
                            Type::F32, insts, next_value_id);
                        let raw = push(Op::FDiv(as_f, c_127_f.clone()),
                            Type::F32, insts, next_value_id);
                        let cl = push(Op::FMax(raw, c_neg1.clone()),
                            Type::F32, insts, next_value_id);
                        lane_fs.push(cl);
                    }
                    Op::ConstVec(lane_fs)
                }
                56 => {
                    // PackSnorm2x16(v):  vec2 in [-1, 1] -> u32.
                    //   fixed_x = round(clamp(v.x, -1, 1) * 32767)
                    //   fixed_y = round(clamp(v.y, -1, 1) * 32767)
                    //   result  = (fixed_x & 0xFFFF) |
                    //             ((fixed_y & 0xFFFF) << 16)
                    // Round via `(int)(scaled + copysign(0.5, scaled))`,
                    // implemented as `s + sign(s)*0.5` where
                    // sign(s) is the FSign synthesis (-1, 0, +1).
                    let v_id = expect_id(&spv_inst.operands, 2)?;
                    let v = resolve_value(v_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let lane_x = push(
                        Op::VectorExtract { vector: v.clone(), index: 0 },
                        Type::F32, insts, next_value_id);
                    let lane_y = push(
                        Op::VectorExtract { vector: v, index: 1 },
                        Type::F32, insts, next_value_id);
                    let c_neg1 = push_cf(-1.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_one_f = push_cf(1.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_zero_f = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_32767_f = push_cf(32767.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_half_pos = push_cf(0.5,
                        source_spirv_offset, insts, next_value_id);
                    let c_half_neg = push_cf(-0.5,
                        source_spirv_offset, insts, next_value_id);
                    let c_mask = push_ci(0xFFFF, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    // quantise one lane:
                    //   c   = clamp(lane, -1, 1) * 32767
                    //   bias = (c >= 0) ? +0.5 : -0.5
                    //   r    = (i32)(c + bias)   (truncate-to-zero)
                    //   pack = r & 0xFFFF
                    let quantise = |
                        lane: Value,
                        neg1: &Value, one: &Value, zero: &Value,
                        scale: &Value, hp: &Value, hn: &Value,
                        mask: &Value,
                        insts: &mut Vec<Inst>,
                        next_value_id: &mut u32,
                    | -> Value {
                        let hi = push(
                            Op::FMin(lane, one.clone()),
                            Type::F32, insts, next_value_id);
                        let cl = push(
                            Op::FMax(hi, neg1.clone()),
                            Type::F32, insts, next_value_id);
                        let scaled = push(
                            Op::FMul(cl, scale.clone()),
                            Type::F32, insts, next_value_id);
                        let is_neg = push(
                            Op::FOrdLt(scaled.clone(), zero.clone()),
                            Type::Bool, insts, next_value_id);
                        let bias = push(
                            Op::Select {
                                cond: is_neg,
                                t_val: hn.clone(),
                                f_val: hp.clone(),
                            },
                            Type::F32, insts, next_value_id);
                        let biased = push(
                            Op::FAdd(scaled, bias),
                            Type::F32, insts, next_value_id);
                        let as_i32 = push(
                            Op::ConvertFToS(biased),
                            Type::I32, insts, next_value_id);
                        push(
                            Op::BitAnd(as_i32, mask.clone()),
                            Type::U32, insts, next_value_id)
                    };
                    let fx = quantise(lane_x, &c_neg1, &c_one_f, &c_zero_f,
                        &c_32767_f, &c_half_pos, &c_half_neg, &c_mask,
                        insts, next_value_id);
                    let fy = quantise(lane_y, &c_neg1, &c_one_f, &c_zero_f,
                        &c_32767_f, &c_half_pos, &c_half_neg, &c_mask,
                        insts, next_value_id);
                    let hi_shifted = push(
                        Op::Shl(fy, c_16),
                        Type::U32, insts, next_value_id);
                    Op::BitOr(fx, hi_shifted)
                }
                60 => {
                    // UnpackSnorm2x16(u): u32 -> vec2 in [-1, 1].
                    //   low_i  = sign-extended low 16 bits of u
                    //          = (u << 16) >> 16   (arithmetic)
                    //   high_i = sign-extended high 16 bits
                    //          = u >> 16          (arithmetic)
                    //   lane_x = max(low_i / 32767, -1)
                    //   lane_y = max(high_i / 32767, -1)
                    let u_id = expect_id(&spv_inst.operands, 2)?;
                    let u = resolve_value(u_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_32767_f = push_cf(32767.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_neg1 = push_cf(-1.0,
                        source_spirv_offset, insts, next_value_id);
                    let shifted_up = push(
                        Op::Shl(u.clone(), c_16.clone()),
                        Type::I32, insts, next_value_id);
                    let low_i = push(
                        Op::AShr(shifted_up, c_16.clone()),
                        Type::I32, insts, next_value_id);
                    let high_i = push(
                        Op::AShr(u, c_16),
                        Type::I32, insts, next_value_id);
                    let low_f = push(
                        Op::ConvertSToF(low_i),
                        Type::F32, insts, next_value_id);
                    let high_f = push(
                        Op::ConvertSToF(high_i),
                        Type::F32, insts, next_value_id);
                    let raw_x = push(
                        Op::FDiv(low_f, c_32767_f.clone()),
                        Type::F32, insts, next_value_id);
                    let raw_y = push(
                        Op::FDiv(high_f, c_32767_f),
                        Type::F32, insts, next_value_id);
                    let lane_x = push(
                        Op::FMax(raw_x, c_neg1.clone()),
                        Type::F32, insts, next_value_id);
                    let lane_y = push(
                        Op::FMax(raw_y, c_neg1),
                        Type::F32, insts, next_value_id);
                    Op::ConstVec(vec![lane_x, lane_y])
                }
                57 => {
                    // PackUnorm2x16(v):  vec2 in [0,1] -> u32.
                    //   fixed_x = round(clamp(v.x, 0, 1) * 65535)
                    //   fixed_y = round(clamp(v.y, 0, 1) * 65535)
                    //   result  = (fixed_x & 0xFFFF) |
                    //             ((fixed_y & 0xFFFF) << 16)
                    // GLSL.std.450 round-to-nearest-even is not
                    // expressible without FRINTN; we use the
                    // `floor(x + 0.5)` round-half-away-from-zero
                    // approximation, which is bit-exact for all
                    // representable [0, 65535.5) inputs (no
                    // ties land on .5 within unorm precision).
                    let v_id = expect_id(&spv_inst.operands, 2)?;
                    let v = resolve_value(v_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let lane_x = push(
                        Op::VectorExtract { vector: v.clone(), index: 0 },
                        Type::F32, insts, next_value_id);
                    let lane_y = push(
                        Op::VectorExtract { vector: v, index: 1 },
                        Type::F32, insts, next_value_id);
                    let c_zero_f = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_one_f = push_cf(1.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_65535_f = push_cf(65535.0,
                        source_spirv_offset, insts, next_value_id);
                    let c_half = push_cf(0.5,
                        source_spirv_offset, insts, next_value_id);
                    let c_mask = push_ci(0xFFFF, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    // Per-lane quantise helper.
                    let quantise = |
                        lane: Value,
                        zero: &Value,
                        one: &Value,
                        scale: &Value,
                        half: &Value,
                        mask: &Value,
                        insts: &mut Vec<Inst>,
                        next_value_id: &mut u32,
                    | -> Value {
                        let lo = push(
                            Op::FMax(lane, zero.clone()),
                            Type::F32, insts, next_value_id);
                        let cl = push(
                            Op::FMin(lo, one.clone()),
                            Type::F32, insts, next_value_id);
                        let scaled = push(
                            Op::FMul(cl, scale.clone()),
                            Type::F32, insts, next_value_id);
                        let plus_half = push(
                            Op::FAdd(scaled, half.clone()),
                            Type::F32, insts, next_value_id);
                        let floored = push(
                            Op::FFloor(plus_half),
                            Type::F32, insts, next_value_id);
                        let as_u32 = push(
                            Op::ConvertFToU(floored),
                            Type::U32, insts, next_value_id);
                        push(
                            Op::BitAnd(as_u32, mask.clone()),
                            Type::U32, insts, next_value_id)
                    };
                    let fx = quantise(lane_x, &c_zero_f, &c_one_f,
                        &c_65535_f, &c_half, &c_mask,
                        insts, next_value_id);
                    let fy = quantise(lane_y, &c_zero_f, &c_one_f,
                        &c_65535_f, &c_half, &c_mask,
                        insts, next_value_id);
                    let hi_shifted = push(
                        Op::Shl(fy, c_16),
                        Type::U32, insts, next_value_id);
                    Op::BitOr(fx, hi_shifted)
                }
                61 => {
                    // UnpackUnorm2x16(u): u32 -> vec2 in [0, 1].
                    //   low_u  = u & 0xFFFF
                    //   high_u = u >> 16
                    //   vec2(low_u / 65535.0, high_u / 65535.0)
                    let u_id = expect_id(&spv_inst.operands, 2)?;
                    let u = resolve_value(u_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let c_mask = push_ci(0xFFFF, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_16 = push_ci(16, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let c_65535_f = push_cf(65535.0,
                        source_spirv_offset, insts, next_value_id);
                    let low_u = push(
                        Op::BitAnd(u.clone(), c_mask),
                        Type::U32, insts, next_value_id);
                    let high_u = push(
                        Op::LShr(u, c_16),
                        Type::U32, insts, next_value_id);
                    let low_f = push(
                        Op::ConvertUToF(low_u),
                        Type::F32, insts, next_value_id);
                    let high_f = push(
                        Op::ConvertUToF(high_u),
                        Type::F32, insts, next_value_id);
                    let lane_x = push(
                        Op::FDiv(low_f, c_65535_f.clone()),
                        Type::F32, insts, next_value_id);
                    let lane_y = push(
                        Op::FDiv(high_f, c_65535_f),
                        Type::F32, insts, next_value_id);
                    Op::ConstVec(vec![lane_x, lane_y])
                }
                53 => {
                    // Ldexp(x, n) = x * 2^n  (f32, scalar).
                    // Synthesise 2^n via the IEEE-754 exponent
                    // bias trick: a normal f32 with sign=0,
                    // exponent=127+n, mantissa=0 has the value
                    // 2^n.  No subnormal / overflow handling
                    // -- callers staying in the [-126, 127]
                    // exponent range get exact results.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let n_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let n = resolve_value(n_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c_127 = push_ci(127, atrium_spv_ir::IntKind::I32,
                        source_spirv_offset, insts, next_value_id);
                    let c_23 = push_ci(23, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let biased = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::I32 };
                        insts.push(Inst {
                            op: Op::IAdd(c_127, n),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let bits = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::I32 };
                        insts.push(Inst {
                            op: Op::Shl(biased, c_23),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let two_n = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::F32 };
                        insts.push(Inst {
                            op: Op::Bitcast(bits, Type::F32),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::FMul(x, two_n)
                }
                70 => {
                    // FaceForward(N, I, Nref):
                    //   return N if dot(Nref, I) < 0, else -N.
                    // Vectors N / I / Nref share the result's
                    // (vector) type.  Synthesised as:
                    //   d        = Dot(Nref, I)          (scalar)
                    //   cond     = d < 0.0               (bool)
                    //   neg_N    = N * -1.0              (vec × scalar)
                    //   result   = cond ? N : neg_N      (vec Select)
                    use atrium_spv_ir::VecElement;
                    let n_id = expect_id(&spv_inst.operands, 2)?;
                    let i_id = expect_id(&spv_inst.operands, 3)?;
                    let r_id = expect_id(&spv_inst.operands, 4)?;
                    let n_v = resolve_value(n_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let i_v = resolve_value(i_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let r_v = resolve_value(r_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let scalar_ty = match &result_ty {
                        Type::Vec2(VecElement::F32)
                        | Type::Vec3(VecElement::F32)
                        | Type::Vec4(VecElement::F32) => Type::F32,
                        other => return Err(FrontendError::Unsupported(format!(
                            "FaceForward with non-f32-vec result type {other:?}"))),
                    };
                    let d = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: scalar_ty.clone() };
                        insts.push(Inst {
                            op: Op::Dot(r_v, i_v),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let zero = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    let cond = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::Bool };
                        insts.push(Inst {
                            op: Op::FOrdLt(d, zero),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let neg_one = push_cf(-1.0,
                        source_spirv_offset, insts, next_value_id);
                    let neg_n = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FMul(n_v.clone(), neg_one),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::Select {
                        cond,
                        t_val: n_v,
                        f_val: neg_n,
                    }
                }
                72 => {
                    // Refract(I, N, eta):
                    //   d  = dot(N, I)
                    //   k  = 1 - eta² * (1 - d²)
                    //   k<0 -> return vec(0)
                    //   k>=0 -> return eta*I - (eta*d + sqrt(k))*N
                    // I, N share the vec result type; eta is a
                    // scalar f32.
                    use atrium_spv_ir::VecElement;
                    let i_id   = expect_id(&spv_inst.operands, 2)?;
                    let n_id   = expect_id(&spv_inst.operands, 3)?;
                    let eta_id = expect_id(&spv_inst.operands, 4)?;
                    let i_v = resolve_value(i_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let n_v = resolve_value(n_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let eta = resolve_value(eta_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let lane_count = match &result_ty {
                        Type::Vec2(VecElement::F32) => 2,
                        Type::Vec3(VecElement::F32) => 3,
                        Type::Vec4(VecElement::F32) => 4,
                        other => return Err(FrontendError::Unsupported(format!(
                            "Refract with non-f32-vec result type {other:?}"))),
                    };
                    let scalar_ty = Type::F32;
                    let push_scalar = |op: Op,
                                        insts: &mut Vec<Inst>,
                                        next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: scalar_ty.clone() };
                        insts.push(Inst {
                            op, result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let push_vec = |op: Op,
                                     insts: &mut Vec<Inst>,
                                     next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op, result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let d = push_scalar(
                        Op::Dot(n_v.clone(), i_v.clone()),
                        insts, next_value_id);
                    let one = push_cf(1.0,
                        source_spirv_offset, insts, next_value_id);
                    let d_sq = push_scalar(
                        Op::FMul(d.clone(), d.clone()), insts, next_value_id);
                    let one_minus_d_sq = push_scalar(
                        Op::FSub(one.clone(), d_sq), insts, next_value_id);
                    let eta_sq = push_scalar(
                        Op::FMul(eta.clone(), eta.clone()),
                        insts, next_value_id);
                    let prod = push_scalar(
                        Op::FMul(eta_sq, one_minus_d_sq),
                        insts, next_value_id);
                    let k = push_scalar(
                        Op::FSub(one, prod), insts, next_value_id);
                    let zero_s = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    let k_negative = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::Bool };
                        insts.push(Inst {
                            op: Op::FOrdLt(k.clone(), zero_s.clone()),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let sqrt_k = push_scalar(
                        Op::FSqrt(k), insts, next_value_id);
                    let eta_d = push_scalar(
                        Op::FMul(eta.clone(), d), insts, next_value_id);
                    let sum = push_scalar(
                        Op::FAdd(eta_d, sqrt_k), insts, next_value_id);
                    let scaled_n = push_vec(
                        Op::FMul(n_v, sum), insts, next_value_id);
                    let eta_i = push_vec(
                        Op::FMul(i_v, eta), insts, next_value_id);
                    let refr = push_vec(
                        Op::FSub(eta_i, scaled_n), insts, next_value_id);
                    // zero_vec = ConstVec[zero_s × lane_count]
                    let zero_vec = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        let lanes = vec![zero_s; lane_count];
                        insts.push(Inst {
                            op: Op::ConstVec(lanes),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::Select {
                        cond: k_negative,
                        t_val: zero_vec,
                        f_val: refr,
                    }
                }
                1 => {
                    // Round(x): round-half-away-from-zero.
                    //   absx       = FAbs(x)
                    //   sum        = absx + 0.5
                    //   floored    = Floor(sum)
                    //   is_neg     = x < 0.0
                    //   neg_floor  = -floored
                    //   result     = is_neg ? neg_floor : floored
                    // Matches glslang's GL_OES_standard_derivatives
                    // round-half-away-from-zero semantics (Round(0.5)
                    // = 1, Round(-0.5) = -1).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let absx = push_f32(Op::FAbs(x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let half = push_cf(0.5,
                        source_spirv_offset, insts, next_value_id);
                    let sum = push_f32(Op::FAdd(absx, half),
                        source_spirv_offset, insts, next_value_id);
                    let floored = push_f32(Op::FFloor(sum),
                        source_spirv_offset, insts, next_value_id);
                    let zero = push_cf(0.0,
                        source_spirv_offset, insts, next_value_id);
                    // FSub(0, floored) -- avoids FNeg (which
                    // the bespoke backend doesn't lower today).
                    let neg_floor = push_f32(
                        Op::FSub(zero.clone(), floored.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let is_neg = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::Bool };
                        insts.push(Inst {
                            op: Op::FOrdLt(x, zero),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::Select {
                        cond: is_neg,
                        t_val: neg_floor,
                        f_val: floored,
                    }
                }
                7 => {
                    // SSign(x): -1 for x<0, 0 for x==0, +1 for x>0.
                    // Branchless bit-twiddle (no Select / no
                    // bool conversions):
                    //   neg_mask = x >> 31     (arithmetic
                    //                           shift; -1 if
                    //                           x<0, else 0)
                    //   pos_mask = (0 - x) >> 31   (logical
                    //                               shift; 1 if
                    //                               x>0, else 0)
                    //   result   = neg_mask | pos_mask
                    //              -> -1, 0, or 1
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c_zero = push_ci(0, atrium_spv_ir::IntKind::I32,
                        source_spirv_offset, insts, next_value_id);
                    let c31 = push_ci(31, atrium_spv_ir::IntKind::U32,
                        source_spirv_offset, insts, next_value_id);
                    let neg_x = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::I32 };
                        insts.push(Inst {
                            op: Op::ISub(c_zero, x.clone()),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let neg_mask = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::I32 };
                        insts.push(Inst {
                            op: Op::AShr(x, c31.clone()),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let pos_mask = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::I32 };
                        insts.push(Inst {
                            op: Op::LShr(neg_x, c31),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::BitOr(neg_mask, pos_mask)
                }
                50 => {
                    // Fma(a, b, c) = a*b + c.  No fused
                    // multiply-add IR op (the bespoke backend
                    // can fold this into ARM's FMADD when it
                    // wants); express as two FMul/FAdd ops.
                    let a_id = expect_id(&spv_inst.operands, 2)?;
                    let b_id = expect_id(&spv_inst.operands, 3)?;
                    let c_id = expect_id(&spv_inst.operands, 4)?;
                    let a = resolve_value(a_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let b = resolve_value(b_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c = resolve_value(c_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let prod = push_f32(Op::FMul(a, b),
                        source_spirv_offset, insts, next_value_id);
                    Op::FAdd(prod, c)
                }
                11 => {
                    // Radians(deg) = deg * (π/180).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c = push_cf(std::f64::consts::PI / 180.0,
                        source_spirv_offset, insts, next_value_id);
                    Op::FMul(x, c)
                }
                12 => {
                    // Degrees(rad) = rad * (180/π).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c = push_cf(180.0 / std::f64::consts::PI,
                        source_spirv_offset, insts, next_value_id);
                    Op::FMul(x, c)
                }
                58 => {
                    // PackHalf2x16(vec2) -> u32.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::PackHalf2x16(x)
                }
                62 => {
                    // UnpackHalf2x16(u32) -> vec2.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::UnpackHalf2x16(x)
                }
                25 => {
                    // Atan2(y, x): four-quadrant arctangent.
                    //   base = atan(y / x)   (atan handles ±Inf
                    //                         when x=0 via its
                    //                         reciprocal branch)
                    //   bias = x<0 ? (y<0 ? -π : π) : 0
                    //   result = base + bias
                    let y_id = expect_id(&spv_inst.operands, 2)?;
                    let x_id = expect_id(&spv_inst.operands, 3)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let ratio = push_f32(Op::FDiv(y.clone(), x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let base = synth_atan(ratio,
                        source_spirv_offset, insts, next_value_id);
                    let zero = push_cf(0.0, source_spirv_offset, insts, next_value_id);
                    let is_x_neg = push_bool(Op::FOrdLt(x, zero.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let is_y_neg = push_bool(Op::FOrdLt(y, zero.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let c_pi = push_cf( std::f64::consts::PI,
                        source_spirv_offset, insts, next_value_id);
                    let c_neg_pi = push_cf(-std::f64::consts::PI,
                        source_spirv_offset, insts, next_value_id);
                    let bias_neg = push_f32(Op::Select {
                        cond: is_y_neg, t_val: c_neg_pi, f_val: c_pi,
                    }, source_spirv_offset, insts, next_value_id);
                    let bias = push_f32(Op::Select {
                        cond: is_x_neg, t_val: bias_neg, f_val: zero,
                    }, source_spirv_offset, insts, next_value_id);
                    Op::FAdd(base, bias)
                }
                16 => {
                    // Asin(x) = Atan(x / sqrt(1 - x²)).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let a = synth_asin(x, source_spirv_offset, insts, next_value_id);
                    let c_one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    Op::FMul(a, c_one)
                }
                17 => {
                    // Acos(x) = π/2 - Asin(x).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let a = synth_asin(x, source_spirv_offset, insts, next_value_id);
                    let c_half_pi = push_cf(std::f64::consts::FRAC_PI_2,
                        source_spirv_offset, insts, next_value_id);
                    Op::FSub(c_half_pi, a)
                }
                26 => {
                    // Pow(x, y) = Exp2(y * Log2(x)).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let lx = synth_log2(x, source_spirv_offset, insts, next_value_id);
                    let ylx = push_f32(Op::FMul(y, lx),
                        source_spirv_offset, insts, next_value_id);
                    let e = synth_exp2(ylx, source_spirv_offset, insts, next_value_id);
                    let c_one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
                    Op::FMul(e, c_one)
                }
                32 => {
                    // InverseSqrt(x) ≡ 1.0 / sqrt(x).  Real
                    // ARM64 has FRSQRTE (estimate) + FRSQRTS
                    // (refinement), but for now lower via
                    // existing primitives -- correct to
                    // f32 ULPs at the cost of two extra
                    // instructions (movz/movk/fmov for 1.0
                    // + fdiv).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty };
                        insts.push(Inst { op, result: Some(v.clone()),
                            source_spirv_offset });
                        v
                    };
                    let sq = push(
                        Op::FSqrt(x), result_ty.clone(),
                        insts, next_value_id);
                    let one = push(
                        Op::ConstFloat { value: 1.0, kind: atrium_spv_ir::FloatKind::F32 },
                        result_ty.clone(), insts, next_value_id);
                    Op::FDiv(one, sq)
                }
                37 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FMin(x, y)
                }
                79 => {
                    // NMin(x, y) = FMin (NaN-suppression
                    // semantics deferred; documented in
                    // tier2-renderer.md).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FMin(x, y)
                }
                80 => {
                    // NMax(x, y) = FMax.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FMax(x, y)
                }
                81 => {
                    // NClamp(x, lo, hi) = FMin(FMax(x, lo), hi).
                    let x_id  = expect_id(&spv_inst.operands, 2)?;
                    let lo_id = expect_id(&spv_inst.operands, 3)?;
                    let hi_id = expect_id(&spv_inst.operands, 4)?;
                    let x  = resolve_value(x_id,  types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let lo = resolve_value(lo_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let hi = resolve_value(hi_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let mid = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FMax(x, lo),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::FMin(mid, hi)
                }
                75 => {
                    // FindUMsb(x): bit index of highest set
                    // bit, or -1 if x==0.
                    //   clz_v = Clz(x)          // 0..32
                    //   msb   = 31 - clz_v
                    // When x=0: clz=32, msb=-1.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let clz_v = push_i32(Op::Clz(x),
                        source_spirv_offset, insts, next_value_id);
                    let c31 = push_ci32(31, source_spirv_offset, insts, next_value_id);
                    Op::ISub(c31, clz_v)
                }
                73 => {
                    // FindILsb(x): bit index of lowest set bit,
                    // or -1 if x==0.
                    //   r     = Rbit(x)
                    //   clz_v = Clz(r)          // 0..32
                    //   lsb   = (x==0) ? -1 : clz_v
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let r = push_i32(Op::Rbit(x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let clz_v = push_i32(Op::Clz(r),
                        source_spirv_offset, insts, next_value_id);
                    let c0 = push_ci32(0, source_spirv_offset, insts, next_value_id);
                    let is_zero = push_bool(Op::IEq(x, c0),
                        source_spirv_offset, insts, next_value_id);
                    let c_neg1 = push_ci32(-1, source_spirv_offset, insts, next_value_id);
                    Op::Select {
                        cond: is_zero, t_val: c_neg1, f_val: clz_v,
                    }
                }
                74 => {
                    // FindSMsb(x): bit index of highest 0 bit if
                    // x<0, else of highest 1 bit.  -1 if x==0 or
                    // x==-1.
                    //   y  = (x<0) ? ~x : x
                    //   r  = FindUMsb(y)
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let c0 = push_ci32(0, source_spirv_offset, insts, next_value_id);
                    let is_neg = push_bool(Op::SLt(x.clone(), c0),
                        source_spirv_offset, insts, next_value_id);
                    let not_x = push_i32(Op::BitNot(x.clone()),
                        source_spirv_offset, insts, next_value_id);
                    let y = push_i32(Op::Select {
                        cond: is_neg, t_val: not_x, f_val: x,
                    }, source_spirv_offset, insts, next_value_id);
                    let clz_v = push_i32(Op::Clz(y),
                        source_spirv_offset, insts, next_value_id);
                    let c31 = push_ci32(31, source_spirv_offset, insts, next_value_id);
                    Op::ISub(c31, clz_v)
                }
                // UMin(38) / SMin(39) / UMax(41) / SMax(42):
                // synth via Select on a signed/unsigned compare.
                // (Arc 45: previously had 38/39 swapped against
                // the GLSL.std.450 spec; matching test cases
                // also corrected.)
                38 | 39 | 41 | 42 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let cmp_op = match inst_enum {
                        38 => Op::ULt(x.clone(), y.clone()), // UMin
                        39 => Op::SLt(x.clone(), y.clone()), // SMin
                        41 => Op::UGt(x.clone(), y.clone()), // UMax
                        42 => Op::SGt(x.clone(), y.clone()), // SMax
                        _ => unreachable!(),
                    };
                    let cond = push_bool(cmp_op,
                        source_spirv_offset, insts, next_value_id);
                    Op::Select { cond, t_val: x, f_val: y }
                }
                // SClamp(45) / UClamp(44): nested min(max(...)).
                44 | 45 => {
                    let x_id  = expect_id(&spv_inst.operands, 2)?;
                    let lo_id = expect_id(&spv_inst.operands, 3)?;
                    let hi_id = expect_id(&spv_inst.operands, 4)?;
                    let x  = resolve_value(x_id,  types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let lo = resolve_value(lo_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let hi = resolve_value(hi_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let signed = inst_enum == 45;
                    // mid = max(x, lo)
                    let cmp_max = if signed {
                        Op::SGt(x.clone(), lo.clone())
                    } else {
                        Op::UGt(x.clone(), lo.clone())
                    };
                    let cond_max = push_bool(cmp_max,
                        source_spirv_offset, insts, next_value_id);
                    let mid = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::Select { cond: cond_max, t_val: x, f_val: lo },
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    // result = min(mid, hi)
                    let cmp_min = if signed {
                        Op::SLt(mid.clone(), hi.clone())
                    } else {
                        Op::ULt(mid.clone(), hi.clone())
                    };
                    let cond_min = push_bool(cmp_min,
                        source_spirv_offset, insts, next_value_id);
                    Op::Select { cond: cond_min, t_val: mid, f_val: hi }
                }
                40 => {
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    Op::FMax(x, y)
                }
                43 => {
                    // FClamp(x, lo, hi) ≡ FMin(FMax(x, lo), hi).
                    // Synthesise the intermediate FMax inline so
                    // the IR carries only the two leaf primitive
                    // ops -- backends don't need a clamp variant.
                    let x_id  = expect_id(&spv_inst.operands, 2)?;
                    let lo_id = expect_id(&spv_inst.operands, 3)?;
                    let hi_id = expect_id(&spv_inst.operands, 4)?;
                    let x  = resolve_value(x_id,  types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let lo = resolve_value(lo_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let hi = resolve_value(hi_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let mid = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FMax(x, lo),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::FMin(mid, hi)
                }
                6 => {
                    // FSign(x) ≡ (x > 0) - (x < 0).  Cleanly
                    // handles +/-0 (both yield 0) and normal
                    // signed values.  Synthesised as:
                    //   gt    = FOrdGt(x, 0.0)       // i32 0/1
                    //   lt    = FOrdLt(x, 0.0)       // i32 0/1
                    //   gt_f  = ConvertUToF(gt)      // 0.0 or 1.0
                    //   lt_f  = ConvertUToF(lt)      // 0.0 or 1.0
                    //   sign  = FSub(gt_f, lt_f)     // -1.0, 0.0, +1.0
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push_inst = |op: Op, ty: Type,
                                     insts: &mut Vec<Inst>,
                                     next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty };
                        insts.push(Inst {
                            op, result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let zero = push_inst(
                        Op::ConstFloat { value: 0.0, kind: atrium_spv_ir::FloatKind::F32 },
                        Type::F32, insts, next_value_id);
                    let gt = push_inst(
                        Op::FOrdGt(x.clone(), zero.clone()),
                        Type::U32, insts, next_value_id);
                    let lt = push_inst(
                        Op::FOrdLt(x, zero),
                        Type::U32, insts, next_value_id);
                    let gt_f = push_inst(
                        Op::ConvertUToF(gt), Type::F32, insts, next_value_id);
                    let lt_f = push_inst(
                        Op::ConvertUToF(lt), Type::F32, insts, next_value_id);
                    Op::FSub(gt_f, lt_f)
                }
                71 => {
                    // Reflect(I, N) ≡ I - 2 * dot(N, I) * N
                    // I and N are both vectors of the result type.
                    let i_id = expect_id(&spv_inst.operands, 2)?;
                    let n_id = expect_id(&spv_inst.operands, 3)?;
                    let i_v = resolve_value(i_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let n_v = resolve_value(n_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    // dot(N, I) is a scalar matching result's
                    // element type.
                    use atrium_spv_ir::VecElement;
                    let scalar_ty = match &result_ty {
                        atrium_spv_ir::Type::Vec2(VecElement::F32)
                        | atrium_spv_ir::Type::Vec3(VecElement::F32)
                        | atrium_spv_ir::Type::Vec4(VecElement::F32) => Type::F32,
                        other => return Err(FrontendError::Unsupported(format!(
                            "Reflect with non-f32-vec result type {other:?}"))),
                    };
                    let d = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: scalar_ty.clone() };
                        insts.push(Inst {
                            op: Op::Dot(n_v.clone(), i_v.clone()),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    // 2.0 constant
                    let two = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: scalar_ty.clone() };
                        insts.push(Inst {
                            op: Op::ConstFloat { value: 2.0, kind: atrium_spv_ir::FloatKind::F32 },
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let two_d = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: scalar_ty };
                        insts.push(Inst {
                            op: Op::FMul(two, d),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    // two_d * N (scalar * vec broadcast).
                    let scaled_n = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FMul(n_v, two_d),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    Op::FSub(i_v, scaled_n)
                }
                69 => {
                    // Normalize(v) ≡ v / length(v).  Result has
                    // the same type as v (a vec).  This needs a
                    // vec/scalar FDiv, which the backend's
                    // existing emit_fp_binop_poly handles via
                    // the (true, false) "vec × scalar" arm.
                    let v_id = expect_id(&spv_inst.operands, 2)?;
                    let v = resolve_value(v_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    // Length: float scalar, even though v is a vec.
                    use atrium_spv_ir::VecElement;
                    let scalar_ty = match &v.ty {
                        atrium_spv_ir::Type::Vec2(VecElement::F32)
                        | atrium_spv_ir::Type::Vec3(VecElement::F32)
                        | atrium_spv_ir::Type::Vec4(VecElement::F32) => Type::F32,
                        other => return Err(FrontendError::Unsupported(format!(
                            "Normalize on non-f32-vector type {other:?}",
                        ))),
                    };
                    let dot_vv = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: scalar_ty.clone() };
                        insts.push(Inst {
                            op: Op::Dot(v.clone(), v.clone()),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let length_v = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: scalar_ty };
                        insts.push(Inst {
                            op: Op::FSqrt(dot_vv),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    Op::FDiv(v, length_v)
                }
                68 => {
                    // Cross(a, b):
                    //   r.x = a.y*b.z - a.z*b.y
                    //   r.y = a.z*b.x - a.x*b.z
                    //   r.z = a.x*b.y - a.y*b.x
                    // a, b are vec3 (or vec4 with .xyz used).
                    // Result is vec3 matching result_ty.
                    let a_id = expect_id(&spv_inst.operands, 2)?;
                    let b_id = expect_id(&spv_inst.operands, 3)?;
                    let a_v = resolve_value(a_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let b_v = resolve_value(b_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty };
                        insts.push(Inst { op, result: Some(v.clone()),
                            source_spirv_offset });
                        v
                    };
                    let extract = |idx: u32, src: &Value,
                                   insts: &mut Vec<Inst>,
                                   next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: Type::F32 };
                        insts.push(Inst {
                            op: Op::VectorExtract { vector: src.clone(), index: idx },
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let ax = extract(0, &a_v, insts, next_value_id);
                    let ay = extract(1, &a_v, insts, next_value_id);
                    let az = extract(2, &a_v, insts, next_value_id);
                    let bx = extract(0, &b_v, insts, next_value_id);
                    let by = extract(1, &b_v, insts, next_value_id);
                    let bz = extract(2, &b_v, insts, next_value_id);
                    let aybz = push(Op::FMul(ay.clone(), bz.clone()),
                        Type::F32, insts, next_value_id);
                    let azby = push(Op::FMul(az.clone(), by.clone()),
                        Type::F32, insts, next_value_id);
                    let rx = push(Op::FSub(aybz, azby),
                        Type::F32, insts, next_value_id);
                    let azbx = push(Op::FMul(az, bx.clone()),
                        Type::F32, insts, next_value_id);
                    let axbz = push(Op::FMul(ax.clone(), bz),
                        Type::F32, insts, next_value_id);
                    let ry = push(Op::FSub(azbx, axbz),
                        Type::F32, insts, next_value_id);
                    let axby = push(Op::FMul(ax, by),
                        Type::F32, insts, next_value_id);
                    let aybx = push(Op::FMul(ay, bx),
                        Type::F32, insts, next_value_id);
                    let rz = push(Op::FSub(axby, aybx),
                        Type::F32, insts, next_value_id);
                    Op::ConstVec(vec![rx, ry, rz])
                }
                66 => {
                    // Length(v) ≡ sqrt(dot(v, v)).  Result is
                    // f32; arg is vec.
                    let v_id = expect_id(&spv_inst.operands, 2)?;
                    let v = resolve_value(v_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let dot_vv = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::Dot(v.clone(), v),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    Op::FSqrt(dot_vv)
                }
                67 => {
                    // Distance(p0, p1) ≡ length(p1 - p0)
                    //                  ≡ sqrt(dot(d, d)) where d = p1-p0.
                    let p0_id = expect_id(&spv_inst.operands, 2)?;
                    let p1_id = expect_id(&spv_inst.operands, 3)?;
                    let p0 = resolve_value(p0_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let p1 = resolve_value(p1_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let diff = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        // Diff has the SAME type as p0/p1 (a vec).
                        let val = Value { id, ty: p0.ty.clone() };
                        insts.push(Inst {
                            op: Op::FSub(p1, p0),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    let dot_dd = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::Dot(diff.clone(), diff),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    Op::FSqrt(dot_dd)
                }
                35 => {
                    // GLSL.std.450 #35 is Modf in the spec, but
                    // Atrium accepts a two-operand FMod-shaped call
                    // (x, y) -> x - y * floor(x / y) for shaders /
                    // tests that emit `mod(x, y)` via ExtInst
                    // instead of the core OpFMod opcode (opcode
                    // 141).  Genuine Modf with a pointer out-param
                    // would resolve_value the pointer and surface
                    // as an Unsupported via the operand-shape
                    // check; the synthesised path below only fires
                    // for two value operands.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty };
                        insts.push(Inst { op, result: Some(v.clone()),
                            source_spirv_offset });
                        v
                    };
                    let q = push(
                        Op::FDiv(x.clone(), y.clone()),
                        result_ty.clone(), insts, next_value_id);
                    let fl = push(
                        Op::FFloor(q), result_ty.clone(),
                        insts, next_value_id);
                    let yfl = push(
                        Op::FMul(y, fl), result_ty.clone(),
                        insts, next_value_id);
                    Op::FSub(x, yfl)
                }
                10 => {
                    // Fract(x) ≡ x - floor(x).  Synthesise inline
                    // using the existing Op::FFloor + Op::FSub.
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let floor_x = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FFloor(x.clone()),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::FSub(x, floor_x)
                }
                49 => {
                    // SmoothStep(e0, e1, x):
                    //   t = clamp((x - e0) / (e1 - e0), 0, 1)
                    //   return t * t * (3 - 2*t)
                    // Hermite interpolation -- branchless
                    // ramp.  Heavily used for ramps/masks
                    // in fragment shaders.
                    let e0_id = expect_id(&spv_inst.operands, 2)?;
                    let e1_id = expect_id(&spv_inst.operands, 3)?;
                    let x_id  = expect_id(&spv_inst.operands, 4)?;
                    let e0 = resolve_value(e0_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let e1 = resolve_value(e1_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let x  = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let push = |op: Op, ty: Type,
                                insts: &mut Vec<Inst>,
                                next_value_id: &mut u32| -> Value {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty };
                        insts.push(Inst { op, result: Some(v.clone()),
                            source_spirv_offset });
                        v
                    };
                    let scalar_ty = result_ty.clone();
                    let x_minus_e0 = push(
                        Op::FSub(x, e0.clone()), scalar_ty.clone(),
                        insts, next_value_id);
                    let e1_minus_e0 = push(
                        Op::FSub(e1, e0), scalar_ty.clone(),
                        insts, next_value_id);
                    let t_raw = push(
                        Op::FDiv(x_minus_e0, e1_minus_e0), scalar_ty.clone(),
                        insts, next_value_id);
                    // clamp(t_raw, 0, 1)
                    let zero = push(
                        Op::ConstFloat { value: 0.0,
                            kind: atrium_spv_ir::FloatKind::F32 },
                        scalar_ty.clone(), insts, next_value_id);
                    let one = push(
                        Op::ConstFloat { value: 1.0,
                            kind: atrium_spv_ir::FloatKind::F32 },
                        scalar_ty.clone(), insts, next_value_id);
                    let t_lo = push(
                        Op::FMax(t_raw, zero), scalar_ty.clone(),
                        insts, next_value_id);
                    let t = push(
                        Op::FMin(t_lo, one.clone()), scalar_ty.clone(),
                        insts, next_value_id);
                    // 3 - 2*t
                    let two = push(
                        Op::ConstFloat { value: 2.0,
                            kind: atrium_spv_ir::FloatKind::F32 },
                        scalar_ty.clone(), insts, next_value_id);
                    let three = push(
                        Op::ConstFloat { value: 3.0,
                            kind: atrium_spv_ir::FloatKind::F32 },
                        scalar_ty.clone(), insts, next_value_id);
                    let two_t = push(
                        Op::FMul(two, t.clone()), scalar_ty.clone(),
                        insts, next_value_id);
                    let three_minus_2t = push(
                        Op::FSub(three, two_t), scalar_ty.clone(),
                        insts, next_value_id);
                    let t_squared = push(
                        Op::FMul(t.clone(), t), scalar_ty,
                        insts, next_value_id);
                    let _ = one;
                    Op::FMul(t_squared, three_minus_2t)
                }
                48 => {
                    // Step(edge, x) ≡ x < edge ? 0.0 : 1.0
                    //              ≡ float(x >= edge).
                    let edge_id = expect_id(&spv_inst.operands, 2)?;
                    let x_id    = expect_id(&spv_inst.operands, 3)?;
                    let edge = resolve_value(edge_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let x = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let ge = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let val = Value { id, ty: Type::U32 };
                        insts.push(Inst {
                            op: Op::FOrdGe(x, edge),
                            result: Some(val.clone()),
                            source_spirv_offset,
                        });
                        val
                    };
                    Op::ConvertUToF(ge)
                }
                46 => {
                    // FMix(x, y, a) ≡ x + a*(y - x).
                    let x_id = expect_id(&spv_inst.operands, 2)?;
                    let y_id = expect_id(&spv_inst.operands, 3)?;
                    let a_id = expect_id(&spv_inst.operands, 4)?;
                    let x  = resolve_value(x_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let y  = resolve_value(y_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let a  = resolve_value(a_id, types, constants, id_map,
                        next_value_id, insts, source_spirv_offset)?;
                    let diff = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FSub(y, x.clone()),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    let scaled = {
                        let id = ValueId(*next_value_id);
                        *next_value_id += 1;
                        let v = Value { id, ty: result_ty.clone() };
                        insts.push(Inst {
                            op: Op::FMul(a, diff),
                            result: Some(v.clone()),
                            source_spirv_offset,
                        });
                        v
                    };
                    Op::FAdd(x, scaled)
                }
                other => return Err(FrontendError::Unsupported(format!(
                    "GLSL.std.450 instruction enum {other} not yet supported",
                ))),
            };
            let result = alloc_or_get_result(
                result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op,
                result: Some(result),
                source_spirv_offset,
            });
            Ok(())
        }

        other => Err(FrontendError::Unsupported(format!(
            "opcode {other:?} not supported in phase 1 v3",
        ))),
    }
}

/// Helper: translate a SPIR-V binary float-arithmetic
/// instruction (FAdd / FSub / FMul / FDiv etc.) into the
/// equivalent atrium-spv-ir Op via a constructor closure.
#[allow(clippy::too_many_arguments)]
/// Helper: translate a SPIR-V binary integer instruction
/// (IAdd / ISub / IMul / SDiv / IEqual / SLessThan etc.)
/// into the equivalent atrium-spv-ir Op. Same shape as
/// [`emit_binop_float`] but stays in the integer family.
#[allow(clippy::too_many_arguments)]
fn emit_binop_int(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    source_spirv_offset: u32,
    make_op: impl FnOnce(Value, Value) -> Op,
) -> Result<(), FrontendError> {
    let _ = iface;
    let result_id = spv_inst.result_id.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result id", spv_inst.class.opcode)))?;
    let result_type_id = spv_inst.result_type.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result type", spv_inst.class.opcode)))?;
    let ty = types.get(result_type_id)?.clone();
    let lhs_id = expect_id(&spv_inst.operands, 0)?;
    let rhs_id = expect_id(&spv_inst.operands, 1)?;
    let lhs = resolve_value(
        lhs_id, types, constants, id_map, next_value_id, insts,
        source_spirv_offset,
    )?;
    let rhs = resolve_value(
        rhs_id, types, constants, id_map, next_value_id, insts,
        source_spirv_offset,
    )?;
    let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
    insts.push(Inst {
        op: make_op(lhs, rhs),
        result: Some(result),
        source_spirv_offset,
    });
    Ok(())
}

/// Helper: translate a SPIR-V unary integer instruction
/// (SNegate) into the equivalent atrium-spv-ir Op.
#[allow(clippy::too_many_arguments)]
fn emit_unop_int(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    source_spirv_offset: u32,
    make_op: impl FnOnce(Value) -> Op,
) -> Result<(), FrontendError> {
    let _ = iface;
    let result_id = spv_inst.result_id.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result id", spv_inst.class.opcode)))?;
    let result_type_id = spv_inst.result_type.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result type", spv_inst.class.opcode)))?;
    let ty = types.get(result_type_id)?.clone();
    let src_id = expect_id(&spv_inst.operands, 0)?;
    let src = resolve_value(
        src_id, types, constants, id_map, next_value_id, insts,
        source_spirv_offset,
    )?;
    let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
    insts.push(Inst {
        op: make_op(src),
        result: Some(result),
        source_spirv_offset,
    });
    Ok(())
}

fn emit_binop_float(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    source_spirv_offset: u32,
    make_op: impl FnOnce(Value, Value) -> Op,
) -> Result<(), FrontendError> {
    let _ = iface;
    let result_id = spv_inst.result_id.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result id", spv_inst.class.opcode)))?;
    let result_type_id = spv_inst.result_type.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result type", spv_inst.class.opcode)))?;
    let ty = types.get(result_type_id)?.clone();
    let lhs_id = expect_id(&spv_inst.operands, 0)?;
    let rhs_id = expect_id(&spv_inst.operands, 1)?;
    let lhs = resolve_value(
        lhs_id, types, constants, id_map, next_value_id, insts,
        source_spirv_offset,
    )?;
    let rhs = resolve_value(
        rhs_id, types, constants, id_map, next_value_id, insts,
        source_spirv_offset,
    )?;
    let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
    insts.push(Inst {
        op: make_op(lhs, rhs),
        result: Some(result),
        source_spirv_offset,
    });
    Ok(())
}

/// Helper: translate a SPIR-V unary float-arithmetic
/// instruction (FNegate) into the equivalent
/// atrium-spv-ir Op.
#[allow(clippy::too_many_arguments)]
fn emit_unop_float(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    source_spirv_offset: u32,
    make_op: impl FnOnce(Value) -> Op,
) -> Result<(), FrontendError> {
    let _ = iface;
    let result_id = spv_inst.result_id.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result id", spv_inst.class.opcode)))?;
    let result_type_id = spv_inst.result_type.ok_or_else(|| FrontendError::Malformed(
        format!("{:?} without result type", spv_inst.class.opcode)))?;
    let ty = types.get(result_type_id)?.clone();
    let src_id = expect_id(&spv_inst.operands, 0)?;
    let src = resolve_value(
        src_id, types, constants, id_map, next_value_id, insts,
        source_spirv_offset,
    )?;
    let result = alloc_or_get_result(result_id, ty, id_map, next_value_id);
    insts.push(Inst {
        op: make_op(src),
        result: Some(result),
        source_spirv_offset,
    });
    Ok(())
}

/// Resolve a SPIR-V id → IR Value, materialising a
/// constant by pushing the defining Inst(s) onto `insts`.
///
/// Idempotent: subsequent lookups of the same id return
/// the cached Value from `id_map` without re-emitting.
///
/// `source_spirv_offset` is the offset of the USING
/// instruction; synthesised constant-defining Insts
/// inherit it so crash triage attributes a faulting
/// instruction back to the spot in the source SPIR-V
/// that first needed the constant.
fn resolve_value(
    id: Word,
    types: &TypeContext,
    constants: &ConstantContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    source_spirv_offset: u32,
) -> Result<Value, FrontendError> {
    if let Some(v) = id_map.get(&id) { return Ok(v.clone()); }

    // Is it a constant? Materialise it.
    if let Some(stored) = constants.get(id) {
        let ty = types.get(stored.type_id)?.clone();
        let stored = stored.clone(); // free of borrow on `constants`
        return materialize_constant(
            id, &stored.kind, ty, types, constants, id_map, next_value_id, insts,
            source_spirv_offset,
        );
    }

    Err(FrontendError::Malformed(format!(
        "SSA id {id} referenced but not defined (no constant, no variable)",
    )))
}

/// Emit the IR instructions defining a constant's value
/// and return its SSA Value. Recursive: composites
/// materialise their elements first.
fn materialize_constant(
    id: Word,
    kind: &ConstantKind,
    ty: Type,
    types: &TypeContext,
    constants: &ConstantContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
    source_spirv_offset: u32,
) -> Result<Value, FrontendError> {
    let result = fresh_value(ty.clone(), next_value_id);
    let op = match kind {
        ConstantKind::Scalar(op) => op.clone(),
        ConstantKind::Null => Op::ConstNull,
        ConstantKind::Composite(element_ids) => {
            // Materialise each element first (recursively).
            let mut elements = Vec::with_capacity(element_ids.len());
            for eid in element_ids {
                let v = resolve_value(
                    *eid, types, constants, id_map, next_value_id, insts,
                    source_spirv_offset,
                )?;
                elements.push(v);
            }
            Op::ConstVec(elements)
        }
    };
    insts.push(Inst {
        op,
        result: Some(result.clone()),
        source_spirv_offset,
    });
    id_map.insert(id, result.clone());
    Ok(result)
}

/// Materialise a variable id as a pointer-typed Value.
///
/// Called from `translate_inst` for OpStore/OpLoad
/// operands before delegating to `resolve_value`.
fn resolve_variable(
    id: Word,
    types: &TypeContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
) -> Result<Option<Value>, FrontendError> {
    if let Some(v) = id_map.get(&id) { return Ok(Some(v.clone())); }
    let Some((storage, pointee_id)) = iface.variables.get(&id) else {
        return Ok(None);
    };
    // Aggregate pointees (Struct / Array) have no IR Type;
    // fall back to Void. Such variables are only addressed
    // through OpAccessChain, which produces a fresh Pointer
    // with the resolved leaf type — the placeholder Void
    // never reaches the backend.
    let pointee = types.get(*pointee_id).cloned().unwrap_or(Type::Void);
    let storage = crate::types::translate_storage(*storage)?;
    let ty = Type::Pointer(storage, Box::new(pointee));
    let v = fresh_value(ty, next_value_id);
    id_map.insert(id, v.clone());
    Ok(Some(v))
}

/// Get the pre-allocated [`Value`] for `result_id` if one
/// exists (set by the pre-pass), else allocate a fresh one
/// and cache it. Used by every translator arm that emits
/// an Inst with a result; making this a single helper lets
/// us pre-allocate Values for forward-referenced ids (e.g.
/// loop-induction Phi back-edge values) so the per-block
/// walk doesn't have to encounter the defining inst before
/// any use.
fn alloc_or_get_result(
    result_id: Word,
    ty: Type,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
) -> Value {
    if let Some(v) = id_map.get(&result_id) { return v.clone(); }
    let v = fresh_value(ty, next_value_id);
    id_map.insert(result_id, v.clone());
    v
}

fn fresh_value(ty: Type, next_value_id: &mut u32) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    Value { id, ty }
}

/// Push a new instruction producing an f32 result.
fn push_f32(
    op: Op,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let v = Value { id, ty: Type::F32 };
    insts.push(Inst { op, result: Some(v.clone()), source_spirv_offset });
    v
}

/// Emit an f32 ConstFloat.
fn push_cf(
    val: f64,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    push_f32(
        Op::ConstFloat { value: val, kind: atrium_spv_ir::FloatKind::F32 },
        source_spirv_offset, insts, next_value_id,
    )
}

/// If the SPIR-V operand list at `start` begins with an
/// `Image-Operands` mask whose `Lod` bit is set, return the
/// LOD `IdRef` that follows it.  Returns `None` when no mask
/// is present or the Lod bit is clear.  Bails on unsupported
/// other mask bits (Bias / Grad / etc.) for storage-image
/// ops — those aren't legal on `OpImageRead` / `OpImageWrite`
/// per the SPIR-V spec.
fn extract_image_operand_lod(
    operands: &[Operand],
    start: usize,
) -> Result<Option<Word>, FrontendError> {
    use rspirv::spirv::ImageOperands as IO;
    let Some(first) = operands.get(start) else { return Ok(None); };
    let mask = match first {
        Operand::ImageOperands(m) => *m,
        // No mask -> no Lod.  Operand after coord/texel may
        // also be unrelated (some encodings skip the mask).
        _ => return Ok(None),
    };
    if !mask.contains(IO::LOD) {
        return Ok(None);
    }
    // Per the SPIR-V spec, image-operand parameters follow
    // the mask in lowest-bit-first order.  Lod has bit 1
    // (mask 0x2); anything below it is Bias (bit 0, mask
    // 0x1) which storage-image ops don't allow, so when
    // present we can require Lod is the first parameter.
    if mask.contains(IO::BIAS) {
        return Err(FrontendError::Unsupported(
            "Image-Operands::Bias on storage-image read/write \
             is illegal per SPIR-V spec".to_string()));
    }
    // Lod's arg sits at start+1.
    match operands.get(start + 1) {
        Some(Operand::IdRef(id)) => Ok(Some(*id)),
        other => Err(FrontendError::Malformed(format!(
            "Image-Operands::Lod expected IdRef after mask, got {other:?}"))),
    }
}

/// If the SPIR-V operand list at `start` begins with an
/// `Image-Operands` mask whose `Bias` bit is set, return the
/// Bias `IdRef` that follows.  Returns `None` when there is no
/// mask or the Bias bit is clear.  Bias is bit 0 of the mask
/// (`0x1`); its argument is the first parameter after the mask
/// since lower bits come first in the parameter list (SPIR-V
/// spec, §3.14).
fn extract_image_operand_bias(
    operands: &[Operand],
    start: usize,
) -> Result<Option<Word>, FrontendError> {
    use rspirv::spirv::ImageOperands as IO;
    let Some(first) = operands.get(start) else { return Ok(None); };
    let mask = match first {
        Operand::ImageOperands(m) => *m,
        _ => return Ok(None),
    };
    if !mask.contains(IO::BIAS) {
        return Ok(None);
    }
    match operands.get(start + 1) {
        Some(Operand::IdRef(id)) => Ok(Some(*id)),
        other => Err(FrontendError::Malformed(format!(
            "Image-Operands::Bias expected IdRef after mask, got {other:?}"))),
    }
}

/// If the SPIR-V operand list at `start` begins with an
/// `Image-Operands` mask whose `ConstOffset` or `Offset` bit
/// is set, return the offset `IdRef` that follows.  Returns
/// `None` when no mask is present or neither bit is clear.
/// Bails on combos we don't handle (Bias, Grad).
///
/// Bit order (lowest first) in the args list:
///   Bias(0x1), Lod(0x2), Grad(0x4 — *2* args), ConstOffset(0x8),
///   Offset(0x10), ConstOffsets(0x20), Sample(0x40), MinLod(0x80)
fn extract_image_operand_offset(
    operands: &[Operand],
    start: usize,
) -> Result<Option<Word>, FrontendError> {
    use rspirv::spirv::ImageOperands as IO;
    let Some(first) = operands.get(start) else { return Ok(None); };
    let mask = match first {
        Operand::ImageOperands(m) => *m,
        _ => return Ok(None),
    };
    if mask.contains(IO::GRAD) {
        return Err(FrontendError::Unsupported(
            "Image-Operands::Grad not supported on ImageFetch".to_string()));
    }
    if !mask.contains(IO::CONST_OFFSET) && !mask.contains(IO::OFFSET) {
        return Ok(None);
    }
    // Skip past lower-bit args.
    let mut idx = start + 1;
    if mask.contains(IO::BIAS) { idx += 1; }
    if mask.contains(IO::LOD)  { idx += 1; }
    // Grad would be +2 but we bailed above.
    match operands.get(idx) {
        Some(Operand::IdRef(id)) => Ok(Some(*id)),
        other => Err(FrontendError::Malformed(format!(
            "Image-Operands::ConstOffset expected IdRef at {idx}, got {other:?}"))),
    }
}

/// Add an integer-vector `offset` to `coord` lane-wise,
/// returning a new coord Value of the same type.  Used by the
/// `Image-Operands::ConstOffset / Offset` lowerings for
/// `OpImageFetch` / `OpImageRead` / `OpImageWrite` -- the
/// backends only know scalar `IAdd`, so we decompose to
/// per-lane `VectorExtract` + `IAdd` + `ConstVec` rebuild.
fn lane_add_int_vec(
    coord: Value,
    offset: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Result<Value, FrontendError> {
    let n = match &coord.ty {
        Type::Vec2(_) => 2,
        Type::Vec3(_) => 3,
        Type::Vec4(_) => 4,
        other => return Err(FrontendError::Unsupported(format!(
            "lane_add_int_vec expects a vector coord, got {other:?}"))),
    };
    let mut sum_lanes: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        let cl_id = ValueId(*next_value_id);
        *next_value_id += 1;
        let cl = Value { id: cl_id, ty: Type::I32 };
        insts.push(Inst {
            op: Op::VectorExtract { vector: coord.clone(), index: i as u32 },
            result: Some(cl.clone()),
            source_spirv_offset,
        });
        let ol_id = ValueId(*next_value_id);
        *next_value_id += 1;
        let ol = Value { id: ol_id, ty: Type::I32 };
        insts.push(Inst {
            op: Op::VectorExtract { vector: offset.clone(), index: i as u32 },
            result: Some(ol.clone()),
            source_spirv_offset,
        });
        let s_id = ValueId(*next_value_id);
        *next_value_id += 1;
        let s = Value { id: s_id, ty: Type::I32 };
        insts.push(Inst {
            op: Op::IAdd(cl, ol),
            result: Some(s.clone()),
            source_spirv_offset,
        });
        sum_lanes.push(s);
    }
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let new_coord = Value { id, ty: coord.ty.clone() };
    insts.push(Inst {
        op: Op::ConstVec(sum_lanes),
        result: Some(new_coord.clone()),
        source_spirv_offset,
    });
    Ok(new_coord)
}

/// Same as [`push_extract_lane`] but the returned Value is
/// typed `Type::U32`.  Used by `OpAny` / `OpAll` lowerings
/// where the vec lanes are bool/i32-backed.
fn push_extract_lane_i32(
    vector: Value,
    index: u32,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Result<Value, FrontendError> {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let lane = Value { id, ty: Type::U32 };
    insts.push(Inst {
        op: Op::VectorExtract { vector, index },
        result: Some(lane.clone()),
        source_spirv_offset,
    });
    Ok(lane)
}

/// Synthesize an `Op::VectorExtract` on `vector` at lane
/// `index`, returning the F32 lane Value.  Used by the
/// `ImageSampleProj*` lowering to peel apart the (s, t, [r,]
/// q) coord before dividing by `q`.
fn push_extract_lane(
    vector: Value,
    index: u32,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Result<Value, FrontendError> {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let lane = Value { id, ty: Type::F32 };
    insts.push(Inst {
        op: Op::VectorExtract { vector, index },
        result: Some(lane.clone()),
        source_spirv_offset,
    });
    Ok(lane)
}

/// Emit a u32-typed instruction; returns the result Value.
fn push_u32(
    op: Op,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let v = Value { id, ty: Type::U32 };
    insts.push(Inst { op, result: Some(v.clone()), source_spirv_offset });
    v
}

/// Emit an integer ConstInt with the given value/kind.
fn push_ci(
    value: i64,
    kind: atrium_spv_ir::IntKind,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let ty = match kind {
        atrium_spv_ir::IntKind::I32 => Type::I32,
        atrium_spv_ir::IntKind::U32 => Type::U32,
        atrium_spv_ir::IntKind::I64 => Type::I64,
        atrium_spv_ir::IntKind::U64 => Type::U64,
    };
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let v = Value { id, ty };
    insts.push(Inst {
        op: Op::ConstInt { value, kind },
        result: Some(v.clone()),
        source_spirv_offset,
    });
    v
}

/// Identity element for an OpGroupNonUniform<Op> reduce
/// applied to a single invocation (subgroupSize=1).
/// Returns the IR Op that materialises the identity at
/// `result_ty` (used for ExclusiveScan, which yields the
/// reduction of the empty prefix).
fn identity_for_group_op(
    spv_op: rspirv::spirv::Op,
    result_ty: &Type,
) -> Result<Op, FrontendError> {
    use rspirv::spirv::Op as SpvOp;
    let kind = match result_ty {
        Type::I32 => atrium_spv_ir::IntKind::I32,
        Type::U32 => atrium_spv_ir::IntKind::U32,
        _ => atrium_spv_ir::IntKind::I32, // unused for float
    };
    let int_id = |v: i64| Op::ConstInt { value: v, kind };
    let f32_id = |v: f64| Op::ConstFloat {
        value: v, kind: atrium_spv_ir::FloatKind::F32 };
    Ok(match spv_op {
        SpvOp::GroupNonUniformIAdd
        | SpvOp::GroupNonUniformBitwiseOr
        | SpvOp::GroupNonUniformBitwiseXor
        | SpvOp::GroupNonUniformLogicalOr
        | SpvOp::GroupNonUniformLogicalXor => int_id(0),
        SpvOp::GroupNonUniformIMul => int_id(1),
        SpvOp::GroupNonUniformBitwiseAnd
        | SpvOp::GroupNonUniformLogicalAnd => int_id(-1), // ~0
        SpvOp::GroupNonUniformSMin => int_id(i32::MAX as i64),
        SpvOp::GroupNonUniformSMax => int_id(i32::MIN as i64),
        SpvOp::GroupNonUniformUMin =>
            Op::ConstInt { value: u32::MAX as i64,
                kind: atrium_spv_ir::IntKind::U32 },
        SpvOp::GroupNonUniformUMax =>
            Op::ConstInt { value: 0,
                kind: atrium_spv_ir::IntKind::U32 },
        SpvOp::GroupNonUniformFAdd => f32_id(0.0),
        SpvOp::GroupNonUniformFMul => f32_id(1.0),
        SpvOp::GroupNonUniformFMin => f32_id(f64::INFINITY),
        SpvOp::GroupNonUniformFMax => f32_id(f64::NEG_INFINITY),
        other => return Err(FrontendError::Unsupported(format!(
            "no identity defined for {other:?}"))),
    })
}

/// Range-reduce x to x_red ∈ [-π/2, π/2] for trig polynomials.
///
/// Returns (x_red, sign) where:
///   k     = floor(x/π + 0.5)        // round-to-nearest integer
///   x_red = x - k*π                 // ∈ [-π/2, π/2]
///   sign  = (-1)^k                  // 1.0 or -1.0
///
/// sin(x) = sin(x_red) * sign;  cos(x) = cos(x_red) * sign.
/// For tan, signs cancel in the quotient.
fn synth_trig_reduce(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> (Value, Value) {
    use std::f64::consts::PI;
    let c_inv_pi = push_cf(1.0 / PI, source_spirv_offset, insts, next_value_id);
    let c_pi     = push_cf(PI,        source_spirv_offset, insts, next_value_id);
    let c_half   = push_cf(0.5,       source_spirv_offset, insts, next_value_id);
    let c_neg_two = push_cf(-2.0,     source_spirv_offset, insts, next_value_id);
    let c_one    = push_cf(1.0,       source_spirv_offset, insts, next_value_id);
    let c_neg_two_b = push_cf(-2.0,   source_spirv_offset, insts, next_value_id);
    let c_half_b = push_cf(0.5,       source_spirv_offset, insts, next_value_id);
    // x_over_pi = x * (1/π)
    let x_over_pi = push_f32(Op::FMul(x.clone(), c_inv_pi),
        source_spirv_offset, insts, next_value_id);
    // shifted = x_over_pi + 0.5
    let shifted = push_f32(Op::FAdd(x_over_pi, c_half),
        source_spirv_offset, insts, next_value_id);
    // k = floor(shifted)
    let k = push_f32(Op::FFloor(shifted),
        source_spirv_offset, insts, next_value_id);
    // k_pi = k * π
    let k_pi = push_f32(Op::FMul(k.clone(), c_pi),
        source_spirv_offset, insts, next_value_id);
    // x_red = x - k_pi
    let x_red = push_f32(Op::FSub(x, k_pi),
        source_spirv_offset, insts, next_value_id);
    // half_k = floor(k * 0.5)
    let half_k_in = push_f32(Op::FMul(k.clone(), c_half_b),
        source_spirv_offset, insts, next_value_id);
    let half_k = push_f32(Op::FFloor(half_k_in),
        source_spirv_offset, insts, next_value_id);
    // parity = k - 2*half_k  ∈ {0.0, 1.0}
    let two_half_k = push_f32(Op::FMul(half_k, c_neg_two),
        source_spirv_offset, insts, next_value_id);
    // FAdd(k, -2*half_k)  ≡  k - 2*half_k
    let parity = push_f32(Op::FAdd(k, two_half_k),
        source_spirv_offset, insts, next_value_id);
    // sign = 1 - 2*parity = 1 + (-2)*parity
    let neg_two_parity = push_f32(Op::FMul(parity, c_neg_two_b),
        source_spirv_offset, insts, next_value_id);
    let sign = push_f32(Op::FAdd(c_one, neg_two_parity),
        source_spirv_offset, insts, next_value_id);
    (x_red, sign)
}

/// Push an i32 ConstInt.
fn push_ci32(
    val: i64,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let v = Value { id, ty: Type::I32 };
    insts.push(Inst {
        op: Op::ConstInt { value: val, kind: atrium_spv_ir::IntKind::I32 },
        result: Some(v.clone()),
        source_spirv_offset,
    });
    v
}

/// Push an integer-typed instruction (I32).
fn push_i32(
    op: Op,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let v = Value { id, ty: Type::I32 };
    insts.push(Inst { op, result: Some(v.clone()), source_spirv_offset });
    v
}

/// Synthesise Exp2(x) over the safe f32 range.
///
/// Strategy:
///   x_clamped = clamp(x, -126.0, 127.0)   // avoids denormals / Inf
///   k_float   = floor(x_clamped + 0.5)    // round-to-nearest
///   r         = x_clamped - k_float       // ∈ [-0.5, 0.5]
///   exp2_r    = Horner(r, 5 terms of 2^r Taylor)
///   k_int     = (int)k_float
///   pow2k     = bitcast<f32>((127 + k_int) << 23)
///   result    = exp2_r * pow2k
fn synth_exp2(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    // Clamp x to a safe range.
    let c_lo = push_cf(-126.0, source_spirv_offset, insts, next_value_id);
    let c_hi = push_cf( 127.0, source_spirv_offset, insts, next_value_id);
    let x_lo = push_f32(Op::FMax(x, c_lo), source_spirv_offset, insts, next_value_id);
    let x_clamped = push_f32(Op::FMin(x_lo, c_hi), source_spirv_offset, insts, next_value_id);
    // k_float = floor(x + 0.5)
    let c_half = push_cf(0.5, source_spirv_offset, insts, next_value_id);
    let shifted = push_f32(Op::FAdd(x_clamped.clone(), c_half),
        source_spirv_offset, insts, next_value_id);
    let k_float = push_f32(Op::FFloor(shifted),
        source_spirv_offset, insts, next_value_id);
    // r = x - k_float
    let r = push_f32(Op::FSub(x_clamped, k_float.clone()),
        source_spirv_offset, insts, next_value_id);
    // Horner: ((((c5*r + c4)*r + c3)*r + c2)*r + c1)*r + c0
    // 2^r Taylor coefficients at 0.
    let c0 = push_cf(1.0,               source_spirv_offset, insts, next_value_id);
    let c1 = push_cf(0.6931471805599453,source_spirv_offset, insts, next_value_id);
    let c2 = push_cf(0.2402265069591007,source_spirv_offset, insts, next_value_id);
    let c3 = push_cf(0.0555041086648216,source_spirv_offset, insts, next_value_id);
    let c4 = push_cf(0.0096181291076285,source_spirv_offset, insts, next_value_id);
    let c5 = push_cf(0.0013333558146428,source_spirv_offset, insts, next_value_id);
    let p1 = push_f32(Op::FMul(c5, r.clone()),       source_spirv_offset, insts, next_value_id);
    let p2 = push_f32(Op::FAdd(p1, c4),              source_spirv_offset, insts, next_value_id);
    let p3 = push_f32(Op::FMul(p2, r.clone()),       source_spirv_offset, insts, next_value_id);
    let p4 = push_f32(Op::FAdd(p3, c3),              source_spirv_offset, insts, next_value_id);
    let p5 = push_f32(Op::FMul(p4, r.clone()),       source_spirv_offset, insts, next_value_id);
    let p6 = push_f32(Op::FAdd(p5, c2),              source_spirv_offset, insts, next_value_id);
    let p7 = push_f32(Op::FMul(p6, r.clone()),       source_spirv_offset, insts, next_value_id);
    let p8 = push_f32(Op::FAdd(p7, c1),              source_spirv_offset, insts, next_value_id);
    let p9 = push_f32(Op::FMul(p8, r),               source_spirv_offset, insts, next_value_id);
    let exp2_r = push_f32(Op::FAdd(p9, c0),          source_spirv_offset, insts, next_value_id);
    // pow2k = bitcast((127 + k_int) << 23)
    let k_int = push_i32(Op::ConvertFToS(k_float),
        source_spirv_offset, insts, next_value_id);
    let c_bias = push_ci32(127, source_spirv_offset, insts, next_value_id);
    let biased = push_i32(Op::IAdd(k_int, c_bias),
        source_spirv_offset, insts, next_value_id);
    let c_shift = push_ci32(23, source_spirv_offset, insts, next_value_id);
    let shifted_bits = push_i32(Op::Shl(biased, c_shift),
        source_spirv_offset, insts, next_value_id);
    let pow2k = push_f32(Op::Bitcast(shifted_bits, Type::F32),
        source_spirv_offset, insts, next_value_id);
    push_f32(Op::FMul(exp2_r, pow2k),
        source_spirv_offset, insts, next_value_id)
}

/// Synthesise Log2(x) for x > 0 using mantissa-split + 4-term
/// rational approximation (Mineiro-style).
///
///   bits = bitcast<i32>(x)
///   y    = (float)bits * (1/2^23)        // linear approx of log2(x)+offset
///   mant_bits = (bits & 0x007FFFFF) | 0x3f000000
///   m    = bitcast<f32>(mant_bits)       // m ∈ [0.5, 1.0)
///   log2(x) ≈ y - 124.22551499
///                - 1.498030302 * m
///                - 1.72587999 / (0.3520887068 + m)
///
/// Max relative error ≈ 4e-4 for positive x; undefined for x ≤ 0
/// (caller's responsibility, matching GLSL spec).
///
/// Push a bool-typed instruction (Type::Bool).
fn push_bool(
    op: Op,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    let v = Value { id, ty: Type::Bool };
    insts.push(Inst { op, result: Some(v.clone()), source_spirv_offset });
    v
}

/// 6-coefficient minimax Horner polynomial approximation of
/// atan(x) on x ∈ [-1, 1]. Max error ≈ 5e-7.
fn synth_atan_poly(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let x2 = push_f32(Op::FMul(x.clone(), x.clone()),
        source_spirv_offset, insts, next_value_id);
    let c0 = push_cf( 0.99997726, source_spirv_offset, insts, next_value_id);
    let c1 = push_cf(-0.33262347, source_spirv_offset, insts, next_value_id);
    let c2 = push_cf( 0.19354346, source_spirv_offset, insts, next_value_id);
    let c3 = push_cf(-0.11643287, source_spirv_offset, insts, next_value_id);
    let c4 = push_cf( 0.05265332, source_spirv_offset, insts, next_value_id);
    let c5 = push_cf(-0.01172120, source_spirv_offset, insts, next_value_id);
    let p1 = push_f32(Op::FMul(c5, x2.clone()),  source_spirv_offset, insts, next_value_id);
    let p2 = push_f32(Op::FAdd(p1, c4),          source_spirv_offset, insts, next_value_id);
    let p3 = push_f32(Op::FMul(p2, x2.clone()),  source_spirv_offset, insts, next_value_id);
    let p4 = push_f32(Op::FAdd(p3, c3),          source_spirv_offset, insts, next_value_id);
    let p5 = push_f32(Op::FMul(p4, x2.clone()),  source_spirv_offset, insts, next_value_id);
    let p6 = push_f32(Op::FAdd(p5, c2),          source_spirv_offset, insts, next_value_id);
    let p7 = push_f32(Op::FMul(p6, x2.clone()),  source_spirv_offset, insts, next_value_id);
    let p8 = push_f32(Op::FAdd(p7, c1),          source_spirv_offset, insts, next_value_id);
    let p9 = push_f32(Op::FMul(p8, x2),          source_spirv_offset, insts, next_value_id);
    let p10 = push_f32(Op::FAdd(p9, c0),         source_spirv_offset, insts, next_value_id);
    push_f32(Op::FMul(p10, x), source_spirv_offset, insts, next_value_id)
}

/// Synthesise Atan(x) over the full real line.
///
/// For |x| ≤ 1: direct polynomial.
/// For |x| > 1: sign(x)*π/2 - polynomial(1/x).
fn synth_atan(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let abs_x = push_f32(Op::FAbs(x.clone()),
        source_spirv_offset, insts, next_value_id);
    let one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
    let is_big = push_bool(Op::FOrdGt(abs_x, one.clone()),
        source_spirv_offset, insts, next_value_id);
    // safe denom: x if |x|>1 else 1.0  (avoids 1/0 when x=0)
    let one2 = push_cf(1.0, source_spirv_offset, insts, next_value_id);
    let denom = push_f32(Op::Select {
        cond: is_big.clone(),
        t_val: x.clone(),
        f_val: one2,
    }, source_spirv_offset, insts, next_value_id);
    let one3 = push_cf(1.0, source_spirv_offset, insts, next_value_id);
    let inv = push_f32(Op::FDiv(one3, denom),
        source_spirv_offset, insts, next_value_id);
    let arg = push_f32(Op::Select {
        cond: is_big.clone(),
        t_val: inv,
        f_val: x.clone(),
    }, source_spirv_offset, insts, next_value_id);
    let p = synth_atan_poly(arg, source_spirv_offset, insts, next_value_id);
    // half_pi_signed = sign(x) * π/2
    let zero = push_cf(0.0, source_spirv_offset, insts, next_value_id);
    let is_neg = push_bool(Op::FOrdLt(x, zero),
        source_spirv_offset, insts, next_value_id);
    let half_pi_pos = push_cf( std::f64::consts::FRAC_PI_2,
        source_spirv_offset, insts, next_value_id);
    let half_pi_neg = push_cf(-std::f64::consts::FRAC_PI_2,
        source_spirv_offset, insts, next_value_id);
    let half_pi_signed = push_f32(Op::Select {
        cond: is_neg, t_val: half_pi_neg, f_val: half_pi_pos,
    }, source_spirv_offset, insts, next_value_id);
    let big_branch = push_f32(Op::FSub(half_pi_signed, p.clone()),
        source_spirv_offset, insts, next_value_id);
    let _ = one;
    push_f32(Op::Select {
        cond: is_big, t_val: big_branch, f_val: p,
    }, source_spirv_offset, insts, next_value_id)
}

/// Synthesise Asin(x) on x ∈ (-1, 1) via
/// asin(x) = atan(x / sqrt(1 - x²)).  At the endpoints
/// ±1 the sqrt is 0, division yields ±Inf, and atan's
/// |arg|>1 branch correctly returns ±π/2.
fn synth_asin(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let x2 = push_f32(Op::FMul(x.clone(), x.clone()),
        source_spirv_offset, insts, next_value_id);
    let one = push_cf(1.0, source_spirv_offset, insts, next_value_id);
    let one_minus_x2 = push_f32(Op::FSub(one, x2),
        source_spirv_offset, insts, next_value_id);
    let denom = push_f32(Op::FSqrt(one_minus_x2),
        source_spirv_offset, insts, next_value_id);
    let arg = push_f32(Op::FDiv(x, denom),
        source_spirv_offset, insts, next_value_id);
    synth_atan(arg, source_spirv_offset, insts, next_value_id)
}

fn synth_log2(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    // bits = bitcast<i32>(x)
    let bits = push_i32(Op::Bitcast(x, Type::I32),
        source_spirv_offset, insts, next_value_id);
    // y_int_as_float = (float)bits
    let bits_f = push_f32(Op::ConvertSToF(bits.clone()),
        source_spirv_offset, insts, next_value_id);
    let c_inv_2pow23 = push_cf(1.0 / 8388608.0, source_spirv_offset, insts, next_value_id);
    let y = push_f32(Op::FMul(bits_f, c_inv_2pow23),
        source_spirv_offset, insts, next_value_id);
    // mant_bits = (bits & 0x007FFFFF) | 0x3f000000
    let c_mant_mask = push_ci32(0x007FFFFF, source_spirv_offset, insts, next_value_id);
    let c_exp_half  = push_ci32(0x3f000000, source_spirv_offset, insts, next_value_id);
    let mant_only = push_i32(Op::BitAnd(bits, c_mant_mask),
        source_spirv_offset, insts, next_value_id);
    let mant_bits = push_i32(Op::BitOr(mant_only, c_exp_half),
        source_spirv_offset, insts, next_value_id);
    let m = push_f32(Op::Bitcast(mant_bits, Type::F32),
        source_spirv_offset, insts, next_value_id);
    // term1 = 1.498030302 * m
    let c_a = push_cf(1.498030302, source_spirv_offset, insts, next_value_id);
    let term1 = push_f32(Op::FMul(c_a, m.clone()),
        source_spirv_offset, insts, next_value_id);
    // term2 = 1.72587999 / (0.3520887068 + m)
    let c_b = push_cf(0.3520887068, source_spirv_offset, insts, next_value_id);
    let denom = push_f32(Op::FAdd(c_b, m), source_spirv_offset, insts, next_value_id);
    let c_c = push_cf(1.72587999, source_spirv_offset, insts, next_value_id);
    let term2 = push_f32(Op::FDiv(c_c, denom),
        source_spirv_offset, insts, next_value_id);
    // result = y - 124.22551499 - term1 - term2
    let c_offset = push_cf(124.22551499, source_spirv_offset, insts, next_value_id);
    let s1 = push_f32(Op::FSub(y, c_offset),
        source_spirv_offset, insts, next_value_id);
    let s2 = push_f32(Op::FSub(s1, term1),
        source_spirv_offset, insts, next_value_id);
    push_f32(Op::FSub(s2, term2),
        source_spirv_offset, insts, next_value_id)
}

/// 4-term Horner-form sin Taylor polynomial on x ∈ [-π/2, π/2].
///   p = -1/5040, then p = p*x²+1/120, p = p*x²-1/6, p = p*x²+1
///   sin = p * x
fn synth_sin_poly(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let x2 = push_f32(Op::FMul(x.clone(), x.clone()),
        source_spirv_offset, insts, next_value_id);
    let c_inv5040 = push_cf(-1.0 / 5040.0, source_spirv_offset, insts, next_value_id);
    let c_inv120  = push_cf( 1.0 /  120.0, source_spirv_offset, insts, next_value_id);
    let c_neg_inv6 = push_cf(-1.0 / 6.0,    source_spirv_offset, insts, next_value_id);
    let c_one     = push_cf(1.0,            source_spirv_offset, insts, next_value_id);
    let p1 = push_f32(Op::FMul(c_inv5040, x2.clone()), source_spirv_offset, insts, next_value_id);
    let p2 = push_f32(Op::FAdd(p1, c_inv120),          source_spirv_offset, insts, next_value_id);
    let p3 = push_f32(Op::FMul(p2, x2.clone()),        source_spirv_offset, insts, next_value_id);
    let p4 = push_f32(Op::FAdd(p3, c_neg_inv6),        source_spirv_offset, insts, next_value_id);
    let p5 = push_f32(Op::FMul(p4, x2),                source_spirv_offset, insts, next_value_id);
    let p6 = push_f32(Op::FAdd(p5, c_one),             source_spirv_offset, insts, next_value_id);
    push_f32(Op::FMul(p6, x), source_spirv_offset, insts, next_value_id)
}

/// 5-term Horner-form cos Taylor polynomial on x ∈ [-π/2, π/2].
///   p = 1/40320, then p = p*x²-1/720, p = p*x²+1/24, p = p*x²-1/2, p = p*x²+1
fn synth_cos_poly(
    x: Value,
    source_spirv_offset: u32,
    insts: &mut Vec<Inst>,
    next_value_id: &mut u32,
) -> Value {
    let x2 = push_f32(Op::FMul(x.clone(), x), source_spirv_offset, insts, next_value_id);
    let c_inv40320   = push_cf( 1.0 / 40320.0, source_spirv_offset, insts, next_value_id);
    let c_neg_inv720 = push_cf(-1.0 /   720.0, source_spirv_offset, insts, next_value_id);
    let c_inv24      = push_cf( 1.0 /    24.0, source_spirv_offset, insts, next_value_id);
    let c_neg_half   = push_cf(-0.5,           source_spirv_offset, insts, next_value_id);
    let c_one        = push_cf( 1.0,           source_spirv_offset, insts, next_value_id);
    let p1 = push_f32(Op::FMul(c_inv40320, x2.clone()), source_spirv_offset, insts, next_value_id);
    let p2 = push_f32(Op::FAdd(p1, c_neg_inv720),       source_spirv_offset, insts, next_value_id);
    let p3 = push_f32(Op::FMul(p2, x2.clone()),         source_spirv_offset, insts, next_value_id);
    let p4 = push_f32(Op::FAdd(p3, c_inv24),            source_spirv_offset, insts, next_value_id);
    let p5 = push_f32(Op::FMul(p4, x2.clone()),         source_spirv_offset, insts, next_value_id);
    let p6 = push_f32(Op::FAdd(p5, c_neg_half),         source_spirv_offset, insts, next_value_id);
    let p7 = push_f32(Op::FMul(p6, x2),                 source_spirv_offset, insts, next_value_id);
    push_f32(Op::FAdd(p7, c_one), source_spirv_offset, insts, next_value_id)
}

fn expect_id(operands: &[Operand], i: usize) -> Result<Word, FrontendError> {
    match operands.get(i) {
        Some(Operand::IdRef(id)) => Ok(*id),
        other => Err(FrontendError::Malformed(format!(
            "expected IdRef at operand {i}, got {other:?}",
        ))),
    }
}

/// After [`translate_all`], walk the SPIR-V module's
/// function order to patch entry_point.function_index in
/// the InterfaceContext-produced entry_points list.
///
/// Returns the patched list ready to be stored on the
/// final Module.
pub fn patch_entry_point_indices(
    module: &Module,
    iface: &InterfaceContext,
) -> Vec<atrium_spv_ir::EntryPoint> {
    let mut out = iface.entry_points.clone();
    for (idx, spv_func) in module.functions.iter().enumerate() {
        let fn_id = spv_func.def.as_ref().and_then(|d| d.result_id);
        if let Some(fn_id) = fn_id {
            if let Some(ep_idx) = iface.entry_function_ids.get(&fn_id) {
                out[*ep_idx].function_index = idx;
            }
        }
    }
    out
}
