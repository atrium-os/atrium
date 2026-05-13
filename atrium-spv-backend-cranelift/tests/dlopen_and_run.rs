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

/// Build a fragment shader that does:
///   out_color = vec4(a + b, a * b, a - b, c)
/// where a, b, c are SPIR-V constants. Verifies the full
/// arithmetic pipeline (FAdd / FMul / FSub +
/// CompositeConstruct + Store) works end-to-end through
/// frontend → backend → linker → dlopen.
fn build_arithmetic_shader(a: f32, b: f32, c: f32) -> Vec<u8> {
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
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out_vec4 = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let c_a = bld.constant_bit32(f32_ty, a.to_bits());
    let c_b = bld.constant_bit32(f32_ty, b.to_bits());
    let c_c = bld.constant_bit32(f32_ty, c.to_bits());

    let out = bld.variable(ptr_out_vec4, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let sum  = bld.f_add(f32_ty, None, c_a, c_b).unwrap();
    let prod = bld.f_mul(f32_ty, None, c_a, c_b).unwrap();
    let diff = bld.f_sub(f32_ty, None, c_a, c_b).unwrap();
    let color = bld.composite_construct(vec4_f32, None, vec![sum, prod, diff, c_c]).unwrap();
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

/// Build a shader: out_color = vec_a + vec_b
/// where vec_a and vec_b are constant vec4<f32>.
fn build_vec_add_shader(a: [f32; 4], b: [f32; 4]) -> Vec<u8> {
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
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out_vec4 = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let ca: Vec<_> = a.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let cb: Vec<_> = b.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = bld.constant_composite(vec4_f32, ca);
    let vb = bld.constant_composite(vec4_f32, cb);

    let out = bld.variable(ptr_out_vec4, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let sum = bld.f_add(vec4_f32, None, va, vb).unwrap();
    bld.store(out, sum, None, vec![]).unwrap();
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
fn cranelift_shader_runs_vec_arithmetic() {
    let a = [0.1f32, 0.2, 0.3, 0.4];
    let b = [0.5f32, 0.6, 0.7, 0.0];
    let spirv = build_vec_add_shader(a, b);
    let module = atrium_spv_frontend::translate(&spirv).unwrap();
    let object_bytes = atrium_spv_backend_cranelift::compile(
        &module, Target::host(),
    ).unwrap().object;

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
    let expected = [a[0]+b[0], a[1]+b[1], a[2]+b[2], a[3]+b[3]];
    for i in 0..4 {
        assert!((out_color[i] - expected[i]).abs() < 1e-6,
            "lane {i}: got {}, expected {}", out_color[i], expected[i]);
    }
}

/// Build a shader exercising OpVectorTimesScalar + OpDot
/// in a single straight-line body:
///
///   scaled  = vec_a * s              // VectorTimesScalar
///   dot_val = dot(scaled, vec_b)     // Dot
///   out     = vec4(dot_val, 0, 0, 1)
fn build_vec_scale_dot_shader(
    vec_a: [f32; 4], s: f32, vec_b: [f32; 4],
) -> Vec<u8> {
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
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out_vec4 = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let ca: Vec<_> = vec_a.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let cb: Vec<_> = vec_b.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let cs = bld.constant_bit32(f32_ty, s.to_bits());
    let c_zero = bld.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_one  = bld.constant_bit32(f32_ty, 1.0f32.to_bits());

    let va = bld.constant_composite(vec4_f32, ca);
    let vb = bld.constant_composite(vec4_f32, cb);

    let out = bld.variable(ptr_out_vec4, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let scaled = bld.vector_times_scalar(vec4_f32, None, va, cs).unwrap();
    let dot_val = bld.dot(f32_ty, None, scaled, vb).unwrap();
    let color = bld.composite_construct(vec4_f32, None, vec![dot_val, c_zero, c_zero, c_one]).unwrap();
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
fn cranelift_shader_runs_vector_times_scalar_and_dot() {
    let vec_a = [0.5f32, 1.0, 0.25, 0.0];
    let s = 0.5f32;
    let vec_b = [1.0f32, 0.5, 4.0, 0.0];
    let spirv = build_vec_scale_dot_shader(vec_a, s, vec_b);

    let module = atrium_spv_frontend::translate(&spirv).unwrap();
    let object_bytes = atrium_spv_backend_cranelift::compile(
        &module, Target::host(),
    ).unwrap().object;
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

    // Expected:
    //   scaled  = (0.25, 0.5, 0.125, 0.0)
    //   dot     = 0.25*1.0 + 0.5*0.5 + 0.125*4.0 + 0.0*0.0
    //           = 0.25 + 0.25 + 0.5 + 0.0
    //           = 1.0
    //   out     = (1.0, 0.0, 0.0, 1.0)
    let scaled = [
        vec_a[0] * s, vec_a[1] * s, vec_a[2] * s, vec_a[3] * s,
    ];
    let dot = scaled.iter().zip(vec_b.iter()).map(|(a, b)| a * b).sum::<f32>();
    let expected = [dot, 0.0, 0.0, 1.0];

    for i in 0..4 {
        assert!((out_color[i] - expected[i]).abs() < 1e-6,
            "lane {i}: got {}, expected {}", out_color[i], expected[i]);
    }
}

/// Build a shader that swizzles a constant vec4 with
/// OpVectorShuffle:
///
///   color = vec_a              // (r, g, b, a)
///   out   = color.bgra         // swizzle indices (2,1,0,3)
fn build_swizzle_shader(vec_a: [f32; 4], components: [u32; 4]) -> Vec<u8> {
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
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out_vec4 = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let ca: Vec<_> = vec_a.iter().map(|x| bld.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = bld.constant_composite(vec4_f32, ca);

    let out = bld.variable(ptr_out_vec4, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let swizzled = bld.vector_shuffle(vec4_f32, None, va, va,
                                      components.to_vec()).unwrap();
    bld.store(out, swizzled, None, vec![]).unwrap();
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
fn cranelift_shader_runs_swizzle() {
    let vec_a = [0.1f32, 0.2, 0.3, 0.4];
    // .bgra → indices (2, 1, 0, 3).
    let swizzle = [2u32, 1, 0, 3];
    let spirv = build_swizzle_shader(vec_a, swizzle);
    let module = atrium_spv_frontend::translate(&spirv).unwrap();
    let object_bytes = atrium_spv_backend_cranelift::compile(
        &module, Target::host(),
    ).unwrap().object;
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
    let expected = [
        vec_a[swizzle[0] as usize], vec_a[swizzle[1] as usize],
        vec_a[swizzle[2] as usize], vec_a[swizzle[3] as usize],
    ];
    assert_eq!(out_color, expected,
        "swizzle wrong: got {:?}, expected {:?}", out_color, expected);
}

#[test]
fn cranelift_shader_runs_real_arithmetic() {
    let a = 0.5f32;
    let b = 0.25f32;
    let c = 1.0f32;
    let spirv = build_arithmetic_shader(a, b, c);

    let module = atrium_spv_frontend::translate(&spirv)
        .expect("frontend must translate arithmetic shader");
    let object_bytes = atrium_spv_backend_cranelift::compile(
        &module, Target::host(),
    ).expect("backend must compile arithmetic shader").object;

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
    let expected = [a + b, a * b, a - b, c];
    assert_eq!(out_color, expected,
        "Cranelift shader arithmetic wrong: got {:?}, expected {:?}",
        out_color, expected);
}

/// Build a shader exercising OpFOrdLessThan + OpSelect.
///
/// Computes per-channel: `c < threshold ? t : f` for four
/// independent scalar conditions, packing the four
/// resulting f32s into the output vec4. This exercises:
///   - scalar OpFOrdLessThan producing Bool
///   - scalar OpSelect choosing between two f32 branches
///   - vec4 result via OpCompositeConstruct
fn build_compare_select_shader(
    inputs: [(f32, f32, f32, f32); 4],
) -> Vec<u8> {
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
    let bool_ty = bld.type_bool();
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out_vec4 = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let out = bld.variable(ptr_out_vec4, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();

    let mut lanes = Vec::with_capacity(4);
    for (c, thresh, t, f) in inputs.iter().copied() {
        let c_v = bld.constant_bit32(f32_ty, c.to_bits());
        let thr_v = bld.constant_bit32(f32_ty, thresh.to_bits());
        let t_v = bld.constant_bit32(f32_ty, t.to_bits());
        let f_v = bld.constant_bit32(f32_ty, f.to_bits());
        let cond = bld.f_ord_less_than(bool_ty, None, c_v, thr_v).unwrap();
        let chosen = bld.select(f32_ty, None, cond, t_v, f_v).unwrap();
        lanes.push(chosen);
    }
    let color = bld.composite_construct(vec4_f32, None, lanes).unwrap();
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
fn cranelift_shader_runs_comparison_and_select() {
    // Lane 0: 0.1 < 0.5 → true  → 1.0
    // Lane 1: 0.9 < 0.5 → false → 0.25
    // Lane 2: 0.5 < 0.5 → false → 0.75   (ord-less is strict)
    // Lane 3: -1.0 < 0.0 → true → 0.5
    let inputs = [
        (0.1f32, 0.5f32, 1.0f32, 0.0f32),
        (0.9f32, 0.5f32, 0.0f32, 0.25f32),
        (0.5f32, 0.5f32, 0.0f32, 0.75f32),
        (-1.0f32, 0.0f32, 0.5f32, 0.0f32),
    ];
    let expected = [1.0f32, 0.25, 0.75, 0.5];

    let spirv = build_compare_select_shader(inputs);
    let module = translate(&spirv)
        .expect("frontend must translate compare/select shader");
    let object_bytes = compile(&module, Target::host())
        .expect("backend must compile compare/select shader").object;

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
    assert_eq!(out_color, expected,
        "compare/select wrong: got {:?}, expected {:?}",
        out_color, expected);

    // Differential: interpreter must agree.
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&ShaderInputs::default())
        .expect("interp run");
    let interp_px = interp_out.pixels.first().copied()
        .expect("interp must emit at least one pixel");
    assert_eq!(interp_px, expected,
        "interpreter compare/select disagrees: {:?} vs {:?}",
        interp_px, expected);
}

/// Build a shader that reads a single `float scale` from a
/// push-constant block at offset 0 and writes
/// `vec4(scale, 0.0, 0.0, 1.0)` to the fragment output.
///
/// SPIR-V layout:
///   - struct PC { float scale; } with OpMemberDecorate
///     Offset 0
///   - OpVariable<PushConstant>(ptr_to_PC) %pc_var
///   - body: OpAccessChain ptr_f32 %pc_var %i0
///           OpLoad f32 %scale through that ptr
///           CompositeConstruct vec4(scale, 0, 0, 1)
///           Store to fragment output
fn build_pushconst_scale_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };

    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let i32_ty = bld.type_int(32, 1);
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);

    // struct PC { float scale; }
    let pc_struct = bld.type_struct(vec![f32_ty]);
    bld.member_decorate(pc_struct, 0, Decoration::Offset,
                        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    bld.decorate(pc_struct, Decoration::Block, vec![]);

    let ptr_pc_struct = bld.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = bld.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out_vec4  = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let zero_i = bld.constant_bit32(i32_ty, 0u32);
    let c0 = bld.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1 = bld.constant_bit32(f32_ty, 1.0f32.to_bits());

    let pc_var  = bld.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out     = bld.variable(ptr_out_vec4,  None, StorageClass::Output,       None);
    bld.decorate(out, Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let scale_ptr = bld.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let scale = bld.load(f32_ty, None, scale_ptr, None, vec![]).unwrap();
    let color = bld.composite_construct(vec4_f32, None,
                                        vec![scale, c0, c0, c1]).unwrap();
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
fn cranelift_shader_reads_push_constant_scale() {
    let spirv = build_pushconst_scale_shader();

    // Frontend must discover the push-constant block + size.
    let module = translate(&spirv).expect("frontend translate");
    assert_eq!(module.push_constants_size, 4,
        "expected 4-byte push-constant block, got {}",
        module.push_constants_size);

    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
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

    // Push-constant buffer holds a single f32 at offset 0.
    let scale_value = 0.7f32;
    let mut pc_buf = [0u8; 4];
    pc_buf.copy_from_slice(&scale_value.to_le_bytes());

    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let expected = [scale_value, 0.0, 0.0, 1.0];
    assert_eq!(out_color, expected,
        "push-const scale wrong: got {:?}, expected {:?}",
        out_color, expected);

    // Differential against interpreter oracle.
    let mut interp_inputs = ShaderInputs::default();
    interp_inputs.push_constants[..4].copy_from_slice(&pc_buf);
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&interp_inputs)
        .expect("interp run");
    let interp_px = interp_out.pixels.first().copied()
        .expect("interp must emit at least one pixel");
    assert_eq!(interp_px, expected,
        "interpreter push-const disagrees: {:?} vs {:?}",
        interp_px, expected);
}

/// Build a shader reading a `vec4 color` from a push-
/// constant block at offset 0. Exercises the vec lane-
/// walk path of Op::Load.
fn build_pushconst_vec4_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };

    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let i32_ty = bld.type_int(32, 1);
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);

    let pc_struct = bld.type_struct(vec![vec4_f32]);
    bld.member_decorate(pc_struct, 0, Decoration::Offset,
                        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    bld.decorate(pc_struct, Decoration::Block, vec![]);

    let ptr_pc_struct = bld.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_vec4   = bld.type_pointer(None, StorageClass::PushConstant, vec4_f32);
    let ptr_out_vec4  = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let zero_i = bld.constant_bit32(i32_ty, 0u32);

    let pc_var = bld.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = bld.variable(ptr_out_vec4,  None, StorageClass::Output,       None);
    bld.decorate(out, Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let color_ptr = bld.access_chain(ptr_pc_vec4, None, pc_var, vec![zero_i]).unwrap();
    let color = bld.load(vec4_f32, None, color_ptr, None, vec![]).unwrap();
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
fn cranelift_shader_reads_push_constant_vec4() {
    let spirv = build_pushconst_vec4_shader();
    let module = translate(&spirv).expect("frontend translate");
    assert_eq!(module.push_constants_size, 16,
        "expected 16-byte push-constant block, got {}",
        module.push_constants_size);

    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
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

    let expected = [0.1f32, 0.3, 0.5, 0.9];
    let mut pc_buf = [0u8; 16];
    for (i, v) in expected.iter().enumerate() {
        pc_buf[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
    }

    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, expected,
        "push-const vec4 wrong: got {:?}, expected {:?}",
        out_color, expected);

    let mut interp_inputs = ShaderInputs::default();
    interp_inputs.push_constants[..16].copy_from_slice(&pc_buf);
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&interp_inputs)
        .expect("interp run");
    let interp_px = interp_out.pixels.first().copied()
        .expect("at least one pixel");
    assert_eq!(interp_px, expected,
        "interpreter push-const vec4 disagrees: {:?} vs {:?}",
        interp_px, expected);
}

/// Build a shader reading a `vec4 color` from a Uniform
/// (UBO) block at descriptor (set=0, binding=0).
/// Exercises the StorageClass::Uniform path through the
/// uniforms ABI parameter (frag param index 1).
fn build_uniform_vec4_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };

    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let i32_ty = bld.type_int(32, 1);
    let vec4_f32 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);

    let ubo_struct = bld.type_struct(vec![vec4_f32]);
    bld.member_decorate(ubo_struct, 0, Decoration::Offset,
                        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    bld.decorate(ubo_struct, Decoration::Block, vec![]);

    let ptr_ubo_struct = bld.type_pointer(None, StorageClass::Uniform, ubo_struct);
    let ptr_ubo_vec4   = bld.type_pointer(None, StorageClass::Uniform, vec4_f32);
    let ptr_out_vec4   = bld.type_pointer(None, StorageClass::Output, vec4_f32);

    let zero_i = bld.constant_bit32(i32_ty, 0u32);

    let ubo_var = bld.variable(ptr_ubo_struct, None, StorageClass::Uniform, None);
    bld.decorate(ubo_var, Decoration::DescriptorSet,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    bld.decorate(ubo_var, Decoration::Binding,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let out = bld.variable(ptr_out_vec4, None, StorageClass::Output, None);
    bld.decorate(out, Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let color_ptr = bld.access_chain(ptr_ubo_vec4, None, ubo_var, vec![zero_i]).unwrap();
    let color = bld.load(vec4_f32, None, color_ptr, None, vec![]).unwrap();
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
fn cranelift_shader_reads_uniform_vec4() {
    let spirv = build_uniform_vec4_shader();
    let module = translate(&spirv).expect("frontend translate");
    assert!(!module.uniforms.is_empty(), "expected one uniform binding");

    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
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

    let expected = [0.2f32, 0.4, 0.6, 0.8];
    let mut ubo_buf = vec![0u8; 16];
    for (i, v) in expected.iter().enumerate() {
        ubo_buf[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
    }

    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), ubo_buf.as_ptr(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, expected,
        "uniform vec4 wrong: got {:?}, expected {:?}",
        out_color, expected);

    let interp_inputs = ShaderInputs {
        uniforms: ubo_buf.clone(),
        ..ShaderInputs::default()
    };
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&interp_inputs)
        .expect("interp run");
    let interp_px = interp_out.pixels.first().copied()
        .expect("at least one pixel");
    assert_eq!(interp_px, expected,
        "interpreter uniform vec4 disagrees: {:?} vs {:?}",
        interp_px, expected);
}

/// Build a shader with a real if/else CFG:
///
/// ```glsl
/// if (scale < 0.5) out = vec4(1, 0, 0, 1);  // red
/// else             out = vec4(0, 0, 1, 1);  // blue
/// ```
///
/// `scale` is loaded from a push-constant struct member.
/// Exercises Op{Branch,BranchCond} + multi-block CFG +
/// stores in branches.
fn build_if_else_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, SelectionControl,
        StorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let bool_ty = b.type_bool();
    let i32_ty = b.type_int(32, 1);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let pc_struct = b.type_struct(vec![f32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);

    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out_vec4  = b.type_pointer(None, StorageClass::Output, vec4_f32);

    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let c05 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let red  = b.constant_composite(vec4_f32, vec![c1, c0, c0, c1]);
    let blue = b.constant_composite(vec4_f32, vec![c0, c0, c1, c1]);

    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = b.variable(ptr_out_vec4,  None, StorageClass::Output,       None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let scale_ptr = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let scale = b.load(f32_ty, None, scale_ptr, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, scale, c05).unwrap();
    let then_id = b.id();
    let else_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.branch_conditional(cond, then_id, else_id, vec![]).unwrap();

    b.begin_block(Some(then_id)).unwrap();
    b.store(out, red, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();

    b.begin_block(Some(else_id)).unwrap();
    b.store(out, blue, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();

    b.begin_block(Some(merge_id)).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn run_if_else(scale: f32) -> [f32; 4] {
    let spirv = build_if_else_shader();
    let module = translate(&spirv).expect("frontend translate");
    // Sanity: function must have 4 blocks now (entry,
    // then, else, merge).
    assert_eq!(module.functions[0].blocks.len(), 4,
        "expected 4 IR blocks; got {}",
        module.functions[0].blocks.len());

    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
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
    let mut pc_buf = [0u8; 4];
    pc_buf.copy_from_slice(&scale.to_le_bytes());
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }

    // Differential against interpreter.
    let mut interp_inputs = ShaderInputs::default();
    interp_inputs.push_constants[..4].copy_from_slice(&pc_buf);
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&interp_inputs).expect("interp run");
    let interp_px = interp_out.pixels.first().copied().expect("at least one pixel");
    assert_eq!(out_color, interp_px,
        "interp disagrees with backend at scale={scale}: \
         backend={out_color:?} interp={interp_px:?}");
    out_color
}

#[test]
fn cranelift_shader_runs_if_else_then_branch() {
    let out = run_if_else(0.2);   // 0.2 < 0.5 → red
    assert_eq!(out, [1.0, 0.0, 0.0, 1.0]);
}

#[test]
fn cranelift_shader_runs_if_else_else_branch() {
    let out = run_if_else(0.8);   // 0.8 < 0.5 → false → blue
    assert_eq!(out, [0.0, 0.0, 1.0, 1.0]);
}

/// Build a shader using OpPhi at an if/else merge:
///
/// ```glsl
/// float v = (scale < 0.5) ? 1.0 : 0.25;
/// out = vec4(v, v, v, 1.0);
/// ```
///
/// Emitted as branching CFG with a Phi in the merge
/// block (not OpSelect) to exercise the Phi codegen path.
fn build_phi_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, SelectionControl,
        StorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let bool_ty = b.type_bool();
    let i32_ty = b.type_int(32, 1);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let pc_struct = b.type_struct(vec![f32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);

    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out_vec4  = b.type_pointer(None, StorageClass::Output, vec4_f32);

    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c05  = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1   = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let c025 = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let c10  = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = b.variable(ptr_out_vec4,  None, StorageClass::Output,       None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let entry_label = b.selected_block().unwrap();
    let _ = entry_label;
    let scale_ptr = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let scale = b.load(f32_ty, None, scale_ptr, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, scale, c05).unwrap();

    let then_id = b.id();
    let else_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.branch_conditional(cond, then_id, else_id, vec![]).unwrap();

    // then-branch: yields 1.0, ends at then_id_actual
    b.begin_block(Some(then_id)).unwrap();
    b.branch(merge_id).unwrap();

    // else-branch: yields 0.25
    b.begin_block(Some(else_id)).unwrap();
    b.branch(merge_id).unwrap();

    // merge: %v = phi (1.0 from then, 0.25 from else)
    b.begin_block(Some(merge_id)).unwrap();
    let v = b.phi(f32_ty, None, vec![(c1, then_id), (c025, else_id)]).unwrap();
    let color = b.composite_construct(vec4_f32, None, vec![v, v, v, c10]).unwrap();
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

fn run_phi_shader(scale: f32) -> [f32; 4] {
    let spirv = build_phi_shader();
    let module = translate(&spirv).expect("frontend translate");
    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
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
    let mut pc_buf = [0u8; 4];
    pc_buf.copy_from_slice(&scale.to_le_bytes());
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let mut interp_inputs = ShaderInputs::default();
    interp_inputs.push_constants[..4].copy_from_slice(&pc_buf);
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&interp_inputs).expect("interp run");
    let interp_px = interp_out.pixels.first().copied().expect("pixel");
    assert_eq!(out_color, interp_px,
        "phi: backend/interp disagree at scale={scale}: \
         backend={out_color:?} interp={interp_px:?}");
    out_color
}

#[test]
fn cranelift_shader_runs_phi_then() {
    let out = run_phi_shader(0.2);   // 0.2 < 0.5 → 1.0
    assert_eq!(out, [1.0, 1.0, 1.0, 1.0]);
}

#[test]
fn cranelift_shader_runs_phi_else() {
    let out = run_phi_shader(0.8);   // 0.8 < 0.5 false → 0.25
    assert_eq!(out, [0.25, 0.25, 0.25, 1.0]);
}

/// Build a shader exercising integer arithmetic +
/// comparison + ConvertSToF. Reads an i32 counter from a
/// push-constant block, computes `(n * 2 + 1) / 100.0`,
/// and writes that scalar to the output's red channel.
/// We avoid OpConvertSToF (not yet implemented) by using
/// purely-integer arithmetic and constructing the
/// fragment colour from a precomputed table: pixel =
/// vec4(n == 7 ? 1.0 : 0.5, 0, 0, 1).
fn build_int_arith_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);

    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out_vec4  = b.type_pointer(None, StorageClass::Output, vec4_f32);

    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let two_i  = b.constant_bit32(i32_ty, 2u32);
    let one_i  = b.constant_bit32(i32_ty, 1u32);
    let seven_i = b.constant_bit32(i32_ty, 7u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c05 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = b.variable(ptr_out_vec4,  None, StorageClass::Output,       None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let n_ptr = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, n_ptr, None, vec![]).unwrap();
    // doubled = n * 2; added = doubled + 1; eq7 = (added == 7)
    let doubled = b.i_mul(i32_ty, None, n, two_i).unwrap();
    let added = b.i_add(i32_ty, None, doubled, one_i).unwrap();
    let eq7 = b.i_equal(bool_ty, None, added, seven_i).unwrap();
    let red = b.select(f32_ty, None, eq7, c1, c05).unwrap();
    let color = b.composite_construct(vec4_f32, None, vec![red, c0, c0, c1]).unwrap();
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

fn run_int_arith(n: i32) -> [f32; 4] {
    let spirv = build_int_arith_shader();
    let module = translate(&spirv).expect("frontend translate");
    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
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
    let mut pc_buf = [0u8; 4];
    pc_buf.copy_from_slice(&n.to_le_bytes());
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let mut interp_inputs = ShaderInputs::default();
    interp_inputs.push_constants[..4].copy_from_slice(&pc_buf);
    let interp = Interpreter::new(&spirv).expect("interp parse");
    let interp_out = interp.run_fragment(&interp_inputs).expect("interp run");
    let interp_px = interp_out.pixels.first().copied().expect("pixel");
    assert_eq!(out_color, interp_px,
        "int-arith: backend/interp disagree at n={n}: \
         backend={out_color:?} interp={interp_px:?}");
    out_color
}

#[test]
fn cranelift_shader_runs_int_arith_match() {
    // n=3: 3*2+1 = 7 → red=1.0
    let out = run_int_arith(3);
    assert_eq!(out, [1.0, 0.0, 0.0, 1.0]);
}

#[test]
fn cranelift_shader_runs_int_arith_mismatch() {
    // n=2: 2*2+1 = 5 → red=0.5
    let out = run_int_arith(2);
    assert_eq!(out, [0.5, 0.0, 0.0, 1.0]);
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
