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
    u32, u32, u32, u32, u32, u32, *mut u8);

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
    // Per-workgroup shared-memory scratch size (0 for shaders
    // that declare no Workgroup-storage variables).
    let wg_bytes = module.functions.first()
        .map(|f| f.workgroup_size as usize).unwrap_or(0);
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
    let mut wg_buf: Vec<u8> = vec![0u8; wg_bytes];
    for &(gx, gy, gz) in gids {
        // Fresh shared memory per workgroup.
        for b in wg_buf.iter_mut() { *b = 0; }
        let wg_ptr = if wg_bytes == 0 {
            std::ptr::null_mut()
        } else { wg_buf.as_mut_ptr() };
        unsafe {
            cs_main(
                std::ptr::null(), std::ptr::null(), out_ptr,
                gx, gy, gz, 0, 0, 0, wg_ptr,
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
fn differential_glsl_cross() {
    // cross(x_axis, y_axis) = z_axis: (1,0,0) × (0,1,0) = (0,0,1)
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
    let vec3   = b.type_vector(f32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    // Three-element vector store layout: tightly packed at
    // byte offsets 0, 4, 8.
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
    let c_one_f  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let c_zero_f = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let v_x = b.constant_composite(vec3, vec![c_one_f, c_zero_f, c_zero_f]);
    let v_y = b.constant_composite(vec3, vec![c_zero_f, c_one_f, c_zero_f]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let cr = b.ext_inst(vec3, None, std_450, 68,
        vec![rspirv::dr::Operand::IdRef(v_x),
             rspirv::dr::Operand::IdRef(v_y)]).unwrap();
    // Store the three lanes individually since vec3 doesn't have
    // a pack-to-12-bytes form -- extract + store each.
    for i in 0..3u32 {
        let ci = b.constant_bit32(u32_ty, i);
        let lane = b.composite_extract(f32_ty, None, cr, vec![i]).unwrap();
        let d = b.access_chain(ptr_f, None, ssbo, vec![c_zero, ci]).unwrap();
        b.store(d, lane, None, vec![]).unwrap();
    }
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
    assert_eq!(b_buf, c_buf, "diverge on cross");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    // x_axis × y_axis = z_axis
    assert_eq!([read(0), read(1), read(2)], [0.0, 0.0, 1.0],
        "(1,0,0) × (0,1,0) = (0,0,1)");
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
fn differential_glsl_sabs() {
    // sabs(-5) = 5; sabs(7) = 7; sabs(0) = 0.
    // Synthesised via (x XOR (x>>31)) - (x>>31).
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
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    // -5 as u32: 0xFFFFFFFB.  +7 unchanged.
    let c_neg5 = b.constant_bit32(u32_ty, (-5i32) as u32);
    let c_pos7 = b.constant_bit32(u32_ty, 7);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let abs_neg5 = b.ext_inst(u32_ty, None, std_450, 5,
        vec![rspirv::dr::Operand::IdRef(c_neg5)]).unwrap();
    let abs_pos7 = b.ext_inst(u32_ty, None, std_450, 5,
        vec![rspirv::dr::Operand::IdRef(c_pos7)]).unwrap();
    let d0 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d0, abs_neg5, None, vec![]).unwrap();
    let c_one = b.constant_bit32(u32_ty, 1);
    let d1 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_one]).unwrap();
    b.store(d1, abs_pos7, None, vec![]).unwrap();
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
    assert_eq!(b_buf, c_buf, "diverge on sabs");
    let v0 = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
    let v1 = u32::from_le_bytes(b_buf[4..8].try_into().unwrap());
    assert_eq!(v0, 5, "sabs(-5) = 5");
    assert_eq!(v1, 7, "sabs(7)  = 7");
}

#[test]
fn differential_glsl_tan() {
    // tan(0) = 0; tan(π/4) = 1.
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
    let c_one  = b.constant_bit32(u32_ty, 1);
    let c_0f      = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_pi_4    = b.constant_bit32(f32_ty, std::f32::consts::FRAC_PI_4.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let tan_0  = b.ext_inst(f32_ty, None, std_450, 15,
        vec![rspirv::dr::Operand::IdRef(c_0f)]).unwrap();
    let tan_pi_4 = b.ext_inst(f32_ty, None, std_450, 15,
        vec![rspirv::dr::Operand::IdRef(c_pi_4)]).unwrap();
    let d0 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d0, tan_0, None, vec![]).unwrap();
    let d1 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_one]).unwrap();
    b.store(d1, tan_pi_4, None, vec![]).unwrap();
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
    assert_eq!(b_buf, c_buf, "diverge on tan");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    assert_eq!(read(0), 0.0, "tan(0) = 0");
    assert!((read(1) - 1.0).abs() < 1e-3,
        "tan(π/4) ≈ 1 (Taylor tolerance); got {}", read(1));
}

#[test]
fn differential_glsl_sin_cos() {
    // sin(0) = 0, sin(π/2) ≈ 1
    // cos(0) = 1, cos(π/2) ≈ 0
    // Within the polynomial's [-π/2, π/2] accuracy domain.
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
    let c_0f      = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_pi_half = b.constant_bit32(f32_ty, std::f32::consts::FRAC_PI_2.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sin_0 = b.ext_inst(f32_ty, None, std_450, 13,
        vec![rspirv::dr::Operand::IdRef(c_0f)]).unwrap();
    let sin_pi_half = b.ext_inst(f32_ty, None, std_450, 13,
        vec![rspirv::dr::Operand::IdRef(c_pi_half)]).unwrap();
    let cos_0 = b.ext_inst(f32_ty, None, std_450, 14,
        vec![rspirv::dr::Operand::IdRef(c_0f)]).unwrap();
    let cos_pi_half = b.ext_inst(f32_ty, None, std_450, 14,
        vec![rspirv::dr::Operand::IdRef(c_pi_half)]).unwrap();
    let vs = [sin_0, sin_pi_half, cos_0, cos_pi_half];
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
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on sin/cos");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    // sin(0) exactly 0.
    assert_eq!(read(0), 0.0, "sin(0) = 0");
    // sin(π/2) ≈ 1 within polynomial-approximation tolerance.
    assert!((read(1) - 1.0).abs() < 1e-3,
        "sin(π/2) ≈ 1 (Taylor tolerance); got {}", read(1));
    // cos(0) exactly 1.
    assert_eq!(read(2), 1.0, "cos(0) = 1");
    // cos(π/2) ≈ 0.
    assert!(read(3).abs() < 1e-3,
        "cos(π/2) ≈ 0 (Taylor tolerance); got {}", read(3));
}

// Build a small single-binding compute shader that runs
// `ext_op` (a GLSL.std.450 enum) on either two (binop=true)
// or three (binop=false) constant int operands and stores
// the result at ssbo[0].  `signed=true` selects i32_ty, else
// u32_ty.  Returns the assembled SPIR-V blob.
fn build_int_glsl_shader(ext_op: u32, signed: bool, args: &[u32]) -> Vec<u8> {
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
    let int_ty = if signed { b.type_int(32, 1) } else { u32_ty };
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(int_ty);
    b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_x = b.type_pointer(None, StorageClass::StorageBuffer, int_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let consts: Vec<u32> = args.iter()
        .map(|&a| b.constant_bit32(int_ty, a))
        .collect();
    let operands: Vec<rspirv::dr::Operand> = consts.iter()
        .map(|&c| rspirv::dr::Operand::IdRef(c)).collect();
    let r = b.ext_inst(int_ty, None, std_450, ext_op, operands).unwrap();
    let d = b.access_chain(ptr_x, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(d, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    spv
}

#[test]
fn differential_glsl_nan_min_max_clamp() {
    // NMin(79) / NMax(80) / NClamp(81) currently alias to
    // FMin / FMax / FClamp.  The IEEE 754-2008 NaN-
    // suppressing semantics are deferred (ARM FMINNM /
    // FMAXNM would provide them).  These tests assert the
    // common case (no NaN inputs) lowers identically.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let cases: &[(&str, u32, &[f32], f32)] = &[
        ("nmin(3,7)",    79, &[3.0, 7.0], 3.0),
        ("nmin(-1,2)",   79, &[-1.0, 2.0], -1.0),
        ("nmax(3,7)",    80, &[3.0, 7.0], 7.0),
        ("nclamp(5,1,3)",81, &[5.0, 1.0, 3.0], 3.0),
        ("nclamp(-1,0,1)",81, &[-1.0, 0.0, 1.0], 0.0),
    ];
    for &(label, ext_op, args, expected) in cases {
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 3);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let std_450 = b.ext_inst_import("GLSL.std.450");
        let void   = b.type_void();
        let u32_ty = b.type_int(32, 0);
        let f32_ty = b.type_float(32, None);
        let void_fn = b.type_function(void, vec![]);
        let rt = b.type_runtime_array(f32_ty);
        b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
        let s = b.type_struct(vec![rt]);
        b.decorate(s, Decoration::Block, vec![]);
        b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
        let ptr_f = b.type_pointer(None, StorageClass::StorageBuffer, f32_ty);
        let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
        b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let c_zero = b.constant_bit32(u32_ty, 0);
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let cs: Vec<u32> = args.iter()
            .map(|&v| b.constant_bit32(f32_ty, v.to_bits()))
            .collect();
        let operands: Vec<rspirv::dr::Operand> = cs.iter()
            .map(|&c| rspirv::dr::Operand::IdRef(c)).collect();
        let r = b.ext_inst(f32_ty, None, std_450, ext_op, operands).unwrap();
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
        let mut b_buf = vec![0u8; 4];
        let mut c_buf = vec![0u8; 4];
        invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
        invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
        assert_eq!(b_buf, c_buf, "{label}: bespoke vs cranelift diverge");
        let got = f32::from_le_bytes(b_buf[0..4].try_into().unwrap());
        assert!((got - expected).abs() < 1e-6,
            "{label}: got {}, want {}", got, expected);
    }
}

#[test]
fn lse_atomics_are_race_safe_under_concurrent_threads() {
    // Compile a shader that does `ssbo[0] = atomicAdd(ssbo[0], 1)`
    // then spawn N OS threads each running cs_main once, all
    // racing on the same output buffer.  With LSE atomics
    // (LDADDAL), the final counter must equal N.  With the old
    // load-op-store sequence the counter would lose updates.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, Scope,
        StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let scope_dev = b.constant_bit32(u32_ty, Scope::Device as u32);
    let sem_rel = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, p, scope_dev, sem_rel, c_one).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        BespokeTarget::Aarch64Darwin
    } else { BespokeTarget::Aarch64FreeBSD };
    let obj = bespoke_compile(&module, target).expect("compile").object;
    let obj_path = dir.path().join("atomic_race.o");
    std::fs::write(&obj_path, &obj).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("atomic_race.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).expect("link");
    let lib = unsafe { libloading::Library::new(&lib_path) }.expect("dlopen");
    let cs_main: libloading::Symbol<CsMain> = unsafe {
        lib.get(b"atrium_cs_main").expect("atrium_cs_main")
    };
    // We need a stable function pointer for the threads; copy
    // the C function pointer out of the libloading::Symbol.
    let cs_fn: CsMain = *cs_main;

    const N_THREADS: usize = 32;
    const N_ITERS_PER_THREAD: usize = 100;
    let mut buf = vec![0u8; 4];
    let addr = buf.as_mut_ptr() as usize;
    std::thread::scope(|scope| {
        for _ in 0..N_THREADS {
            scope.spawn(move || {
                let p = addr as *mut u8;
                for _ in 0..N_ITERS_PER_THREAD {
                    unsafe {
                        cs_fn(std::ptr::null(), std::ptr::null(), p,
                              0, 0, 0, 0, 0, 0, std::ptr::null_mut());
                    }
                }
            });
        }
    });
    let counter = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let expected = (N_THREADS * N_ITERS_PER_THREAD) as u32;
    assert_eq!(counter, expected,
        "LSE atomic counter raced: got {counter}, want {expected}");
}

#[test]
fn differential_op_bit_count_and_reverse() {
    // OpBitCount (SWAR popcount in IR) and OpBitReverse
    // (Op::Rbit / Cranelift bitrev).  These are SPIR-V core
    // ops, not GLSL.std.450 ExtInst -- we build the test
    // shader by hand.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let cases: &[(&str, &str, u32, u32)] = &[
        // (label, opcode, input, expected)
        ("bitcount(0)",       "count", 0, 0),
        ("bitcount(1)",       "count", 1, 1),
        ("bitcount(0xFF)",    "count", 0xFF, 8),
        ("bitcount(0xFFFFFFFF)", "count", 0xFFFF_FFFF, 32),
        ("bitcount(0xAAAAAAAA)", "count", 0xAAAA_AAAA, 16),
        ("bitreverse(1)",     "rev", 1, 0x8000_0000),
        ("bitreverse(0x80000000)", "rev", 0x8000_0000, 1),
        ("bitreverse(0x12345678)", "rev", 0x1234_5678, 0x1E6A_2C48),
    ];
    for &(label, op, input, expected) in cases {
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 3);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void   = b.type_void();
        let u32_ty = b.type_int(32, 0);
        let void_fn = b.type_function(void, vec![]);
        let rt = b.type_runtime_array(u32_ty);
        b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
        let s = b.type_struct(vec![rt]);
        b.decorate(s, Decoration::Block, vec![]);
        b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
        let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
        let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
        b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let c_zero = b.constant_bit32(u32_ty, 0);
        let c_in = b.constant_bit32(u32_ty, input);
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let r = if op == "count" {
            b.bit_count(u32_ty, None, c_in).unwrap()
        } else {
            b.bit_reverse(u32_ty, None, c_in).unwrap()
        };
        let d = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
        b.store(d, r, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
        b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
        let words: Vec<u32> = b.module().assemble();
        let mut spv = Vec::with_capacity(words.len() * 4);
        for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
        let dir = TempDir::new().unwrap();
        let mut b_buf = vec![0u8; 4];
        let mut c_buf = vec![0u8; 4];
        invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
        invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
        assert_eq!(b_buf, c_buf, "{label}: bespoke vs cranelift diverge");
        let got = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
        assert_eq!(got, expected,
            "{label}: got {:#x}, want {:#x}", got, expected);
    }
}

#[test]
fn differential_glsl_int_bit_scan() {
    // FindILsb(73) / FindSMsb(74) / FindUMsb(75) lower onto
    // Op::Clz + Op::Rbit (new IR variants) with edge-case
    // selects for x==0 / x<0.
    let cases: &[(&str, u32, bool, &[u32], i32)] = &[
        ("findumsb(1)",     75, false, &[1],          0),
        ("findumsb(0x80)",  75, false, &[0x80],       7),
        ("findumsb(0x8000_0000)", 75, false, &[0x8000_0000], 31),
        ("findumsb(0)",     75, false, &[0],         -1),
        ("findilsb(1)",     73, true,  &[1],          0),
        ("findilsb(0x80)",  73, true,  &[0x80],       7),
        ("findilsb(0x8000_0000)", 73, true, &[0x8000_0000], 31),
        ("findilsb(0)",     73, true,  &[0],         -1),
        ("findsmsb(1)",     74, true,  &[1],          0),
        ("findsmsb(-1)",    74, true,  &[-1i32 as u32], -1),
        ("findsmsb(-2)",    74, true,  &[-2i32 as u32], 0),
        ("findsmsb(0x7FFF_FFFF)", 74, true, &[0x7FFF_FFFF], 30),
    ];
    for &(label, ext_op, signed, args, expected) in cases {
        let spv = build_int_glsl_shader(ext_op, signed, args);
        let dir = TempDir::new().unwrap();
        let mut b_buf = vec![0u8; 4];
        let mut c_buf = vec![0u8; 4];
        invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
        invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
        assert_eq!(b_buf, c_buf, "{label}: bespoke vs cranelift diverge");
        let got = i32::from_le_bytes(b_buf[0..4].try_into().unwrap());
        assert_eq!(got, expected,
            "{label}: got {}, want {}", got, expected);
    }
}

#[test]
fn differential_glsl_int_min_max_clamp() {
    // SMin/UMin/SMax/UMax/SClamp/UClamp lower to Select on
    // SLt/ULt/SGt/UGt.  Each op gets its own tiny shader to
    // keep entry-block constant pre-materialisation under
    // the bespoke int W-pool budget; verify the output
    // matches host behaviour and bespoke/cranelift produce
    // byte-identical SSBO buffers.
    let cases: &[(&str, u32, bool, &[u32], u32)] = &[
        // (label, ext_op, signed, args, expected)
        ("umin(3,7)",        39, false, &[3, 7],            3),
        ("umin(10,2)",       39, false, &[10, 2],           2),
        ("umax(3,7)",        41, false, &[3, 7],            7),
        ("umax(0,0xFFFFFFFF)",41,false, &[0, 0xFFFF_FFFF],  0xFFFF_FFFF),
        ("uclamp(15,1,10)",  44, false, &[15, 1, 10],       10),
        ("uclamp(5,1,10)",   44, false, &[5,  1, 10],       5),
        ("smin(3,-7)",       38, true,  &[3u32, -7i32 as u32], -7i32 as u32),
        ("smax(-5,5)",       42, true,  &[-5i32 as u32, 5], 5),
        ("sclamp(-50,-10,10)",45,true,  &[-50i32 as u32, -10i32 as u32, 10], -10i32 as u32),
        ("sclamp(-3,-10,10)",45, true,  &[-3i32 as u32, -10i32 as u32, 10], -3i32 as u32),
    ];
    for &(label, ext_op, signed, args, expected) in cases {
        let spv = build_int_glsl_shader(ext_op, signed, args);
        let dir = TempDir::new().unwrap();
        let mut b_buf = vec![0u8; 4];
        let mut c_buf = vec![0u8; 4];
        invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
        invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
        assert_eq!(b_buf, c_buf, "{label}: bespoke vs cranelift diverge");
        let got = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
        assert_eq!(got, expected,
            "{label}: got {:#x}, want {:#x}", got, expected);
    }
}


#[test]
fn differential_glsl_hyperbolic() {
    // Sinh / Cosh / Tanh / Asinh / Acosh / Atanh.
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
    // Each row: (op_enum, input, host_fn_result).
    let sinh_in:  [f32; 3] = [0.0, 1.0, -2.0];
    let cosh_in:  [f32; 3] = [0.0, 1.0,  2.0];
    let tanh_in:  [f32; 3] = [0.0, 1.0, -3.0];
    let asinh_in: [f32; 3] = [0.0, 1.0, -2.0];
    let acosh_in: [f32; 3] = [1.0, 2.0, 10.0];
    let atanh_in: [f32; 3] = [0.0, 0.5, -0.9];
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut results: Vec<u32> = Vec::new();
    let mut emit_one = |ext_op: u32, inputs: &[f32], b: &mut rspirv::dr::Builder, results: &mut Vec<u32>| {
        for &val in inputs {
            let c = b.constant_bit32(f32_ty, val.to_bits());
            let r = b.ext_inst(f32_ty, None, std_450, ext_op,
                vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
            results.push(r);
        }
    };
    emit_one(19, &sinh_in,  &mut b, &mut results);
    emit_one(20, &cosh_in,  &mut b, &mut results);
    emit_one(21, &tanh_in,  &mut b, &mut results);
    emit_one(22, &asinh_in, &mut b, &mut results);
    emit_one(23, &acosh_in, &mut b, &mut results);
    emit_one(24, &atanh_in, &mut b, &mut results);
    for (i, v) in results.iter().enumerate() {
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
    let n = results.len();
    let mut b_buf = vec![0u8; n * 4];
    let mut c_buf = vec![0u8; n * 4];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on hyperbolic");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    // Approximations stack exp+log errors; allow ~1% rel.
    let rel_tol = 1e-2_f32;
    let abs_tol = 5e-3_f32;
    let mut idx = 0;
    let check = |idx: usize, x: f32, want: f32, name: &str| {
        let got = read(idx);
        let err = (got - want).abs();
        let rel = err / want.abs().max(1e-3);
        assert!(err < abs_tol || rel < rel_tol,
            "{}({}) = {} (want {}, abs_err {}, rel_err {})", name, x, got, want, err, rel);
    };
    for &x in &sinh_in  { check(idx, x, x.sinh(),  "sinh");  idx += 1; }
    for &x in &cosh_in  { check(idx, x, x.cosh(),  "cosh");  idx += 1; }
    for &x in &tanh_in  { check(idx, x, x.tanh(),  "tanh");  idx += 1; }
    for &x in &asinh_in { check(idx, x, x.asinh(), "asinh"); idx += 1; }
    for &x in &acosh_in { check(idx, x, x.acosh(), "acosh"); idx += 1; }
    for &x in &atanh_in { check(idx, x, x.atanh(), "atanh"); idx += 1; }
}

#[test]
fn differential_glsl_atan2() {
    // Atan2(y, x): four-quadrant arctangent.
    //   atan2(0, 1)   = 0
    //   atan2(1, 1)   = π/4
    //   atan2(1, 0)   = π/2
    //   atan2(1, -1)  = 3π/4
    //   atan2(-1, -1) = -3π/4
    //   atan2(-1, 0)  = -π/2
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
    let inputs: [(f32, f32); 6] = [
        (0.0,  1.0),
        (1.0,  1.0),
        (1.0,  0.0),
        (1.0, -1.0),
        (-1.0, -1.0),
        (-1.0,  0.0),
    ];
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut results: Vec<u32> = Vec::new();
    for &(y, x) in &inputs {
        let cy = b.constant_bit32(f32_ty, y.to_bits());
        let cx = b.constant_bit32(f32_ty, x.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 25,
            vec![rspirv::dr::Operand::IdRef(cy), rspirv::dr::Operand::IdRef(cx)]).unwrap();
        results.push(r);
    }
    for (i, v) in results.iter().enumerate() {
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
    let n = results.len();
    let mut b_buf = vec![0u8; n * 4];
    let mut c_buf = vec![0u8; n * 4];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on atan2");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    let tol = 5e-4_f32;
    for (i, &(y, x)) in inputs.iter().enumerate() {
        let want = y.atan2(x);
        let got = read(i);
        assert!((got - want).abs() < tol,
            "atan2({}, {}) = {} (want {})", y, x, got, want);
    }
}

#[test]
fn differential_glsl_atan_asin_acos() {
    // Atan(x): full real line, reciprocal range reduction.
    //   atan(0)=0, atan(1)=π/4, atan(-1)=-π/4,
    //   atan(2)≈1.1071, atan(-100)≈-1.5608.
    // Asin(x) for x ∈ [-1, 1]:
    //   asin(0)=0, asin(0.5)=π/6, asin(1)=π/2, asin(-1)=-π/2.
    // Acos(x):
    //   acos(0)=π/2, acos(1)=0, acos(-1)=π, acos(0.5)=π/3.
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
    let atan_inputs: [f32; 5] = [0.0, 1.0, -1.0, 2.0, -100.0];
    let asin_inputs: [f32; 4] = [0.0, 0.5, 1.0, -1.0];
    let acos_inputs: [f32; 4] = [0.0, 1.0, -1.0, 0.5];
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut results: Vec<u32> = Vec::new();
    for &val in &atan_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 18,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for &val in &asin_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 16,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for &val in &acos_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 17,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for (i, v) in results.iter().enumerate() {
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
    let n = results.len();
    let mut b_buf = vec![0u8; n * 4];
    let mut c_buf = vec![0u8; n * 4];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on atan/asin/acos");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    let tol = 5e-4_f32;
    for (i, &x) in atan_inputs.iter().enumerate() {
        let want = x.atan();
        let got = read(i);
        assert!((got - want).abs() < tol,
            "atan({}) = {} (want {})", x, got, want);
    }
    for (i, &x) in asin_inputs.iter().enumerate() {
        let want = x.asin();
        let got = read(5 + i);
        assert!((got - want).abs() < tol,
            "asin({}) = {} (want {})", x, got, want);
    }
    for (i, &x) in acos_inputs.iter().enumerate() {
        let want = x.acos();
        let got = read(9 + i);
        assert!((got - want).abs() < tol,
            "acos({}) = {} (want {})", x, got, want);
    }
}

#[test]
fn differential_glsl_exp_exp2() {
    // Exp2: 2^0 = 1, 2^1 = 2, 2^3 = 8, 2^-2 = 0.25, 2^10 = 1024.
    // Exp:  e^0 = 1, e^1 ≈ 2.71828, e^-1 ≈ 0.3679,
    //       e^5 ≈ 148.413.
    // Both lower to synth_exp2 with IEEE-754 exponent
    // reconstruction (requires Op::Bitcast lowering).
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
    // Exp2 inputs at indices 0..5, Exp inputs at 5..10.
    let exp2_inputs: [f32; 5] = [0.0, 1.0, 3.0, -2.0, 10.0];
    let exp_inputs:  [f32; 5] = [0.0, 1.0, -1.0, 5.0, 2.5];
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut results: Vec<u32> = Vec::new();
    for &val in &exp2_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 29,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for &val in &exp_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 27,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for (i, v) in results.iter().enumerate() {
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
    let n = results.len();
    let mut b_buf = vec![0u8; n * 4];
    let mut c_buf = vec![0u8; n * 4];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on exp/exp2");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    // 5-term Taylor in [-0.5, 0.5] has relative error ~3e-5,
    // amplified slightly by the 2^k multiply.  Use relative
    // tolerance of 1e-3 to cover the full domain.
    let rel = |got: f32, want: f32| -> f32 {
        if want == 0.0 { got.abs() } else { (got - want).abs() / want.abs() }
    };
    for (i, &x) in exp2_inputs.iter().enumerate() {
        let want = 2.0f32.powf(x);
        let got = read(i);
        assert!(rel(got, want) < 1e-3,
            "exp2({}) = {} (want {})", x, got, want);
    }
    for (i, &x) in exp_inputs.iter().enumerate() {
        let want = x.exp();
        let got = read(5 + i);
        assert!(rel(got, want) < 1e-3,
            "exp({}) = {} (want {})", x, got, want);
    }
}

#[test]
fn differential_glsl_log_log2_pow() {
    // Log2: 1 → 0, 2 → 1, 8 → 3, 0.25 → -2, 1024 → 10.
    // Log:  e → 1, 1 → 0, 100 → 4.6052, 0.5 → -0.6931.
    // Pow:  2^3 = 8, 10^2 = 100, 2^0.5 = √2, 0.5^2 = 0.25.
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
    let log2_inputs: [f32; 5] = [1.0, 2.0, 8.0, 0.25, 1024.0];
    let log_inputs:  [f32; 4] = [std::f32::consts::E, 1.0, 100.0, 0.5];
    let pow_inputs:  [(f32, f32); 4] = [(2.0, 3.0), (10.0, 2.0), (2.0, 0.5), (0.5, 2.0)];
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut results: Vec<u32> = Vec::new();
    for &val in &log2_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 30,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for &val in &log_inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 28,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(r);
    }
    for &(x, y) in &pow_inputs {
        let cx = b.constant_bit32(f32_ty, x.to_bits());
        let cy = b.constant_bit32(f32_ty, y.to_bits());
        let r = b.ext_inst(f32_ty, None, std_450, 26,
            vec![rspirv::dr::Operand::IdRef(cx), rspirv::dr::Operand::IdRef(cy)]).unwrap();
        results.push(r);
    }
    for (i, v) in results.iter().enumerate() {
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
    let n = results.len();
    let mut b_buf = vec![0u8; n * 4];
    let mut c_buf = vec![0u8; n * 4];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on log/log2/pow");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    // Mineiro-style log2 has max relative error ~4e-4 on its
    // own; pow stacks log2 + exp2 errors so allow a bit more.
    let abs_tol = 5e-3_f32;
    for (i, &x) in log2_inputs.iter().enumerate() {
        let want = x.log2();
        let got = read(i);
        assert!((got - want).abs() < abs_tol,
            "log2({}) = {} (want {})", x, got, want);
    }
    for (i, &x) in log_inputs.iter().enumerate() {
        let want = x.ln();
        let got = read(5 + i);
        assert!((got - want).abs() < abs_tol,
            "log({}) = {} (want {})", x, got, want);
    }
    for (i, &(x, y)) in pow_inputs.iter().enumerate() {
        let want = x.powf(y);
        let got = read(9 + i);
        let rel = (got - want).abs() / want.abs().max(1e-6);
        assert!(rel < 1e-2,
            "pow({}, {}) = {} (want {}, rel {})", x, y, got, want, rel);
    }
}

#[test]
fn differential_glsl_sin_cos_extended_range() {
    // Range-reduction validation: sin/cos at arguments well
    // outside the polynomial's native [-π/2, π/2] domain.
    // The frontend reduces x to x_red ∈ [-π/2, π/2] modulo π
    // and flips sign on odd quadrants.
    //
    //   sin(π)     ≈  0       cos(π)     = -1
    //   sin(3π/2)  ≈ -1       cos(2π)    ≈  1
    //   sin(2π)    ≈  0       cos(5π/2)  ≈  0
    //   sin(-π/2)  = -1       cos(-π)    = -1
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
    use std::f32::consts::PI;
    let inputs = [
        PI,            // sin op + cos op
        3.0 * PI / 2.0,
        2.0 * PI,
        5.0 * PI / 2.0,
        -PI / 2.0,
        -PI,
    ];
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut results: Vec<u32> = Vec::new();
    for &val in &inputs {
        let c = b.constant_bit32(f32_ty, val.to_bits());
        let s = b.ext_inst(f32_ty, None, std_450, 13,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        let cs = b.ext_inst(f32_ty, None, std_450, 14,
            vec![rspirv::dr::Operand::IdRef(c)]).unwrap();
        results.push(s);
        results.push(cs);
    }
    for (i, v) in results.iter().enumerate() {
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
    let n = inputs.len() * 2;
    let mut b_buf = vec![0u8; n * 4];
    let mut c_buf = vec![0u8; n * 4];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    invoke_with_gids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &[(0, 0, 0)]);
    assert_eq!(b_buf, c_buf, "diverge on sin/cos extended range");
    let read = |i: usize| -> f32 {
        f32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    let tol = 2e-3;
    let expected = [
        (PI,            0.0,  -1.0),
        (3.0*PI/2.0,   -1.0,   0.0),
        (2.0*PI,        0.0,   1.0),
        (5.0*PI/2.0,    1.0,   0.0),
        (-PI/2.0,      -1.0,   0.0),
        (-PI,           0.0,  -1.0),
    ];
    for (i, (x, esin, ecos)) in expected.iter().enumerate() {
        let got_s = read(i*2);
        let got_c = read(i*2 + 1);
        assert!((got_s - esin).abs() < tol,
            "sin({}) ≈ {} (got {})", x, esin, got_s);
        assert!((got_c - ecos).abs() < tol,
            "cos({}) ≈ {} (got {})", x, ecos, got_c);
    }
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
fn differential_glsl_vec4_mix() {
    // mix(vec4(0), vec4(100), 0.25) = vec4(25).
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
    let v_lo = mk_vec(&mut b, [0.0, 10.0, 0.0, 0.0]);
    let v_hi = mk_vec(&mut b, [100.0, 20.0, 50.0, 1.0]);
    let v_t  = mk_vec(&mut b, [0.25, 0.5, 0.75, 1.0]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let r = b.ext_inst(vec4, None, std_450, 46,
        vec![rspirv::dr::Operand::IdRef(v_lo),
             rspirv::dr::Operand::IdRef(v_hi),
             rspirv::dr::Operand::IdRef(v_t)]).unwrap();
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
    assert_eq!(b_buf, c_buf, "diverge on vec4 mix");
    let read = |off: usize| -> f32 {
        f32::from_le_bytes(b_buf[off..off+4].try_into().unwrap())
    };
    // mix(lo, hi, t) = lo + t*(hi - lo)
    // x: mix(0, 100, 0.25) = 25
    // y: mix(10, 20, 0.5)  = 15
    // z: mix(0, 50, 0.75)  = 37.5
    // w: mix(0, 1, 1.0)    = 1
    assert_eq!([read(0), read(4), read(8), read(12)],
        [25.0, 15.0, 37.5, 1.0]);
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
fn differential_workgroup_shared_memory_accumulator() {
    // `shared uint acc;` -- each invocation in a workgroup
    // does `acc = acc + 1; ssbo[lid.x] = acc;`.  Because
    // invocations within a workgroup run serially, invocation
    // k (0-indexed) observes acc == k+1, so ssbo ends up
    // [1, 2, 3, 4] for a 4-wide workgroup.  This exercises:
    //  - StorageClass::Workgroup OpVariable allocation,
    //  - the workgroup_buf ABI slot (10th cs_main arg),
    //  - shared-memory persistence across invocations.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration,
        ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let v3u    = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    // SSBO output.
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u_ssbo = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // Workgroup-shared accumulator.
    let ptr_u_wg = b.type_pointer(None, StorageClass::Workgroup, u32_ty);
    let acc = b.variable(ptr_u_wg, None, StorageClass::Workgroup, None);
    // LocalInvocationId builtin.
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let lid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(lid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::LocalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // acc = acc + 1
    let cur = b.load(u32_ty, None, acc, None, vec![]).unwrap();
    let next = b.i_add(u32_ty, None, cur, c_one).unwrap();
    b.store(acc, next, None, vec![]).unwrap();
    // lid_x = LocalInvocationId.x -- load the whole vec3
    // builtin, then composite-extract lane 0 (the frontend
    // lowers OpLoad of a builtin var to Op::LoadBuiltin).
    let lid_vec = b.load(v3u, None, lid_var, None, vec![]).unwrap();
    let lid_x = b.composite_extract(u32_ty, None, lid_vec, vec![0]).unwrap();
    // ssbo[lid_x] = acc
    let dst = b.access_chain(ptr_u_ssbo, None, ssbo, vec![c_zero, lid_x]).unwrap();
    b.store(dst, next, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo, acc, lid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    // Four local invocations of a single workgroup.
    let lids: Vec<(u32, u32, u32)> =
        (0..4u32).map(|i| (i, 0, 0)).collect();
    invoke_cs_main_with_lids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &lids);
    invoke_cs_main_with_lids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &lids);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on workgroup shared memory");
    for i in 0..4u32 {
        let v = u32::from_le_bytes(
            b_buf[(i as usize)*4..(i as usize)*4+4].try_into().unwrap());
        assert_eq!(v, i + 1,
            "ssbo[{i}] should hold {} (serial accumulation); got {v}", i + 1);
    }
}

/// Run a compute shader that uses storage images.  Builds a
/// v1 image descriptor table (helper pointers + one
/// `ImageDesc` slot) and calls `cs_main` once per workgroup
/// id with the table base in the X0 (`uniforms`) slot.
/// `img_data` is the storage image's row-major pixel buffer.
fn invoke_compute_image(
    spv: &[u8], use_bespoke: bool, dir: &Path, name: &str,
    img_data: &mut [u8], width: u32, height: u32, depth: u32,
    format: u32, gids: &[(u32, u32, u32)],
) {
    use atrium_spv_runtime::{
        ImageDesc, image_table_buffer, write_image_helper_pointers,
        write_image_descriptor_slot,
        atrium_img_read_2d, atrium_img_write_2d,
        atrium_img_read_3d, atrium_img_write_3d,
    };
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
    let lib = unsafe { libloading::Library::new(&lib_path) }.expect("dlopen");
    let cs_main: libloading::Symbol<CsMain> = unsafe {
        lib.get(b"atrium_cs_main").expect("atrium_cs_main symbol")
    };
    let bpt = match format { 2 => 16, _ => 4 }; // Rgba32Float=16
    let img = ImageDesc {
        data: img_data.as_mut_ptr(),
        width, height,
        stride_bytes: width * bpt,
        format,
        depth,
        slice_bytes: width * height * bpt,
    };
    let mut table = image_table_buffer(1);
    unsafe {
        write_image_helper_pointers(
            &mut table,
            atrium_img_read_2d, atrium_img_write_2d,
            atrium_img_read_3d, atrium_img_write_3d);
        write_image_descriptor_slot(&mut table, 0, &img as *const _);
    }
    let table_base = table.as_ptr() as *const u8;
    for &(gx, gy, gz) in gids {
        unsafe {
            cs_main(table_base, std::ptr::null(), std::ptr::null_mut(),
                    gx, gy, gz, 0, 0, 0, std::ptr::null_mut());
        }
    }
}

#[test]
fn differential_storage_image_write() {
    // A compute shader that writes
    //   img[gid.x, gid.y] = vec4(gid.x, gid.y, 0.5, 1.0)
    // to a 3×2 Rgba32Float storage image.  One invocation
    // per texel; verify the buffer and that bespoke vs
    // cranelift produce byte-identical results.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let v3u    = b.type_vector(u32_ty, 3);
    let v2u    = b.type_vector(u32_ty, 2);
    let v4f    = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    // Storage image: 2D, f32 sampled type, sampled=2, Rgba32f.
    let img_ty = b.type_image(f32_ty, Dim::Dim2D, 0, 0, 0, 2,
        ImageFormat::Rgba32f, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // GlobalInvocationId.
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c_one  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    let gx = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let gy = b.composite_extract(u32_ty, None, gid, vec![1]).unwrap();
    let coord = b.composite_construct(v2u, None, vec![gx, gy]).unwrap();
    let fx = b.convert_u_to_f(f32_ty, None, gx).unwrap();
    let fy = b.convert_u_to_f(f32_ty, None, gy).unwrap();
    let texel = b.composite_construct(v4f, None,
        vec![fx, fy, c_half, c_one]).unwrap();
    let img = b.load(img_ty, None, img_var, None, vec![]).unwrap();
    b.image_write(img, coord, texel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h) = (3u32, 2u32);
    let gids: Vec<(u32, u32, u32)> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y, 0u32)))
        .collect();
    let dir = TempDir::new().unwrap();
    let mut b_img = vec![0u8; (w * h * 16) as usize];
    let mut c_img = vec![0u8; (w * h * 16) as usize];
    invoke_compute_image(&spv, true,  dir.path(), "b",
        &mut b_img, w, h, 1, 2, &gids);
    invoke_compute_image(&spv, false, dir.path(), "c",
        &mut c_img, w, h, 1, 2, &gids);
    assert_eq!(b_img, c_img, "storage-image write diverges between backends");
    // Verify each texel.
    let read = |buf: &[u8], x: u32, y: u32, c: usize| -> f32 {
        let off = ((y * w + x) as usize) * 16 + c * 4;
        f32::from_le_bytes(buf[off..off+4].try_into().unwrap())
    };
    for y in 0..h {
        for x in 0..w {
            assert_eq!(read(&b_img, x, y, 0), x as f32, "texel ({x},{y}).r");
            assert_eq!(read(&b_img, x, y, 1), y as f32, "texel ({x},{y}).g");
            assert_eq!(read(&b_img, x, y, 2), 0.5,      "texel ({x},{y}).b");
            assert_eq!(read(&b_img, x, y, 3), 1.0,      "texel ({x},{y}).a");
        }
    }
}

#[test]
fn differential_storage_image_query_size_2d() {
    // imageSize on a 5x3 R32f image2D, then imageStore the
    // width into texel (0,0) and the height into texel (1,0).
    // After one invocation per write, texel(0,0) = 5.0 and
    // texel(1,0) = 3.0 (R32Float, one channel).
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let v3u    = b.type_vector(u32_ty, 3);
    let v2u    = b.type_vector(u32_ty, 2);
    let v4f    = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let img_ty = b.type_image(f32_ty, Dim::Dim2D, 0, 0, 0, 2,
        ImageFormat::R32f, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero_f = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_zero_u = b.constant_bit32(u32_ty, 0);
    let bool_ty = b.type_bool();
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let img = b.load(img_ty, None, img_var, None, vec![]).unwrap();
    let sz  = b.image_query_size(v2u, None, img).unwrap();
    let sx  = b.composite_extract(u32_ty, None, sz, vec![0]).unwrap();
    let sy  = b.composite_extract(u32_ty, None, sz, vec![1]).unwrap();
    let fx  = b.convert_u_to_f(f32_ty, None, sx).unwrap();
    let fy  = b.convert_u_to_f(f32_ty, None, sy).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    let gx  = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    // Pick width or height to write based on gid.x (0 -> w, 1 -> h).
    let cond = b.i_equal(bool_ty, None, gx, c_zero_u).unwrap();
    let pick = b.select(f32_ty, None, cond, fx, fy).unwrap();
    let coord = b.composite_construct(v2u, None, vec![gx, c_zero_u]).unwrap();
    let texel = b.composite_construct(v4f, None,
        vec![pick, c_zero_f, c_zero_f, c_zero_f]).unwrap();
    b.image_write(img, coord, texel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h) = (5u32, 3u32);
    let gids = [(0u32, 0u32, 0u32), (1u32, 0u32, 0u32)];
    let mut b_img = vec![0u8; (w * h * 4) as usize];
    let mut c_img = vec![0u8; (w * h * 4) as usize];
    let dir = TempDir::new().unwrap();
    invoke_compute_image(&spv, true,  dir.path(), "b",
        &mut b_img, w, h, 1, 1, &gids);
    invoke_compute_image(&spv, false, dir.path(), "c",
        &mut c_img, w, h, 1, 1, &gids);
    assert_eq!(b_img, c_img,
        "imageSize 2D diverges between backends");
    let read = |buf: &[u8], x: u32, y: u32| -> f32 {
        let off = ((y * w + x) * 4) as usize;
        f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    };
    assert_eq!(read(&b_img, 0, 0), 5.0, "imageSize(img).x");
    assert_eq!(read(&b_img, 1, 0), 3.0, "imageSize(img).y");
}

#[test]
fn differential_storage_image_query_size_3d() {
    // imageSize on a 2x3x4 R32f image3D returns uvec3.  Three
    // invocations each write one component (w/h/d as f32) into
    // texels (0,0,0), (1,0,0), (0,1,0) respectively.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let v3u    = b.type_vector(u32_ty, 3);
    let v4f    = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let img_ty = b.type_image(f32_ty, Dim::Dim3D, 0, 0, 0, 2,
        ImageFormat::R32f, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero_f = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let bool_ty = b.type_bool();
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let img = b.load(img_ty, None, img_var, None, vec![]).unwrap();
    let sz  = b.image_query_size(v3u, None, img).unwrap();
    let sx  = b.composite_extract(u32_ty, None, sz, vec![0]).unwrap();
    let sy  = b.composite_extract(u32_ty, None, sz, vec![1]).unwrap();
    let sz_ = b.composite_extract(u32_ty, None, sz, vec![2]).unwrap();
    let fx  = b.convert_u_to_f(f32_ty, None, sx).unwrap();
    let fy  = b.convert_u_to_f(f32_ty, None, sy).unwrap();
    let fz  = b.convert_u_to_f(f32_ty, None, sz_).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    let gx  = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let gy  = b.composite_extract(u32_ty, None, gid, vec![1]).unwrap();
    let c0  = b.constant_bit32(u32_ty, 0);
    let c1  = b.constant_bit32(u32_ty, 1);
    // gid==(0,0,0) -> fx; (1,0,0) -> fy; (0,1,0) -> fz.
    let eq_gx1 = b.i_equal(bool_ty, None, gx, c1).unwrap();
    let eq_gy1 = b.i_equal(bool_ty, None, gy, c1).unwrap();
    // val = eq_gy1 ? fz : (eq_gx1 ? fy : fx)
    let inner = b.select(f32_ty, None, eq_gx1, fy, fx).unwrap();
    let val   = b.select(f32_ty, None, eq_gy1, fz, inner).unwrap();
    let coord = b.composite_construct(v3u, None, vec![gx, gy, c0]).unwrap();
    let texel = b.composite_construct(v4f, None,
        vec![val, c_zero_f, c_zero_f, c_zero_f]).unwrap();
    b.image_write(img, coord, texel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h, d) = (2u32, 3u32, 4u32);
    let gids = [(0u32, 0u32, 0u32), (1u32, 0u32, 0u32), (0u32, 1u32, 0u32)];
    let mut b_img = vec![0u8; (w * h * d * 4) as usize];
    let mut c_img = vec![0u8; (w * h * d * 4) as usize];
    let dir = TempDir::new().unwrap();
    invoke_compute_image(&spv, true,  dir.path(), "b",
        &mut b_img, w, h, d, 1, &gids);
    invoke_compute_image(&spv, false, dir.path(), "c",
        &mut c_img, w, h, d, 1, &gids);
    assert_eq!(b_img, c_img,
        "imageSize 3D diverges between backends");
    // R32 image -> 4 bytes/texel; slice = w*h*4.
    let read = |buf: &[u8], x: u32, y: u32, z: u32| -> f32 {
        let off = ((z * w * h + y * w + x) * 4) as usize;
        f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    };
    assert_eq!(read(&b_img, 0, 0, 0), 2.0, "imageSize(img3D).x");
    assert_eq!(read(&b_img, 1, 0, 0), 3.0, "imageSize(img3D).y");
    assert_eq!(read(&b_img, 0, 1, 0), 4.0, "imageSize(img3D).z");
}

#[test]
fn differential_storage_image_3d_write() {
    // image3D imageStore via the 3D helper path: a 2×2×2
    // Rgba32f storage image, one invocation per texel writes
    // vec4(gid.x, gid.y, gid.z, 1.0).  Exercises the 3D
    // helper call sequence — bespoke loads helper @ #16
    // (read_3d) / #24 (write_3d) and passes z in W3, rgba
    // in X4; Cranelift goes through the same table via
    // call_indirect with the 5-arg signature.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let v3u    = b.type_vector(u32_ty, 3);
    let v4f    = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    // 3D storage image, f32, sampled=2, Rgba32f.
    let img_ty = b.type_image(f32_ty, Dim::Dim3D, 0, 0, 0, 2,
        ImageFormat::Rgba32f, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    let gx = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let gy = b.composite_extract(u32_ty, None, gid, vec![1]).unwrap();
    let gz = b.composite_extract(u32_ty, None, gid, vec![2]).unwrap();
    let fx = b.convert_u_to_f(f32_ty, None, gx).unwrap();
    let fy = b.convert_u_to_f(f32_ty, None, gy).unwrap();
    let fz = b.convert_u_to_f(f32_ty, None, gz).unwrap();
    let texel = b.composite_construct(v4f, None,
        vec![fx, fy, fz, c_one_f]).unwrap();
    let img = b.load(img_ty, None, img_var, None, vec![]).unwrap();
    b.image_write(img, gid, texel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h, d) = (2u32, 2u32, 2u32);
    let gids: Vec<(u32, u32, u32)> = (0..d).flat_map(|z|
        (0..h).flat_map(move |y| (0..w).map(move |x| (x, y, z)))).collect();
    let mut b_img = vec![0u8; (w * h * d * 16) as usize];
    let mut c_img = vec![0u8; (w * h * d * 16) as usize];
    let dir = TempDir::new().unwrap();
    invoke_compute_image(&spv, true,  dir.path(), "b",
        &mut b_img, w, h, d, 2, &gids);
    invoke_compute_image(&spv, false, dir.path(), "c",
        &mut c_img, w, h, d, 2, &gids);
    assert_eq!(b_img, c_img,
        "image3D write diverges between backends");
    let read = |buf: &[u8], x: u32, y: u32, z: u32, c: usize| -> f32 {
        let off = ((z * w * h + y * w + x) as usize) * 16 + c * 4;
        f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    };
    for z in 0..d { for y in 0..h { for x in 0..w {
        assert_eq!(read(&b_img, x, y, z, 0), x as f32);
        assert_eq!(read(&b_img, x, y, z, 1), y as f32);
        assert_eq!(read(&b_img, x, y, z, 2), z as f32);
        assert_eq!(read(&b_img, x, y, z, 3), 1.0);
    }}}
}

#[test]
fn differential_storage_image_atomic_add() {
    // imageAtomicAdd: each invocation forms a texel pointer
    // via OpImageTexelPointer into an R32 storage image and
    // atomicAdd's 1 into it.  Every texel of a 2×2 image is
    // visited 3 times, so each ends at (initial + 3).  The
    // OpImageTexelPointer path is bespoke-only (Cranelift's
    // aarch64 backend can't form the Image-class pointer), so
    // the oracle is the hand-computed final buffer.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, MemorySemantics, Scope, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let v3u    = b.type_vector(u32_ty, 3);
    let v2u    = b.type_vector(u32_ty, 2);
    let void_fn = b.type_function(void, vec![]);
    // Storage image: 2D, u32 sampled type, sampled=2, R32ui.
    let img_ty = b.type_image(u32_ty, Dim::Dim2D, 0, 0, 0, 2,
        ImageFormat::R32ui, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // Pointer-to-texel type (StorageClass Image).
    let ptr_texel = b.type_pointer(None, StorageClass::Image, u32_ty);
    // GlobalInvocationId.
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    let gx = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let gy = b.composite_extract(u32_ty, None, gid, vec![1]).unwrap();
    let coord = b.composite_construct(v2u, None, vec![gx, gy]).unwrap();
    // ptr = OpImageTexelPointer(img_var, coord, sample=0)
    let texel_ptr = b.image_texel_pointer(ptr_texel, None,
        img_var, coord, c_zero).unwrap();
    // atomicAdd(*ptr, 1)
    let _ = b.atomic_i_add(u32_ty, None, texel_ptr,
        c_scope, c_sem, c_one).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h) = (2u32, 2u32);
    // Each texel visited 3 times.
    let mut gids: Vec<(u32, u32, u32)> = Vec::new();
    for _ in 0..3 {
        for y in 0..h { for x in 0..w { gids.push((x, y, 0)); } }
    }
    // Prefill: texel(x,y) = y*w + x.
    let mut img = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 4) as usize;
            img[off..off + 4].copy_from_slice(&(y * w + x).to_le_bytes());
        }
    }
    let dir = TempDir::new().unwrap();
    // format=1 (R32) -> 4 bytes/texel; depth=1 (2D).
    invoke_compute_image(&spv, true, dir.path(), "b",
        &mut img, w, h, 1, 1, &gids);
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 4) as usize;
            let got = u32::from_le_bytes(
                img[off..off + 4].try_into().unwrap());
            let want = y * w + x + 3;
            assert_eq!(got, want,
                "texel ({x},{y}): got {got}, want {want}");
        }
    }
}

#[test]
fn differential_storage_image_atomic_cas() {
    // imageAtomicCompareSwap via the OpImageTexelPointer +
    // OpAtomicCompareExchange chain.  2x1 R32 image; texel
    // (0,0) prefilled with 42 (CAS matches -> swap to 99),
    // texel (1,0) prefilled with 7 (CAS mismatches -> stays).
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, MemorySemantics, Scope, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let v3u    = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let img_ty = b.type_image(u32_ty, Dim::Dim2D, 0, 0, 0, 2,
        ImageFormat::R32ui, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_texel = b.type_pointer(None, StorageClass::Image, u32_ty);
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero   = b.constant_bit32(u32_ty, 0);
    let c_42     = b.constant_bit32(u32_ty, 42);
    let c_99     = b.constant_bit32(u32_ty, 99);
    let c_scope  = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem    = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    let texel_ptr = b.image_texel_pointer(ptr_texel, None,
        img_var, gid, c_zero).unwrap();
    // CAS(*ptr, comparator=42, desired=99) — succeeds only
    // where the texel is exactly 42.
    let _ = b.atomic_compare_exchange(u32_ty, None, texel_ptr,
        c_scope, c_sem, c_sem, c_99, c_42).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h) = (2u32, 1u32);
    let mut img = vec![0u8; (w * h * 4) as usize];
    img[0..4].copy_from_slice(&42u32.to_le_bytes()); // (0,0) -> 42 (match)
    img[4..8].copy_from_slice(&7u32.to_le_bytes());  // (1,0) -> 7  (no match)
    let gids = [(0u32, 0u32, 0u32), (1u32, 0u32, 0u32)];
    let dir = TempDir::new().unwrap();
    invoke_compute_image(&spv, true, dir.path(), "b",
        &mut img, w, h, 1, 1, &gids);
    let got0 = u32::from_le_bytes(img[0..4].try_into().unwrap());
    let got1 = u32::from_le_bytes(img[4..8].try_into().unwrap());
    assert_eq!(got0, 99, "CAS hit: 42 -> 99");
    assert_eq!(got1,  7, "CAS miss: 7 stays");
}

#[test]
fn differential_storage_image_3d_atomic_add() {
    // image3D imageAtomicAdd: a 2×2×2 R32 storage image,
    // texel pointer formed via OpImageTexelPointer with a
    // 3-lane coord (gid.x, gid.y, gid.z).  The bespoke
    // codegen folds in z*slice_bytes off the ImageDesc.
    // Each texel is visited twice -> final = initial + 2.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, Dim,
        ExecutionMode, ExecutionModel, FunctionControl, ImageFormat,
        MemoryModel, MemorySemantics, Scope, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let v3u    = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    // Storage image: 3D, u32 sampled type, sampled=2, R32ui.
    let img_ty = b.type_image(u32_ty, Dim::Dim3D, 0, 0, 0, 2,
        ImageFormat::R32ui, None);
    let ptr_img = b.type_pointer(None, StorageClass::UniformConstant, img_ty);
    let img_var = b.variable(ptr_img, None, StorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_texel = b.type_pointer(None, StorageClass::Image, u32_ty);
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let gid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(v3u, None, gid_var, None, vec![]).unwrap();
    // 3-lane coord -> the texel-pointer codegen sees an image3D.
    let texel_ptr = b.image_texel_pointer(ptr_texel, None,
        img_var, gid, c_zero).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, texel_ptr,
        c_scope, c_sem, c_one).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![img_var, gid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let (w, h, d) = (2u32, 2u32, 2u32);
    // Each texel visited twice.
    let mut gids: Vec<(u32, u32, u32)> = Vec::new();
    for _ in 0..2 {
        for z in 0..d { for y in 0..h { for x in 0..w {
            gids.push((x, y, z));
        }}}
    }
    // Prefill: texel(x,y,z) = z*w*h + y*w + x.
    let mut img = vec![0u8; (w * h * d * 4) as usize];
    for z in 0..d {
        for y in 0..h {
            for x in 0..w {
                let idx = z * w * h + y * w + x;
                let off = (idx * 4) as usize;
                img[off..off + 4].copy_from_slice(&idx.to_le_bytes());
            }
        }
    }
    let dir = TempDir::new().unwrap();
    invoke_compute_image(&spv, true, dir.path(), "b",
        &mut img, w, h, d, 1, &gids);
    for z in 0..d {
        for y in 0..h {
            for x in 0..w {
                let idx = z * w * h + y * w + x;
                let off = (idx * 4) as usize;
                let got = u32::from_le_bytes(
                    img[off..off + 4].try_into().unwrap());
                let want = idx + 2;
                assert_eq!(got, want,
                    "texel ({x},{y},{z}): got {got}, want {want}");
            }
        }
    }
}

#[test]
fn differential_subgroup_ops_size1_degenerate() {
    // Tier-2 runs each workgroup serially on one CPU thread,
    // so subgroupSize=1 and every OpGroupNonUniform* lowers
    // to a trivial expression at frontend time:
    //   subgroupElect()                -> true
    //   subgroupBroadcastFirst(x)      -> x
    //   subgroupAdd(x) [Reduce]        -> x
    //   subgroupExclusiveAdd(x)        -> 0  (identity)
    //   subgroupBallot(true)           -> uvec4(1,0,0,0)
    // Stores to ssbo[0..5] for cross-backend byte-compare.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, GroupOperation, MemoryModel,
        Scope, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.capability(Capability::GroupNonUniform);
    b.capability(Capability::GroupNonUniformArithmetic);
    b.capability(Capability::GroupNonUniformBallot);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let v4u    = b.type_vector(u32_ty, 4);
    let bool_ty = b.type_bool();
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
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
    let c0_u = b.constant_bit32(u32_ty, 0);
    let c1_u = b.constant_bit32(u32_ty, 1);
    let c2_u = b.constant_bit32(u32_ty, 2);
    let c3_u = b.constant_bit32(u32_ty, 3);
    let c4_u = b.constant_bit32(u32_ty, 4);
    let c5_u = b.constant_bit32(u32_ty, 5);
    let c6_u = b.constant_bit32(u32_ty, 6);
    let c7_u = b.constant_bit32(u32_ty, 7);
    let c42_u = b.constant_bit32(u32_ty, 42);
    let c_true = b.constant_true(bool_ty);
    let c_scope = b.constant_bit32(u32_ty, Scope::Subgroup as u32);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // ssbo[0] = subgroupElect() ? 1 : 0   -> 1
    let elect = b.group_non_uniform_elect(bool_ty, None, c_scope).unwrap();
    let sel0 = b.select(u32_ty, None, elect, c1_u, c0_u).unwrap();
    let p0 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c0_u]).unwrap();
    b.store(p0, sel0, None, vec![]).unwrap();
    // ssbo[1] = subgroupBroadcastFirst(42)   -> 42
    let bf = b.group_non_uniform_broadcast_first(
        u32_ty, None, c_scope, c42_u).unwrap();
    let p1 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c1_u]).unwrap();
    b.store(p1, bf, None, vec![]).unwrap();
    // ssbo[2] = subgroupAdd(42) [Reduce]     -> 42
    let red = b.group_non_uniform_i_add(
        u32_ty, None, c_scope, GroupOperation::Reduce, c42_u, None).unwrap();
    let p2 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c2_u]).unwrap();
    b.store(p2, red, None, vec![]).unwrap();
    // ssbo[3] = subgroupExclusiveAdd(42)     -> 0 (identity)
    let exc = b.group_non_uniform_i_add(
        u32_ty, None, c_scope, GroupOperation::ExclusiveScan,
        c42_u, None).unwrap();
    let p3 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c3_u]).unwrap();
    b.store(p3, exc, None, vec![]).unwrap();
    // ssbo[4..8] = subgroupBallot(true)      -> (1,0,0,0)
    let bal = b.group_non_uniform_ballot(v4u, None, c_scope, c_true).unwrap();
    let bal0 = b.composite_extract(u32_ty, None, bal, vec![0]).unwrap();
    let bal1 = b.composite_extract(u32_ty, None, bal, vec![1]).unwrap();
    let bal2 = b.composite_extract(u32_ty, None, bal, vec![2]).unwrap();
    let bal3 = b.composite_extract(u32_ty, None, bal, vec![3]).unwrap();
    let p4 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c4_u]).unwrap();
    b.store(p4, bal0, None, vec![]).unwrap();
    let p5 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c5_u]).unwrap();
    b.store(p5, bal1, None, vec![]).unwrap();
    let p6 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c6_u]).unwrap();
    b.store(p6, bal2, None, vec![]).unwrap();
    let p7 = b.access_chain(ptr_u, None, ssbo, vec![c0_u, c7_u]).unwrap();
    b.store(p7, bal3, None, vec![]).unwrap();
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
    invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
    invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
    assert_eq!(b_buf, c_buf,
        "subgroup-op lowering diverges between backends");
    let at = |i: usize| u32::from_le_bytes(
        b_buf[i*4..i*4+4].try_into().unwrap());
    assert_eq!(at(0), 1, "subgroupElect");
    assert_eq!(at(1), 42, "subgroupBroadcastFirst(42)");
    assert_eq!(at(2), 42, "subgroupAdd(42) Reduce");
    assert_eq!(at(3), 0,  "subgroupExclusiveAdd(42)");
    assert_eq!(at(4), 1,  "subgroupBallot(true).x");
    assert_eq!(at(5), 0,  "subgroupBallot(true).y");
    assert_eq!(at(6), 0,  "subgroupBallot(true).z");
    assert_eq!(at(7), 0,  "subgroupBallot(true).w");
}

