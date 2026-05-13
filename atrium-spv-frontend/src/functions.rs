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
use crate::constants::ConstantContext;
use crate::error::FrontendError;
use crate::interface::InterfaceContext;
use crate::types::TypeContext;

/// Translate every function in the module.
///
/// Each function becomes one [`Function`] in the returned
/// `Vec`. Order matches the SPIR-V module's function order
/// (so an entry-point's `function_index` is a direct lookup
/// against this Vec).
pub fn translate_all(
    module: &Module,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
) -> Result<Vec<Function>, FrontendError> {
    let mut out = Vec::with_capacity(module.functions.len());
    for func in &module.functions {
        let translated = translate_one(func, types, constants, iface)?;
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

    for spv_inst in &spv_block.instructions {
        if let Some(ir_inst) = translate_inst(
            spv_inst,
            types,
            constants,
            iface,
            &mut id_map,
            &mut next_value_id,
        )? {
            insts.push(ir_inst);
        }
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

/// Translate one SPIR-V instruction.
///
/// Returns `Ok(None)` for instructions we deliberately drop
/// (debug names, decorations rolled into the interface
/// pass, etc.). Returns `Ok(Some(inst))` when we emit an
/// IR instruction.
fn translate_inst(
    spv_inst: &Instruction,
    types: &TypeContext,
    constants: &ConstantContext,
    iface: &InterfaceContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
) -> Result<Option<Inst>, FrontendError> {
    let _ = iface; // reserved for future passes (descriptor lookup)

    // Source SPIR-V offset for the PC-map sidecar (constraint A2).
    // rspirv doesn't surface the original byte offset on dr::Instruction,
    // so for phase 1 v1 we use 0 as a placeholder. Phase 1 v2 wires this
    // through a parser-level adapter (a custom Consumer impl) that records
    // offsets per instruction.
    let source_spirv_offset = 0;

    match spv_inst.class.opcode {
        // Block label — handled by block-walking, no IR emit.
        SpvOp::Label | SpvOp::Nop => Ok(None),

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
                    ptr_id, types, constants, id_map, next_value_id,
                )?,
            };
            let value_value = resolve_value(
                value_id, types, constants, id_map, next_value_id,
            )?;
            // OpStore has no result.
            Ok(Some(Inst {
                op: Op::Store { ptr: ptr_value, value: value_value },
                result: None,
                source_spirv_offset,
            }))
        }

        SpvOp::Return => Ok(Some(Inst {
            op: Op::Return,
            result: None,
            source_spirv_offset,
        })),

        SpvOp::ReturnValue => {
            let val_id = expect_id(&spv_inst.operands, 0)?;
            let val = resolve_value(
                val_id, types, constants, id_map, next_value_id,
            )?;
            Ok(Some(Inst {
                op: Op::ReturnValue(val),
                result: None,
                source_spirv_offset,
            }))
        }

        SpvOp::FunctionEnd => Ok(None),

        other => Err(FrontendError::Unsupported(format!(
            "opcode {other:?} not supported in phase 1 v1",
        ))),
    }
}

/// Resolve a SPIR-V id → IR Value, materialising a constant
/// or variable handle into a fresh SSA value the first
/// time we see it.
fn resolve_value(
    id: Word,
    types: &TypeContext,
    constants: &ConstantContext,
    id_map: &mut HashMap<Word, Value>,
    next_value_id: &mut u32,
) -> Result<Value, FrontendError> {
    if let Some(v) = id_map.get(&id) { return Ok(v.clone()); }

    // Is it a constant?
    if let Some(stored) = constants.get(id) {
        let ty = types.get(stored.type_id)?.clone();
        let v = fresh_value(ty, next_value_id);
        id_map.insert(id, v.clone());
        // Phase 1 v1: constants used in the body are
        // expected to have been materialised into the
        // instruction stream by the function-translation
        // pass. For OpStore-of-const-composite (the
        // canonical phase-0c case), the const composite's
        // *value* doesn't actually need an IR Inst — the
        // backend can reference the stored constant
        // directly via Op::ConstVec lookup. So we don't
        // emit an Inst here; the OpStore's value reads
        // through the SSA id_map to find this Value.
        //
        // For richer cases (OpIAdd of two constants, etc.)
        // we'll need to materialise the constant as an
        // actual IR instruction. Phase 1 v2 handles that
        // when arithmetic lands.
        return Ok(v);
    }

    // Variables: we need a different path that consults
    // `iface.variables`. resolve_value's signature didn't
    // thread the interface context through; refactor to
    // accept it in a follow-up commit if we hit a shader
    // that needs OpLoad/OpStore on a variable id that
    // wasn't already mapped by the function pass.
    //
    // For phase 1 v1's constant-store case, the OpStore
    // path materialises the Output-variable id via
    // `resolve_variable` directly before calling
    // resolve_value on it — see `translate_inst`.

    Err(FrontendError::Malformed(format!(
        "SSA id {id} referenced but not defined (no constant, no variable)",
    )))
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
