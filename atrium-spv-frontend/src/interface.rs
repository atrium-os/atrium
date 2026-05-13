//! Entry-point + interface discovery.
//!
//! Walks `OpEntryPoint`, `OpDecorate`, and `OpVariable`
//! instructions to populate the module-level interface
//! fields: entry points + uniforms + push-constants +
//! per-stage I/O variables.
//!
//! Phase 1 v1 handles only the narrow case the v0c
//! interpreter exercises: a single fragment entry-point
//! with one Output variable. Wider discovery (descriptor
//! sets, per-binding decorations, vertex attribute
//! locations, etc.) lands as needed.

use std::collections::HashMap;

use atrium_spv_ir::{EntryPoint, ShaderStage, UniformBinding, Varying, VertexInput};
use rspirv::dr::{Module, Operand};
use rspirv::spirv::{Decoration, ExecutionModel, Op as SpvOp, StorageClass as SpvStorageClass, Word};

use crate::error::FrontendError;
use crate::types::TypeContext;

/// Discovered interface for the module.
#[derive(Debug, Default)]
pub struct InterfaceContext {
    /// Discovered entry points.
    pub entry_points: Vec<EntryPoint>,
    /// Per-entry-point: function id (SPIR-V) → index into
    /// the `entry_points` vector. Lets later passes find
    /// "which entry point is this function?" cheaply.
    pub entry_function_ids: HashMap<Word, usize>,
    /// Uniform bindings.
    pub uniforms: Vec<UniformBinding>,
    /// Total push-constants size in bytes (computed from
    /// the PushConstant variable's struct type, or 0 if
    /// none).
    pub push_constants_size: u32,
    /// Vertex-stage inputs.
    pub vertex_inputs: Vec<VertexInput>,
    /// Inter-stage varyings.
    pub varyings: Vec<Varying>,
    /// SPIR-V Variable id → (storage class, pointee type
    /// id). Used by function translation to figure out
    /// what each `OpStore`/`OpLoad` is touching.
    pub variables: HashMap<Word, (SpvStorageClass, Word)>,
}

