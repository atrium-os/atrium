//! Integration tests for the phase-0 interpreter against
//! hand-built SPIR-V modules.
//!
//! These are the simplest possible shaders the interpreter
//! has to handle correctly — constant-colour fragment
//! shaders. Phase-1 widens to arithmetic, comparisons, and
//! control flow; phase-2 adds the production backends as
//! comparison runners in the harness.

use atrium_spv_tests::{
    harness::{assert_shader_agrees, InterpreterRunner, ShaderRunner},
    interpreter::{Interpreter, ShaderInputs},
    pixels::ColorTolerance,
};

/// Build the smallest possible fragment shader: writes
/// `vec4(1.0, 0.5, 0.25, 1.0)` to its single Output
/// variable and returns. Hand-assembled via rspirv's
/// Builder to keep the test self-contained (no glslc
/// dependency at test time).
fn build_constant_color_shader(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    // Types.
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4_f32);

    // Constants.
    let c0 = b.constant_bit32(f32_ty, rgba[0].to_bits());
    let c1 = b.constant_bit32(f32_ty, rgba[1].to_bits());
    let c2 = b.constant_bit32(f32_ty, rgba[2].to_bits());
    let c3 = b.constant_bit32(f32_ty, rgba[3].to_bits());
    let color = b.constant_composite(vec4_f32, vec![c0, c1, c2, c3]);

    // Output variable.
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);

    // Entry-point function.
    let main = b.begin_function(
        void, None, FunctionControl::NONE, void_fn,
    ).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let module = b.module();
    let words: Vec<u32> = module.assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bytes
}

#[test]
fn interpreter_runs_constant_color_shader() {
    let spirv = build_constant_color_shader([1.0, 0.5, 0.25, 1.0]);
    let interp = Interpreter::new(&spirv).expect("interpreter must parse");
    let out = interp.run_fragment(&ShaderInputs::default())
        .expect("interpreter must run");
    assert_eq!(out.pixels.len(), 1);
    assert_eq!(out.pixels[0], [1.0, 0.5, 0.25, 1.0]);
}

#[test]
fn interpreter_runs_with_multiple_invocations() {
    let spirv = build_constant_color_shader([0.2, 0.4, 0.6, 0.8]);
    let interp = Interpreter::new(&spirv).expect("interpreter must parse");
    let inputs = ShaderInputs {
        varyings_per_invocation: vec![vec![], vec![], vec![]],
        ..ShaderInputs::default()
    };
    let out = interp.run_fragment(&inputs)
        .expect("interpreter must run");
    assert_eq!(out.pixels.len(), 3);
    // Constant shader produces the same color for every
    // invocation, regardless of varyings.
    for pixel in &out.pixels {
        assert_eq!(*pixel, [0.2, 0.4, 0.6, 0.8]);
    }
}

#[test]
#[should_panic(expected = "needs ≥2 successful runners")]
fn harness_with_only_interpreter_runner_panics() {
    // ≥2 runners required; one runner alone has nothing
    // to compare against. Documented in
    // assert_shader_agrees's contract.
    let spirv = build_constant_color_shader([1.0, 0.0, 0.0, 1.0]);
    let interp = InterpreterRunner;
    let runners: &[&dyn ShaderRunner] = &[&interp];
    assert_shader_agrees(
        &spirv,
        &ShaderInputs::default(),
        ColorTolerance::Exact,
        runners,
    );
}

#[test]
fn harness_with_two_interpreter_runners_agrees() {
    // Two instances of the same runner trivially agree —
    // exercises the harness plumbing end-to-end without
    // needing the production backends to be wired up yet.
    let spirv = build_constant_color_shader([0.1, 0.2, 0.3, 0.4]);
    let a = InterpreterRunner;
    let b = InterpreterRunner;
    let runners: &[&dyn ShaderRunner] = &[&a, &b];
    assert_shader_agrees(
        &spirv,
        &ShaderInputs::default(),
        ColorTolerance::Exact,
        runners,
    );
}
