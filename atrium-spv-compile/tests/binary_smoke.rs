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
fn binary_spec_const_changes_cache_key() {
    // Two compiles of the same SPIR-V with different
    // --spec-const values must produce different cache
    // filenames (otherwise specialised builds collide in
    // the cache).  Also verifies the binary accepts the
    // --spec-const flag and doesn't error.
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let i32_ty = b.type_int(32, 1);
    let void_fn = b.type_function(void, vec![]);
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride,
        vec![Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![Operand::LiteralBit32(0)]);
    let sc_n = b.spec_constant_bit32(i32_ty, 7u32);
    b.decorate(sc_n, Decoration::SpecId,
        vec![Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let n_u = b.bitcast(u32_ty, None, sc_n).unwrap();
    let p = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(p, n_u, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut spirv = Vec::with_capacity(words.len() * 4);
    for w in words { spirv.extend_from_slice(&w.to_le_bytes()); }

    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("in.spv");
    std::fs::write(&input_path, &spirv).unwrap();
    let cache_a = dir.path().join("cache_a");
    let cache_b = dir.path().join("cache_b");
    let cache_default = dir.path().join("cache_default");

    let run = |out_dir: &std::path::Path, extra: &[&str]| {
        let mut cmd = Command::new(binary_path());
        cmd.arg("--input").arg(&input_path)
            .arg("--output-dir").arg(out_dir);
        for a in extra { cmd.arg(a); }
        let status = cmd.status().expect("spawn atrium-spv-compile");
        assert!(status.success(),
            "atrium-spv-compile failed with extras {extra:?}: {status}");
    };
    run(&cache_default, &[]);
    run(&cache_a, &["--spec-const", "0=42"]);
    run(&cache_b, &["--spec-const", "0=99"]);

    let blob_name_in = |d: &std::path::Path| -> String {
        let entries: Vec<_> = std::fs::read_dir(d).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension()
                .map(|x| x == "afblob").unwrap_or(false))
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 1,
            "expected one .afblob in {}, got {:?}", d.display(), entries);
        entries.into_iter().next().unwrap()
    };
    let d = blob_name_in(&cache_default);
    let a = blob_name_in(&cache_a);
    let b_ = blob_name_in(&cache_b);
    assert_ne!(d, a, "default vs --spec-const 0=42 collided");
    assert_ne!(a, b_, "--spec-const 0=42 vs 0=99 collided");
    assert_ne!(d, b_, "default vs --spec-const 0=99 collided");

    // Rerunning with the SAME overrides must hit the SAME hash.
    let cache_a2 = dir.path().join("cache_a2");
    run(&cache_a2, &["--spec-const", "0=42"]);
    assert_eq!(blob_name_in(&cache_a2), a,
        "same --spec-const should be deterministic");
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
