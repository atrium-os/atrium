//! Fragment shader that reads a `vec4` from a uniform
//! block and stores it to `out_color`. Gates the existing
//! `(Fragment, Uniform) → X1 / params[1]` plumbing in
//! both backends + the interpreter's `Uniform` path in
//! `load_from_storage`. Nothing new in the backends —
//! this just adds an end-to-end test for the path that's
//! been latently working since phase 2.

use atrium_spv_differential::{BespokeRunner, CraneliftRunner};
use atrium_spv_tests::harness::{
    assert_shader_agrees, InterpreterRunner, ShaderRunner,
};
use atrium_spv_tests::interpreter::ShaderInputs;
use atrium_spv_tests::pixels::ColorTolerance;

fn build_uniform_color_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    // struct UniformBlock { vec4 color; };
    let ub_struct = b.type_struct(vec![vec4]);
    b.member_decorate(ub_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub_struct, Decoration::Block, vec![]);

    let ptr_ub_struct = b.type_pointer(None, SpvStorageClass::Uniform, ub_struct);
    let ptr_ub_vec4   = b.type_pointer(None, SpvStorageClass::Uniform, vec4);
    let ptr_out_vec4  = b.type_pointer(None, SpvStorageClass::Output, vec4);

    let ub = b.variable(ptr_ub_struct, None, SpvStorageClass::Uniform, None);
    b.decorate(ub, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(i32_ty, 0u32);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // OpAccessChain through member 0 → vec4 pointer.
    let color_ptr = b.access_chain(ptr_ub_vec4, None, ub, vec![c_zero]).unwrap();
    let color = b.load(vec4, None, color_ptr, None, vec![]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![ub, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Two-member uniform block — exercises the AccessChain
/// member-offset computation (member 1 sits at byte 16,
/// not 0). `out_color = ub.color_b` where color_a sits at
/// offset 0 and color_b at offset 16.
fn build_uniform_second_member_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let ub_struct = b.type_struct(vec![vec4, vec4]);
    b.member_decorate(ub_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(ub_struct, 1, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(16)]);
    b.decorate(ub_struct, Decoration::Block, vec![]);

    let ptr_ub_struct = b.type_pointer(None, SpvStorageClass::Uniform, ub_struct);
    let ptr_ub_vec4   = b.type_pointer(None, SpvStorageClass::Uniform, vec4);
    let ptr_out_vec4  = b.type_pointer(None, SpvStorageClass::Output, vec4);

    let ub = b.variable(ptr_ub_struct, None, SpvStorageClass::Uniform, None);
    b.decorate(ub, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_one  = b.constant_bit32(i32_ty, 1u32);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let b_ptr = b.access_chain(ptr_ub_vec4, None, ub, vec![c_one]).unwrap();
    let color_b = b.load(vec4, None, b_ptr, None, vec![]).unwrap();
    b.store(out, color_b, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![ub, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn three_way_uniform_second_member() {
    let color_a = [1.0_f32, 0.0, 0.0, 1.0];  // red — slot 0, ignored
    let color_b = [0.0_f32, 0.0, 1.0, 1.0];  // blue — slot 1, returned
    let mut uniforms = Vec::with_capacity(32);
    for f in color_a { uniforms.extend_from_slice(&f.to_le_bytes()); }
    for f in color_b { uniforms.extend_from_slice(&f.to_le_bytes()); }
    let inputs = ShaderInputs { uniforms, ..ShaderInputs::default() };

    let spirv = build_uniform_second_member_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> = runners.iter().map(|b| b.as_ref()).collect();
    let tol = ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);
}

#[test]
fn three_way_uniform_vec4_color() {
    let color = [1.0_f32, 0.5, 0.0, 1.0];  // orange
    let mut uniforms = Vec::with_capacity(16);
    for f in color { uniforms.extend_from_slice(&f.to_le_bytes()); }
    let inputs = ShaderInputs { uniforms, ..ShaderInputs::default() };

    let spirv = build_uniform_color_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> = runners.iter().map(|b| b.as_ref()).collect();
    let tol = ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);
}
