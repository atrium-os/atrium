//! End-to-end test: Tier2Backend renders a registered
//! Tier-2 fragment shader into a backend-owned image and
//! the resulting pixels match the shader's expected output.

use std::path::PathBuf;
use std::sync::Arc;

use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu_host::{Backend, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use tempfile::TempDir;

fn locate_compile_binary() -> PathBuf {
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); p.pop(); p.pop(); p.pop(); p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(),
        "atrium-spv-compile binary not found at {}", p.display());
    p
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
    let cs: Vec<_> = rgba.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
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
fn tier2_backend_runs_fragment_shader_into_image() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry.clone());

    // Register a 64×32 image.
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x1000);
    backend.image_created(image_id, 64, 32);

    // Register a constant-colour fragment shader.
    let expected = [0.1f32, 0.8, 0.2, 1.0];
    let spirv = build_constant_color_spirv(expected);
    let shader_id = registry.register(&spirv).expect("register");

    // Run it into the image.
    backend.run_fragment_shader_into(image_id, shader_id, &[], &[])
        .expect("run_fragment_shader_into");

    // Read back the pixels.
    let pixels = backend.read_image_pixels(image_id)
        .expect("image must be registered");
    assert_eq!(pixels.len(), 64 * 32 * 4);
    let eq = |v: f32| (v * 255.0 + 0.5) as u8;
    let expected_u8 = [eq(expected[0]), eq(expected[1]),
                       eq(expected[2]), eq(expected[3])];
    for px in pixels.chunks_exact(4) {
        assert_eq!(px, &expected_u8[..],
            "pixel {px:?} != {expected_u8:?}");
    }
}

#[test]
fn tier2_backend_image_destroyed_drops_storage() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x2000);
    backend.image_created(image_id, 16, 16);
    assert!(backend.read_image_pixels(image_id).is_some());
    backend.image_destroyed(image_id);
    assert!(backend.read_image_pixels(image_id).is_none());
}

#[test]
fn tier2_backend_submit_frame_runs_bound_shader() {
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::opcodes::FrameOp;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry.clone());

    // Set up: 8×4 image + shader + bind pipeline → shader.
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x4000);
    backend.image_created(image_id, 8, 4);
    let expected = [0.7f32, 0.2, 0.5, 1.0];
    let spirv = build_constant_color_spirv(expected);
    let shader_id = registry.register(&spirv).unwrap();
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0x4001);
    backend.bind_pipeline(pipeline_id, shader_id);

    // Build a frame stream: Begin RP → BindPipeline → End RP.
    let mut fb = FrameBuilder::new(1024);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&8u32.to_le_bytes());
    begin[8..12].copy_from_slice(&4u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    // Submit. Pre-submit the image should be zeros.
    let pre = backend.read_image_pixels(image_id).unwrap();
    assert!(pre.iter().all(|b| *b == 0), "pre-submit must be cleared");

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x4002);
    let signalled = backend.submit_frame(fence, 1, fb.as_bytes());
    assert!(signalled);

    // Post-submit: every pixel should be the shader colour.
    let post = backend.read_image_pixels(image_id).unwrap();
    let eq = |v: f32| (v * 255.0 + 0.5) as u8;
    let exp_u8 = [eq(expected[0]), eq(expected[1]),
                  eq(expected[2]), eq(expected[3])];
    for px in post.chunks_exact(4) {
        assert_eq!(px, &exp_u8[..],
            "post-submit pixel {px:?} != expected {exp_u8:?}");
    }
}

#[test]
fn tier2_backend_submit_frame_no_bound_pipeline_leaves_image_unchanged() {
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::opcodes::FrameOp;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x5000);
    backend.image_created(image_id, 4, 4);

    let mut fb = FrameBuilder::new(1024);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&4u32.to_le_bytes());
    begin[8..12].copy_from_slice(&4u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x5002);
    backend.submit_frame(fence, 1, fb.as_bytes());

    // No bound pipeline → no shader fired → image still cleared.
    let pixels = backend.read_image_pixels(image_id).unwrap();
    assert!(pixels.iter().all(|b| *b == 0));
}

