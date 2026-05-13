//! Three-way differential tests: interpreter + Cranelift +
//! bespoke must all agree on the pixel output for the same
//! SPIR-V shader.

use atrium_spv_differential::{BespokeRunner, CraneliftRunner};
use atrium_spv_tests::harness::{
    assert_shader_agrees, InterpreterRunner, ShaderRunner,
};
use atrium_spv_tests::interpreter::ShaderInputs;
use atrium_spv_tests::pixels::ColorTolerance;

fn build_constant_color_spirv(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let cs: Vec<_> = rgba.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out, rspirv::spirv::Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn build_arith_shader(a: f32, b: f32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let vec4 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out = bld.type_pointer(None, StorageClass::Output, vec4);
    let ca = bld.constant_bit32(f32_ty, a.to_bits());
    let cb = bld.constant_bit32(f32_ty, b.to_bits());
    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let sum  = bld.f_add(f32_ty, None, ca, cb).unwrap();
    let diff = bld.f_sub(f32_ty, None, ca, cb).unwrap();
    let prod = bld.f_mul(f32_ty, None, ca, cb).unwrap();
    let quot = bld.f_div(f32_ty, None, ca, cb).unwrap();
    let color = bld.composite_construct(vec4, None,
        vec![sum, diff, prod, quot]).unwrap();
    bld.store(out, color, None, vec![]).unwrap();
    bld.ret().unwrap();
    bld.end_function().unwrap();
    bld.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    bld.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = bld.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Build `out = (a_vec + b_vec) * (a_vec - b_vec)` using
/// SPIR-V vec4 ops. Exercises vec×vec FAdd / FSub / FMul.
fn build_vec_arith_shader(a: [f32; 4], b: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let vec4 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out = bld.type_pointer(None, StorageClass::Output, vec4);
    let ca: Vec<_> = a.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let cb: Vec<_> = b.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = bld.constant_composite(vec4, ca);
    let vb = bld.constant_composite(vec4, cb);
    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let sum  = bld.f_add(vec4, None, va, vb).unwrap();
    let diff = bld.f_sub(vec4, None, va, vb).unwrap();
    let prod = bld.f_mul(vec4, None, sum, diff).unwrap();
    bld.store(out, prod, None, vec![]).unwrap();
    bld.ret().unwrap();
    bld.end_function().unwrap();
    bld.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    bld.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = bld.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn build_if_else_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, SelectionControl,
        StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let bool_ty = b.type_bool();
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![f32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c05 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let red  = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let blue = b.constant_composite(vec4, vec![c0, c0, c1, c1]);
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let v = b.load(f32_ty, None, p, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, v, c05).unwrap();
    let then_id = b.id();
    let else_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.branch_conditional(cond, then_id, else_id, vec![]).unwrap();
    b.begin_block(Some(then_id)).unwrap();
    b.store(out, red, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(else_id)).unwrap();
    b.store(out, blue, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(merge_id)).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn runners() -> [Box<dyn ShaderRunner>; 3] {
    [
        Box::new(InterpreterRunner::default()),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ]
}

#[test]
fn three_way_constant_color_shader() {
    let spirv = build_constant_color_spirv([0.4, 0.5, 0.6, 1.0]);
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(
        &spirv,
        &ShaderInputs::default(),
        ColorTolerance::Exact,
        &refs,
    );
}

#[test]
fn three_way_fp_arithmetic_shader() {
    let spirv = build_arith_shader(0.75, 0.25);
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(
        &spirv,
        &ShaderInputs::default(),
        ColorTolerance::Exact,
        &refs,
    );
}

#[test]
fn three_way_vec_arithmetic() {
    let a = [0.5f32, 0.6, 0.7, 0.8];
    let b = [0.1f32, 0.2, 0.3, 0.4];
    let spirv = build_vec_arith_shader(a, b);
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(
        &spirv,
        &ShaderInputs::default(),
        ColorTolerance::Exact,
        &refs,
    );
}

#[test]
fn three_way_if_else_then_branch() {
    let spirv = build_if_else_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0.2f32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(
        &spirv,
        &inputs,
        ColorTolerance::Exact,
        &refs,
    );
}

#[test]
fn three_way_if_else_else_branch() {
    let spirv = build_if_else_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0.8f32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(
        &spirv,
        &inputs,
        ColorTolerance::Exact,
        &refs,
    );
}
