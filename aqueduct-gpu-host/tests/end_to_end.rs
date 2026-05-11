//! End-to-end tests: real listener thread + real
//! aqueduct-gpu-client client. Goes over a real Unix socket; no
//! mocks.
//!
//! Note this crate depends on `aqueduct-gpu-client` as a dev-only
//! dependency so the host integration tests can drive it as a real
//! client. (Production usage flows the other direction: clients
//! depend on the protocol crate; the host doesn't depend on the
//! client.)

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct::Connection;
use aqueduct_gpu::{
    backends::GpuVendor,
    payloads::{ClientKind, MemoryUsage, ShaderKind},
};
use aqueduct_gpu_client::GpuClient;
use aqueduct_gpu_host::{Listener, StubBackend};

/// Create a unique socket path under a tempdir so parallel tests
/// don't collide.
fn tmp_socket(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("aqueduct-gpu-test-{}-{}.sock",
                   std::process::id(), name));
    p
}

#[test]
fn handshake_against_stub_backend() {
    let sock = tmp_socket("handshake");
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&sock, backend).unwrap();

    let server_thread = thread::spawn(move || {
        // accept_loop blocks; let the first connection drive a
        // handshake then drop everything.
        let _ = listener.accept_loop();
    });

    // Give the listener a moment to actually bind.
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    let resp = client.handshake(ClientKind::FrescodRenderer).unwrap().clone();
    assert_eq!(resp.backend.vendor, GpuVendor::Software);
    assert_eq!(resp.max_frame_bytes, 1 << 20);

    drop(client);
    // server_thread will exit when the listener's socket is removed
    // on Drop or the process ends; for tests we leak the thread.
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn memory_create_against_stub_backend() {
    let sock = tmp_socket("memory");
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&sock, backend).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let resp = client.allocate_memory(64 * 1024, MemoryUsage::BufferBacking).unwrap();
    assert_eq!(resp.size, 64 * 1024);
    assert_eq!(resp.atrium_gpu_token[31], 0xAB);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn submit_frame_signals_fence() {
    use aqueduct_gpu::opcodes::FrameOp;

    let sock = tmp_socket("frame");
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&sock, backend.clone()).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fb.push(FrameOp::BeginRenderPass, &[0; 16]).unwrap();
    fb.push(FrameOp::Draw, &[1, 2, 3, 4]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    client.submit_frame(fence, fb, 1).unwrap();

    // Stub backend signals fences immediately, so we should see
    // either the async event or a positive wait_fence response.
    let signalled = client.wait_fence(fence, 1_000_000_000).unwrap();
    assert!(signalled);

    assert_eq!(backend.submission_count(), 1);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn pre_handshake_op_rejected() {
    let sock = tmp_socket("prehs");
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&sock, backend).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    // Bypass the GpuClient handshake helper by talking directly.
    let mut conn = Connection::connect(&sock).unwrap();
    use aqueduct::envelope::flag;
    use aqueduct_gpu::{CLASS_GPU, opcodes::OP_GPU_MEMORY_CREATE};
    use aqueduct_gpu::payloads::MemoryCreatePayload;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};

    let req = MemoryCreatePayload {
        region_id: ResourceId::new(IdNamespace::IcdRuntime, 1),
        size: 1024,
        usage: MemoryUsage::Staging,
    };
    let payload = postcard::to_stdvec(&req).unwrap();
    conn.send_message(CLASS_GPU, OP_GPU_MEMORY_CREATE,
                       flag::RESPONSE_EXPECTED, &payload).unwrap();

    // The server should refuse with a validation error event, not
    // process the memory create.
    let m = conn.recv_message().unwrap();
    assert_eq!(m.opcode_class, CLASS_GPU);
    assert_eq!(m.op, aqueduct_gpu::opcodes::OP_GPU_VALIDATION_ERR);

    drop(conn);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn software_backend_renders_rect_end_to_end() {
    // Full client→listener→SoftwareBackend→tiny-skia path:
    // - client connects, handshake
    // - client creates an image (SoftwareBackend allocates Pixmap)
    // - client builds a frame with one rect, submits it
    // - SoftwareBackend dispatches via TinySkiaRenderer
    // - read back the rendered pixels from the backend
    //
    // This is the demonstration artifact for Phase 1.3c — proof
    // that aqueduct-gpu produces real pixels through tier-1 SW.
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::payloads::ImageCreatePayload;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};
    use aqueduct_gpu_host::software::{
        BeginRenderPassBody, RectOpParams, BUILTIN_PIPELINE_RECT,
    };

    let sock = tmp_socket("sw_render");
    let backend = Arc::new(StubBackend::new()); // placeholder for the path below

    // We want direct access to the backend's read_image_pixels after
    // dispatch — but the listener takes Arc<dyn Backend>. Use a
    // SoftwareBackend instead and keep a separate Arc handle for
    // the test to read back pixels.
    drop(backend);
    let sw_backend = Arc::new(aqueduct_gpu_host::SoftwareBackend::new());
    let backend_for_listener: Arc<dyn aqueduct_gpu_host::Backend> = sw_backend.clone();

    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    // Create a backing memory region and an image of 64x64.
    let mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();

    // SoftwareBackend pre-assigns image IDs in the client; we need
    // to know the ID the client picks so we can read pixels back.
    // Construct the image and capture its assigned ID.
    let image_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), // overwritten by alloc_id
        backing_region: mem.region_id,
        region_offset: 0,
        format: 37, // VK_FORMAT_R8G8B8A8_UNORM
        width: 64,
        height: 64,
        depth: 1,
        mip_levels: 1,
        array_layers: 1,
        usage: 0x07,
    }).unwrap();

    // Give the session thread a moment to dispatch image_created
    // into the backend (it's a fire-and-forget op).
    thread::sleep(Duration::from_millis(20));

    // Build a frame that fills the image with cyan via the rect pipeline.
    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fb.push(FrameOp::BeginRenderPass, &BeginRenderPassBody {
        target_image_id: image_id.raw(),
        clear_color_rgba8: [0, 0, 0, 255],
    }.to_bytes()).unwrap();

    let rect_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_RECT);
    fb.push(FrameOp::BindPipeline, &rect_pipe.raw().to_le_bytes()).unwrap();

    let params = RectOpParams {
        x: 0.0, y: 0.0, w: 64.0, h: 64.0,
        r: 0.0, g: 1.0, b: 1.0, a: 1.0, // cyan
    };
    let mut pc_body = vec![0u8; 4]; // stage_mask + offset + reserved
    pc_body.extend_from_slice(&params.to_bytes());
    fb.push(FrameOp::PushConstants, &pc_body).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000).unwrap();

    // Give the session another beat to finish dispatch.
    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(image_id)
        .expect("SoftwareBackend should have pixels for the image");
    assert_eq!(pixels.len(), 64 * 64 * 4);

    // tiny-skia stores RGBA premultiplied. Cyan at full alpha
    // round-trips to (0, 255, 255, 255). Check the centre pixel.
    let centre = ((32 * 64) + 32) * 4;
    assert_eq!(pixels[centre + 0],   0, "R at centre");
    assert_eq!(pixels[centre + 1], 255, "G at centre");
    assert_eq!(pixels[centre + 2], 255, "B at centre");
    assert_eq!(pixels[centre + 3], 255, "A at centre");

    // Backend telemetry should show one submission and zero failures.
    assert_eq!(sw_backend.submission_count(), 1);
    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn shader_resolve_returns_miss_then_upload_succeeds() {
    use aqueduct_gpu::backends::BackendId;

    let sock = tmp_socket("shader");
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&sock, backend).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let hash = [0x77; 32];
    let backend_id = BackendId::new(GpuVendor::Software, 0);

    // First: resolve misses (stub always misses).
    let err = client.resolve_shader(hash, ShaderKind::SpirV, backend_id).unwrap_err();
    matches!(err, aqueduct_gpu_client::GpuClientError::ShaderResolveMissed { .. });

    // Then: upload succeeds (stub accepts everything).
    let shader_id = client.upload_shader(
        hash, ShaderKind::SpirV, backend_id, vec![0xDE, 0xAD, 0xBE, 0xEF],
    ).unwrap();
    assert!(shader_id.local_id() > 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
