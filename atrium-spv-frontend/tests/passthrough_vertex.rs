//! Phase 0 of the vertex-stage arc — frontend smoke gate.
//!
//! Hand-builds a passthrough vertex shader (vec3 input
//! attribute, writes `vec4(pos, 1.0)` to gl_Position) and
//! checks that `translate` accepts it: produces a Vertex-
//! stage function with the right entry point and exactly
//! one Input variable at location 0. Doesn't exercise any
//! backend yet — the matching codegen lands in subsequent
//! phases (per the RUNBOOK scoping).

use atrium_spv_ir::{ShaderStage, Type, VecElement};
use atrium_spv_frontend::translate;

fn build_passthrough_vertex_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    // gl_PerVertex { vec4 gl_Position; } — the minimal
    // built-in output block a Vulkan vertex shader writes.
    // Member 0 (gl_Position) gets both a BuiltIn decoration
    // (for the writer side) and an Offset (so the
    // frontend's struct-layout pass picks it up — the
    // current AccessChain implementation walks struct
    // members by their Offset annotation).
    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);

    let ptr_pv_struct = b.type_pointer(
        None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4);
    let ptr_in_vec3 = b.type_pointer(
        None, SpvStorageClass::Input, vec3);

    // in vec3 a_position;
    let in_pos = b.variable(ptr_in_vec3, None, SpvStorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    // gl_PerVertex output variable.
    let pv_var = b.variable(ptr_pv_struct, None,
                            SpvStorageClass::Output, None);

    let i32_ty = b.type_int(32, 1);
    let c_zero = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();

    // pos = OpLoad %vec3 %in_pos
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    // pos4 = vec4(pos.x, pos.y, pos.z, 1.0) via
    // OpCompositeExtract per lane + OpCompositeConstruct.
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    // gl_Position = pos4 — OpAccessChain into member 0 of
    // the gl_PerVertex output, OpStore.
    let pos_ptr = b.access_chain(
        ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(pos_ptr, pos4, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_pos, pv_var]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn frontend_translates_passthrough_vertex() {
    let spirv = build_passthrough_vertex_shader();
    let module = translate(&spirv).expect(
        "frontend must accept a passthrough vertex shader");

    // One function.
    assert_eq!(module.functions.len(), 1);
    let func = &module.functions[0];
    assert_eq!(func.stage, ShaderStage::Vertex);

    // One entry point, Vertex stage, named "main".
    assert_eq!(module.entry_points.len(), 1);
    let ep = &module.entry_points[0];
    assert_eq!(ep.stage, ShaderStage::Vertex);
    assert_eq!(ep.name, "main");

    // One vertex input at location 0, vec3<f32>.
    assert_eq!(module.vertex_inputs.len(), 1);
    let vi = &module.vertex_inputs[0];
    assert_eq!(vi.location, 0);
    assert!(matches!(vi.ty, Type::Vec3(VecElement::F32)),
        "vertex input type: {:?}", vi.ty);
}