#[test]
fn differential_spec_constants_host_overrides() {
    // Same shader shape as differential_spec_constants_default_values,
    // but compiled with translate_with_spec_overrides({0: 99, 1: 0}).
    // -> ssbo[0] = 99           (overrides default 7)
    //    ssbo[1] = 0            (SpecConstantTrue overridden to false)
    //    ssbo[2] = 99 * 2 = 198 (override flows through IMul)
    use atrium_spv_frontend::SpecOverrides;
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
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
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
    let sc_int = b.spec_constant_bit32(i32_ty, 7u32);
    b.decorate(sc_int, Decoration::SpecId,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let sc_bool = b.spec_constant_true(bool_ty);
    b.decorate(sc_bool, Decoration::SpecId,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let c_two_u = b.constant_bit32(u32_ty, 2);
    let c_two_i = b.constant_bit32(i32_ty, 2u32);
    let c_zero_u = b.constant_bit32(u32_ty, 0);
    let c_one_u  = b.constant_bit32(u32_ty, 1);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sc_u = b.bitcast(u32_ty, None, sc_int).unwrap();
    let p0 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(p0, sc_u, None, vec![]).unwrap();
    let sel = b.select(u32_ty, None, sc_bool, c_one_u, c_zero_u).unwrap();
    let p1 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_one]).unwrap();
    b.store(p1, sel, None, vec![]).unwrap();
    let prod = b.i_mul(i32_ty, None, sc_int, c_two_i).unwrap();
    let prod_u = b.bitcast(u32_ty, None, prod).unwrap();
    let p2 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_two_u]).unwrap();
    b.store(p2, prod_u, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    // Override SpecId 0 -> 99, SpecId 1 -> 0 (false).
    let mut overrides = SpecOverrides::new();
    overrides.insert(0, 99);
    overrides.insert(1, 0);

    // Compile through both backends using the overrides path,
    // then dlopen + run inline.  Mirrors invoke()'s shape.
    let run_one = |use_bespoke: bool, name: &str,
                   out_ptr: *mut u8| {
        let dir = TempDir::new().unwrap();
        let module = atrium_spv_frontend::translate_with_spec_overrides(
            &spv, &overrides).expect("frontend");
        let obj = if use_bespoke {
            let t = if cfg!(target_os = "macos") {
                BespokeTarget::Aarch64Darwin
            } else { BespokeTarget::Aarch64FreeBSD };
            bespoke_compile(&module, t).expect("bespoke compile").object
        } else {
            cranelift_compile(&module, CraneliftTarget::host())
                .expect("cranelift compile").object
        };
        let obj_path = dir.path().join(format!("{name}.o"));
        std::fs::write(&obj_path, &obj).unwrap();
        let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
        let lib_path = dir.path().join(format!("{name}.{ext}"));
        link_to_shared_library(&obj_path, &lib_path).expect("link");
        let lib = unsafe { libloading::Library::new(&lib_path) }
            .expect("dlopen");
        let cs_main: libloading::Symbol<CsMain> = unsafe {
            lib.get(b"atrium_cs_main").expect("atrium_cs_main symbol")
        };
        unsafe {
            cs_main(std::ptr::null(), std::ptr::null(), out_ptr,
                0, 0, 0, 0, 0, 0, std::ptr::null_mut());
        }
        // Drop in order: lib must outlive cs_main use.
        drop(cs_main);
        drop(lib);
    };
    let mut b_buf = vec![0u8; 12];
    let mut c_buf = vec![0u8; 12];
    run_one(true,  "b_ov", b_buf.as_mut_ptr());
    run_one(false, "c_ov", c_buf.as_mut_ptr());
    assert_eq!(b_buf, c_buf,
        "spec-constant overrides diverge between backends");
    let at = |i: usize| u32::from_le_bytes(
        b_buf[i*4..i*4+4].try_into().unwrap());
    assert_eq!(at(0),  99, "scalar SpecConstant override");
    assert_eq!(at(1),   0, "SpecConstantTrue overridden to false");
    assert_eq!(at(2), 198, "override flows through IMul: 99*2");
}

