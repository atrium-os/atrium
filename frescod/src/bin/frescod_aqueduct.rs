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
//!
//! # Rendering pipeline (current state)
//!
//! Triple-buffered scanout ring, vblank-paced, queued page-flip.
//! See `docs/spec/aqueduct-gpu.md` §6.5.5 (a, b, c — all landed).
//!
//! - **Triple-buffer ring** (§6.5.5.a): three scanout BOs, round-
//!   robin advance on real flips, keepalive flips re-assert the
//!   current scanout BO. Eliminates render-into-live-BO tearing.
//! - **Vblank pacing** (§6.5.5.b): `dpy.wait_vblank(conn.id)` once
//!   per loop iteration in place of wall-clock sleep — frame loop
//!   is phase-locked to the kmod's emulated vblank cadence.
//! - **Queued page-flip** (§6.5.5.c): `dpy.page_flip_queued`
//!   records the request in the kmod's per-connector slot; a
//!   taskqueue worker fires SET_SCANOUT_BLOB + RESOURCE_FLUSH at
//!   the next vblank tick. Render(N+1) overlaps with the kmod's
//!   deferred flip of frame N.
//!
//! # Skip hierarchy (§6.5.6 — all landed)
//!
//! 1. **Per-screen flip skip.** No `page_flip` when composite bytes
//!    AND per-window dirty status are both unchanged.
//! 2. **Per-window rasterise skip.** Each window has its own
//!    offscreen `WindowSurface`; non-dirty windows skip submit +
//!    wait + readback. A final composite pass textured-rects every
//!    visible surface onto the screen target.
//! 3. **Intra-window dirty rect.** Per-node hash + bbox tracking;
//!    if the damage rect covers less than
//!    `FRESCOD_DAMAGE_THRESHOLD` (default 0.5) of the window area,
//!    emit `BEGIN_RENDERPASS_NO_CLEAR` + `SET_SCISSOR(damage)`.
//!
//! # Environment variables
//!
//! - `FRESCOD_SOCK` — Unix socket path for fresco-protocol clients
//!   (default `/tmp/frescod.sock`).
//! - `FRESCOD_AQUEDUCT_SOCK` — Unix socket path for the in-process
//!   aqueduct-gpu listener (default `/tmp/frescod-aqueduct.sock`).
//! - `FRESCOD_UNCAPPED=1` — disable vblank pacing (benchmark mode).
//! - `FRESCOD_DAMAGE_THRESHOLD=<f32>` — override the level-3
//!   partial-redraw threshold (0.0..=1.0; 0.0 disables level-3).

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

