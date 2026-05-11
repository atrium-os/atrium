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
