//! `frescod-aqueduct-smoke` — paint a fresco-protocol scene to the
//! kmod scanout via the aqueduct-gpu stack.
//!
//! The minimal demonstration that aqueduct-gpu can replace
//! fresco-vulkan's `HeadlessRenderer` in frescod's render loop:
//!
//! ```text
//!   atrium-gpu kmod (scanout BO + page-flip)
//!     ▲
//!     │  memcpy RGBA→BGRA
//!     │
//!   aqueduct-gpu-host SoftwareBackend (tier-1 tiny-skia)
//!     ▲
//!     │  aqueduct-gpu wire (Unix socket, in-process)
//!     │
//!   GpuClient + fresco-aqueduct-bridge
//!     ▲
//!     │  fresco-protocol Params (rect / path / glyph_run)
//!     │
//!   THIS BINARY
//! ```
//!
//! Compared to frescod's main loop, this binary:
//! - Doesn't accept fresco-protocol connections from clients
//!   (single-shot in-process scene)
//! - Doesn't run the input readers
//! - Doesn't load atrium-core / atrium-text SPIR-V bundles
//!   (tier-1 SW renderer has hand-coded equivalents)
//! - Otherwise drives the same atrium-gpu-rs scanout BO + page-flip
//!   path frescod uses
//!
//! Useful for proving the aqueduct-gpu stack drives real pixels on
//! a FreeBSD guest before doing the full frescod-main rewire.
//!
//! Usage (inside the FreeBSD VM):
//!
//! ```sh
//! DEMO_FONT=/mnt/host/test-assets/DejaVuSans.ttf  \
//!   /mnt/host/frescod/target/aarch64-unknown-freebsd/debug/frescod-aqueduct-smoke
//! ```
//!
//! Paints one frame and exits. The kmod's last page-flip stays on
//! display until something else replaces it.

use std::io;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use atrium_gpu::abi::*;
use atrium_gpu::{Display, Gpu};

use aqueduct::Connection;
use aqueduct_gpu::{
    ids::ResourceId,
    payloads::{ClientKind, ImageCreatePayload, MemoryUsage},
};
use aqueduct_gpu_client::GpuClient;
use aqueduct_gpu_host::{Backend, Listener, SoftwareBackend};
use fresco_protocol as fp;