/// `FRESCOD_UNCAPPED=1` to disable the display-rate cap (benchmark mode).
fn uncapped() -> bool {
    std::env::var("FRESCOD_UNCAPPED").map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

/// Frame interval to pace at, derived from the connector's reported
/// refresh rate (mHz → ns). Falls back to 60 Hz if the kmod returns
/// something unusable.
fn frame_interval_ns(refresh_mhz: u32) -> u64 {
    if refresh_mhz < 1000 {
        return 1_000_000_000 / 60; // sane fallback
    }
    // refresh_mhz / 1000 = Hz; 1e9 / Hz = ns per frame
    1_000_000_000_000 / (refresh_mhz as u64)
}

/// VRR keepalive interval. Even when the scene is fully static the
/// kmod's page-flip cadence should not stall indefinitely (some
/// connectors require periodic refresh to maintain sync). Emit one
/// "redundant" flip per this many real refresh intervals when idle.
const VRR_KEEPALIVE_INTERVALS: u32 = 60;

fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    // ── Display + scanout BO (same as venus path) ────────────────
    let gpu = Gpu::open()?;
    let dpy = Display::open()?;
    dpy.bind(&gpu)?;
    let connectors = dpy.connectors()?;
    let conn = connectors.first().expect("at least one connector").clone();
    let mode = dpy.preferred_mode(conn.id)?;
    let frame_ns = frame_interval_ns(mode.refresh_mhz);
    let target_fps = 1_000_000_000.0 / (frame_ns as f64);
    eprintln!(
        "frescod-aqueduct: connector {} {}x{} @ {} mHz, pacing at {:.1} fps",
        conn.id, mode.width, mode.height, mode.refresh_mhz, target_fps,
    );

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let flags = ATRIUM_GPU_BO_GPU_VISIBLE
        | ATRIUM_GPU_BO_CPU_VISIBLE
        | ATRIUM_GPU_BO_COHERENT
        | ATRIUM_GPU_BO_SCANOUT;
    // ── Triple-buffered scanout ──────────────────────────────────
    //
    // Three scanout BOs used round-robin on real flips. Eliminates
    // render-into-live-BO tearing structurally — we never mutate
    // the BO the kmod is reading.
    //
    // Slot semantics:
    //   bos[next_render_idx]  — about-to-be-written by the next
    //                           dirty render
    //   bos[last_flipped_idx] — currently being scanned out;
    //                           keepalive flips re-assert this one
    //   the third slot        — "settled" from a previous flip,
    //                           safe to ignore
    //
    // Depends on kmod page_flip rebinding the connector's scanout
    // when the supplied BO differs from the bound one (Phase 1.5b.a
    // in `docs/spec/aqueduct-gpu.md` §6.5.5.a). Without that fix,
    // the connector would keep scanning out bos[0] no matter which
    // BO we hand to page_flip → second-frame black.
    //
    // Real vsync (kqueue on vblank-fd) still deferred to §6.5.5.b.
    // Today's pacing remains wall-clock against mode.refresh_mhz.
    let mut bos: [atrium_gpu::Bo; 3] = [
        gpu.alloc(bytes, flags)?,
        gpu.alloc(bytes, flags)?,
        gpu.alloc(bytes, flags)?,
    ];
    // Triple-buffer ring indices. Set by the first-frame block
    // below (declared `mut` because the main loop rotates them on
    // every real flip). Avoiding `= 0` initialization here keeps
    // rustc from flagging the first-frame writes as dead.
    let mut next_render_idx: usize;
    let mut last_flipped_idx: usize;

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

    // Per-window dirty tracking + per-window offscreen surfaces.
    //
    // For each window the renderer:
    //   1. Builds a "mini-frame" of just that window's nodes in
    //      window-local coordinates.
    //   2. Byte-compares against the prior frame's mini-frame for
    //      the same window.
    //   3. If different (or first-seen), submits a render into the
    //      window's offscreen image (window-sized, persistent).
    //   4. Otherwise leaves the window's image intact — its pixels
    //      from a previous frame are still valid.
    //
    // A final composite pass then textured-rects every visible
    // window's surface onto the screen target at its (pos, size).
    // The whole-screen skip path (composite bytes unchanged AND no
    // window dirtied this frame) lets us elide the page-flip too.
    let mut window_surfaces: HashMap<u32, WindowSurface> = HashMap::new();
    // Initialised by the first-frame block below; main loop reassigns
    // on every real flip. See next_render_idx above for the rationale.
    let mut last_composite_bytes: Vec<u8>;
    let mut frames_since_real_flip: u32 = 0;

    // First frame: render into bos[0]; set mode + first flip against it.
    {
        let mut p = FrameProfile::default();
        timeline += 1;
        let (composite, _) = render_one_frame_multipass(
            &mut client, &sw_backend, &frontend, &comp,
            target, fence, &mut timeline,
            &mut slot_images, &mut window_surfaces,
            mode.width, mode.height,
            &mut bos[0], &mut p,
            /* force_full = */ true,
        ).map_err(io_other)?;
        last_composite_bytes = composite;
    }
    dpy.set_mode(conn.id, &bos[0], mode)?;
    dpy.page_flip(conn.id, &bos[0])?;
    last_flipped_idx = 0;
    next_render_idx = 1;

    // Rolling FPS counter. Once per FPS_REPORT_SECS we log the
    // window's frame count + min/avg/max render-time + per-phase
    // breakdown + skip-ratio (scene-unchanged fast path).
    const FPS_REPORT_SECS: u64 = 5;
    let mut window_start = Instant::now();
    let mut window_frames: u32 = 0;     // total iterations (incl. skipped)
    let mut window_flips:  u32 = 0;     // real submits + page-flips
    let mut window_render_min = Duration::from_secs(60);
    let mut window_render_max = Duration::ZERO;
    let mut window_render_sum = Duration::ZERO;
    let mut window_profile = FrameProfile::default();

    loop {
        let iter_t0 = Instant::now();
        let mut prof = FrameProfile::default();
        timeline += 1;

        // Render into bos[next_render_idx]. The kmod is scanning
        // bos[last_flipped_idx]; the third slot is settled.
        let (composite, any_dirty) = render_one_frame_multipass(
            &mut client, &sw_backend, &frontend, &comp,
            target, fence, &mut timeline,
            &mut slot_images, &mut window_surfaces,
            mode.width, mode.height,
            &mut bos[next_render_idx], &mut prof,
            /* force_full = */ false,
        ).map_err(io_other)?;

        let layout_changed = composite != last_composite_bytes;
        let need_keepalive = frames_since_real_flip >= VRR_KEEPALIVE_INTERVALS;
        let real_flip = any_dirty || layout_changed;

        if real_flip && !composite.is_empty() {
            // Queue the flip for the next vblank tick — kmod's
            // taskqueue worker performs the SET_SCANOUT_BLOB +
            // RESOURCE_FLUSH at panel-refresh boundary. Returns
            // immediately so the next render can overlap with
            // vblank wait. See aqueduct-gpu.md §6.5.5.c.
            dpy.page_flip_queued(conn.id, &bos[next_render_idx], timeline)?;
            last_flipped_idx = next_render_idx;
            next_render_idx = (next_render_idx + 1) % bos.len();
            last_composite_bytes = composite;
            frames_since_real_flip = 0;
            window_flips += 1;
        } else if need_keepalive {
            // VRR keepalive — re-queue the current scanout BO. Same-BO
            // case in the worker hits the fast path (no SET_SCANOUT,
            // just RESOURCE_FLUSH).
            dpy.page_flip_queued(conn.id, &bos[last_flipped_idx], timeline)?;
            frames_since_real_flip = 0;
            window_flips += 1;
        } else {
            frames_since_real_flip += 1;
        }

        let render_dur = iter_t0.elapsed();
        window_frames += 1;
        window_render_sum += render_dur;
        if render_dur < window_render_min { window_render_min = render_dur; }
        if render_dur > window_render_max { window_render_max = render_dur; }
        window_profile.add(&prof);
        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(FPS_REPORT_SECS) {
            let secs = elapsed.as_secs_f64();
            let total_fps = window_frames as f64 / secs;
            let flip_fps = window_flips as f64 / secs;
            let n = window_frames.max(1) as f64;
            let avg_ms = window_render_sum.as_secs_f64() * 1000.0 / n;
            let skip_ratio = 1.0 - (window_flips as f64 / window_frames.max(1) as f64);
            eprintln!(
                "frescod-aqueduct: {:.1} iter/s ({:.1} flips/s, {:.0}% skipped) \
                 over {:.1}s; loop time min/avg/max = {:.2}/{:.2}/{:.2} ms",
                total_fps, flip_fps, skip_ratio * 100.0, secs,
                window_render_min.as_secs_f64() * 1000.0,
                avg_ms,
                window_render_max.as_secs_f64() * 1000.0,
            );
            eprintln!(
                "frescod-aqueduct:   per-phase avg ms: uploads={:.2} \
                 build={:.2} submit={:.2} wait={:.2} readback={:.2} bo-copy={:.2}",
                window_profile.uploads.as_secs_f64() * 1000.0 / n,
                window_profile.build.as_secs_f64() * 1000.0 / n,
                window_profile.submit.as_secs_f64() * 1000.0 / n,
                window_profile.wait.as_secs_f64() * 1000.0 / n,
                window_profile.readback.as_secs_f64() * 1000.0 / n,
                window_profile.copy_to_bo.as_secs_f64() * 1000.0 / n,
            );
            let total_dirty = window_profile.partial_passes + window_profile.full_passes;
            let partial_pct = if total_dirty > 0 {
                100.0 * window_profile.partial_passes as f64 / total_dirty as f64
            } else { 0.0 };
            eprintln!(
                "frescod-aqueduct:   damage-rect: {} partial, {} full ({:.0}% partial)",
                window_profile.partial_passes,
                window_profile.full_passes,
                partial_pct,
            );
            window_start = Instant::now();
            window_frames = 0;
            window_flips = 0;
            window_render_min = Duration::from_secs(60);
            window_render_max = Duration::ZERO;
            window_render_sum = Duration::ZERO;
            window_profile = FrameProfile::default();
        }

        if !uncapped() {
            // Phase-lock to the kmod's vblank tick. The kmod's
            // wait_vblank ioctl blocks until the next emulated
            // vblank (callout-driven at the connector's refresh
            // interval today; will become a real IRQ on D5+).
            // Replaces wall-clock thread::sleep — see file header
            // and aqueduct-gpu.md §6.5.5.b.
            let _ = dpy.wait_vblank(conn.id);
        }
    }
}

