//! Bespoke compute path: prove an empty GLCompute shader
//! compiles through the bespoke ARM64 backend (not via the
//! Cranelift fallback).
//!
//! atrium-spv-compile's selection logic tries bespoke first
//! and falls back to Cranelift on `BackendError::Unsupported`.
//! For correctness this is fine -- Cranelift produces the
//! same runtime behaviour. But the perf premise of the
//! bespoke backend is "hand-tuned ARM64 codegen, faster than
//! Cranelift" -- so we want to know which compute shaders
//! actually exercise the bespoke path.
//!
//! This test calls `compile_blob` directly (no fallback), so
//! it succeeds only if bespoke can handle the shader end-to-
//! end.  The empty CS is the floor: header + LocalSize + a
//! single OpReturn.  If even this rejects, the bespoke
//! compute foundation isn't wired.

use atrium_spv_backend_bespoke::{compile_blob, Target};
use atrium_spv_frontend::translate;

fn build_empty_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_compiles_empty_compute_shader() {
    let spv = build_empty_cs();
    let module = translate(&spv).expect("frontend translate");

    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else {
        Target::Aarch64FreeBSD
    };

    let out = compile_blob(&module, target)
        .expect("bespoke should compile an empty compute shader directly \
                 (no Cranelift fallback)");

    assert!(!out.blob.is_empty(),
        "bespoke compute output should be non-empty");
    // PC-map can legitimately be empty for an empty function
    // (a single OpReturn may not get its own pc entry depending
    // on the emission order); the load-bearing assertion is
    // that the blob exists.
}
