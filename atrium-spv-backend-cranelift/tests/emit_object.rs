//! Integration test: the empty-function backend pipeline
//! produces a structurally-valid object file with the
//! expected exported symbol name.
//!
//! Phase 2 v1 doesn't yet implement IR-instruction
//! translation — every shader compiles to a function
//! that immediately returns. The point of this test is
//! to lock in:
//!
//! 1. The Cranelift toolchain wiring (target ISA →
//!    ObjectModule → finish → emit) actually produces
//!    bytes.
//! 2. The output starts with the right object-format
//!    magic for the target (Mach-O on Darwin, ELF on
//!    FreeBSD/Linux).
//! 3. The exported symbol matches the shader-ABI spec'd
//!    name (`atrium_fs_main` for a Fragment-stage entry).
//!
//! Phase 2 v2 adds Op::Store + Op::ConstFloat translation
//! and grows this test into a real "load the .so, call
//! atrium_fs_main, check pixels" round-trip.

use std::collections::HashMap;

use atrium_spv_backend_cranelift::{compile, Target};
use atrium_spv_ir::{
    Block, BlockId, BlockKind, EntryPoint, Function, Inst, Module, Op,
    ShaderStage, Type,
};

/// Build an atrium-spv-ir Module with one minimal
/// Fragment-stage function that just returns.
fn build_minimal_fragment_module() -> Module {
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
    let func = Function {
        name: "main".to_string(),
        stage: ShaderStage::Fragment,
        params: Vec::new(),
        return_type: Type::Void,
        entry_block,
        blocks,
        local_size: None,
    };
    Module {
        functions: vec![func],
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
fn compile_produces_object_bytes_with_target_magic() {
    let module = build_minimal_fragment_module();
    // Pick a target that matches the host so we can also
    // inspect the bytes meaningfully. Real production
    // targets FreeBSD-ARM64; for the test we accept
    // Darwin-ARM64 (the dev host) when running on macOS.
    let target = Target::host();
    let bytes = compile(&module, target)
        .expect("compile should succeed for an empty Fragment function")
        .object;

    assert!(!bytes.is_empty(), "expected non-empty object output");

    // First 4 bytes identify the object format:
    //   Mach-O 64-bit: 0xFEEDFACF (LE = CF FA ED FE)
    //   ELF (any):     0x7F 'E' 'L' 'F'
    let magic = &bytes[..4];
    let is_macho =
        magic == [0xCF, 0xFA, 0xED, 0xFE] ||
        magic == [0xCE, 0xFA, 0xED, 0xFE];
    let is_elf = magic == [0x7F, b'E', b'L', b'F'];
    assert!(is_macho || is_elf,
        "object bytes start with unexpected magic: {magic:?}");
}

#[test]
fn compile_emits_atrium_fs_main_symbol() {
    // Verify the produced object contains the exported
    // symbol "atrium_fs_main" by string-searching the
    // bytes. Cheap + crude, but locks in that we're
    // using the shader-ABI name regardless of the
    // atrium-spv-ir Function's own `name` field.
    let module = build_minimal_fragment_module();
    let bytes = compile(&module, Target::host()).unwrap().object;
    let needle = b"atrium_fs_main";
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(found, "symbol 'atrium_fs_main' not found in object bytes");
}

#[test]
fn vertex_stage_emits_atrium_vs_main_symbol() {
    let mut module = build_minimal_fragment_module();
    module.functions[0].stage = ShaderStage::Vertex;
    module.entry_points[0].stage = ShaderStage::Vertex;
    let bytes = compile(&module, Target::host()).unwrap().object;
    assert!(bytes.windows(b"atrium_vs_main".len())
            .any(|w| w == b"atrium_vs_main"));
}

#[test]
fn compute_stage_emits_atrium_cs_main_symbol() {
    let mut module = build_minimal_fragment_module();
    module.functions[0].stage = ShaderStage::Compute;
    module.entry_points[0].stage = ShaderStage::Compute;
    let bytes = compile(&module, Target::host()).unwrap().object;
    assert!(bytes.windows(b"atrium_cs_main".len())
            .any(|w| w == b"atrium_cs_main"));
}
