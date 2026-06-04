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

// ---- D.5: hello-triangle end-to-end through the wire ----

fn build_passthrough_vs_d5() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let per_vertex = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);

    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4);
    let ptr_in_vec3  = b.type_pointer(None, StorageClass::Input, vec3);

    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_pos, pv_var]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn tier2_backend_d5_hello_triangle_through_wire() {
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder, SetViewportCmd};
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

    let vs_id = registry.register(&build_passthrough_vs_d5()).expect("vs");
    let fs_id = registry.register(&build_constant_color_spirv([1.0, 0.2, 0.2, 1.0]))
        .expect("fs");
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0xF001);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF002);
    let vbuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0xF003);

    backend.image_created(image_id, 8, 8);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    backend.bind_layout(pipeline_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    });

    // Same NDC triangle as rasterizer_r1_hello_triangle.
    let mut src = Vec::<u8>::with_capacity(36);
    for v in [[-0.5_f32, -0.5, 0.0], [0.5, -0.5, 0.0], [0.0, 0.5, 0.0]] {
        for f in v { src.extend_from_slice(&f.to_le_bytes()); }
    }
    backend.buffer_created(vbuf_id, 36);
    backend.buffer_write_bytes(vbuf_id, 0, &src).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xF004);
    backend.submit_frame(fence, 1, fb.as_bytes());

    assert_eq!(backend.draw_count(), 1);

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    let red = [255u8, 51, 51, 255];

    // Interior pixels of the same triangle the rasterizer
    // unit test verified.
    assert_eq!(px(2, 2), red, "inside (2,2): {:?}", px(2, 2));
    assert_eq!(px(4, 2), red, "inside (4,2): {:?}", px(4, 2));
    assert_eq!(px(3, 3), red, "inside (3,3): {:?}", px(3, 3));
    assert_eq!(px(4, 4), red, "inside (4,4): {:?}", px(4, 4));

    // Outside pixels stay at cleared (0,0,0,0).
    assert_eq!(px(0, 0), [0, 0, 0, 0]);
    assert_eq!(px(7, 7), [0, 0, 0, 0]);
    assert_eq!(px(4, 0), [0, 0, 0, 0]);
}

#[test]
fn tier2_backend_d6_depth_test_rejects_farther_fragments() {
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::{
        Tier2DepthState, VertexAttributeDesc, VertexBindingDesc,
        VertexFormat, VertexInputState,
    };

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));

    // Same VS, two FSs (red, blue), two pipelines with
    // identical depth + layout state. Triangle 1 (red) draws
    // at z=0.3; triangle 2 (blue) draws at z=0.7 covering the
    // same pixels. With LESS depth test, triangle 2's pixels
    // fail (0.7 < 0.3 false), so pixels stay red.
    let vs_id      = registry.register(&build_passthrough_vs_d5()).expect("vs");
    let fs_red_id  = registry.register(&build_constant_color_spirv([1.0, 0.0, 0.0, 1.0])).unwrap();
    let fs_blue_id = registry.register(&build_constant_color_spirv([0.0, 0.0, 1.0, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x10001);
    let pipe_red_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x10002);
    let pipe_blue_id = ResourceId::new(IdNamespace::IcdRuntime, 0x10003);
    let vbuf_red_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x10004);
    let vbuf_blue_id = ResourceId::new(IdNamespace::IcdRuntime, 0x10005);
    backend.image_created(image_id, 8, 8);

    let layout = VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    };
    let depth_on = Some(Tier2DepthState {
        test_enable: true, write_enable: true, ..Default::default()
    });

    for (pipe, fs) in [(pipe_red_id, fs_red_id), (pipe_blue_id, fs_blue_id)] {
        backend.bind_pipeline_vs(pipe, vs_id);
        backend.bind_pipeline(pipe, fs);
        backend.bind_layout(pipe, layout.clone());
        backend.bind_raster_state(pipe, depth_on, None, &[], None, Default::default(), None, false);
    }

    // Same NDC triangle at two different z values.
    fn write_triangle(buf: &mut Vec<u8>, z: f32) {
        for v in [[-0.5_f32, -0.5, z], [0.5, -0.5, z], [0.0, 0.5, z]] {
            for f in v { buf.extend_from_slice(&f.to_le_bytes()); }
        }
    }
    let mut red_src = Vec::new();
    write_triangle(&mut red_src, 0.3);
    let mut blue_src = Vec::new();
    write_triangle(&mut blue_src, 0.7);
    backend.buffer_created(vbuf_red_id,  36);
    backend.buffer_created(vbuf_blue_id, 36);
    backend.buffer_write_bytes(vbuf_red_id,  0, &red_src).unwrap();
    backend.buffer_write_bytes(vbuf_blue_id, 0, &blue_src).unwrap();

    let mut fb = FrameBuilder::new(8192);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    // Red first @ z=0.3.
    fb.push(FrameOp::BindPipeline, &pipe_red_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_red_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    // Blue second @ z=0.7 -- must lose to depth-test.
    fb.push(FrameOp::BindPipeline, &pipe_blue_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_blue_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x10006);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    // Interior pixels must still be red (255, 0, 0, 255),
    // proving the deeper blue draw was rejected by depth test.
    assert_eq!(px(3, 3), [255, 0, 0, 255], "(3,3) = {:?}", px(3, 3));
    assert_eq!(px(4, 3), [255, 0, 0, 255], "(4,3) = {:?}", px(4, 3));
    assert_eq!(px(4, 4), [255, 0, 0, 255], "(4,4) = {:?}", px(4, 4));
}

