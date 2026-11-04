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
//!   atrium_gpu::amd::Scanout (CPU staging BO -> CP DMA -> one of two VRAM BOs;
//!   with FRESCOD_RENDERER=gpu the RDNA4 compute kernels write the VRAM BO
//!   directly, see render_backend.rs)
//!   ↓ (the copy signals a syncobj)
//!   atrium_gpu::amd::Display::commit (BO fd, in-fence = the copy, out-fence = flip done)
//!
//! Double-buffered: each frame goes into the buffer that is not on screen,
//! the flip waits for the copy (in-fence) and latches at vblank, and the next
//! frame doesn't start until the previous flip landed (out-fence). That paces
//! presents to the display's refresh, never faster, without a timer.
//!
//! The legacy tiny-skia / 128-byte / CommandFrontend path was removed
//! at the M2.7 cutover. Today this binary is single-display.
//!
//! Multi-window composition: per frame, walk WM z-order; for each
//! window, take its `WindowSceneState` rect/texture nodes, translate
//! by `window.pos`, accumulate into one merged rect/texture set, then
//! issue one `HeadlessRenderer::render_to_buffer()`. Texture slot
//! collisions across windows are not yet handled (M3+); typical
//! single-window apps render correctly.

mod injector_reader;
mod input_reader;
mod laminar;
mod pack;
mod pointer_reader;
mod redraw;
mod render_backend;
mod socket_server;
mod split;

use atrium_gpu::amd::{Display, Gpu, Scanout, COMMIT_MODESET};

use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;
use fresco_vulkan::HeadlessRenderer;

use render_backend::{Backend, ScreenInput, WindowFrame};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Scanout buffers: one on screen, one being filled.
const SCANOUT_BUFFERS: usize = 2;
/// How long to wait for a flip to land before giving up on the display.
/// FRESCOD_FLIP_TIMEOUT_MS overrides it: a GPU renderer on gpusim takes
/// tens of seconds per 1080p frame.
const FLIP_TIMEOUT_MS: u32 = 2000;

