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
    /// Logical shape of the constant.
    pub kind: ConstantKind,
}

/// Logical structure of a stored constant.
///
/// The function-translation pass uses this to materialise
/// the constant as one or more IR instructions on first
/// use. Scalars become one `Op::ConstInt` or `ConstFloat`;
/// composites become N scalar-defining Insts plus one
/// `Op::ConstVec` aggregating them.
#[derive(Debug, Clone)]
pub enum ConstantKind {
    /// A scalar (int or float). The Op variant carries
    /// the actual value.
    Scalar(Op),
    /// The null / zero value for the constant's type.
    Null,
    /// A composite (vector or matrix) whose elements
    /// are themselves stored constants. SPIR-V ids of
    /// the element constants, in lane order.
    Composite(Vec<Word>),
}

/// Map from SPIR-V constant id → [`StoredConstant`].
#[derive(Debug, Default)]
pub struct ConstantContext {
    pub(crate) constants: HashMap<Word, StoredConstant>,
}

impl ConstantContext {
    /// Walk OpConstant* in declaration order, applying no
    /// spec-constant overrides (each `OpSpecConstant*` keeps
    /// its SPIR-V-declared default).  Convenience wrapper
    /// around [`Self::build_with_spec_overrides`].
    pub fn build(
        module: &Module,
        types: &TypeContext,
    ) -> Result<Self, FrontendError> {
        Self::build_with_spec_overrides(
            module, types, &std::collections::HashMap::new())
    }

    /// Walk OpConstant* / OpSpecConstant* in declaration
    /// order.  `spec_overrides` maps an `OpSpecConstant*`
    /// result id to its host-supplied 32-bit override value
    /// (the value's bit-encoding for f32; non-zero=`true` for
    /// bool).  Missing entries fall through to the
    /// SPIR-V-declared default.
    pub fn build_with_spec_overrides(
        module: &Module,
        types: &TypeContext,
        spec_overrides: &std::collections::HashMap<rspirv::spirv::Word, u32>,
    ) -> Result<Self, FrontendError> {
        let mut ctx = ConstantContext::default();
        for inst in &module.types_global_values {
            let id = match inst.result_id { Some(id) => id, None => continue };
            // Apply spec-constant override if present: rewrite
            // the OpSpecConstant{,True,False}'s payload bytes
            // to the host-supplied value, then fall through to
            // the regular constant decoder.  Composites carry
            // no SpecId of their own — only their scalar
            // operands do, so they need no special handling.
            let mut inst_owned: Option<rspirv::dr::Instruction>;
            let inst: &rspirv::dr::Instruction =
                if let Some(value) = spec_overrides.get(&id).copied() {
                    inst_owned = Some(inst.clone());
                    let cloned = inst_owned.as_mut().unwrap();
                    use rspirv::dr::Operand;
                    use rspirv::spirv::Op as SpvOp;
                    match cloned.class.opcode {
                        SpvOp::SpecConstant => {
                            // Replace the first literal operand.
                            if let Some(slot) = cloned.operands.first_mut() {
                                *slot = Operand::LiteralBit32(value);
                            }
                            // Re-tag as a plain Constant so the
                            // dispatch below decodes it.
                            cloned.class = rspirv::grammar::INSTRUCTION_TABLE.get(
                                SpvOp::Constant);
                        }
                        SpvOp::SpecConstantTrue | SpvOp::SpecConstantFalse => {
                            cloned.class = rspirv::grammar::INSTRUCTION_TABLE.get(
                                if value != 0 { SpvOp::ConstantTrue }
                                else { SpvOp::ConstantFalse });
                        }
                        _ => {}
                    }
                    cloned
                } else {
                    inst
                };
            let constant = match inst.class.opcode {
                // OpSpecConstant{,True,False,Composite} share
                // the exact wire encoding of their OpConstant
                // counterparts -- the only difference is that
                // a Vulkan API caller MAY override the value
                // via VkSpecializationInfo at pipeline-create
                // time.  Tier-2 v1 uses the SPIR-V-declared
                // default value (no VkSpecializationInfo
                // plumbing yet); this covers the common case
                // where the shader's compile-time default
                // matches the host's intent.  Folded here so
                // any later operand resolution treats the
                // spec constant as a regular constant.
                SpvOp::Constant | SpvOp::SpecConstant =>
                    translate_op_constant(inst, types)?,
                SpvOp::ConstantTrue | SpvOp::SpecConstantTrue => StoredConstant {
                    type_id: inst.result_type.ok_or_else(|| FrontendError::Malformed(
                        "ConstantTrue without result type".to_string()))?,
                    kind: ConstantKind::Scalar(Op::ConstInt {
                        value: 1, kind: IntKind::I32,
                    }),
                },
                SpvOp::ConstantFalse | SpvOp::SpecConstantFalse => StoredConstant {
                    type_id: inst.result_type.ok_or_else(|| FrontendError::Malformed(
                        "ConstantFalse without result type".to_string()))?,
                    kind: ConstantKind::Scalar(Op::ConstInt {
                        value: 0, kind: IntKind::I32,
                    }),
                },
                SpvOp::ConstantNull => StoredConstant {
                    type_id: inst.result_type.ok_or_else(|| FrontendError::Malformed(
                        "ConstantNull without result type".to_string()))?,
                    kind: ConstantKind::Null,
                },
                SpvOp::ConstantComposite | SpvOp::SpecConstantComposite =>
                    translate_constant_composite(inst, &ctx)?,
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

    /// Iterate over every (id, stored-constant) pair.
    /// Used by multi-block translation to pre-materialise
    /// every constant in the entry block so SSA references
    /// from any subsequent block stay dominated.
    pub fn iter(&self) -> impl Iterator<Item = (&Word, &StoredConstant)> {
        self.constants.iter()
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
                kind: ConstantKind::Scalar(Op::ConstInt { value, kind }),
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
                kind: ConstantKind::Scalar(Op::ConstInt { value, kind }),
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
                kind: ConstantKind::Scalar(Op::ConstFloat {
                    value: f32::from_bits(bits) as f64,
                    kind: FloatKind::F32,
                }),
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
                kind: ConstantKind::Scalar(Op::ConstFloat {
                    value: f64::from_bits(bits),
                    kind: FloatKind::F64,
                }),
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
    Ok(StoredConstant {
        type_id,
        kind: ConstantKind::Composite(element_ids),
    })
}
