//! Phase 3 skeleton sanity tests.

use std::collections::HashMap;

use atrium_spv_backend_bespoke::{compile, Target};
use atrium_spv_ir::{
    Block, BlockId, BlockKind, EntryPoint, Function, Inst, Module, Op,
    ShaderStage, Type,
};

fn empty_fragment_module() -> Module {
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
    Module {
        functions: vec![Function {
            name: "main".to_string(),
            stage: ShaderStage::Fragment,
            params: Vec::new(),
            return_type: Type::Void,
            entry_block,
            blocks,
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
    }
}

#[test]
fn empty_fragment_emits_ret_instruction() {
    let module = empty_fragment_module();
    let out = compile(&module, Target::host()).expect("compile");

    // The function body is exactly one ARM64 `ret`
    // instruction (4 bytes, little-endian 0xD65F03C0).
    // The object file wraps it with headers + symbol table;
    // we just verify the instruction bytes appear somewhere
    // in the object.
    let ret_bytes = 0xD65F_03C0u32.to_le_bytes();
    assert!(
        out.object.windows(4).any(|w| w == ret_bytes),
        "expected ARM64 `ret` (0xD65F03C0) bytes in object; got len={}",
        out.object.len(),
    );

    // The exported symbol must be `atrium_fs_main`.
    assert!(
        out.object.windows(b"atrium_fs_main".len())
            .any(|w| w == b"atrium_fs_main"),
        "atrium_fs_main symbol not found in object",
    );

    // pcmap must parse + carry exactly one entry per function.
    let pcmap = atrium_spv_pcmap::PcMap::from_bytes(&out.pcmap)
        .expect("pcmap parses");
    assert_eq!(pcmap.entries().len(), module.functions.len());
}

#[test]
fn unsupported_op_falls_back_with_unsupported_error() {
    use atrium_spv_backend_bespoke::BackendError;
    use atrium_spv_ir::{Value, ValueId};

    // Build a module with an int-arithmetic op. Step 2's
    // ISel doesn't cover IAdd yet → must return
    // Unsupported (the production driver interprets that
    // as "fall back to Cranelift").
    let entry_block = BlockId(0);
    let mut blocks = HashMap::new();
    let v = Value { id: ValueId(0), ty: Type::I32 };
    blocks.insert(entry_block, Block {
        id: entry_block,
        kind: BlockKind::Linear,
        insts: vec![
            Inst {
                op: Op::IAdd(v.clone(), v.clone()),
                result: Some(Value { id: ValueId(1), ty: Type::I32 }),
                source_spirv_offset: 0,
            },
            Inst { op: Op::Return, result: None, source_spirv_offset: 0 },
        ],
    });
    let module = Module {
        functions: vec![Function {
            name: "main".to_string(),
            stage: ShaderStage::Fragment,
            params: Vec::new(),
            return_type: Type::Void,
            entry_block,
            blocks,
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
    let err = compile(&module, Target::host()).expect_err("must reject");
    assert!(matches!(err, BackendError::Unsupported(_)),
        "expected Unsupported, got {err:?}");
}

#[test]
fn object_magic_matches_host_format() {
    let module = empty_fragment_module();
    let out = compile(&module, Target::host()).unwrap();
    let magic = &out.object[..4];
    let is_macho = magic == [0xCF, 0xFA, 0xED, 0xFE]
                || magic == [0xCE, 0xFA, 0xED, 0xFE];
    let is_elf = magic == [0x7F, b'E', b'L', b'F'];
    assert!(is_macho || is_elf,
        "unexpected magic: {magic:?}");
}
