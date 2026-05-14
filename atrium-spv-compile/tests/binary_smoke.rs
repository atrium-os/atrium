//! Integration smoke test: spawn the
//! `atrium-spv-compile` binary as a subprocess, hand it
//! a SPIR-V file via `--input`, verify it writes the
//! expected `<hash>.so` (or `.dylib`) and `<hash>.pcmap`
//! files into the output directory and exits 0.
//!
//! Mirrors the production code path the daemon's
//! `Tier2Backend` will use once phase 2 v5b lands —
//! the daemon spawns this binary in a Portcullis jail
//! and watches the exit code + stderr JSON for the
//! result + metrics.

use std::process::Command;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

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

fn binary_path() -> std::path::PathBuf {
    // Cargo gives us the workspace target dir via env at
    // test-build time. Cross-platform: the binary lives
    // next to the test executable in target/<profile>/.
    let mut p = std::env::current_exe().unwrap();
    // current_exe points to .../target/debug/deps/<name>-<hash>
    // pop the hash, the `deps`, leaving target/debug
    p.pop(); p.pop();
    p.push("atrium-spv-compile");
    p
}

#[test]
fn binary_compiles_constant_color_shader() {
    let spirv = build_constant_color_spirv([1.0, 0.5, 0.25, 1.0]);
    let mut hasher = Sha256::new();
    hasher.update(&spirv);
    let hash = format!("{:x}", hasher.finalize());

    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("in.spv");
    std::fs::write(&input_path, &spirv).unwrap();
    let output_dir = dir.path().join("cache");

    let status = Command::new(binary_path())
        .arg("--input").arg(&input_path)
        .arg("--output-dir").arg(&output_dir)
        .status()
        .expect("spawn atrium-spv-compile");
    assert!(status.success(), "binary exited non-zero: {status}");

    // A constant-colour shader compiles via the bespoke
    // backend, which now takes the JIT-emit path: the
    // artifact is a flat `<hash>.afblob`, not a `cc`-linked
    // `.so`. (Cranelift-fallback shaders still produce a
    // `.so` — see binary_smoke's unsupported-capability
    // test for that path.)
    let blob_path = output_dir.join(format!("{hash}.afblob"));
    let pcmap_path = output_dir.join(format!("{hash}.pcmap"));
    assert!(blob_path.exists(),
        "expected {} to exist after compile", blob_path.display());
    assert!(pcmap_path.exists(),
        "expected {} to exist after compile", pcmap_path.display());

    // The blob path runs no linker, so neither an
    // intermediate `.o` nor a `.so` should appear.
    let obj_path = output_dir.join(format!("{hash}.o"));
    assert!(!obj_path.exists(),
        "intermediate .o leaked into cache: {}", obj_path.display());
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let so_path = output_dir.join(format!("{hash}.{ext}"));
    assert!(!so_path.exists(),
        "bespoke JIT-emit path should not produce a .so: {}",
        so_path.display());
}

#[test]
fn binary_exits_unsupported_on_capability_we_dont_handle() {
    // A shader that declares Float64 — the frontend's
    // capability allowlist rejects it (constraint A3).
    // The binary must exit with code 1 (Unsupported)
    // rather than panic or write garbage.
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionModel, FunctionControl,
        MemoryModel,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.capability(Capability::Float64);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut spirv = Vec::with_capacity(words.len() * 4);
    for w in words { spirv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("bad.spv");
    std::fs::write(&input_path, &spirv).unwrap();
    let output_dir = dir.path().join("cache");

    let status = Command::new(binary_path())
        .arg("--input").arg(&input_path)
        .arg("--output-dir").arg(&output_dir)
        .status()
        .expect("spawn atrium-spv-compile");
    assert_eq!(status.code(), Some(1),
        "expected exit code 1 (Unsupported), got {status}");
    // No cache files should have been written.
    if let Ok(entries) = std::fs::read_dir(&output_dir) {
        for entry in entries.flatten() {
            panic!("expected empty cache dir; found {}",
                   entry.path().display());
        }
    }
}

#[test]
fn binary_emits_metrics_json_on_stderr() {
    let spirv = build_constant_color_spirv([0.7, 0.6, 0.5, 1.0]);
    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("in.spv");
    std::fs::write(&input_path, &spirv).unwrap();
    let output_dir = dir.path().join("cache");

    let output = Command::new(binary_path())
        .arg("--input").arg(&input_path)
        .arg("--output-dir").arg(&output_dir)
        .output()
        .expect("spawn atrium-spv-compile");
    assert!(output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("\"shader_hash\":"));
    // A constant-colour store is fully within the bespoke
    // ARM64 backend's opcode surface, and atrium-spv-compile
    // tries bespoke first (spec §2 production order), so the
    // metrics line must report the bespoke backend — not the
    // Cranelift fallback.
    assert!(stderr.contains("\"backend\":\"bespoke\""),
            "expected bespoke backend, got: {stderr}");
    // Microsecond resolution: with `cc` gone the whole
    // compile is sub-millisecond, so `compile_ms` would
    // truncate to a near-useless integer.
    assert!(stderr.contains("\"compile_us\":"));
    assert!(stderr.contains("\"size_bytes\":"));
}
