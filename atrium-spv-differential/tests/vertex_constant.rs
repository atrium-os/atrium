//! Phase 2: Cranelift compiles a constant-position vertex
//! shader, the harness dlopens it, calls `atrium_vs_main`
//! with a one-element vertex batch, and compares the
//! returned position against the interpreter's.
//!
//! No bespoke runner yet — that lands in phase 3. The
//! vertex-stage three-way differential lands in phase 4.

#![allow(dead_code)]

use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};

fn build_constant_position_vertex_shader(p: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);
    let ptr_pv_struct = b.type_pointer(None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4 = b.type_pointer(None, SpvStorageClass::Output, vec4);
    let pv_var = b.variable(ptr_pv_struct, None, SpvStorageClass::Output, None);
    let c0 = b.constant_bit32(f32_ty, p[0].to_bits());
    let c1 = b.constant_bit32(f32_ty, p[1].to_bits());
    let c2 = b.constant_bit32(f32_ty, p[2].to_bits());
    let c3 = b.constant_bit32(f32_ty, p[3].to_bits());
    let pos = b.constant_composite(vec4, vec![c0, c1, c2, c3]);
    let c_zero = b.constant_bit32(i32_ty, 0u32);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos_ptr = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(pos_ptr, pos, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![pv_var]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// One of the two production backends.
#[derive(Copy, Clone, Debug)]
enum Backend { Cranelift, Bespoke }

fn compile_to_object(spirv: &[u8], backend: Backend) -> Vec<u8> {
    let module = atrium_spv_frontend::translate(spirv).unwrap();
    match backend {
        Backend::Cranelift => atrium_spv_backend_cranelift::compile(
            &module, atrium_spv_backend_cranelift::Target::host()).unwrap().object,
        Backend::Bespoke => atrium_spv_backend_bespoke::compile(
            &module, atrium_spv_backend_bespoke::Target::host()).unwrap().object,
    }
}

fn run_vertex_const(spirv: &[u8], backend: Backend) -> [f32; 4] {
    let object = compile_to_object(spirv, backend);
    run_object_const(&object)
}

fn run_object_const(object: &[u8]) -> [f32; 4] {
    let dir = tempfile::tempdir().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let status = std::process::Command::new("cc")
        .arg(flag).arg("-o").arg(&lib_path).arg(&obj_path)
        .status().unwrap();
    assert!(status.success(), "cc -shared failed");

    let lib = unsafe { libloading::Library::new(&lib_path) }.unwrap();
    // atrium_vs_main(in_attributes, in_attr_strides,
    //                uniforms, push_constants,
    //                vertex_index, instance_index,
    //                out_position, out_varyings,
    //                out_clip_distance)
    type VsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, *const u8,
        u32, u32,
        *mut f32, *mut u8, *mut f32,
    );
    let vs_main: libloading::Symbol<VsMain> = unsafe {
        lib.get(b"atrium_vs_main").unwrap()
    };
    let mut pos = [0.0f32; 4];
    let mut varyings = [0u8; 256];
    let mut clip = [0.0f32; 8];
    unsafe {
        vs_main(
            std::ptr::null(), std::ptr::null(),
            std::ptr::null(), std::ptr::null(),
            0, 0,
            pos.as_mut_ptr(), varyings.as_mut_ptr(), clip.as_mut_ptr(),
        );
    }
    pos
}

fn build_passthrough_vertex_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SpvStorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);
    let ptr_pv_struct = b.type_pointer(None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4 = b.type_pointer(None, SpvStorageClass::Output, vec4);
    let ptr_in_vec3 = b.type_pointer(None, SpvStorageClass::Input, vec3);
    let in_pos = b.variable(ptr_in_vec3, None, SpvStorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv_struct, None, SpvStorageClass::Output, None);
    let c_zero_i = b.constant_bit32(i32_ty, 0u32);
    let c_one_f  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let pos_ptr = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero_i]).unwrap();
    b.store(pos_ptr, pos4, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_pos, pv_var]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn run_object_passthrough(object: &[u8], attr: &[u8]) -> [f32; 4] {
    let dir = tempfile::tempdir().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let status = std::process::Command::new("cc")
        .arg(flag).arg("-o").arg(&lib_path).arg(&obj_path)
        .status().unwrap();
    assert!(status.success(), "cc -shared failed");

    let lib = unsafe { libloading::Library::new(&lib_path) }.unwrap();
    type VsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, *const u8,
        u32, u32,
        *mut f32, *mut u8, *mut f32,
    );
    let vs_main: libloading::Symbol<VsMain> = unsafe {
        lib.get(b"atrium_vs_main").unwrap()
    };
    let mut pos = [0.0f32; 4];
    let mut varyings = [0u8; 256];
    let mut clip = [0.0f32; 8];
    unsafe {
        vs_main(
            attr.as_ptr(), std::ptr::null(),
            std::ptr::null(), std::ptr::null(),
            0, 0,
            pos.as_mut_ptr(), varyings.as_mut_ptr(), clip.as_mut_ptr(),
        );
    }
    pos
}

fn run_passthrough(spirv: &[u8], attr: &[u8], backend: Backend) -> [f32; 4] {
    let object = compile_to_object(spirv, backend);
    run_object_passthrough(&object, attr)
}

fn assert_constant_position(backend: Backend) {
    let expected = [0.25_f32, 0.5, 0.75, 1.0];
    let spirv = build_constant_position_vertex_shader(expected);
    // Sanity-check via interpreter.
    let interp = Interpreter::new(&spirv).unwrap();
    let interp_pos = interp.run_vertex(&ShaderInputs::default()).unwrap()
        .positions[0];
    for k in 0..4 {
        assert!((interp_pos[k] - expected[k]).abs() < 1e-6,
            "interpreter lane {k}: {} vs {}", interp_pos[k], expected[k]);
    }
    let pos = run_vertex_const(&spirv, backend);
    for k in 0..4 {
        assert!((pos[k] - expected[k]).abs() < 1e-6,
            "{backend:?} lane {k}: {} vs {}", pos[k], expected[k]);
    }
}

fn assert_passthrough(backend: Backend) {
    let spirv = build_passthrough_vertex_shader();
    let expected_pos = [0.25_f32, -0.5, 0.75];
    let mut attr = Vec::with_capacity(12);
    for f in expected_pos { attr.extend_from_slice(&f.to_le_bytes()); }

    let interp = Interpreter::new(&spirv).unwrap();
    let inputs = ShaderInputs {
        vertex_attributes_per_invocation: vec![attr.clone()],
        ..ShaderInputs::default()
    };
    let interp_pos = interp.run_vertex(&inputs).unwrap().positions[0];

    let pos = run_passthrough(&spirv, &attr, backend);
    for k in 0..4 {
        assert!((pos[k] - interp_pos[k]).abs() < 1e-6,
            "{backend:?} lane {k}: {} vs interpreter {}",
            pos[k], interp_pos[k]);
    }
}

#[test] fn cranelift_constant_position_vertex() { assert_constant_position(Backend::Cranelift); }
#[test] fn cranelift_passthrough_vertex()       { assert_passthrough(Backend::Cranelift); }
#[test] fn bespoke_constant_position_vertex()   { assert_constant_position(Backend::Bespoke); }
#[test] fn bespoke_passthrough_vertex()         { assert_passthrough(Backend::Bespoke); }
