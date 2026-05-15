//! Interpreter `run_vertex` smoke gate — phase 1 of the
//! vertex-stage arc.
//!
//! Builds a vertex shader that writes a *constant* vec4 to
//! `gl_Position` (no input attribute read). The interpreter's
//! `run_vertex` should produce that exact constant. No
//! backend is exercised yet — Cranelift / bespoke vertex
//! codegen lands in the next phases.

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

    // gl_PerVertex { vec4 gl_Position; }.
    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);

    let ptr_pv_struct = b.type_pointer(
        None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4);

    let pv_var = b.variable(ptr_pv_struct, None,
                            SpvStorageClass::Output, None);

    let c0 = b.constant_bit32(f32_ty, p[0].to_bits());
    let c1 = b.constant_bit32(f32_ty, p[1].to_bits());
    let c2 = b.constant_bit32(f32_ty, p[2].to_bits());
    let c3 = b.constant_bit32(f32_ty, p[3].to_bits());
    let pos = b.constant_composite(vec4, vec![c0, c1, c2, c3]);
    let c_zero = b.constant_bit32(i32_ty, 0u32);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos_ptr = b.access_chain(
        ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(pos_ptr, pos, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![pv_var]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn interpreter_run_vertex_constant_position() {
    let expected = [0.25_f32, 0.5, 0.75, 1.0];
    let spirv = build_constant_position_vertex_shader(expected);
    let interp = Interpreter::new(&spirv)
        .expect("interpreter must accept a vertex shader");
    let out = interp.run_vertex(&ShaderInputs::default())
        .expect("run_vertex must succeed");
    assert_eq!(out.positions.len(), 1,
        "default ShaderInputs → 1 invocation");
    for k in 0..4 {
        assert!((out.positions[0][k] - expected[k]).abs() < 1e-6,
            "lane {k}: expected {}, got {} (full {:?})",
            expected[k], out.positions[0][k], out.positions[0]);
    }
}

#[test]
fn interpreter_run_vertex_one_invocation_per_attribute_entry() {
    let expected = [0.0_f32, 0.0, 0.0, 1.0];
    let spirv = build_constant_position_vertex_shader(expected);
    let interp = Interpreter::new(&spirv).unwrap();
    // Three "vertices" (the shader ignores the attribute
    // bytes, just constants out) — we should get three
    // identical positions back.
    let inputs = ShaderInputs {
        vertex_attributes_per_invocation: vec![vec![]; 3],
        ..ShaderInputs::default()
    };
    let out = interp.run_vertex(&inputs).unwrap();
    assert_eq!(out.positions.len(), 3);
    for p in &out.positions {
        for k in 0..4 {
            assert!((p[k] - expected[k]).abs() < 1e-6);
        }
    }
}
