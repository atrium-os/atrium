//! Phase 1b: the passthrough vertex shader actually reads
//! a per-vertex `vec3` attribute and constructs
//! `vec4(pos, 1.0)` as `gl_Position`. Exercises the new
//! `StorageClass::Input` path in `load_from_storage` +
//! invocation-index threading through `eval_inst`.

use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};

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

/// Pack `[f32; 3]` as little-endian bytes — the layout the
/// interpreter's `load_from_storage` reads through.
fn pack_vec3(v: [f32; 3]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    for f in v { b.extend_from_slice(&f.to_le_bytes()); }
    b
}

#[test]
fn interpreter_passthrough_vertex_single_invocation() {
    let spirv = build_passthrough_vertex_shader();
    let interp = Interpreter::new(&spirv).unwrap();
    let attr = pack_vec3([0.25, 0.5, -0.75]);
    let inputs = ShaderInputs {
        vertex_attributes_per_invocation: vec![attr],
        ..ShaderInputs::default()
    };
    let out = interp.run_vertex(&inputs).expect("run_vertex must succeed");
    assert_eq!(out.positions.len(), 1);
    let p = out.positions[0];
    let expected = [0.25_f32, 0.5, -0.75, 1.0];
    for k in 0..4 {
        assert!((p[k] - expected[k]).abs() < 1e-6,
            "lane {k}: expected {}, got {} (full {:?})",
            expected[k], p[k], p);
    }
}

#[test]
fn interpreter_passthrough_vertex_three_vertices() {
    // Each vertex gets a distinct attribute → distinct
    // position. Verifies invocation-index threading.
    let spirv = build_passthrough_vertex_shader();
    let interp = Interpreter::new(&spirv).unwrap();
    let attrs = vec![
        pack_vec3([1.0, 0.0, 0.0]),  // vertex 0
        pack_vec3([0.0, 1.0, 0.0]),  // vertex 1
        pack_vec3([0.0, 0.0, 1.0]),  // vertex 2
    ];
    let inputs = ShaderInputs {
        vertex_attributes_per_invocation: attrs,
        ..ShaderInputs::default()
    };
    let out = interp.run_vertex(&inputs).unwrap();
    assert_eq!(out.positions.len(), 3);
    let expected = [
        [1.0_f32, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 1.0],
    ];
    for (i, p) in out.positions.iter().enumerate() {
        for k in 0..4 {
            assert!((p[k] - expected[i][k]).abs() < 1e-6,
                "vertex {i} lane {k}: expected {} got {} (full {:?})",
                expected[i][k], p[k], p);
        }
    }
}