fn main() -> io::Result<()> {
    let _ = env_logger::try_init();

    // ── Open the kmod's scanout chain (same as frescod main.rs) ──
    let gpu = Gpu::open()?;
    let dpy = Display::open()?;
    dpy.bind(&gpu)?;
    let connectors = dpy.connectors()?;
    let conn = connectors.first().expect("at least one connector").clone();
    let mode = dpy.preferred_mode(conn.id)?;
    eprintln!(
        "frescod-aqueduct-smoke: connector {} {}×{} @ {} mHz",
        conn.id, mode.width, mode.height, mode.refresh_mhz,
    );

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let flags = ATRIUM_GPU_BO_GPU_VISIBLE
        | ATRIUM_GPU_BO_CPU_VISIBLE
        | ATRIUM_GPU_BO_COHERENT
        | ATRIUM_GPU_BO_SCANOUT;
    let mut bo = gpu.alloc(bytes, flags)?;

    // ── Spawn an aqueduct-gpu-host with SoftwareBackend ────────────
    let sock_path = std::env::var("FRESCOD_AQUEDUCT_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod-aqueduct.sock".to_string());
    let _ = std::fs::remove_file(&sock_path);
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock_path, backend_for_listener)
        .map_err(io_other)?;
    thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    eprintln!("frescod-aqueduct-smoke: in-process daemon on {sock_path}");

    // ── Client side: connect, handshake, allocate target ──────────
    let aq = Connection::connect(&sock_path)?;
    let mut client = GpuClient::new(aq);
    client.handshake(ClientKind::FrescodRenderer)
        .map_err(|e| io_other(format!("handshake: {e:?}")))?;
    let mem = client.allocate_memory(bytes, MemoryUsage::ImageBacking)
        .map_err(|e| io_other(format!("alloc mem: {e:?}")))?;
    let target = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id,
        region_offset: 0,
        format: 37, // VK_FORMAT_R8G8B8A8_UNORM
        width: mode.width, height: mode.height, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).map_err(|e| io_other(format!("create image: {e:?}")))?;
    thread::sleep(Duration::from_millis(30));

    // ── Build a small fresco scene scaled to the screen ───────────
    let w = mode.width as f32;
    let h = mode.height as f32;
    let bg = fp::RectParams {
        x: 0.0, y: 0.0, w, h,
        r: 0.07, g: 0.10, b: 0.22, a: 1.0, radius: 0.0,
    };
    let card = fp::RectParams {
        x: w * 0.20, y: h * 0.25, w: w * 0.60, h: h * 0.50,
        r: 0.85, g: 0.20, b: 0.20, a: 1.0, radius: 24.0,
    };
    let rotated = fp::PathParams {
        cx: w * 0.5, cy: h * 0.5,
        length: w * 0.55, width: h * 0.04,
        angle: -0.35,
        r: 0.95, g: 0.85, b: 0.20, a: 1.0,
    };
    let stripe = fp::RectParams {
        x: 0.0, y: h - 8.0, w, h: 6.0,
        r: 0.20, g: 0.80, b: 0.40, a: 1.0, radius: 0.0,
    };

    // ── Shape + rasterize text via fresco-text + upload atlas ────
    let font_path = std::env::var("DEMO_FONT").unwrap_or_else(|_|
        "/mnt/host/test-assets/DejaVuSans.ttf".to_string()
    );
    let font_bytes = std::fs::read(&font_path)
        .map_err(|e| io_other(format!("read font {font_path}: {e}")))?;
    let atlas = fresco_text::shape_and_rasterize(
        &font_bytes,
        "aqueduct-gpu on FreeBSD",
        (h * 0.05).max(16.0),
    ).map_err(|e| io_other(format!("shape: {e}")))?;
    eprintln!(
        "frescod-aqueduct-smoke: shaped {} glyphs into {}×{} atlas",
        atlas.glyphs.len(), atlas.width, atlas.height,
    );

    // Pre-premultiply atlas so tiny-skia's post-read premultiply is
    // a no-op. fresco-text emits (R=G=B=255, A=coverage).
    let mut atlas_rgba = atlas.pixels.clone();
    for px in atlas_rgba.chunks_exact_mut(4) {
        let a = px[3];
        px[0] = a; px[1] = a; px[2] = a;
    }
    let atlas_mem = client.allocate_memory(
        atlas_rgba.len() as u64,
        MemoryUsage::ImageBacking,
    ).map_err(|e| io_other(format!("alloc atlas mem: {e:?}")))?;
    let atlas_image = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: atlas_mem.region_id, region_offset: 0,
        format: 37, width: atlas.width, height: atlas.height, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).map_err(|e| io_other(format!("create atlas image: {e:?}")))?;
    thread::sleep(Duration::from_millis(30));
    client.write_image(atlas_image, atlas.width * 4, atlas_rgba)
        .map_err(|e| io_other(format!("write atlas: {e:?}")))?;

    let glyph_instances: Vec<_> = atlas.glyphs.iter().map(|q| {
        let au = (q.u0 * atlas.width  as f32).round() as u32;
        let av = (q.v0 * atlas.height as f32).round() as u32;
        let aw = ((q.u1 - q.u0) * atlas.width  as f32).round() as u32;
        let ah = ((q.v1 - q.v0) * atlas.height as f32).round() as u32;
        fp::GlyphInstance {
            dx: q.dx0, dy: 0.0,
            atlas_u: au, atlas_v: av,
            atlas_w: aw, atlas_h: ah,
            bearing_x: 0.0, bearing_y: -q.dy0,
        }
    }).collect();
    let text = fp::GlyphRunParams {
        x: w * 0.20,
        y: h * 0.90,
        atlas_slot_id: 0,
        atlas_width: atlas.width, atlas_height: atlas.height,
        r: 1.0, g: 1.0, b: 1.0, a: 1.0,
        glyphs: glyph_instances,
    };

    // ── Submit one frame ──────────────────────────────────────────
    let fence = client.create_fence()
        .map_err(|e| io_other(format!("create_fence: {e:?}")))?;
    let mut fb = client.frame_builder();
    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 0, 255])
        .map_err(io_other)?;
    fresco_aqueduct_bridge::translate_rect(&mut fb, &bg).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_rect(&mut fb, &card).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_path(&mut fb, &rotated).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_rect(&mut fb, &stripe).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_glyph_run(&mut fb, &text, atlas_image)
        .map_err(io_other)?;
    fresco_aqueduct_bridge::end_renderpass(&mut fb).map_err(io_other)?;

    client.submit_frame(fence, fb, 1)
        .map_err(|e| io_other(format!("submit_frame: {e:?}")))?;
    let _ = client.wait_fence(fence, 1_000_000_000)
        .map_err(|e| io_other(format!("wait_fence: {e:?}")))?;
    thread::sleep(Duration::from_millis(50));

    // ── Read pixels back from SoftwareBackend → scanout BO ───────
    let pixels = sw_backend.read_image_pixels(target)
        .ok_or_else(|| io_other("backend didn't materialise target image"))?;
    let dst = bo.as_mut_slice();
    if pixels.len() != dst.len() {
        return Err(io_other(format!(
            "pixel size mismatch: backend {} vs BO {}", pixels.len(), dst.len()
        )));
    }
    // tiny-skia's Pixmap is RGBA premultiplied. The kmod's scanout
    // format is BGRA8. Swap R↔B during the copy (same as the D1
    // step 2(a) hello_rect path).
    for (i, px) in pixels.chunks_exact(4).enumerate() {
        let off = i * 4;
        dst[off + 0] = px[2]; // B
        dst[off + 1] = px[1]; // G
        dst[off + 2] = px[0]; // R
        dst[off + 3] = px[3]; // A
    }

    // ── Display ──────────────────────────────────────────────────
    dpy.set_mode(conn.id, &bo, mode)?;
    dpy.page_flip(conn.id, &bo)?;
    eprintln!("frescod-aqueduct-smoke: page-flipped one frame");
    eprintln!("frescod-aqueduct-smoke: {} submissions, {} dispatch failures",
              sw_backend.submission_count(),
              sw_backend.dispatch_failure_count());

    // Hold the frame on screen for a few seconds so QEMU's display
    // updates before the BO is released.
    thread::sleep(Duration::from_secs(5));
    Ok(())
}

fn io_other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}
