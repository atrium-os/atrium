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

/// Shader: read i32 `n` from push-const, compute
/// `vec4(float(n), float(n*2), float(n+5), 1.0)`.
/// Exercises ConstInt, IAdd, IMul, ConvertSToF.
fn build_int_arith_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
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
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let two_i  = b.constant_bit32(i32_ty, 2u32);
    let five_i = b.constant_bit32(i32_ty, 5u32);
    let c1f = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let two_n = b.i_mul(i32_ty, None, n, two_i).unwrap();
    let n_plus_5 = b.i_add(i32_ty, None, n, five_i).unwrap();
    let n_f = b.convert_s_to_f(f32_ty, None, n).unwrap();
    let two_n_f = b.convert_s_to_f(f32_ty, None, two_n).unwrap();
    let np5_f = b.convert_s_to_f(f32_ty, None, n_plus_5).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![n_f, two_n_f, np5_f, c1f]).unwrap();
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

/// Shader exercising bitwise + int↔float. Reads i32
/// `n` from push-const, computes:
///   hi   = (n >> 4) & 0xF       upper nibble
///   lo   = n & 0xF              lower nibble
///   xor  = n ^ 0xAA             bitwise xor
///   or_  = n | 0x10             bitwise or
/// Output: vec4(float(hi)/15.0, float(lo)/15.0,
///             float(xor)/255.0, float(or_)/255.0)
fn build_int_cmp_bitwise_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
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
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let four_i = b.constant_bit32(i32_ty, 4u32);
    let mask_i = b.constant_bit32(i32_ty, 0xFu32);
    let xor_i  = b.constant_bit32(i32_ty, 0xAAu32);
    let or_i   = b.constant_bit32(i32_ty, 0x10u32);
    let c15 = b.constant_bit32(f32_ty, 15.0f32.to_bits());
    let c255 = b.constant_bit32(f32_ty, 255.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let shifted = b.shift_right_arithmetic(i32_ty, None, n, four_i).unwrap();
    let hi = b.bitwise_and(i32_ty, None, shifted, mask_i).unwrap();
    let lo = b.bitwise_and(i32_ty, None, n, mask_i).unwrap();
    let xor = b.bitwise_xor(i32_ty, None, n, xor_i).unwrap();
    let or_ = b.bitwise_or(i32_ty, None, n, or_i).unwrap();
    let hi_f = b.convert_s_to_f(f32_ty, None, hi).unwrap();
    let lo_f = b.convert_s_to_f(f32_ty, None, lo).unwrap();
    let xor_f = b.convert_s_to_f(f32_ty, None, xor).unwrap();
    let or_f = b.convert_s_to_f(f32_ty, None, or_).unwrap();
    let hi_n = b.f_div(f32_ty, None, hi_f, c15).unwrap();
    let lo_n = b.f_div(f32_ty, None, lo_f, c15).unwrap();
    let xor_n = b.f_div(f32_ty, None, xor_f, c255).unwrap();
    let or_n = b.f_div(f32_ty, None, or_f, c255).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![hi_n, lo_n, xor_n, or_n]).unwrap();
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

/// Shader using integer compare + structured if/else:
/// if (n < 10) out = red; else out = blue.
fn build_int_if_else_shader() -> Vec<u8> {
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
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let ten_i  = b.constant_bit32(i32_ty, 10u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let red  = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let blue = b.constant_composite(vec4, vec![c0, c0, c1, c1]);
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let cond = b.s_less_than(bool_ty, None, n, ten_i).unwrap();
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

/// Build an if/else shader that uses OpPhi at the merge
/// block to pick the f32 result, then writes it as a
/// vec4. Exercises cross-block scalar liveness + Phi
/// edge-moves.
fn build_phi_if_else_shader() -> Vec<u8> {
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
    let c025 = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let c05 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let entry_label = b.id();
    let _ = entry_label;
    let p = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let v = b.load(f32_ty, None, p, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, v, c05).unwrap();
    let then_id = b.id();
    let else_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.branch_conditional(cond, then_id, else_id, vec![]).unwrap();
    b.begin_block(Some(then_id)).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(else_id)).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(merge_id)).unwrap();
    let chosen = b.phi(f32_ty, None,
        vec![(c1, then_id), (c025, else_id)]).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![chosen, chosen, chosen, c1]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    let _ = c0;
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Build a shader using OpSelect to pick a scalar:
/// `v = (scale < 0.5) ? 1.0 : 0.25; out = vec4(v, v, v, 1)`.
fn build_select_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
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
    let c025 = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let c05  = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1   = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let v = b.load(f32_ty, None, p, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, v, c05).unwrap();
    let chosen = b.select(f32_ty, None, cond, c1, c025).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![chosen, chosen, chosen, c1]).unwrap();
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

