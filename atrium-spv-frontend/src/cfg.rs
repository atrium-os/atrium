//! Structured CFG recovery from SPIR-V's
//! `OpSelectionMerge` / `OpLoopMerge` markers (constraint
//! A4).
//!
//! # Phase status
//!
//! **Phase 1 v3 step 1** — structured-CFG classifier.
//!
//! Walks every block in a function and classifies each by
//! its role in the structured CFG: `Linear`, `IfHeader`,
//! `LoopHeader`, `SwitchHeader`, or `Merge`. The
//! classification is what later passes consume to emit
//! the equivalent control flow.
//!
//! For now we only *validate + classify*; actual
//! multi-block IR generation still lands a step later, and
//! the existing single-block translator continues to
//! handle that case directly. Functions that contain any
//! control flow we don't yet translate are rejected with a
//! structured diagnostic (which header kind / which block
//! id) rather than the previous "more than 1 block" blob.
//!
//! Rules (SPIR-V spec §2.11 Structured Control Flow):
//!
//! - A block ending in `OpBranchConditional` MUST be
//!   preceded by an `OpSelectionMerge` (giving the merge
//!   block) OR an `OpLoopMerge` (giving merge + continue).
//! - A block ending in `OpSwitch` MUST be preceded by an
//!   `OpSelectionMerge`.
//! - All other terminator-bearing blocks are `Linear`.
//! - A block referenced by an `OpSelectionMerge` /
//!   `OpLoopMerge`'s `Merge Block` operand is classified
//!   as `Merge`.

use std::collections::HashMap;

use atrium_spv_ir::BlockKind;
use rspirv::dr::{Function, Operand};
use rspirv::spirv::{Op as SpvOp, Word};

use crate::error::FrontendError;

/// Classification of every block in a SPIR-V function.
///
/// Keyed by the SPIR-V block label-id (the `OpLabel`
/// result-id). Returned by [`classify`] for later passes
/// to consume.
#[derive(Debug, Default, Clone)]
pub struct BlockClassification {
    /// SPIR-V block label-id → classified `BlockKind`.
    pub kinds: HashMap<Word, BlockKind>,
}

impl BlockClassification {
    /// Look up the classification for a given block id.
    pub fn get(&self, id: Word) -> Option<&BlockKind> {
        self.kinds.get(&id)
    }
}

/// Classify every block in `func`.
///
/// Returns a map keyed by SPIR-V label id. Each block gets
/// exactly one entry. A block can only appear in one role
/// (Linear / IfHeader / LoopHeader / SwitchHeader / Merge);
/// a SPIR-V module that names the same block twice would
/// have been rejected by the validator long before we get
/// here.
pub fn classify(func: &Function) -> Result<BlockClassification, FrontendError> {
    let mut out = BlockClassification::default();

    // First pass: walk every block; if its terminator is a
    // structured branch, the *previous* instruction
    // declares the merge (and optionally continue) targets.
    for block in &func.blocks {
        let label_id = block
            .label
            .as_ref()
            .and_then(|l| l.result_id)
            .ok_or_else(|| FrontendError::Malformed(
                "block without OpLabel".to_string()))?;

        let insts = &block.instructions;
        // The terminator is the LAST instruction.
        let term = insts.last().ok_or_else(|| FrontendError::Malformed(format!(
            "block {label_id} has no instructions",
        )))?;
        // The merge marker (if any) is the one before it.
        let merge_marker = if insts.len() >= 2 {
            Some(&insts[insts.len() - 2])
        } else {
            None
        };

        match term.class.opcode {
            SpvOp::BranchConditional => {
                match merge_marker.map(|m| m.class.opcode) {
                    Some(SpvOp::SelectionMerge) => {
                        let merge_id = read_id_ref(
                            &merge_marker.unwrap().operands, 0)?;
                        out.kinds.insert(label_id, BlockKind::IfHeader {
                            merge: atrium_spv_ir::BlockId(merge_id),
                        });
                        out.kinds.entry(merge_id)
                            .or_insert(BlockKind::Merge);
                    }
                    Some(SpvOp::LoopMerge) => {
                        let merge_id = read_id_ref(
                            &merge_marker.unwrap().operands, 0)?;
                        let continue_id = read_id_ref(
                            &merge_marker.unwrap().operands, 1)?;
                        out.kinds.insert(label_id, BlockKind::LoopHeader {
                            merge: atrium_spv_ir::BlockId(merge_id),
                            continue_: atrium_spv_ir::BlockId(continue_id),
                        });
                        out.kinds.entry(merge_id)
                            .or_insert(BlockKind::Merge);
                    }
                    _ => return Err(FrontendError::Unsupported(format!(
                        "block {label_id}: OpBranchConditional without a \
                         preceding OpSelectionMerge / OpLoopMerge \
                         (unstructured CFG)",
                    ))),
                }
            }
            SpvOp::Switch => {
                match merge_marker.map(|m| m.class.opcode) {
                    Some(SpvOp::SelectionMerge) => {
                        let merge_id = read_id_ref(
                            &merge_marker.unwrap().operands, 0)?;
                        out.kinds.insert(label_id, BlockKind::SwitchHeader {
                            merge: atrium_spv_ir::BlockId(merge_id),
                        });
                        out.kinds.entry(merge_id)
                            .or_insert(BlockKind::Merge);
                    }
                    _ => return Err(FrontendError::Unsupported(format!(
                        "block {label_id}: OpSwitch without a preceding \
                         OpSelectionMerge (unstructured CFG)",
                    ))),
                }
            }
            // Linear terminators: unconditional branch,
            // return, kill/terminate-invocation.
            SpvOp::Branch | SpvOp::Return | SpvOp::ReturnValue
            | SpvOp::Kill | SpvOp::TerminateInvocation | SpvOp::Unreachable => {
                out.kinds.entry(label_id).or_insert(BlockKind::Linear);
            }
            other => return Err(FrontendError::Unsupported(format!(
                "block {label_id}: terminator {other:?} not supported",
            ))),
        }
    }
    Ok(out)
}