fn flip_timeout_ms() -> u32 {
    std::env::var("FRESCOD_FLIP_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(FLIP_TIMEOUT_MS)
}

fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    // Headless compositor mode: same multi-window pipeline, but read the composited
    // frame back to a PNG instead of scanning out — captures the assembled (jailed)
    // desktop with no GPU/display device. Used for the end-to-end jailed-desktop run.
    if let Ok(png) = std::env::var("FRESCOD_HEADLESS_PNG") {
        return run_headless(&png);
    }

    /* ── Display + scanout (GPU ABI 'A' + display ABI v2) ────────────
     * The GPU and display are decoupled and share only fds: the compositor
     * renders into a Scanout (System staging BO -> CP DMA copy -> VRAM BO)
     * and commits the VRAM BO to the display by its fd. */
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;

    let conn = dpy.connector()?;
    let modes = dpy.modes()?;
    let mode = *modes.first().expect("at least one mode");
    eprintln!(
        "frescod: connector type {} {}x{} @ {} mHz, {} scanout buffers",
        conn.connector_type, mode.width, mode.height, mode.refresh_mhz, SCANOUT_BUFFERS,
    );

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let scan = Scanout::with_buffers(&vm, bytes, SCANOUT_BUFFERS)?;
    /* copy_done reaches N when frame N's pixels are in VRAM; flip_done
     * reaches N when frame N is on screen (and frame N-1's buffer is free). */
    let copy_done = gpu.create_syncobj()?;
    let flip_done = gpu.create_syncobj()?;

    /* Frame deadline = one vblank interval (refresh is milli-Hz, so the period in
     * ns is 1e12 / refresh_mHz). The compositor stamps its scanout copy with this
     * so the GPU scheduler serves it ahead of background work — it makes its
     * vblank under contention (the GPU side of the frame-pacing deadline, matching
     * the CPU-side vblank deadline frescod already sponsors). */
    let frame_deadline_ns: u32 = if mode.refresh_mhz > 0 {
        (1_000_000_000_000u64 / u64::from(mode.refresh_mhz)).min(u32::MAX as u64) as u32
    } else {
        16_000_000 // ~60 Hz fallback
    };
    /* Reusable CPU framebuffer the renderer paints into each frame, then
     * uploaded into the VRAM scanout via scan.update (DMA copy). */
    let mut framebuffer = vec![0u8; bytes as usize];

    /* ── Renderer (FRESCOD_RENDERER: vulkan | compute) ───────────── */
    let mut renderer = Backend::from_env(mode.width, mode.height, Some(&vm), load_bundles)
        .map_err(io_other)?;
    eprintln!("frescod: renderer {}", renderer.name());

    /* ── Shared scene-server state ───────────────────────────────── */
    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));

    /* DisplayEvent sink: compositor pushes window events, input_reader
     * pushes keyboard events; fan-out thread encodes + broadcasts to
     * all per-connection writer mpscs. Senders are cheaply cloneable
     * so each producer keeps its own. */
    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx.clone());

    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));
    /* Publish the mode we just read from the connector, so clients can ASK
     * (OP_DISPLAY_INFO) rather than each assuming a screen size. */
    frontend.lock().unwrap()
        .set_display_mode(mode.width, mode.height, mode.refresh_mhz);

    /* ── Socket server ───────────────────────────────────────────── */
    let sock_path = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco/fresco.sock"));
    let wm_sock = Some(std::env::var("FRESCOD_WM_SOCK").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco-wm/fresco-wm.sock")));
    /* Task #25: damage signal shared by every producer (socket ops,
     * input, compositor events) and consumed by the display loop so it
     * recomposes on change instead of spinning at TARGET_FPS. */
    let redraw = redraw::RedrawSignal::new();
    let split = split::Broker::from_env(comp.clone(), redraw.clone(), (mode.width, mode.height, mode.refresh_mhz));

    let event_subs = socket_server::spawn(
        socket_server::Shared {
            frontend: frontend.clone(),
            lane: None,
            redraw: redraw.clone(),
            hand_off: split.as_ref().map(|b| b.hand_off()),
        },
        &sock_path,
        wm_sock.as_deref(),
    )?;
    socket_server::spawn_event_fanout(ev_rx, event_subs, redraw.clone(), comp.clone());

    /* Native FreeBSD keyboard + pointer input. Both read /dev/hidraw*
     * directly (boot-protocol HID), update server cursor / focus state,
     * and push DisplayEvents through the shared event sink. Fail-soft:
     * frescod runs without input if no matching /dev/hidraw* is found. */
    input_reader::spawn(ev_tx.clone(), comp.clone());
    pointer_reader::spawn(ev_tx.clone(), comp.clone(), mode.width, mode.height);
    /* Synthetic input injection (dev/test harness): feeds the same event sink as
     * the HID readers, so scripted input routes through hit-test/focus identically.
     * Lets us drive the input loop without an HID device. */
    if let Ok(sock) = std::env::var("FRESCOD_INPUT_SOCK") {
        injector_reader::spawn(ev_tx, comp.clone(), mode.width, mode.height, sock);
    }

    /* ── Frame loop ─────────────────────────────────────────────── */
    /* Frame 1 lights the CRTC with buffer 0 (MODESET is immediate). */
    let mut frame: u64 = 1;
    let mut on_screen: usize = 0;
    render_one_frame(&mut renderer, &frontend, &comp, split.as_deref(), mode.width, mode.height)
        .map_err(io_other)?;
    renderer.present(&scan, on_screen, &mut framebuffer, frame_deadline_ns, (&copy_done, frame))
        .map_err(io_other)?;
    check_fault("modeset", dpy.commit(
        scan.buffer(on_screen), COMMIT_MODESET, 0,
        Some((&copy_done, frame)), Some((&flip_done, frame)),
    ).or_else(|e| {
        /* MODESET doesn't wait for its in-fence: if the copy is still
         * running, wait for it and retry once. */
        if e.raw_os_error() == Some(libc::EAGAIN) {
            copy_done.wait(frame, flip_timeout_ms())?;
            dpy.commit(scan.buffer(on_screen), COMMIT_MODESET, 0, None, Some((&flip_done, frame)))
        } else {
            Err(e)
        }
    })?)?;

    /* ── Damage-driven present (task #25) ────────────────────────────
     * Recompose + flip only when a producer signalled `redraw`; otherwise
     * block. The display engine scans out the shared VRAM BO on its own,
     * so a static screen needs no frescod work. Bursts of damage between
     * frame boundaries coalesce into one render (last_gen jumps ahead),
     * capping present rate at TARGET_FPS. IDLE_HEARTBEAT re-renders as a
     * safety net against any damage source not wired to `redraw`. */
    const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);
    /* The heartbeat is a SAFETY NET, and it must never become the
     * workload: on a slow renderer (lavapipe in a VM renders a frame
     * in whole seconds) a fixed 1 s heartbeat re-renders back-to-back
     * forever — a treadmill that pegged every vCPU and starved sshd.
     * Scale the interval to the measured render cost so idle overhead
     * is bounded (~1/8 of one core worst-case); fast hardware keeps
     * the 1 s beat. */
    const HEARTBEAT_COST_FACTOR: u32 = 8;
    let mut heartbeat = IDLE_HEARTBEAT;
    let mut slow_render_warned = false;
    let mut last_gen: u64 = 0;              /* 0 != initial 1 -> render frame 0 */
    /* Force the first present immediately (heartbeat treated as already due). */
    let mut last_render = Instant::now()
        .checked_sub(IDLE_HEARTBEAT)
        .unwrap_or_else(Instant::now);
    loop {
        let cur = redraw.current();
        let heartbeat_due = last_render.elapsed() >= heartbeat;
        if cur == last_gen && !heartbeat_due {
            /* Nothing changed since the last present — sleep until the
             * next damage signal, or until the heartbeat comes due,
             * consuming no CPU meanwhile. The bounded wait is the safety
             * net: even a damage source not wired to `redraw` refreshes
             * within `heartbeat`. */
            let until_beat =
                heartbeat.saturating_sub(last_render.elapsed());
            redraw.wait_past(last_gen, until_beat);
            continue;
        }

        /* Pace on the display: the previous frame must be on screen before
         * this one starts, both because its flip released the buffer we are
         * about to fill and because nothing presents faster than the
         * refresh. Damage that lands meanwhile coalesces into this frame. */
        flip_done.wait(frame, flip_timeout_ms())?;

        /* Snapshot the generation AFTER the wait so damage during it is
         * folded into this present rather than triggering an extra one. */
        last_gen = redraw.current();
        let t_render = Instant::now();
        render_one_frame(&mut renderer, &frontend, &comp, split.as_deref(), mode.width, mode.height)
            .map_err(io_other)?;
        frame += 1;
        let back = (on_screen + 1) % scan.len();
        renderer.present(&scan, back, &mut framebuffer, frame_deadline_ns, (&copy_done, frame))
            .map_err(io_other)?;
        check_fault("flip", dpy.commit(
            scan.buffer(back), 0, 0,
            Some((&copy_done, frame)), Some((&flip_done, frame)),
        )?)?;
        on_screen = back;

        let cost = t_render.elapsed();
        heartbeat = IDLE_HEARTBEAT.max(cost * HEARTBEAT_COST_FACTOR);
        if heartbeat > IDLE_HEARTBEAT && !slow_render_warned {
            slow_render_warned = true;
            eprintln!(
                "frescod: slow renderer (frame took {cost:?}); \
                 idle heartbeat backed off to {heartbeat:?}"
            );
        }

        last_render = Instant::now();
    }
}

