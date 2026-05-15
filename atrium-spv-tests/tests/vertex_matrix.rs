//! Interpreter handles `OpMatrixTimesVector` end-to-end:
//! a vertex shader transforms a `vec4(in_pos, 1.0)` by a
//! `mat4` from a uniform block and stores the result to
//! `gl_Position`. Phase 2 of the matrix arc; backends
//! land in phases 3 + 4.

use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};

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

    // gl_PerVertex { vec4 gl_Position; }
    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);

    // Uniform block: struct UB { mat4 mvp; }
    let ub_struct = b.type_struct(vec![mat4]);
    b.member_decorate(ub_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // MatrixStride = 16 (column stride for vec4 column).
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

/// Pack a column-major mat4 (`[[f32;4];4]`) as 64 bytes
/// LE. Each column is 4 contiguous f32s. The SPIR-V
/// `OpLoad` of a mat4 reads exactly this layout from
/// uniforms.
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

#[test]
fn interpreter_mvp_transforms_position() {
    // Pure translation matrix:
    //   |1 0 0 tx|
    //   |0 1 0 ty|
    //   |0 0 1 tz|
    //   |0 0 0 1 |
    // Column-major: each "column" is the row of constants
    // that multiplies the corresponding vector component.
    //   col 0 = (1, 0, 0, 0)  → multiplies x
    //   col 1 = (0, 1, 0, 0)  → y
    //   col 2 = (0, 0, 1, 0)  → z
    //   col 3 = (tx, ty, tz, 1) → 1 (the w lane of pos4)
    let (tx, ty, tz) = (10.0_f32, 20.0, 30.0);
    let mvp = [
        [1.0, 0.0, 0.0, 0.0],   // column 0
        [0.0, 1.0, 0.0, 0.0],   // column 1
        [0.0, 0.0, 1.0, 0.0],   // column 2
        [tx,  ty,  tz,  1.0],   // column 3
    ];
    let pos = [0.5_f32, 1.5, 2.5];

    let inputs = ShaderInputs {
        uniforms: pack_mat4(mvp),
        vertex_attributes_per_invocation: vec![pack_vec3(pos)],
        ..ShaderInputs::default()
    };

    let spirv = build_mvp_vertex_shader();
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_vertex(&inputs).unwrap();
    assert_eq!(out.positions.len(), 1);
    let p = out.positions[0];
    let expected = [pos[0] + tx, pos[1] + ty, pos[2] + tz, 1.0_f32];
    for k in 0..4 {
        assert!((p[k] - expected[k]).abs() < 1e-6,
            "lane {k}: expected {} got {} (full {:?})",
            expected[k], p[k], p);
    }
}

#[test]
fn interpreter_mvp_scale_matrix() {
    // Uniform scale by 2.0 along x, 3.0 along y, 4.0 along z.
    // col 0 = (2, 0, 0, 0); col 1 = (0, 3, 0, 0);
    // col 2 = (0, 0, 4, 0); col 3 = (0, 0, 0, 1).
    let mvp = [
        [2.0, 0.0, 0.0, 0.0],
        [0.0, 3.0, 0.0, 0.0],
        [0.0, 0.0, 4.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let pos = [0.25_f32, 0.5, -0.75];
    let inputs = ShaderInputs {
        uniforms: pack_mat4(mvp),
        vertex_attributes_per_invocation: vec![pack_vec3(pos)],
        ..ShaderInputs::default()
    };
    let spirv = build_mvp_vertex_shader();
    let interp = Interpreter::new(&spirv).unwrap();
    let p = interp.run_vertex(&inputs).unwrap().positions[0];
    let expected = [pos[0] * 2.0, pos[1] * 3.0, pos[2] * 4.0, 1.0];
    for k in 0..4 {
        assert!((p[k] - expected[k]).abs() < 1e-6,
            "lane {k}: expected {} got {}", expected[k], p[k]);
    }
}
