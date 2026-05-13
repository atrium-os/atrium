//! SPIR-V constant ids → IR-friendly constant values.
//!
//! Phase 1 v1 stores constants as a side table indexed by
//! SPIR-V id. When the function-translation pass
//! encounters a use of a constant id, it emits an
//! [`atrium_spv_ir::Op::ConstInt`] / `ConstFloat` /
//! `ConstVec` / `ConstNull` instruction with the result
//! mapped back to that id in the SSA renaming map.

use std::collections::HashMap;

use atrium_spv_ir::{FloatKind, IntKind, Op, Type};
use rspirv::dr::{Instruction, Module, Operand};
use rspirv::spirv::{Op as SpvOp, Word};

use crate::error::FrontendError;
use crate::types::TypeContext;

/// One stored SPIR-V constant.
///
/// `result_type` is the SPIR-V id of the constant's
/// result type — looked up against [`TypeContext`] later
/// to construct the IR [`atrium_spv_ir::Value`] when this
/// constant is referenced.
#[derive(Debug, Clone)]
pub struct StoredConstant {
    /// SPIR-V id of the constant's result type.
    pub type_id: Word,
    /// The IR Op that constructs this constant value.
    pub op: Op,
}

/// Map from SPIR-V constant id → [`StoredConstant`].
#[derive(Debug, Default)]
pub struct ConstantContext {
    pub(crate) constants: HashMap<Word, StoredConstant>,
}

impl ConstantContext {
    /// Walk OpConstant* in declaration order.
    pub fn build(
        module: &Module,
        types: &TypeContext,
    ) -> Result<Self, FrontendError> {
        let mut ctx = ConstantContext::default();
        for inst in &module.types_global_values {
            let id = match inst.result_id { Some(id) => id, None => continue };
            let constant = match inst.class.opcode {
                SpvOp::Constant => translate_op_constant(inst, types)?,
                SpvOp::ConstantTrue => StoredConstant {
                    type_id: inst.result_type.ok_or_else(|| FrontendError::Malformed(
                        "ConstantTrue without result type".to_string()))?,
                    op: Op::ConstInt { value: 1, kind: IntKind::I32 },
                },
                SpvOp::ConstantFalse => StoredConstant {
                    type_id: inst.result_type.ok_or_else(|| FrontendError::Malformed(
                        "ConstantFalse without result type".to_string()))?,
                    op: Op::ConstInt { value: 0, kind: IntKind::I32 },
                },
                SpvOp::ConstantNull => StoredConstant {
                    type_id: inst.result_type.ok_or_else(|| FrontendError::Malformed(
                        "ConstantNull without result type".to_string()))?,
                    op: Op::ConstNull,
                },
                SpvOp::ConstantComposite => translate_constant_composite(
                    inst, &ctx,
                )?,
                _ => continue,
            };
            ctx.constants.insert(id, constant);
        }
        Ok(ctx)
    }

    /// Get a constant by SPIR-V id.
    pub fn get(&self, id: Word) -> Option<&StoredConstant> {
        self.constants.get(&id)
    }
}

