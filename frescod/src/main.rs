//! frescod — Fresco scene-server daemon.
//!
//! Wires the M2 envelope-based stack onto real FreeBSD scanout:
//!
//!   aqueduct::Connection (envelope transport, per-client thread pair)
//!   ↓
//!   fresco_scene_server::EnvelopeFrontend (per-window scene state)
//!   ↓ (extract_rect_nodes / extract_texture_batches, merged in z-order)
//!   fresco_vulkan::HeadlessRenderer (compute + indirect draw)
//!   ↓ (read_pixels, BGRA8)
//!   atrium_gpu::Bo (scanout BO mapped CPU-visible)
//!   ↓
//!   atrium_gpu::Display::page_flip
//!
//! The legacy tiny-skia / 128-byte / CommandFrontend path was removed
//! at the M2.7 cutover. Today this binary is single-display, runs at
//! a fixed ~30 fps cadence (no vblank events on virtio-gpu yet).
//!
//! Multi-window composition: per frame, walk WM z-order; for each
//! window, take its `WindowSceneState` rect/texture nodes, translate
//! by `window.pos`, accumulate into one merged rect/texture set, then
//! issue one `HeadlessRenderer::render_to_buffer()`. Texture slot
//! collisions across windows are not yet handled (M3+); typical
//! single-window apps render correctly.

mod socket_server;

use atrium_gpu::abi::*;
use atrium_gpu::{Display, Gpu};

use fresco_protocol::{RectParams, TextureParams};
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;
use fresco_vulkan::{HeadlessRenderer, SceneNode, TextureBatch, TextureNode};

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TARGET_FPS: u64 = 30;
const FRAME_NS:   u64 = 1_000_000_000 / TARGET_FPS;

fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    /* ── Display + scanout BO ─────────────────────────────────────── */
    let gpu = Gpu::open()?;
    let dpy = Display::open()?;
    dpy.bind(&gpu)?;

    let connectors = dpy.connectors()?;
    let conn = connectors.first().expect("at least one connector").clone();
    let mode = dpy.preferred_mode(conn.id)?;
    eprintln!(
        "frescod: connector {} {}x{} @ {} mHz, target {} fps",
        conn.id, mode.width, mode.height, mode.refresh_mhz, TARGET_FPS,
    );

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let flags = ATRIUM_GPU_BO_GPU_VISIBLE
        | ATRIUM_GPU_BO_CPU_VISIBLE
        | ATRIUM_GPU_BO_COHERENT
        | ATRIUM_GPU_BO_SCANOUT;
    let mut bo = gpu.alloc(bytes, flags)?;

    /* ── Vulkan renderer ─────────────────────────────────────────── */
    let mut renderer = HeadlessRenderer::new(mode.width, mode.height)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other,
            format!("HeadlessRenderer::new: {e}")))?;
    let bundle_path = bundle_path()?;
    renderer.load_bundle(&bundle_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other,
            format!("load_bundle: {e}")))?;
    eprintln!("frescod: atrium-core bundle loaded ({} ops)", renderer.op_count());

    /* ── Shared scene-server state ───────────────────────────────── */
    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));

    /* DisplayEvent sink: compositor pushes events; fan-out thread
     * encodes + broadcasts to all per-connection writer mpscs. */
    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx);

    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));

    /* ── Socket server ───────────────────────────────────────────── */
    let sock_path = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/frescod.sock"));
    let event_subs = socket_server::spawn(
        socket_server::Shared { frontend: frontend.clone() },
        &sock_path,
    )?;
    socket_server::spawn_event_fanout(ev_rx, event_subs);

    /* ── Frame loop ─────────────────────────────────────────────── */
    let mut frame: u64 = 0;
    /* First frame: render once + SET_MODE before flipping. */
    render_one_frame(&mut renderer, &frontend, &comp, mode.width, mode.height)
        .map_err(io_other)?;
    copy_renderer_to_bo(&renderer, &mut bo).map_err(io_other)?;
    dpy.set_mode(conn.id, &bo, mode)?;
    dpy.page_flip(conn.id, &bo)?;
    frame += 1;

    let mut next = Instant::now() + Duration::from_nanos(FRAME_NS);
    loop {
        render_one_frame(&mut renderer, &frontend, &comp, mode.width, mode.height)
            .map_err(io_other)?;
        copy_renderer_to_bo(&renderer, &mut bo).map_err(io_other)?;
        dpy.page_flip(conn.id, &bo)?;
        frame = frame.wrapping_add(1);
        let _ = frame;

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += Duration::from_nanos(FRAME_NS);
    }
}

