//! Integration tests: hand-built constant-color fragment
//! shader translated through the frontend, with the
//! result shape verified.
//!
//! Phase 1 v1 doesn't yet have a backend that can execute
//! the IR, so we can't do full differential testing
//! against the interpreter at this layer. What we DO
//! check:
//!
//! 1. Frontend doesn't reject the shader.
//! 2. The resulting [`Module`] has exactly one function,
//!    declared as Fragment stage, named "main".
//! 3. The function has exactly one block.
//! 4. The block ends with `Op::Return`.
//! 5. The block contains an `Op::Store` whose target
//!    pointer is the Output variable.
//! 6. The Module's interface lists one Output varying at
//!    location 0 of vec4<f32> type.
//!
//! Phase 2 adds the Cranelift backend; at that point the
//! diff harness in atrium-spv-tests can compile the same
//! SPIR-V through (a) the interpreter directly and (b)
//! the frontend + Cranelift, and assert pixel-exact
//! agreement. For now, structural assertion is the best
//! we can do.

use atrium_spv_ir::{BlockKind, Op, ShaderStage, StorageClass, Type, VecElement};
use atrium_spv_frontend::translate;

fn build_constant_color_shader(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out_vec4 = b.type_pointer(None, SpvStorageClass::Output, vec4_f32);

    let c0 = b.constant_bit32(f32_ty, rgba[0].to_bits());
    let c1 = b.constant_bit32(f32_ty, rgba[1].to_bits());
    let c2 = b.constant_bit32(f32_ty, rgba[2].to_bits());
    let c3 = b.constant_bit32(f32_ty, rgba[3].to_bits());
    let color = b.constant_composite(vec4_f32, vec![c0, c1, c2, c3]);

    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, rspirv::spirv::Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
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
fn frontend_translates_constant_color_shader() {
    let spirv = build_constant_color_shader([1.0, 0.5, 0.25, 1.0]);
    let module = translate(&spirv).expect("frontend must accept this shader");

    // ── 1. One function. ────────────────────────────────────
    assert_eq!(module.functions.len(), 1);

    // ── 2. Entry point: Fragment, name "main". ─────────────
    assert_eq!(module.entry_points.len(), 1);
    let ep = &module.entry_points[0];
    assert_eq!(ep.stage, ShaderStage::Fragment);
    assert_eq!(ep.name, "main");
    assert_eq!(ep.function_index, 0);

    // ── 3. Function has one Linear block. ──────────────────
    let func = &module.functions[0];
    assert_eq!(func.stage, ShaderStage::Fragment);
    assert_eq!(func.blocks.len(), 1);
    let block = func.blocks.get(&func.entry_block)
        .expect("entry block must exist");
    assert!(matches!(block.kind, BlockKind::Linear));

    // ── 4. Block ends with Return. ─────────────────────────
    let last = block.insts.last().expect("block must have a terminator");
    assert!(matches!(last.op, Op::Return),
            "expected Op::Return, got {:?}", last.op);

    // ── 5. Block contains constant materialisation + Store ─
    //
    // Per the constant-materialisation rule (constraints
    // A2 + B1: every used value must have a defining
    // Inst), the body should be:
    //   ConstFloat(1.0)
    //   ConstFloat(0.5)
    //   ConstFloat(0.25)
    //   ConstFloat(1.0)
    //   ConstVec([f1, f2, f3, f4])
    //   Store { ptr=output_var, value=vec }
    //   Return
    //
    // We assert: 4 ConstFloats with the right values, 1
    // ConstVec referencing those 4 values, 1 Store using
    // the ConstVec's result, and Return as the terminator.
    let const_floats: Vec<f32> = block.insts.iter().filter_map(|i| {
        if let Op::ConstFloat { value, kind: atrium_spv_ir::FloatKind::F32 } = &i.op {
            Some(*value as f32)
        } else { None }
    }).collect();
    assert_eq!(const_floats, vec![1.0, 0.5, 0.25, 1.0],
               "expected exactly four ConstFloat insts with shader's RGBA");

    let const_vec = block.insts.iter()
        .find(|i| matches!(i.op, Op::ConstVec(_)))
        .expect("expected an Op::ConstVec");
    if let Op::ConstVec(elements) = &const_vec.op {
        assert_eq!(elements.len(), 4, "ConstVec must have 4 elements");
    }

    let store = block.insts.iter()
        .find(|i| matches!(i.op, Op::Store { .. }))
        .expect("expected an Op::Store");
    if let Op::Store { ptr, value } = &store.op {
        match &ptr.ty {
            Type::Pointer(StorageClass::Output, inner) => {
                assert_eq!(**inner, Type::Vec4(VecElement::F32));
            }
            other => panic!("expected Pointer(Output, Vec4(F32)); got {other:?}"),
        }
        // Store.value must be the ConstVec's result.
        let cv_result = const_vec.result.as_ref().unwrap();
        assert_eq!(value.id, cv_result.id);
    }

    // ── 6. Module's varyings list has one entry. ───────────
    assert_eq!(module.varyings.len(), 1);
    assert_eq!(module.varyings[0].location, 0);
    assert_eq!(module.varyings[0].ty, Type::Vec4(VecElement::F32));
}

#[test]
fn frontend_rejects_unsupported_capability() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionModel, FunctionControl,
        MemoryModel,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.capability(Capability::Float64); // ← unsupported in v1
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }

    let err = translate(&bytes).unwrap_err();
    assert!(matches!(err, atrium_spv_frontend::FrontendError::Unsupported(_)),
            "expected Unsupported, got {err:?}");
}
