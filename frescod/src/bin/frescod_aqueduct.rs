//! `frescod-aqueduct` — frescod with `HeadlessRenderer` replaced by the
//! aqueduct-gpu stack.
//!
//! Topology compared to the production frescod binary:
//!
//! ```text
//!   ┌─ same ──────────────────────────────────────────────────┐
//!   │ CAS + SceneGraph + SlotTable + Compositor              │
//!   │ EnvelopeFrontend (fresco-protocol dispatcher)          │
//!   │ socket_server (per-client fan-in/fan-out)              │
//!   │ input_reader  (keyboard via /dev/input)                │
//!   │ pointer_reader (mouse)                                  │
//!   │ atrium_gpu::{Gpu, Display, Bo}  + page-flip            │
//!   └─────────────────────────────────────────────────────────┘
//!
//!   ┌─ different ─────────────────────────────────────────────┐
//!   │  HeadlessRenderer (venus / fresco-vulkan)              │
//!   │     becomes                                             │
//!   │  in-process aqueduct-gpu-host SoftwareBackend +        │
//!   │  GpuClient + fresco-aqueduct-bridge                    │
//!   └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Run inside the FreeBSD VM:
//!
//! ```sh
//! /mnt/host/frescod/target/aarch64-unknown-freebsd/debug/frescod-aqueduct
//! ```
//!
//! Listens on `/tmp/frescod.sock` (or `$FRESCOD_SOCK`). Standard
//! fresco-protocol clients connect unchanged.

#[path = "../input_reader.rs"]
mod input_reader;
#[path = "../pointer_reader.rs"]
mod pointer_reader;
#[path = "../socket_server.rs"]
mod socket_server;

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
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;
use fresco_vulkan::UploadRequest;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TARGET_FPS: u64 = 30;
const FRAME_NS:   u64 = 1_000_000_000 / TARGET_FPS;

fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    // ── Display + scanout BO (same as venus path) ────────────────
    let gpu = Gpu::open()?;
    let dpy = Display::open()?;
    dpy.bind(&gpu)?;
    let connectors = dpy.connectors()?;
    let conn = connectors.first().expect("at least one connector").clone();
    let mode = dpy.preferred_mode(conn.id)?;
    eprintln!(
        "frescod-aqueduct: connector {} {}x{} @ {} mHz, target {} fps",
        conn.id, mode.width, mode.height, mode.refresh_mhz, TARGET_FPS,
    );

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let flags = ATRIUM_GPU_BO_GPU_VISIBLE
        | ATRIUM_GPU_BO_CPU_VISIBLE
        | ATRIUM_GPU_BO_COHERENT
        | ATRIUM_GPU_BO_SCANOUT;
    let mut bo = gpu.alloc(bytes, flags)?;

    // ── In-process aqueduct-gpu-host + GpuClient ─────────────────
    // Use a Unix socket on tmpfs. In the D5+ end-state the client
    // talks directly to the atrium-gpu kmod and this daemon goes
    // away; for now in-process keeps it simple.
    let aq_sock = std::env::var("FRESCOD_AQUEDUCT_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod-aqueduct.sock".to_string());
    let _ = std::fs::remove_file(&aq_sock);
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&aq_sock, backend_for_listener)
        .map_err(io_other)?;
    thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    eprintln!("frescod-aqueduct: in-process daemon on {aq_sock}");

    let conn_aq = Connection::connect(&aq_sock)?;
    let mut client = GpuClient::new(conn_aq);
    client.handshake(ClientKind::FrescodRenderer)
        .map_err(|e| io_other(format!("handshake: {e:?}")))?;

    // Persistent target image for the whole screen. Reused every
    // frame — never resized, since modesetting happens once at
    // startup. Backing memory is allocated by the SoftwareBackend
    // internally (the region_id we pass in is just for resource-
    // table bookkeeping).
    let mem = client.allocate_memory(bytes, MemoryUsage::ImageBacking)
        .map_err(|e| io_other(format!("alloc target mem: {e:?}")))?;
    let target = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0),
        backing_region: mem.region_id, region_offset: 0,
        format: 37, width: mode.width, height: mode.height, depth: 1,
        mip_levels: 1, array_layers: 1, usage: 0x07,
    }).map_err(|e| io_other(format!("create target image: {e:?}")))?;
    thread::sleep(Duration::from_millis(30));

    // Persistent fence for frame submission — reused via timeline.
    let fence = client.create_fence()
        .map_err(|e| io_other(format!("create_fence: {e:?}")))?;

    // slot_id → ResourceId map for the atlas / texture pipeline.
    // Populated lazily on the first UploadRequest::Texture for a slot.
    let mut slot_images: HashMap<u32, SlotImage> = HashMap::new();

    // ── Shared scene-server state (same as venus path) ───────────
    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));

    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx.clone());

    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));

    // ── Fresco-protocol socket server (same as venus path) ───────
    let sock_path = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/frescod.sock"));
    let event_subs = socket_server::spawn(
        socket_server::Shared { frontend: frontend.clone() },
        &sock_path,
    )?;
    socket_server::spawn_event_fanout(ev_rx, event_subs);

    // Input readers (same as venus path).
    input_reader::spawn(ev_tx.clone(), comp.clone());
    pointer_reader::spawn(ev_tx, comp.clone(), mode.width, mode.height);

    // ── Frame loop ───────────────────────────────────────────────
    let mut timeline: u64 = 0;
    let render_and_flip = |client: &mut GpuClient,
                           slot_images: &mut HashMap<u32, SlotImage>,
                           timeline: &mut u64,
                           bo: &mut atrium_gpu::Bo|
        -> Result<(), std::io::Error>
    {
        *timeline += 1;
        render_one_frame_aqueduct(
            client, &frontend, &comp, target, fence, *timeline, slot_images,
        ).map_err(io_other)?;
        copy_backend_to_bo(&sw_backend, target, bo).map_err(io_other)?;
        Ok(())
    };

    // First frame: render + SET_MODE before flipping.
    render_and_flip(&mut client, &mut slot_images, &mut timeline, &mut bo)?;
    dpy.set_mode(conn.id, &bo, mode)?;
    dpy.page_flip(conn.id, &bo)?;

    let mut next = Instant::now() + Duration::from_nanos(FRAME_NS);
    loop {
        render_and_flip(&mut client, &mut slot_images, &mut timeline, &mut bo)?;
        dpy.page_flip(conn.id, &bo)?;

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += Duration::from_nanos(FRAME_NS);
    }
}