/// Per-phase timing of one render-and-flip cycle. Summed across the
/// reporting window in `frescod-aqueduct`'s main loop.
#[derive(Default)]
struct FrameProfile {
    /// `EnvelopeFrontend::take_pending_uploads` + write_image[_region] loop.
    uploads:    Duration,
    /// Layer snapshot + bridge::translate_* into a FrameBuilder.
    build:      Duration,
    /// `client.submit_frame` (postcard encode + Unix-socket write).
    submit:     Duration,
    /// `client.wait_fence`.
    wait:       Duration,
    /// `sw_backend.read_image_pixels` (clones the Pixmap data).
    readback:   Duration,
    /// BGRA-swap copy from readback buffer into the scanout BO.
    copy_to_bo: Duration,
    /// Per-window dirty-render passes this frame that took the
    /// intra-window damage-rect path (skip-hierarchy level 3).
    partial_passes: u32,
    /// Per-window dirty-render passes this frame that took the full
    /// window clear-and-redraw path.
    full_passes:    u32,
}

impl FrameProfile {
    fn add(&mut self, other: &FrameProfile) {
        self.uploads    += other.uploads;
        self.build      += other.build;
        self.submit     += other.submit;
        self.wait       += other.wait;
        self.readback   += other.readback;
        self.copy_to_bo += other.copy_to_bo;
        self.partial_passes += other.partial_passes;
        self.full_passes    += other.full_passes;
    }
}