#[test]
fn differential_spec_constants_default_values() {
    // OpSpecConstant{,True,False,Composite} -- when no
    // VkSpecializationInfo is supplied, Tier-2 v1 uses the
    // SPIR-V-declared default value.  This test declares a
    // scalar i32 spec const (default=7), a scalar bool spec
    // const (default=true), and writes:
    //   ssbo[0] = scalar_int_default   (expect 7)
    //   ssbo[1] = scalar_bool_default ? 1 : 0   (expect 1)
    //   ssbo[2] = scalar_int_default * 2  (regular OpIMul on
    //             the spec constant + a plain constant 2)
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
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
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
    // Spec constants -- declared with default values.
    let sc_int = b.spec_constant_bit32(i32_ty, 7u32);
    b.decorate(sc_int, Decoration::SpecId,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let sc_bool = b.spec_constant_true(bool_ty);
    b.decorate(sc_bool, Decoration::SpecId,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let c_two_u = b.constant_bit32(u32_ty, 2);
    let c_two_i = b.constant_bit32(i32_ty, 2u32);
    let c_zero_u = b.constant_bit32(u32_ty, 0);
    let c_one_u  = b.constant_bit32(u32_ty, 1);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // ssbo[0] = bitcast<u32>(sc_int)  (i32 7 -> u32 7)
    let sc_u = b.bitcast(u32_ty, None, sc_int).unwrap();
    let p0 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(p0, sc_u, None, vec![]).unwrap();
    // ssbo[1] = sc_bool ? 1u : 0u
    let sel = b.select(u32_ty, None, sc_bool, c_one_u, c_zero_u).unwrap();
    let p1 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_one]).unwrap();
    b.store(p1, sel, None, vec![]).unwrap();
    // ssbo[2] = bitcast<u32>(sc_int * 2)
    let prod = b.i_mul(i32_ty, None, sc_int, c_two_i).unwrap();
    let prod_u = b.bitcast(u32_ty, None, prod).unwrap();
    let p2 = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_two_u]).unwrap();
    b.store(p2, prod_u, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 12];
    let mut c_buf = vec![0u8; 12];
    invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
    invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
    assert_eq!(b_buf, c_buf,
        "spec-constant default-values diverge between backends");
    let at = |i: usize| u32::from_le_bytes(
        b_buf[i*4..i*4+4].try_into().unwrap());
    assert_eq!(at(0),  7, "scalar SpecConstant int default");
    assert_eq!(at(1),  1, "SpecConstantTrue -> 1");
    assert_eq!(at(2), 14, "SpecConstant int * 2 = 14");
}