#[test]
fn tier2_backend_d6_depth_disabled_means_later_wins() {
    // Control: same setup but depth disabled => later (blue)
    // draw wins, proving the D.6 plumbing is actually
    // depth-test-conditional rather than always-on.
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder, SetViewportCmd};
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
    let vs_id      = registry.register(&build_passthrough_vs_d5()).unwrap();
    let fs_red_id  = registry.register(&build_constant_color_spirv([1.0, 0.0, 0.0, 1.0])).unwrap();
    let fs_blue_id = registry.register(&build_constant_color_spirv([0.0, 0.0, 1.0, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x11001);
    let pipe_red_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x11002);
    let pipe_blue_id = ResourceId::new(IdNamespace::IcdRuntime, 0x11003);
    let vbuf_red_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x11004);
    let vbuf_blue_id = ResourceId::new(IdNamespace::IcdRuntime, 0x11005);
    backend.image_created(image_id, 8, 8);

    let layout = VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    };
    for (pipe, fs) in [(pipe_red_id, fs_red_id), (pipe_blue_id, fs_blue_id)] {
        backend.bind_pipeline_vs(pipe, vs_id);
        backend.bind_pipeline(pipe, fs);
        backend.bind_layout(pipe, layout.clone());
        backend.bind_raster_state(pipe, None, None, &[], None, Default::default(), None, false); // depth OFF
    }

    let mut red_src = Vec::new();
    let mut blue_src = Vec::new();
    for v in [[-0.5_f32, -0.5, 0.3], [0.5, -0.5, 0.3], [0.0, 0.5, 0.3]] {
        for f in v { red_src.extend_from_slice(&f.to_le_bytes()); }
    }
    for v in [[-0.5_f32, -0.5, 0.7], [0.5, -0.5, 0.7], [0.0, 0.5, 0.7]] {
        for f in v { blue_src.extend_from_slice(&f.to_le_bytes()); }
    }
    backend.buffer_created(vbuf_red_id,  36);
    backend.buffer_created(vbuf_blue_id, 36);
    backend.buffer_write_bytes(vbuf_red_id,  0, &red_src).unwrap();
    backend.buffer_write_bytes(vbuf_blue_id, 0, &blue_src).unwrap();

    let mut fb = FrameBuilder::new(8192);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    fb.push(FrameOp::BindPipeline, &pipe_red_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_red_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::BindPipeline, &pipe_blue_id.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_blue_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x11006);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    // Blue wins because depth test is off and blue was last.
    assert_eq!(px(3, 3), [0, 0, 255, 255], "(3,3) = {:?}", px(3, 3));
    assert_eq!(px(4, 4), [0, 0, 255, 255], "(4,4) = {:?}", px(4, 4));
}

#[test]
fn tier2_backend_d7_multi_primitive_quad_in_one_draw() {
    // Draw{vertex_count: 6} as two triangles (a quad covering
    // the full 8x8 image), to prove the walker iterates all
    // tri_count primitives in a single Draw record.
    use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder, SetViewportCmd};
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
    let vs_id = registry.register(&build_passthrough_vs_d5()).unwrap();
    let fs_id = registry.register(&build_constant_color_spirv([0.0, 1.0, 0.0, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0x12001);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0x12002);
    let vbuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x12003);
    backend.image_created(image_id, 8, 8);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    backend.bind_layout(pipeline_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    });

    // Full-screen NDC quad as 2 triangles (6 vertices):
    //   tri 1: (-1,-1), (1,-1), (-1, 1)
    //   tri 2: (1,-1), (1, 1), (-1, 1)
    let mut src = Vec::<u8>::new();
    let verts = [
        [-1.0f32, -1.0, 0.0], [ 1.0, -1.0, 0.0], [-1.0,  1.0, 0.0],
        [ 1.0,    -1.0, 0.0], [ 1.0,  1.0, 0.0], [-1.0,  1.0, 0.0],
    ];
    for v in verts { for f in v { src.extend_from_slice(&f.to_le_bytes()); } }
    backend.buffer_created(vbuf_id, 72);
    backend.buffer_write_bytes(vbuf_id, 0, &src).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 6, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x12004);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let green = [0u8, 255, 0, 255];
    // Every pixel of the 8x8 should be green (the quad covers
    // the whole NDC space). If only one triangle ran the
    // diagonal pixels on the wrong side would be cleared.
    for y in 0..8 {
        for x in 0..8 {
            let i = (y * 8 + x) * 4;
            assert_eq!(&pixels[i..i+4], &green[..],
                "pixel ({x},{y}) = {:?}", &pixels[i..i+4]);
        }
    }
}

#[test]
fn tier2_backend_d8_draw_indexed_uint16_hello_triangle() {
    use aqueduct_gpu::frame::{
        BindIndexBufCmd, BindVertexBufCmd, DrawIndexedCmd, FrameBuilder,
        IndexType, SetViewportCmd,
    };
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

    let vs_id = registry.register(&build_passthrough_vs_d5()).unwrap();
    let fs_id = registry.register(&build_constant_color_spirv([1.0, 0.2, 0.2, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0x13001);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0x13002);
    let vbuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x13003);
    let ibuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x13004);
    backend.image_created(image_id, 8, 8);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    backend.bind_layout(pipeline_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    });

    // Same NDC vertices as the D.5 hello-triangle, but with
    // an extra unused vertex at the front to prove indices
    // are honoured (not just sequential).
    let mut vsrc = Vec::<u8>::new();
    for v in [
        [9.0_f32, 9.0, 9.0],     // index 0: dummy, never referenced
        [-0.5,   -0.5, 0.0],     // index 1
        [ 0.5,   -0.5, 0.0],     // index 2
        [ 0.0,    0.5, 0.0],     // index 3
    ] {
        for f in v { vsrc.extend_from_slice(&f.to_le_bytes()); }
    }
    backend.buffer_created(vbuf_id, vsrc.len() as u64);
    backend.buffer_write_bytes(vbuf_id, 0, &vsrc).unwrap();

    // uint16 index buffer: [1, 2, 3] -- skip the dummy.
    let idx: [u16; 3] = [1, 2, 3];
    let isrc: Vec<u8> = idx.iter().flat_map(|i| i.to_le_bytes()).collect();
    backend.buffer_created(ibuf_id, isrc.len() as u64);
    backend.buffer_write_bytes(ibuf_id, 0, &isrc).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    fb.push_bind_index_buf(BindIndexBufCmd {
        buffer_id: ibuf_id.raw(), index_type: IndexType::Uint16, offset: 0,
    }).unwrap();
    fb.push_draw_indexed(DrawIndexedCmd {
        index_count: 3, instance_count: 1, first_index: 0,
        vertex_offset: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x13005);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    let red = [255u8, 51, 51, 255];
    assert_eq!(px(2, 2), red);
    assert_eq!(px(4, 2), red);
    assert_eq!(px(3, 3), red);
    assert_eq!(px(4, 4), red);
    assert_eq!(px(0, 0), [0, 0, 0, 0]);
    assert_eq!(px(7, 7), [0, 0, 0, 0]);
}