/// 4-case Switch shader: `case 0 → red; case 1 → green;
/// case 2 → blue; default → white`.
fn build_switch_shader() -> Vec<u8> {
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
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c0 = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1 = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let red   = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let green = b.constant_composite(vec4, vec![c0, c1, c0, c1]);
    let blue  = b.constant_composite(vec4, vec![c0, c0, c1, c1]);
    let white = b.constant_composite(vec4, vec![c1, c1, c1, c1]);
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let c0_id = b.id();
    let c1_id = b.id();
    let c2_id = b.id();
    let dflt_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.switch(n, dflt_id, vec![
        (rspirv::dr::Operand::LiteralBit32(0), c0_id),
        (rspirv::dr::Operand::LiteralBit32(1), c1_id),
        (rspirv::dr::Operand::LiteralBit32(2), c2_id),
    ]).unwrap();
    b.begin_block(Some(c0_id)).unwrap();
    b.store(out, red, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(c1_id)).unwrap();
    b.store(out, green, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(c2_id)).unwrap();
    b.store(out, blue, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(dflt_id)).unwrap();
    b.store(out, white, None, vec![]).unwrap();
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

/// Build a simple counted-loop shader:
///   int acc = 0; for (int i = 0; i < n; i++) acc += i;
///   out = (acc == EXPECTED) ? vec4(1,1,1,1) : vec4(0,0,0,1)
/// Phi at the loop header (induction `i` + accumulator
/// `acc`). BranchCond at the header tests `i < n` and
/// branches to body or merge — neither is Phi-bearing
/// (the header itself is, but isn't a BranchCond target).
fn build_loop_sum_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, LoopControl, MemoryModel,
        SelectionControl, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let one_i  = b.constant_bit32(i32_ty, 1u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();

    // Pre-allocate block + value ids so the Phi can name
    // the back-edge values.
    let entry_id = b.id();
    let header_id = b.id();
    let body_id = b.id();
    let cont_id = b.id();
    let merge_id = b.id();
    let i_next = b.id();
    let acc_next = b.id();

    // entry: load n, branch to header.
    b.begin_block(Some(entry_id)).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    b.branch(header_id).unwrap();

    // header: phi i, phi acc, cmp, LoopMerge, BranchCond.
    b.begin_block(Some(header_id)).unwrap();
    let i_phi = b.phi(i32_ty, None, vec![
        (zero_i, entry_id),
        (i_next, cont_id),
    ]).unwrap();
    let acc_phi = b.phi(i32_ty, None, vec![
        (zero_i, entry_id),
        (acc_next, cont_id),
    ]).unwrap();
    let cond = b.s_less_than(bool_ty, None, i_phi, n).unwrap();
    b.loop_merge(merge_id, cont_id, LoopControl::NONE, vec![]).unwrap();
    b.branch_conditional(cond, body_id, merge_id, vec![]).unwrap();

    // body: empty, branch to continue.
    b.begin_block(Some(body_id)).unwrap();
    b.branch(cont_id).unwrap();

    // continue: i_next = i+1, acc_next = acc+i, back-edge.
    b.begin_block(Some(cont_id)).unwrap();
    b.i_add(i32_ty, Some(i_next), i_phi, one_i).unwrap();
    b.i_add(i32_ty, Some(acc_next), acc_phi, i_phi).unwrap();
    b.branch(header_id).unwrap();

    // merge: check acc_phi vs expected, write colour.
    b.begin_block(Some(merge_id)).unwrap();
    // expected = n*(n-1)/2; we hardcode for n=5 → 10.
    let expected = b.constant_bit32(i32_ty, 10u32);
    let ok = b.i_equal(bool_ty, None, acc_phi, expected).unwrap();
    let red = b.select(f32_ty, None, ok, c1, c0).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![red, red, red, c1]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    let _ = SelectionControl::NONE; // silence unused

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// OpSelect with scalar cond + vec4 t/f operands.
/// out = (scale < 0.5) ? red_vec : blue_vec
fn build_vec_select_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
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
    let red_v  = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let blue_v = b.constant_composite(vec4, vec![c0, c0, c1, c1]);
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let v = b.load(f32_ty, None, p, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, v, c05).unwrap();
    let chosen = b.select(vec4, None, cond, red_v, blue_v).unwrap();
    b.store(out, chosen, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Shader exercising OpDot + OpVectorTimesScalar +
/// OpCompositeConstruct of computed-lane values.
///
/// d = dot(va, vb);                 // scalar
/// scaled = vb * d;                 // vec4 = vec × scalar
/// out = vec4(scaled.x, scaled.y, scaled.z, d)
fn build_dot_vts_shader() -> Vec<u8> {
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

    let a = [0.1f32, 0.2, 0.3, 0.4];
    let b = [0.5f32, 0.6, 0.7, 0.8];
    let ca: Vec<_> = a.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let cb: Vec<_> = b.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = bld.constant_composite(vec4, ca.clone());
    let vb = bld.constant_composite(vec4, cb.clone());

    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    // d = dot(va, vb)
    let d = bld.dot(f32_ty, None, va, vb).unwrap();
    // scaled = vb * d  (OpVectorTimesScalar)
    let scaled = bld.vector_times_scalar(vec4, None, vb, d).unwrap();
    // Re-extract via swizzle/access — easier: pull lanes
    // from the original cb constants then construct
    // (xy lanes from scaled, then d). We need a way to
    // get scaled.x/y/z. Use VectorShuffle to grab the
    // first three lanes of scaled + d.
    //
    // Actually atrium-spv-frontend translates VectorShuffle
    // to Op::VectorShuffle, which the bespoke backend
    // doesn't yet support. Instead build the output via
    // a fresh CompositeConstruct using vb's lanes scaled
    // manually (vb_i * d for i ∈ {0,1,2}) + d itself.
    let _ = scaled; // exercise VectorTimesScalar (lowers
                    // to vec×scalar FMul which we DO
                    // support); the result isn't used to
                    // avoid OpVectorShuffle / extract.

    // Build vec4(va_0 * vb_0, va_1 * vb_1, va_2 * vb_2, d)
    // — exercises FMul on individual scalars +
    // CompositeConstruct of computed lanes + the Dot
    // result threaded through to a lane.
    let m0 = bld.f_mul(f32_ty, None, ca[0], cb[0]).unwrap();
    let m1 = bld.f_mul(f32_ty, None, ca[1], cb[1]).unwrap();
    let m2 = bld.f_mul(f32_ty, None, ca[2], cb[2]).unwrap();
    let color = bld.composite_construct(vec4, None,
        vec![m0, m1, m2, d]).unwrap();
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

/// OpVectorShuffle exercise: out = va.bgra (swizzle).
fn build_swizzle_shader(comps: [u32; 4]) -> Vec<u8> {
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
    let lanes = [0.1f32, 0.2, 0.3, 0.4];
    let cs: Vec<_> = lanes.iter()
        .map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = bld.constant_composite(vec4, cs);
    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let swizzled = bld.vector_shuffle(vec4, None, va, va, comps.to_vec()).unwrap();
    bld.store(out, swizzled, None, vec![]).unwrap();
    bld.ret().unwrap();
    bld.end_function().unwrap();
    bld.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    bld.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = bld.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// OpCompositeExtract exercise: extract individual lanes
/// from a vec4 and recombine them in a different order
/// via OpCompositeConstruct.
///   va = (0.1, 0.2, 0.3, 0.4)
///   out = vec4(va[3], va[0], va[2], va[1])
fn build_composite_extract_shader() -> Vec<u8> {
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
    let lanes = [0.1f32, 0.2, 0.3, 0.4];
    let cs: Vec<_> = lanes.iter()
        .map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = bld.constant_composite(vec4, cs);
    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let e3 = bld.composite_extract(f32_ty, None, va, vec![3]).unwrap();
    let e0 = bld.composite_extract(f32_ty, None, va, vec![0]).unwrap();
    let e2 = bld.composite_extract(f32_ty, None, va, vec![2]).unwrap();
    let e1 = bld.composite_extract(f32_ty, None, va, vec![1]).unwrap();
    let color = bld.composite_construct(vec4, None,
        vec![e3, e0, e2, e1]).unwrap();
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
fn three_way_bitwise_and_shift() {
    let spirv = build_int_cmp_bitwise_shader();
    let mut inputs = ShaderInputs::default();
    let n: i32 = 0xC3; // upper=0xC, lower=0x3
    inputs.push_constants[..4].copy_from_slice(&n.to_le_bytes());
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
fn three_way_int_compare_then_branch() {
    let spirv = build_int_if_else_shader();
    let mut inputs = ShaderInputs::default();
    let n: i32 = 5;
    inputs.push_constants[..4].copy_from_slice(&n.to_le_bytes());
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
fn three_way_int_compare_else_branch() {
    let spirv = build_int_if_else_shader();
    let mut inputs = ShaderInputs::default();
    let n: i32 = 99;
    inputs.push_constants[..4].copy_from_slice(&n.to_le_bytes());
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
fn three_way_phi_then_branch() {
    let spirv = build_phi_if_else_shader();
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
fn three_way_phi_else_branch() {
    let spirv = build_phi_if_else_shader();
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

#[test]
fn three_way_select_then() {
    let spirv = build_select_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0.2f32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_select_else() {
    let spirv = build_select_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0.8f32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_switch_case0() {
    let spirv = build_switch_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0i32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_switch_case1() {
    let spirv = build_switch_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&1i32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_switch_default() {
    let spirv = build_switch_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&99i32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_simple_loop() {
    let spirv = build_loop_sum_shader();
    let mut inputs = ShaderInputs::default();
    let n: i32 = 5; // 0+1+2+3+4 = 10 = expected
    inputs.push_constants[..4].copy_from_slice(&n.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_vec_select_then() {
    let spirv = build_vec_select_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0.2f32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_vec_select_else() {
    let spirv = build_vec_select_shader();
    let mut inputs = ShaderInputs::default();
    inputs.push_constants[..4].copy_from_slice(&0.8f32.to_le_bytes());
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &inputs, ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_vector_shuffle_bgra() {
    // .bgra swizzle: indices [2, 1, 0, 3].
    let spirv = build_swizzle_shader([2, 1, 0, 3]);
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &ShaderInputs::default(),
                         ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_vector_shuffle_cross_source() {
    // Pulls from both src1 and src2 (=src1 in our test):
    // [0, 5, 2, 7] picks va.x, va.y(=src2[1]), va.z,
    // va.w(=src2[3]). Same as [0,1,2,3] when both
    // sources are the same vector — exercises the
    // cross-source branch of the shuffle.
    let spirv = build_swizzle_shader([0, 5, 2, 7]);
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &ShaderInputs::default(),
                         ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_composite_extract() {
    let spirv = build_composite_extract_shader();
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &ShaderInputs::default(),
                         ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_dot_and_composite() {
    let spirv = build_dot_vts_shader();
    let rs = runners();
    let refs: Vec<&dyn ShaderRunner> = rs.iter().map(|b| b.as_ref()).collect();
    assert_shader_agrees(&spirv, &ShaderInputs::default(),
                         ColorTolerance::Exact, &refs);
}

#[test]
fn three_way_int_arith_and_convert() {
    let spirv = build_int_arith_shader();
    let mut inputs = ShaderInputs::default();
    let n: i32 = 7;
    inputs.push_constants[..4].copy_from_slice(&n.to_le_bytes());
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
