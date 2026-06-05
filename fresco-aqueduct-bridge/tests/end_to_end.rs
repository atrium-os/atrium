//! End-to-end: fresco scene params → bridge translator →
//! aqueduct-gpu-client → SoftwareBackend → real rendered pixels.
//!
//! This is the demonstrator for Phase 1.4: shows that frescod's
//! rendering can be expressed purely as wire-side FrameOp records
//! produced by the bridge.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct::Connection;
use aqueduct_gpu::{
    ids::{IdNamespace, ResourceId},
    payloads::{ClientKind, MemoryUsage},
};
use aqueduct_gpu_client::GpuClient;
use aqueduct_gpu_host::{Backend, Listener, SoftwareBackend};
use fresco_protocol as fp;

fn tmp_socket(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("frescobridge-{}-{}.sock",
                   std::process::id(), name));
    p
}

#[test]
fn rect_through_bridge_renders_pixels() {
    let sock = tmp_socket("rect");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();
    let target = client.create_image(aqueduct_gpu::payloads::ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id,
        region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    thread::sleep(Duration::from_millis(30));

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 0, 255]).unwrap();
    fresco_aqueduct_bridge::translate_rect(&mut fb, &fp::RectParams {
        x: 16.0, y: 16.0, w: 32.0, h: 32.0,
        r: 1.0, g: 0.5, b: 0.0, a: 1.0, // orange
    }).unwrap();
    fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000);
    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target).expect("pixels");
    // Centre at (32, 32) is inside the orange rect.
    let off = (32 * 64 + 32) * 4;
    // tiny-skia stores premultiplied. Orange (255, 128, 0, 255) ⇒
    // premultiplied (255, 128, 0, 255). Allow ±1 for fp rounding.
    let r = pixels[off + 0];
    let g = pixels[off + 1];
    let b = pixels[off + 2];
    let a = pixels[off + 3];
    assert_eq!(r, 255, "R");
    assert!(g >= 127 && g <= 128, "G was {g}");
    assert_eq!(b, 0, "B");
    assert_eq!(a, 255, "A");

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn path_through_bridge_renders_axis_aligned_rect_at_angle_zero() {
    let sock = tmp_socket("path");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();
    let target = client.create_image(aqueduct_gpu::payloads::ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id,
        region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    thread::sleep(Duration::from_millis(30));

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 0, 255]).unwrap();
    // angle=0: axis-aligned 20×10 quad centred at (32, 32) ⇒
    // covers x∈[22..42], y∈[27..37].
    fresco_aqueduct_bridge::translate_path(&mut fb, &fp::PathParams {
        cx: 32.0, cy: 32.0, length: 20.0, width: 10.0, angle: 0.0,
        r: 0.0, g: 1.0, b: 0.0, a: 1.0,
    }).unwrap();
    fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000);
    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target).expect("pixels");
    // Pixel (32, 32) should be inside the green quad.
    let off = (32 * 64 + 32) * 4;
    assert_eq!(pixels[off + 0], 0, "R inside path");
    assert_eq!(pixels[off + 1], 255, "G inside path");
    assert_eq!(pixels[off + 2], 0, "B inside path");
    // Pixel (5, 5) should still be clear black.
    let off2 = (5 * 64 + 5) * 4;
    assert_eq!(pixels[off2 + 1], 0, "outside path stays black");

    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn multi_node_scene_composites_through_bridge() {
    // Demonstrates the real frescod-shaped flow: one frame, multiple
    // fresco scene nodes, translated one-by-one into a single
    // FrameOp stream and rendered together.
    let sock = tmp_socket("scene");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();
    let target = client.create_image(aqueduct_gpu::payloads::ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id, region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    thread::sleep(Duration::from_millis(30));

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();
    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [10, 10, 30, 255]).unwrap();

    // Background rect.
    fresco_aqueduct_bridge::translate_rect(&mut fb, &fp::RectParams {
        x: 0.0, y: 0.0, w: 64.0, h: 64.0,
        r: 0.1, g: 0.1, b: 0.2, a: 1.0,
    }).unwrap();
    // Foreground rotated quad (axis-aligned for predictable test).
    fresco_aqueduct_bridge::translate_path(&mut fb, &fp::PathParams {
        cx: 32.0, cy: 32.0, length: 16.0, width: 16.0, angle: 0.0,
        r: 1.0, g: 0.0, b: 0.0, a: 1.0,
    }).unwrap();

    fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();
    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000);
    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target).expect("pixels");
    // Centre should be red (foreground covers it).
    let centre = (32 * 64 + 32) * 4;
    assert_eq!(pixels[centre + 0], 255, "centre R");
    assert_eq!(pixels[centre + 1], 0,   "centre G");
    assert_eq!(pixels[centre + 2], 0,   "centre B");
    // Corner should be background (dark blue tint).
    let corner = (1 * 64 + 1) * 4;
    assert!(pixels[corner + 0] < 64,  "corner R dim");
    assert!(pixels[corner + 2] > pixels[corner + 0], "corner B > R");

    assert_eq!(sw_backend.submission_count(), 1);
    assert_eq!(sw_backend.dispatch_failure_count(), 0);

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn partial_redraw_via_bridge_helpers() {
    // Regression: begin_renderpass_no_clear + set_scissor through the
    // bridge produce a working intra-window dirty-rect pass.
    //
    // Pass 1: full clear + cyan fill (whole 64×64).
    // Pass 2: no_clear + scissor to top-left 16×16, magenta fill.
    // Expected: scissor area magenta, rest still cyan.
    let sock = tmp_socket("partial");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer).unwrap();

    let mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();
    let target = client.create_image(aqueduct_gpu::payloads::ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id,
        region_offset: 0,
        format: 37, width: 64, height: 64, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    thread::sleep(Duration::from_millis(30));

    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();

    // Pass 1: full clear-and-fill cyan.
    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 0, 255]).unwrap();
    fresco_aqueduct_bridge::translate_rect(&mut fb, &fp::RectParams {
        x: 0.0, y: 0.0, w: 64.0, h: 64.0,
        r: 0.0, g: 1.0, b: 1.0, a: 1.0,
    }).unwrap();
    fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();

    // Pass 2: no-clear + scissor to top-left 16×16, draw magenta over the
    // whole image. Only the scissor region should change colour.
    fresco_aqueduct_bridge::begin_renderpass_no_clear(&mut fb, target).unwrap();
    fresco_aqueduct_bridge::set_scissor(&mut fb, 0, 0, 16, 16).unwrap();
    fresco_aqueduct_bridge::translate_rect(&mut fb, &fp::RectParams {
        x: 0.0, y: 0.0, w: 64.0, h: 64.0,
        r: 1.0, g: 0.0, b: 1.0, a: 1.0,
    }).unwrap();
    fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    let _ = client.wait_fence(fence, 1_000_000_000);
    thread::sleep(Duration::from_millis(50));

    let pixels = sw_backend.read_image_pixels(target).expect("pixels");
    // (8, 8): inside scissor → magenta (255, 0, 255, 255).
    let inside = (8 * 64 + 8) * 4;
    assert_eq!(pixels[inside + 0], 255, "R inside scissor");
    assert_eq!(pixels[inside + 1],   0, "G inside scissor");
    assert_eq!(pixels[inside + 2], 255, "B inside scissor");
    // (20, 20): outside scissor → cyan preserved (0, 255, 255, 255).
    let outside = (20 * 64 + 20) * 4;
    assert_eq!(pixels[outside + 0],   0, "R outside (cyan preserved)");
    assert_eq!(pixels[outside + 1], 255, "G outside");
    assert_eq!(pixels[outside + 2], 255, "B outside");

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn _unused_idnamespace_warning_suppressor() {
    // Imports referenced by docs only.
    let _ = IdNamespace::Builtin;
}

