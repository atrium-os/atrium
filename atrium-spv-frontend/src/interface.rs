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
    /// SPIR-V struct id → list of `(member_byte_offset,
    /// member_type_id)` in declaration order. Populated for
    /// every `OpTypeStruct` whose members carry an explicit
    /// `OpMemberDecorate Offset N` annotation (i.e. blocks
    /// in Uniform / PushConstant / StorageBuffer storage).
    /// `OpAccessChain` translation consults this map to
    /// resolve member indices to constant byte offsets per
    /// constraint B5.
    pub struct_layouts: HashMap<Word, Vec<StructMember>>,
    /// SPIR-V variable id of the (singleton) push-constant
    /// block, if any. SPIR-V allows at most one such
    /// variable per entry point.
    pub push_constant_var: Option<Word>,
    /// Workgroup-storage variables in this module.  Each maps
    /// to its byte offset within the per-workgroup scratch
    /// buffer that the dispatcher allocates and passes via the
    /// `workgroup_buf` ABI slot.  Offsets are assigned in
    /// declaration order, packed (no padding beyond the
    /// natural alignment of each type's size).
    pub workgroup_var_offset: HashMap<Word, u32>,
    /// Total size of the per-workgroup scratch buffer in
    /// bytes (sum of all workgroup-storage variables).
    pub workgroup_size: u32,
    /// SPIR-V variable id → `(set, binding)` from `OpDecorate`
    /// `DescriptorSet`/`Binding`. Function translation uses
    /// this for image/sampler `OpLoad` — those become
    /// `Op::ImageHandle { set, binding }` rather than a
    /// memory load, since descriptor-bound resources aren't
    /// loadable byte regions.
    pub var_binding: HashMap<Word, (u32, u32)>,
    /// Subset of `var_binding`: SPIR-V var ids whose storage
    /// class is exactly `StorageBuffer` (SSBOs).  Used by
    /// `Function::ssbo_bindings` to filter out
    /// `UniformConstant` entries (storage images, samplers)
    /// that share the descriptor-set/binding decoration but
    /// must NOT be treated as descriptor-table SSBO slots by
    /// the backends' multi-binding compute prologue.  Without
    /// this filter a shader with one SSBO + one storage image
    /// presents `ssbo_bindings.len() == 2` to the backend,
    /// which then misreads the SSBO pointer-param as a
    /// descriptor-table base and dereferences garbage.
    pub storage_buffer_vars: std::collections::HashSet<Word>,
    /// SPIR-V variable id → byte offset within the VS's
    /// `out_varyings` buffer for Location-decorated
    /// `Output`-storage variables.  Vars are sorted by
    /// Location decoration; each gets `prefix_sum(byte_size)`
    /// as its offset.  Consumed by the backend's VS function
    /// translator to route `OpStore` through a `Variable` of
    /// this kind into `params[7] + offset` rather than into
    /// `params[6]` (out_position).  BuiltIn outputs
    /// (`gl_Position`) are NOT in this map -- they stay on
    /// the legacy out_position path through `builtin_vars`.
    pub output_varying_byte_offset: HashMap<Word, u32>,
    /// SPIR-V variable id → byte offset within the
    /// per-vertex / per-fragment `Input` buffer for
    /// Location-decorated `Input`-storage variables.  Same
    /// shape as `output_varying_byte_offset` but for the
    /// read side.  For VS this maps onto `in_attributes`
    /// (params[0]); for FS onto `in_varyings` (params[0]).
    /// Without this, the generic `(stage, Input) -> params[0]`
    /// rule sends every load to offset 0 -- a shader with
    /// two Inputs at Locations 0 and 1 would read both from
    /// the same bytes.
    pub input_varying_byte_offset: HashMap<Word, u32>,
    /// SPIR-V variable id → recognised stage built-in.  Set
    /// from `OpDecorate <var> BuiltIn <kind>`; consumed by
    /// function translation to lower an `OpLoad` through one
    /// of these variables into `Op::LoadBuiltin(kind)` rather
    /// than going through memory.
    pub builtin_vars: HashMap<Word, atrium_spv_ir::BuiltinKind>,
    /// Per-function `LocalSize` SPIR-V execution mode, if
    /// declared. Compute shaders set this via `OpExecutionMode
    /// %main LocalSize x y z`; other stages don't have it.
    /// The Function translator stamps this on each Function
    /// in IR so the backend can fold it into
    /// `gl_GlobalInvocationID` codegen.
    pub local_sizes: HashMap<Word, (u32, u32, u32)>,
    /// SPIR-V OpExtInstImport result ids that map to the
    /// GLSL.std.450 extended instruction set.  Used by the
    /// function translator to distinguish OpExtInst calls
    /// into GLSL.std.450 (which we handle) from other sets
    /// (which we reject).
    pub glsl_std_450_imports: std::collections::HashSet<Word>,
}

