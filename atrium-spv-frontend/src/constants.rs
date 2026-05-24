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
                // OpSpecConstantOp: constant expression on
                // previously-resolved spec/regular constants.
                // Operand layout: <opcode-literal> <id>...
                // Evaluate at compile time against ctx; the
                // result enters ctx as if it were an OpConstant.
                SpvOp::SpecConstantOp => translate_spec_constant_op(
                    inst, types, &ctx)?,
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

/// Evaluate `OpSpecConstantOp` at compile time.
///
/// SPIR-V operand layout:
///   <result_type> <result_id> <sub_opcode:LiteralBit32>
///   <operand_id> [<operand_id>...]
///
/// The sub_opcode literal names another SPIR-V opcode whose
/// operands must already be constants (regular or spec).  We
/// look each operand up in `ctx.constants`, evaluate the
/// expression in Rust, and return the resulting StoredConstant.
///
/// Supported sub-opcodes (the set glslang typically emits for
/// arithmetic on spec constants):
///   IAdd, ISub, IMul, SDiv, UDiv, SMod, UMod, SNegate
///   BitwiseAnd, BitwiseOr, BitwiseXor
///   ShiftLeftLogical, ShiftRightLogical, ShiftRightArithmetic
///   IEqual, INotEqual,
///   SLessThan, ULessThan, SLessThanEqual, ULessThanEqual,
///   SGreaterThan, UGreaterThan, SGreaterThanEqual,
///   UGreaterThanEqual
///   LogicalNot, LogicalAnd, LogicalOr, LogicalEqual,
///   LogicalNotEqual
///   Select
fn translate_spec_constant_op(
    inst: &Instruction,
    types: &TypeContext,
    ctx: &ConstantContext,
) -> Result<StoredConstant, FrontendError> {
    let type_id = inst.result_type.ok_or_else(|| FrontendError::Malformed(
        "SpecConstantOp without result type".to_string()))?;
    let result_ty = types.get(type_id)?;
    let sub_op = match inst.operands.first() {
        Some(Operand::LiteralSpecConstantOpInteger(op)) => *op,
        Some(Operand::LiteralBit32(v)) => {
            use rspirv::spirv::Op as SpvOp;
            // Map u32 -> SpvOp.  Only the opcodes we evaluate
            // need to round-trip; unknown ones fall through to
            // the Unsupported error below.
            match *v {
                128 => SpvOp::IAdd,
                130 => SpvOp::ISub,
                132 => SpvOp::IMul,
                134 => SpvOp::SDiv,
                133 => SpvOp::UDiv,
                139 => SpvOp::SMod,
                137 => SpvOp::UMod,
                126 => SpvOp::SNegate,
                199 => SpvOp::BitwiseAnd,
                197 => SpvOp::BitwiseOr,
                198 => SpvOp::BitwiseXor,
                196 => SpvOp::ShiftLeftLogical,
                195 => SpvOp::ShiftRightLogical,
                194 => SpvOp::ShiftRightArithmetic,
                170 => SpvOp::IEqual,
                171 => SpvOp::INotEqual,
                177 => SpvOp::SLessThan,
                176 => SpvOp::ULessThan,
                179 => SpvOp::SLessThanEqual,
                178 => SpvOp::ULessThanEqual,
                173 => SpvOp::SGreaterThan,
                172 => SpvOp::UGreaterThan,
                175 => SpvOp::SGreaterThanEqual,
                174 => SpvOp::UGreaterThanEqual,
                // Arc 56: per SPIR-V spec the Logical/Select
                // block runs 164..169.  Previous mapping had
                // 167/168/169 shifted by one (LogicalEqual /
                // LogicalAnd / LogicalNot), which made spec-
                // const OpLogicalAnd evaluate as LogicalEqual
                // ((a!=0) == (b!=0)) -- diverges from true AND
                // semantics on (false, false): AND returns 0,
                // Equal returns 1.  Mapping corrected; Select
                // (169) now reachable.
                164 => SpvOp::LogicalEqual,
                165 => SpvOp::LogicalNotEqual,
                166 => SpvOp::LogicalOr,
                167 => SpvOp::LogicalAnd,
                168 => SpvOp::LogicalNot,
                169 => SpvOp::Select,
                _ => return Err(FrontendError::Unsupported(format!(
                    "SpecConstantOp sub-opcode literal {v} not recognised"))),
            }
        }
        other => return Err(FrontendError::Malformed(format!(
            "SpecConstantOp expected opcode literal, got {other:?}"))),
    };
    // Pull operand id refs (everything after the opcode literal).
    let mut operand_ids: Vec<Word> = Vec::with_capacity(inst.operands.len());
    for op in inst.operands.iter().skip(1) {
        if let Operand::IdRef(id) = op {
            operand_ids.push(*id);
        }
    }
    let load_i64 = |id: Word| -> Result<i64, FrontendError> {
        let sc = ctx.constants.get(&id).ok_or_else(||
            FrontendError::Malformed(format!(
                "SpecConstantOp operand {id} not a resolved constant")))?;
        match &sc.kind {
            ConstantKind::Scalar(Op::ConstInt { value, .. }) => Ok(*value),
            other => Err(FrontendError::Unsupported(format!(
                "SpecConstantOp expected integer operand, got {other:?}"))),
        }
    };
    let kind_from_ty = |ty: &Type| match ty {
        Type::I32 | Type::Bool => IntKind::I32,
        Type::U32 => IntKind::U32,
        Type::I64 => IntKind::I64,
        Type::U64 => IntKind::U64,
        _ => IntKind::I32,
    };
    use rspirv::spirv::Op as SpvOp;
    let kind = kind_from_ty(&result_ty);
    // Helper to wrap a computed i64 into a StoredConstant.
    let wrap = |v: i64, k: IntKind| StoredConstant {
        type_id,
        kind: ConstantKind::Scalar(Op::ConstInt { value: v, kind: k }),
    };
    // Binary integer ops: load two operands then evaluate.
    let bin_int = |f: &dyn Fn(i64, i64) -> i64|
        -> Result<StoredConstant, FrontendError>
    {
        if operand_ids.len() < 2 {
            return Err(FrontendError::Malformed(format!(
                "SpecConstantOp {sub_op:?} needs ≥2 operands, got {}",
                operand_ids.len())));
        }
        let a = load_i64(operand_ids[0])?;
        let b = load_i64(operand_ids[1])?;
        Ok(wrap(f(a, b), kind))
    };
    // Comparison: produce bool (StoredConstant carries i32 1/0).
    let cmp = |f: &dyn Fn(i64, i64) -> bool|
        -> Result<StoredConstant, FrontendError>
    {
        if operand_ids.len() < 2 {
            return Err(FrontendError::Malformed(format!(
                "SpecConstantOp {sub_op:?} needs ≥2 operands, got {}",
                operand_ids.len())));
        }
        let a = load_i64(operand_ids[0])?;
        let b = load_i64(operand_ids[1])?;
        Ok(wrap(if f(a, b) { 1 } else { 0 }, IntKind::I32))
    };
    let result = match sub_op {
        SpvOp::IAdd => bin_int(&|a, b| (a as i32).wrapping_add(b as i32) as i64)?,
        SpvOp::ISub => bin_int(&|a, b| (a as i32).wrapping_sub(b as i32) as i64)?,
        SpvOp::IMul => bin_int(&|a, b| (a as i32).wrapping_mul(b as i32) as i64)?,
        SpvOp::SDiv => bin_int(&|a, b|
            if b == 0 { 0 } else { (a as i32).wrapping_div(b as i32) as i64 })?,
        SpvOp::UDiv => bin_int(&|a, b|
            if b == 0 { 0 } else { ((a as u32) / (b as u32)) as i64 })?,
        SpvOp::SMod => bin_int(&|a, b|
            if b == 0 { 0 } else { (a as i32).wrapping_rem(b as i32) as i64 })?,
        SpvOp::UMod => bin_int(&|a, b|
            if b == 0 { 0 } else { ((a as u32) % (b as u32)) as i64 })?,
        SpvOp::SNegate => {
            let a = load_i64(operand_ids[0])?;
            wrap((a as i32).wrapping_neg() as i64, kind)
        }
        SpvOp::BitwiseAnd => bin_int(&|a, b| (a as u32 & b as u32) as i64)?,
        SpvOp::BitwiseOr  => bin_int(&|a, b| (a as u32 | b as u32) as i64)?,
        SpvOp::BitwiseXor => bin_int(&|a, b| (a as u32 ^ b as u32) as i64)?,
        SpvOp::ShiftLeftLogical => bin_int(&|a, b|
            ((a as u32).wrapping_shl((b as u32) & 31)) as i64)?,
        SpvOp::ShiftRightLogical => bin_int(&|a, b|
            ((a as u32).wrapping_shr((b as u32) & 31)) as i64)?,
        SpvOp::ShiftRightArithmetic => bin_int(&|a, b|
            ((a as i32).wrapping_shr((b as u32) & 31)) as i64)?,
        SpvOp::IEqual    => cmp(&|a, b| (a as u32) == (b as u32))?,
        SpvOp::INotEqual => cmp(&|a, b| (a as u32) != (b as u32))?,
        SpvOp::SLessThan        => cmp(&|a, b| (a as i32) <  (b as i32))?,
        SpvOp::SLessThanEqual   => cmp(&|a, b| (a as i32) <= (b as i32))?,
        SpvOp::SGreaterThan     => cmp(&|a, b| (a as i32) >  (b as i32))?,
        SpvOp::SGreaterThanEqual=> cmp(&|a, b| (a as i32) >= (b as i32))?,
        SpvOp::ULessThan        => cmp(&|a, b| (a as u32) <  (b as u32))?,
        SpvOp::ULessThanEqual   => cmp(&|a, b| (a as u32) <= (b as u32))?,
        SpvOp::UGreaterThan     => cmp(&|a, b| (a as u32) >  (b as u32))?,
        SpvOp::UGreaterThanEqual=> cmp(&|a, b| (a as u32) >= (b as u32))?,
        SpvOp::LogicalAnd => cmp(&|a, b| (a != 0) && (b != 0))?,
        SpvOp::LogicalOr  => cmp(&|a, b| (a != 0) || (b != 0))?,
        SpvOp::LogicalEqual    => cmp(&|a, b| (a != 0) == (b != 0))?,
        SpvOp::LogicalNotEqual => cmp(&|a, b| (a != 0) != (b != 0))?,
        SpvOp::LogicalNot => {
            let a = load_i64(operand_ids[0])?;
            wrap(if a == 0 { 1 } else { 0 }, IntKind::I32)
        }
        SpvOp::Select => {
            if operand_ids.len() < 3 {
                return Err(FrontendError::Malformed(
                    "SpecConstantOp Select needs 3 operands".into()));
            }
            let cond = load_i64(operand_ids[0])?;
            let pick = if cond != 0 {
                operand_ids[1]
            } else {
                operand_ids[2]
            };
            // Clone the picked constant under our result type.
            let sc = ctx.constants.get(&pick).ok_or_else(||
                FrontendError::Malformed(format!(
                    "SpecConstantOp Select operand {pick} not resolved")))?;
            StoredConstant { type_id, kind: sc.kind.clone() }
        }
        other => return Err(FrontendError::Unsupported(format!(
            "SpecConstantOp sub-opcode {other:?} not supported"))),
    };
    Ok(result)
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
