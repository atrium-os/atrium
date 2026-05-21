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

#[test]
fn differential_six_binding_constant_store() {
    let spv = build_n_binding_constants(6);
    let (b, c) = diff(&spv, 6);
    assert_equal("six-binding (max bespoke cap)", &b, &c);
}
