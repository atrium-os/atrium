//! Vertex shader reads `gl_Position` from a uniform block.
//! Gates the `(Vertex, Uniform) → X2 / params[2]` mappings
//! in both backends + the interpreter's `Uniform` path in
//! a vertex-stage invocation. Same latent plumbing as the
//! fragment-side `uniform_block.rs` tests, but cross-checks
//! the vertex codegen now picks up uniforms from the right
//! AAPCS64 register (X2, not X1 like fragment).

use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};

fn build_vertex_uniform_pos_shader() -> Vec<u8> {
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

    // gl_PerVertex { vec4 gl_Position; }
    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);

    // Uniform block: struct UB { vec4 position; }
    let ub_struct = b.type_struct(vec![vec4]);
    b.member_decorate(ub_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub_struct, Decoration::Block, vec![]);

    let ptr_pv_struct = b.type_pointer(None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4  = b.type_pointer(None, SpvStorageClass::Output, vec4);
    let ptr_ub_struct = b.type_pointer(None, SpvStorageClass::Uniform, ub_struct);
    let ptr_ub_vec4   = b.type_pointer(None, SpvStorageClass::Uniform, vec4);

    let pv_var = b.variable(ptr_pv_struct, None, SpvStorageClass::Output, None);
    let ub = b.variable(ptr_ub_struct, None, SpvStorageClass::Uniform, None);
    b.decorate(ub, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(i32_ty, 0u32);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src_ptr = b.access_chain(ptr_ub_vec4, None, ub, vec![c_zero]).unwrap();
    let pos = b.load(vec4, None, src_ptr, None, vec![]).unwrap();
    let dst_ptr = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst_ptr, pos, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![ub, pv_var]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Reuses the helpers from vertex_constant.rs's
/// `run_object_const`-style cc+dlopen pipeline. We don't
/// want to factor this into a shared module just yet
/// (the test corpus is small), so inline the minimal
/// runner here.
fn compile_to_object(spirv: &[u8], backend: &str) -> Vec<u8> {
    let module = atrium_spv_frontend::translate(spirv).unwrap();
    match backend {
        "cranelift" => atrium_spv_backend_cranelift::compile(
            &module, atrium_spv_backend_cranelift::Target::host()).unwrap().object,
        "bespoke" => atrium_spv_backend_bespoke::compile(
            &module, atrium_spv_backend_bespoke::Target::host()).unwrap().object,
        _ => panic!("unknown backend"),
    }
}

fn run_vertex_with_uniforms(spirv: &[u8], backend: &str, uniforms: &[u8]) -> [f32; 4] {
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
            std::ptr::null(), std::ptr::null(),
            uniforms.as_ptr(), std::ptr::null(),
            0, 0,
            pos.as_mut_ptr(), varyings.as_mut_ptr(), clip.as_mut_ptr(),
        );
    }
    pos
}

#[test]
fn three_way_vertex_uniform_position() {
    let pos = [0.5_f32, -0.25, 0.125, 1.0];
    let mut uniforms = Vec::with_capacity(16);
    for f in pos { uniforms.extend_from_slice(&f.to_le_bytes()); }

    let spirv = build_vertex_uniform_pos_shader();
    // Interpreter oracle.
    let interp = Interpreter::new(&spirv).unwrap();
    let inputs = ShaderInputs { uniforms: uniforms.clone(), ..ShaderInputs::default() };
    let interp_pos = interp.run_vertex(&inputs).unwrap().positions[0];

    // Both backends agree.
    for backend in ["cranelift", "bespoke"] {
        let p = run_vertex_with_uniforms(&spirv, backend, &uniforms);
        for k in 0..4 {
            assert!((p[k] - pos[k]).abs() < 1e-6,
                "{backend} lane {k}: expected {} got {}", pos[k], p[k]);
            assert!((p[k] - interp_pos[k]).abs() < 1e-6,
                "{backend} vs interp lane {k}: {} vs {}", p[k], interp_pos[k]);
        }
    }
}