#[test]
fn tier2_backend_d8_draw_indexed_uint32_with_vertex_offset() {
    use aqueduct_gpu::frame::{
        BindIndexBufCmd, BindVertexBufCmd, DrawIndexedCmd, FrameBuilder,
        IndexType, SetViewportCmd,
    };
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
    let vs_id = registry.register(&build_passthrough_vs_d5()).unwrap();
    let fs_id = registry.register(&build_constant_color_spirv([0.2, 0.2, 1.0, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0x14001);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0x14002);
    let vbuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x14003);
    let ibuf_id     = ResourceId::new(IdNamespace::IcdRuntime, 0x14004);
    backend.image_created(image_id, 8, 8);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    backend.bind_layout(pipeline_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    });

    // Real triangle at vertex slots 5..7. Indices = [0, 1, 2]
    // + vertex_offset = 5 -> slots 5, 6, 7.
    let mut vsrc = Vec::<u8>::new();
    // Slots 0..4: dummies.
    for _ in 0..5 {
        for f in [9.0_f32, 9.0, 9.0] {
            vsrc.extend_from_slice(&f.to_le_bytes());
        }
    }
    for v in [[-0.5_f32, -0.5, 0.0], [0.5, -0.5, 0.0], [0.0, 0.5, 0.0]] {
        for f in v { vsrc.extend_from_slice(&f.to_le_bytes()); }
    }
    backend.buffer_created(vbuf_id, vsrc.len() as u64);
    backend.buffer_write_bytes(vbuf_id, 0, &vsrc).unwrap();

    let idx: [u32; 3] = [0, 1, 2];
    let isrc: Vec<u8> = idx.iter().flat_map(|i| i.to_le_bytes()).collect();
    backend.buffer_created(ibuf_id, isrc.len() as u64);
    backend.buffer_write_bytes(ibuf_id, 0, &isrc).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
    }).unwrap();
    fb.push_bind_index_buf(BindIndexBufCmd {
        buffer_id: ibuf_id.raw(), index_type: IndexType::Uint32, offset: 0,
    }).unwrap();
    fb.push_draw_indexed(DrawIndexedCmd {
        index_count: 3, instance_count: 1, first_index: 0,
        vertex_offset: 5, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x14005);
    backend.submit_frame(fence, 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let blue = [51u8, 51, 255, 255];
    let i = (3 * 8 + 3) * 4;
    assert_eq!(&pixels[i..i+4], &blue[..], "(3,3) = {:?}", &pixels[i..i+4]);
}

#[test]
fn tier2_backend_present_invokes_callback() {
    use std::sync::Mutex as StdMutex;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);

    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x20001);
    backend.image_created(image_id, 4, 4);

    // Capture all (surface_id, w, h, frame_id, first_4_pixel_bytes)
    // tuples the callback receives.
    let captured: Arc<StdMutex<Vec<(u64, u32, u32, u64, [u8; 4])>>> =
        Arc::new(StdMutex::new(Vec::new()));
    let captured_cb = captured.clone();
    backend.set_present_callback(Box::new(move |surface_id, frame| {
        let mut head = [0u8; 4];
        head.copy_from_slice(&frame.pixels[..4]);
        captured_cb.lock().unwrap().push((
            surface_id, frame.width, frame.height, frame.frame_id, head,
        ));
    }));

    backend.present(image_id, 999, 7);
    backend.present(image_id, 999, 8);
    backend.present(image_id, 1000, 9);

    let got = captured.lock().unwrap().clone();
    assert_eq!(got.len(), 3, "callback should fire once per present");
    assert_eq!(got[0].0, 999); assert_eq!(got[0].3, 7);
    assert_eq!(got[1].0, 999); assert_eq!(got[1].3, 8);
    assert_eq!(got[2].0, 1000); assert_eq!(got[2].3, 9);
    for (_, w, h, _, _) in &got {
        assert_eq!(*w, 4);
        assert_eq!(*h, 4);
    }

    // Clear hook -> further presents don't append.
    backend.clear_present_callback();
    backend.present(image_id, 999, 10);
    assert_eq!(captured.lock().unwrap().len(), 3,
        "callback should not fire after clear");

    // last_presented_frame still tracks (callback is push, the
    // accessor is the pull mirror).
    let f = backend.last_presented_frame(999).unwrap();
    assert_eq!(f.frame_id, 10);
}

#[test]
fn tier2_backend_present_skips_callback_when_image_missing() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);

    let fired = Arc::new(std::sync::Mutex::new(0u32));
    let fired_cb = fired.clone();
    backend.set_present_callback(Box::new(move |_, _| {
        *fired_cb.lock().unwrap() += 1;
    }));

    // Image was never registered -- present should warn-and-skip.
    let bogus = ResourceId::new(IdNamespace::IcdRuntime, 0x20099);
    backend.present(bogus, 5, 1);

    assert_eq!(*fired.lock().unwrap(), 0,
        "callback must not fire for a present whose source image is unknown");
}