/// Cached state for one fresco SlotTable slot's atlas/texture image
/// on the aqueduct-gpu side.
struct SlotImage {
    image: ResourceId,
    width: u32,
    height: u32,
}

/// Snapshot per-window scene state, drain pending texture uploads,
/// translate every node via `fresco-aqueduct-bridge`, submit one
/// frame, wait its fence.
fn render_one_frame_aqueduct(
    client: &mut GpuClient,
    frontend: &Arc<Mutex<EnvelopeFrontend>>,
    comp: &Arc<Mutex<Compositor>>,
    target: ResourceId,
    fence: ResourceId,
    timeline: u64,
    slot_images: &mut HashMap<u32, SlotImage>,
) -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Drain pending texture uploads → write_image into the
    //       backend's per-slot Pixmaps.
    let (uploads, clears) = {
        let mut fe = frontend.lock().unwrap();
        fe.take_pending_uploads()
    };
    for clear in &clears {
        if let Some(s) = slot_images.remove(clear) {
            let _ = client.destroy_image(s.image);
        }
    }
    for upload in uploads {
        match upload {
            UploadRequest::Texture { slot_id, bytes, width, height, format: _ } => {
                // Drop any prior image for this slot; format/dim may
                // have changed.
                if let Some(s) = slot_images.remove(&slot_id) {
                    let _ = client.destroy_image(s.image);
                }
                let mem = client.allocate_memory(
                    bytes.len() as u64,
                    MemoryUsage::ImageBacking,
                ).map_err(|e| io_other(format!("alloc slot {slot_id}: {e:?}")))?;
                let image = client.create_image(ImageCreatePayload {
                    image_id: ResourceId(0),
                    backing_region: mem.region_id, region_offset: 0,
                    format: 37, width, height, depth: 1,
                    mip_levels: 1, array_layers: 1, usage: 0x07,
                }).map_err(|e| io_other(format!("create slot {slot_id}: {e:?}")))?;
                // Don't sleep here on the hot path; the daemon
                // dispatches sequentially per session and create
                // precedes write.
                let row_pitch = width * 4;
                let pixels = premultiply_for_tiny_skia(&bytes);
                client.write_image(image, row_pitch, pixels)
                    .map_err(|e| io_other(format!("write slot {slot_id}: {e:?}")))?;
                slot_images.insert(slot_id, SlotImage { image, width, height });
            }
            UploadRequest::TextureRegion { slot_id, .. } => {
                // Sub-region atlas patching (used by the server-side
                // text engine for incremental glyph upload). Not yet
                // supported by aqueduct-gpu's OP_GPU_IMAGE_WRITE which
                // writes from offset 0. Log + skip. The text engine
                // will re-emit a full Texture upload on the next
                // dirty cycle which we DO handle.
                log::debug!("frescod-aqueduct: skipped TextureRegion for slot {slot_id} \
                             (sub-region upload not yet implemented)");
            }
        }
    }

    // ── 2. Snapshot per-window WM state in z-order bottom→top.
    let layers: Vec<(u32, (f32, f32))> = {
        let g = comp.lock().unwrap();
        let mut out = Vec::with_capacity(g.windows.len());
        out.push((0u32, (0.0, 0.0)));
        for &id in &g.z_order {
            if id == 0 { continue; }
            if let Some(w) = g.windows.get(&id) {
                out.push((id as u32, (w.pos.0, w.pos.1)));
            }
        }
        out
    };

    // ── 3. Build a single frame: BeginRenderPass → walk nodes →
    //       per-node bridge translator → EndRenderPass.
    let mut fb = client.frame_builder();
    fresco_aqueduct_bridge::begin_renderpass(&mut fb, target, [0, 0, 0, 255])?;

    {
        let fe = frontend.lock().unwrap();
        for (win_id, (ox, oy)) in &layers {
            let Some(state) = fe.window_state(*win_id) else { continue; };

            // rect_nodes
            for p in state.rect_nodes.values() {
                let translated = fp::RectParams { x: p.x + ox, y: p.y + oy, ..*p };
                fresco_aqueduct_bridge::translate_rect(&mut fb, &translated)?;
            }
            // path_nodes (centre coords get the window offset)
            for p in state.path_nodes.values() {
                let translated = fp::PathParams { cx: p.cx + ox, cy: p.cy + oy, ..*p };
                fresco_aqueduct_bridge::translate_path(&mut fb, &translated)?;
            }
            // texture_nodes — need the slot's image_id
            for p in state.texture_nodes.values() {
                let Some(slot) = slot_images.get(&p.slot_id) else { continue; };
                let translated = fp::TextureParams { x: p.x + ox, y: p.y + oy, ..*p };
                fresco_aqueduct_bridge::translate_texture(
                    &mut fb, &translated,
                    slot.image, slot.width, slot.height,
                )?;
            }
            // glyph_run_nodes — same: look up the atlas slot's image
            for p in state.glyph_run_nodes.values() {
                let Some(slot) = slot_images.get(&p.atlas_slot_id) else { continue; };
                let translated = fp::GlyphRunParams {
                    x: p.x + ox, y: p.y + oy,
                    glyphs: p.glyphs.clone(),
                    ..*p
                };
                fresco_aqueduct_bridge::translate_glyph_run(
                    &mut fb, &translated, slot.image,
                )?;
            }
        }
    }

    fresco_aqueduct_bridge::end_renderpass(&mut fb)?;

    // ── 4. Submit + wait.
    client.submit_frame(fence, fb, timeline)?;
    let _ = client.wait_fence(fence, 50_000_000)?; // 50ms budget per frame at 30 fps
    Ok(())
}