/// Snapshot the current per-window scene states, merge into a single
/// rect/texture set with per-window position offsets applied, then
/// drive one `HeadlessRenderer::render_to_buffer()` cycle.
fn render_one_frame(
    renderer: &mut HeadlessRenderer,
    frontend: &Arc<Mutex<EnvelopeFrontend>>,
    comp:     &Arc<Mutex<Compositor>>,
    _screen_w: u32, _screen_h: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    /* Snapshot WM state to (id, pos) pairs, in z-order bottom→top. The
     * implicit screen window (id 0) has pos (0,0); content windows
     * carry their drag-set position. */
    let layers: Vec<(u32, (f32, f32))> = {
        let g = comp.lock().unwrap();
        let mut out = Vec::with_capacity(g.windows.len());
        /* Always include window 0 first (background scene). */
        out.push((0u32, (0.0, 0.0)));
        for &id in &g.z_order {
            if id == 0 { continue; }
            if let Some(w) = g.windows.get(&id) {
                out.push((id as u32, (w.pos.0, w.pos.1)));
            }
        }
        out
    };

    let mut rects: Vec<SceneNode> = Vec::new();
    let mut tex_by_slot: std::collections::HashMap<u32, Vec<TextureNode>> =
        std::collections::HashMap::new();
    {
        let fe = frontend.lock().unwrap();
        for (win_id, (ox, oy)) in &layers {
            let Some(state) = fe.window_state(*win_id) else { continue; };
            for p in state.rect_nodes.values() {
                rects.push(translate_rect(p, *ox, *oy));
            }
            for p in state.texture_nodes.values() {
                tex_by_slot.entry(p.slot_id).or_default()
                    .push(translate_texture(p, *ox, *oy));
            }
        }
    }
    let batches: Vec<TextureBatch> = tex_by_slot.into_iter()
        .map(|(slot_id, nodes)| TextureBatch { slot_id, nodes })
        .collect();

    renderer.set_rect_nodes(rects);
    renderer.set_texture_batches(batches);
    renderer.render_to_buffer()?;
    Ok(())
}

fn translate_rect(p: &RectParams, ox: f32, oy: f32) -> SceneNode {
    SceneNode {
        position: [p.x + ox, p.y + oy],
        size:     [p.w, p.h],
        color:    [p.r, p.g, p.b, p.a],
    }
}

fn translate_texture(p: &TextureParams, ox: f32, oy: f32) -> TextureNode {
    TextureNode { model: [p.x + ox, p.y + oy, p.w, p.h] }
}

/// Pull the rendered framebuffer out of `renderer` (BGRA8) and copy
/// directly into the scanout BO. The kmod's scanout format is BGRA8 —
/// matching HeadlessRenderer's color attachment — so this is a memcpy.
fn copy_renderer_to_bo(
    renderer: &HeadlessRenderer,
    bo:       &mut atrium_gpu::Bo,
) -> Result<(), Box<dyn std::error::Error>> {
    let dst = bo.as_mut_slice();
    renderer.read_pixels(dst)?;
    Ok(())
}

fn bundle_path() -> std::io::Result<PathBuf> {
    if let Ok(p) = std::env::var("FRESCOD_BUNDLE") {
        return Ok(PathBuf::from(p));
    }
    /* Default search order:
     *   1. ../bundles/atrium-core      (workspace dev layout)
     *   2. /usr/local/share/atrium/bundles/atrium-core (installed) */
    let candidates = [
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
            .join("bundles/atrium-core"),
        PathBuf::from("/usr/local/share/atrium/bundles/atrium-core"),
    ];
    for c in &candidates {
        if c.join("compute/op_rectangle.comp.spv").exists() {
            return Ok(c.clone());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "atrium-core bundle not found; set FRESCOD_BUNDLE or build bundles/atrium-core",
    ))
}

fn io_other<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
