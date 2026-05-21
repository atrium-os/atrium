//! Cross-backend differential: compile the same compute
//! SPIR-V through bespoke + Cranelift, dlopen both, call
//! atrium_cs_main with identical inputs, assert outputs
//! match.
//!
//! Today bespoke and Cranelift each have their own
//! end-to-end test for multi-binding SSBO (bespoke via
//! atrium-vk-icd, Cranelift via dlopen).  Both should be
//! correct -- but nothing currently catches a *divergence*
//! where one backend produces a different value than the
//! other for the same shader.
//!
//! This test runs a small matrix of shader shapes through
//! both backends and diffs the per-binding output buffers.
//! Differences would surface a codegen drift between the
//! two paths (e.g. a Cranelift bug that the bespoke tests
//! don't see).

use atrium_spv_backend_bespoke::{compile as bespoke_compile, Target as BespokeTarget};
use atrium_spv_backend_cranelift::{compile as cranelift_compile, Target as CraneliftTarget};
use atrium_spv_frontend::translate;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn link_to_shared_library(obj: &Path, out: &Path) -> Result<(), String> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let o = Command::new("cc").arg(flag).arg("-o").arg(out).arg(obj)
        .output().map_err(|e| format!("cc: {e}"))?;
    if !o.status.success() {
        return Err(format!("cc failed: {}\n{}",
            o.status, String::from_utf8_lossy(&o.stderr)));
    }
    Ok(())
}

type CsMain = unsafe extern "C" fn(
    *const u8, *const u8, *mut u8,
    u32, u32, u32, u32, u32, u32);

/// Compile `spv` via `compile_fn`, link to .so in `dir`,
/// dlopen, then invoke atrium_cs_main with the given
/// `out_ptr` (which can be either a direct SSBO pointer or
/// a descriptor-table base, depending on the shader).
fn invoke(spv: &[u8], use_bespoke: bool, dir: &Path, name: &str,
          out_ptr: *mut u8) {
    invoke_with_gids(spv, use_bespoke, dir, name, out_ptr, &[(0, 0, 0)]);
}