/// Headless compositor: the same multi-window socket-server + compositing pipeline
/// as the display path, but reads the composited screen back to a PNG each frame
/// instead of scanning out. No GPU/display device needed — so the full jailed
/// desktop (forum-wm + jailed chrome) can be composited and captured anywhere.
fn run_headless(png: &str) -> std::io::Result<()> {
    const W: u32 = 1280;
    const H: u32 = 720;

    let mut renderer = Backend::from_env(W, H, None, load_bundles).map_err(io_other)?;
    eprintln!("frescod-headless: renderer {}", renderer.name());

    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx.clone());
    /* Synthetic input injection — the headless interactive harness. No HID device
     * needed: scripted input over a socket feeds the same event sink + hit-test as
     * real /dev/hidraw input, so we can drive + debug the input→WM→render loop. */
    if let Ok(isock) = std::env::var("FRESCOD_INPUT_SOCK") {
        injector_reader::spawn(ev_tx.clone(), comp.clone(), W, H, isock);
    }
    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));
    /* Headless has no connector, but clients still need an answer — the surface
     * it renders into IS the screen here. Refresh 0 says "no real mode". */
    frontend.lock().unwrap().set_display_mode(W, H, 0);

    let sock = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco/fresco.sock"));
    let wm_sock = Some(std::env::var("FRESCOD_WM_SOCK").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco-wm/fresco-wm.sock")));
    let redraw2 = redraw::RedrawSignal::new();
    let split = split::Broker::from_env(comp.clone(), redraw2.clone(), (W, H, 0));
    let subs = socket_server::spawn(
        socket_server::Shared {
            frontend: frontend.clone(),
            lane: None,
            redraw: redraw2.clone(),
            hand_off: split.as_ref().map(|b| b.hand_off()),
        },
        &sock, wm_sock.as_deref())?;
    socket_server::spawn_event_fanout(ev_rx, subs, redraw2.clone(), comp.clone());
    eprintln!("frescod-headless: {W}x{H} on {} → {png}.png", sock.display());

    /* Damage-driven, like the display path — this loop used to render
     * + PNG-encode unconditionally at 5 Hz (encode alone costs ~1.5 s
     * under a loaded VM, and the PNG lands on the 9p share), which
     * pegged the VM whenever a jailed-desktop harness was left
     * running. Renders only when a client op or input event signals
     * damage; a long heartbeat covers unwired sources. */
    /* The capture PNG going stale between damage events is harmless
     * (it's a debug/e2e artifact), so the safety-net heartbeat is long
     * and — like the display path — scales with measured render cost:
     * a slow renderer must never turn the safety net into the load. */
    const HEADLESS_HEARTBEAT: Duration = Duration::from_secs(60);
    const HEADLESS_MIN_INTERVAL: Duration = Duration::from_millis(200);
    let out = format!("{png}.png");
    let mut heartbeat = HEADLESS_HEARTBEAT;
    let mut last_gen: u64 = 0;
    let mut last_render = Instant::now()
        .checked_sub(HEADLESS_HEARTBEAT)
        .unwrap_or_else(Instant::now);
    loop {
        let cur = redraw2.current();
        let heartbeat_due = last_render.elapsed() >= heartbeat;
        if cur == last_gen && !heartbeat_due {
            let until_beat = heartbeat.saturating_sub(last_render.elapsed());
            redraw2.wait_past(last_gen, until_beat);
            continue;
        }
        /* Coalesce bursts: floor the interval between captures. */
        let since = last_render.elapsed();
        if since < HEADLESS_MIN_INTERVAL {
            std::thread::sleep(HEADLESS_MIN_INTERVAL - since);
        }

        last_gen = redraw2.current();
        let t_render = Instant::now();
        render_one_frame(&mut renderer, &frontend, &comp, split.as_deref(), W, H).map_err(io_other)?;
        let mut rgba = vec![0u8; (W * H * 4) as usize];
        renderer.read_pixels(&mut rgba).map_err(io_other)?;
        for px in rgba.chunks_exact_mut(4) { px.swap(0, 2); } // BGRA → RGBA
        if let Some(img) = image::RgbaImage::from_raw(W, H, rgba) {
            let tmp = format!("{out}.new");
            if img.save_with_format(&tmp, image::ImageFormat::Png).is_ok() {
                let _ = std::fs::rename(&tmp, &out);
            }
        }
        heartbeat = HEADLESS_HEARTBEAT.max(t_render.elapsed() * 8);
        last_render = Instant::now();
    }
}