impl InterfaceContext {
    /// Build by walking the module.
    pub fn build(
        module: &Module,
        types: &TypeContext,
    ) -> Result<Self, FrontendError> {
        let mut ctx = InterfaceContext::default();

        // Entry points.
        for inst in &module.entry_points {
            if inst.class.opcode != SpvOp::EntryPoint { continue; }
            let stage = read_execution_model(&inst.operands, 0)?;
            let fn_id = read_id_ref(&inst.operands, 1)?;
            let name = read_string(&inst.operands, 2)?;
            let stage = translate_stage(stage)?;
            let entry = EntryPoint {
                stage,
                function_index: 0, // patched up after functions translate
                name: name.clone(),
            };
            ctx.entry_function_ids.insert(fn_id, ctx.entry_points.len());
            ctx.entry_points.push(entry);
        }

        // Variables. Walk types_global_values for
        // OpVariable.
        for inst in &module.types_global_values {
            if inst.class.opcode != SpvOp::Variable { continue; }
            let var_id = inst.result_id.ok_or_else(|| FrontendError::Malformed(
                "OpVariable without result id".to_string()))?;
            let storage = match inst.operands.first() {
                Some(Operand::StorageClass(sc)) => *sc,
                other => return Err(FrontendError::Malformed(format!(
                    "OpVariable: expected StorageClass, got {other:?}",
                ))),
            };
            // The variable's result type is a pointer type
            // whose pointee is the actual data shape.
            let ptr_ty_id = inst.result_type.ok_or_else(|| FrontendError::Malformed(
                "OpVariable without result type".to_string()))?;
            let ptr_inst = types.get_raw(ptr_ty_id)?;
            if ptr_inst.class.opcode != SpvOp::TypePointer {
                return Err(FrontendError::Malformed(format!(
                    "OpVariable's result type {ptr_ty_id} is not a pointer",
                )));
            }
            // pointer pointee is operand 1 of TypePointer
            let pointee_id = match ptr_inst.operands.get(1) {
                Some(Operand::IdRef(id)) => *id,
                other => return Err(FrontendError::Malformed(format!(
                    "TypePointer pointee operand: {other:?}",
                ))),
            };
            ctx.variables.insert(var_id, (storage, pointee_id));
        }

        // Per-binding decorations (set / binding /
        // location). Phase 1 v1: we collect what's there
        // but only the structures below are populated when
        // the matching variable + type info is available.
        let mut decorations: HashMap<Word, VarDecorations> = HashMap::new();
        for inst in &module.annotations {
            if inst.class.opcode != SpvOp::Decorate { continue; }
            let target = read_id_ref(&inst.operands, 0)?;
            let kind = match inst.operands.get(1) {
                Some(Operand::Decoration(d)) => *d,
                _ => continue,
            };
            let d = decorations.entry(target).or_default();
            match kind {
                Decoration::DescriptorSet => {
                    if let Some(Operand::LiteralBit32(v)) = inst.operands.get(2) {
                        d.descriptor_set = Some(*v);
                    }
                }
                Decoration::Binding => {
                    if let Some(Operand::LiteralBit32(v)) = inst.operands.get(2) {
                        d.binding = Some(*v);
                    }
                }
                Decoration::Location => {
                    if let Some(Operand::LiteralBit32(v)) = inst.operands.get(2) {
                        d.location = Some(*v);
                    }
                }
                _ => {}
            }
        }

        // Cross-reference variables with their decorations
        // to populate uniforms / vertex_inputs / varyings.
        // Phase 1 v1 is minimal here — full interface
        // discovery (descriptor-set flattening, struct
        // member walking) lands when the first shader
        // needing it shows up.
        for (var_id, (storage, pointee_id)) in &ctx.variables {
            let deco = decorations.get(var_id);
            let pointee_ty = types.get(*pointee_id).ok().cloned();
            match storage {
                SpvStorageClass::Uniform | SpvStorageClass::UniformConstant => {
                    if let Some(d) = deco {
                        if let (Some(set), Some(binding), Some(ty)) =
                            (d.descriptor_set, d.binding, pointee_ty)
                        {
                            ctx.uniforms.push(UniformBinding {
                                set, binding, offset: 0, ty,
                            });
                        }
                    }
                }
                SpvStorageClass::Input => {
                    if let Some(d) = deco {
                        if let (Some(location), Some(ty)) = (d.location, pointee_ty) {
                            // For phase 1 v1 we treat any
                            // Input variable as a vertex
                            // attribute (the only stage
                            // that uses Input today).
                            ctx.vertex_inputs.push(VertexInput {
                                location, offset: 0, ty,
                            });
                        }
                    }
                }
                SpvStorageClass::Output => {
                    if let Some(d) = deco {
                        if let (Some(location), Some(ty)) = (d.location, pointee_ty) {
                            ctx.varyings.push(Varying {
                                location, offset: 0, ty,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(ctx)
    }
}

#[derive(Debug, Default, Clone)]
struct VarDecorations {
    descriptor_set: Option<u32>,
    binding: Option<u32>,
    location: Option<u32>,
}

fn read_execution_model(operands: &[Operand], i: usize) -> Result<ExecutionModel, FrontendError> {
    match operands.get(i) {
        Some(Operand::ExecutionModel(m)) => Ok(*m),
        other => Err(FrontendError::Malformed(format!(
            "expected ExecutionModel at operand {i}, got {other:?}",
        ))),
    }
}

fn read_id_ref(operands: &[Operand], i: usize) -> Result<Word, FrontendError> {
    match operands.get(i) {
        Some(Operand::IdRef(id)) => Ok(*id),
        other => Err(FrontendError::Malformed(format!(
            "expected IdRef at operand {i}, got {other:?}",
        ))),
    }
}

fn read_string(operands: &[Operand], i: usize) -> Result<String, FrontendError> {
    match operands.get(i) {
        Some(Operand::LiteralString(s)) => Ok(s.clone()),
        other => Err(FrontendError::Malformed(format!(
            "expected LiteralString at operand {i}, got {other:?}",
        ))),
    }
}

fn translate_stage(m: ExecutionModel) -> Result<ShaderStage, FrontendError> {
    match m {
        ExecutionModel::Vertex   => Ok(ShaderStage::Vertex),
        ExecutionModel::Fragment => Ok(ShaderStage::Fragment),
        ExecutionModel::GLCompute => Ok(ShaderStage::Compute),
        other => Err(FrontendError::Unsupported(format!(
            "execution model {other:?} not supported",
        ))),
    }
}