/// Proves the correctness invariant behind frescod's level-3 damage-rect
/// NODE CULLING (frescod_aqueduct.rs): on a partial pass
/// (`begin_renderpass_no_clear` + `set_scissor`), a node whose geometry
/// lies entirely OUTSIDE the scissor contributes ZERO pixels — so emitting
/// it (no cull) and not emitting it (cull) produce byte-IDENTICAL output.
/// The daemon's cull skips exactly such nodes (bbox ∩ scissor == ∅), so it
/// cannot change a rendered frame; it only saves their setup. This also
/// validates that the SoftwareBackend actually honours SET_SCISSOR (a node
/// inside the frame but outside the scissor must not draw).
#[test]
fn offscreen_node_in_partial_pass_contributes_nothing() {
    // Render base(blue) → partial(no_clear + scissor over A) emitting the
    // given rects; return the read-back RGBA8.
    fn render(emit_b: bool) -> Vec<u8> {
        let sock = tmp_socket(if emit_b { "cull-ab" } else { "cull-a" });
        let sw = Arc::new(SoftwareBackend::new());
        let backend: Arc<dyn Backend> = sw.clone();
        let listener = Listener::bind(&sock, backend).unwrap();
        let server = thread::spawn(move || { let _ = listener.accept_loop(); });
        thread::sleep(Duration::from_millis(50));

        let conn = Connection::connect(&sock).unwrap();
        let mut client = GpuClient::new(conn);
        client.handshake(ClientKind::FrescodRenderer).unwrap();
        let mem = client.allocate_memory(64 * 64 * 4, MemoryUsage::ImageBacking).unwrap();
        let target = client.create_image(aqueduct_gpu::payloads::ImageCreatePayload {
            image_id: ResourceId(0), backing_region: mem.region_id,
            region_offset: 0, format: 37, width: 64, height: 64, depth: 1,
            mip_levels: 1, array_layers: 1, usage: 0x07,
        }).unwrap();
        thread::sleep(Duration::from_millis(30));
        let fence = client.create_fence().unwrap();

        // Frame 1: full clear to blue — the retained base.
        let mut fb = client.frame_builder();
        fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 255, 255]).unwrap();
        fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();
        client.submit_frame(fence, fb, 1).unwrap();
        let _ = client.wait_fence(fence, 1_000_000_000);

        // Frame 2: PARTIAL — no-clear + scissor over A's region [8,24)².
        // A (red) is inside the scissor; B (green) is at [40,56)², fully
        // outside it — the exact case the daemon's cull drops.
        let mut fb = client.frame_builder();
        fresco_aqueduct_bridge::begin_renderpass_no_clear(&mut fb, target).unwrap();
        fresco_aqueduct_bridge::set_scissor(&mut fb, 8, 8, 16, 16).unwrap();
        fresco_aqueduct_bridge::translate_rect(&mut fb, &fp::RectParams {
            x: 8.0, y: 8.0, w: 16.0, h: 16.0, r: 1.0, g: 0.0, b: 0.0, a: 1.0,
        }).unwrap();
        if emit_b {
            fresco_aqueduct_bridge::translate_rect(&mut fb, &fp::RectParams {
                x: 40.0, y: 40.0, w: 16.0, h: 16.0, r: 0.0, g: 1.0, b: 0.0, a: 1.0,
            }).unwrap();
        }
        fresco_aqueduct_bridge::end_renderpass(&mut fb).unwrap();
        client.submit_frame(fence, fb, 2).unwrap();
        let _ = client.wait_fence(fence, 1_000_000_000);
        thread::sleep(Duration::from_millis(50));

        let px = sw.read_image_pixels(target).expect("pixels");
        drop(client);
        let _ = server;
        let _ = std::fs::remove_file(&sock);
        px
    }

    let with_b = render(true);    // no cull (emit the off-scissor node)
    let without_b = render(false); // cull (skip it)
    let at = |buf: &[u8], x: usize, y: usize| {
        let o = (y * 64 + x) * 4;
        [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
    };

    // THE INVARIANT: emitting vs culling the off-scissor node is identical.
    assert_eq!(with_b, without_b,
        "off-scissor node changed the framebuffer — culling would be unsafe \
         (or SET_SCISSOR is not honoured)");

    // Meaningful, not vacuous: A actually drew (red) inside the scissor...
    assert_eq!(at(&without_b, 16, 16), [255, 0, 0, 255], "A should be red");
    // ...and B's region stayed the base blue (scissor clipped it out, proving
    // the SW backend honours the scissor — the precondition for the cull).
    assert_eq!(at(&with_b, 48, 48), [0, 0, 255, 255],
        "B region must remain base blue — scissor must clip the off-region draw");
}