/// Snapshot the current per-window scene states, one [`WindowFrame`] per
/// visible window bottom to top, each in its own coordinates, and render
/// the screen with the selected backend. The compute and gpu backends draw
/// each window into its own surface and compose them (threat model
/// §2.5.2); Vulkan merges them into one scene.
fn render_one_frame(
    renderer: &mut Backend<'_>,
    frontend: &Arc<Mutex<EnvelopeFrontend>>,
    comp:     &Arc<Mutex<Compositor>>,
    split:    Option<&split::Broker>,
    screen_w: u32, screen_h: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(b) = split {
        b.sync();
    }
    /* Snapshot WM state to (id, pos, size), in z-order bottom→top. The
     * implicit screen window (id 0) is the whole screen; a content
     * window with no size yet gets the screen's, as before surfaces. */
    let full = (screen_w as f32, screen_h as f32);
    let layers: Vec<(u32, (f32, f32), (f32, f32))> = {
        let g = comp.lock().unwrap();
        let mut out = Vec::with_capacity(g.windows.len());
        /* Always include window 0 first (background scene). */
        out.push((0u32, (0.0, 0.0), full));
        for &id in &g.z_order {
            if id == 0 { continue; }
            if let Some(w) = g.windows.get(&id) {
                /* Skip surfaces the WM gated (OP_WM_SET_RENDERING): a
                 * fully-occluded surface contributes nothing, so don't
                 * compose it — its GPU work for this frame is elided. */
                if !w.rendering { continue; }
                let size = if w.size.0 >= 1.0 && w.size.1 >= 1.0 { w.size } else { full };
                out.push((id as u32, (w.pos.0, w.pos.1), size));
            }
        }
        out
    };

    let mut windows: Vec<WindowFrame> = Vec::with_capacity(layers.len());
    {
        let fe = frontend.lock().unwrap();
        for (win_id, pos, size) in &layers {
            /* A window a renderer draws (split mode) comes as its last
             * checked frame, at the size the broker gave it. */
            if let Some(f) = split.and_then(|b| b.frame(*win_id)) {
                windows.push(WindowFrame {
                    id: *win_id,
                    pos: *pos,
                    size: (f.header.width, f.header.height),
                    frame: Some(f),
                    ..Default::default()
                });
            } else if let Some(w) = pack::window_frame(&fe, *win_id, *pos, *size) {
                windows.push(w);
            }
        }
    }

    let (uploads, clears) = {
        let mut fe = frontend.lock().unwrap();
        fe.take_pending_uploads()
    };
    /* What compose puts on screen, for pointer hit-testing (F4c): each
     * window at its rounded position, at its surface's size. */
    let rects: Vec<(u32, [i32; 4])> = windows.iter().map(|w| {
        let (x, y) = (w.pos.0.round() as i32, w.pos.1.round() as i32);
        let (sw, sh) = render_backend::surface_size(w);
        (w.id, [x, y, x.saturating_add(sw as i32), y.saturating_add(sh as i32)])
    }).collect();
    renderer.render(ScreenInput { windows, uploads, clears })?;
    /* Each layer's alpha comes from the frame it was drawn from, so a
     * pixel a window left transparent passes the click on (F4c′). */
    let frames: std::collections::HashMap<u32, Arc<fresco_compute::Frame>> =
        renderer.frames().iter().cloned().collect();
    let shown = rects.into_iter().map(|(id, rect)| {
        let alpha = frames.get(&id).cloned().map(|f| -> fresco_scene_server::window::AlphaProbe {
            Arc::new(move |x, y| fresco_compute::reference::alpha_at(&f, x, y))
        });
        fresco_scene_server::window::ShownLayer { id: id as u16, rect, alpha }
    }).collect();
    comp.lock().unwrap().shown = Some(shown);
    Ok(())
}

