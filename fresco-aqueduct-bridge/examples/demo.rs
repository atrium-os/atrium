//! `fresco-aqueduct-bridge` end-to-end demonstrator.
//!
//! Spins up a `SoftwareBackend` host endpoint in-process, connects a
//! `GpuClient` over a real Unix socket, builds a fresco-protocol
//! scene with a few nodes, translates it via the bridge, submits,
//! reads back the rendered pixels, and writes a PNG.
//!
//! No Vulkan, no GPU required — exercises the full Phase 1.3c +
//! 1.4 stack on tier-1 tiny-skia.
//!
//! Run:
//! ```sh
//! cargo run --example demo -p fresco-aqueduct-bridge
//! ```
//!
//! Output: `aqueduct-gpu-demo.png` in the current working directory.

use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct::Connection;
use aqueduct_gpu::{
    ids::ResourceId,
    payloads::{ClientKind, ImageCreatePayload, MemoryUsage},
};
use aqueduct_gpu_client::GpuClient;
use aqueduct_gpu_host::{Backend, Listener, SoftwareBackend};
use fresco_protocol as fp;

const W: u32 = 320;
const H: u32 = 200;

fn main() -> std::io::Result<()> {
    eprintln!("aqueduct-gpu-demo: {W}×{H} composite via tier-1 SW backend");

    // ── Spin up an in-process daemon ──────────────────────────────
    let sock = {
        let mut p = std::env::temp_dir();
        p.push(format!("aqueduct-gpu-demo-{}.sock", std::process::id()));
        p
    };
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener)
        .map_err(io_other)?;
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    eprintln!("aqueduct-gpu-demo: listener on {}", sock.display());

    // ── Client side ───────────────────────────────────────────────
    let conn = Connection::connect(&sock)?;
    let mut client = GpuClient::new(conn);
    client.handshake(ClientKind::FrescodRenderer)
        .map_err(|e| io_other(format!("handshake: {e:?}")))?;

    let mem = client.allocate_memory(
        (W * H * 4) as u64,
        MemoryUsage::ImageBacking,
    ).map_err(|e| io_other(format!("allocate_memory: {e:?}")))?;
    let target = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id,
        region_offset: 0,
        format: 37, width: W, height: H, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).map_err(|e| io_other(format!("create_image: {e:?}")))?;
    thread::sleep(Duration::from_millis(30));

    // ── Build a fresco-protocol scene ─────────────────────────────
    // - dark blue background
    // - a centred red rect
    // - a yellow rotated path
    // - a thin green stripe
    // - "aqueduct-gpu" rendered via real fresco-text shaping +
    //   rasterization, composited via glyph_run
    let scene_bg = fp::RectParams {
        x: 0.0, y: 0.0, w: W as f32, h: H as f32,
        r: 0.07, g: 0.10, b: 0.22, a: 1.0,
    };
    let scene_card = fp::RectParams {
        x: 60.0, y: 50.0, w: 200.0, h: 100.0,
        r: 0.85, g: 0.20, b: 0.20, a: 1.0,
    };
    let scene_stripe = fp::RectParams {
        x: 0.0, y: (H - 8) as f32, w: W as f32, h: 6.0,
        r: 0.20, g: 0.80, b: 0.40, a: 1.0,
    };
    let scene_rot = fp::PathParams {
        cx: (W as f32) * 0.5, cy: (H as f32) * 0.5,
        length: 220.0, width: 12.0,
        angle: -0.35, // ~ -20°
        r: 0.95, g: 0.85, b: 0.20, a: 1.0,
    };

    // ── Shape + rasterize text via fresco-text ────────────────────
    let font_path = std::env::var("DEMO_FONT").unwrap_or_else(|_|
        format!("{}/../test-assets/DejaVuSans.ttf", env!("CARGO_MANIFEST_DIR"))
    );
    let font_bytes = std::fs::read(&font_path)
        .map_err(|e| io_other(format!("read font {font_path}: {e}")))?;
    let atlas = fresco_text::shape_and_rasterize(
        &font_bytes,
        "aqueduct-gpu",
        20.0, // pixel size
    ).map_err(|e| io_other(format!("shape: {e}")))?;
    eprintln!(
        "aqueduct-gpu-demo: shaped {} glyphs into {}×{} atlas",
        atlas.glyphs.len(), atlas.width, atlas.height,
    );

    // Upload the atlas as a server-side image. fresco-text writes
    // its alpha as (R=G=B=255, A=coverage); tiny-skia premultiplies
    // on read, which on a premultiplied-RGBA destination would dim
    // glyphs. Pre-premultiply: store (A, A, A, A) so the post-read
    // premultiply is a no-op.
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
        backing_region: atlas_mem.region_id,
        region_offset: 0,
        format: 37, width: atlas.width, height: atlas.height, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).map_err(|e| io_other(format!("create atlas image: {e:?}")))?;
    thread::sleep(Duration::from_millis(30));
    client.write_image(atlas_image, atlas.width * 4, atlas_rgba)
        .map_err(|e| io_other(format!("write atlas: {e:?}")))?;

    // Build a fresco-protocol GlyphRunParams using the shaped quads.
    // fresco-text emits run-origin-relative quad rects in pixel
    // space; we transform them into the per-glyph (dx, dy, atlas_uv,
    // bearing) shape that the bridge's translate_glyph_run expects.
    let mut glyph_instances = Vec::with_capacity(atlas.glyphs.len());
    for q in &atlas.glyphs {
        // q.dx0/dy0 are run-origin-relative; (dy0 is baseline-relative
        // with ascenders negative). Atlas UV is in [0..1] from
        // fresco-text; convert back to pixel coords for fresco's
        // GlyphInstance.
        let au = (q.u0 * atlas.width  as f32).round() as u32;
        let av = (q.v0 * atlas.height as f32).round() as u32;
        let aw = ((q.u1 - q.u0) * atlas.width  as f32).round() as u32;
        let ah = ((q.v1 - q.v0) * atlas.height as f32).round() as u32;
        glyph_instances.push(fp::GlyphInstance {
            dx: q.dx0, dy: 0.0,
            atlas_u: au, atlas_v: av,
            atlas_w: aw, atlas_h: ah,
            bearing_x: 0.0,
            bearing_y: -q.dy0, // fresco-text already encodes glyph-top
        });
    }
    let text_run = fp::GlyphRunParams {
        x: 80.0,
        y: 175.0,  // baseline; ascenders draw ~16px above
        atlas_slot_id: 0,
        atlas_width: atlas.width,
        atlas_height: atlas.height,
        r: 1.0, g: 1.0, b: 1.0, a: 1.0,
        glyphs: glyph_instances,
    };

    // ── Submit a frame ────────────────────────────────────────────
    let fence = client.create_fence()
        .map_err(|e| io_other(format!("create_fence: {e:?}")))?;
    let mut fb = client.frame_builder();

    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 0, 255])
        .map_err(io_other)?;
    fresco_aqueduct_bridge::translate_rect(&mut fb, &scene_bg).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_rect(&mut fb, &scene_card).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_path(&mut fb, &scene_rot).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_rect(&mut fb, &scene_stripe).map_err(io_other)?;
    fresco_aqueduct_bridge::translate_glyph_run(&mut fb, &text_run, atlas_image)
        .map_err(io_other)?;
    fresco_aqueduct_bridge::end_renderpass(&mut fb).map_err(io_other)?;

    client.submit_frame(fence, fb, 1)
        .map_err(|e| io_other(format!("submit_frame: {e:?}")))?;
    let _ = client.wait_fence(fence, 1_000_000_000)
        .map_err(|e| io_other(format!("wait_fence: {e:?}")))?;
    thread::sleep(Duration::from_millis(50));

    // ── Read back and write PNG ───────────────────────────────────
    let pixels = sw_backend.read_image_pixels(target)
        .ok_or_else(|| io_other("backend didn't materialise target image"))?;
    if pixels.len() != (W * H * 4) as usize {
        return Err(io_other(format!(
            "unexpected pixel length {} (want {})", pixels.len(), W * H * 4
        )));
    }

    // tiny-skia stores premultiplied RGBA; un-premultiply for output
    // so colours look correct in image viewers.
    let mut rgba = vec![0u8; pixels.len()];
    for (i, px) in pixels.chunks_exact(4).enumerate() {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        let (or, og, ob) = if a == 0 {
            (0, 0, 0)
        } else {
            let inv = 255.0 / a as f32;
            (
                (r as f32 * inv).min(255.0) as u8,
                (g as f32 * inv).min(255.0) as u8,
                (b as f32 * inv).min(255.0) as u8,
            )
        };
        let off = i * 4;
        rgba[off + 0] = or;
        rgba[off + 1] = og;
        rgba[off + 2] = ob;
        rgba[off + 3] = a;
    }

    let out_path = std::env::args().nth(1)
        .unwrap_or_else(|| "aqueduct-gpu-demo.png".to_string());
    let file = File::create(&out_path)?;
    let w = BufWriter::new(file);
    let mut enc = png::Encoder::new(w, W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&rgba)?;

    eprintln!("aqueduct-gpu-demo: wrote {out_path}");
    eprintln!("aqueduct-gpu-demo: {} submissions, {} dispatch failures",
              sw_backend.submission_count(),
              sw_backend.dispatch_failure_count());

    drop(client);
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

fn io_other<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
