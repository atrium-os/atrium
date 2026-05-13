//! Phase 2 v3: end-to-end pixel-correctness via dlopen.
//!
//! Pipeline under test:
//!
//! ```text
//!   SPIR-V (rspirv-built constant-colour fragment shader)
//!     │
//!     ├─→  atrium-spv-frontend  →  atrium-spv-ir Module
//!     │                                │
//!     │                                ▼
//!     │                       atrium-spv-backend-cranelift
//!     │                                │
//!     │                                ▼
//!     │                          object bytes
//!     │                                │
//!     │                                ▼
//!     │                      cc -dynamiclib / -shared
//!     │                                │
//!     │                                ▼
//!     │                         .dylib / .so on disk
//!     │                                │
//!     │                                ▼
//!     │                       dlopen via libloading
//!     │                                │
//!     │                                ▼
//!     │                    call atrium_fs_main(.., &mut out_color, ..)
//!     │                                │
//!     │                                ▼
//!     │                       observed [f32; 4]   ──┐
//!     │                                              │
//!     └─→  atrium-spv-tests::interpreter            │
//!              ↓                                     │
//!         expected [f32; 4]                          │
//!              │                                     │
//!              └─── differential equality ───────────┘
//! ```
//!
//! If the Cranelift-compiled shader's output bytes match
//! the interpreter's output bytes, tier-2 v3 has the
//! first real pixel-correctness signal: the whole stack
//! (frontend + backend + linker + dlopen) produces a
//! shader that does what the SPIR-V specified.
//!
//! Test runs only on the host (macOS for now; Linux/FreeBSD
//! when there). Cross-target compile is covered by other
//! tests; this test specifically needs `cc` + dlopen.

use std::process::Command;

use atrium_spv_backend_cranelift::{compile, Target};
use atrium_spv_frontend::translate;
use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};
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

    let c0 = b.constant_bit32(f32_ty, rgba[0].to_bits());
    let c1 = b.constant_bit32(f32_ty, rgba[1].to_bits());
    let c2 = b.constant_bit32(f32_ty, rgba[2].to_bits());
    let c3 = b.constant_bit32(f32_ty, rgba[3].to_bits());
    let color = b.constant_composite(vec4_f32, vec![c0, c1, c2, c3]);

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

/// Link an object file at `obj_path` into a shared library
/// at `out_path`, returning Ok on success. Uses `cc` so
/// the platform-appropriate flags (`-dynamiclib` on macOS,
/// `-shared` elsewhere) come from the host toolchain.
fn link_to_shared_library(
    obj_path: &std::path::Path,
    out_path: &std::path::Path,
) -> Result<(), String> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let output = Command::new("cc")
        .arg(flag)
        .arg("-o")
        .arg(out_path)
        .arg(obj_path)
        .output()
        .map_err(|e| format!("failed to spawn cc: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cc failed: status={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

#[test]
fn cranelift_shader_dlopens_and_writes_expected_rgba() {
    // Cranelift produces a Mach-O object on macOS via the
    // backend's Target::host(); the link path uses `cc`
    // which knows the host platform's dylib conventions.
    let rgba = [1.0f32, 0.5, 0.25, 1.0];
    let spirv = build_constant_color_spirv(rgba);

    // Frontend.
    let module = translate(&spirv).expect("frontend must accept the shader");
    // Backend → object bytes.
    let output = compile(&module, Target::host())
        .expect("backend must compile the shader");
    let object_bytes = output.object;

    // Write object bytes to tempfile, link into a .dylib /
    // .so via `cc`.
    let dir = TempDir::new().expect("tempdir");
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &object_bytes).expect("write obj");
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path)
        .expect("cc must link the object into a shared library");

    // dlopen.
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .expect("dlopen the compiled shader");

    // The Fragment-stage shader-ABI signature (per
    // build_signature in lib.rs + the spec's §4.1):
    //   atrium_fs_main(
    //     in_varyings, uniforms, push_constants,
    //     frag_coord_x, _y, _z, _w,
    //     samples_mask,
    //     out_color,
    //     out_depth,
    //   )
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32,
        u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main")
    }.expect("atrium_fs_main symbol must exist");

    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0,
            0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }

    assert_eq!(out_color, rgba,
        "Cranelift-compiled shader produced wrong colour: got {:?}, expected {:?}",
        out_color, rgba);

    // Differential: run the same SPIR-V through the
    // interpreter and assert it agrees.
    let interp = Interpreter::new(&spirv).expect("interpreter must parse");
    let interp_out = interp.run_fragment(&ShaderInputs::default())
        .expect("interpreter must run");
    assert_eq!(interp_out.pixels.len(), 1);
    assert_eq!(interp_out.pixels[0], rgba,
        "interpreter and shader spec disagree (bug in interpreter or test)");

    // The differential constraint (constraint F1): both
    // production paths agree with the interpreter for
    // every supported shader. Since we just verified
    // out_color == rgba and interp.pixels[0] == rgba,
    // by transitivity Cranelift agrees with interpreter.
    assert_eq!(out_color, interp_out.pixels[0],
        "Cranelift output and interpreter output disagree");
}

#[test]
fn cranelift_shader_dlopens_and_writes_different_rgba() {
    // Sanity guard: a different shader produces different
    // pixels. Catches a regression where the backend
    // silently emits a hardcoded constant instead of
    // honouring the IR's ConstFloat values.
    let rgba = [0.2, 0.7, 0.1, 0.9];
    let spirv = build_constant_color_spirv(rgba);
    let module = translate(&spirv).unwrap();
    let object_bytes = compile(&module, Target::host()).unwrap().object;

    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &object_bytes).unwrap();
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
    assert_eq!(out_color, rgba);
}