#[test]
fn differential_glsl_radians_degrees() {
    // Radians(180) ≈ π; Degrees(π) ≈ 180.  Round-trip a few
    // angles through both and compare to f32 truth.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    // Each invocation reads ssbo[i*2], applies Radians then
    // Degrees (round-trip), and writes to ssbo[i*2+1].
    let cases: &[f32] = &[0.0, 45.0, 90.0, 180.0, 270.0, -123.5];
    for &deg in cases {
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 3);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let std_450 = b.ext_inst_import("GLSL.std.450");
        let void   = b.type_void();
        let u32_ty = b.type_int(32, 0);
        let f32_ty = b.type_float(32, None);
        let void_fn = b.type_function(void, vec![]);
        let rt = b.type_runtime_array(f32_ty);
        b.decorate(rt, Decoration::ArrayStride,
            vec![rspirv::dr::Operand::LiteralBit32(4)]);
        let s = b.type_struct(vec![rt]);
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
        let c_zero = b.constant_bit32(u32_ty, 0);
        let c_one  = b.constant_bit32(u32_ty, 1);
        let c_two  = b.constant_bit32(u32_ty, 2);
        let c_in = b.constant_bit32(f32_ty, deg.to_bits());
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        // r = Radians(deg)
        let r = b.ext_inst(f32_ty, None, std_450, 11,
            vec![rspirv::dr::Operand::IdRef(c_in)]).unwrap();
        // d = Degrees(r)
        let d = b.ext_inst(f32_ty, None, std_450, 12,
            vec![rspirv::dr::Operand::IdRef(r)]).unwrap();
        // ssbo[0] = r; ssbo[1] = d; ssbo[2] = deg (oracle).
        let p0 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_zero]).unwrap();
        b.store(p0, r, None, vec![]).unwrap();
        let p1 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_one]).unwrap();
        b.store(p1, d, None, vec![]).unwrap();
        let p2 = b.access_chain(ptr_f, None, ssbo, vec![c_zero, c_two]).unwrap();
        b.store(p2, c_in, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
        b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
        let words: Vec<u32> = b.module().assemble();
        let mut spv = Vec::with_capacity(words.len() * 4);
        for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
        let dir = TempDir::new().unwrap();
        let mut b_buf = vec![0u8; 12];
        let mut c_buf = vec![0u8; 12];
        invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
        invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
        assert_eq!(b_buf, c_buf,
            "Radians/Degrees diverges for deg={deg}");
        let f32_at = |buf: &[u8], i: usize| -> f32 {
            f32::from_le_bytes(buf[i*4..i*4+4].try_into().unwrap())
        };
        let want_rad = deg * (std::f32::consts::PI / 180.0);
        let want_deg = want_rad * (180.0 / std::f32::consts::PI);
        let got_rad = f32_at(&b_buf, 0);
        let got_deg = f32_at(&b_buf, 1);
        let tol = (deg.abs() * 1e-5).max(1e-5);
        assert!((got_rad - want_rad).abs() <= tol,
            "Radians({deg}): got {got_rad}, want {want_rad}");
        assert!((got_deg - deg).abs() <= tol,
            "Degrees(Radians({deg})): got {got_deg}, want {deg}");
        let _ = want_deg;
    }
}