#[test]
fn tier2_backend_depth_attachment_persists_across_draws() {
    use aqueduct_gpu::frame::{
        BindDepthAttachmentCmd, BindVertexBufCmd, DrawCmd, FrameBuilder,
        SetViewportCmd,
    };
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::{
        Tier2DepthState, VertexAttributeDesc, VertexBindingDesc,
        VertexFormat, VertexInputState,
    };

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let vs_id = registry.register(&build_passthrough_vs_d5()).expect("vs");
    let fs_red  = registry.register(&build_constant_color_spirv([1.0, 0.0, 0.0, 1.0])).unwrap();
    let fs_blue = registry.register(&build_constant_color_spirv([0.0, 0.0, 1.0, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let color_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x40001);
    let depth_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x40002);
    let pipe_red  = ResourceId::new(IdNamespace::IcdRuntime, 0x40003);
    let pipe_blue = ResourceId::new(IdNamespace::IcdRuntime, 0x40004);
    let vbuf_red  = ResourceId::new(IdNamespace::IcdRuntime, 0x40005);
    let vbuf_blue = ResourceId::new(IdNamespace::IcdRuntime, 0x40006);

    backend.image_created(color_id, 8, 8);
    backend.register_depth_image(depth_id, 8, 8);

    let layout = VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    };
    let depth_on = Some(Tier2DepthState {
        test_enable: true, write_enable: true, ..Default::default()
    });
    for (pipe, fs) in [(pipe_red, fs_red), (pipe_blue, fs_blue)] {
        backend.bind_pipeline_vs(pipe, vs_id);
        backend.bind_pipeline(pipe, fs);
        backend.bind_layout(pipe, layout.clone());
        backend.bind_raster_state(pipe, depth_on, None, &[], None, Default::default(), None, false);
    }

    // Red @ z=0.3 first, blue @ z=0.7 second.  LESS depth
    // test makes blue lose: red pixels stay AND the depth
    // image's pixels reflect z=0.3 in the triangle's interior.
    let mut red_src = Vec::new();
    let mut blue_src = Vec::new();
    for v in [[-0.5_f32, -0.5, 0.3], [0.5, -0.5, 0.3], [0.0, 0.5, 0.3]] {
        for f in v { red_src.extend_from_slice(&f.to_le_bytes()); }
    }
    for v in [[-0.5_f32, -0.5, 0.7], [0.5, -0.5, 0.7], [0.0, 0.5, 0.7]] {
        for f in v { blue_src.extend_from_slice(&f.to_le_bytes()); }
    }
    backend.buffer_created(vbuf_red,  36);
    backend.buffer_created(vbuf_blue, 36);
    backend.buffer_write_bytes(vbuf_red,  0, &red_src).unwrap();
    backend.buffer_write_bytes(vbuf_blue, 0, &blue_src).unwrap();

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&color_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push_bind_depth_attachment(BindDepthAttachmentCmd {
        image_id: depth_id.raw(), clear_value: 1.0,
    }).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    // Red @ z=0.3
    fb.push(FrameOp::BindPipeline, &pipe_red.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_red.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    // Blue @ z=0.7 -- loses to depth test
    fb.push(FrameOp::BindPipeline, &pipe_blue.raw().to_le_bytes()).unwrap();
    fb.push_bind_vertex_buf(BindVertexBufCmd {
        binding: 0, buffer_id: vbuf_blue.raw(), offset: 0,
    }).unwrap();
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x40007);
    backend.submit_frame(fence, 1, fb.as_bytes());

    // Color: interior pixels stay red (blue rejected by depth).
    let pixels = backend.read_image_pixels(color_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    assert_eq!(px(3, 3), [255, 0, 0, 255]);
    assert_eq!(px(4, 4), [255, 0, 0, 255]);

    // Depth image was written: interior pixels show z=0.3
    // (the red draw's value), background pixels still at the
    // BindDepthAttachment's clear_value (1.0).
    let depth = backend.read_depth_image_pixels(depth_id).unwrap();
    let dz = |x: usize, y: usize| -> f32 { depth[y * 8 + x] };
    assert!((dz(3, 3) - 0.3).abs() < 1e-5,
            "(3,3) depth = {}, expected 0.3", dz(3, 3));
    assert!((dz(4, 4) - 0.3).abs() < 1e-5,
            "(4,4) depth = {}, expected 0.3", dz(4, 4));
    assert!((dz(0, 0) - 1.0).abs() < 1e-5,
            "(0,0) depth = {}, expected 1.0 (cleared)", dz(0, 0));
    assert!((dz(7, 7) - 1.0).abs() < 1e-5,
            "(7,7) depth = {}, expected 1.0 (cleared)", dz(7, 7));
}

#[test]
fn tier2_backend_depth_attachment_persists_across_passes() {
    // Two render passes back-to-back against the same depth
    // image. The cleared depth image from pass 1 should be
    // re-cleared at the start of pass 2 (Vulkan's default
    // LoadOp = Clear semantics).
    use aqueduct_gpu::frame::{
        BindDepthAttachmentCmd, BindVertexBufCmd, DrawCmd, FrameBuilder,
        SetViewportCmd,
    };
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::{
        Tier2DepthState, VertexAttributeDesc, VertexBindingDesc,
        VertexFormat, VertexInputState,
    };

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let vs_id = registry.register(&build_passthrough_vs_d5()).unwrap();
    let fs_id = registry.register(&build_constant_color_spirv([0.0, 1.0, 0.0, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let color_id = ResourceId::new(IdNamespace::IcdRuntime, 0x41001);
    let depth_id = ResourceId::new(IdNamespace::IcdRuntime, 0x41002);
    let pipe_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x41003);
    let vbuf_id  = ResourceId::new(IdNamespace::IcdRuntime, 0x41004);
    backend.image_created(color_id, 8, 8);
    backend.register_depth_image(depth_id, 8, 8);
    backend.bind_pipeline_vs(pipe_id, vs_id);
    backend.bind_pipeline(pipe_id, fs_id);
    backend.bind_layout(pipe_id, VertexInputState {
        bindings: vec![VertexBindingDesc {
            binding: 0, stride: 12, per_instance: false,
        }],
        attributes: vec![VertexAttributeDesc {
            location: 0, binding: 0,
            format: VertexFormat::R32g32b32Sfloat, offset: 0,
        }],
    });
    backend.bind_raster_state(pipe_id,
        Some(Tier2DepthState { test_enable: true, write_enable: true, ..Default::default() }),
        None, &[], None, Default::default(), None, false);

    let mut src = Vec::new();
    for v in [[-0.5_f32, -0.5, 0.5], [0.5, -0.5, 0.5], [0.0, 0.5, 0.5]] {
        for f in v { src.extend_from_slice(&f.to_le_bytes()); }
    }
    backend.buffer_created(vbuf_id, 36);
    backend.buffer_write_bytes(vbuf_id, 0, &src).unwrap();

    // Build a frame with TWO render passes: each binds the same
    // depth image with clear=1.0, draws the triangle at z=0.5.
    let mut fb = FrameBuilder::new(8192);
    for _pass in 0..2 {
        let mut begin = [0u8; 12];
        begin[..4].copy_from_slice(&color_id.raw().to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
        fb.push_bind_depth_attachment(BindDepthAttachmentCmd {
            image_id: depth_id.raw(), clear_value: 1.0,
        }).unwrap();
        fb.push_set_viewport(SetViewportCmd {
            x: 0.0, y: 0.0, width: 8.0, height: 8.0,
            min_depth: 0.0, max_depth: 1.0,
        }).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe_id.raw().to_le_bytes()).unwrap();
        fb.push_bind_vertex_buf(BindVertexBufCmd {
            binding: 0, buffer_id: vbuf_id.raw(), offset: 0,
        }).unwrap();
        fb.push_draw(DrawCmd {
            vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0,
        }).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    }

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x41005);
    backend.submit_frame(fence, 1, fb.as_bytes());

    // Both passes drew the same triangle at z=0.5; final depth
    // image's interior is 0.5 (regardless of which pass won
    // the LESS test -- both equal-z passes from a freshly-
    // cleared depth buffer write 0.5).
    let depth = backend.read_depth_image_pixels(depth_id).unwrap();
    let dz = |x: usize, y: usize| -> f32 { depth[y * 8 + x] };
    assert!((dz(3, 3) - 0.5).abs() < 1e-5);
    assert!((dz(0, 0) - 1.0).abs() < 1e-5,
            "background still cleared after both passes; got {}", dz(0, 0));
}

/// A vertex shader that emits a full-screen triangle purely from
/// `gl_VertexIndex` — no vertex-input attributes, no bound vertex
/// buffer. This is the canonical post-processing / present pattern
/// (`vkCmdDraw(3, 1, 0, 0)`). Identical to the cross-tier shaded
/// test's builder: idx → clip pos covering [-1,1]² with one tri.
fn build_fullscreen_tri_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![v4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let ptr_in_i32 = b.type_pointer(None, StorageClass::Input, i32t);
    let in_idx = b.variable(ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(in_idx, Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c0i = b.constant_bit32(i32t, 0);
    let c1i = b.constant_bit32(i32t, 1);
    let c2i = b.constant_bit32(i32t, 2);
    let c2f = b.constant_bit32(f32t, 2.0f32.to_bits());
    let c1f = b.constant_bit32(f32t, 1.0f32.to_bits());
    let c0f = b.constant_bit32(f32t, 0.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let idx = b.load(i32t, None, in_idx, None, vec![]).unwrap();
    let sh = b.shift_left_logical(i32t, None, idx, c1i).unwrap();
    let xb = b.bitwise_and(i32t, None, sh, c2i).unwrap();
    let yb = b.bitwise_and(i32t, None, idx, c2i).unwrap();
    let xf = b.convert_s_to_f(f32t, None, xb).unwrap();
    let yf = b.convert_s_to_f(f32t, None, yb).unwrap();
    let xm = b.f_mul(f32t, None, xf, c2f).unwrap();
    let x = b.f_sub(f32t, None, xm, c1f).unwrap();
    let ym = b.f_mul(f32t, None, yf, c2f).unwrap();
    let y = b.f_sub(f32t, None, ym, c1f).unwrap();
    let pos = b.composite_construct(v4, None, vec![x, y, c0f, c1f]).unwrap();
    let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c0i]).unwrap();
    b.store(dst, pos, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_idx, pv_var]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Regression: a vertex-less draw — a full-screen-triangle VS that
/// reads only `gl_VertexIndex`, with NO vertex-input layout ever
/// bound and NO vertex buffer — must render, not be skipped.
///
/// Before the fix, `dispatch_draw` bailed with "pipeline … has no
/// vertex-input layout; skipping" (observed in the in-VM routing
/// demo), leaving the surface unrenderable on Tier-2. The fix
/// defaults an absent layout to an empty `VertexInputState`, so the
/// VS runs once per vertex with the correct `gl_VertexIndex`.
#[test]
fn tier2_backend_vertexless_fullscreen_triangle_renders() {
    use aqueduct_gpu::frame::{DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;

    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));

    let vs_id = registry.register(&build_fullscreen_tri_vs()).expect("vs");
    let fs_id = registry
        .register(&build_constant_color_spirv([0.1, 0.8, 0.3, 1.0]))
        .expect("fs");
    let backend = Tier2Backend::new(registry);

    let image_id    = ResourceId::new(IdNamespace::IcdRuntime, 0xF101);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF102);

    backend.image_created(image_id, 8, 8);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    // Deliberately NO bind_layout: this is the vertex-less case.

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: 8.0, height: 8.0,
        min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    // No BindVertexBuf. Three vertices sourced from gl_VertexIndex.
    fb.push_draw(DrawCmd {
        vertex_count: 3, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xF104);
    backend.submit_frame(fence, 1, fb.as_bytes());

    // The draw must have executed (not skipped for "no layout").
    assert_eq!(backend.draw_count(), 1, "vertex-less draw was skipped");

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    // FS constant [0.1, 0.8, 0.3, 1.0] → ~[26, 204, 77, 255].
    let green = [26u8, 204, 77, 255];
    // The full-screen triangle covers (-1,-1),(3,-1),(-1,3) — every
    // pixel of the 8×8 target is inside it.
    for (x, y) in [(0, 0), (4, 4), (7, 0), (0, 7), (3, 5)] {
        assert_eq!(px(x, y), green,
            "vertex-less full-screen triangle should cover ({x},{y}); \
             got {:?}", px(x, y));
    }
}

/// Vertex-less full-screen-triangle VS that also writes screen-space UV in
/// [0,1] as a `Location=0` vec4 varying. Pairs with a varying-reading FS to
/// exercise the VS→FS varying path on the compiled Tier-2 backend.
fn build_fullscreen_tri_uv_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![v4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let ptr_in_i32 = b.type_pointer(None, StorageClass::Input, i32t);
    let in_idx = b.variable(ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(in_idx, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let uv_var = b.variable(ptr_out_v4, None, StorageClass::Output, None);
    b.decorate(uv_var, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let c0i = b.constant_bit32(i32t, 0);
    let c1i = b.constant_bit32(i32t, 1);
    let c2i = b.constant_bit32(i32t, 2);
    let c2f = b.constant_bit32(f32t, 2.0f32.to_bits());
    let c1f = b.constant_bit32(f32t, 1.0f32.to_bits());
    let chalf = b.constant_bit32(f32t, 0.5f32.to_bits());
    let c0f = b.constant_bit32(f32t, 0.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let idx = b.load(i32t, None, in_idx, None, vec![]).unwrap();
    let sh = b.shift_left_logical(i32t, None, idx, c1i).unwrap();
    let xb = b.bitwise_and(i32t, None, sh, c2i).unwrap();
    let yb = b.bitwise_and(i32t, None, idx, c2i).unwrap();
    let xf = b.convert_s_to_f(f32t, None, xb).unwrap();
    let yf = b.convert_s_to_f(f32t, None, yb).unwrap();
    let xm = b.f_mul(f32t, None, xf, c2f).unwrap();
    let x = b.f_sub(f32t, None, xm, c1f).unwrap();
    let ym = b.f_mul(f32t, None, yf, c2f).unwrap();
    let y = b.f_sub(f32t, None, ym, c1f).unwrap();
    let pos = b.composite_construct(v4, None, vec![x, y, c0f, c1f]).unwrap();
    let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c0i]).unwrap();
    b.store(dst, pos, None, vec![]).unwrap();
    let ux = b.f_mul(f32t, None, x, chalf).unwrap();
    let u = b.f_add(f32t, None, ux, chalf).unwrap();
    let vy = b.f_mul(f32t, None, y, chalf).unwrap();
    let v = b.f_add(f32t, None, vy, chalf).unwrap();
    let uv = b.composite_construct(v4, None, vec![u, v, c0f, c1f]).unwrap();
    b.store(uv_var, uv, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_idx, pv_var, uv_var]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// FS that passes a `Location=0` vec4 input varying straight to the output.
fn build_uv_passthrough_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in = b.type_pointer(None, StorageClass::Input, v4);
    let in_uv = b.variable(ptr_in, None, StorageClass::Input, None);
    b.decorate(in_uv, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let uv = b.load(v4, None, in_uv, None, vec![]).unwrap();
    b.store(out, uv, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![in_uv, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[test]
fn tier2_backend_vs_output_varying_roundtrip() {
    use aqueduct_gpu::frame::{DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;
    const N: u32 = 32;
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let vs_id = registry.register(&build_fullscreen_tri_uv_vs()).expect("vs");
    let fs_id = registry.register(&build_uv_passthrough_fs()).expect("fs");
    // The VS writes one vec4 varying (16 bytes), derived from the compiled
    // VS itself (authoritative — no client-supplied count).
    assert_eq!(registry.vs_varying_bytes(vs_id), Some(16),
        "compiled VS should report 16 varying bytes (one vec4)");
    let backend = Tier2Backend::new(registry);
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF251);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF252);
    backend.image_created(image_id, N, N);
    // bind_pipeline_vs derives + sets the varying stride; no manual call.
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd { x: 0.0, y: 0.0, width: N as f32, height: N as f32, min_depth: 0.0, max_depth: 1.0 }).unwrap();
    fb.push_draw(DrawCmd { vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0 }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    backend.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0xF254), 1, fb.as_bytes());
    let pixels = backend.read_image_pixels(image_id).unwrap();
    let at = |x: usize, y: usize| { let i = (y * N as usize + x) * 4; [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]] };
    let (tl, br) = (at(1, 1), at(30, 30));
    eprintln!("vs-varying TL={tl:?} BR={br:?}");
    // Interpolated screen UV: TL dark, BR bright (R≈u, G≈v).
    assert!(br[0] > 180 && br[1] > 180, "BR bright, got {br:?}");
    assert!(tl[0] < 60 && tl[1] < 60, "TL dark, got {tl:?}");
}

/// `gl_FragCoord` fragment shader: a gradient `(x/W, y/H, 0.5, 1)` read from
/// the FragCoord builtin (no vertex buffer, no varyings). Used to verify the
/// Tier-2 FragCoord fix (frontend routes the FragCoord-decorated load to
/// `Op::LoadBuiltin(FragCoord)`; the backend materialises it from the
/// per-pixel frag_coord params).
fn build_fragcoord_gradient_fs(w: u32, h: u32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_v4 = b.type_pointer(None, StorageClass::Input, v4);
    let fragcoord = b.variable(ptr_in_v4, None, StorageClass::Input, None);
    b.decorate(fragcoord, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::FragCoord)]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let inv_w = b.constant_bit32(f32t, (1.0f32 / w as f32).to_bits());
    let inv_h = b.constant_bit32(f32t, (1.0f32 / h as f32).to_bits());
    let chalf = b.constant_bit32(f32t, 0.5f32.to_bits());
    let cone = b.constant_bit32(f32t, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fc = b.load(v4, None, fragcoord, None, vec![]).unwrap();
    let fx = b.composite_extract(f32t, None, fc, vec![0]).unwrap();
    let fy = b.composite_extract(f32t, None, fc, vec![1]).unwrap();
    let u = b.f_mul(f32t, None, fx, inv_w).unwrap();
    let v = b.f_mul(f32t, None, fy, inv_h).unwrap();
    let color = b.composite_construct(v4, None, vec![u, v, chalf, cone]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![fragcoord, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// gl_FragCoord + GLSL.std.450: an orange diamond on a blue/green gradient,
/// the original scanout-demo shape. Exercises FragCoord *and* FAbs/FClamp/FMix
/// together (no vertex buffer, no varyings).
fn build_fragcoord_diamond_fs(w: u32, h: u32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    const FABS: u32 = 4;
    const FCLAMP: u32 = 43;
    const FMIX: u32 = 46;
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    let glsl = b.ext_inst_import("GLSL.std.450");
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let v3 = b.type_vector(f32t, 3);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_v4 = b.type_pointer(None, StorageClass::Input, v4);
    let fragcoord = b.variable(ptr_in_v4, None, StorageClass::Input, None);
    b.decorate(fragcoord, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::FragCoord)]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let k = |b: &mut rspirv::dr::Builder, x: f32| b.constant_bit32(f32t, x.to_bits());
    let inv_w = k(&mut b, 1.0 / w as f32);
    let inv_h = k(&mut b, 1.0 / h as f32);
    let half = k(&mut b, 0.5);
    let edge = k(&mut b, 0.30);
    let sharp = k(&mut b, 12.0);
    let zero = k(&mut b, 0.0);
    let one = k(&mut b, 1.0);
    let bg_rx = k(&mut b, 0.35);
    let bg_gy = k(&mut b, 0.55);
    let bg_b = k(&mut b, 0.75);
    let fg_r = k(&mut b, 1.0);
    let fg_g = k(&mut b, 0.55);
    let fg_b = k(&mut b, 0.10);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fc = b.load(v4, None, fragcoord, None, vec![]).unwrap();
    let fx = b.composite_extract(f32t, None, fc, vec![0]).unwrap();
    let fy = b.composite_extract(f32t, None, fc, vec![1]).unwrap();
    let u = b.f_mul(f32t, None, fx, inv_w).unwrap();
    let v = b.f_mul(f32t, None, fy, inv_h).unwrap();
    let du = b.f_sub(f32t, None, u, half).unwrap();
    let dv = b.f_sub(f32t, None, v, half).unwrap();
    let adx = b.ext_inst(f32t, None, glsl, FABS, vec![Operand::IdRef(du)]).unwrap();
    let ady = b.ext_inst(f32t, None, glsl, FABS, vec![Operand::IdRef(dv)]).unwrap();
    let diamond = b.f_add(f32t, None, adx, ady).unwrap();
    let em = b.f_sub(f32t, None, edge, diamond).unwrap();
    let scaled = b.f_mul(f32t, None, em, sharp).unwrap();
    let t = b.ext_inst(f32t, None, glsl, FCLAMP,
        vec![Operand::IdRef(scaled), Operand::IdRef(zero), Operand::IdRef(one)]).unwrap();
    let br = b.f_mul(f32t, None, u, bg_rx).unwrap();
    let bgc = b.f_mul(f32t, None, v, bg_gy).unwrap();
    let bg = b.composite_construct(v3, None, vec![br, bgc, bg_b]).unwrap();
    let fg = b.composite_construct(v3, None, vec![fg_r, fg_g, fg_b]).unwrap();
    let tv = b.composite_construct(v3, None, vec![t, t, t]).unwrap();
    let rgb = b.ext_inst(v3, None, glsl, FMIX,
        vec![Operand::IdRef(bg), Operand::IdRef(fg), Operand::IdRef(tv)]).unwrap();
    let rr = b.composite_extract(f32t, None, rgb, vec![0]).unwrap();
    let gg = b.composite_extract(f32t, None, rgb, vec![1]).unwrap();
    let bb = b.composite_extract(f32t, None, rgb, vec![2]).unwrap();
    let color = b.composite_construct(v4, None, vec![rr, gg, bb, one]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![fragcoord, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Render the FragCoord diamond through both the per-pixel and the SoA span
/// path; both must agree (orange centre, blue corner).
fn render_fragcoord_diamond(span: bool) -> [[u8; 4]; 2] {
    use aqueduct_gpu::frame::{DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;
    const N: u32 = 32;
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let vs_id = registry.register(&build_fullscreen_tri_vs()).expect("vs");
    let fs_id = registry.register(&build_fragcoord_diamond_fs(N, N)).expect("diamond fs");
    let backend = Tier2Backend::new(registry);
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF241);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF242);
    backend.image_created(image_id, N, N);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd { x: 0.0, y: 0.0, width: N as f32, height: N as f32, min_depth: 0.0, max_depth: 1.0 }).unwrap();
    fb.push_draw(DrawCmd { vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0 }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    let _ = span; // path is chosen by the ATRIUM_TIER2_SPAN env set by the caller
    backend.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0xF244), 1, fb.as_bytes());
    let pixels = backend.read_image_pixels(image_id).unwrap();
    let at = |x: usize, y: usize| { let i = (y * N as usize + x) * 4; [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]] };
    [at(16, 16), at(1, 1)]
}

#[test]
fn tier2_backend_gl_fragcoord_diamond() {
    let [centre, corner] = render_fragcoord_diamond(false);
    eprintln!("diamond centre={centre:?} corner={corner:?}");
    // Centre inside the diamond → orange (R high, G mid, B low).
    assert!(centre[0] > 200 && centre[1] > 110 && centre[1] < 180 && centre[2] < 70,
        "centre should be the orange diamond, got {centre:?}");
    // Corner outside → blue gradient background (B high, R low).
    assert!(corner[2] > 150 && corner[0] < 80,
        "corner should be the blue gradient, got {corner:?}");
}

#[test]
fn tier2_backend_gl_fragcoord_gradient() {
    use aqueduct_gpu::frame::{DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;

    const N: u32 = 32;
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let vs_id = registry.register(&build_fullscreen_tri_vs()).expect("vs");
    let fs_id = registry.register(&build_fragcoord_gradient_fs(N, N)).expect("fragcoord fs");
    let backend = Tier2Backend::new(registry);
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF231);
    let pipeline_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF232);
    backend.image_created(image_id, N, N);
    backend.bind_pipeline_vs(pipeline_id, vs_id);
    backend.bind_pipeline(pipeline_id, fs_id);
    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pipeline_id.raw().to_le_bytes()).unwrap();
    fb.push_set_viewport(SetViewportCmd { x: 0.0, y: 0.0, width: N as f32, height: N as f32, min_depth: 0.0, max_depth: 1.0 }).unwrap();
    fb.push_draw(DrawCmd { vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0 }).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    backend.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0xF234), 1, fb.as_bytes());
    let pixels = backend.read_image_pixels(image_id).unwrap();
    let at = |x: usize, y: usize| { let i = (y * N as usize + x) * 4; [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]] };
    // gl_FragCoord.xy in pixels → u=x/N, v=y/N. R grows with x, G with y.
    let tl = at(1, 1);
    let br = at(30, 30);
    eprintln!("fragcoord gradient TL={tl:?} BR={br:?}");
    assert!(br[0] > 180 && br[1] > 180, "bottom-right bright (u,v→1), got {br:?}");
    assert!(tl[0] < 60 && tl[1] < 60, "top-left dark (u,v→0), got {tl:?}");
    // Red tracks x, green tracks y: a horizontal-only sample is red-dominant.
    let right_mid = at(30, 2);
    assert!(right_mid[0] > 180 && right_mid[1] < 60,
        "right-but-top is red-dominant (high u, low v), got {right_mid:?}");
}

#[test]
fn tier2_backend_scanout_nested_squares_via_scissor() {
    use aqueduct_gpu::frame::{DrawCmd, FrameBuilder, SetViewportCmd};
    use aqueduct_gpu::opcodes::FrameOp;

    const N: u32 = 32;
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    // Proven primitives only: the vertex-less full-screen triangle + a
    // constant-colour FS (the green-triangle path), one pipeline per colour,
    // each draw clipped to a scissor rect → nested coloured squares. No
    // varyings / FragCoord / ext-inst (all of which crash the compiled Tier-2
    // path here — see scratch notes). This is what the scanout demo draws.
    let vs_id = registry.register(&build_fullscreen_tri_vs()).expect("vs");
    let blue = registry.register(&build_constant_color_spirv([0.10, 0.20, 0.90, 1.0])).unwrap();
    let orange = registry.register(&build_constant_color_spirv([1.0, 0.55, 0.10, 1.0])).unwrap();
    let white = registry.register(&build_constant_color_spirv([0.95, 0.95, 0.95, 1.0])).unwrap();
    let backend = Tier2Backend::new(registry);

    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0xF201);
    backend.image_created(image_id, N, N);
    // One pipeline per colour (shared VS).
    let squares = [
        (ResourceId::new(IdNamespace::IcdRuntime, 0xF210), blue,   (4u32, 4u32, 24u32, 24u32)),
        (ResourceId::new(IdNamespace::IcdRuntime, 0xF211), orange, (10, 10, 12, 12)),
        (ResourceId::new(IdNamespace::IcdRuntime, 0xF212), white,  (14, 14, 4, 4)),
    ];
    for (pid, fs, _) in &squares {
        backend.bind_pipeline_vs(*pid, vs_id);
        backend.bind_pipeline(*pid, *fs);
    }

    let mut fb = FrameBuilder::new(4096);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&image_id.raw().to_le_bytes());
    // Clear to dark.
    begin[4..8].copy_from_slice(&[10u8, 10, 10, 255]);
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: N as f32, height: N as f32, min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    for (pid, _, (x, y, w, h)) in &squares {
        fb.push(FrameOp::BindPipeline, &pid.raw().to_le_bytes()).unwrap();
        fb.push(FrameOp::SetScissor,
            &aqueduct_gpu::frame::SetScissorCmd { x: *x, y: *y, width: *w, height: *h }.to_bytes()).unwrap();
        fb.push_draw(DrawCmd { vertex_count: 3, instance_count: 1, first_vertex: 0, first_instance: 0 }).unwrap();
    }
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    backend.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0xF204), 1, fb.as_bytes());

    let pixels = backend.read_image_pixels(image_id).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * N as usize + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    // Centre = innermost white square.
    let c = px(16, 16);
    assert!(c[0] > 200 && c[1] > 200 && c[2] > 200, "centre should be white, got {c:?}");
    // Inside the orange ring (between mid and inner squares).
    let o = px(11, 11);
    assert!(o[0] > 200 && o[1] > 110 && o[1] < 180 && o[2] < 70,
        "should be orange, got {o:?}");
    // Inside the blue ring.
    let bl = px(6, 6);
    assert!(bl[2] > 200 && bl[0] < 90, "should be blue, got {bl:?}");
    // Corner outside all squares = the dark clear.
    let bg = px(1, 1);
    assert!(bg[0] < 40 && bg[1] < 40 && bg[2] < 40, "corner should be dark clear, got {bg:?}");
}
