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
        ("cond",       build_cond_cs()),
        ("loop",       build_loop_cs()),
        ("f_scalar",   build_float_arith_cs()),
        ("f_vec4",     build_float_vec4_arith_cs()),
    ] {
        let backend = invoked_backend(&spv)
            .unwrap_or_else(|e| panic!("compile failed for {name}: {e}"));
        assert_eq!(backend, "bespoke",
            "{name} should compile through bespoke, got {backend}");
    }
}

/// Compute SPIR-V that reads ssbo.a (offset 0), adds a
/// constant, writes the result to ssbo.b (offset 4).  Tests
/// the read side of the StorageBuffer codegen -- OpLoad
/// through an SSBO pointer at a non-zero AccessChain offset.
fn build_ssbo_rmw_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);

    // struct SSBO { uint a; uint b; };
    let ssbo_struct = b.type_struct(vec![u32_ty, u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(ssbo_struct, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_seven = b.constant_bit32(u32_ty, 7);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    let loaded = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let sum = b.i_add(u32_ty, None, loaded, c_seven).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_one]).unwrap();
    b.store(dst, sum, None, vec![]).unwrap();
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
fn bespoke_compiles_ssbo_read_modify_write() {
    let spv = build_ssbo_rmw_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS that reads SSBO[a]+7 -> SSBO[b]");
    assert!(!out.blob.is_empty());
}

/// Compute SPIR-V with a conditional branch + phi:
///   if (gid.x < 10) ssbo[0] = gid.x * 2;
///   else            ssbo[0] = 0;
///
/// Exercises the compute control-flow path through bespoke
/// (Op::BranchCond + Op::Phi + Op::ULessThan) which until
/// now was only verified via fragment/vertex shaders.
fn build_cond_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, SelectionControl,
        StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let bool_ty = b.type_bool();
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
    let c_two  = b.constant_bit32(u32_ty, 2);
    let c_ten  = b.constant_bit32(u32_ty, 10);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let lt = b.u_less_than(bool_ty, None, gid_x, c_ten).unwrap();
    let then_lbl = b.id();
    let else_lbl = b.id();
    let merge_lbl = b.id();
    b.selection_merge(merge_lbl, SelectionControl::NONE).unwrap();
    b.branch_conditional(lt, then_lbl, else_lbl, vec![]).unwrap();
    b.begin_block(Some(then_lbl)).unwrap();
    let dbl = b.i_mul(u32_ty, None, gid_x, c_two).unwrap();
    b.branch(merge_lbl).unwrap();
    b.begin_block(Some(else_lbl)).unwrap();
    b.branch(merge_lbl).unwrap();
    b.begin_block(Some(merge_lbl)).unwrap();
    let phi = b.phi(u32_ty, None,
        vec![(dbl, then_lbl), (c_zero, else_lbl)]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, phi, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_conditional_compute() {
    let spv = build_cond_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS with a conditional branch + phi \
                 -- exercises Op::ULessThan + Op::BranchCond + Op::Phi \
                 through the compute codegen path");
    assert!(!out.blob.is_empty());
}

/// Compute SPIR-V with a counted loop:
///   uint sum = 0;
///   for (uint i = 0; i < 8; ++i) sum += i;
///   ssbo[0] = sum;
///
/// Exercises a back-edge through Op::Branch with two Phis
/// at the header (loop-carried i and sum), plus IAdd+ULT
/// in the loop body.  This is the canonical "real shaders
/// loop" shape.
fn build_loop_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, LoopControl, MemoryModel,
        SelectionControl, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let bool_ty = b.type_bool();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
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
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_eight = b.constant_bit32(u32_ty, 8);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    let entry = b.id();
    let header = b.id();
    let body = b.id();
    let cont = b.id();
    let merge = b.id();
    b.begin_block(Some(entry)).unwrap();
    b.branch(header).unwrap();
    b.begin_block(Some(header)).unwrap();
    // Phis filled in later (need backedge ids first).  Use
    // forward references: rspirv builder can't easily do
    // that, so we cheat -- declare the phi after we know
    // both predecessor ids.  Build body+cont first via
    // begin_block with explicit ids, then patch phis.
    // Actually rspirv requires the phi at the start; we
    // know cont's id already (we minted it above).
    // Forward-declare back-edge value ids so the header
    // Phis can name them before the body/cont blocks emit
    // the corresponding instructions.
    let new_i_id   = b.id();
    let new_sum_id = b.id();
    let i_phi  = b.phi(u32_ty, None,
        vec![(c_zero, entry), (new_i_id,   cont)]).unwrap();
    let sum_phi = b.phi(u32_ty, None,
        vec![(c_zero, entry), (new_sum_id, cont)]).unwrap();
    let cond = b.u_less_than(bool_ty, None, i_phi, c_eight).unwrap();
    b.loop_merge(merge, cont, LoopControl::NONE, vec![]).unwrap();
    b.branch_conditional(cond, body, merge, vec![]).unwrap();
    b.begin_block(Some(body)).unwrap();
    b.i_add(u32_ty, Some(new_sum_id), sum_phi, i_phi).unwrap();
    b.branch(cont).unwrap();
    b.begin_block(Some(cont)).unwrap();
    b.i_add(u32_ty, Some(new_i_id), i_phi, c_one).unwrap();
    b.branch(header).unwrap();
    b.begin_block(Some(merge)).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, sum_phi, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    let _ = SelectionControl::NONE; // silence unused
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_loop_compute() {
    let spv = build_loop_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a counted-loop CS -- exercises \
                 the loop-header + back-edge + Phi-as-loop-carried path");
    assert!(!out.blob.is_empty());
}

/// Compute SPIR-V doing scalar float arithmetic:
///   ssbo[0] = float(gid.x) * 1.5 + 2.0
///
/// Exercises Op::ConvertUToF + Op::FMul + Op::FAdd + scalar
/// float store through StorageBuffer, end-to-end in compute.
fn build_float_arith_cs() -> Vec<u8> {
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
    let f32_ty = b.type_float(32, None);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let ssbo_struct = b.type_struct(vec![f32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_f32    = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_1p5   = b.constant_bit32(f32_ty, 1.5f32.to_bits());
    let c_2p0   = b.constant_bit32(f32_ty, 2.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let f = b.convert_u_to_f(f32_ty, None, gid_x).unwrap();
    let scaled = b.f_mul(f32_ty, None, f, c_1p5).unwrap();
    let biased = b.f_add(f32_ty, None, scaled, c_2p0).unwrap();
    let dst = b.access_chain(ptr_ssbo_f32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, biased, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_float_arith_compute() {
    let spv = build_float_arith_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS doing scalar float arith \
                 -- exercises Op::ConvertUToF + Op::FMul + Op::FAdd + \
                 scalar f32 SSBO store through the compute path");
    assert!(!out.blob.is_empty());
}

/// Compute SPIR-V doing vec4 float arithmetic:
///   ssbo[0] = vec4(1,2,3,4) * vec4(0.5) + vec4(10)
///
/// Exercises FMul + FAdd on the SIMD/NEON lane path through
/// compute, complementing the scalar float test.
fn build_float_vec4_arith_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
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
    let cs_a: Vec<_> = [1.0f32, 2.0, 3.0, 4.0].iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let vec_a = b.constant_composite(vec4, cs_a);
    let cs_b: Vec<_> = [0.5f32; 4].iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let vec_b = b.constant_composite(vec4, cs_b);
    let cs_c: Vec<_> = [10.0f32; 4].iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let vec_c = b.constant_composite(vec4, cs_c);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let prod = b.f_mul(vec4, None, vec_a, vec_b).unwrap();
    let sum  = b.f_add(vec4, None, prod, vec_c).unwrap();
    let dst = b.access_chain(ptr_ssbo_vec4, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, sum, None, vec![]).unwrap();
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
fn bespoke_compiles_float_vec4_arith_compute() {
    let spv = build_float_vec4_arith_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS doing vec4 float arith \
                 -- exercises FMul + FAdd on the NEON SIMD path \
                 through compute");
    assert!(!out.blob.is_empty());
}

/// Compute SPIR-V with two SSBO bindings:
///   layout(set=0,binding=0) buffer In  { uint a; };
///   layout(set=0,binding=1) buffer Out { uint b; };
///   void main() { b = a + 7; }
///
/// Today bespoke maps ALL StorageBuffer variables to X2 --
/// this test exists to document the gap.  It's #[ignore]d
/// so it doesn't fail the suite, and serves as a TODO marker
/// for the multi-binding SSBO arc.
fn build_two_ssbo_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
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
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_seven = b.constant_bit32(u32_ty, 7);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_ssbo_u32, None, in_var,  vec![c_zero]).unwrap();
    let v   = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let s   = b.i_add(u32_ty, None, v, c_seven).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, out_var, vec![c_zero]).unwrap();
    b.store(dst, s, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![in_var, out_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// CS that writes a distinct constant to each of four
/// SSBO bindings.  Exercises the extended (X16, X17, X12,
/// X13) binding-pool layout that lifted the original
/// 2-binding cap.
fn build_four_ssbo_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let pu = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let mut vars = Vec::new();
    for i in 0..4 {
        let s = b.type_struct(vec![u32_ty]);
        b.decorate(s, Decoration::Block, vec![]);
        b.member_decorate(s, 0, Decoration::Offset,
            vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let p = b.type_pointer(None, StorageClass::StorageBuffer, s);
        let v = b.variable(p, None, StorageClass::StorageBuffer, None);
        b.decorate(v, Decoration::DescriptorSet,
            vec![rspirv::dr::Operand::LiteralBit32(0)]);
        b.decorate(v, Decoration::Binding,
            vec![rspirv::dr::Operand::LiteralBit32(i)]);
        vars.push(v);
    }
    let c_zero = b.constant_bit32(u32_ty, 0);
    let consts: Vec<_> = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444]
        .iter().map(|x| b.constant_bit32(u32_ty, *x)).collect();
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    for (i, v) in vars.iter().enumerate() {
        let d = b.access_chain(pu, None, *v, vec![c_zero]).unwrap();
        b.store(d, consts[i], None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vars);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_four_binding_ssbo() {
    let spv = build_four_ssbo_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS with 4 SSBO bindings -- \
                 exercises the X12-X17 binding-register pool");
    assert!(!out.blob.is_empty());
}

#[test]
fn bespoke_compiles_multi_binding_ssbo() {
    let spv = build_two_ssbo_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    // The bespoke compute path now distinguishes SSBO
    // bindings: X2 holds a descriptor table base, and the
    // emitted prologue pre-loads X16=tbl[0] (binding 0
    // pointer) and X17=tbl[1] (binding 1 pointer).
    // Subsequent SSBO accesses route through the per-binding
    // X-reg rather than aliasing X2.
    let out = compile_blob(&module, target)
        .expect("bespoke should compile a CS with 2 SSBO bindings -- \
                 exercises the multi-binding descriptor-table path");
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