/// One member of an `OpTypeStruct` annotated with an
/// explicit byte offset.
#[derive(Debug, Clone)]
pub struct StructMember {
    /// `OpMemberDecorate Offset` literal.
    pub byte_offset: u32,
    /// SPIR-V id of the member's type (looked up via
    /// [`TypeContext::get`] to materialise an IR `Type`).
    pub type_id: Word,
}

impl InterfaceContext {
    /// Build by walking the module.
    pub fn build(
        module: &Module,
        types: &TypeContext,
    ) -> Result<Self, FrontendError> {
        Self::build_with_constants(module, types, None)
    }

    /// Same as [`build`] but with an optional `ConstantContext`
    /// so `OpExecutionModeId` can resolve `LocalSizeId` operand
    /// ids to literal values.  When `constants` is `None` the
    /// ExecutionModeId path is ignored (which matches the
    /// pre-Arc-53 behaviour).
    pub fn build_with_constants(
        module: &Module,
        types: &TypeContext,
        constants: Option<&crate::constants::ConstantContext>,
    ) -> Result<Self, FrontendError> {
        let mut ctx = InterfaceContext::default();
        // Locals: (spv_var_id, location, byte_size) for
        // Location-decorated Output / Input vars.  Sorted +
        // offset-assigned at end of the walk.
        let mut output_var_loc_size: Vec<(Word, u32, u32)> = Vec::new();
        let mut input_var_loc_size: Vec<(Word, u32, u32)> = Vec::new();

        // OpExtInstImport: record result_ids whose set name
        // is "GLSL.std.450" so the function translator can
        // dispatch OpExtInst calls against the right set.
        for inst in &module.ext_inst_imports {
            if inst.class.opcode != SpvOp::ExtInstImport { continue; }
            let Some(result_id) = inst.result_id else { continue };
            let name = match inst.operands.first() {
                Some(Operand::LiteralString(s)) => s.clone(),
                _ => continue,
            };
            if name == "GLSL.std.450" {
                ctx.glsl_std_450_imports.insert(result_id);
            }
        }

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

        // Execution modes -- pull LocalSize for compute
        // entry points so the backend can fold it into
        // gl_GlobalInvocationID codegen.
        //
        // Arc 53: also handle `OpExecutionModeId` with
        // `LocalSizeId` (the spec-constant variant).  Three
        // ID operands reference resolved constants; we look
        // each one up in the constants context and read the
        // i32 / u32 value out.  Without a constants context
        // we silently skip the ExecutionModeId path -- matches
        // the pre-Arc-53 behaviour.
        //
        // NOTE (Arc 78): the rspirv 0.13 loader currently
        // refuses modules containing OpExecutionModeId
        // (it's not in its dr::loader dispatch table -- only
        // OpExecutionMode is).  So this branch is defensive:
        // it lights up only if a future rspirv release admits
        // OpExecutionModeId into `module.execution_modes`.
        // A direct integration test would need to bypass
        // rspirv's loader, or wait for the upstream fix.
        for inst in &module.execution_modes {
            let fn_id = read_id_ref(&inst.operands, 0)?;
            let mode = match inst.operands.get(1) {
                Some(Operand::ExecutionMode(m)) => *m,
                _ => continue,
            };
            match (inst.class.opcode, mode) {
                (SpvOp::ExecutionMode, rspirv::spirv::ExecutionMode::LocalSize) => {
                    let x = match inst.operands.get(2) {
                        Some(Operand::LiteralBit32(v)) => *v,
                        _ => continue,
                    };
                    let y = match inst.operands.get(3) {
                        Some(Operand::LiteralBit32(v)) => *v,
                        _ => continue,
                    };
                    let z = match inst.operands.get(4) {
                        Some(Operand::LiteralBit32(v)) => *v,
                        _ => continue,
                    };
                    ctx.local_sizes.insert(fn_id, (x, y, z));
                }
                (SpvOp::ExecutionModeId,
                 rspirv::spirv::ExecutionMode::LocalSizeId) => {
                    let constants = match constants {
                        Some(c) => c,
                        None => continue,
                    };
                    let resolve = |idx: usize| -> Option<u32> {
                        let id = match inst.operands.get(idx) {
                            Some(Operand::IdRef(id)) => *id,
                            _ => return None,
                        };
                        let sc = constants.get(id)?;
                        if let crate::constants::ConstantKind::Scalar(
                            atrium_spv_ir::Op::ConstInt { value, .. }) = &sc.kind
                        {
                            Some(*value as u32)
                        } else {
                            None
                        }
                    };
                    let (x, y, z) = match (resolve(2), resolve(3), resolve(4)) {
                        (Some(x), Some(y), Some(z)) => (x, y, z),
                        _ => continue,
                    };
                    ctx.local_sizes.insert(fn_id, (x, y, z));
                }
                _ => {}
            }
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

        // Member decorations: collect `Offset` literals
        // for every OpTypeStruct. Used by AccessChain
        // resolution to flatten member indices → byte
        // offsets (constraint B5).
        let mut member_offsets: HashMap<(Word, u32), u32> = HashMap::new();
        for inst in &module.annotations {
            if inst.class.opcode != SpvOp::MemberDecorate { continue; }
            // Operands: struct_id, member_idx, Decoration, [literal].
            let struct_id = read_id_ref(&inst.operands, 0)?;
            let member_idx = match inst.operands.get(1) {
                Some(Operand::LiteralBit32(v)) => *v,
                _ => continue,
            };
            let kind = match inst.operands.get(2) {
                Some(Operand::Decoration(d)) => *d,
                _ => continue,
            };
            if kind == Decoration::Offset {
                if let Some(Operand::LiteralBit32(off)) = inst.operands.get(3) {
                    member_offsets.insert((struct_id, member_idx), *off);
                }
            }
        }

        // Now walk OpTypeStruct in the global type table
        // and build struct_layouts. We only retain structs
        // whose members all carry an explicit Offset.
        for inst in &module.types_global_values {
            if inst.class.opcode != SpvOp::TypeStruct { continue; }
            let Some(struct_id) = inst.result_id else { continue };
            let mut members = Vec::with_capacity(inst.operands.len());
            let mut all_have_offsets = true;
            for (idx, op) in inst.operands.iter().enumerate() {
                let type_id = match op {
                    Operand::IdRef(id) => *id,
                    _ => { all_have_offsets = false; break; }
                };
                match member_offsets.get(&(struct_id, idx as u32)) {
                    Some(off) => members.push(StructMember {
                        byte_offset: *off,
                        type_id,
                    }),
                    None => { all_have_offsets = false; break; }
                }
            }
            if all_have_offsets && !members.is_empty() {
                ctx.struct_layouts.insert(struct_id, members);
            }
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
                Decoration::BuiltIn => {
                    if let Some(Operand::BuiltIn(b)) = inst.operands.get(2) {
                        use rspirv::spirv::BuiltIn as SpvBuiltIn;
                        let mapped = match b {
                            SpvBuiltIn::WorkgroupId =>
                                Some(atrium_spv_ir::BuiltinKind::WorkgroupId),
                            SpvBuiltIn::LocalInvocationId =>
                                Some(atrium_spv_ir::BuiltinKind::LocalInvocationId),
                            SpvBuiltIn::GlobalInvocationId =>
                                Some(atrium_spv_ir::BuiltinKind::GlobalInvocationId),
                            SpvBuiltIn::LocalInvocationIndex =>
                                Some(atrium_spv_ir::BuiltinKind::LocalInvocationIndex),
                            SpvBuiltIn::WorkgroupSize =>
                                Some(atrium_spv_ir::BuiltinKind::WorkgroupSize),
                            SpvBuiltIn::VertexIndex =>
                                Some(atrium_spv_ir::BuiltinKind::VertexIndex),
                            SpvBuiltIn::InstanceIndex =>
                                Some(atrium_spv_ir::BuiltinKind::InstanceIndex),
                            SpvBuiltIn::FrontFacing =>
                                Some(atrium_spv_ir::BuiltinKind::FrontFacing),
                            SpvBuiltIn::PrimitiveId =>
                                Some(atrium_spv_ir::BuiltinKind::PrimitiveId),
                            // Other builtins (Position, FragCoord, etc.)
                            // already flow through the existing
                            // varying / output paths; skip.
                            _ => None,
                        };
                        if let Some(kind) = mapped { d.builtin = Some(kind); }
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
            // BuiltIn variables short-circuit: they're handled
            // by `Op::LoadBuiltin` at function translation, no
            // memory-binding interface entry needed.
            if let Some(d) = deco {
                if let Some(kind) = d.builtin {
                    ctx.builtin_vars.insert(*var_id, kind);
                    continue;
                }
            }
            match storage {
                SpvStorageClass::Uniform | SpvStorageClass::UniformConstant => {
                    if let Some(d) = deco {
                        if let (Some(set), Some(binding)) =
                            (d.descriptor_set, d.binding)
                        {
                            // UBO pointees are typically
                            // OpTypeStruct (no IR Type);
                            // record the binding anyway
                            // with Void as a placeholder.
                            // Member-level reads land via
                            // OpAccessChain, which carries
                            // the resolved leaf type.
                            let ty = pointee_ty.clone()
                                .unwrap_or(atrium_spv_ir::Type::Void);
                            ctx.uniforms.push(UniformBinding {
                                set, binding, offset: 0, ty,
                            });
                            ctx.var_binding.insert(*var_id, (set, binding));
                        }
                    }
                }
                SpvStorageClass::Input => {
                    if let Some(d) = deco {
                        if let (Some(location), Some(ty)) = (d.location, pointee_ty.clone()) {
                            // For phase 1 v1 we treat any
                            // Input variable as a vertex
                            // attribute (the only stage
                            // that uses Input today).
                            ctx.vertex_inputs.push(VertexInput {
                                location, offset: 0, ty,
                            });
                            // Stash (var_id, location, byte_size)
                            // so we can compute byte offsets in
                            // Location order after the walk.
                            // Same shape as the Output branch
                            // does for varyings.
                            let sz = ir_type_size_bytes_for(
                                &types.types, *pointee_id);
                            input_var_loc_size.push((*var_id, location, sz));
                        }
                    }
                }
                SpvStorageClass::PushConstant => {
                    // SPIR-V mandates exactly one PushConstant
                    // variable per entry point. Record it +
                    // compute the block's total size from
                    // the struct layout.
                    ctx.push_constant_var = Some(*var_id);
                    if let Some(layout) = ctx.struct_layouts.get(pointee_id) {
                        let mut total: u32 = 0;
                        for m in layout {
                            let size = ir_type_size_bytes_for(&types.types, m.type_id);
                            total = total.max(m.byte_offset.saturating_add(size));
                        }
                        ctx.push_constants_size = total;
                    }
                }
                SpvStorageClass::StorageBuffer => {
                    if let Some(d) = deco {
                        if let (Some(set), Some(binding)) =
                            (d.descriptor_set, d.binding)
                        {
                            ctx.var_binding.insert(*var_id, (set, binding));
                            ctx.storage_buffer_vars.insert(*var_id);
                        }
                    }
                }
                SpvStorageClass::Workgroup => {
                    // Each Workgroup variable consumes its
                    // type's byte size in a per-workgroup
                    // scratch buffer.  `aggregate_type_size`
                    // handles scalars, vectors, arrays,
                    // matrices and structs.  Offsets are
                    // assigned in declaration order, each var
                    // aligned to min(size,16).max(4).
                    let size = aggregate_type_size(module, types, *pointee_id);
                    let align = size.min(16).max(4);
                    let aligned = (ctx.workgroup_size + align - 1) & !(align - 1);
                    ctx.workgroup_var_offset.insert(*var_id, aligned);
                    ctx.workgroup_size = aligned + size;
                }
                SpvStorageClass::Output => {
                    if let Some(d) = deco {
                        if let (Some(location), Some(ty)) = (d.location, pointee_ty.clone()) {
                            ctx.varyings.push(Varying {
                                location, offset: 0, ty,
                            });
                            // Stash (var_id, location, byte_size)
                            // so we can compute byte offsets in
                            // Location order after the walk
                            // completes.  pointee_ty is the
                            // resolved IR Type for the
                            // OpTypePointer's pointee -- use
                            // it directly via the same
                            // ir_type_size_bytes helper that
                            // sizes uniforms / workgroups.
                            let sz = ir_type_size_bytes_for(
                                &types.types,
                                *pointee_id,
                            );
                            output_var_loc_size.push((*var_id, location, sz));
                        }
                    }
                }
                _ => {}
            }
        }

        // Assign byte offsets to Location-decorated VS Output
        // variables in Location order so the backend can
        // route OpStore through them into `out_varyings +
        // offset`.  Matches the FS-side packing the
        // rasterizer's interpolator produces.
        output_var_loc_size.sort_by_key(|(_, loc, _)| *loc);
        let mut running: u32 = 0;
        for (var_id, _loc, sz) in &output_var_loc_size {
            ctx.output_varying_byte_offset.insert(*var_id, running);
            running = running.saturating_add(*sz);
        }
        // Same shape for Location-decorated Input vars:
        // sort by Location, assign prefix-sum byte offsets.
        // For VS this maps onto the daemon's
        // `assemble_vertices` output (already packed in
        // shader-location order).  For FS this maps onto
        // the same `out_varyings` packing the VS produced.
        input_var_loc_size.sort_by_key(|(_, loc, _)| *loc);
        let mut running: u32 = 0;
        for (var_id, _loc, sz) in &input_var_loc_size {
            ctx.input_varying_byte_offset.insert(*var_id, running);
            running = running.saturating_add(*sz);
        }

        Ok(ctx)
    }
}

#[derive(Debug, Default, Clone)]
struct VarDecorations {
    descriptor_set: Option<u32>,
    binding: Option<u32>,
    location: Option<u32>,
    /// `Decoration::BuiltIn <kind>`; only the kinds in
    /// `atrium_spv_ir::BuiltinKind` are recognised, the rest
    /// stay `None` and the variable falls through to the
    /// regular Input/Output path.
    builtin: Option<atrium_spv_ir::BuiltinKind>,
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

/// Byte size of a leaf IR type, looked up by SPIR-V id
/// from the TypeContext's type map. Used to compute push-
/// constant block totals from struct-member offsets +
/// trailing-member size. Unknown types return 0.
pub(crate) fn ir_type_size_bytes_for(
    types: &HashMap<Word, atrium_spv_ir::Type>,
    id: Word,
) -> u32 {
    use atrium_spv_ir::Type;
    match types.get(&id) {
        Some(Type::Bool)
        | Some(Type::I32) | Some(Type::U32) | Some(Type::F32) => 4,
        Some(Type::I64) | Some(Type::U64) | Some(Type::F64) => 8,
        Some(Type::Vec2(_)) => 8,
        Some(Type::Vec3(_)) => 12,
        Some(Type::Vec4(_)) => 16,
        _ => 0,
    }
}

/// Look up an `OpConstant`'s integer literal value by its
/// result id.  Scans `module.types_global_values` (constants
/// and types share that section).  Returns `None` for
/// non-integer or non-constant ids.
fn constant_u32_value(module: &Module, id: Word) -> Option<u32> {
    for inst in &module.types_global_values {
        if inst.class.opcode != SpvOp::Constant { continue; }
        if inst.result_id != Some(id) { continue; }
        // OpConstant operand 0 is the literal value.
        return match inst.operands.first() {
            Some(Operand::LiteralBit32(v)) => Some(*v),
            Some(Operand::LiteralBit64(v)) => Some(*v as u32),
            _ => None,
        };
    }
    None
}

/// Recursively compute the byte size of an arbitrary SPIR-V
/// type for workgroup-storage layout.  Handles scalars and
/// vectors (via [`ir_type_size_bytes_for`]) plus aggregates:
///   - `OpTypeArray`: element_size × constant length,
///   - `OpTypeMatrix`: column_size × column count,
///   - `OpTypeStruct`: sum of member sizes (packed; matches
///     the workgroup buffer's no-padding layout).
/// Returns 0 for `OpTypeRuntimeArray` (unsized -- not valid
/// in Workgroup storage) and anything unrecognised.
pub(crate) fn aggregate_type_size(
    module: &Module,
    types: &TypeContext,
    type_id: Word,
) -> u32 {
    // Scalar / vector fast path.
    let leaf = ir_type_size_bytes_for(&types.types, type_id);
    if leaf > 0 { return leaf; }
    // Aggregate: consult the raw OpType* instruction.
    let raw = match types.get_raw(type_id) { Ok(r) => r, Err(_) => return 0 };
    match raw.class.opcode {
        SpvOp::TypeArray => {
            // operands: [element_type_id, length_const_id]
            let elem = match raw.operands.first() {
                Some(Operand::IdRef(id)) => *id,
                _ => return 0,
            };
            let len_id = match raw.operands.get(1) {
                Some(Operand::IdRef(id)) => *id,
                _ => return 0,
            };
            let count = constant_u32_value(module, len_id).unwrap_or(0);
            aggregate_type_size(module, types, elem).saturating_mul(count)
        }
        SpvOp::TypeMatrix => {
            // operands: [column_type_id, column_count]
            let col = match raw.operands.first() {
                Some(Operand::IdRef(id)) => *id,
                _ => return 0,
            };
            let count = match raw.operands.get(1) {
                Some(Operand::LiteralBit32(c)) => *c,
                _ => return 0,
            };
            aggregate_type_size(module, types, col).saturating_mul(count)
        }
        SpvOp::TypeStruct => {
            let mut total = 0u32;
            for op in &raw.operands {
                if let Operand::IdRef(member_ty) = op {
                    total = total.saturating_add(
                        aggregate_type_size(module, types, *member_ty));
                }
            }
            total
        }
        _ => 0,
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
