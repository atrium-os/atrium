//! End-to-end wire test: client talks to a stub host over a
//! `UnixStream` pair. Exercises handshake, resource creation, frame
//! submit + fence wait, and event reception — without a real GPU.
//!
//! The stub host is the minimum protocol-correct responder: it
//! reads request envelopes, replies with hardcoded responses, and
//! injects async events on cue. Failures here usually mean the
//! envelope flag handling, postcard schema, or class-routing logic
//! is wrong.

use std::os::unix::net::UnixStream;
use std::thread;

use aqueduct::envelope::flag;
use aqueduct::Connection;
use aqueduct_gpu::{backends::{BackendId, GpuVendor}, ids::{IdNamespace, ResourceId},
                    opcodes::*, payloads::*, CLASS_GPU};
use aqueduct_gpu_client::{GpuClient, GpuEvent, PROTOCOL_VERSION};

/// Spin up a paired connection: client + a "host" Connection the
/// test thread can drive directly.
fn paired() -> (GpuClient, Connection) {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let client_conn = Connection::wrap(a).expect("wrap client");
    let host_conn   = Connection::wrap(b).expect("wrap host");
    (GpuClient::new(client_conn), host_conn)
}

/// Encode a payload, send as a response on the host side.
fn host_reply<T: serde::Serialize>(
    host: &mut Connection,
    op: u16,
    payload: &T,
) {
    let bytes = postcard::to_stdvec(payload).expect("encode");
    host.send_message(CLASS_GPU, op, flag::IS_RESPONSE, &bytes)
        .expect("host send reply");
}

fn host_event<T: serde::Serialize>(
    host: &mut Connection,
    op: u16,
    payload: &T,
) {
    let bytes = postcard::to_stdvec(payload).expect("encode");
    host.send_message(CLASS_GPU, op, flag::ASYNC_EVENT, &bytes)
        .expect("host send event");
}

#[test]
fn handshake_negotiates_protocol_version() {
    let (mut client, mut host) = paired();

    let host_thread = thread::spawn(move || {
        let req = host.recv_message().expect("host recv handshake");
        assert_eq!(req.opcode_class, CLASS_GPU);
        assert_eq!(req.op, OP_GPU_HANDSHAKE);
        let p: HandshakePayload = postcard::from_bytes(&req.payload).unwrap();
        assert_eq!(p.protocol_version, PROTOCOL_VERSION);
        assert_eq!(p.client_kind, ClientKind::FrescodRenderer);

        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            backend: BackendId::new(GpuVendor::Apple, 4),
            caps: HandshakeResponse::CAPS_COMPUTE
                | HandshakeResponse::CAPS_SHARE_SURFACE,
            max_frame_bytes: 1 << 20,
            max_fences_inflight: 64,
        });
        host
    });

    let resp = client.handshake(ClientKind::FrescodRenderer).unwrap().clone();
    assert_eq!(resp.protocol_version, PROTOCOL_VERSION);
    assert_eq!(resp.backend.vendor, GpuVendor::Apple);
    assert_eq!(resp.max_frame_bytes, 1 << 20);

    host_thread.join().unwrap();
}

#[test]
fn allocate_memory_returns_host_token() {
    let (mut client, mut host) = paired();

    let host_thread = thread::spawn(move || {
        // Handshake first.
        let _ = host.recv_message().unwrap();
        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            backend: BackendId::new(GpuVendor::AtriumGpu, 1),
            caps: 0,
            max_frame_bytes: 4096,
            max_fences_inflight: 16,
        });

        // Memory create.
        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_MEMORY_CREATE);
        let p: MemoryCreatePayload = postcard::from_bytes(&req.payload).unwrap();
        assert_eq!(p.size, 64 * 1024);
        assert_eq!(p.usage, MemoryUsage::BufferBacking);
        // ID should be in the IcdRuntime namespace (top 4 bits = 0xF).
        assert_eq!(p.region_id.namespace(), Some(IdNamespace::IcdRuntime));

        host_reply(&mut host, OP_GPU_MEMORY_CREATE, &MemoryCreateResponse {
            region_id: p.region_id,
            size: 64 * 1024,
            host_va_hint: 0xDEAD_BEEF_0000,
            atrium_gpu_token: [0xAB; 32],
        });
        host
    });

    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let resp = client.allocate_memory(64 * 1024, MemoryUsage::BufferBacking).unwrap();
    assert_eq!(resp.size, 64 * 1024);
    assert_eq!(resp.atrium_gpu_token, [0xAB; 32]);

    host_thread.join().unwrap();
}

#[test]
fn fire_and_forget_image_create_does_not_block() {
    let (mut client, mut host) = paired();

    let host_thread = thread::spawn(move || {
        let _ = host.recv_message().unwrap();
        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            backend: BackendId::new(GpuVendor::AtriumGpu, 1),
            caps: 0, max_frame_bytes: 4096, max_fences_inflight: 16,
        });

        // We should receive an image-create with NO response expected.
        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_IMAGE_CREATE);
        assert_eq!(req.flags & flag::RESPONSE_EXPECTED, 0,
            "fire-and-forget ops must not set RESPONSE_EXPECTED");
        host
    });

    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), // overwritten by alloc_id
        backing_region: ResourceId::new(IdNamespace::IcdRuntime, 0x1),
        region_offset: 0,
        format: 37, width: 1280, height: 720, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    assert_eq!(id.namespace(), Some(IdNamespace::IcdRuntime));

    host_thread.join().unwrap();
}