/// Same as `invoke`, but calls `cs_main` once per entry in
/// `gids`, simulating the per-invocation dispatch loop the
/// host runs for real workgroups.  Each tuple is
/// `(gid_x, gid_y, gid_z)`; local-invocation lanes are kept
/// at 0 since these tests don't exercise lid math.
fn invoke_with_gids(spv: &[u8], use_bespoke: bool, dir: &Path, name: &str,
                    out_ptr: *mut u8, gids: &[(u32, u32, u32)]) {
    let module = translate(spv).expect("frontend");
    let obj = if use_bespoke {
        let t = if cfg!(target_os = "macos") {
            BespokeTarget::Aarch64Darwin
        } else { BespokeTarget::Aarch64FreeBSD };
        bespoke_compile(&module, t).expect("bespoke compile").object
    } else {
        cranelift_compile(&module, CraneliftTarget::host())
            .expect("cranelift compile").object
    };
    let obj_path = dir.join(format!("{name}.o"));
    std::fs::write(&obj_path, &obj).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.join(format!("{name}.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).expect("link");
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .expect("dlopen");
    let cs_main: libloading::Symbol<CsMain> = unsafe {
        lib.get(b"atrium_cs_main").expect("atrium_cs_main symbol")
    };
    for &(gx, gy, gz) in gids {
        unsafe {
            cs_main(
                std::ptr::null(), std::ptr::null(), out_ptr,
                gx, gy, gz, 0, 0, 0,
            );
        }
    }
}

/// Run `spv` through both backends with N output buffers,
/// returning `(bespoke_outputs, cranelift_outputs)`.
/// `n_bindings` >= 2 uses a descriptor-table calling
/// convention; n_bindings == 1 passes the buffer pointer
/// directly.
fn diff(spv: &[u8], n_bindings: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let dir = TempDir::new().unwrap();
    // Bespoke run.
    let mut b_bufs: Vec<Vec<u8>> = (0..n_bindings).map(|_| vec![0u8; 16]).collect();
    if n_bindings >= 2 {
        let table: Vec<u64> = b_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
        invoke(spv, true, dir.path(), "b", table.as_ptr() as *mut u8);
    } else {
        let p = b_bufs[0].as_mut_ptr();
        invoke(spv, true, dir.path(), "b", p);
    }
    // Cranelift run.
    let mut c_bufs: Vec<Vec<u8>> = (0..n_bindings).map(|_| vec![0u8; 16]).collect();
    if n_bindings >= 2 {
        let table: Vec<u64> = c_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
        invoke(spv, false, dir.path(), "c", table.as_ptr() as *mut u8);
    } else {
        let p = c_bufs[0].as_mut_ptr();
        invoke(spv, false, dir.path(), "c", p);
    }
    (b_bufs, c_bufs)
}

fn assert_equal(label: &str, b: &[Vec<u8>], c: &[Vec<u8>]) {
    assert_eq!(b.len(), c.len(),
        "{label}: binding count mismatch ({} vs {})", b.len(), c.len());
    for (i, (bb, cc)) in b.iter().zip(c.iter()).enumerate() {
        let bv = u32::from_le_bytes(bb[0..4].try_into().unwrap());
        let cv = u32::from_le_bytes(cc[0..4].try_into().unwrap());
        assert_eq!(bv, cv,
            "{label}: binding {i} diverges -- bespoke={bv:#x} cranelift={cv:#x}");
    }
}

// ── Shader builders ────────────────────────────────────────

/// CS with `n` SSBO bindings, each writing a distinct
/// constant (lo nibble = binding index).
fn build_n_binding_constants(n: u32) -> Vec<u8> {
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
    for i in 0..n {
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
    let consts: Vec<_> = (0..n)
        .map(|i| b.constant_bit32(u32_ty, 0xCAFE_0000 | i))
        .collect();
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
fn differential_one_binding_constant_store() {
    let spv = build_n_binding_constants(1);
    let (b, c) = diff(&spv, 1);
    assert_equal("one-binding", &b, &c);
    let v = u32::from_le_bytes(b[0][0..4].try_into().unwrap());
    assert_eq!(v, 0xCAFE_0000);
}

#[test]
fn differential_two_binding_constant_store() {
    let spv = build_n_binding_constants(2);
    let (b, c) = diff(&spv, 2);
    assert_equal("two-binding", &b, &c);
    let v0 = u32::from_le_bytes(b[0][0..4].try_into().unwrap());
    let v1 = u32::from_le_bytes(b[1][0..4].try_into().unwrap());
    assert_eq!(v0, 0xCAFE_0000);
    assert_eq!(v1, 0xCAFE_0001);
}

#[test]
fn differential_four_binding_constant_store() {
    let spv = build_n_binding_constants(4);
    let (b, c) = diff(&spv, 4);
    assert_equal("four-binding", &b, &c);
    for i in 0..4 {
        let v = u32::from_le_bytes(b[i][0..4].try_into().unwrap());
        assert_eq!(v, 0xCAFE_0000 | (i as u32));
    }
}

/// CS: `ssbo.data[gid_x] = gid_x`.  Driving this with
/// gid_x in 0..N writes the identity permutation into the
/// first N u32 slots.  Validates Op::PtrOffsetDynamic
/// produces matching results in both backends.
fn build_dyn_ssbo_identity() -> Vec<u8> {
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
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, gid_x]).unwrap();
    b.store(dst, gid_x, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_dynamic_ssbo_index_writes_per_lane() {
    let spv = build_dyn_ssbo_identity();
    let dir = TempDir::new().unwrap();
    // Allocate a 64-byte buffer (16 u32 slots) -- plenty
    // for 8 invocations.
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    let gids: Vec<(u32, u32, u32)> = (0..8).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf,
        "bespoke and cranelift diverge on dynamic SSBO index:\n  \
         bespoke   = {b_buf:?}\n  \
         cranelift = {c_buf:?}");
    // Sanity-check the actual values too.
    for i in 0..8usize {
        let v = u32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap());
        assert_eq!(v, i as u32,
            "slot {i} should hold {i}, got {v}");
    }
}

/// CS: `ssbo.data[gid_x + 4] = ssbo.data[gid_x] + 100`.
/// With a pre-filled input of [10, 20, 30, 40] and gid_x in
/// 0..4, both backends should produce identical buffers:
/// input slice unchanged, output slice = [110, 120, 130, 140].
/// Exercises Op::PtrOffsetDynamic on both the LOAD and STORE
/// sides plus IAdd on the dynamic index.
fn build_dyn_rmw_diff_cs() -> Vec<u8> {
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
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_four = b.constant_bit32(u32_ty, 4);
    let c_100  = b.constant_bit32(u32_ty, 100);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let src = b.access_chain(ptr_u, None, ssbo, vec![c_zero, gid_x]).unwrap();
    let v = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let v_plus = b.i_add(u32_ty, None, v, c_100).unwrap();
    let idx_out = b.i_add(u32_ty, None, gid_x, c_four).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, idx_out]).unwrap();
    b.store(dst, v_plus, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_dynamic_rmw_with_prefill() {
    let spv = build_dyn_rmw_diff_cs();
    let dir = TempDir::new().unwrap();
    let prefill: Vec<u8> = [10u32, 20, 30, 40]
        .iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    b_buf[..16].copy_from_slice(&prefill);
    c_buf[..16].copy_from_slice(&prefill);
    let gids: Vec<(u32, u32, u32)> = (0..4).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on dynamic RMW:\n  \
         bespoke   = {b_buf:?}\n  \
         cranelift = {c_buf:?}");
    // Sanity-check the actual computation result too.
    let expected_out = [110u32, 120, 130, 140];
    for i in 0..4 {
        let off = (i + 4) * 4;
        let v = u32::from_le_bytes(b_buf[off..off+4].try_into().unwrap());
        assert_eq!(v, expected_out[i],
            "slot {} should hold {}, got {v}", i + 4, expected_out[i]);
    }
}

/// CS combining multi-binding + dynamic index:
///   ssbo_out.data[gid_x] = ssbo_in.data[gid_x] * 2
/// Two SSBOs, both with RuntimeArray<u32>, both indexed by
/// the same gid_x.  Exercises the X16/X17 descriptor-table
/// prologue + Op::PtrOffsetDynamic on each binding +
/// IMul on the loaded value.
fn build_copy_double_two_ssbos_cs() -> Vec<u8> {
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
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let mk_block = |b: &mut rspirv::dr::Builder| {
        let s = b.type_struct(vec![rt_arr]);
        b.decorate(s, Decoration::Block, vec![]);
        b.member_decorate(s, 0, Decoration::Offset,
            vec![rspirv::dr::Operand::LiteralBit32(0)]);
        s
    };
    let s_in  = mk_block(&mut b);
    let s_out = mk_block(&mut b);
    let ptr_s_in  = b.type_pointer(None, StorageClass::StorageBuffer, s_in);
    let ptr_s_out = b.type_pointer(None, StorageClass::StorageBuffer, s_out);
    let ptr_u     = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let v_in  = b.variable(ptr_s_in,  None, StorageClass::StorageBuffer, None);
    b.decorate(v_in, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_in, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let v_out = b.variable(ptr_s_out, None, StorageClass::StorageBuffer, None);
    b.decorate(v_out, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_out, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_two  = b.constant_bit32(u32_ty, 2);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let src = b.access_chain(ptr_u, None, v_in,  vec![c_zero, gid_x]).unwrap();
    let val = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let doubled = b.i_mul(u32_ty, None, val, c_two).unwrap();
    let dst = b.access_chain(ptr_u, None, v_out, vec![c_zero, gid_x]).unwrap();
    b.store(dst, doubled, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, v_in, v_out]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_multi_binding_plus_dynamic_index() {
    let spv = build_copy_double_two_ssbos_cs();
    let dir = TempDir::new().unwrap();
    // Per-binding 64-byte buffers; pre-fill binding 0 with
    // a known sequence so the doubling is observable.
    let prefill: Vec<u8> = [7u32, 13, 21, 30]
        .iter().flat_map(|v| v.to_le_bytes()).collect();
    let make_bufs = || -> Vec<Vec<u8>> {
        let mut b0 = vec![0u8; 64]; b0[..16].copy_from_slice(&prefill);
        let b1 = vec![0u8; 64];
        vec![b0, b1]
    };
    let mut b_bufs = make_bufs();
    let mut c_bufs = make_bufs();
    let b_table: Vec<u64> = b_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
    let c_table: Vec<u64> = c_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
    let gids: Vec<(u32, u32, u32)> = (0..4).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b",
        b_table.as_ptr() as *mut u8, &gids);
    invoke_with_gids(&spv, false, dir.path(), "c",
        c_table.as_ptr() as *mut u8, &gids);
    assert_eq!(b_bufs, c_bufs,
        "bespoke vs cranelift diverge on multi-binding + dynamic:\n  \
         bespoke   in={:?} out={:?}\n  \
         cranelift in={:?} out={:?}",
        &b_bufs[0][..16], &b_bufs[1][..16],
        &c_bufs[0][..16], &c_bufs[1][..16]);
    // Sanity: binding 0 unchanged, binding 1 has doubled values.
    let in_expected  = [7u32, 13, 21, 30];
    let out_expected = [14u32, 26, 42, 60];
    for i in 0..4 {
        let in_v  = u32::from_le_bytes(b_bufs[0][i*4..i*4+4].try_into().unwrap());
        let out_v = u32::from_le_bytes(b_bufs[1][i*4..i*4+4].try_into().unwrap());
        assert_eq!(in_v,  in_expected[i],
            "binding 0 slot {i} should be {} (unchanged), got {in_v}", in_expected[i]);
        assert_eq!(out_v, out_expected[i],
            "binding 1 slot {i} should be {} (= in * 2), got {out_v}", out_expected[i]);
    }
}

/// CS: atomicAdd(ssbo.counter, gid_x).  Per-invocation
/// load-add-store sequence on the serial dispatcher --
/// invoking with gid_x in 0..N should leave the counter
/// at sum(0..N) = N*(N-1)/2.
fn build_atomic_counter_diff_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3 = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, dst, c_scope, c_sem, gid_x).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_atomic_iadd_accumulator() {
    let spv = build_atomic_counter_diff_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    let gids: Vec<(u32, u32, u32)> = (0..8).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on atomicAdd:\n  \
         bespoke   = {b_buf:?}\n  \
         cranelift = {c_buf:?}");
    let v = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, (0..8).sum(),
        "atomic counter should equal sum(0..8) = 28, got {v}");
}

/// Builds a CS with the named atomic opcode applied to
/// ssbo.counter with the per-invocation gid_x as the
/// addend.  `op` is one of: AtomicIAdd, AtomicAnd, AtomicOr,
/// AtomicXor, AtomicExchange.
fn build_atomic_op_cs(op: rspirv::spirv::Op) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, Op as SpvOp, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    let _ = match op {
        SpvOp::AtomicIAdd     => b.atomic_i_add  (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicAnd      => b.atomic_and    (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicOr       => b.atomic_or     (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicXor      => b.atomic_xor    (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicSMin     => b.atomic_s_min  (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicSMax     => b.atomic_s_max  (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicUMin     => b.atomic_u_min  (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicUMax     => b.atomic_u_max  (u32_ty, None, dst, c_scope, c_sem, gid_x),
        SpvOp::AtomicExchange => b.atomic_exchange(u32_ty, None, dst, c_scope, c_sem, gid_x),
        _ => panic!("unsupported test op {op:?}"),
    }.unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn run_atomic_diff(op: rspirv::spirv::Op, prefill: u32, gids_range: u32) -> u32 {
    let spv = build_atomic_op_cs(op);
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    b_buf[..4].copy_from_slice(&prefill.to_le_bytes());
    c_buf[..4].copy_from_slice(&prefill.to_le_bytes());
    let gids: Vec<(u32, u32, u32)> = (0..gids_range).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on {op:?}:\n  \
         bespoke   = {b_buf:?}\n  cranelift = {c_buf:?}");
    u32::from_le_bytes(b_buf[0..4].try_into().unwrap())
}

#[test]
fn differential_atomic_or_sets_bits_across_invocations() {
    // Pre-fill 0; gid_x in 0..4 sets bits 0..3 via atomicOr.
    use rspirv::spirv::Op as SpvOp;
    let got = run_atomic_diff(SpvOp::AtomicOr, 0, 4);
    // gid_x = 0 OR's 0 (no-op), 1 sets bit 0, 2 sets bit 1, 3 sets bits 0+1.
    // OR is idempotent + commutative: result = 0|0|1|2|3 = 3.
    assert_eq!(got, 0 | 0 | 1 | 2 | 3, "got {got:#x}");
}

#[test]
fn differential_atomic_and_clears_bits() {
    // Pre-fill 0x0F (= bits 0..3 set); gid_x in 0..4 AND-masks.
    // After: 0x0F & 0 & 1 & 2 & 3 = 0 (any AND with 0 clears all).
    use rspirv::spirv::Op as SpvOp;
    let got = run_atomic_diff(SpvOp::AtomicAnd, 0x0F, 4);
    assert_eq!(got, 0, "got {got:#x}");
}

#[test]
fn differential_atomic_xor_toggles() {
    // Pre-fill 0; gid_x in 1..5 XORs in 1,2,3,4 = 1^2^3^4 = 4.
    use rspirv::spirv::Op as SpvOp;
    let spv = build_atomic_op_cs(SpvOp::AtomicXor);
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    let gids: Vec<(u32, u32, u32)> = (1..5).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf);
    let got = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(got, 1u32 ^ 2 ^ 3 ^ 4, "got {got:#x}");
}

#[test]
fn differential_atomic_exchange_replaces() {
    // Pre-fill 0xDEAD; gid_x in 0..3 exchanges -> final = 2 (last write wins).
    use rspirv::spirv::Op as SpvOp;
    let got = run_atomic_diff(SpvOp::AtomicExchange, 0xDEAD, 3);
    assert_eq!(got, 2, "exchange should leave the last gid_x; got {got:#x}");
}

/// CS: `ssbo.b = atomicLoad(ssbo.a)` -- proves AtomicLoad
/// reads the prefill correctly and AtomicStore writes it.
/// Uses a struct { uint a; uint b; } so we can verify both
/// stay correct.
fn build_atomic_load_store_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty, u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    let v = b.atomic_load(u32_ty, None, src, c_scope, c_sem).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_one]).unwrap();
    b.atomic_store(dst, c_scope, c_sem, v).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_atomic_load_then_store() {
    let spv = build_atomic_load_store_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    // Pre-fill a = 0x12345678; b stays 0.
    b_buf[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    c_buf[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on atomicLoad/Store");
    let a = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    let b = u32::from_le_bytes(b_buf[4..8].try_into().unwrap());
    assert_eq!(a, 0x1234_5678, "a should still hold the prefill");
    assert_eq!(b, 0x1234_5678, "b should have received the loaded value");
}

/// CS doing CAS: if (ssbo.a == comparator) ssbo.a = desired.
/// Result is the old value.  Tests both the success and
/// failure paths by varying the prefill.
fn build_atomic_cas_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty, u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero    = b.constant_bit32(u32_ty, 0);
    let c_one     = b.constant_bit32(u32_ty, 1);
    let c_42      = b.constant_bit32(u32_ty, 42);
    let c_99      = b.constant_bit32(u32_ty, 99);
    let c_scope   = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem     = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst_a = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    // CAS(ssbo.a, comparator=42, desired=99) -> old value
    let old = b.atomic_compare_exchange(
        u32_ty, None, dst_a, c_scope, c_sem, c_sem, c_99, c_42).unwrap();
    // ssbo.b = old value returned by CAS
    let dst_b = b.access_chain(ptr_u, None, ssbo, vec![c_one]).unwrap();
    b.store(dst_b, old, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_atomic_cas_success_path() {
    // Prefill ssbo.a = 42 (matches comparator).
    // Expect: ssbo.a = 99 (swap succeeded), ssbo.b = 42 (old).
    let spv = build_atomic_cas_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    b_buf[0..4].copy_from_slice(&42u32.to_le_bytes());
    c_buf[0..4].copy_from_slice(&42u32.to_le_bytes());
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on CAS success");
    assert_eq!(u32::from_le_bytes(b_buf[0..4].try_into().unwrap()), 99,
        "CAS success should write desired (99)");
    assert_eq!(u32::from_le_bytes(b_buf[4..8].try_into().unwrap()), 42,
        "CAS should return old value (42)");
}

#[test]
fn differential_atomic_cas_failure_path() {
    // Prefill ssbo.a = 7 (does NOT match comparator=42).
    // Expect: ssbo.a unchanged at 7, ssbo.b = 7 (returned old).
    let spv = build_atomic_cas_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    b_buf[0..4].copy_from_slice(&7u32.to_le_bytes());
    c_buf[0..4].copy_from_slice(&7u32.to_le_bytes());
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on CAS failure");
    assert_eq!(u32::from_le_bytes(b_buf[0..4].try_into().unwrap()), 7,
        "CAS failure should leave value unchanged");
    assert_eq!(u32::from_le_bytes(b_buf[4..8].try_into().unwrap()), 7,
        "CAS should return the old value even on failure");
}

/// CS: `atomicIncrement(ssbo.counter)` per invocation.
/// Frontend lowers to AtomicIAdd with synth +1; here we
/// verify the synthesis actually produces +1 (not 0 or -1).
fn build_atomic_increment_cs(decrement: bool) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    let _ = if decrement {
        b.atomic_i_decrement(u32_ty, None, dst, c_scope, c_sem)
    } else {
        b.atomic_i_increment(u32_ty, None, dst, c_scope, c_sem)
    }.unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_atomic_iincrement_counts_invocations() {
    // Prefill 0; 5 invocations of atomicIncrement -> counter = 5.
    let spv = build_atomic_increment_cs(false);
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    let gids: Vec<(u32, u32, u32)> = (0..5).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf);
    let v = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, 5, "5 increments should leave counter at 5; got {v}");
}

#[test]
fn differential_atomic_idecrement_counts_down() {
    // Prefill 10; 4 invocations of atomicDecrement -> counter = 6.
    let spv = build_atomic_increment_cs(true);
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    b_buf[0..4].copy_from_slice(&10u32.to_le_bytes());
    c_buf[0..4].copy_from_slice(&10u32.to_le_bytes());
    let gids: Vec<(u32, u32, u32)> = (0..4).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf);
    let v = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, 6, "10 - 4 decrements should leave 6; got {v}");
}

#[test]
fn differential_atomic_isub_subtracts() {
    // Same scaffolding as iadd but with OpAtomicISub.  Prefill 100;
    // gid_x in 1..5 subtracts 1,2,3,4 -> result = 100-10 = 90.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3 = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    let _ = b.atomic_i_sub(u32_ty, None, dst, c_scope, c_sem, gid_x).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    b_buf[0..4].copy_from_slice(&100u32.to_le_bytes());
    c_buf[0..4].copy_from_slice(&100u32.to_le_bytes());
    let gids: Vec<(u32, u32, u32)> = (1..5).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf, "diverge on isub");
    let v = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, 100 - (1 + 2 + 3 + 4),
        "100 - sum(1..4) should be 90; got {v}");
}

#[test]
fn differential_atomic_umin_finds_smallest() {
    // Prefill 100; gid_x in 0..6 atomicMin's it.  Result =
    // min(100, 0, 1, 2, 3, 4, 5) = 0.
    use rspirv::spirv::Op as SpvOp;
    let got = run_atomic_diff(SpvOp::AtomicUMin, 100, 6);
    assert_eq!(got, 0, "umin should reduce to 0; got {got}");
}

#[test]
fn differential_atomic_umax_finds_largest() {
    // Prefill 0; gid_x in 0..6 atomicMax's it.  Result =
    // max(0, 0, 1, 2, 3, 4, 5) = 5.
    use rspirv::spirv::Op as SpvOp;
    let got = run_atomic_diff(SpvOp::AtomicUMax, 0, 6);
    assert_eq!(got, 5, "umax should reduce to 5; got {got}");
}

#[test]
fn differential_atomic_smin_signed_negatives() {
    // Prefill 0x8000_0000 (= -2^31, smallest signed); gid_x
    // in 0..4 SMin's it -- those are tiny POSITIVE values
    // as signed, so the prefill stays the minimum.
    use rspirv::spirv::Op as SpvOp;
    let got = run_atomic_diff(SpvOp::AtomicSMin, 0x8000_0000, 4);
    assert_eq!(got, 0x8000_0000,
        "smin should keep the most-negative prefill; got {got:#x}");
}

#[test]
fn differential_atomic_smax_keeps_positive_over_negative() {
    // Prefill 0xFFFF_FFFF (= -1 signed); gid_x in 1..5 are
    // 1..4 (positive signed).  SMax should bump the value
    // up to 4.
    use rspirv::spirv::Op as SpvOp;
    let spv = build_atomic_op_cs(SpvOp::AtomicSMax);
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    b_buf[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    c_buf[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let gids: Vec<(u32, u32, u32)> = (1..5).map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf, "diverge on smax");
    let got = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(got, 4, "smax should leave 4; got {got}");
}

/// Real-world integration: build a histogram by reading
/// a sample from one SSBO and atomicAdd'ing into another
/// using a dynamically-computed bucket.  This is the
/// canonical shape that motivated the entire atomics-on-
/// dynamic-index work.
///
/// Uses TWO SSBOs (in + out).  Note: today this hits a
/// known bespoke W-reg pressure cliff (the integration
/// allocates more live ints than the IntPool can satisfy
/// without spilling, and spilling isn't implemented).  The
/// vk-icd histogram test still works because the production
/// selector falls back to Cranelift on Unsupported -- this
/// dlopen-direct test exists to flag the bespoke regression
/// when spilling lands.
#[allow(dead_code)]
fn build_histogram_diff_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s_in  = b.type_struct(vec![rt_arr]);
    b.decorate(s_in, Decoration::Block, vec![]);
    b.member_decorate(s_in, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let s_out = b.type_struct(vec![rt_arr]);
    b.decorate(s_out, Decoration::Block, vec![]);
    b.member_decorate(s_out, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s_in  = b.type_pointer(None, StorageClass::StorageBuffer, s_in);
    let ptr_s_out = b.type_pointer(None, StorageClass::StorageBuffer, s_out);
    let ptr_u     = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let v_in  = b.variable(ptr_s_in,  None, StorageClass::StorageBuffer, None);
    b.decorate(v_in, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_in, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let v_out = b.variable(ptr_s_out, None, StorageClass::StorageBuffer, None);
    b.decorate(v_out, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_out, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_three = b.constant_bit32(u32_ty, 3);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let in_ptr = b.access_chain(ptr_u, None, v_in, vec![c_zero, gid_x]).unwrap();
    let sample = b.load(u32_ty, None, in_ptr, None, vec![]).unwrap();
    let bucket = b.bitwise_and(u32_ty, None, sample, c_three).unwrap();
    let out_ptr = b.access_chain(ptr_u, None, v_out, vec![c_zero, bucket]).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, out_ptr, c_scope, c_sem, c_one).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, v_in, v_out]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Single-SSBO histogram-shaped CS: read from
/// ssbo.data[gid_x + 8] (the sample region), compute bucket
/// = sample & 3, atomicAdd 1 into ssbo.data[bucket] (the
/// bin region in the same buffer).  Bespoke-friendly
/// because it only uses one binding (single X2-direct
/// pointer, no descriptor table prologue).  Validates
/// every dynamic-index + atomic + bitwise interaction
/// without tripping bespoke's W-reg cliff.
fn build_histogram_one_ssbo_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_eight = b.constant_bit32(u32_ty, 8);
    let c_three = b.constant_bit32(u32_ty, 3);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    // sample = ssbo.data[gid_x + 8]
    let sample_idx = b.i_add(u32_ty, None, gid_x, c_eight).unwrap();
    let in_ptr = b.access_chain(ptr_u, None, ssbo, vec![c_zero, sample_idx]).unwrap();
    let sample = b.load(u32_ty, None, in_ptr, None, vec![]).unwrap();
    let bucket = b.bitwise_and(u32_ty, None, sample, c_three).unwrap();
    // atomicAdd(ssbo.data[bucket], 1)
    let out_ptr = b.access_chain(ptr_u, None, ssbo, vec![c_zero, bucket]).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, out_ptr, c_scope, c_sem, c_one).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// CS: `ssbo.data[gl_LocalInvocationIndex] = gl_LocalInvocationIndex`.
/// With LocalSize=(4,2,1), a single workgroup produces 8
/// invocations whose indices linearise as
///   ly=0,lx=0..3 -> 0..3
///   ly=1,lx=0..3 -> 4..7
/// so the buffer should hold [0..7].
fn build_local_index_cs() -> Vec<u8> {
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
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_u32 = b.type_pointer(None, StorageClass::Input, u32_ty);
    let li_var = b.variable(ptr_in_u32, None, StorageClass::Input, None);
    b.decorate(li_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::LocalInvocationIndex)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let li = b.load(u32_ty, None, li_var, None, vec![]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, li]).unwrap();
    b.store(dst, li, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![li_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 2, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// CS: writes the three components of gl_WorkGroupSize
/// into ssbo.data[0..3].  Verifies both backends materialise
/// the LocalSize constants in matching order.
fn build_workgroup_size_cs() -> Vec<u8> {
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
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let ws_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(ws_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::WorkgroupSize)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let ws = b.load(uvec3, None, ws_var, None, vec![]).unwrap();
    for i in 0..3u32 {
        let c_i = b.constant_bit32(u32_ty, i);
        let lane = b.composite_extract(u32_ty, None, ws, vec![i]).unwrap();
        let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_i]).unwrap();
        b.store(dst, lane, None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ws_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [7u32, 5, 3]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_workgroup_size_materialises_localsize() {
    let spv = build_workgroup_size_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on gl_WorkGroupSize");
    // LocalSize=(7,5,3) -> slots 0,1,2 should hold 7,5,3.
    let got: Vec<u32> = (0..3)
        .map(|i| u32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap()))
        .collect();
    assert_eq!(got, vec![7, 5, 3], "got {got:?}");
}

/// CS: writes [fabs(x), sqrt(y), fmin(x,y), fmax(x,y)]
/// where x = -2.5 and y = 4.0 (both as constants).
/// Exercises the GLSL.std.450 dispatch path for the four
/// scalar f32 math ops that just landed.
fn build_glsl_math_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let cx = b.constant_bit32(f32_ty, (-2.5f32).to_bits());
    let cy = b.constant_bit32(f32_ty, 4.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let abs_x  = b.ext_inst(f32_ty, None, std_450, 4,  vec![rspirv::dr::Operand::IdRef(cx)]).unwrap();
    let sqrt_y = b.ext_inst(f32_ty, None, std_450, 31, vec![rspirv::dr::Operand::IdRef(cy)]).unwrap();
    let min_xy = b.ext_inst(f32_ty, None, std_450, 37,
        vec![rspirv::dr::Operand::IdRef(cx), rspirv::dr::Operand::IdRef(cy)]).unwrap();
    let max_xy = b.ext_inst(f32_ty, None, std_450, 40,
        vec![rspirv::dr::Operand::IdRef(cx), rspirv::dr::Operand::IdRef(cy)]).unwrap();
    let values = [abs_x, sqrt_y, min_xy, max_xy];
    for (i, v) in values.iter().enumerate() {
        let c_i = b.constant_bit32(u32_ty, i as u32);
        let dst = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_i]).unwrap();
        b.store(dst, *v, None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_glsl_std_450_scalar_math() {
    let spv = build_glsl_math_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on GLSL.std.450 scalar math");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(0), 2.5,  "fabs(-2.5) should be 2.5");
    assert_eq!(read(1), 2.0,  "sqrt(4.0) should be 2.0");
    assert_eq!(read(2), -2.5, "fmin(-2.5, 4.0) should be -2.5");
    assert_eq!(read(3), 4.0,  "fmax(-2.5, 4.0) should be 4.0");
}

/// CS: writes [clamp(x, 0.0, 1.0), mix(0.0, 100.0, t)]
/// where x = 1.5 and t = 0.25.  Verifies the frontend's
/// clamp + mix synthesis works through both backends.
fn build_glsl_clamp_mix_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let cx   = b.constant_bit32(f32_ty, 1.5f32.to_bits());
    let clo  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let chi  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let cy   = b.constant_bit32(f32_ty, 100.0f32.to_bits());
    let ct   = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let cl = b.ext_inst(f32_ty, None, std_450, 43,
        vec![rspirv::dr::Operand::IdRef(cx),
             rspirv::dr::Operand::IdRef(clo),
             rspirv::dr::Operand::IdRef(chi)]).unwrap();
    let mx = b.ext_inst(f32_ty, None, std_450, 46,
        vec![rspirv::dr::Operand::IdRef(clo),
             rspirv::dr::Operand::IdRef(cy),
             rspirv::dr::Operand::IdRef(ct)]).unwrap();
    let c0 = b.constant_bit32(u32_ty, 0);
    let c1 = b.constant_bit32(u32_ty, 1);
    let d0 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c0]).unwrap();
    b.store(d0, cl, None, vec![]).unwrap();
    let d1 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c1]).unwrap();
    b.store(d1, mx, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// vec4 GLSL math: ssbo[0..4] = fabs(vec4(-1, -2, 3, -4));
///                  ssbo[4..8] = fmin(a, b) over two vec4 constants.
fn build_glsl_vec4_math_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(16)]);
    let ptr_s   = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v   = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo    = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let mk_vec = |b: &mut rspirv::dr::Builder, vs: [f32; 4]| {
        let lanes: Vec<_> = vs.iter()
            .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        b.constant_composite(vec4, lanes)
    };
    let c_v_in_abs = mk_vec(&mut b, [-1.0, -2.0, 3.0, -4.0]);
    let c_a = mk_vec(&mut b, [1.0, 5.0, 3.0, 7.0]);
    let c_b = mk_vec(&mut b, [4.0, 2.0, 6.0, 0.5]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let abs_v = b.ext_inst(vec4, None, std_450, 4,
        vec![rspirv::dr::Operand::IdRef(c_v_in_abs)]).unwrap();
    let min_v = b.ext_inst(vec4, None, std_450, 37,
        vec![rspirv::dr::Operand::IdRef(c_a),
             rspirv::dr::Operand::IdRef(c_b)]).unwrap();
    let d0 = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    b.store(d0, abs_v, None, vec![]).unwrap();
    let d1 = b.access_chain(ptr_v, None, ssbo, vec![c_one]).unwrap();
    b.store(d1, min_v, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_glsl_vec4_math() {
    let spv = build_glsl_vec4_math_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on vec4 GLSL math");
    let read = |off: usize| -> f32 {
        f32::from_le_bytes(b_buf[off..off+4].try_into().unwrap())
    };
    // fabs(vec4(-1, -2, 3, -4)) = [1, 2, 3, 4]
    assert_eq!([read(0), read(4), read(8), read(12)], [1.0, 2.0, 3.0, 4.0]);
    // fmin(vec4(1,5,3,7), vec4(4,2,6,0.5)) = [1, 2, 3, 0.5]
    assert_eq!([read(16), read(20), read(24), read(28)], [1.0, 2.0, 3.0, 0.5]);
}

/// CS: writes [floor(3.7), ceil(3.2), trunc(-2.7)] into
/// ssbo.data[0..3].  Exercises the three new FRINT*-based
/// GLSL.std.450 ops.
fn build_glsl_floor_ceil_trunc_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_a = b.constant_bit32(f32_ty,   3.7f32.to_bits());
    let c_b = b.constant_bit32(f32_ty,   3.2f32.to_bits());
    let c_c = b.constant_bit32(f32_ty, (-2.7f32).to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let floor_a = b.ext_inst(f32_ty, None, std_450, 8,
        vec![rspirv::dr::Operand::IdRef(c_a)]).unwrap();
    let ceil_b  = b.ext_inst(f32_ty, None, std_450, 9,
        vec![rspirv::dr::Operand::IdRef(c_b)]).unwrap();
    let trunc_c = b.ext_inst(f32_ty, None, std_450, 3,
        vec![rspirv::dr::Operand::IdRef(c_c)]).unwrap();
    let vs = [floor_a, ceil_b, trunc_c];
    for (i, v) in vs.iter().enumerate() {
        let ci = b.constant_bit32(u32_ty, i as u32);
        let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, ci]).unwrap();
        b.store(d, *v, None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_glsl_smoothstep() {
    // smoothstep(0, 1, 0.5) = 0.5 (mid-point of the S-curve).
    // smoothstep(0, 1, 0.0) = 0.
    // smoothstep(0, 1, 1.0) = 1.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_0 = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_1 = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let c_05 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let r = b.ext_inst(f32_ty, None, std_450, 49,
        vec![rspirv::dr::Operand::IdRef(c_0),
             rspirv::dr::Operand::IdRef(c_1),
             rspirv::dr::Operand::IdRef(c_05)]).unwrap();
    let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on smoothstep");
    let v = f32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, 0.5, "smoothstep(0,1,0.5) = 0.5; got {v}");
}

#[test]
fn differential_glsl_sign_and_step() {
    // sign(-2.5) = -1, sign(0) = 0, sign(3) = 1
    // step(0.5, 0.3) = 0, step(0.5, 0.7) = 1
    // The 3-bool W-pool would normally exhaust here; works
    // because ConvertUToF eagerly frees its bool input
    // after consumption.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_neg2_5 = b.constant_bit32(f32_ty, (-2.5f32).to_bits());
    let c_zero_f = b.constant_bit32(f32_ty,  0.0f32.to_bits());
    let c_three  = b.constant_bit32(f32_ty,  3.0f32.to_bits());
    let c_half   = b.constant_bit32(f32_ty,  0.5f32.to_bits());
    let c_0_3    = b.constant_bit32(f32_ty,  0.3f32.to_bits());
    let c_0_7    = b.constant_bit32(f32_ty,  0.7f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let s_neg = b.ext_inst(f32_ty, None, std_450, 6,
        vec![rspirv::dr::Operand::IdRef(c_neg2_5)]).unwrap();
    let s_zero = b.ext_inst(f32_ty, None, std_450, 6,
        vec![rspirv::dr::Operand::IdRef(c_zero_f)]).unwrap();
    let s_pos = b.ext_inst(f32_ty, None, std_450, 6,
        vec![rspirv::dr::Operand::IdRef(c_three)]).unwrap();
    let step_lo = b.ext_inst(f32_ty, None, std_450, 48,
        vec![rspirv::dr::Operand::IdRef(c_half),
             rspirv::dr::Operand::IdRef(c_0_3)]).unwrap();
    let step_hi = b.ext_inst(f32_ty, None, std_450, 48,
        vec![rspirv::dr::Operand::IdRef(c_half),
             rspirv::dr::Operand::IdRef(c_0_7)]).unwrap();
    let vs = [s_neg, s_zero, s_pos, step_lo, step_hi];
    for (i, v) in vs.iter().enumerate() {
        let ci = b.constant_bit32(u32_ty, i as u32);
        let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, ci]).unwrap();
        b.store(d, *v, None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 32];
    let mut c_buf = vec![0u8; 32];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on sign/step");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(0), -1.0, "sign(-2.5) = -1");
    assert_eq!(read(1),  0.0, "sign(0) = 0");
    assert_eq!(read(2),  1.0, "sign(3) = 1");
    assert_eq!(read(3),  0.0, "step(0.5, 0.3) = 0");
    assert_eq!(read(4),  1.0, "step(0.5, 0.7) = 1");
}

#[test]
fn differential_glsl_reflect() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let mk_vec = |b: &mut rspirv::dr::Builder, v: [f32; 4]| {
        let ls: Vec<_> = v.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        b.constant_composite(vec4, ls)
    };
    // I = (1, -1, 0, 0), N = (0, 1, 0, 0) -- light hitting a horizontal surface.
    // dot(N, I) = -1.
    // reflect = I - 2*(-1)*N = (1, -1, 0, 0) + 2*(0, 1, 0, 0) = (1, 1, 0, 0).
    let i_v = mk_vec(&mut b, [1.0, -1.0, 0.0, 0.0]);
    let n_v = mk_vec(&mut b, [0.0,  1.0, 0.0, 0.0]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let r = b.ext_inst(vec4, None, std_450, 71,
        vec![rspirv::dr::Operand::IdRef(i_v),
             rspirv::dr::Operand::IdRef(n_v)]).unwrap();
    let d = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    b.store(d, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on reflect");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!([read(0), read(1), read(2), read(3)],
        [1.0, 1.0, 0.0, 0.0],
        "reflect((1,-1,0,0), (0,1,0,0)) = (1,1,0,0)");
}

#[test]
fn differential_glsl_normalize() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let mk_vec = |b: &mut rspirv::dr::Builder, v: [f32; 4]| {
        let ls: Vec<_> = v.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        b.constant_composite(vec4, ls)
    };
    // normalize((3,4,0,0)) should give (0.6, 0.8, 0, 0) since length = 5.
    let v_in = mk_vec(&mut b, [3.0, 4.0, 0.0, 0.0]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let n = b.ext_inst(vec4, None, std_450, 69,
        vec![rspirv::dr::Operand::IdRef(v_in)]).unwrap();
    let d = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    b.store(d, n, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on normalize");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(0), 0.6);
    assert_eq!(read(1), 0.8);
    assert_eq!(read(2), 0.0);
    assert_eq!(read(3), 0.0);
}

#[test]
fn differential_glsl_length_and_distance() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let mk_vec = |b: &mut rspirv::dr::Builder, v: [f32; 4]| {
        let ls: Vec<_> = v.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        b.constant_composite(vec4, ls)
    };
    // length(vec4(3,4,0,0)) = 5
    let v1 = mk_vec(&mut b, [3.0, 4.0, 0.0, 0.0]);
    // distance(vec4(0), vec4(0,0,3,4)) = 5
    let v0 = mk_vec(&mut b, [0.0, 0.0, 0.0, 0.0]);
    let v2 = mk_vec(&mut b, [0.0, 0.0, 3.0, 4.0]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let len_v = b.ext_inst(f32_ty, None, std_450, 66,
        vec![rspirv::dr::Operand::IdRef(v1)]).unwrap();
    let dist_v = b.ext_inst(f32_ty, None, std_450, 67,
        vec![rspirv::dr::Operand::IdRef(v0),
             rspirv::dr::Operand::IdRef(v2)]).unwrap();
    let d0 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d0, len_v, None, vec![]).unwrap();
    let d1 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_one]).unwrap();
    b.store(d1, dist_v, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on length/distance");
    let len = f32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    let dst = f32::from_le_bytes(b_buf[4..8].try_into().unwrap());
    assert_eq!(len, 5.0, "length(3,4,0,0) = 5");
    assert_eq!(dst, 5.0, "distance(0, (0,0,3,4)) = 5");
}

#[test]
fn differential_glsl_inversesqrt() {
    // inverseSqrt(4.0) = 1/2 = 0.5.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_x = b.constant_bit32(f32_ty, 4.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let r = b.ext_inst(f32_ty, None, std_450, 32,
        vec![rspirv::dr::Operand::IdRef(c_x)]).unwrap();
    let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on inversesqrt");
    let v = f32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, 0.5, "1/sqrt(4) = 0.5; got {v}");
}

#[test]
fn differential_glsl_fmod() {
    // fmod(5.7, 2.0) = 5.7 - 2.0 * floor(5.7/2.0)
    //                = 5.7 - 2.0 * 2  =  1.7
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_x = b.constant_bit32(f32_ty, 5.7f32.to_bits());
    let c_y = b.constant_bit32(f32_ty, 2.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let r = b.ext_inst(f32_ty, None, std_450, 35,
        vec![rspirv::dr::Operand::IdRef(c_x),
             rspirv::dr::Operand::IdRef(c_y)]).unwrap();
    let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on fmod");
    let v = f32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert!((v - 1.7).abs() < 1e-5, "fmod(5.7, 2.0) ≈ 1.7; got {v}");
}

#[test]
fn differential_glsl_fract() {
    // fract(x) ≡ x - floor(x).
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(f32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_x = b.constant_bit32(f32_ty, 3.75f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fr = b.ext_inst(f32_ty, None, std_450, 10,
        vec![rspirv::dr::Operand::IdRef(c_x)]).unwrap();
    let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d, fr, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf);
    let v = f32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    assert_eq!(v, 0.75, "fract(3.75) = 0.75; got {v}");
}

/// vec4 floor / ceil / trunc -- single NEON .4S instruction
/// when both operand and result are packed.
fn build_glsl_vec4_round_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(16)]);
    b.member_decorate(s, 2, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(32)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c0 = b.constant_bit32(u32_ty, 0);
    let c1 = b.constant_bit32(u32_ty, 1);
    let c2 = b.constant_bit32(u32_ty, 2);
    let mk_vec = |b: &mut rspirv::dr::Builder, v: [f32; 4]| {
        let ls: Vec<_> = v.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        b.constant_composite(vec4, ls)
    };
    let v_in = mk_vec(&mut b, [3.7, -2.3, 0.5, -0.5]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fl = b.ext_inst(vec4, None, std_450, 8,
        vec![rspirv::dr::Operand::IdRef(v_in)]).unwrap();
    let ce = b.ext_inst(vec4, None, std_450, 9,
        vec![rspirv::dr::Operand::IdRef(v_in)]).unwrap();
    let tr = b.ext_inst(vec4, None, std_450, 3,
        vec![rspirv::dr::Operand::IdRef(v_in)]).unwrap();
    let d0 = b.access_chain(ptr_v, None, ssbo, vec![c0]).unwrap();
    b.store(d0, fl, None, vec![]).unwrap();
    let d1 = b.access_chain(ptr_v, None, ssbo, vec![c1]).unwrap();
    b.store(d1, ce, None, vec![]).unwrap();
    let d2 = b.access_chain(ptr_v, None, ssbo, vec![c2]).unwrap();
    b.store(d2, tr, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_glsl_vec4_floor_ceil_trunc() {
    let spv = build_glsl_vec4_round_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on vec4 floor/ceil/trunc");
    let read = |off: usize| -> f32 {
        f32::from_le_bytes(b_buf[off..off+4].try_into().unwrap())
    };
    // floor((3.7, -2.3, 0.5, -0.5)) -> (3, -3, 0, -1)
    assert_eq!([read(0), read(4), read(8), read(12)], [3.0, -3.0, 0.0, -1.0]);
    // ceil  -> (4, -2, 1, 0)
    assert_eq!([read(16), read(20), read(24), read(28)], [4.0, -2.0, 1.0, 0.0]);
    // trunc -> (3, -2, 0, 0)
    assert_eq!([read(32), read(36), read(40), read(44)], [3.0, -2.0, 0.0, 0.0]);
}

#[test]
fn differential_glsl_floor_ceil_trunc() {
    let spv = build_glsl_floor_ceil_trunc_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on floor/ceil/trunc");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(0),  3.0, "floor(3.7) = 3.0");
    assert_eq!(read(1),  4.0, "ceil(3.2)  = 4.0");
    assert_eq!(read(2), -2.0, "trunc(-2.7) = -2.0");
}

#[test]
fn differential_glsl_clamp_and_mix() {
    let spv = build_glsl_clamp_mix_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on clamp/mix");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(0), 1.0, "clamp(1.5, 0, 1) should be 1.0");
    assert_eq!(read(1), 25.0, "mix(0, 100, 0.25) should be 25.0");
}

/// Per-pixel tonemap shader.  Reads an HDR color from the
/// input SSBO, applies a simple Reinhard tonemap + sqrt
/// gamma + clamp(0,1), writes to the output SSBO.  The
/// math:
///
///   c       = ssbo_in.data[gid_x]                   (vec4)
///   mapped  = c / (c + vec4(1))                     (Reinhard)
///   gamma   = sqrt(mapped)                          (~gamma 2)
///   clamped = clamp(gamma, vec4(0), vec4(1))
///   ssbo_out.data[gid_x] = clamped
///
/// This is a real-graphics shape: vec4 fdiv + fsqrt +
/// clamp via fmax/fmin, all on the NEON-packed path.
fn build_tonemap_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let uvec3  = b.type_vector(u32_ty, 3);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let rt_vec4 = b.type_runtime_array(vec4);
    b.decorate(rt_vec4, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(16)]);
    let s_in  = b.type_struct(vec![rt_vec4]);
    b.decorate(s_in, Decoration::Block, vec![]);
    b.member_decorate(s_in, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let s_out = b.type_struct(vec![rt_vec4]);
    b.decorate(s_out, Decoration::Block, vec![]);
    b.member_decorate(s_out, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s_in  = b.type_pointer(None, StorageClass::StorageBuffer, s_in);
    let ptr_s_out = b.type_pointer(None, StorageClass::StorageBuffer, s_out);
    let ptr_v     = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let v_in  = b.variable(ptr_s_in,  None, StorageClass::StorageBuffer, None);
    b.decorate(v_in, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_in, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let v_out = b.variable(ptr_s_out, None, StorageClass::StorageBuffer, None);
    b.decorate(v_out, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_out, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let mk_vec = |b: &mut rspirv::dr::Builder, x: f32| {
        let l = b.constant_bit32(f32_ty, x.to_bits());
        b.constant_composite(vec4, vec![l, l, l, l])
    };
    let v_one  = mk_vec(&mut b, 1.0);
    let v_zero = mk_vec(&mut b, 0.0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let in_ptr = b.access_chain(ptr_v, None, v_in, vec![c_zero, gid_x]).unwrap();
    let c   = b.load(vec4, None, in_ptr, None, vec![]).unwrap();
    let denom = b.f_add(vec4, None, c, v_one).unwrap();
    let mapped = b.f_div(vec4, None, c, denom).unwrap();
    let gamma  = b.ext_inst(vec4, None, std_450, 31,
        vec![rspirv::dr::Operand::IdRef(mapped)]).unwrap();
    let clamped = b.ext_inst(vec4, None, std_450, 43,
        vec![rspirv::dr::Operand::IdRef(gamma),
             rspirv::dr::Operand::IdRef(v_zero),
             rspirv::dr::Operand::IdRef(v_one)]).unwrap();
    let out_ptr = b.access_chain(ptr_v, None, v_out, vec![c_zero, gid_x]).unwrap();
    b.store(out_ptr, clamped, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, v_in, v_out]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Minimal load+fsqrt+store test to surface the per-lane
/// vec4 unop bug independently of the rest of the tonemap
/// pipeline.
fn build_load_sqrt_store_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let uvec3  = b.type_vector(u32_ty, 3);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(16)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let _gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let in_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    let v = b.load(vec4, None, in_ptr, None, vec![]).unwrap();
    let r = b.ext_inst(vec4, None, std_450, 31,
        vec![rspirv::dr::Operand::IdRef(v)]).unwrap();
    let out_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_one]).unwrap();
    b.store(out_ptr, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn differential_load_sqrt_store_vec4() {
    let spv = build_load_sqrt_store_cs();
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    for (j, x) in [4.0f32, 9.0, 16.0, 25.0].iter().enumerate() {
        b_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
        c_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
    }
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on load+sqrt+store vec4");
    let read = |buf: &[u8], i: usize| -> f32 {
        f32::from_le_bytes(buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(&b_buf, 4), 2.0, "sqrt(4) should be 2");
    assert_eq!(read(&b_buf, 5), 3.0);
    assert_eq!(read(&b_buf, 6), 4.0);
    assert_eq!(read(&b_buf, 7), 5.0);
}

/// FMax with TWO loaded vec4s (no constant) -- isolates
/// whether the bug is in const-vec broadcast vs in
/// per-lane FMax itself.
#[test]
fn differential_load_fmax_loaded_store_vec4() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let uvec3  = b.type_vector(u32_ty, 3);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(16)]);
    b.member_decorate(s, 2, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(32)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c0 = b.constant_bit32(u32_ty, 0);
    let c1 = b.constant_bit32(u32_ty, 1);
    let c2 = b.constant_bit32(u32_ty, 2);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let _gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let a_ptr = b.access_chain(ptr_v, None, ssbo, vec![c0]).unwrap();
    let av = b.load(vec4, None, a_ptr, None, vec![]).unwrap();
    let b_ptr = b.access_chain(ptr_v, None, ssbo, vec![c1]).unwrap();
    let bv = b.load(vec4, None, b_ptr, None, vec![]).unwrap();
    let r = b.ext_inst(vec4, None, std_450, 40,
        vec![rspirv::dr::Operand::IdRef(av),
             rspirv::dr::Operand::IdRef(bv)]).unwrap();
    let out_ptr = b.access_chain(ptr_v, None, ssbo, vec![c2]).unwrap();
    b.store(out_ptr, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 80];
    let mut c_buf = vec![0u8; 80];
    for (j, x) in [-2.0f32, 5.0, -1.0, 8.0].iter().enumerate() {
        b_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
        c_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
    }
    for (j, x) in [3.0f32, 2.0, -3.0, 4.0].iter().enumerate() {
        b_buf[16+j*4..16+j*4+4].copy_from_slice(&x.to_le_bytes());
        c_buf[16+j*4..16+j*4+4].copy_from_slice(&x.to_le_bytes());
    }
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on fmax(loaded, loaded)");
}

/// Same shape but with FAdd instead of FMax -- tests
/// whether this is a pre-existing bespoke per-lane bug or
/// specific to FMax/FMin.
#[test]
fn differential_load_fadd_const_store_vec4() {
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
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(16)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let mk_vec = |b: &mut rspirv::dr::Builder, x: f32| {
        let l = b.constant_bit32(f32_ty, x.to_bits());
        b.constant_composite(vec4, vec![l, l, l, l])
    };
    let v_ten = mk_vec(&mut b, 10.0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let _gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let in_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    let c = b.load(vec4, None, in_ptr, None, vec![]).unwrap();
    let r = b.f_add(vec4, None, c, v_ten).unwrap();
    let out_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_one]).unwrap();
    b.store(out_ptr, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    for (j, x) in [1.0f32, 2.0, 3.0, 4.0].iter().enumerate() {
        b_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
        c_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
    }
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on load + fadd(loaded, const_vec) + store");
}

/// Direct FMax-of-vectors variant -- known to diverge.
/// The bespoke per-lane FMax/FMin path produces wrong
/// results for some lanes when one operand is a loaded
/// vec4 and the other is a (disqualified) const vec4.
/// The same shape with FAdd is correct, so the bug is
/// specific to the FMax/FMin emit (not the classifier or
/// poly helper).  Marked #[ignore] until the per-lane
/// FMax/FMin path is debugged.  The scalar FMax/FMin
/// path and the all-packed-vec4 FMin/FMax path both
/// work.
#[test]
fn differential_tonemap_with_direct_fmax() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let uvec3  = b.type_vector(u32_ty, 3);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(16)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let mk_vec = |b: &mut rspirv::dr::Builder, x: f32| {
        let l = b.constant_bit32(f32_ty, x.to_bits());
        b.constant_composite(vec4, vec![l, l, l, l])
    };
    let v_zero_vec = mk_vec(&mut b, 0.0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let _gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let in_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    let c = b.load(vec4, None, in_ptr, None, vec![]).unwrap();
    // Direct FMax of loaded vec4 with const-zero vec4.
    let r = b.ext_inst(vec4, None, std_450, 40,
        vec![rspirv::dr::Operand::IdRef(c),
             rspirv::dr::Operand::IdRef(v_zero_vec)]).unwrap();
    let out_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_one]).unwrap();
    b.store(out_ptr, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    for (j, x) in [-0.5f32, 0.3, -1.0, 0.7].iter().enumerate() {
        b_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
        c_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
    }
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on direct fmax(loaded vec, zero vec)");
}

/// Tonemap without clamp -- isolates whether the bug is in
/// the FMax/FMin path or upstream.
#[test]
fn differential_tonemap_without_clamp() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let std_450 = b.ext_inst_import("GLSL.std.450");
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let uvec3  = b.type_vector(u32_ty, 3);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![vec4, vec4]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(s, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(16)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_v = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let mk_vec = |b: &mut rspirv::dr::Builder, x: f32| {
        let l = b.constant_bit32(f32_ty, x.to_bits());
        b.constant_composite(vec4, vec![l, l, l, l])
    };
    let v_one  = mk_vec(&mut b, 1.0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let _gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let in_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_zero]).unwrap();
    let c = b.load(vec4, None, in_ptr, None, vec![]).unwrap();
    let denom = b.f_add(vec4, None, c, v_one).unwrap();
    let mapped = b.f_div(vec4, None, c, denom).unwrap();
    let gamma = b.ext_inst(vec4, None, std_450, 31,
        vec![rspirv::dr::Operand::IdRef(mapped)]).unwrap();
    let out_ptr = b.access_chain(ptr_v, None, ssbo, vec![c_one]).unwrap();
    b.store(out_ptr, gamma, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    for (j, x) in [0.1f32, 0.2, 0.3, 1.0].iter().enumerate() {
        b_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
        c_buf[j*4..j*4+4].copy_from_slice(&x.to_le_bytes());
    }
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on tonemap-no-clamp");
}

#[test]
#[ignore = "V-pool exhaustion -- codegen-synth lanes don't participate in last_use (broader RA issue, not specific to GLSL math)"]
fn differential_tonemap_per_pixel_vec4() {
    let spv = build_tonemap_cs();
    let dir = TempDir::new().unwrap();
    // 4 HDR pixels: a mix of low + high luminance.
    let pixels: [[f32; 4]; 4] = [
        [0.1, 0.2, 0.3, 1.0],
        [1.0, 1.0, 1.0, 1.0],
        [3.0, 0.5, 0.0, 1.0],
        [10.0, 10.0, 10.0, 1.0],
    ];
    let make_bufs = || -> Vec<Vec<u8>> {
        let mut in_buf = vec![0u8; 256];
        for (i, p) in pixels.iter().enumerate() {
            for (j, x) in p.iter().enumerate() {
                let off = i*16 + j*4;
                in_buf[off..off+4].copy_from_slice(&x.to_le_bytes());
            }
        }
        let out_buf = vec![0u8; 256];
        vec![in_buf, out_buf]
    };
    let mut b_bufs = make_bufs();
    let mut c_bufs = make_bufs();
    let b_table: Vec<u64> = b_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
    let c_table: Vec<u64> = c_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
    let gids: Vec<(u32, u32, u32)> = (0..pixels.len() as u32)
        .map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b",
        b_table.as_ptr() as *mut u8, &gids);
    invoke_with_gids(&spv, false, dir.path(), "c",
        c_table.as_ptr() as *mut u8, &gids);
    assert_eq!(b_bufs, c_bufs, "tonemap diverges between backends");
    // Spot-check: pixel 2 (saturated white in) should land
    // at ~sqrt(0.5) ≈ 0.7071 in all RGB channels.
    let read = |buf: &[u8], i: usize, j: usize| -> f32 {
        f32::from_le_bytes(buf[i*16 + j*4..i*16 + j*4+4].try_into().unwrap())
    };
    let r = read(&b_bufs[1], 1, 0);
    assert!((r - 2.0f32.sqrt() / 2.0).abs() < 1e-5,
        "white pixel R should be ~0.7071, got {r}");
}

#[test]
fn differential_local_invocation_index_linearises() {
    let spv = build_local_index_cs();
    let dir = TempDir::new().unwrap();
    // 8 invocations: (lx, ly) over (0..4, 0..2).  Dispatch
    // loop nests lx innermost.
    let mut b_buf = vec![0u8; 64];
    let mut c_buf = vec![0u8; 64];
    let mut gids = Vec::new();
    for ly in 0..2u32 {
        for lx in 0..4u32 {
            gids.push((lx, ly, 0u32));
        }
    }
    // We invoke directly, so the gid_x/gid_y supplied here
    // become lx/ly (workgroup_id is 0).  The shader uses
    // LocalInvocationIndex which depends on the (lx, ly, lz)
    // params passed through cs_main's W6/W7/[SP+0] slots.
    // But our invoke_with_gids helper passes the FIRST 3 args
    // as workgroup_id and the LAST 3 as lid.  So we need a
    // different helper -- or use the existing one with lid in
    // the right slots.  Use the existing helper -- its 4th-6th
    // args ARE lid (per the cs_main signature).
    let lid_tuples: Vec<(u32, u32, u32)> = gids;
    invoke_cs_main_with_lids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &lid_tuples);
    invoke_cs_main_with_lids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &lid_tuples);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on LocalInvocationIndex");
    for i in 0..8u32 {
        let v = u32::from_le_bytes(b_buf[(i as usize)*4..(i as usize)*4+4]
            .try_into().unwrap());
        assert_eq!(v, i, "slot {i} should hold {i}; got {v}");
    }
}

/// Same as invoke_with_gids but the tuple is (lid_x, lid_y,
/// lid_z) -- the LAST three cs_main args.  Workgroup is
/// always (0,0,0) here.  Used by tests that need to drive
/// the lid lanes directly.
fn invoke_cs_main_with_lids(spv: &[u8], use_bespoke: bool, dir: &Path, name: &str,
                            out_ptr: *mut u8, lids: &[(u32, u32, u32)]) {
    let module = translate(spv).expect("frontend");
    let obj = if use_bespoke {
        let t = if cfg!(target_os = "macos") {
            BespokeTarget::Aarch64Darwin
        } else { BespokeTarget::Aarch64FreeBSD };
        bespoke_compile(&module, t).expect("bespoke compile").object
    } else {
        cranelift_compile(&module, CraneliftTarget::host())
            .expect("cranelift compile").object
    };
    let obj_path = dir.join(format!("{name}.o"));
    std::fs::write(&obj_path, &obj).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.join(format!("{name}.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).expect("link");
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .expect("dlopen");
    let cs_main: libloading::Symbol<CsMain> = unsafe {
        lib.get(b"atrium_cs_main").expect("atrium_cs_main symbol")
    };
    for &(lx, ly, lz) in lids {
        unsafe {
            cs_main(
                std::ptr::null(), std::ptr::null(), out_ptr,
                0, 0, 0,  // workgroup id
                lx, ly, lz,
            );
        }
    }
}

#[test]
fn differential_histogram_one_binding() {
    let spv = build_histogram_one_ssbo_cs();
    let dir = TempDir::new().unwrap();
    // Buffer layout:
    //   [0..16]  = 4 bins (initially 0)
    //   [16..48] = 8 samples [3,1,4,1,5,9,2,6]
    let samples = [3u32, 1, 4, 1, 5, 9, 2, 6];
    let make_buf = || -> Vec<u8> {
        let mut buf = vec![0u8; 64];
        for (i, v) in samples.iter().enumerate() {
            // Sample region starts at byte 32 (slot 8).
            let off = 32 + i*4;
            buf[off..off+4].copy_from_slice(&v.to_le_bytes());
        }
        buf
    };
    let mut b_buf = make_buf();
    let mut c_buf = make_buf();
    let gids: Vec<(u32, u32, u32)> = (0..samples.len() as u32)
        .map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &gids);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &gids);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on one-ssbo histogram");
    let bins: Vec<u32> = (0..4)
        .map(|i| u32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap()))
        .collect();
    assert_eq!(bins, vec![1, 4, 2, 1], "got {bins:?}");
}

#[test]
fn differential_histogram_integration() {
    let spv = build_histogram_diff_cs();
    let dir = TempDir::new().unwrap();
    let samples = [3u32, 1, 4, 1, 5, 9, 2, 6];
    let make_bufs = || -> Vec<Vec<u8>> {
        let mut in_buf = vec![0u8; 64];
        for (i, v) in samples.iter().enumerate() {
            in_buf[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
        }
        let out_buf = vec![0u8; 64];
        vec![in_buf, out_buf]
    };
    let mut b_bufs = make_bufs();
    let mut c_bufs = make_bufs();
    let b_table: Vec<u64> = b_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
    let c_table: Vec<u64> = c_bufs.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
    let gids: Vec<(u32, u32, u32)> = (0..samples.len() as u32)
        .map(|i| (i, 0, 0)).collect();
    invoke_with_gids(&spv, true,  dir.path(), "b",
        b_table.as_ptr() as *mut u8, &gids);
    invoke_with_gids(&spv, false, dir.path(), "c",
        c_table.as_ptr() as *mut u8, &gids);
    assert_eq!(b_bufs, c_bufs,
        "bespoke vs cranelift diverge on histogram integration");
    // Sanity-check the actual histogram.
    let bins: Vec<u32> = (0..4)
        .map(|i| u32::from_le_bytes(b_bufs[1][i*4..i*4+4].try_into().unwrap()))
        .collect();
    assert_eq!(bins, vec![1, 4, 2, 1], "got {bins:?}");
}

#[test]
fn differential_six_binding_constant_store() {
    let spv = build_n_binding_constants(6);
    let (b, c) = diff(&spv, 6);
    assert_equal("six-binding (max bespoke cap)", &b, &c);
}