/// End-to-end wire integration: shader upload + pipeline
/// create + submit_frame all flow through a real Listener
/// + Session, and the Tier2Backend ends up with the right
/// pipeline → shader binding. We assert via the backend's
/// own state (the wire doesn't yet have a read-pixels op).
#[test]
fn session_pipeline_create_auto_binds_tier2_shader() {
    use std::thread;
    use std::time::Duration;

    use aqueduct::Connection;
    use aqueduct_gpu::ClientKind;
    use aqueduct_gpu::backends::{BackendId, GpuVendor};
    use aqueduct_gpu::payloads::{PipelineKind, ShaderKind};
    use aqueduct_gpu_client::GpuClient;
    use aqueduct_gpu_host::{Backend, Listener};

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Arc::new(Tier2Backend::new(registry.clone()));
    let backend_dyn: Arc<dyn Backend> = backend.clone();

    let sock = std::env::temp_dir().join(format!(
        "atrium-tier2-session-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    let listener = Listener::bind(&sock, backend_dyn)
        .unwrap()
        .with_tier2_registry(registry.clone());
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    // Build a fragment shader the registry can compile.
    let spirv = build_constant_color_spirv([0.9, 0.1, 0.4, 1.0]);
    let mut hash = [0u8; 32];
    {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(&spirv);
        hash.copy_from_slice(&h.finalize());
    }
    let backend_id = BackendId::new(GpuVendor::Software, 2);

    // Connection 1: handshake, upload shader, create pipeline.
    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let shader_id = client.upload_shader(
        hash, ShaderKind::SpirV, backend_id, spirv.clone()).unwrap();
    // Pipeline shaders = [vertex, fragment]; we'll use the
    // same id for both — the upload was a fragment shader,
    // and only the fragment slot matters for Tier-2 auto-
    // bind right now.
    let pipeline_id = client.create_pipeline(
        PipelineKind::Graphics,
        vec![shader_id, shader_id],
        vec![],
    ).unwrap();

    // Give the daemon a beat to process.
    drop(client);
    thread::sleep(Duration::from_millis(50));

    // The backend's pipeline_shaders map should now hold
    // the binding. We don't expose it directly; instead
    // confirm by directly calling submit_frame in this
    // thread on the same backend handle and verifying
    // pixel output.
    let image_id = aqueduct_gpu::ids::ResourceId::new(
        aqueduct_gpu::ids::IdNamespace::IcdRuntime, 0xA002);
    backend.image_created(image_id, 4, 4);

    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::opcodes::FrameOp;
    let mut fb = FrameBuilder::new(1024);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&4u32.to_le_bytes());
    begin[8..12].copy_from_slice(&4u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = aqueduct_gpu::ids::ResourceId::new(
        aqueduct_gpu::ids::IdNamespace::IcdRuntime, 0xA003);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let r = (0.9 * 255.0 + 0.5) as u8;
    let g = (0.1 * 255.0 + 0.5) as u8;
    let b = (0.4 * 255.0 + 0.5) as u8;
    assert_eq!(&pixels[..4], &[r, g, b, 255],
        "session pipeline_create must auto-bind the Tier-2 shader");

    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn tier2_backend_submit_frame_counts() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);
    assert_eq!(backend.submission_count(), 0);
    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x3000);
    let signalled = backend.submit_frame(fence, 1, &[]);
    assert!(signalled);
    assert_eq!(backend.submission_count(), 1);
}

#[test]
fn tier2_backend_buffer_storage_roundtrip() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);

    let buf_id = ResourceId::new(IdNamespace::IcdRuntime, 0xB001);
    backend.buffer_created(buf_id, 64);

    // Initial read sees zeroed bytes.
    let got = backend.read_buffer_bytes(buf_id).unwrap();
    assert_eq!(got.len(), 64);
    assert!(got.iter().all(|&b| b == 0));

    // Inline write at offset 8.
    backend.buffer_write_bytes(buf_id, 8, &[0xAA, 0xBB, 0xCC, 0xDD]).unwrap();
    let got = backend.read_buffer_bytes(buf_id).unwrap();
    assert_eq!(&got[8..12], &[0xAA, 0xBB, 0xCC, 0xDD]);
    assert!(got[..8].iter().all(|&b| b == 0));
    assert!(got[12..].iter().all(|&b| b == 0));

    // Out-of-range write rejected.
    let r = backend.buffer_write_bytes(buf_id, 60, &[0; 8]);
    assert!(r.is_err(), "write past end must error: {:?}", r);

    // Unknown-buffer write rejected.
    let other = ResourceId::new(IdNamespace::IcdRuntime, 0xBEEF);
    let r = backend.buffer_write_bytes(other, 0, &[1, 2, 3]);
    assert!(r.is_err(), "write to unknown buffer must error");

    // Destroy frees the storage.
    backend.buffer_destroyed(buf_id);
    assert!(backend.read_buffer_bytes(buf_id).is_none());
}

