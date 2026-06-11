//! End-to-end test: a hand-built SPIR-V constant-color
//! fragment shader runs through the frontend AND the
//! Cranelift backend, producing a valid object file that
//! contains the expected `atrium_fs_main` symbol and (by
//! the disassembled byte count) more than just a return.
//!
//! Phase 2 v2 scope: we don't yet load the .so and run
//! the shader; that lands in v3 once `ld` linking + dlopen
//! is wired up. For now we verify the object file is
//! well-formed and that the function body is more than a
//! single return — proxy for "real instructions got
//! emitted."

use atrium_spv_backend_cranelift::{compile, Target};
use atrium_spv_frontend::translate;

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

#[test]
fn frontend_to_backend_constant_color() {
    let spirv = build_constant_color_spirv([1.0, 0.5, 0.25, 1.0]);

    // Frontend.
    let module = translate(&spirv).expect("frontend must translate the shader");
    assert_eq!(module.functions.len(), 1);

    // Backend.
    let object_bytes = compile(&module, Target::host())
        .expect("backend must compile the IR")
        .object;

    // Sanity: valid object-file magic + symbol present.
    assert!(!object_bytes.is_empty());
    let magic = &object_bytes[..4];
    let is_macho =
        magic == [0xCF, 0xFA, 0xED, 0xFE] ||
        magic == [0xCE, 0xFA, 0xED, 0xFE];
    let is_elf = magic == [0x7F, b'E', b'L', b'F'];
    assert!(is_macho || is_elf,
        "unexpected object magic: {magic:?}");
    assert!(object_bytes.windows(b"atrium_fs_main".len())
            .any(|w| w == b"atrium_fs_main"),
        "atrium_fs_main symbol not found in object bytes");

    // Function-size proxy: an empty-body shader compiles
    // to ~150-250 bytes of object output (mostly headers
    // + symbol-table boilerplate). A shader with a real
    // ConstFloat × 4 + ConstVec + Store-lanes-×-4 + Return
    // should produce noticeably more bytes. We assert
    // > 300 bytes as a sanity floor — well above the
    // empty-body case, well below any realistic
    // upper bound. This catches regressions where the
    // backend silently emits an empty body.
    assert!(object_bytes.len() > 300,
        "object bytes look suspiciously short ({} bytes); \
         may indicate empty function body",
        object_bytes.len());
}

#[test]
fn compile_emits_a_parseable_pcmap_sidecar() {
    // The CompileOutput.pcmap bytes must be a well-formed
    // atrium-spv-pcmap v1 sidecar. Cranelift's pcmap is
    // function-granularity: one entry per Function in the
    // module, host_offset=0, spirv_offset=first inst's
    // source_spirv_offset. Phase 1 v1 always sets the
    // latter to 0 (placeholder); we assert the structural
    // properties regardless.
    let spirv = build_constant_color_spirv([0.1, 0.2, 0.3, 0.4]);
    let module = atrium_spv_frontend::translate(&spirv).unwrap();
    let output = compile(&module, Target::host()).unwrap();

    // Parse the sidecar; it must round-trip.
    let pcmap = atrium_spv_pcmap::PcMap::from_bytes(&output.pcmap)
        .expect("pcmap sidecar must parse");
    assert_eq!(pcmap.entries().len(), module.functions.len(),
        "expected one pcmap entry per IR function");
    // Every entry's host_offset is 0 (function-relative)
    // per the Cranelift backend's documented granularity.
    for e in pcmap.entries() {
        assert_eq!(e.host_offset, 0,
            "Cranelift pcmap entries are function-relative; expected host_offset=0, got {}",
            e.host_offset);
    }
    // Per phase 1 v2: source_spirv_offset is now real,
    // not placeholder 0. The pcmap entry for the entry-
    // point function should carry the offset of its first
    // body instruction (a non-zero value past the module
    // header + global declarations).
    let entry = pcmap.entries().first().expect("at least one entry");
    assert!(entry.spirv_offset > 0,
        "expected non-zero spirv_offset post-phase-1-v2; \
         got {}", entry.spirv_offset);
}

#[test]
fn empty_module_still_produces_valid_object() {
    use std::collections::HashMap;
    use atrium_spv_ir::{
        Block, BlockId, BlockKind, EntryPoint, Function, Inst, Module, Op,
        ShaderStage, Type,
    };

    // Smallest possible Module: one Fragment fn whose body
    // is just Return. Verifies the empty-body case still
    // works after we've added the real translator (no
    // regression of the phase 2 v1 baseline).
    let entry_block = BlockId(0);
    let mut blocks = HashMap::new();
    blocks.insert(entry_block, Block {
        id: entry_block,
        kind: BlockKind::Linear,
        insts: vec![Inst {
            op: Op::Return,
            result: None,
            source_spirv_offset: 0,
        }],
    });
    let module = Module {
        functions: vec![Function {
            name: "main".to_string(),
            stage: ShaderStage::Fragment,
            params: Vec::new(),
            return_type: Type::Void,
            entry_block,
            blocks,
            local_size: None,
            ssbo_bindings: std::collections::HashMap::new(),
            workgroup_size: 0,
            workgroup_var_offset: std::collections::HashMap::new(),
            output_varying_byte_offset: std::collections::HashMap::new(),
            input_varying_byte_offset: std::collections::HashMap::new(),
            frag_depth_output: None,
            varying_output_bytes: 0,
        }],
        entry_points: vec![EntryPoint {
            stage: ShaderStage::Fragment,
            function_index: 0,
            name: "main".to_string(),
        }],
        uniforms: Vec::new(),
        push_constants_size: 0,
        vertex_inputs: Vec::new(),
        varyings: Vec::new(),
    };
    let output = compile(&module, Target::host()).unwrap();
    assert!(!output.object.is_empty());
}
