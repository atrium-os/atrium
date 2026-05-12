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
fn _unused_idnamespace_warning_suppressor() {
    // Imports referenced by docs only.
    let _ = IdNamespace::Builtin;
}