#[test]
fn tier2_backend_buffer_oversize_rejected() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);

    let buf_id = ResourceId::new(IdNamespace::IcdRuntime, 0xB002);
    // 1 GiB > 256 MiB cap: silently rejected (logged, not registered).
    backend.buffer_created(buf_id, 1 << 30);
    assert!(backend.read_buffer_bytes(buf_id).is_none());
}

#[test]
fn tier2_backend_draw_walker_increments_draw_count() {
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));

    let spirv = build_constant_color_spirv([0.2, 0.3, 0.4, 1.0]);
    let shader_id = registry.register(&spirv).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id   = ResourceId::new(IdNamespace::IcdRuntime, 0xD001);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xD002);
    let vbuf_id    = ResourceId::new(IdNamespace::IcdRuntime, 0xD003);
    backend.image_created(image_id, 8, 8);
    backend.bind_pipeline(pipeline_id, shader_id);
    backend.bind_layout(pipeline_id, aqueduct_gpu::VertexInputState {
        bindings: vec![aqueduct_gpu::VertexBindingDesc {
            binding: 0, stride: 4, per_instance: false,
        }],
        attributes: vec![aqueduct_gpu::VertexAttributeDesc {
            location: 0, binding: 0,
            format: aqueduct_gpu::VertexFormat::R32Sfloat, offset: 0,
        }],
    });
    backend.buffer_created(vbuf_id, 256);

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&8u32.to_le_bytes());
    begin[8..12].copy_from_slice(&8u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 6, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xD004);
    backend.submit_frame(fence, 1, fb.as_bytes());

    assert_eq!(backend.draw_count(), 2);
    assert_eq!(backend.draws_skipped(), 0);
}

#[test]
fn tier2_backend_draw_without_pipeline_skipped() {
    use aqueduct_gpu::frame::{DrawCmd, FrameBuilder};
    use aqueduct_gpu::opcodes::FrameOp;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);

    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0xD101);
    backend.image_created(image_id, 4, 4);

    let mut fb = FrameBuilder::new(1024);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&4u32.to_le_bytes());
    begin[8..12].copy_from_slice(&4u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xD102);
    backend.submit_frame(fence, 1, fb.as_bytes());

    assert_eq!(backend.draw_count(), 0);
    assert_eq!(backend.draws_skipped(), 1);
}

#[test]
fn tier2_backend_legacy_fullscreen_path_still_fires() {
    // BindPipeline followed by EndRenderPass with NO Draw
    // must still fire the legacy fullscreen FS fill (pre-D.3
    // wire-format shape used by integration tests).
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::opcodes::FrameOp;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let expected = [0.7, 0.2, 0.5, 1.0];
    let spirv = build_constant_color_spirv(expected);
    let shader_id = registry.register(&spirv).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0xD201);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xD202);
    backend.image_created(image_id, 2, 2);
    backend.bind_pipeline(pipeline_id, shader_id);

    let mut fb = FrameBuilder::new(1024);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&2u32.to_le_bytes());
    begin[8..12].copy_from_slice(&2u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xD203);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let r = (expected[0] * 255.0 + 0.5) as u8;
    let g = (expected[1] * 255.0 + 0.5) as u8;
    let b = (expected[2] * 255.0 + 0.5) as u8;
    assert_eq!(&pixels[..4], &[r, g, b, 255]);
    assert_eq!(backend.draw_count(), 0);
}