#[test]
fn differential_glsl_pack_unpack_half2x16() {
    // packHalf2x16(vec2(a,b)) -> u32, then unpackHalf2x16
    // back to vec2.  Inputs chosen exactly representable in
    // f16 so the round-trip is bit-exact:
    //   1.0 -> 0x3C00, 0.5 -> 0x3800, -1.5 -> 0xBE00, 2.0 -> 0x4000
    // ssbo[0] = packHalf2x16(1.0, 0.5)            (expect 0x38003C00)
    // ssbo[1] = unpack(...).x                     (expect 1.0)
    // ssbo[2] = unpack(...).y                     (expect 0.5)
    // ssbo[3] = packHalf2x16(-1.5, 2.0)           (expect 0x4000BE00)
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
    let v2f    = b.type_vector(f32_ty, 2);
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_1    = b.constant_bit32(u32_ty, 1);
    let c_2    = b.constant_bit32(u32_ty, 2);
    let c_3    = b.constant_bit32(u32_ty, 3);
    let f_1    = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let f_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let f_n15  = b.constant_bit32(f32_ty, (-1.5f32).to_bits());
    let f_2    = b.constant_bit32(f32_ty, 2.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // p = packHalf2x16(vec2(1.0, 0.5))
    let v = b.composite_construct(v2f, None, vec![f_1, f_half]).unwrap();
    let p = b.ext_inst(u32_ty, None, std_450, 58,
        vec![rspirv::dr::Operand::IdRef(v)]).unwrap();
    // u = unpackHalf2x16(p)
    let u = b.ext_inst(v2f, None, std_450, 62,
        vec![rspirv::dr::Operand::IdRef(p)]).unwrap();
    let ux = b.composite_extract(f32_ty, None, u, vec![0]).unwrap();
    let uy = b.composite_extract(f32_ty, None, u, vec![1]).unwrap();
    let ux_bits = b.bitcast(u32_ty, None, ux).unwrap();
    let uy_bits = b.bitcast(u32_ty, None, uy).unwrap();
    // p2 = packHalf2x16(vec2(-1.5, 2.0))
    let v2 = b.composite_construct(v2f, None, vec![f_n15, f_2]).unwrap();
    let p2 = b.ext_inst(u32_ty, None, std_450, 58,
        vec![rspirv::dr::Operand::IdRef(v2)]).unwrap();
    for (slot, val) in [(c_zero, p), (c_1, ux_bits), (c_2, uy_bits), (c_3, p2)] {
        let d = b.access_chain(ptr_u, None, ssbo, vec![c_zero, slot]).unwrap();
        b.store(d, val, None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
    let dir = TempDir::new().unwrap();
    // Bespoke-only: PackHalf2x16/UnpackHalf2x16 use the ARM
    // FCVT half-precision instructions; Cranelift's aarch64
    // backend can't lower f16 conversion, so this op is
    // bespoke-path-only.  The hand-computed f16 bit patterns
    // below are the correctness oracle.
    let mut b_buf = vec![0u8; 16];
    invoke_with_gids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &[(0, 0, 0)]);
    let u32_at = |i: usize| -> u32 {
        u32::from_le_bytes(b_buf[i*4..i*4+4].try_into().unwrap())
    };
    let f32_at = |i: usize| -> f32 { f32::from_bits(u32_at(i)) };
    assert_eq!(u32_at(0), 0x3800_3C00, "packHalf2x16(1.0,0.5)");
    assert_eq!(f32_at(1), 1.0, "unpack .x");
    assert_eq!(f32_at(2), 0.5, "unpack .y");
    assert_eq!(u32_at(3), 0x4000_BE00, "packHalf2x16(-1.5,2.0)");
}

#[test]
fn differential_op_isnan_isinf() {
    // OpIsNan / OpIsInf are core SPIR-V ops.  The shader
    // applies one of them to a constant f32, OpSelects the
    // bool result to 1u/0u, and stores to ssbo[0].
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    // (label, opcode "nan"|"inf", input, expected)
    let cases: &[(&str, &str, f32, u32)] = &[
        ("isnan(0)",    "nan", 0.0,                0),
        ("isnan(1)",    "nan", 1.0,                0),
        ("isnan(inf)",  "nan", f32::INFINITY,      0),
        ("isnan(nan)",  "nan", f32::NAN,           1),
        ("isinf(0)",    "inf", 0.0,                0),
        ("isinf(1)",    "inf", 1.0,                0),
        ("isinf(inf)",  "inf", f32::INFINITY,      1),
        ("isinf(-inf)", "inf", f32::NEG_INFINITY,  1),
        ("isinf(nan)",  "inf", f32::NAN,           0),
    ];
    for &(label, op, input, expected) in cases {
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 3);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void   = b.type_void();
        let u32_ty = b.type_int(32, 0);
        let f32_ty = b.type_float(32, None);
        let bool_ty = b.type_bool();
        let void_fn = b.type_function(void, vec![]);
        let rt = b.type_runtime_array(u32_ty);
        b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
        let s = b.type_struct(vec![rt]);
        b.decorate(s, Decoration::Block, vec![]);
        b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
        let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
        let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
        b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let c_zero = b.constant_bit32(u32_ty, 0);
        let c_one  = b.constant_bit32(u32_ty, 1);
        let c_in   = b.constant_bit32(f32_ty, input.to_bits());
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let cond = if op == "nan" {
            b.is_nan(bool_ty, None, c_in).unwrap()
        } else {
            b.is_inf(bool_ty, None, c_in).unwrap()
        };
        let sel = b.select(u32_ty, None, cond, c_one, c_zero).unwrap();
        let d = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
        b.store(d, sel, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
        b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
        let words: Vec<u32> = b.module().assemble();
        let mut spv = Vec::with_capacity(words.len() * 4);
        for w in words { spv.extend_from_slice(&w.to_le_bytes()); }
        let dir = TempDir::new().unwrap();
        let mut b_buf = vec![0u8; 4];
        let mut c_buf = vec![0u8; 4];
        invoke(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr());
        invoke(&spv, false, dir.path(), "c", c_buf.as_mut_ptr());
        assert_eq!(b_buf, c_buf, "{label}: bespoke vs cranelift diverge");
        let got = u32::from_le_bytes(b_buf[0..4].try_into().unwrap());
        assert_eq!(got, expected, "{label}: got {got}, want {expected}");
    }
}

#[test]
fn differential_workgroup_shared_array() {
    // `shared uint tile[4];` -- each invocation writes
    // `tile[lid] = lid + 1`, then `ssbo[lid] = tile[lid] +
    // tile[0]`.  Serial execution means invocation 0 sets
    // tile[0]=1 first, so every later invocation's tile[0]
    // read sees 1.  Result: ssbo == [2, 3, 4, 5].  Exercises
    // a workgroup-storage ARRAY (dynamic + constant index).
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration,
        ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let v3u    = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    // SSBO output (runtime array).
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride, vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u_ssbo = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // Workgroup-shared uint[4].
    let c_four = b.constant_bit32(u32_ty, 4);
    let arr_ty = b.type_array(u32_ty, c_four);
    let ptr_arr_wg = b.type_pointer(None, StorageClass::Workgroup, arr_ty);
    let tile = b.variable(ptr_arr_wg, None, StorageClass::Workgroup, None);
    let ptr_u_wg = b.type_pointer(None, StorageClass::Workgroup, u32_ty);
    // LocalInvocationId builtin.
    let ptr_v3u_in = b.type_pointer(None, StorageClass::Input, v3u);
    let lid_var = b.variable(ptr_v3u_in, None, StorageClass::Input, None);
    b.decorate(lid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::LocalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_one  = b.constant_bit32(u32_ty, 1);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // lid_x
    let lid_vec = b.load(v3u, None, lid_var, None, vec![]).unwrap();
    let lid_x = b.composite_extract(u32_ty, None, lid_vec, vec![0]).unwrap();
    // tile[lid_x] = lid_x + 1
    let lidp1 = b.i_add(u32_ty, None, lid_x, c_one).unwrap();
    let tile_k = b.access_chain(ptr_u_wg, None, tile, vec![lid_x]).unwrap();
    b.store(tile_k, lidp1, None, vec![]).unwrap();
    // v_k = tile[lid_x]
    let tile_k2 = b.access_chain(ptr_u_wg, None, tile, vec![lid_x]).unwrap();
    let v_k = b.load(u32_ty, None, tile_k2, None, vec![]).unwrap();
    // v_0 = tile[0]
    let tile_0 = b.access_chain(ptr_u_wg, None, tile, vec![c_zero]).unwrap();
    let v_0 = b.load(u32_ty, None, tile_0, None, vec![]).unwrap();
    // sum = v_k + v_0
    let sum = b.i_add(u32_ty, None, v_k, v_0).unwrap();
    // ssbo[lid_x] = sum
    let dst = b.access_chain(ptr_u_ssbo, None, ssbo, vec![c_zero, lid_x]).unwrap();
    b.store(dst, sum, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo, tile, lid_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spv = Vec::with_capacity(words.len() * 4);
    for w in words { spv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let mut b_buf = vec![0u8; 16];
    let mut c_buf = vec![0u8; 16];
    let lids: Vec<(u32, u32, u32)> =
        (0..4u32).map(|i| (i, 0, 0)).collect();
    invoke_cs_main_with_lids(&spv, true,  dir.path(), "b", b_buf.as_mut_ptr(), &lids);
    invoke_cs_main_with_lids(&spv, false, dir.path(), "c", c_buf.as_mut_ptr(), &lids);
    assert_eq!(b_buf, c_buf,
        "bespoke vs cranelift diverge on workgroup shared array");
    for i in 0..4u32 {
        let v = u32::from_le_bytes(
            b_buf[(i as usize)*4..(i as usize)*4+4].try_into().unwrap());
        assert_eq!(v, i + 2,
            "ssbo[{i}] = tile[{i}]+tile[0] should be {}; got {v}", i + 2);
    }
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
    // All `lids` belong to one workgroup (wg id 0,0,0), so a
    // single shared-memory scratch buffer spans the loop.
    let wg_bytes = module.functions.first()
        .map(|f| f.workgroup_size as usize).unwrap_or(0);
    let mut wg_buf: Vec<u8> = vec![0u8; wg_bytes];
    let wg_ptr = if wg_bytes == 0 {
        std::ptr::null_mut()
    } else { wg_buf.as_mut_ptr() };
    for &(lx, ly, lz) in lids {
        unsafe {
            cs_main(
                std::ptr::null(), std::ptr::null(), out_ptr,
                0, 0, 0,  // workgroup id
                lx, ly, lz,
                wg_ptr,
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