/// Single-block guard kept for the existing translator.
///
/// Until [`translate_one`](crate::functions) handles the
/// classified multi-block forms, we still reject anything
/// past one block — but with a richer diagnostic now that
/// [`classify`] has run.
pub fn reject_unstructured(func: &Function) -> Result<(), FrontendError> {
    if func.blocks.len() <= 1 { return Ok(()); }
    // Validate the CFG shape even though we can't translate
    // it yet — earlier rejection produces a better error
    // (we mention the header kinds, not just "n blocks").
    let cls = classify(func)?;
    let header_kinds: Vec<_> = cls.kinds.values()
        .filter(|k| !matches!(k, BlockKind::Linear | BlockKind::Merge))
        .collect();
    Err(FrontendError::Unsupported(format!(
        "function has {} blocks ({} structured headers); \
         phase 1 v3 step 1 classifies but does not yet translate \
         multi-block CFG. Step 2 will land if/else translation",
        func.blocks.len(), header_kinds.len(),
    )))
}

fn read_id_ref(operands: &[Operand], i: usize) -> Result<Word, FrontendError> {
    match operands.get(i) {
        Some(Operand::IdRef(id)) => Ok(*id),
        other => Err(FrontendError::Malformed(format!(
            "expected IdRef at operand {i}, got {other:?}",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, SelectionControl, StorageClass,
    };

    /// Helper: build a fragment shader with a single
    /// structured if/else writing to a vec4 output.
    fn build_if_else_module() -> rspirv::dr::Module {
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 0);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

        let void = b.type_void();
        let f32_ty = b.type_float(32, None);
        let bool_ty = b.type_bool();
        let vec4_f32 = b.type_vector(f32_ty, 4);
        let void_fn = b.type_function(void, vec![]);
        let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4_f32);

        let cf  = b.constant_bit32(f32_ty, 0.5f32.to_bits());
        let thr = b.constant_bit32(f32_ty, 0.3f32.to_bits());
        let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
        let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
        let v_then = b.constant_composite(vec4_f32, vec![c1, c0, c0, c1]);
        let v_else = b.constant_composite(vec4_f32, vec![c0, c0, c1, c1]);

        let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
        let _main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let cond = b.f_ord_less_than(bool_ty, None, cf, thr).unwrap();
        let then_id = b.id();
        let else_id = b.id();
        let merge_id = b.id();
        b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
        b.branch_conditional(cond, then_id, else_id, vec![]).unwrap();
        b.begin_block(Some(then_id)).unwrap();
        b.store(out, v_then, None, vec![]).unwrap();
        b.branch(merge_id).unwrap();
        b.begin_block(Some(else_id)).unwrap();
        b.store(out, v_else, None, vec![]).unwrap();
        b.branch(merge_id).unwrap();
        b.begin_block(Some(merge_id)).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::Fragment, _main, "main", vec![out]);
        b.execution_mode(_main, ExecutionMode::OriginUpperLeft, vec![]);
        b.module()
    }

    #[test]
    fn classifies_if_else_shapes() {
        let module = build_if_else_module();
        let func = &module.functions[0];
        let cls = classify(func).expect("must classify");
        // Exactly one IfHeader, two Linear (then/else),
        // one Merge.
        let mut counts = (0, 0, 0);
        for k in cls.kinds.values() {
            match k {
                BlockKind::IfHeader { .. } => counts.0 += 1,
                BlockKind::Linear => counts.1 += 1,
                BlockKind::Merge => counts.2 += 1,
                _ => {}
            }
        }
        assert_eq!(counts, (1, 2, 1),
            "expected 1 IfHeader + 2 Linear + 1 Merge, got {counts:?}");
    }

    #[test]
    fn round_trips_via_assemble_for_real_spirv() {
        // Sanity: serialise + reparse the synthesised module
        // and confirm classify still works on the parsed
        // form (the rspirv builder might produce a different
        // in-memory shape than the parser).
        let module = build_if_else_module();
        let words: Vec<u32> = module.assemble();
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
        let mut loader = rspirv::dr::Loader::new();
        rspirv::binary::parse_bytes(&bytes, &mut loader).expect("reparse");
        let parsed = loader.module();
        let cls = classify(&parsed.functions[0]).expect("classify on parsed");
        assert!(cls.kinds.values().any(|k| matches!(k, BlockKind::IfHeader { .. })));
        assert!(cls.kinds.values().any(|k| matches!(k, BlockKind::Merge)));
    }
}
