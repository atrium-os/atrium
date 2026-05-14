//! Integration test: drive the ShaderCache end-to-end
//! against a hand-built constant-colour shader.
//!
//! Spawns the real `atrium-spv-compile` binary (built by
//! cargo for this workspace test). Verifies:
//! 1. First call → cache miss, binary runs, .so + .pcmap
//!    land on disk, dlopen succeeds, function pointer
//!    works.
//! 2. Second call with the same SPIR-V → cache hit, no
//!    new compile (timing or file-mtime sanity).
//! 3. The dlopened atrium_fs_main writes the expected
//!    RGBA bytes through the out_color pointer.

use std::path::PathBuf;

use atrium_spv_loader::{LoaderConfig, ShaderCache};
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

/// Locate the workspace-built `atrium-spv-compile` binary.
///
/// Cargo runs this test from
/// `atrium-spv-loader/target/debug/deps/`, but the binary
/// lives in `atrium-spv-compile/target/debug/`. The
/// integration test depends on the binary having been
/// built first; if not, the test fails with a clear
/// "binary missing" error rather than a confusing dlopen
/// failure later.
fn locate_compile_binary() -> PathBuf {
    // current_exe is at
    //   .../atrium-spv-loader/target/debug/deps/cache_smoke-<hash>
    // We need
    //   .../atrium-spv-compile/target/debug/atrium-spv-compile
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    // current_exe = .../atrium-spv-loader/target/debug/deps/cache_smoke-<hash>
    p.pop();   // .../target/debug/deps
    p.pop();   // .../target/debug
    p.pop();   // .../target
    p.pop();   // .../atrium-spv-loader
    p.pop();   // .../bsd
    // sideways into sibling crate
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    if p.exists() {
        return p;
    }
    panic!(
        "atrium-spv-compile binary not found at {}. Build it first:\n  \
         (cd ../atrium-spv-compile && cargo build)",
        p.display(),
    );
}

#[test]
fn cache_miss_compiles_then_hit_returns_same_handle() {
    let rgba = [1.0, 0.5, 0.25, 1.0];
    let spirv = build_constant_color_spirv(rgba);

    let dir = TempDir::new().unwrap();
    let cache = ShaderCache::new(LoaderConfig {
        cache_root: dir.path().to_path_buf(),
        abi_version: 1,
        compile_binary: locate_compile_binary(),
    });

    // First call: cache miss → spawn compile.
    let s1 = cache.load_or_compile(&spirv).expect("first load must succeed");
    assert!(s1.entry_points.fs_main.is_some(), "fs_main symbol must resolve");
    assert!(s1.entry_points.vs_main.is_none());
    assert!(s1.entry_points.cs_main.is_none());

    // The cached files should exist now. A constant-colour
    // shader compiles via the bespoke backend → JIT-emit
    // path → flat `.afblob` artifact, not a `cc`-linked
    // `.so`.
    let hash = ShaderCache::hash(&spirv);
    assert!(cache.blob_path(&hash).exists(),
        "expected {} after compile", cache.blob_path(&hash).display());
    assert!(cache.pcmap_path(&hash).exists());

    // Second call: cache hit → Arc returns same allocation.
    let s2 = cache.load_or_compile(&spirv).expect("cache hit");
    assert!(std::sync::Arc::ptr_eq(&s1, &s2),
        "expected Arc::ptr_eq on cache hit");

    // Call the shader and verify it writes the expected RGBA.
    let fs_main = s1.entry_points.fs_main.unwrap();
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0,
            0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, rgba);
}

#[test]
fn disk_cache_survives_in_memory_eviction() {
    // First-call compile → second-call drop from memory →
    // third-call still hits disk cache without a new
    // compile. Validates that .so + .pcmap persist across
    // ShaderCache lifetime.
    let rgba = [0.3, 0.4, 0.5, 1.0];
    let spirv = build_constant_color_spirv(rgba);

    let dir = TempDir::new().unwrap();
    let cache = ShaderCache::new(LoaderConfig {
        cache_root: dir.path().to_path_buf(),
        abi_version: 1,
        compile_binary: locate_compile_binary(),
    });

    let _ = cache.load_or_compile(&spirv).unwrap();
    let hash = ShaderCache::hash(&spirv);
    cache.forget(&hash);

    // Note when the artifact was last modified, drop the
    // in-memory entry, reload, and confirm no
    // re-compilation (mtime unchanged). The bespoke
    // JIT-emit path's artifact is the `.afblob`.
    let blob_path = cache.blob_path(&hash);
    let mtime_before = std::fs::metadata(&blob_path).unwrap()
        .modified().unwrap();
    let _s3 = cache.load_or_compile(&spirv).unwrap();
    let mtime_after = std::fs::metadata(&blob_path).unwrap()
        .modified().unwrap();
    assert_eq!(mtime_before, mtime_after,
        "disk cache hit must not re-run the compile binary");
}

#[test]
fn unsupported_shader_returns_unsupported() {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionModel, FunctionControl,
        MemoryModel,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.capability(Capability::Float64); // ← rejected by frontend
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
    let cache = ShaderCache::new(LoaderConfig {
        cache_root: dir.path().to_path_buf(),
        abi_version: 1,
        compile_binary: locate_compile_binary(),
    });
    let err = cache.load_or_compile(&spirv).unwrap_err();
    assert!(matches!(err, atrium_spv_loader::LoadError::Unsupported(_)),
            "expected Unsupported, got {err:?}");
}
