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
fn software_backend_renders_textured_rect_end_to_end() {
    // Full path for the textured-rect built-in pipeline:
    // - create a 4x4 atlas image, upload red pixels via write_image
    // - create a 64x64 target image
    // - draw the atlas, scaled, into the target via the textured-rect pipeline
    // - read back target pixels and verify they're red
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::payloads::ImageCreatePayload;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};
    use aqueduct_gpu_host::software::{
        BeginRenderPassBody, TexturedRectOpParams, BUILTIN_PIPELINE_TEXTURED_RECT,
    };

    let sock = tmp_socket("sw_textured");
    let sw_backend = Arc::new(aqueduct_gpu_host::SoftwareBackend::new());
    let backend_for_listener: Arc<dyn aqueduct_gpu_host::Backend> = sw_backend.clone();

    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    // Memory for atlas + target.
    let atlas_mem = client.allocate_memory(4 * 4 * 4, MemoryUsage::ImageBacking).unwrap();
    let target_mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();

    // Atlas (4×4 red).
    let atlas_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: atlas_mem.region_id,
        region_offset: 0,
        format: 37, width: 4, height: 4, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();

    // Target.
    let target_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: target_mem.region_id,
        region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();

    // Let session register both images in the backend.
    thread::sleep(Duration::from_millis(30));

    // Upload 4x4 red atlas (RGBA8 premultiplied: (255, 0, 0, 255)).
    let mut atlas_pixels = vec![0u8; 4 * 4 * 4];
    for px in atlas_pixels.chunks_exact_mut(4) {
        px[0] = 255; // R
        px[1] = 0;   // G
        px[2] = 0;   // B
        px[3] = 255; // A
    }
    client.write_image(atlas_id, 4 * 4, atlas_pixels).unwrap();

    // Build a frame: clear black, textured-rect from atlas to target.
    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fb.push(FrameOp::BeginRenderPass, &BeginRenderPassBody {
        target_image_id: target_id.raw(),
        clear_color_rgba8: [0, 0, 0, 255],
    }.to_bytes()).unwrap();

    let pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_TEXTURED_RECT);
    fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();

    let params = TexturedRectOpParams {
        dst_x: 0.0, dst_y: 0.0, dst_w: 64.0, dst_h: 64.0,
        atlas_image_id: atlas_id.raw(),
        src_u0: 0.0, src_v0: 0.0, src_u1: 4.0, src_v1: 4.0,
        tint_r: 1.0, tint_g: 1.0, tint_b: 1.0, tint_a: 1.0,
    };
    let mut pc_body = vec![0u8; 4]; // stage_mask+offset+reserved
    pc_body.extend_from_slice(&params.to_bytes());
    fb.push(FrameOp::PushConstants, &pc_body).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000).unwrap();

    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target_id)
        .expect("target pixels");
    assert_eq!(pixels.len(), 64 * 64 * 4);
    let centre = ((32 * 64) + 32) * 4;
    assert_eq!(pixels[centre + 0], 255, "R at centre");
    assert_eq!(pixels[centre + 1],   0, "G at centre");
    assert_eq!(pixels[centre + 2],   0, "B at centre");
    assert_eq!(pixels[centre + 3], 255, "A at centre");

    assert_eq!(sw_backend.submission_count(), 1);
    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn write_image_region_patches_subrect_only() {
    // Upload a fully-red 16×16 image, then patch a 4×4 green sub-rect
    // at offset (6, 6). Render the patched image via textured-rect
    // pipeline and verify (a) centre pixel is green (from the patch)
    // and (b) corner pixels are still red.
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::payloads::ImageCreatePayload;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};
    use aqueduct_gpu_host::software::{
        BeginRenderPassBody, TexturedRectOpParams,
        BUILTIN_PIPELINE_TEXTURED_RECT,
    };

    let sock = tmp_socket("write_region");
    let sw_backend = Arc::new(aqueduct_gpu_host::SoftwareBackend::new());
    let backend_for_listener: Arc<dyn aqueduct_gpu_host::Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let atlas_mem  = client.allocate_memory(16*16*4, MemoryUsage::ImageBacking).unwrap();
    let target_mem = client.allocate_memory(64*64*4, MemoryUsage::ImageBacking).unwrap();
    let atlas = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), backing_region: atlas_mem.region_id, region_offset: 0,
        format: 37, width: 16, height: 16, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    let target = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), backing_region: target_mem.region_id, region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    thread::sleep(Duration::from_millis(30));

    // Initial: fill atlas red.
    let mut red = vec![0u8; 16*16*4];
    for px in red.chunks_exact_mut(4) { px[0]=255; px[3]=255; }
    client.write_image(atlas, 16*4, red).unwrap();

    // Patch a 4×4 green sub-rect at (6, 6).
    let mut green = vec![0u8; 4*4*4];
    for px in green.chunks_exact_mut(4) { px[1]=255; px[3]=255; }
    client.write_image_region(atlas, 6, 6, 4, 4, 4*4, green).unwrap();

    // Render atlas scaled into target.
    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fb.push(FrameOp::BeginRenderPass, &BeginRenderPassBody {
        target_image_id: target.raw(),
        clear_color_rgba8: [0, 0, 0, 255],
    }.to_bytes()).unwrap();
    let pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_TEXTURED_RECT);
    fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
    let params = TexturedRectOpParams {
        dst_x: 0.0, dst_y: 0.0, dst_w: 64.0, dst_h: 64.0,
        atlas_image_id: atlas.raw(),
        src_u0: 0.0, src_v0: 0.0, src_u1: 16.0, src_v1: 16.0,
        tint_r: 1.0, tint_g: 1.0, tint_b: 1.0, tint_a: 1.0,
    };
    let mut pc = vec![0u8; 4];
    pc.extend_from_slice(&params.to_bytes());
    fb.push(FrameOp::PushConstants, &pc).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000).unwrap();
    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target).expect("pixels");
    // Atlas (8, 8) is inside the green patch (6..10). With src→dst
    // scale 4×, target pixel (32, 32) samples atlas (8, 8) ⇒ green.
    let centre = (32 * 64 + 32) * 4;
    assert_eq!(pixels[centre + 0],   0, "centre R should be from green patch");
    assert_eq!(pixels[centre + 1], 255, "centre G");
    // Atlas (0, 0) is OUTSIDE the patch; target (0, 0) ⇒ red.
    let corner = (0 * 64 + 0) * 4;
    assert_eq!(pixels[corner + 0], 255, "corner R should still be red");
    assert_eq!(pixels[corner + 1],   0, "corner G");

    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn software_backend_handles_multi_renderpass_frame() {
    // Two renderpasses in one frame:
    //   pass 1: render a solid red 8×8 image (acts as a generated atlas)
    //   pass 2: textured-rect from pass 1's image into a 64×64 target
    // Verifies the backend partitions renderpasses correctly, that
    // pass 1's output is observable as a source image in pass 2, and
    // that both target Pixmaps get re-inserted between passes.
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::payloads::ImageCreatePayload;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};
    use aqueduct_gpu_host::software::{
        BeginRenderPassBody, RectOpParams, TexturedRectOpParams,
        BUILTIN_PIPELINE_RECT, BUILTIN_PIPELINE_TEXTURED_RECT,
    };

    let sock = tmp_socket("sw_multipass");
    let sw_backend = Arc::new(aqueduct_gpu_host::SoftwareBackend::new());
    let backend_for_listener: Arc<dyn aqueduct_gpu_host::Backend> = sw_backend.clone();

    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    // Pass-1 target: 8×8 atlas.
    let atlas_mem = client.allocate_memory(8 * 8 * 4, MemoryUsage::ImageBacking).unwrap();
    let atlas_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: atlas_mem.region_id, region_offset: 0,
        format: 37, width: 8, height: 8, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    // Pass-2 target: 64×64 screen.
    let screen_mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();
    let screen_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: screen_mem.region_id, region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    thread::sleep(Duration::from_millis(30));

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();

    // ── Pass 1: fill atlas with red via the rect pipeline ────────
    fb.push(FrameOp::BeginRenderPass, &BeginRenderPassBody {
        target_image_id: atlas_id.raw(),
        clear_color_rgba8: [0, 0, 0, 255],
    }.to_bytes()).unwrap();
    let rect_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_RECT);
    fb.push(FrameOp::BindPipeline, &rect_pipe.raw().to_le_bytes()).unwrap();
    let mut pc = vec![0u8; 4];
    pc.extend_from_slice(&RectOpParams {
        x: 0.0, y: 0.0, w: 8.0, h: 8.0,
        r: 1.0, g: 0.0, b: 0.0, a: 1.0,
    }.to_bytes());
    fb.push(FrameOp::PushConstants, &pc).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    // ── Pass 2: sample atlas into screen via textured-rect ──────
    fb.push(FrameOp::BeginRenderPass, &BeginRenderPassBody {
        target_image_id: screen_id.raw(),
        clear_color_rgba8: [0, 0, 0, 255],
    }.to_bytes()).unwrap();
    let tex_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_TEXTURED_RECT);
    fb.push(FrameOp::BindPipeline, &tex_pipe.raw().to_le_bytes()).unwrap();
    let mut pc = vec![0u8; 4];
    pc.extend_from_slice(&TexturedRectOpParams {
        dst_x: 16.0, dst_y: 16.0, dst_w: 32.0, dst_h: 32.0,
        atlas_image_id: atlas_id.raw(),
        src_u0: 0.0, src_v0: 0.0, src_u1: 8.0, src_v1: 8.0,
        tint_r: 1.0, tint_g: 1.0, tint_b: 1.0, tint_a: 1.0,
    }.to_bytes());
    fb.push(FrameOp::PushConstants, &pc).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000).unwrap();
    thread::sleep(Duration::from_millis(50));

    // Verify pass 1's output: the atlas should be fully red.
    let atlas_pixels = sw_backend.read_image_pixels(atlas_id).expect("atlas pixels");
    assert_eq!(atlas_pixels[0], 255, "atlas R at (0,0)");
    assert_eq!(atlas_pixels[1], 0,   "atlas G at (0,0)");
    assert_eq!(atlas_pixels[2], 0,   "atlas B at (0,0)");

    // Verify pass 2's output: pixel (32, 32) inside the 16..48 dst
    // rect should be red (sampled from atlas).
    let screen_pixels = sw_backend.read_image_pixels(screen_id).expect("screen pixels");
    let centre = (32 * 64 + 32) * 4;
    assert_eq!(screen_pixels[centre + 0], 255, "screen R at centre");
    assert_eq!(screen_pixels[centre + 1], 0,   "screen G at centre");
    assert_eq!(screen_pixels[centre + 2], 0,   "screen B at centre");
    // Outside the dst rect (corner) should be the clear colour (black).
    let corner = (4 * 64 + 4) * 4;
    assert_eq!(screen_pixels[corner + 0], 0, "screen corner R (cleared)");

    // One submission, no dispatch failures despite two renderpasses.
    assert_eq!(sw_backend.submission_count(), 1);
    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn software_backend_renders_glyph_run_end_to_end() {
    // Build a 16×16 atlas containing two distinguishable "glyphs":
    //   - glyph A at (0,0)..(8,16): solid red premultiplied
    //   - glyph B at (8,0)..(16,16): solid green premultiplied
    // Draw a glyph_run that stamps A at run-origin+(8,8) and B at
    // run-origin+(24,8) into a 64×64 target. Verify both colours
    // land at expected pixel positions.
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu::payloads::ImageCreatePayload;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};
    use aqueduct_gpu_host::software::{
        BeginRenderPassBody, GlyphInstance, GlyphRunParams,
        BUILTIN_PIPELINE_GLYPH_RUN,
    };

    let sock = tmp_socket("sw_glyph");
    let sw_backend = Arc::new(aqueduct_gpu_host::SoftwareBackend::new());
    let backend_for_listener: Arc<dyn aqueduct_gpu_host::Backend> = sw_backend.clone();

    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let atlas_mem = client.allocate_memory(16 * 16 * 4, MemoryUsage::ImageBacking).unwrap();
    let target_mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();

    let atlas_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: atlas_mem.region_id,
        region_offset: 0,
        format: 37, width: 16, height: 16, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    let target_id = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: target_mem.region_id,
        region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();

    thread::sleep(Duration::from_millis(30));

    // Build the atlas pixels: left half red, right half green.
    let mut atlas = vec![0u8; 16 * 16 * 4];
    for y in 0..16 {
        for x in 0..16 {
            let off = (y * 16 + x) * 4;
            if x < 8 {
                atlas[off + 0] = 255; atlas[off + 3] = 255;
            } else {
                atlas[off + 1] = 255; atlas[off + 3] = 255;
            }
        }
    }
    client.write_image(atlas_id, 16 * 4, atlas).unwrap();

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fb.push(FrameOp::BeginRenderPass, &BeginRenderPassBody {
        target_image_id: target_id.raw(),
        clear_color_rgba8: [0, 0, 0, 255],
    }.to_bytes()).unwrap();

    let pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_GLYPH_RUN);
    fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();

    let run = GlyphRunParams {
        color: [1.0, 1.0, 1.0, 1.0],
        atlas_image_id: atlas_id.raw(),
        origin: [10.0, 10.0],
        glyphs: vec![
            // Glyph A (red half), stamped at origin+(0,0) → (10,10).
            GlyphInstance { dx: 0.0, dy: 0.0, atlas_u: 0, atlas_v: 0, atlas_w: 8, atlas_h: 16 },
            // Glyph B (green half), stamped at origin+(16,0) → (26,10).
            GlyphInstance { dx: 16.0, dy: 0.0, atlas_u: 8, atlas_v: 0, atlas_w: 8, atlas_h: 16 },
        ],
    };
    let mut pc_body = vec![0u8; 4]; // stage_mask+offset+reserved
    pc_body.extend_from_slice(&run.to_bytes());
    fb.push(FrameOp::PushConstants, &pc_body).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000).unwrap();

    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target_id)
        .expect("target pixels");
    let at = |x: usize, y: usize| {
        let off = (y * 64 + x) * 4;
        (pixels[off], pixels[off+1], pixels[off+2], pixels[off+3])
    };

    // Inside red glyph (centre of (10,10)..(18,26)): (14, 18)
    assert_eq!(at(14, 18), (255, 0, 0, 255), "red glyph centre");
    // Inside green glyph (centre of (26,10)..(34,26)): (30, 18)
    assert_eq!(at(30, 18), (0, 255, 0, 255), "green glyph centre");
    // Between them (gap at x=20..25): should be cleared black.
    assert_eq!(at(22, 18), (0, 0, 0, 255), "gap pixel");

    assert_eq!(sw_backend.submission_count(), 1);
    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn shader_cache_warm_path_hit_after_upload() {
    // Listener configured with a real shader cache:
    //   - 1st connection: resolve misses, upload succeeds (cache populated).
    //   - 2nd connection (same listener): resolve HITS without re-upload.
    use aqueduct_gpu::backends::BackendId;
    use aqueduct_gpu_host::shader_cache::ShaderCache;

    let sock = tmp_socket("shader_cache");
    let cache_dir = {
        let mut p = std::env::temp_dir();
        p.push(format!("aqueduct-gpu-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    };
    let cache = Arc::new(ShaderCache::open(&cache_dir).unwrap());
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&sock, backend).unwrap().with_shader_cache(cache.clone());
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let hash = [0x42; 32];
    let backend_id = BackendId::new(GpuVendor::Software, 0);

    // Build minimal valid SPIR-V header.
    let mut spv = Vec::with_capacity(20);
    spv.extend_from_slice(&0x07230203u32.to_le_bytes());
    spv.extend_from_slice(&0x0001_0000u32.to_le_bytes());
    spv.extend_from_slice(&0u32.to_le_bytes());
    spv.extend_from_slice(&1u32.to_le_bytes());
    spv.extend_from_slice(&0u32.to_le_bytes());

    // ── Connection 1: resolve miss → upload → cache populated ────
    {
        let conn = Connection::connect(&sock).unwrap();
        let mut client = GpuClient::new(conn);
        client.handshake(ClientKind::FrescodRenderer).unwrap();

        let err = client.resolve_shader(hash, ShaderKind::SpirV, backend_id).unwrap_err();
        matches!(err, aqueduct_gpu_client::GpuClientError::ShaderResolveMissed { .. });

        let id = client.upload_shader(hash, ShaderKind::SpirV, backend_id, spv.clone()).unwrap();
        assert!(id.local_id() > 0);
    }
    // Give the cache write a beat to land.
    thread::sleep(Duration::from_millis(50));

    // ── Connection 2: resolve HITS via the shared cache ──────────
    {
        let conn = Connection::connect(&sock).unwrap();
        let mut client = GpuClient::new(conn);
        client.handshake(ClientKind::FrescodRenderer).unwrap();
        let id = client.resolve_shader(hash, ShaderKind::SpirV, backend_id)
            .expect("warm path should hit after prior upload");
        assert!(id.local_id() > 0);
    }

    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_dir_all(&cache_dir);
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

    // Then: upload succeeds (stub accepts everything that passes
    // the structural validator). Build a minimal SPIR-V header:
    // magic, version 1.0, generator, bound, schema.
    let mut spv = Vec::with_capacity(20);
    spv.extend_from_slice(&0x07230203u32.to_le_bytes()); // SPIRV magic
    spv.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // 1.0
    spv.extend_from_slice(&0u32.to_le_bytes()); // generator
    spv.extend_from_slice(&1u32.to_le_bytes()); // bound
    spv.extend_from_slice(&0u32.to_le_bytes()); // schema

    let shader_id = client.upload_shader(
        hash, ShaderKind::SpirV, backend_id, spv,
    ).unwrap();
    assert!(shader_id.local_id() > 0);

    // Validator-reject path: a forbidden capability (PhysicalStorage-
    // BufferAddresses = 5347) must be rejected.
    let mut bad_spv = Vec::new();
    bad_spv.extend_from_slice(&0x07230203u32.to_le_bytes());
    bad_spv.extend_from_slice(&0x0001_0000u32.to_le_bytes());
    bad_spv.extend_from_slice(&0u32.to_le_bytes());
    bad_spv.extend_from_slice(&1u32.to_le_bytes());
    bad_spv.extend_from_slice(&0u32.to_le_bytes());
    // OpCapability instruction: word_count=2, opcode=17.
    bad_spv.extend_from_slice(&((2u32 << 16) | 17u32).to_le_bytes());
    bad_spv.extend_from_slice(&5347u32.to_le_bytes());
    let rej = client.upload_shader(hash, ShaderKind::SpirV, backend_id, bad_spv);
    assert!(rej.is_err(), "validator must reject buffer-device-address");

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
