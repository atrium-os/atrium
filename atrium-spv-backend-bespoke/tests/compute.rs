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
use std::path::PathBuf;
use std::process::Command;

/// Locate the just-built atrium-spv-compile binary alongside
/// this test's deps/ dir.
fn locate_spv_compile() -> PathBuf {
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); p.pop(); p.pop(); p.pop(); p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(), "atrium-spv-compile not at {}", p.display());
    p
}

/// Invoke atrium-spv-compile against `spv` bytes and parse
/// the JSON report's backend field.  Returns Err on
/// compilation failure (which atrium-spv-compile would
/// surface to the daemon as a fallback signal too).
fn invoked_backend(spv: &[u8]) -> Result<String, String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let input_path = tmp.path().join("in.spv");
    std::fs::write(&input_path, spv).map_err(|e| e.to_string())?;
    let target = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else {
        "aarch64-unknown-freebsd"
    };
    let out = Command::new(locate_spv_compile())
        .arg("--input").arg(&input_path)
        .arg("--output-dir").arg(tmp.path())
        .arg("--target").arg(target)
        .arg("--hash").arg("deadbeef")
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("atrium-spv-compile exit code {:?}: stderr={} stdout={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)));
    }
    // The JSON report goes to stderr (per main.rs G7 metrics
    // convention).  Hand-parse for `"backend":"NAME"`.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let needle = "\"backend\":\"";
    let i = stderr.find(needle).ok_or_else(||
        format!("no backend field in stderr: {stderr}"))?;
    let rest = &stderr[i + needle.len()..];
    let end = rest.find('"').ok_or_else(||
        "unterminated backend field".to_string())?;
    Ok(rest[..end].to_string())
}

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

/// Compute SPIR-V that reads gl_GlobalInvocationID.x and
/// writes it to ssbo[0].  With LocalSize=(4,1,1) the
/// codegen has to materialise LocalSize as a runtime
/// constant + emit mul_w + add_w per lane, exercising the
/// non-unit-LocalSize branch of GlobalInvocationId codegen.
fn build_gid_cs(local_size_x: u32) -> Vec<u8> {
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
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
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
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, gid_x, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [local_size_x, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_gid_with_local_size_4() {
    let spv = build_gid_cs(4);
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS reading gl_GlobalInvocationID \
                 with LocalSize=4 -- exercises the mul+add path that folds \
                 LocalSize into the GID formula");
    assert!(!out.blob.is_empty());
}

#[test]
fn spv_compile_picks_bespoke_for_supported_compute_shaders() {
    // Production selection logic: atrium-spv-compile tries
    // bespoke first, falls back to Cranelift on Unsupported.
    // After the bespoke compute foundation + Op::LoadBuiltin
    // + Op::Store + GID-with-LocalSize + lid.z patch series,
    // these shaders all compile through bespoke directly.
    // This test invokes the real production binary and parses
    // its JSON report.

    // Skip on non-aarch64 hosts: bespoke is ARM64-only.
    if !cfg!(target_arch = "aarch64") {
        return;
    }

    for (name, spv) in [
        ("empty",      build_empty_cs()),
        ("ssbo_vec4",  build_ssbo_vec4_cs([1.0, 2.0, 3.0, 4.0])),
        ("lid_mul",    build_lid_mul_cs()),
        ("gid_ls4",    build_gid_cs(4)),
    ] {
        let backend = invoked_backend(&spv)
            .unwrap_or_else(|e| panic!("compile failed for {name}: {e}"));
        assert_eq!(backend, "bespoke",
            "{name} should compile through bespoke, got {backend}");
    }
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
