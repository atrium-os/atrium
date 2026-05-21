//! Bespoke compute path: prove an empty GLCompute shader
//! compiles through the bespoke ARM64 backend (not via the
//! Cranelift fallback).
//!
//! atrium-spv-compile's selection logic tries bespoke first
//! and falls back to Cranelift on `BackendError::Unsupported`.
//! For correctness this is fine -- Cranelift produces the
//! same runtime behaviour. But the perf premise of the
//! bespoke backend is "hand-tuned ARM64 codegen, faster than
//! Cranelift" -- so we want to know which compute shaders
//! actually exercise the bespoke path.
//!
//! This test calls `compile_blob` directly (no fallback), so
//! it succeeds only if bespoke can handle the shader end-to-
//! end.  The empty CS is the floor: header + LocalSize + a
//! single OpReturn.  If even this rejects, the bespoke
//! compute foundation isn't wired.

use atrium_spv_backend_bespoke::{compile_blob, Target};
use atrium_spv_frontend::translate;

fn build_empty_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Compute SPIR-V that writes a vec4<f32> to ssbo[0] via
/// OpAccessChain + OpStore. Exercises the (Compute,
/// StorageBuffer) -> X2 mapping that lets a real-output
/// compute shader compile through bespoke without touching
/// gl_*InvocationID (those land in a separate commit).
fn build_ssbo_vec4_cs(values: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let ssbo_struct = b.type_struct(vec![vec4]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_vec4   = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(u32_ty, 0);
    let cs: Vec<_> = values.iter()
        .map(|v| b.constant_bit32(f32_ty, v.to_bits())).collect();
    let c_vec = b.constant_composite(vec4, cs);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst = b.access_chain(ptr_ssbo_vec4, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, c_vec, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_ssbo_vec4_store() {
    let spv = build_ssbo_vec4_cs([1.0, 2.0, 3.0, 4.0]);
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a vec4 SSBO write -- this exercises \
                 OpAccessChain + OpStore through the (Compute, StorageBuffer) \
                 -> X2 path");
    assert!(!out.blob.is_empty());
}

/// Compute SPIR-V that reads gl_LocalInvocationID.x, multiplies
/// by 100, writes to ssbo[0].  Exercises Op::LoadBuiltin
/// (compute) -> Op::VectorExtract (int-lane path) ->
/// Op::IMul -> Op::Store (scalar u32) through bespoke.
fn build_lid_mul_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);

    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let lid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(lid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::LocalInvocationId)]);

    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero    = b.constant_bit32(u32_ty, 0);
    let c_hundred = b.constant_bit32(u32_ty, 100);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let lid = b.load(uvec3, None, lid_var, None, vec![]).unwrap();
    let lid_x = b.composite_extract(u32_ty, None, lid, vec![0]).unwrap();
    let product = b.i_mul(u32_ty, None, lid_x, c_hundred).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, product, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![lid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_lid_mul_ssbo_write() {
    let spv = build_lid_mul_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS reading gl_LocalInvocationID + \
                 writing a scalar u32 via SSBO -- exercises Op::LoadBuiltin \
                 (Compute, LocalInvocationId) + Op::VectorExtract (int lane) \
                 + Op::IMul + Op::Store (scalar u32 path)");
    assert!(!out.blob.is_empty());
}

#[test]
fn bespoke_compiles_empty_compute_shader() {
    let spv = build_empty_cs();
    let module = translate(&spv).expect("frontend translate");

    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };

    let out = compile_blob(&module, target)
        .expect("bespoke should compile an empty compute shader directly \
                 (no Cranelift fallback)");

    assert!(!out.blob.is_empty(),
        "bespoke compute output should be non-empty");
    // PC-map can legitimately be empty for an empty function
    // (a single OpReturn may not get its own pc entry depending
    // on the emission order); the load-bearing assertion is
    // that the blob exists.
}