/// Read tier-1 SW backend's target Pixmap → BGRA-swap → scanout BO.
fn copy_backend_to_bo(
    sw: &SoftwareBackend,
    target: ResourceId,
    bo: &mut atrium_gpu::Bo,
) -> Result<(), Box<dyn std::error::Error>> {
    let pixels = sw.read_image_pixels(target)
        .ok_or_else(|| io_other("SoftwareBackend missing target image"))?;
    let dst = bo.as_mut_slice();
    if pixels.len() != dst.len() {
        return Err(Box::new(io_other(format!(
            "pixel size mismatch: backend {} vs BO {}", pixels.len(), dst.len()
        ))));
    }
    // tiny-skia Pixmap is RGBA premultiplied; kmod scanout is BGRA8.
    // Swap R↔B inline.
    for (i, px) in pixels.chunks_exact(4).enumerate() {
        let off = i * 4;
        dst[off + 0] = px[2];
        dst[off + 1] = px[1];
        dst[off + 2] = px[0];
        dst[off + 3] = px[3];
    }
    Ok(())
}

/// fresco-text writes its alpha atlas as (R=G=B=255, A=coverage)
/// straight (non-premultiplied) RGBA. tiny-skia's read path
/// premultiplies on store, which would darken the result on a
/// premultiplied display. Pre-premultiply here so the round-trip
/// is a no-op.
///
/// Heuristic: detect "looks like a fresco-text atlas" by R=255 on
/// non-zero alpha pixels, and convert in-place. Other formats pass
/// through unchanged.
fn premultiply_for_tiny_skia(src: &[u8]) -> Vec<u8> {
    let mut out = src.to_vec();
    if out.len() % 4 != 0 { return out; }
    // Quick scan: if every non-zero-alpha pixel has R=G=B=255, it's
    // a fresco-text atlas — overwrite RGB with A.
    let mut looks_text = true;
    for px in out.chunks_exact(4) {
        if px[3] != 0 && (px[0] != 255 || px[1] != 255 || px[2] != 255) {
            looks_text = false; break;
        }
    }
    if looks_text {
        for px in out.chunks_exact_mut(4) {
            let a = px[3];
            px[0] = a; px[1] = a; px[2] = a;
        }
    }
    out
}

fn io_other<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
