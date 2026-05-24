//! SPIR-V type ids → [`atrium_spv_ir::Type`].
//!
//! Walks `OpType*` instructions in declaration order and
//! builds an `id -> Type` map. Subsequent passes look up
//! types by SPIR-V id.

use std::collections::HashMap;

use atrium_spv_ir::{ImageDimensionality, StorageClass, Type, VecElement};
use rspirv::dr::{Instruction, Module, Operand};
use rspirv::spirv::{Op, StorageClass as SpvStorageClass, Word};

use crate::error::FrontendError;

/// Map from SPIR-V type id → translated IR type.
///
/// Built once at translate-time; consumed by every later
/// pass.
#[derive(Debug, Default)]
pub struct TypeContext {
    pub(crate) types: HashMap<Word, Type>,
    /// Raw `OpType*` instruction for each id; needed by
    /// the interface pass to read structure members,
    /// pointer pointee types, etc., without re-walking the
    /// module.
    pub(crate) raw: HashMap<Word, Instruction>,
}

impl TypeContext {
    /// Walk the module's types and build the map.
    pub fn build(module: &Module) -> Result<Self, FrontendError> {
        let mut ctx = TypeContext::default();
        for inst in &module.types_global_values {
            // OpType* opcodes always have a result id.
            let id = match inst.result_id { Some(id) => id, None => continue };
            // Only handle type opcodes here; constants and
            // variables are processed by other passes.
            match inst.class.opcode {
                Op::TypeVoid | Op::TypeBool | Op::TypeInt | Op::TypeFloat
                | Op::TypeVector | Op::TypeMatrix | Op::TypePointer
                | Op::TypeFunction
                | Op::TypeImage | Op::TypeSampler | Op::TypeSampledImage => {
                    ctx.raw.insert(id, inst.clone());
                    let ty = ctx.translate_inst(inst)?;
                    ctx.types.insert(id, ty);
                }
                // TypeStruct / TypeArray have no leaf IR
                // Type (pointers to them get resolved by
                // OpAccessChain in the frontend, before
                // they reach the backend). We still need
                // the raw instruction so the interface
                // pass can walk struct members.
                Op::TypeStruct | Op::TypeArray | Op::TypeRuntimeArray => {
                    ctx.raw.insert(id, inst.clone());
                }
                _ => {}
            }
        }
        Ok(ctx)
    }

    /// Look up a type by SPIR-V id.
    pub fn get(&self, id: Word) -> Result<&Type, FrontendError> {
        self.types.get(&id).ok_or_else(|| FrontendError::Malformed(
            format!("type id {id} not found"),
        ))
    }

    /// Look up the raw `OpType*` instruction by SPIR-V id.
    ///
    /// Used by the interface pass to walk pointer pointee
    /// types and struct members.
    pub fn get_raw(&self, id: Word) -> Result<&Instruction, FrontendError> {
        self.raw.get(&id).ok_or_else(|| FrontendError::Malformed(
            format!("type id {id} not found (raw)"),
        ))
    }