#[test]
fn submit_frame_then_wait_fence() {
    let (mut client, mut host) = paired();

    let host_thread = thread::spawn(move || {
        let _ = host.recv_message().unwrap();
        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            backend: BackendId::new(GpuVendor::AtriumGpu, 1),
            caps: 0, max_frame_bytes: 1 << 16, max_fences_inflight: 16,
        });

        // Fence create (fire-and-forget).
        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_FENCE_CREATE);

        // Submit frame (fire-and-forget).
        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_SUBMIT_FRAME);
        let p: SubmitFramePayload = postcard::from_bytes(&req.payload).unwrap();
        assert_eq!(p.timeline, 42);
        assert!(!p.command_buf.is_empty());

        // Wait fence (request-response).
        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_WAIT_FENCE);
        let p: WaitFencePayload = postcard::from_bytes(&req.payload).unwrap();
        host_reply(&mut host, OP_GPU_WAIT_FENCE, &WaitFenceResponse {
            fence_id: p.fence_id,
            signalled: true,
        });
        host
    });

    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fb.push(FrameOp::BeginRenderPass, &[0; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    client.submit_frame(fence, fb, 42).unwrap();
    let signalled = client.wait_fence(fence, 1_000_000_000).unwrap();
    assert!(signalled);

    host_thread.join().unwrap();
}

#[test]
fn shader_resolve_miss_propagates_as_error() {
    let (mut client, mut host) = paired();

    let host_thread = thread::spawn(move || {
        let _ = host.recv_message().unwrap();
        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            backend: BackendId::new(GpuVendor::AtriumGpu, 1),
            caps: 0, max_frame_bytes: 4096, max_fences_inflight: 16,
        });

        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_SHADER_RESOLVE);
        let p: ShaderResolvePayload = postcard::from_bytes(&req.payload).unwrap();
        host_reply(&mut host, OP_GPU_SHADER_RESOLVE, &ShaderResolveResponse {
            bytecode_hash: p.bytecode_hash,
            status: ShaderResolveStatus::Miss,
            shader_id: None,
        });
        host
    });

    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let hash = [0xCC; 32];
    let backend = BackendId::new(GpuVendor::AtriumGpu, 1);
    let err = client.resolve_shader(hash, ShaderKind::SpirV, backend).unwrap_err();
    match err {
        aqueduct_gpu_client::GpuClientError::ShaderResolveMissed { hash: h } => {
            assert_eq!(h, hash);
        }
        other => panic!("expected ShaderResolveMissed, got {other:?}"),
    }

    host_thread.join().unwrap();
}

#[test]
fn async_event_arrives_during_request_then_via_recv_event() {
    let (mut client, mut host) = paired();

    let fence_id = ResourceId::new(IdNamespace::IcdRuntime, 0x300);
    let host_thread = thread::spawn(move || {
        let _ = host.recv_message().unwrap();
        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            backend: BackendId::new(GpuVendor::AtriumGpu, 1),
            caps: 0, max_frame_bytes: 4096, max_fences_inflight: 16,
        });

        // Receive memory-create request. BEFORE replying, fire an
        // async fence-signaled event — the client should queue it
        // and not confuse it with the reply.
        let req = host.recv_message().unwrap();
        assert_eq!(req.op, OP_GPU_MEMORY_CREATE);
        let p: MemoryCreatePayload = postcard::from_bytes(&req.payload).unwrap();

        host_event(&mut host, OP_GPU_FENCE_SIGNALED, &FenceSignaledEvent {
            fence_id,
            timeline: 99,
        });

        host_reply(&mut host, OP_GPU_MEMORY_CREATE, &MemoryCreateResponse {
            region_id: p.region_id,
            size: 4096,
            host_va_hint: 0,
            atrium_gpu_token: [0; 32],
        });
        host
    });

    client.handshake(ClientKind::FrescodRenderer).unwrap();
    let _resp = client.allocate_memory(4096, MemoryUsage::Staging).unwrap();

    // The fence-signaled event the host fired before the reply
    // should be in our pending queue.
    let ev = client.recv_event(Some(std::time::Duration::from_secs(1))).unwrap();
    match ev {
        Some(GpuEvent::FenceSignaled(fs)) => {
            assert_eq!(fs.fence_id, fence_id);
            assert_eq!(fs.timeline, 99);
        }
        other => panic!("expected queued FenceSignaled, got {other:?}"),
    }

    host_thread.join().unwrap();
}

#[test]
fn protocol_mismatch_surfaces_error() {
    let (mut client, mut host) = paired();

    let host_thread = thread::spawn(move || {
        let _ = host.recv_message().unwrap();
        // Reply with a different protocol version.
        host_reply(&mut host, OP_GPU_HANDSHAKE, &HandshakeResponse {
            protocol_version: 999,
            backend: BackendId::new(GpuVendor::Software, 0),
            caps: 0, max_frame_bytes: 0, max_fences_inflight: 0,
        });
        host
    });

    let err = client.handshake(ClientKind::FrescodRenderer).unwrap_err();
    match err {
        aqueduct_gpu_client::GpuClientError::ProtocolMismatch { client: c, host: h } => {
            assert_eq!(c, PROTOCOL_VERSION);
            assert_eq!(h, 999);
        }
        other => panic!("expected ProtocolMismatch, got {other:?}"),
    }

    host_thread.join().unwrap();
}