/// Cached state for one fresco SlotTable slot's atlas/texture image
/// on the aqueduct-gpu side.
struct SlotImage {
    image: ResourceId,
    width: u32,
    height: u32,
}

/// Per-window persistent offscreen surface. Each window's nodes are
/// rasterised into its own image (window-local coords). The final
/// composite pass textured-rects each surface onto the screen target.
///
/// Lets the render loop skip rasterisation of un-dirty windows. Only
/// the window whose scene state changed pays the per-glyph cost.
struct WindowSurface {
    image: ResourceId,
    width: u32,
    height: u32,
    /// Last frame's mini-FrameOp byte stream for this window. Used to
    /// detect dirty status by byte-compare.
    last_bytes: Vec<u8>,
    /// Per-node hash + bbox snapshot from the previous frame.
    /// Key: (class_tag, node_id). Used to compute an intra-window
    /// damage rect for partial redraw (skip-hierarchy level 3).
    /// `class_tag` is 0=rect, 1=texture, 2=path, 3=glyph_run.
    prev_nodes: HashMap<(u8, u32), (u64, [f32; 4])>,
}

/// Damage-rect partial-redraw threshold. If the union of changed node
/// bboxes is smaller than this fraction of the window area, the
/// renderer is asked to redraw only that scissor; otherwise we use
/// the normal full-window clear path.
///
/// 0.5 = sub-half-window changes go partial; bigger ones clear. The
/// breakeven for tiny-skia is somewhere around 0.3–0.6 depending on
/// content; 0.5 is the safe default and was easy to verify by eye.
const DAMAGE_RECT_THRESHOLD_DEFAULT: f32 = 0.5;

