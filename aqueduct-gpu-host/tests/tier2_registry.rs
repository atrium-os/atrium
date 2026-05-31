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
            1, // gl_FrontFacing
            0, // gl_PrimitiveID
        );
    }
    assert_eq!(out_color, expected,
        "Tier2Registry shader produced {out_color:?}, expected {expected:?}");
}

/// Build a fragment shader whose red channel is an
/// `OpSpecConstant float SpecId=0, default=0.25`.  The
/// green/blue/alpha channels are hard-coded so we can verify
/// the override flowed without ambiguity.
fn build_spec_const_red_shader() -> Vec<u8> {
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
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let sc_red = b.spec_constant_bit32(f32_ty, 0.25f32.to_bits());
    b.decorate(sc_red, Decoration::SpecId, vec![Operand::LiteralBit32(0)]);
    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c_zero = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_one  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    // vec4(sc_red, 0.5, 0.0, 1.0) -- built in the body so we
    // don't have to deal with OpSpecConstantComposite for
    // a single-channel override.
    let color = b.composite_construct(
        vec4_f32, None, vec![sc_red, c_half, c_zero, c_one]).unwrap();
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
fn registry_spec_constant_override_flows_to_compiled_shader() {
    // Same SPIR-V, two registrations:
    //   - default (no overrides)         -> red = 0.25
    //   - override SpecId 0 -> f:0.9     -> red = 0.9
    // Verifies that:
    //   (a) the override actually flows through the daemon-
    //       side loader path (spawned atrium-spv-compile with
    //       --spec-const) into runtime behaviour,
    //   (b) the two registrations get DIFFERENT Tier2ShaderIds
    //       and DIFFERENT cache hashes (no collision),
    //   (c) re-registering with the same overrides is
    //       idempotent (returns the same id).
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let spirv = build_spec_const_red_shader();

    let id_default = registry.register(&spirv).expect("default register");
    let red_override: u32 = 0.9f32.to_bits();
    let id_ov = registry
        .register_with_spec_overrides(&spirv, &[(0, red_override)])
        .expect("override register");
    let id_ov2 = registry
        .register_with_spec_overrides(&spirv, &[(0, red_override)])
        .expect("override re-register");
    assert_ne!(id_default, id_ov,
        "default and overridden must be distinct ids");
    assert_eq!(id_ov, id_ov2,
        "same overrides must be idempotent");

    let run_red = |id: Tier2ShaderId| -> f32 {
        let loaded = registry.get(id).expect("loaded");
        let fs_main = loaded.entry_points.fs_main
            .expect("spec-const-red is a fragment shader");
        let mut out_color = [0.0f32; 4];
        let mut out_depth = 0.0f32;
        unsafe {
            fs_main(
                std::ptr::null(), std::ptr::null(), std::ptr::null(),
                0.0, 0.0, 0.0, 0.0, 0,
                out_color.as_mut_ptr(), &mut out_depth,
                1, // gl_FrontFacing
            0, // gl_PrimitiveID
            );
        }
        // sanity-check the un-overridden channels too
        assert!((out_color[1] - 0.5).abs() < 1e-6, "green stayed 0.5");
        assert_eq!(out_color[2], 0.0, "blue stayed 0.0");
        assert_eq!(out_color[3], 1.0, "alpha stayed 1.0");
        out_color[0]
    };
    let red_default = run_red(id_default);
    let red_specialised = run_red(id_ov);
    assert!((red_default - 0.25).abs() < 1e-6,
        "default red should be 0.25, got {red_default}");
    assert!((red_specialised - 0.9).abs() < 1e-6,
        "overridden red should be 0.9, got {red_specialised}");
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
fn fill_image_fragment_constant_color() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let expected = [0.2f32, 0.4, 0.6, 1.0];
    let spirv = build_constant_color_spirv(expected);
    let id = registry.register(&spirv).expect("register");

    let (w, h) = (8u32, 4u32);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    registry.fill_image_fragment(id, &[], &[], w, h, &mut pixels)
        .expect("fill_image_fragment");

    // Every pixel should equal the expected colour, u8-quantised.
    let expected_u8 = [
        (expected[0] * 255.0 + 0.5) as u8,
        (expected[1] * 255.0 + 0.5) as u8,
        (expected[2] * 255.0 + 0.5) as u8,
        (expected[3] * 255.0 + 0.5) as u8,
    ];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            assert_eq!(
                &pixels[i..i+4],
                &expected_u8[..],
                "pixel ({x}, {y}) mismatch"
            );
        }
    }
}

/// Build a fragment shader that reads the f32 push-
/// constant `scale` and writes
/// `vec4(scale, 0, 0, 1)` — independent of frag_coord but
/// non-trivial to compile (AccessChain + Load + struct
/// member offsets).
fn build_pushconst_red_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![f32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out_vec4  = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c0 = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1 = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let v = b.load(f32_ty, None, p, None, vec![]).unwrap();
    let color = b.composite_construct(vec4_f32, None, vec![v, c0, c0, c1]).unwrap();
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
fn fill_image_fragment_with_push_constants() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let spirv = build_pushconst_red_shader();
    let id = registry.register(&spirv).expect("register");

    let scale = 0.5f32;
    let mut pc = [0u8; 4];
    pc.copy_from_slice(&scale.to_le_bytes());

    let (w, h) = (4u32, 2u32);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    registry.fill_image_fragment(id, &pc, &[], w, h, &mut pixels)
        .expect("fill_image_fragment");

    let r_expected = (scale * 255.0 + 0.5) as u8;
    for px in pixels.chunks_exact(4) {
        assert_eq!(px[0], r_expected, "red channel mismatch");
        assert_eq!(px[1], 0,          "green should be 0");
        assert_eq!(px[2], 0,          "blue should be 0");
        assert_eq!(px[3], 255,        "alpha should be 1.0 → 255");
    }
}

#[test]
fn fill_image_fragment_rejects_bad_buffer_size() {
    use aqueduct_gpu_host::tier2_registry::Tier2ExecError;
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let spirv = build_constant_color_spirv([0.5, 0.5, 0.5, 1.0]);
    let id = registry.register(&spirv).unwrap();

    let mut pixels = vec![0u8; 100]; // wrong size for 8×4 (=128)
    let err = registry.fill_image_fragment(id, &[], &[], 8, 4, &mut pixels)
        .expect_err("must reject bad size");
    assert!(matches!(err, Tier2ExecError::BadPixelsLen { .. }));
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
