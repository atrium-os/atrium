//! Function body translation.
//!
//! Walks each SPIR-V function's instructions and emits
//! the corresponding atrium-spv-ir [`Function`] structure.
//! Maintains an `id_map` from SPIR-V id → IR
//! [`ValueId`] so all SSA references stay consistent.

use std::collections::HashMap;

use atrium_spv_ir::{
    Block, BlockId, BlockKind, Function, Inst, Op, ShaderStage, Type, Value, ValueId,
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
    // Reject control flow per v1 stub.
    cfg::reject_unstructured(spv)?;

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
    let mut id_map: HashMap<Word, Value> = HashMap::new();
    let mut next_value_id: u32 = 0;
    let mut blocks: HashMap<BlockId, Block> = HashMap::new();

    // Phase 1 v1: exactly one block per cfg::reject_unstructured.
    let block_id = BlockId(0);
    let spv_block = &spv.blocks[0];
    let mut insts: Vec<Inst> = Vec::new();

    // The OffsetTable index for this function's first
    // body instruction:
    //   fn_start_index points at OpFunction.
    //   + 1 OpFunction itself
    //   + parameters.len() OpFunctionParameter records
    //   + 1 OpLabel of the first (and only) block
    let mut spv_inst_index = fn_start_index
        + 1
        + spv.parameters.len()
        + spv_block.label.iter().count();

    for spv_inst in &spv_block.instructions {
        let source_offset = offsets.get(spv_inst_index);
        translate_inst(
            spv_inst,
            types,
            constants,
            iface,
            &mut id_map,
            &mut next_value_id,
            &mut insts,
            source_offset,
        )?;
        spv_inst_index += 1;
    }

    blocks.insert(block_id, Block {
        id: block_id,
        kind: BlockKind::Linear,
        insts,
    });

    Ok(Function {
        name,
        stage,
        params: Vec::new(), // no params in v1 narrow scope
        return_type,
        entry_block: block_id,
        blocks,
    })
}

/// Translate one SPIR-V instruction. Pushes zero or more
/// [`Inst`]s onto `insts` — constants that need
/// materialising prefix any non-constant use.
///
/// `source_spirv_offset` is the byte offset of this
/// instruction in the source SPIR-V; preserved on every
/// emitted IR Inst per constraint A2.
fn translate_inst(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
    insts: &mut Vec<Inst>,
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

        SpvOp::Return => {
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
        SpvOp::FMul => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FMul(a, b),
        ),
        SpvOp::FDiv => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FDiv(a, b),
        ),
        SpvOp::FNegate => emit_unop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a| Op::FNeg(a),
        ),

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
            let result = fresh_value(ty, next_value_id);
            id_map.insert(result_id, result.clone());
            insts.push(Inst {
                op: Op::ConstVec(elements),
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
    let result = fresh_value(ty, next_value_id);
    id_map.insert(result_id, result.clone());
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
    let result = fresh_value(ty, next_value_id);
    id_map.insert(result_id, result.clone());
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
    let pointee = types.get(*pointee_id)?.clone();
    let storage = crate::types::translate_storage(*storage)?;
    let ty = Type::Pointer(storage, Box::new(pointee));
    let v = fresh_value(ty, next_value_id);
    id_map.insert(id, v.clone());
    Ok(Some(v))
}

fn fresh_value(ty: Type, next_value_id: &mut u32) -> Value {
    let id = ValueId(*next_value_id);
    *next_value_id += 1;
    Value { id, ty }
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