/// A non-zero display fault code (from a commit) is an error.
fn check_fault(op: &str, fault: u32) -> std::io::Result<()> {
    if fault != 0 {
        Err(io_other(format!("display {op} fault={fault}")))
    } else {
        Ok(())
    }
}

/// Discover bundles to load. atrium-core is mandatory; atrium-text is
/// optional (only fails if FRESCOD_BUNDLES explicitly names it).
///
/// FRESCOD_BUNDLES, colon-separated paths, takes precedence; otherwise
/// FRESCOD_BUNDLE (legacy single-path) is honoured for atrium-core and
/// atrium-text is searched for next to it.
fn bundle_paths() -> std::io::Result<Vec<PathBuf>> {
    if let Ok(list) = std::env::var("FRESCOD_BUNDLES") {
        let v: Vec<PathBuf> = list.split(':').filter(|s| !s.is_empty())
            .map(PathBuf::from).collect();
        if !v.is_empty() { return Ok(v); }
    }

    let mut out = Vec::new();

    let core_candidates: Vec<PathBuf> = if let Ok(p) = std::env::var("FRESCOD_BUNDLE") {
        vec![PathBuf::from(p)]
    } else {
        vec![
            Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
                .join("bundles/atrium-core"),
            PathBuf::from("/usr/local/share/atrium/bundles/atrium-core"),
        ]
    };
    let core = core_candidates.iter().find(|c|
        c.join("compute/op_rectangle.comp.spv").exists()
    ).ok_or_else(|| std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "atrium-core bundle not found; set FRESCOD_BUNDLE/FRESCOD_BUNDLES",
    ))?.clone();
    out.push(core.clone());

    /* atrium-text is best-effort: looked for as a sibling of atrium-core. */
    let text_candidates = [
        core.parent().map(|d| d.join("atrium-text")),
        Some(PathBuf::from("/usr/local/share/atrium/bundles/atrium-text")),
    ];
    for c in text_candidates.into_iter().flatten() {
        if c.join("compute/op_glyph_run.comp.spv").exists() {
            out.push(c);
            break;
        }
    }
    Ok(out)
}

/// Load the op bundles into the Vulkan renderer.
fn load_bundles(r: &mut HeadlessRenderer) -> Result<(), Box<dyn std::error::Error>> {
    for bp in bundle_paths()? {
        r.load_bundle(&bp).map_err(|e| format!("load_bundle({}): {e}", bp.display()))?;
        eprintln!("frescod: bundle loaded {}", bp.display());
    }
    eprintln!("frescod: total ops registered: {}", r.op_count());
    Ok(())
}

fn io_other<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
