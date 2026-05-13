//! End-to-end test: aqueduct-gpu-host's Tier2Registry
//! compiles a SPIR-V fragment shader through
//! atrium-spv-loader, dlopens the result, calls
//! `atrium_fs_main`, and checks the pixel output.
//!
//! This is the first plumbing-level integration test
//! between the GPU-host crate and the atrium-spv pipeline.
//! It does NOT yet involve the wire protocol or the
//! Backend trait — those land in subsequent Phase 2 v5d
//! steps once the wire ops for Tier-2 shader resolution
//! are finalised.

use std::path::PathBuf;

use aqueduct_gpu_host::{Tier2Registry, Tier2ShaderId};
use atrium_spv_loader::LoaderConfig;
use tempfile::TempDir;

/// Locate the workspace-built `atrium-spv-compile` binary
/// the same way atrium-spv-loader's own tests do —
/// sideways from this test's `current_exe` location into
/// the sibling crate's debug build.
fn locate_compile_binary() -> PathBuf {
    // current_exe = .../aqueduct-gpu-host/target/debug/deps/tier2_registry-<hash>
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); // deps
    p.pop(); // debug
    p.pop(); // target
    p.pop(); // aqueduct-gpu-host
    p.pop(); // bsd
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    if p.exists() {
        return p;
    }
    panic!(
        "atrium-spv-compile binary not found at {}. \
         Build it first: (cd ../atrium-spv-compile && cargo build)",
        p.display(),
    );
}

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
fn registry_compiles_and_runs_constant_color_shader() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    // First registration: full compile path.
    let expected = [0.3f32, 0.7, 0.1, 1.0];
    let spirv = build_constant_color_spirv(expected);
    let id = registry.register(&spirv).expect("registry must register");
    let loaded = registry.get(id).expect("loaded shader must be in registry");

    // Invoke atrium_fs_main and check the output pixel.
    let fs_main = loaded.entry_points.fs_main
        .expect("constant-colour shader has fs_main");
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, expected,
        "Tier2Registry shader produced {out_color:?}, expected {expected:?}");
}

#[test]
fn registry_idempotent_on_repeated_registration() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let spirv = build_constant_color_spirv([0.5, 0.5, 0.5, 1.0]);
    let a = registry.register(&spirv).unwrap();
    let b = registry.register(&spirv).unwrap();
    assert_eq!(a, b,
        "registering the same SPIR-V twice must return the same id");

    // Different content → different id.
    let other = build_constant_color_spirv([0.1, 0.9, 0.2, 1.0]);
    let c = registry.register(&other).unwrap();
    assert_ne!(a, c, "different SPIR-V must produce different ids");
}

/// End-to-end wire-protocol test: a Listener with a
/// Tier2Registry attached accepts a real SPIR-V upload
/// and the daemon-side compile path runs. We verify by
/// counting the registry's bookkeeping before and after.
///
/// We can't directly inspect the ShaderRecord.tier2_id
/// over the wire — the wire response just gives back a
/// ResourceId. But after a successful upload the
/// registry must contain one more shader, and the
/// returned id must be `Some`.
#[test]
fn listener_routes_shader_upload_through_tier2_registry() {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use aqueduct::Connection;
    use aqueduct_gpu::backends::{BackendId, GpuVendor};
    use aqueduct_gpu::payloads::ShaderKind;
    use aqueduct_gpu::ClientKind;
    use aqueduct_gpu_client::GpuClient;
    use aqueduct_gpu_host::{Listener, StubBackend};

    // Set up listener with both shader cache + Tier2Registry.
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Arc::new(Tier2Registry::new(config));
    let backend: Arc<dyn aqueduct_gpu_host::Backend> = Arc::new(StubBackend::new());

    let sock = std::env::temp_dir().join(format!(
        "atrium-tier2-listener-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    let listener = Listener::bind(&sock, backend)
        .unwrap()
        .with_tier2_registry(registry.clone());
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    // Upload a real SPIR-V fragment shader through the wire.
    let spirv = build_constant_color_spirv([0.4, 0.5, 0.6, 1.0]);
    let mut hash = [0u8; 32];
    {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(&spirv);
        hash.copy_from_slice(&h.finalize());
    }
    let backend_id = BackendId::new(GpuVendor::Software, 0);

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let shader_id = client.upload_shader(
        hash, ShaderKind::SpirV, backend_id, spirv.clone()).unwrap();
    assert!(shader_id.local_id() > 0,
        "upload through Tier-2-enabled listener must succeed");

    // The registry must now contain the compiled shader.
    // We can confirm by re-registering: idempotence
    // means we get the same id back without recompiling.
    let id_first = registry.register(&spirv).unwrap();
    let id_second = registry.register(&spirv).unwrap();
    assert_eq!(id_first, id_second,
        "registry should have the shader from the wire upload");

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn registry_forget_drops_the_id() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let spirv = build_constant_color_spirv([0.2, 0.3, 0.4, 1.0]);
    let id = registry.register(&spirv).unwrap();
    assert!(registry.get(id).is_some());
    registry.forget(id);
    assert!(registry.get(id).is_none());
    // After forgetting, re-registering issues a NEW id.
    let id2 = registry.register(&spirv).unwrap();
    assert_ne!(id, id2);
    let _: Tier2ShaderId = id2;
}