/// Read the damage-rect threshold from `FRESCOD_DAMAGE_THRESHOLD` if
/// set (clamped to (0.0, 1.0]); else the compile-time default.
/// Lets perf A/B testing tune the level-3 partial-redraw breakeven
/// without rebuilding. `0.0` disables level-3 entirely (everything
/// goes through the clear path); `1.0` always goes partial when any
/// damage is detected.
fn damage_rect_threshold() -> f32 {
    std::env::var("FRESCOD_DAMAGE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)
        .unwrap_or(DAMAGE_RECT_THRESHOLD_DEFAULT)
}

/// Hash a slice of f32 / u32 fields treated as raw little-endian
/// bytes. f32 bits-equal comparison is what we want for "did this
/// node's params change?" — NaN-equality / +0/-0 quirks are moot
/// because params are produced deterministically by the client.
fn hash_words(words: &[u32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for w in words {
        w.hash(&mut h);
    }
    h.finish()
}

fn rect_hash(p: &fp::RectParams) -> u64 {
    hash_words(&[
        p.x.to_bits(), p.y.to_bits(), p.w.to_bits(), p.h.to_bits(),
        p.r.to_bits(), p.g.to_bits(), p.b.to_bits(), p.a.to_bits(),
    ])
}
fn rect_bbox(p: &fp::RectParams) -> [f32; 4] {
    [p.x, p.y, p.x + p.w, p.y + p.h]
}

fn texture_hash(p: &fp::TextureParams) -> u64 {
    hash_words(&[
        p.x.to_bits(), p.y.to_bits(), p.w.to_bits(), p.h.to_bits(),
        p.slot_id,
    ])
}
fn texture_bbox(p: &fp::TextureParams) -> [f32; 4] {
    [p.x, p.y, p.x + p.w, p.y + p.h]
}

fn path_hash(p: &fp::PathParams) -> u64 {
    hash_words(&[
        p.cx.to_bits(), p.cy.to_bits(), p.length.to_bits(),
        p.width.to_bits(), p.angle.to_bits(),
        p.r.to_bits(), p.g.to_bits(), p.b.to_bits(), p.a.to_bits(),
    ])
}
fn path_bbox(p: &fp::PathParams) -> [f32; 4] {
    // Conservative AABB of a rotated rect: half-diagonal radius
    // around (cx, cy).
    let half_diag = ((p.length * 0.5).powi(2) + (p.width * 0.5).powi(2)).sqrt();
    [p.cx - half_diag, p.cy - half_diag, p.cx + half_diag, p.cy + half_diag]
}

fn glyph_run_hash(p: &fp::GlyphRunParams) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.x.to_bits().hash(&mut h);
    p.y.to_bits().hash(&mut h);
    p.atlas_slot_id.hash(&mut h);
    p.atlas_width.hash(&mut h);
    p.atlas_height.hash(&mut h);
    p.r.to_bits().hash(&mut h);
    p.g.to_bits().hash(&mut h);
    p.b.to_bits().hash(&mut h);
    p.a.to_bits().hash(&mut h);
    for g in &p.glyphs {
        g.dx.to_bits().hash(&mut h);
        g.dy.to_bits().hash(&mut h);
        g.atlas_u.hash(&mut h);
        g.atlas_v.hash(&mut h);
        g.atlas_w.hash(&mut h);
        g.atlas_h.hash(&mut h);
        g.bearing_x.to_bits().hash(&mut h);
        g.bearing_y.to_bits().hash(&mut h);
    }
    h.finish()
}
fn glyph_run_bbox(p: &fp::GlyphRunParams) -> [f32; 4] {
    if p.glyphs.is_empty() {
        return [p.x, p.y, p.x, p.y];
    }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for g in &p.glyphs {
        let dst_x = p.x + g.dx + g.bearing_x;
        let dst_y = p.y + g.dy - g.bearing_y;
        let dx2 = dst_x + g.atlas_w as f32;
        let dy2 = dst_y + g.atlas_h as f32;
        if dst_x < min_x { min_x = dst_x; }
        if dst_y < min_y { min_y = dst_y; }
        if dx2   > max_x { max_x = dx2; }
        if dy2   > max_y { max_y = dy2; }
    }
    [min_x, min_y, max_x, max_y]
}

/// Union two AABBs in (x0, y0, x1, y1) form.
fn bbox_union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// Clip an AABB to a window (0, 0, w, h) and snap to integer pixel
/// extents. Returns None if the rect is empty after clipping.
fn clip_and_snap(bb: [f32; 4], win_w: u32, win_h: u32) -> Option<(u32, u32, u32, u32)> {
    let x0 = bb[0].floor().max(0.0) as i64;
    let y0 = bb[1].floor().max(0.0) as i64;
    let x1 = bb[2].ceil().min(win_w as f32) as i64;
    let y1 = bb[3].ceil().min(win_h as f32) as i64;
    if x1 <= x0 || y1 <= y0 { return None; }
    Some((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Orchestrate one frame of per-window rendering + composite.
///
/// Steps:
/// 1. Drain pending texture/atlas uploads (shared across all windows).
/// 2. Snapshot WM state into a list of (window_id, pos, size).
/// 3. Reconcile per-window surface map: allocate new windows'
///    surfaces, destroy obsolete ones (window-closed).
/// 4. For each window: build mini-frame in window-local coords;
///    byte-compare; only rasterise into its surface if dirty.
/// 5. Build the composite frame: BeginRenderPass on `target`,
///    textured-rect each visible window's surface at its (pos, size),
///    EndRenderPass.
/// 6. If any window dirtied this frame (or `force_full`): submit
///    composite, wait fence, read back, BGRA-copy into the BO.
///    Otherwise skip the readback chain — the BO still holds the
///    last good composite.
///
/// Returns `(composite_bytes, any_window_rasterised)`. The caller's
/// outer skip-flip condition needs BOTH:
///
///   - `composite_bytes` catches LAYOUT changes (window moved/resized,
///     window list mutated). The composite FrameOp stream encodes
///     (target, image_id, dst_rect) per window; if any of those change
///     the bytes differ.
///   - `any_window_rasterised` catches CONTENT changes within a stable
///     layout. The composite command stream is identical when only
///     window contents change (same textured-rect ops referencing
///     same image_ids), but the BO has fresh pixels and must be
///     flipped.
#[allow(clippy::too_many_arguments)]
fn render_one_frame_multipass(
    client: &mut GpuClient,
    sw_backend: &SoftwareBackend,
    frontend: &Arc<Mutex<EnvelopeFrontend>>,
    comp: &Arc<Mutex<Compositor>>,
    target: ResourceId,
    fence: ResourceId,
    timeline: &mut u64,
    slot_images: &mut HashMap<u32, SlotImage>,
    window_surfaces: &mut HashMap<u32, WindowSurface>,
    screen_w: u32,
    screen_h: u32,
    bo: &mut atrium_gpu::Bo,
    prof: &mut FrameProfile,
    force_full: bool,
) -> Result<(Vec<u8>, bool), Box<dyn std::error::Error>> {
    // ── 1. Drain pending texture uploads → write_image into the
    //       backend's per-slot Pixmaps.
    let t_uploads = Instant::now();
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
            UploadRequest::TextureRegion {
                slot_id, bytes, dst_x, dst_y, width, height,
            } => {
                let Some(slot) = slot_images.get(&slot_id) else {
                    log::debug!("frescod-aqueduct: TextureRegion for unknown slot {slot_id}; \
                                 skipping (no prior Texture upload)");
                    continue;
                };
                let row_pitch = width * 4;
                let pixels = premultiply_for_tiny_skia(&bytes);
                client.write_image_region(
                    slot.image,
                    dst_x, dst_y, width, height,
                    row_pitch, pixels,
                ).map_err(|e| io_other(format!(
                    "write_image_region slot {slot_id}: {e:?}"
                )))?;
            }
        }
    }

    prof.uploads += t_uploads.elapsed();

    // ── 2. Snapshot per-window WM state in z-order bottom→top.
    //       Each entry: (window_id, pos, size). Window 0 (background)
    //       spans the whole screen.
    let t_build = Instant::now();
    let layers: Vec<(u32, (f32, f32), (u32, u32))> = {
        let g = comp.lock().unwrap();
        let mut out = Vec::with_capacity(g.windows.len());
        out.push((0u32, (0.0, 0.0), (screen_w, screen_h)));
        for &id in &g.z_order {
            if id == 0 { continue; }
            if let Some(w) = g.windows.get(&id) {
                let sw = (w.size.0.max(1.0) as u32).min(screen_w);
                let sh = (w.size.1.max(1.0) as u32).min(screen_h);
                out.push((id as u32, (w.pos.0, w.pos.1), (sw, sh)));
            }
        }
        out
    };

    // ── 3. Reconcile per-window surface map: drop surfaces for
    //       closed windows or windows whose size changed.
    let live_ids: std::collections::HashSet<u32> = layers.iter().map(|(id, ..)| *id).collect();
    let to_destroy: Vec<u32> = window_surfaces.keys()
        .filter(|id| !live_ids.contains(id))
        .copied()
        .collect();
    for id in to_destroy {
        if let Some(s) = window_surfaces.remove(&id) {
            let _ = client.destroy_image(s.image);
        }
    }
    for (win_id, _, (sw, sh)) in &layers {
        if let Some(existing) = window_surfaces.get(win_id) {
            if existing.width == *sw && existing.height == *sh {
                continue;
            }
            // Size changed: drop and re-create.
            let _ = client.destroy_image(existing.image);
            window_surfaces.remove(win_id);
        }
        let mem = client.allocate_memory(
            (*sw as u64) * (*sh as u64) * 4,
            MemoryUsage::ImageBacking,
        )?;
        let image = client.create_image(ImageCreatePayload {
            image_id: ResourceId(0),
            backing_region: mem.region_id, region_offset: 0,
            format: 37, width: *sw, height: *sh, depth: 1,
            mip_levels: 1, array_layers: 1, usage: 0x07,
        })?;
        window_surfaces.insert(*win_id, WindowSurface {
            image, width: *sw, height: *sh,
            last_bytes: Vec::new(),
            prev_nodes: HashMap::new(),
        });
    }

    // ── 4. Build & maybe-render each window's mini-frame.
    //       For window 0 (screen background) keep screen-space coords
    //       (nodes are already in screen coords; pos is (0,0)).
    //       For other windows render in window-local coords; the
    //       composite pass places the surface at the window's pos.
    let mut any_dirty = false || force_full;
    // (win_id, bytes, new_node_map). new_node_map is captured here
    // and stored on the surface after the dirty check so we only pay
    // the bbox/hash work once.
    let mut per_window_frames:
        Vec<(u32, Vec<u8>, HashMap<(u8, u32), (u64, [f32; 4])>)> =
        Vec::with_capacity(layers.len());
    {
        let fe = frontend.lock().unwrap();
        for (win_id, _, (sw, sh)) in &layers {
            let surface = window_surfaces.get(win_id).unwrap();

            // Build current frame's per-node hash + bbox map. Used
            // both to compute the damage rect and to roll forward
            // surface.prev_nodes after rasterise.
            let mut new_nodes: HashMap<(u8, u32), (u64, [f32; 4])> =
                HashMap::new();
            if let Some(state) = fe.window_state(*win_id) {
                for (id, p) in &state.rect_nodes {
                    new_nodes.insert((0, *id), (rect_hash(p), rect_bbox(p)));
                }
                for (id, p) in &state.texture_nodes {
                    new_nodes.insert((1, *id), (texture_hash(p), texture_bbox(p)));
                }
                for (id, p) in &state.path_nodes {
                    new_nodes.insert((2, *id), (path_hash(p), path_bbox(p)));
                }
                for (id, p) in &state.glyph_run_nodes {
                    new_nodes.insert((3, *id), (glyph_run_hash(p), glyph_run_bbox(p)));
                }
            }

            // Damage rect = union of bboxes of nodes that changed
            // (added / removed / hash-different). Compared against
            // surface.prev_nodes.
            let mut damage: Option<[f32; 4]> = None;
            for (key, (h_new, bb_new)) in &new_nodes {
                match surface.prev_nodes.get(key) {
                    Some((h_old, bb_old)) if h_old == h_new => { /* unchanged */ }
                    Some((_, bb_old)) => {
                        // Hash changed → union old + new.
                        let u = bbox_union(*bb_old, *bb_new);
                        damage = Some(match damage { Some(d) => bbox_union(d, u), None => u });
                    }
                    None => {
                        // New node this frame.
                        damage = Some(match damage {
                            Some(d) => bbox_union(d, *bb_new), None => *bb_new,
                        });
                    }
                }
            }
            for (key, (_, bb_old)) in &surface.prev_nodes {
                if !new_nodes.contains_key(key) {
                    // Node removed this frame.
                    damage = Some(match damage {
                        Some(d) => bbox_union(d, *bb_old), None => *bb_old,
                    });
                }
            }

            // Fast path: nothing in this window changed since last
            // frame. Reuse the prior mini-frame bytes verbatim so the
            // dirty byte-compare matches and we skip the submit.
            // Without this short-circuit, switching between partial
            // and full byte streams on a no-change frame would falsely
            // mark the window dirty and trigger a redundant rasterise.
            if !force_full
                && damage.is_none()
                && !surface.prev_nodes.is_empty()
            {
                per_window_frames.push(
                    (*win_id, surface.last_bytes.clone(), new_nodes),
                );
                continue;
            }

            // Decide partial vs full. Window 0 (background) and
            // forced full frames always use the clear path.
            let clip = damage.and_then(|d| clip_and_snap(d, *sw, *sh));
            let go_partial = !force_full
                && *win_id != 0
                && !surface.prev_nodes.is_empty()
                && match clip {
                    Some((_, _, dw, dh)) => {
                        let dmg_area = (dw as f32) * (dh as f32);
                        let win_area = (*sw as f32) * (*sh as f32);
                        // Cache the env-resolved threshold across frames;
                        // OnceLock::get_or_init runs the env read once.
                        static THRESH: std::sync::OnceLock<f32> =
                            std::sync::OnceLock::new();
                        let t = *THRESH.get_or_init(damage_rect_threshold);
                        win_area > 0.0 && dmg_area / win_area < t
                    }
                    None => false,
                };

            let mut fb = client.frame_builder();
            if go_partial {
                let (dx, dy, dw, dh) = clip.unwrap();
                prof.partial_passes += 1;
                fresco_aqueduct_bridge::begin_renderpass_no_clear(
                    &mut fb, surface.image,
                )?;
                fresco_aqueduct_bridge::set_scissor(&mut fb, dx, dy, dw, dh)?;
            } else {
                prof.full_passes += 1;
                fresco_aqueduct_bridge::begin_renderpass(
                    &mut fb, surface.image,
                    if *win_id == 0 { [0, 0, 0, 255] } else { [0, 0, 0, 0] },
                )?;
            }
            if let Some(state) = fe.window_state(*win_id) {
                for p in state.rect_nodes.values() {
                    fresco_aqueduct_bridge::translate_rect(&mut fb, p)?;
                }
                for p in state.path_nodes.values() {
                    fresco_aqueduct_bridge::translate_path(&mut fb, p)?;
                }
                for p in state.texture_nodes.values() {
                    let Some(slot) = slot_images.get(&p.slot_id) else { continue; };
                    fresco_aqueduct_bridge::translate_texture(
                        &mut fb, p, slot.image, slot.width, slot.height,
                    )?;
                }
                for p in state.glyph_run_nodes.values() {
                    let Some(slot) = slot_images.get(&p.atlas_slot_id) else { continue; };
                    fresco_aqueduct_bridge::translate_glyph_run(
                        &mut fb, p, slot.image,
                    )?;
                }
            }
            fresco_aqueduct_bridge::end_renderpass(&mut fb)?;
            let bytes = fb.into_buf();
            per_window_frames.push((*win_id, bytes, new_nodes));
        }
    }

    // Detect dirty windows & rasterise them. Submit one frame per
    // dirty window — they all target distinct images so this can be
    // batched in a single multi-renderpass submit, but per-window
    // submits give us the early-skip without extra plumbing.
    for (win_id, bytes, new_nodes) in per_window_frames.drain(..) {
        let surface = window_surfaces.get_mut(&win_id).unwrap();
        let dirty = force_full || bytes != surface.last_bytes;
        if dirty {
            any_dirty = true;
            *timeline += 1;
            let t_submit = Instant::now();
            let fb = aqueduct_gpu::frame::FrameBuilder::from_bytes(
                bytes.len() as u32 + 16, bytes.clone(),
            );
            client.submit_frame(fence, fb, *timeline)?;
            prof.submit += t_submit.elapsed();
            let t_wait = Instant::now();
            let _ = client.wait_fence(fence, 50_000_000)?;
            prof.wait += t_wait.elapsed();
            surface.last_bytes = bytes;
        }
        // Always roll prev_nodes forward — even on a non-dirty
        // frame the map may be empty-but-stable, and we want
        // diff-against-truth, not against stale state.
        surface.prev_nodes = new_nodes;
    }

    // ── 5. Composite pass: textured-rect each window's surface onto
    //       the screen target at (pos, size). Window 0 first (it
    //       acts as the screen background); subsequent windows
    //       layer on top in z-order.
    let mut composite = client.frame_builder();
    fresco_aqueduct_bridge::begin_renderpass(&mut composite, target, [0, 0, 0, 255])?;
    for (win_id, (ox, oy), (sw, sh)) in &layers {
        let surface = window_surfaces.get(win_id).unwrap();
        let tex = fp::TextureParams {
            x: *ox, y: *oy, w: *sw as f32, h: *sh as f32, slot_id: 0,
        };
        fresco_aqueduct_bridge::translate_texture(
            &mut composite, &tex, surface.image, *sw, *sh,
        )?;
    }
    fresco_aqueduct_bridge::end_renderpass(&mut composite)?;
    let composite_bytes = composite.into_buf();
    prof.build += t_build.elapsed();

    // ── 6. Submit composite + read back to BO only when something
    //       changed. The outer skip path (composite bytes unchanged)
    //       still handles "no flip needed" at the page-flip layer.
    if any_dirty {
        *timeline += 1;
        let t_submit = Instant::now();
        let fb = aqueduct_gpu::frame::FrameBuilder::from_bytes(
            composite_bytes.len() as u32 + 16, composite_bytes.clone(),
        );
        client.submit_frame(fence, fb, *timeline)?;
        prof.submit += t_submit.elapsed();
        let t_wait = Instant::now();
        let _ = client.wait_fence(fence, 50_000_000)?;
        prof.wait += t_wait.elapsed();
        copy_backend_to_bo_profiled(sw_backend, target, bo, prof)?;
    }

    Ok((composite_bytes, any_dirty))
}

/// Read tier-1 SW backend's target Pixmap → BGRA-swap → scanout BO.
///
/// Caller passes a FrameProfile to attribute time between readback
/// (the Pixmap → owned Vec clone inside read_image_pixels) and the
/// BGRA-swap copy proper. Useful for steering tier-1 perf work —
/// the clone is a known unnecessary copy if we add a direct-borrow
/// accessor.
fn copy_backend_to_bo_profiled(
    sw: &SoftwareBackend,
    target: ResourceId,
    bo: &mut atrium_gpu::Bo,
    prof: &mut FrameProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    let t_readback = Instant::now();
    let pixels = sw.read_image_pixels(target)
        .ok_or_else(|| io_other("SoftwareBackend missing target image"))?;
    prof.readback += t_readback.elapsed();

    let t_copy = Instant::now();
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
    prof.copy_to_bo += t_copy.elapsed();
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