#[test]
fn tier2_backend_dispatch_assembles_vertex_bytes() {
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder};
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::{
        VertexAttributeDesc, VertexBindingDesc, VertexFormat, VertexInputState,
    };

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let spirv = build_constant_color_spirv([0.1, 0.2, 0.3, 1.0]);
    let shader_id = registry.register(&spirv).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0xE001);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xE002);
    let vbuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0xE003);
    backend.image_created(image_id, 4, 4);
    backend.bind_pipeline(pipeline_id, shader_id);

    // Two attributes per vertex: vec3 position @0, vec2 uv @1.
    // Stride = 12 + 8 = 20 bytes; D.4 packs densely in
    // location order so packed_stride == 20 too.
    backend.bind_layout(pipeline_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 20, per_instance: false,
        }],
        attributes: vec![
            VertexAttributeDesc {
                location: 0, binding: 0,
                format: VertexFormat::R32g32b32Sfloat, offset: 0,
            },
            VertexAttributeDesc {
                location: 1, binding: 0,
                format: VertexFormat::R32g32Sfloat, offset: 12,
            },
        ],
    });

    // 3 vertices: (pos=(1,2,3), uv=(0.5,0.25)),
    //              (pos=(4,5,6), uv=(0.75,0.0)),
    //              (pos=(7,8,9), uv=(1.0,1.0)).
    let mut src = Vec::<u8>::with_capacity(60);
    let verts: [(f32,f32,f32,f32,f32); 3] = [
        (1.0, 2.0, 3.0, 0.5, 0.25),
        (4.0, 5.0, 6.0, 0.75, 0.0),
        (7.0, 8.0, 9.0, 1.0, 1.0),
    ];
    for (x,y,z,u,v) in verts {
        src.extend_from_slice(&x.to_le_bytes());
        src.extend_from_slice(&y.to_le_bytes());
        src.extend_from_slice(&z.to_le_bytes());
        src.extend_from_slice(&u.to_le_bytes());
        src.extend_from_slice(&v.to_le_bytes());
    }
    backend.buffer_created(vbuf_id, src.len() as u64);
    backend.buffer_write_bytes(vbuf_id, 0, &src).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&4u32.to_le_bytes());
    begin[8..12].copy_from_slice(&4u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xE004);
    backend.submit_frame(fence, 1, fb.as_bytes());

    assert_eq!(backend.draw_count(), 1);
    let asm = backend.last_assembled_vertices().expect("vertices assembled");
    assert_eq!(asm.vertex_count, 3);
    assert_eq!(asm.stride, 20);
    assert_eq!(asm.attribute_offsets, vec![0, 12, 20]);
    assert_eq!(asm.bytes.len(), 60);

    // Decode each vertex and verify it round-trips.
    for (i, (x, y, z, u, v)) in verts.iter().enumerate() {
        let base = i * 20;
        let gx = f32::from_le_bytes(asm.bytes[base..base+4].try_into().unwrap());
        let gy = f32::from_le_bytes(asm.bytes[base+4..base+8].try_into().unwrap());
        let gz = f32::from_le_bytes(asm.bytes[base+8..base+12].try_into().unwrap());
        let gu = f32::from_le_bytes(asm.bytes[base+12..base+16].try_into().unwrap());
        let gv = f32::from_le_bytes(asm.bytes[base+16..base+20].try_into().unwrap());
        assert_eq!((gx,gy,gz,gu,gv), (*x,*y,*z,*u,*v),
                   "vertex {i} mismatch");
    }
}

#[test]
fn tier2_backend_dispatch_first_vertex_offset() {
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder};
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::{
        VertexAttributeDesc, VertexBindingDesc, VertexFormat, VertexInputState,
    };

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let spirv = build_constant_color_spirv([0.0, 0.0, 0.0, 1.0]);
    let shader_id = registry.register(&spirv).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0xE101);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xE102);
    let vbuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0xE103);
    backend.image_created(image_id, 4, 4);
    backend.bind_pipeline(pipeline_id, shader_id);
    backend.bind_layout(pipeline_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 4, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32Sfloat, offset: 0,
        }],
    });

    // 5 vertices with values [0, 10, 20, 30, 40].
    let mut src = Vec::<u8>::with_capacity(20);
    for i in 0..5 {
        src.extend_from_slice(&((i as f32) * 10.0).to_le_bytes());
    }
    backend.buffer_created(vbuf_id, src.len() as u64);
    backend.buffer_write_bytes(vbuf_id, 0, &src).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&4u32.to_le_bytes());
    begin[8..12].copy_from_slice(&4u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    // Draw vertices [2..5) -- expect 20, 30, 40.
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1,
        first_vertex: 2, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xE104);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let asm = backend.last_assembled_vertices().unwrap();
    assert_eq!(asm.vertex_count, 3);
    assert_eq!(asm.stride, 4);
    let v0 = f32::from_le_bytes(asm.bytes[0..4].try_into().unwrap());
    let v1 = f32::from_le_bytes(asm.bytes[4..8].try_into().unwrap());
    let v2 = f32::from_le_bytes(asm.bytes[8..12].try_into().unwrap());
    assert_eq!((v0, v1, v2), (20.0, 30.0, 40.0));
}
