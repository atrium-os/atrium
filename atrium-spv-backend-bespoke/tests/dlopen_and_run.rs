//! End-to-end pixel-correctness: the bespoke ARM64 backend
//! compiles a real SPIR-V constant-colour fragment shader,
//! links into a `.dylib`/`.so`, dlopens, and the resulting
//! `atrium_fs_main` writes the expected RGBA into the
//! output pointer. First proof that the bespoke perf-path
//! produces runnable code.

use std::process::Command;

use atrium_spv_backend_bespoke::{compile, Target};
use atrium_spv_frontend::translate;
use tempfile::TempDir;

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

fn link_to_shared_library(
    obj_path: &std::path::Path,
    lib_path: &std::path::Path,
) -> std::io::Result<()> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let status = Command::new("cc")
        .arg(flag)
        .arg("-o").arg(lib_path)
        .arg(obj_path)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "cc {flag} failed: {status}")));
    }
    Ok(())
}

/// Build `vec4(a+b, a-b, a*b, a/b)` and store. Exercises
/// all four scalar f32 arith ops through the bespoke
/// pipeline.
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

#[test]
fn bespoke_shader_runs_fp_arithmetic() {
    let (a, b) = (0.75f32, 0.25f32);
    let spirv = build_arith_shader(a, b);
    let module = translate(&spirv).expect("translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");

    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();
    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let expected = [a + b, a - b, a * b, a / b];
    assert_eq!(out_color, expected,
        "bespoke FP arith wrong: got {out_color:?}, expected {expected:?}");
}

#[test]
fn bespoke_shader_dlopens_and_writes_constant_color() {
    let rgba = [0.25f32, 0.5, 0.75, 1.0];
    let spirv = build_constant_color_spirv(rgba);

    let module = translate(&spirv).expect("frontend translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");

    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();

    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, rgba,
        "bespoke shader wrong: got {out_color:?}, expected {rgba:?}");
}
