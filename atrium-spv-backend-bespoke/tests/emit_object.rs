//! Phase 3 skeleton sanity tests.

use std::collections::HashMap;

use atrium_spv_backend_bespoke::{compile, compile_blob, Target};
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
fn compile_blob_emits_parseable_flat_blob() {
    let module = empty_fragment_module();
    let out = compile_blob(&module, Target::host()).expect("compile_blob");

    // The blob must parse + validate.
    let blob = atrium_spv_blob::ShaderBlob::from_bytes(&out.blob)
        .expect("blob parses");
    assert_eq!(blob.arch, atrium_spv_blob::ARCH_AARCH64);

    // A single fragment function → the fs entry sits at
    // offset 0, vs/cs absent.
    assert_eq!(blob.entries.fs, Some(0));
    assert_eq!(blob.entries.vs, None);
    assert_eq!(blob.entries.cs, None);

    // The code is the raw function body — exactly the ARM64
    // `ret` (0xD65F03C0) for an empty fragment shader, with
    // no object-file framing around it.
    let ret_bytes = 0xD65F_03C0u32.to_le_bytes();
    assert!(
        blob.code.windows(4).any(|w| w == ret_bytes),
        "expected ARM64 `ret` bytes in the blob code; got len={}",
        blob.code.len(),
    );

    // The blob carries *only* code — no `atrium_fs_main`
    // symbol string, no ELF/Mach-O headers (that's the
    // whole point: the loader resolves the entry point by
    // offset, not by `dlsym`).
    assert!(
        !blob.code.windows(b"atrium_fs_main".len())
            .any(|w| w == b"atrium_fs_main"),
        "blob code should not contain a symbol name",
    );

    // pcmap is identical in shape to the object path: one
    // entry per lowered IR instruction.
    let pcmap = atrium_spv_pcmap::PcMap::from_bytes(&out.pcmap)
        .expect("pcmap parses");
    assert_eq!(pcmap.entries().len(), module.functions.len());

    // The blob's code is the same bytes the object path
    // wraps — sanity-check they agree on the body.
    let obj_out = compile(&module, Target::host()).expect("compile");
    assert!(
        obj_out.object.windows(blob.code.len())
            .any(|w| w == blob.code.as_slice()),
        "object's .text should contain exactly the blob's code",
    );
}

#[test]
fn unsupported_op_falls_back_with_unsupported_error() {
    use atrium_spv_backend_bespoke::BackendError;
    use atrium_spv_ir::{Value, ValueId};

    // Build a module with a screen-space derivative op.
    // The bespoke backend's ISel covers the full common
    // fragment-shader surface now, but derivatives
    // (DPdx/DPdy/Fwidth) need quad-level cooperation the
    // bespoke per-invocation model doesn't provide → it
    // returns Unsupported, and the production driver
    // falls back to Cranelift.
    let entry_block = BlockId(0);
    let mut blocks = HashMap::new();
    let v = Value { id: ValueId(0), ty: Type::F32 };
    blocks.insert(entry_block, Block {
        id: entry_block,
        kind: BlockKind::Linear,
        insts: vec![
            Inst {
                op: Op::Fwidth(v.clone()),
                result: Some(Value { id: ValueId(1), ty: Type::F32 }),
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