fn translate_op_constant(
    inst: &Instruction,
    types: &TypeContext,
) -> Result<StoredConstant, FrontendError> {
    let type_id = inst.result_type.ok_or_else(|| FrontendError::Malformed(
        "Constant without result type".to_string()))?;
    let ty = types.get(type_id)?;
    match ty {
        Type::I32 | Type::U32 => {
            let bits = match inst.operands.first() {
                Some(Operand::LiteralBit32(v)) => *v,
                other => return Err(FrontendError::Malformed(format!(
                    "Constant<i32/u32> expected LiteralBit32, got {other:?}",
                ))),
            };
            let kind = if matches!(ty, Type::I32) { IntKind::I32 } else { IntKind::U32 };
            let value = if matches!(ty, Type::I32) {
                bits as i32 as i64
            } else {
                bits as i64
            };
            Ok(StoredConstant {
                type_id,
                op: Op::ConstInt { value, kind },
            })
        }
        Type::I64 | Type::U64 => {
            // SPIR-V 64-bit constants encode as either a single
            // LiteralBit64 or two LiteralBit32 (low, high). Handle
            // both.
            let value = match inst.operands.first() {
                Some(Operand::LiteralBit64(v)) => *v as i64,
                Some(Operand::LiteralBit32(lo)) => {
                    let hi = match inst.operands.get(1) {
                        Some(Operand::LiteralBit32(v)) => *v,
                        other => return Err(FrontendError::Malformed(format!(
                            "Constant<i64/u64> expected LiteralBit32 high half, got {other:?}",
                        ))),
                    };
                    (((hi as u64) << 32) | (*lo as u64)) as i64
                }
                other => return Err(FrontendError::Malformed(format!(
                    "Constant<i64/u64> expected literal, got {other:?}",
                ))),
            };
            let kind = if matches!(ty, Type::I64) { IntKind::I64 } else { IntKind::U64 };
            Ok(StoredConstant {
                type_id,
                op: Op::ConstInt { value, kind },
            })
        }
        Type::F32 => {
            let bits = match inst.operands.first() {
                Some(Operand::LiteralBit32(v)) => *v,
                other => return Err(FrontendError::Malformed(format!(
                    "Constant<f32> expected LiteralBit32, got {other:?}",
                ))),
            };
            Ok(StoredConstant {
                type_id,
                op: Op::ConstFloat {
                    value: f32::from_bits(bits) as f64,
                    kind: FloatKind::F32,
                },
            })
        }
        Type::F64 => {
            let bits = match inst.operands.first() {
                Some(Operand::LiteralBit64(v)) => *v,
                Some(Operand::LiteralBit32(lo)) => {
                    let hi = match inst.operands.get(1) {
                        Some(Operand::LiteralBit32(v)) => *v,
                        other => return Err(FrontendError::Malformed(format!(
                            "Constant<f64> expected LiteralBit32 high half, got {other:?}",
                        ))),
                    };
                    ((hi as u64) << 32) | (*lo as u64)
                }
                other => return Err(FrontendError::Malformed(format!(
                    "Constant<f64> expected literal, got {other:?}",
                ))),
            };
            Ok(StoredConstant {
                type_id,
                op: Op::ConstFloat {
                    value: f64::from_bits(bits),
                    kind: FloatKind::F64,
                },
            })
        }
        other => Err(FrontendError::Unsupported(format!(
            "OpConstant for type {other:?} not supported",
        ))),
    }
}

fn translate_constant_composite(
    inst: &Instruction,
    ctx: &ConstantContext,
) -> Result<StoredConstant, FrontendError> {
    let type_id = inst.result_type.ok_or_else(|| FrontendError::Malformed(
        "ConstantComposite without result type".to_string()))?;
    // We can't yet build the inner `Value` list — the
    // function-translation pass owns the SSA renaming and
    // will look up these constituent constants when it
    // emits the composite. For now we store the
    // operand id list as a `ConstVec` with placeholder
    // values; the function pass replaces those with real
    // Values during translation.
    //
    // Phase 1 v1's narrow scope means we only encounter
    // ConstantComposite for vec4 outputs, which the
    // function pass handles specially. Storing the
    // op-with-placeholder lets us defer the real
    // construction to the place that has the SSA-renaming
    // context.
    let mut element_ids = Vec::with_capacity(inst.operands.len());
    for op in &inst.operands {
        match op {
            Operand::IdRef(id) => {
                if !ctx.constants.contains_key(id) {
                    return Err(FrontendError::Malformed(format!(
                        "ConstantComposite references unknown constant {id}",
                    )));
                }
                element_ids.push(*id);
            }
            other => return Err(FrontendError::Malformed(format!(
                "ConstantComposite expected IdRef, got {other:?}",
            ))),
        }
    }
    // Stash the element ids on the op via a side channel:
    // we re-use Op::ConstVec but with an empty Vec, and
    // the function pass reads the operand list directly
    // from the SPIR-V instruction when it needs the
    // element ids. To make that possible we also return
    // the raw element-id list via the ConstantContext —
    // but for v1 the function pass walks the source
    // SPIR-V instruction directly.
    let _ = element_ids; // see comment above
    Ok(StoredConstant {
        type_id,
        op: Op::ConstVec(Vec::new()),
    })
}
