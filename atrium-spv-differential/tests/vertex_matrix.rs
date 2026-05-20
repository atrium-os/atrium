//! Matrix arc phase 5 — three-way differential for the MVP
//! `gl_Position = mvp * vec4(pos, 1.0)` shape.
//!
//! Drives the SAME SPIR-V through:
//!
//! 1. the interpreter   (phase 2)
//! 2. Cranelift backend (phase 3)
//! 3. bespoke backend   (phase 4)
//!
//! All three runners must agree on the produced `gl_Position`
//! within a small float tolerance.  Mirrors `vertex_constant.rs`
//! / `vertex_uniform.rs` -- bypasses the fragment-shaped
//! ShaderRunner trait (which doesn't carry vertex outputs yet)
//! and shells out to each backend directly.
//!
//! Bespoke is gated on `target_arch = "aarch64"` because its
//! emitted machine code is AArch64-only.  On non-arm64 hosts
//! the bespoke runner is skipped silently (the test still
//! validates interpreter == cranelift agreement).

#![allow(dead_code)]

use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};

// ---------------------------------------------------------------------
// Shared SPIR-V builder + uniform / attribute packers.
// Same shape as the per-backend matrix tests in
// atrium-spv-backend-{cranelift,bespoke}/tests/dlopen_and_run.rs.

fn build_mvp_vertex_shader() -> Vec<u8> {
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
    let mat4 = b.type_matrix(vec4, 4);
    let void_fn = b.type_function(void, vec![]);

    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);

    let ub_struct = b.type_struct(vec![mat4]);
    b.member_decorate(ub_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(ub_struct, 0, Decoration::MatrixStride,
                      vec![rspirv::dr::Operand::LiteralBit32(16)]);
    b.member_decorate(ub_struct, 0, Decoration::ColMajor, vec![]);
    b.decorate(ub_struct, Decoration::Block, vec![]);

    let ptr_pv_struct = b.type_pointer(None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4  = b.type_pointer(None, SpvStorageClass::Output, vec4);
    let ptr_ub_struct = b.type_pointer(None, SpvStorageClass::Uniform, ub_struct);
    let ptr_ub_mat4   = b.type_pointer(None, SpvStorageClass::Uniform, mat4);
    let ptr_in_vec3   = b.type_pointer(None, SpvStorageClass::Input, vec3);

    let in_pos = b.variable(ptr_in_vec3, None, SpvStorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv_struct, None, SpvStorageClass::Output, None);
    let ub = b.variable(ptr_ub_struct, None, SpvStorageClass::Uniform, None);
    b.decorate(ub, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos3 = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos3, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos3, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos3, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let mvp_ptr = b.access_chain(ptr_ub_mat4, None, ub, vec![c_zero]).unwrap();
    let mvp = b.load(mat4, None, mvp_ptr, None, vec![]).unwrap();
    let transformed = b.matrix_times_vector(vec4, None, mvp, pos4).unwrap();
    let dst_ptr = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst_ptr, transformed, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main",
                  vec![in_pos, pv_var, ub]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn pack_mat4(m: [[f32; 4]; 4]) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    for col in m {
        for f in col { b.extend_from_slice(&f.to_le_bytes()); }
    }
    b
}

fn pack_vec3(v: [f32; 3]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    for f in v { b.extend_from_slice(&f.to_le_bytes()); }
    b
}

// ---------------------------------------------------------------------
// Per-backend execution.

#[derive(Copy, Clone, Debug)]
enum Backend { Cranelift, Bespoke }

fn compile_to_object(spirv: &[u8], backend: Backend) -> Vec<u8> {
    let module = atrium_spv_frontend::translate(spirv).unwrap();
    match backend {
        Backend::Cranelift => atrium_spv_backend_cranelift::compile(
            &module, atrium_spv_backend_cranelift::Target::host())
            .unwrap().object,
        Backend::Bespoke => atrium_spv_backend_bespoke::compile(
            &module, atrium_spv_backend_bespoke::Target::host())
            .unwrap().object,
    }
}

fn run_mvp(spirv: &[u8], attr: &[u8], ubo: &[u8], backend: Backend) -> [f32; 4] {
    let object = compile_to_object(spirv, backend);
    let dir = tempfile::tempdir().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &object).unwrap();
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
            attr.as_ptr(),
            std::ptr::null(),
            ubo.as_ptr(),
            std::ptr::null(),
            0, 0,
            pos.as_mut_ptr(),
            varyings.as_mut_ptr(),
            clip.as_mut_ptr(),
        );
    }
    pos
}

