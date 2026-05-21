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

    Ok(Function {
        name,
        stage,
        params: Vec::new(), // no params in v1 narrow scope
        return_type,
        entry_block: entry_block_id,
        blocks,
        local_size,
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
            id_map, next_value_id, insts, source_spirv_offset,
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
        SpvOp::SMod => emit_binop_int(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::SMod(a, b),
        ),
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
        // OpFMul + OpVectorTimesScalar both lower to
        // Op::FMul; the backend's emit_float_binop
        // dispatches on (scalar × scalar) / (vec × vec) /
        // (vec × scalar with broadcast) by inspecting the
        // operand storage.
        SpvOp::FMul | SpvOp::VectorTimesScalar => emit_binop_float(
            spv_inst, types, constants, iface,
            id_map, next_value_id, insts, source_spirv_offset,
            |a, b| Op::FMul(a, b),
        ),
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
            let base = resolve_variable(
                base_id, types, iface, id_map, next_value_id,
            )?.ok_or_else(|| FrontendError::Unsupported(format!(
                "AccessChain base id {base_id} is not a Variable",
            )))?;

            // Recover the variable's pointee type id from
            // iface.variables so we can walk struct members
            // for offset resolution.
            let (_storage, mut current_pointee_id) = iface.variables
                .get(&base_id)
                .copied()
                .ok_or_else(|| FrontendError::Malformed(format!(
                    "AccessChain base var {base_id} not in iface.variables",
                )))?;

            let mut byte_offset: u32 = 0;
            for op in spv_inst.operands.iter().skip(1) {
                let idx_id = match op {
                    Operand::IdRef(id) => *id,
                    other => return Err(FrontendError::Malformed(format!(
                        "AccessChain expected IdRef index, got {other:?}",
                    ))),
                };
                let idx_const = constants.get(idx_id).ok_or_else(||
                    FrontendError::Unsupported(format!(
                        "AccessChain index id {idx_id} is not a constant \
                         (constraint B5 requires constant indices)",
                    )))?;
                let idx_val: u32 = match &idx_const.kind {
                    ConstantKind::Scalar(Op::ConstInt { value, .. }) =>
                        *value as u32,
                    other => return Err(FrontendError::Unsupported(format!(
                        "AccessChain index must be a scalar int constant, got {other:?}",
                    ))),
                };

                // Step through the pointee. If it's a
                // struct we know about, descend via the
                // recorded layout; otherwise we don't
                // support deeper chains yet.
                if let Some(layout) = iface.struct_layouts.get(&current_pointee_id) {
                    let member = layout.get(idx_val as usize).ok_or_else(||
                        FrontendError::Malformed(format!(
                            "AccessChain index {idx_val} out of range for struct \
                             with {} members",
                            layout.len(),
                        )))?;
                    byte_offset = byte_offset.saturating_add(member.byte_offset);
                    current_pointee_id = member.type_id;
                } else {
                    return Err(FrontendError::Unsupported(format!(
                        "AccessChain step through non-struct pointee \
                         (type id {current_pointee_id}) not supported yet",
                    )));
                }
            }

            let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::AccessChain { base, byte_offset },
                result: Some(result),
                source_spirv_offset,
            });
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
        SpvOp::ImageSampleImplicitLod | SpvOp::ImageSampleExplicitLod => {
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
            let coord = id_map.get(&coord_id).cloned().ok_or_else(||
                FrontendError::Malformed(format!(
                    "ImageSample coord id {coord_id} not yet defined")))?;
            let op = if spv_inst.class.opcode == SpvOp::ImageSampleExplicitLod {
                let lod_id = expect_id(&spv_inst.operands, 2)?;
                let lod = id_map.get(&lod_id).cloned().ok_or_else(||
                    FrontendError::Malformed(format!(
                        "ImageSampleExplicitLod lod id {lod_id} not yet defined")))?;
                Op::ImageSampleExplicitLod { sampled_image, coord, lod }
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
            // Operands mask; we don't decode it yet — the
            // runtime helper ignores `lod` in v1, so we pass
            // None unconditionally.
            let lod = None;
            let result = alloc_or_get_result(result_id, result_ty, id_map, next_value_id);
            insts.push(Inst {
                op: Op::ImageFetch { image, coord, lod },
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
