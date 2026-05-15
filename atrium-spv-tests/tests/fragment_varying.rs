//! Fragment-varying-load: a `vec4 in_color` Location=0
//! fragment input copied straight to out_color. Gates the
//! interpreter's Input-storage dispatch (`is_vertex=false`
//! → reads `varyings_per_invocation[inv_idx]`) and the
//! invocation-index threading across multiple per-pixel
//! varying entries.

use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs};

fn build_passthrough_color_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_vec4 = b.type_pointer(None, SpvStorageClass::Input, vec4);
    let ptr_out_vec4 = b.type_pointer(None, SpvStorageClass::Output, vec4);

    let in_color = b.variable(ptr_in_vec4, None, SpvStorageClass::Input, None);
    b.decorate(in_color, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out_color = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out_color, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let color = b.load(vec4, None, in_color, None, vec![]).unwrap();
    b.store(out_color, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main",
                  vec![in_color, out_color]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn pack_vec4(v: [f32; 4]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    for f in v { b.extend_from_slice(&f.to_le_bytes()); }
    b
}

#[test]
fn interpreter_fragment_passthrough_varying() {
    let spirv = build_passthrough_color_shader();
    let interp = Interpreter::new(&spirv).unwrap();

    // Three "pixels" — distinct varying colour per
    // invocation. Each should emerge as the output
    // colour, gating both the Input-storage dispatch
    // (varyings_per_invocation, not the vertex-attribute
    // buffer) and the per-invocation inv_idx threading.
    let colors = [
        [1.0_f32, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 1.0],
    ];
    let inputs = ShaderInputs {
        varyings_per_invocation: colors.iter().map(|c| pack_vec4(*c)).collect(),
        ..ShaderInputs::default()
    };
    let out = interp.run_fragment(&inputs).unwrap();
    assert_eq!(out.pixels.len(), 3);
    for (i, p) in out.pixels.iter().enumerate() {
        for k in 0..4 {
            assert!((p[k] - colors[i][k]).abs() < 1e-6,
                "pixel {i} lane {k}: expected {} got {} (full {:?})",
                colors[i][k], p[k], p);
        }
    }
}