// ---------------------------------------------------------------------
// Three-way differential entry point.

/// Run the MVP shader through the interpreter, then Cranelift, then
/// (on aarch64 only) bespoke.  Assert all available runners agree
/// within a float tolerance and match `expected`.
fn assert_three_way_mvp(mvp: [[f32; 4]; 4], pos: [f32; 3], expected: [f32; 4]) {
    let spirv = build_mvp_vertex_shader();
    let attr  = pack_vec3(pos);
    let ubo   = pack_mat4(mvp);

    // Interpreter -- always available.
    let interp_inputs = ShaderInputs {
        uniforms: ubo.clone(),
        vertex_attributes_per_invocation: vec![attr.clone()],
        ..ShaderInputs::default()
    };
    let interp = Interpreter::new(&spirv).expect("interpreter parse");
    let interp_pos = interp.run_vertex(&interp_inputs)
        .expect("interpreter run").positions[0];
    for k in 0..4 {
        assert!((interp_pos[k] - expected[k]).abs() < 1e-6,
            "interpreter lane {k}: {} vs expected {}",
            interp_pos[k], expected[k]);
    }

    // Cranelift -- always available (runs on any host that has cc).
    let cl_pos = run_mvp(&spirv, &attr, &ubo, Backend::Cranelift);
    for k in 0..4 {
        assert!((cl_pos[k] - interp_pos[k]).abs() < 1e-6,
            "cranelift vs interpreter lane {k}: {} vs {}",
            cl_pos[k], interp_pos[k]);
    }

    // Bespoke -- aarch64 only.  Skip silently elsewhere; the
    // interpreter/cranelift pair still validates the shader.
    if cfg!(target_arch = "aarch64") {
        let bs_pos = run_mvp(&spirv, &attr, &ubo, Backend::Bespoke);
        for k in 0..4 {
            assert!((bs_pos[k] - interp_pos[k]).abs() < 1e-6,
                "bespoke vs interpreter lane {k}: {} vs {}",
                bs_pos[k], interp_pos[k]);
        }
    }
}

// ---------------------------------------------------------------------
// Test cases.

/// Translation matrix: gl_Position = pos + (tx, ty, tz).
#[test]
fn three_way_mvp_translation() {
    let (tx, ty, tz) = (10.0f32, 20.0, 30.0);
    let mvp = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [tx,  ty,  tz,  1.0],
    ];
    let pos = [0.5f32, 1.5, 2.5];
    let expected = [pos[0] + tx, pos[1] + ty, pos[2] + tz, 1.0];
    assert_three_way_mvp(mvp, pos, expected);
}

/// Diagonal scale matrix: each lane scaled by the column diagonal.
#[test]
fn three_way_mvp_scale() {
    let mvp = [
        [2.0, 0.0, 0.0, 0.0],
        [0.0, 3.0, 0.0, 0.0],
        [0.0, 0.0, 4.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let pos = [0.25f32, 0.5, -0.75];
    let expected = [pos[0] * 2.0, pos[1] * 3.0, pos[2] * 4.0, 1.0];
    assert_three_way_mvp(mvp, pos, expected);
}

/// Mixed matrix combining translation + scale + a non-zero
/// off-diagonal lane to make sure every (col, lane) pair is
/// exercised at least once (the prior two tests have all-zero
/// off-diagonals in columns 0..2, which can mask a per-lane
/// indexing bug).
#[test]
fn three_way_mvp_mixed() {
    // col 0 = (2,   0.5, 0,   0)
    // col 1 = (0,   3,   0.25, 0)
    // col 2 = (0.1, 0,   4,   0)
    // col 3 = (10,  20,  30,  1)
    let mvp = [
        [2.0,  0.5,  0.0,  0.0],
        [0.0,  3.0,  0.25, 0.0],
        [0.1,  0.0,  4.0,  0.0],
        [10.0, 20.0, 30.0, 1.0],
    ];
    let pos = [0.5f32, -1.0, 2.0];
    // result[i] = Σ_j mvp[j][i] * v[j], with v = (pos.x, pos.y, pos.z, 1)
    let mut expected = [0.0f32; 4];
    let v = [pos[0], pos[1], pos[2], 1.0];
    for i in 0..4 {
        for j in 0..4 {
            expected[i] += mvp[j][i] * v[j];
        }
    }
    assert_three_way_mvp(mvp, pos, expected);
}