    fn translate_inst(&self, inst: &Instruction) -> Result<Type, FrontendError> {
        match inst.class.opcode {
            Op::TypeVoid => Ok(Type::Void),
            Op::TypeBool => Ok(Type::Bool),
            Op::TypeInt => {
                let width = read_lit(&inst.operands, 0)?;
                let signed = read_lit(&inst.operands, 1)? != 0;
                match (width, signed) {
                    (32, false) => Ok(Type::U32),
                    (32, true)  => Ok(Type::I32),
                    (64, false) => Ok(Type::U64),
                    (64, true)  => Ok(Type::I64),
                    other => Err(FrontendError::Unsupported(format!(
                        "OpTypeInt width={}, signed={} not supported",
                        other.0, other.1,
                    ))),
                }
            }
            Op::TypeFloat => {
                let width = read_lit(&inst.operands, 0)?;
                match width {
                    32 => Ok(Type::F32),
                    64 => Ok(Type::F64),
                    other => Err(FrontendError::Unsupported(format!(
                        "OpTypeFloat width={other} not supported (need 32 or 64)",
                    ))),
                }
            }
            Op::TypeVector => {
                let elem_id = read_id(&inst.operands, 0)?;
                let count = read_lit(&inst.operands, 1)?;
                let elem = self.types.get(&elem_id).ok_or_else(||
                    FrontendError::Malformed(format!(
                        "TypeVector references unknown element type {elem_id}",
                    )))?;
                let elem_kind = match elem {
                    Type::F32 => VecElement::F32,
                    Type::F64 => VecElement::F64,
                    Type::I32 => VecElement::I32,
                    Type::U32 => VecElement::U32,
                    // Arc 46: bvec<N> aliased to uvec<N>.
                    // Bools are i32-backed (0 or 1) per
                    // constraint B4, so the bit-level layout
                    // is identical and Any/All/LogicalAnd/Or
                    // can fold them with the existing
                    // bitwise ops.
                    Type::Bool => VecElement::U32,
                    other => return Err(FrontendError::Unsupported(format!(
                        "vector of {other:?} not supported",
                    ))),
                };
                match count {
                    2 => Ok(Type::Vec2(elem_kind)),
                    3 => Ok(Type::Vec3(elem_kind)),
                    4 => Ok(Type::Vec4(elem_kind)),
                    other => Err(FrontendError::Unsupported(format!(
                        "vector length {other} not supported (need 2/3/4)",
                    ))),
                }
            }
            Op::TypeMatrix => {
                // OpTypeMatrix column_type column_count.
                // v1 supports only 4-column f32 matrices
                // (mat4) since that's the canonical MVP /
                // model-view-projection shape; mat2/mat3
                // need their own type variants when a real
                // shader demands them.
                let column_type_id = read_id(&inst.operands, 0)?;
                let column_count = read_lit(&inst.operands, 1)?;
                let col_ty = self.types.get(&column_type_id).ok_or_else(||
                    FrontendError::Malformed(format!(
                        "TypeMatrix column type {column_type_id} unknown")))?;
                let elem = match col_ty {
                    Type::Vec4(VecElement::F32) => VecElement::F32,
                    other => return Err(FrontendError::Unsupported(format!(
                        "TypeMatrix column type {other:?} not supported \
                         (v1: vec4<f32> only)"))),
                };
                if column_count != 4 {
                    return Err(FrontendError::Unsupported(format!(
                        "TypeMatrix with {column_count} columns not supported \
                         (v1: 4-column only)")));
                }
                Ok(Type::Mat4(elem))
            }
            Op::TypePointer => {
                let storage = read_storage(&inst.operands, 0)?;
                let pointee_id = read_id(&inst.operands, 1)?;
                // The pointee might be an aggregate (Struct
                // / Array) that has no IR Type; we still
                // need a Pointer with *some* pointee for
                // type tracking through Store/AccessChain.
                // Fall back to Void — leaf pointers (used by
                // OpLoad) always go through OpAccessChain,
                // which builds a fresh Pointer with the
                // resolved leaf type, so this placeholder
                // never reaches the backend's load path.
                let pointee = self.types.get(&pointee_id)
                    .cloned()
                    .unwrap_or(Type::Void);
                Ok(Type::Pointer(translate_storage(storage)?, Box::new(pointee)))
            }
            Op::TypeFunction => {
                // Function types aren't represented in
                // atrium-spv-ir as a Type variant — the
                // function's signature lives on the
                // Function struct itself. We never look up
                // a function-type via TypeContext::get(),
                // only via get_raw() during function
                // translation. Return a Void placeholder.
                Ok(Type::Void)
            }
            Op::TypeImage => {
                // OpTypeImage operands: SampledType, Dim,
                // Depth, Arrayed, MS, Sampled, Format,
                // [AccessQualifier]. We pull the Dim.
                let dim_op = inst.operands.get(1)
                    .ok_or_else(|| FrontendError::Malformed(
                        "TypeImage missing Dim operand".to_string()))?;
                let dim = match dim_op {
                    Operand::Dim(d) => *d,
                    _ => return Err(FrontendError::Malformed(
                        "TypeImage operand 1 is not Dim".to_string())),
                };
                Ok(Type::Image(translate_dim(dim)?))
            }
            Op::TypeSampler => Ok(Type::Sampler),
            Op::TypeSampledImage => {
                // SampledImage operand 0 is the underlying image type id.
                let img_id = read_id(&inst.operands, 0)?;
                let img = self.types.get(&img_id).ok_or_else(||
                    FrontendError::Malformed(format!(
                        "TypeSampledImage references unknown image type {img_id}",
                    )))?;
                let dim = match img {
                    Type::Image(d) => *d,
                    other => return Err(FrontendError::Malformed(format!(
                        "TypeSampledImage's image is not an Image type: {other:?}",
                    ))),
                };
                Ok(Type::SampledImage(dim))
            }
            other => Err(FrontendError::Internal(format!(
                "translate_inst called on non-type opcode {other:?}",
            ))),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn read_id(operands: &[Operand], i: usize) -> Result<Word, FrontendError> {
    match operands.get(i) {
        Some(Operand::IdRef(id)) => Ok(*id),
        Some(other) => Err(FrontendError::Malformed(format!(
            "expected IdRef at operand {i}, got {other:?}",
        ))),
        None => Err(FrontendError::Malformed(format!(
            "missing operand {i}",
        ))),
    }
}

fn read_lit(operands: &[Operand], i: usize) -> Result<u32, FrontendError> {
    match operands.get(i) {
        Some(Operand::LiteralBit32(v)) => Ok(*v),
        Some(other) => Err(FrontendError::Malformed(format!(
            "expected LiteralBit32 at operand {i}, got {other:?}",
        ))),
        None => Err(FrontendError::Malformed(format!(
            "missing operand {i}",
        ))),
    }
}

fn read_storage(operands: &[Operand], i: usize) -> Result<SpvStorageClass, FrontendError> {
    match operands.get(i) {
        Some(Operand::StorageClass(sc)) => Ok(*sc),
        Some(other) => Err(FrontendError::Malformed(format!(
            "expected StorageClass at operand {i}, got {other:?}",
        ))),
        None => Err(FrontendError::Malformed(format!(
            "missing operand {i}",
        ))),
    }
}

/// SPIR-V `StorageClass` → IR `StorageClass`.
pub(crate) fn translate_storage(sc: SpvStorageClass) -> Result<StorageClass, FrontendError> {
    match sc {
        SpvStorageClass::Input         => Ok(StorageClass::Input),
        SpvStorageClass::Output        => Ok(StorageClass::Output),
        SpvStorageClass::Uniform       => Ok(StorageClass::Uniform),
        SpvStorageClass::UniformConstant => Ok(StorageClass::UniformConstant),
        SpvStorageClass::StorageBuffer => Ok(StorageClass::StorageBuffer),
        SpvStorageClass::PushConstant  => Ok(StorageClass::PushConstant),
        SpvStorageClass::Function      => Ok(StorageClass::Function),
        SpvStorageClass::Private       => Ok(StorageClass::Private),
        SpvStorageClass::Workgroup     => Ok(StorageClass::Workgroup),
        SpvStorageClass::Image         => Ok(StorageClass::Image),
        other => Err(FrontendError::Unsupported(format!(
            "storage class {other:?} not supported",
        ))),
    }
}

fn translate_dim(d: rspirv::spirv::Dim) -> Result<ImageDimensionality, FrontendError> {
    use rspirv::spirv::Dim;
    match d {
        Dim::Dim1D     => Ok(ImageDimensionality::Dim1D),
        Dim::Dim2D     => Ok(ImageDimensionality::Dim2D),
        Dim::Dim3D     => Ok(ImageDimensionality::Dim3D),
        Dim::DimCube   => Ok(ImageDimensionality::Cube),
        Dim::DimRect   => Ok(ImageDimensionality::Rect),
        Dim::DimBuffer => Ok(ImageDimensionality::Buffer),
        other => Err(FrontendError::Unsupported(format!(
            "image dimensionality {other:?} not supported",
        ))),
    }
}
