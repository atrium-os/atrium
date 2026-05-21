//! Frontend captures (set, binding) for StorageBuffer
//! variables and surfaces them on the IR `Function` so
//! backends can map per-binding without re-walking the
//! SPIR-V interface.

use atrium_spv_frontend::translate;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, Capability, Decoration, ExecutionMode,
    ExecutionModel, FunctionControl, MemoryModel, StorageClass,
};

fn build_two_ssbo_cs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let in_struct = b.type_struct(vec![u32_ty]);
    b.decorate(in_struct, Decoration::Block, vec![]);
    b.member_decorate(in_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out_struct = b.type_struct(vec![u32_ty]);
    b.decorate(out_struct, Decoration::Block, vec![]);
    b.member_decorate(out_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_struct  = b.type_pointer(None, StorageClass::StorageBuffer, in_struct);
    let ptr_out_struct = b.type_pointer(None, StorageClass::StorageBuffer, out_struct);
    let ptr_ssbo_u32   = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let in_var = b.variable(ptr_in_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(in_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(in_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out_var = b.variable(ptr_out_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(out_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(out_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_ssbo_u32, None, in_var,  vec![c_zero]).unwrap();
    let v   = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, out_var, vec![c_zero]).unwrap();
    b.store(dst, v, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![in_var, out_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn ssbo_bindings_captured_on_function() {
    let spv = build_two_ssbo_cs();
    let module = translate(&spv).expect("frontend");
    assert_eq!(module.functions.len(), 1);
    let func = &module.functions[0];
    // Two StorageBuffer variables in this CS, both should
    // appear in the per-function ssbo_bindings map with
    // their (set, binding) preserved.
    let mut pairs: Vec<(u32, u32)> =
        func.ssbo_bindings.values().copied().collect();
    pairs.sort();
    assert_eq!(pairs, vec![(0, 0), (0, 1)],
        "expected two SSBO bindings (0,0) and (0,1), got {pairs:?}");
}
