//! Cranelift parity: multi-binding compute compiles.
//!
//! The bespoke backend handles two-SSBO compute via a
//! descriptor-table prologue.  If a multi-binding shader
//! ever falls back to Cranelift (because bespoke hits some
//! Unsupported in the body), Cranelift must produce
//! equivalent code -- not silently alias both bindings to
//! params[2].  This test compiles a 2-SSBO CS through
//! Cranelift and asserts the emitted blob is non-empty.
//! Runtime correctness is exercised via the bespoke path
//! (atrium-vk-icd tests/tier2_compute.rs's multi-binding
//! end-to-end); this test just locks in that the Cranelift
//! path doesn't bail.

use atrium_spv_backend_cranelift::{compile, compile_blob, Target};
use atrium_spv_frontend::translate;
use std::process::Command;
use tempfile::TempDir;

fn build_two_binding_constants_cs() -> Vec<u8> {
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
    let s0 = b.type_struct(vec![u32_ty]);
    b.decorate(s0, Decoration::Block, vec![]);
    b.member_decorate(s0, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let s1 = b.type_struct(vec![u32_ty]);
    b.decorate(s1, Decoration::Block, vec![]);
    b.member_decorate(s1, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let p0 = b.type_pointer(None, StorageClass::StorageBuffer, s0);
    let p1 = b.type_pointer(None, StorageClass::StorageBuffer, s1);
    let pu = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let v0 = b.variable(p0, None, StorageClass::StorageBuffer, None);
    b.decorate(v0, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v0, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let v1 = b.variable(p1, None, StorageClass::StorageBuffer, None);
    b.decorate(v1, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v1, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let c_zero    = b.constant_bit32(u32_ty, 0);
    let c_badfood = b.constant_bit32(u32_ty, 0x0BADF00D);
    let c_dead    = b.constant_bit32(u32_ty, 0xDEADBEEF);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let d0 = b.access_chain(pu, None, v0, vec![c_zero]).unwrap();
    b.store(d0, c_badfood, None, vec![]).unwrap();
    let d1 = b.access_chain(pu, None, v1, vec![c_zero]).unwrap();
    b.store(d1, c_dead, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![v0, v1]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn cranelift_compiles_two_binding_ssbo_cs() {
    let spv = build_two_binding_constants_cs();
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };
    let out = compile_blob(&module, target)
        .expect("cranelift should compile a 2-SSBO CS through the \
                 descriptor-table prologue");
    assert!(!out.blob.is_empty());
}

fn link_to_shared_library(
    obj_path: &std::path::Path,
    out_path: &std::path::Path,
) -> Result<(), String> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let output = Command::new("cc").arg(flag).arg("-o").arg(out_path).arg(obj_path)
        .output().map_err(|e| format!("cc: {e}"))?;
    if !output.status.success() {
        return Err(format!("cc failed: {}\n{}",
            output.status, String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}

/// End-to-end Cranelift dlopen test: compile a 2-SSBO CS,
/// link to .so via cc, dlopen, build a descriptor table
/// from two heap buffers, call `atrium_cs_main` once, then
/// verify each binding's buffer holds the right constant.
/// Proves the cranelift descriptor-table prologue isn't
/// just compiling -- it generates correct runtime behaviour
/// matching bespoke's verified path.
#[test]
fn cranelift_two_binding_ssbo_writes_correct_buffers() {
    let spv = build_two_binding_constants_cs();
    let module = translate(&spv).expect("frontend");
    let out = compile(&module, Target::host()).expect("cranelift compile");

    let dir = TempDir::new().expect("tempdir");
    let obj = dir.path().join("cs.o");
    std::fs::write(&obj, &out.object).expect("write obj");
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("cs.{ext}"));
    link_to_shared_library(&obj, &lib_path).expect("link");

    let lib = unsafe { libloading::Library::new(&lib_path) }
        .expect("dlopen");
    type CsMain = unsafe extern "C" fn(
        *const u8, *const u8, *mut u8,
        u32, u32, u32, u32, u32, u32);
    let cs_main: libloading::Symbol<CsMain> = unsafe {
        lib.get(b"atrium_cs_main").expect("atrium_cs_main symbol")
    };

    // Two binding buffers, 16 bytes each.  Build a u64[2]
    // descriptor table with their pointers.
    let mut buf0 = vec![0u8; 16];
    let mut buf1 = vec![0u8; 16];
    let table: [u64; 2] = [buf0.as_mut_ptr() as u64, buf1.as_mut_ptr() as u64];

    unsafe {
        cs_main(
            std::ptr::null(),                  // uniforms
            std::ptr::null(),                  // push_constants
            table.as_ptr() as *mut u8,         // out_buffer = descriptor-table base
            0, 0, 0,                            // wg_id
            0, 0, 0,                            // lid
        );
    }

    let got0 = u32::from_le_bytes(buf0[0..4].try_into().unwrap());
    let got1 = u32::from_le_bytes(buf1[0..4].try_into().unwrap());
    assert_eq!(got0, 0x0BADF00D,
        "binding 0 buffer should be 0x0BADF00D, got {got0:#x}");
    assert_eq!(got1, 0xDEADBEEF,
        "binding 1 buffer should be 0xDEADBEEF, got {got1:#x}");
}
