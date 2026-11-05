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
//!   the default compute renderer and FRESCOD_RENDERER=gpu replace the Vulkan
//!   stage; gpu's kernels, compiled for the device's architecture, write the
//!   VRAM BO directly, see render_backend.rs)
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
mod gpu_app;
mod chrome;
mod screen;
mod hdr;

use atrium_gpu::amd::{Display, EventWatch, Gpu, ModeV2, Scanout, MODE_PREFERRED, NOTE_HOTPLUG, NOTE_RESET};

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

    // The app GPU channel alone, with no display or renderer: for testing
    // the channel (atrium-gpu-fuzz) on a machine that cannot compose.
    if std::env::var_os("FRESCOD_GPU_ONLY").is_some() {
        let p = std::env::var("FRESCOD_GPU_SOCK").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco-gpu/fresco-gpu.sock"));
        eprintln!("frescod: WARNING: FRESCOD_GPU_ONLY: serving only the GPU channel, no display");
        gpu_app::serve(&p)?;
        loop {
            std::thread::park();
        }
    }

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

    /* The mode: FRESCOD_MODE (WxH or WxH@Hz) if the monitor offers it,
     * else its preferred one. A WM can switch later (OP_WM_SET_DISPLAY_MODE),
     * and a hotplug re-picks. */
    let conn = dpy.connector_v2()?;
    let mut mode_index = find_mode(&conn.modes, wanted_mode())
        .or_else(|| preferred_mode(&conn.modes))
        .ok_or_else(|| io_other("display: no monitor, or its EDID lists no modes"))?;
    let mut mode = conn.modes[mode_index];
    /* Which monitor the mode was chosen for: a hotplug that brings the same
     * one back keeps its mode, a different one starts at its own choice. */
    let mut monitor_edid = conn.edid.clone();
    eprintln!(
        "frescod: connector type {} {}x{} @ {} mHz ({} of {} modes), {} scanout buffers",
        conn.connector_type, mode.width, mode.height, mode.refresh_mhz, mode_index + 1,
        conn.modes.len(), SCANOUT_BUFFERS,
    );
    screen::set(mode.width, mode.height, mode.refresh_mhz);

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let mut scan = Scanout::with_buffers(&vm, bytes, SCANOUT_BUFFERS)?;
    /* copy_done reaches N when frame N's pixels are in VRAM; flip_done
     * reaches N when frame N is on screen (and frame N-1's buffer is free). */
    let copy_done = gpu.create_syncobj()?;
    let flip_done = gpu.create_syncobj()?;

    /* Frame deadline = one vblank interval (refresh is milli-Hz, so the period in
     * ns is 1e12 / refresh_mHz). The compositor stamps its scanout copy with this
     * so the GPU scheduler serves it ahead of background work — it makes its
     * vblank under contention (the GPU side of the frame-pacing deadline, matching
     * the CPU-side vblank deadline frescod already sponsors). */
    let mut frame_deadline_ns = frame_deadline(mode.refresh_mhz);
    /* Reusable CPU framebuffer the renderer paints into each frame, then
     * uploaded into the VRAM scanout via scan.update (DMA copy). */
    let mut framebuffer = vec![0u8; bytes as usize];

    /* ── Renderer (FRESCOD_RENDERER: compute (default) | gpu | vulkan) ── */
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
    publish_modes(&frontend, &conn.modes, Some(mode_index));

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
    let split = split::Broker::from_env(comp.clone(), redraw.clone(), (mode.width, mode.height, mode.refresh_mhz), renderer.takes_frames(), renderer.wants_pixels());

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
    if let Some(b) = &split {
        frontend.lock().unwrap().set_reserved_top(chrome::STRIP_H as u32);
        b.set_subs(event_subs.clone());
        if let Ok(p) = std::env::var("FRESCOD_CHROME_SOCK") {
            b.serve_chrome(Path::new(&p))?;
        }
    }
    if let Ok(p) = std::env::var("FRESCOD_GPU_SOCK") {
        gpu_app::serve(Path::new(&p))?;
    }
    socket_server::spawn_event_fanout(ev_rx, event_subs, redraw.clone(), comp.clone());

    /* Native FreeBSD keyboard + pointer input. Both read /dev/hidraw*
     * directly (boot-protocol HID), update server cursor / focus state,
     * and push DisplayEvents through the shared event sink. Fail-soft:
     * frescod runs without input if no matching /dev/hidraw* is found. */
    let ev_main = ev_tx.clone();	/* EV_DISPLAY_CHANGED on a mode switch */
    input_reader::spawn(ev_tx.clone(), comp.clone());
    pointer_reader::spawn(ev_tx.clone(), comp.clone(), mode.width, mode.height);
    /* Synthetic input injection (dev/test harness): feeds the same event sink as
     * the HID readers, so scripted input routes through hit-test/focus identically.
     * Lets us drive the input loop without an HID device. */
    if let Ok(sock) = std::env::var("FRESCOD_INPUT_SOCK") {
        injector_reader::spawn(ev_tx, comp.clone(), mode.width, mode.height, sock);
    }

    /* ── Device resets (GPU ABI v2.1 §10.3) ──────────────────────────
     * A device reset can wipe VRAM, the frame on screen included. The
     * display tells us with a NOTE_RESET kqueue event; a thread waits on it
     * and wakes the frame loop, which re-presents at once instead of at the
     * next heartbeat. The gpu renderer falls back to Tier-2 for the
     * recovery, since its GPU state may be gone, and goes back to the GPU
     * once no reset has come for GPU_BACK_AFTER. */
    /* Hotplug (display ABI 2.1): a monitor came, went or was swapped. The
     * same thread counts those; the frame loop re-reads the connector and
     * picks a mode the new monitor offers. */
    const GPU_BACK_AFTER: Duration = Duration::from_secs(5);
    let reset_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hotplugs = Arc::new(std::sync::atomic::AtomicU64::new(0));
    match EventWatch::new(&dpy, NOTE_RESET | NOTE_HOTPLUG) {
        Ok(watch) => {
            let (gen, hp, rd) = (reset_gen.clone(), hotplugs.clone(), redraw.clone());
            std::thread::Builder::new().name("display-events".into()).spawn(move || loop {
                match watch.wait(None) {
                    Ok(Some(ev)) => {
                        if ev.reset {
                            gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        if ev.hotplug {
                            hp.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        rd.wake();
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("frescod: display event watch stopped: {e}");
                        return;
                    }
                }
            })?;
        }
        Err(e) => eprintln!("frescod: no display events ({e}); a reset repaints at the next heartbeat, hotplug is not seen"),
    }
    let mut handled_gen: u64 = 0;
    let mut handled_hotplugs: u64 = 0;
    let mut gpu_back_at: Option<Instant> = None;

    /* ── Frame loop ─────────────────────────────────────────────── */
    /* Frame 1 lights the CRTC with buffer 0 (MODESET is immediate). */
    let mut frame: u64 = 1;
    let mut on_screen: usize = 0;
    render_one_frame(&mut renderer, &frontend, &comp, split.as_deref(), mode.width, mode.height)
        .map_err(io_other)?;
    let hdr_enc = hdr::Encoder::new();
    let mut hdr_on = apply_hdr(&dpy, &conn, renderer.can_hdr(), None);
    renderer.present(&scan, on_screen, &mut framebuffer, frame_deadline_ns, (&copy_done, frame),
        hdr_on.then_some(&hdr_enc))
        .map_err(io_other)?;
    let vrr = use_vrr(&conn, &mode);
    check_fault("modeset", dpy.modeset_vrr(
        scan.buffer(on_screen), mode_index as u32, conn.generation, 0, vrr,
        Some((&copy_done, frame)), Some((&flip_done, frame)),
    ).or_else(|e| {
        /* MODESET doesn't wait for its in-fence: if the copy is still
         * running, wait for it and retry once. */
        if e.raw_os_error() == Some(libc::EAGAIN) {
            copy_done.wait(frame, flip_timeout_ms())?;
            dpy.modeset_vrr(scan.buffer(on_screen), mode_index as u32, conn.generation, 0, vrr,
                None, Some((&flip_done, frame)))
        } else {
            Err(e)
        }
    })?)?;
    if vrr {
        let (lo, hi) = conn.vrr.unwrap_or_default();
        eprintln!("frescod: variable refresh on ({lo}-{hi} Hz): a frame goes up as soon as it is ready");
    }

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
        let gen = reset_gen.load(std::sync::atomic::Ordering::SeqCst);
        let reset_due = gen != handled_gen;
        if reset_due {
            handled_gen = gen;
            eprintln!("frescod: device reset ({gen} seen): re-presenting the screen");
            match renderer.fall_back_to_cpu(mode.width, mode.height) {
                Ok(true) => {
                    eprintln!("frescod: renderer -> {} until the GPU stays up", renderer.name());
                    gpu_back_at = Some(Instant::now() + GPU_BACK_AFTER);
                }
                Ok(false) => {
                    if gpu_back_at.is_some() {
                        gpu_back_at = Some(Instant::now() + GPU_BACK_AFTER);
                    }
                }
                Err(e) => return Err(io_other(format!("Tier-2 fallback after reset: {e}"))),
            }
        }
        if gpu_back_at.is_some_and(|t| Instant::now() >= t) {
            gpu_back_at = None;
            match renderer.back_to_gpu(&vm, mode.width, mode.height) {
                Ok(()) => eprintln!("frescod: GPU stayed up; renderer -> {}", renderer.name()),
                Err(e) => eprintln!("frescod: GPU renderer not back ({e}); staying on Tier-2"),
            }
        }
        /* A hotplug, or a WM asking for another mode: pick the mode and,
         * if it is not the lit one, switch (see switch_mode). */
        let hp = hotplugs.load(std::sync::atomic::Ordering::SeqCst);
        let request = frontend.lock().unwrap().take_mode_request();
        if hp != handled_hotplugs || request.is_some() {
            let hotplug = hp != handled_hotplugs;
            handled_hotplugs = hp;
            let conn = dpy.connector_v2()?;
            if !conn.connected {
                eprintln!("frescod: monitor unplugged; the last mode stays programmed");
                publish_modes(&frontend, &[], None);
            } else {
                let cur = (mode.width, mode.height, mode.refresh_mhz);
                let same_monitor = conn.edid == monitor_edid;
                monitor_edid = conn.edid.clone();
                /* HDR follows the monitor, whether or not the mode changes:
                 * it goes out with the next commit, so repaint in the new
                 * format at once. */
                if !same_monitor {
                    let was_hdr = hdr_on;
                    hdr_on = apply_hdr(&dpy, &conn, renderer.can_hdr(), Some(was_hdr));
                    if hdr_on != was_hdr {
                        last_gen = 0;
                    }
                }
                let pick = match request {
                    Some(r) => find_mode(&conn.modes, Some(r)),
                    /* The same monitor back: keep its mode. A new one gets
                     * FRESCOD_MODE or its preferred mode, as at startup. */
                    None if same_monitor => find_mode(&conn.modes, Some(cur))
                        .or_else(|| find_mode(&conn.modes, wanted_mode()))
                        .or_else(|| preferred_mode(&conn.modes)),
                    None => find_mode(&conn.modes, wanted_mode())
                        .or_else(|| preferred_mode(&conn.modes)),
                };
                match pick {
                    None => eprintln!("frescod: no mode to switch to ({request:?}); staying"),
                    Some(i) if conn.current == Some(i) => {
                        if hotplug {
                            eprintln!("frescod: monitor plugged in; {}x{}@{} mHz kept",
                                mode.width, mode.height, mode.refresh_mhz);
                        }
                        mode_index = i;
                        publish_modes(&frontend, &conn.modes, Some(i));
                    }
                    Some(i) => {
                        /* The last flip must be on screen first. */
                        let _ = flip_done.wait(frame, flip_timeout_ms());
                        let m = conn.modes[i];
                        let resized = (m.width, m.height) != (mode.width, mode.height);
                        let fault = if resized {
                            /* Light the new mode with fresh buffers, then
                             * rebuild the renderer for them; on a refusal the
                             * old ones stay, untouched. */
                            let bytes = u64::from(m.width) * u64::from(m.height) * 4;
                            /* No room for the new buffers (VRAM or GPU
                             * VA): refuse the switch, keep the old mode. */
                            match Scanout::with_buffers(&vm, bytes, SCANOUT_BUFFERS) {
                                Err(e) => Err(e),
                                Ok(fresh) => {
                                    let r = dpy.modeset_vrr(fresh.buffer(0), i as u32, conn.generation, 0,
                                        use_vrr(&conn, &m), None, None);
                                    if matches!(r, Ok(0)) {
                                        renderer.resize(&vm, m.width, m.height, load_bundles).map_err(io_other)?;
                                        scan = fresh;
                                        framebuffer = vec![0u8; bytes as usize];
                                        on_screen = 0;
                                    }
                                    r
                                }
                            }
                        } else {
                            dpy.modeset_vrr(scan.buffer(on_screen), i as u32, conn.generation, 0,
                                use_vrr(&conn, &m), None, None)
                        };
                        match fault {
                            Ok(0) => {
                                eprintln!("frescod: mode {}x{} @ {} mHz -> {}x{} @ {} mHz{}{}",
                                    mode.width, mode.height, mode.refresh_mhz,
                                    m.width, m.height, m.refresh_mhz,
                                    if use_vrr(&conn, &m) { ", variable refresh" } else { "" },
                                    if hotplug { " (monitor changed)" } else { "" });
                                mode = m;
                                mode_index = i;
                                frame_deadline_ns = frame_deadline(mode.refresh_mhz);
                                screen::set(mode.width, mode.height, mode.refresh_mhz);
                                let info = {
                                    let mut f = frontend.lock().unwrap();
                                    f.set_display_mode(mode.width, mode.height, mode.refresh_mhz);
                                    f.display_info()
                                };
                                publish_modes(&frontend, &conn.modes, Some(i));
                                let _ = ev_main.send(fresco_scene_server::window::DisplayEvent::DisplayChanged {
                                    width: info.width, height: info.height,
                                    refresh_mhz: info.refresh_mhz, reserved_top: info.reserved_top,
                                });
                                last_gen = 0; /* repaint at once */
                            }
                            Ok(f) => eprintln!("frescod: mode switch refused (fault {f}); staying"),
                            Err(e) => eprintln!("frescod: mode switch failed ({e}); staying"),
                        }
                        continue;
                    }
                }
            }
        }
        let _ = mode_index;

        let cur = redraw.current();
        let heartbeat_due = last_render.elapsed() >= heartbeat;
        if cur == last_gen && !heartbeat_due && !reset_due {
            /* Nothing changed since the last present — sleep until the
             * next damage signal, or until the heartbeat comes due,
             * consuming no CPU meanwhile. The bounded wait is the safety
             * net: even a damage source not wired to `redraw` refreshes
             * within `heartbeat`. */
            let mut until_beat =
                heartbeat.saturating_sub(last_render.elapsed());
            if let Some(t) = gpu_back_at {
                until_beat = until_beat.min(t.saturating_duration_since(Instant::now()));
            }
            redraw.wait_past(last_gen, until_beat);
            continue;
        }

        /* Pace on the display: the previous frame must be on screen before
         * this one starts, both because its flip released the buffer we are
         * about to fill and because nothing presents faster than the
         * refresh. Damage that lands meanwhile coalesces into this frame. */
        if let Err(e) = flip_done.wait(frame, flip_timeout_ms()) {
            /* The reset may have taken the last flip's copy with it; the
             * display kept its buffer, so go on and commit a fresh frame. */
            if !reset_due {
                return Err(e);
            }
            eprintln!("frescod: frame {frame} did not land across the reset ({e}); presenting anew");
        }

        /* Snapshot the generation AFTER the wait so damage during it is
         * folded into this present rather than triggering an extra one. */
        last_gen = redraw.current();
        let t_render = Instant::now();
        render_one_frame(&mut renderer, &frontend, &comp, split.as_deref(), mode.width, mode.height)
            .map_err(io_other)?;
        frame += 1;
        let back = (on_screen + 1) % scan.len();
        renderer.present(&scan, back, &mut framebuffer, frame_deadline_ns, (&copy_done, frame),
            hdr_on.then_some(&hdr_enc))
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
    let split = split::Broker::from_env(comp.clone(), redraw2.clone(), (W, H, 0), renderer.takes_frames(), renderer.wants_pixels());
    let subs = socket_server::spawn(
        socket_server::Shared {
            frontend: frontend.clone(),
            lane: None,
            redraw: redraw2.clone(),
            hand_off: split.as_ref().map(|b| b.hand_off()),
        },
        &sock, wm_sock.as_deref())?;
    if let Some(b) = &split {
        frontend.lock().unwrap().set_reserved_top(chrome::STRIP_H as u32);
        b.set_subs(subs.clone());
        if let Ok(p) = std::env::var("FRESCOD_CHROME_SOCK") {
            b.serve_chrome(Path::new(&p))?;
        }
    }
    if let Ok(p) = std::env::var("FRESCOD_GPU_SOCK") {
        gpu_app::serve(Path::new(&p))?;
    }
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
            if let Some(p) = split.and_then(|b| b.pixels(*win_id)) {
                windows.push(WindowFrame {
                    id: *win_id,
                    pos: *pos,
                    size: (p.width, p.height),
                    pixels: Some(p),
                    ..Default::default()
                });
            } else if let Some(f) = split.and_then(|b| b.frame(*win_id)) {
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
    let mut rects: Vec<(u32, [i32; 4])> = windows.iter().map(|w| {
        let (x, y) = (w.pos.0.round() as i32, w.pos.1.round() as i32);
        let (sw, sh) = render_backend::surface_size(w);
        (w.id, [x, y, x.saturating_add(sw as i32), y.saturating_add(sh as i32)])
    }).collect();
    /* Trusted chrome (F4d), split mode: frescod's own layer, on top. */
    if let Some(b) = split {
        /* The focus read and released first: origin() takes the broker's
         * window locks, which a renderer's thread takes before comp. */
        let focus = comp.lock().unwrap().focus;
        let mut c = chrome::build(screen_w, focus, |id| b.origin(id), &rects);
        c.size.1 = c.size.1.min(screen_h);
        rects.push((c.id, [0, 0, c.size.0 as i32, c.size.1 as i32]));
        windows.push(c);
    }
    /* A window drawn as pixels (split pixel mode) answers from them. */
    let drawn: std::collections::HashMap<u32, Arc<pack::Pixels>> =
        windows.iter().filter_map(|w| w.pixels.clone().map(|p| (w.id, p))).collect();
    renderer.render(ScreenInput { windows, uploads, clears })?;
    /* Each layer's alpha comes from the frame it was drawn from, so a
     * pixel a window left transparent passes the click on (F4c′). */
    let frames: std::collections::HashMap<u32, Arc<fresco_compute::Frame>> =
        renderer.frames().iter().cloned().collect();
    let shown = rects.into_iter().map(|(id, rect)| {
        let alpha = if let Some(p) = drawn.get(&id).cloned() {
            Some(Arc::new(move |x, y| p.alpha_at(x, y)) as fresco_scene_server::window::AlphaProbe)
        } else {
            frames.get(&id).cloned().map(|f| -> fresco_scene_server::window::AlphaProbe {
                Arc::new(move |x, y| fresco_compute::reference::alpha_at(&f, x, y))
            })
        };
        fresco_scene_server::window::ShownLayer { id: id as u16, rect, alpha }
    }).collect();
    comp.lock().unwrap().shown = Some(shown);
    Ok(())
}

/// The frame deadline: one refresh interval, in ns (refresh is milli-Hz).
fn frame_deadline(refresh_mhz: u32) -> u32 {
    if refresh_mhz > 0 {
        (1_000_000_000_000u64 / u64::from(refresh_mhz)).min(u32::MAX as u64) as u32
    } else {
        16_000_000 // ~60 Hz fallback
    }
}

/// FRESCOD_MODE: `WxH` or `WxH@Hz` (Hz may have decimals: 59.94).
fn wanted_mode() -> Option<(u32, u32, u32)> {
    let v = std::env::var("FRESCOD_MODE").ok()?;
    let parsed = parse_mode(&v);
    if parsed.is_none() {
        eprintln!("frescod: FRESCOD_MODE={v}: want WxH or WxH@Hz; ignored");
    }
    parsed
}

fn parse_mode(v: &str) -> Option<(u32, u32, u32)> {
    let (size, hz) = match v.split_once('@') {
        Some((s, h)) => (s, Some(h)),
        None => (v, None),
    };
    let (w, h) = size.split_once('x')?;
    let mhz = match hz {
        Some(h) => (h.trim().parse::<f64>().ok()? * 1000.0).round() as u32,
        None => 0,
    };
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?, mhz))
}

/// Signal HDR10 and send frames as 10-bit PQ (hdr.rs) when FRESCOD_HDR=1,
/// the renderer's frames pass through the CPU, and the monitor takes PQ in
/// BT.2020; otherwise SDR in XRGB8888. Takes effect with the next commit.
/// Returns whether HDR is on; logs when that differs from `was`.
fn apply_hdr(dpy: &Display, conn: &atrium_gpu::amd::ConnectorV2, can: bool, was: Option<bool>) -> bool {
    use atrium_gpu::amd::{Hdr, COLORIMETRY_BT2020, EOTF_PQ, FORMAT_XRGB2101010, FORMAT_XRGB8888};
    let want = std::env::var("FRESCOD_HDR").is_ok_and(|v| v == "1");
    if want && conn.hdr && can {
        if let Ok((_, caps)) = dpy.hdr() {
            if caps.eotfs & (1 << EOTF_PQ) != 0 && caps.colorimetry & (1 << COLORIMETRY_BT2020) != 0 {
                /* SDR content: nothing brighter than reference white. */
                let white = hdr::SDR_WHITE_NITS as u16;
                let peak = if caps.max_nits == 0 { 1000 } else { caps.max_nits.min(10000) as u16 };
                if dpy.set_hdr(&Hdr::hdr10(peak, white, white)).is_ok() {
                    dpy.set_format(FORMAT_XRGB2101010);
                    if was != Some(true) {
                        eprintln!("frescod: HDR10 on (monitor peak {} cd/m2): the desktop at {white} cd/m2 white, 10-bit PQ",
                            caps.max_nits);
                    }
                    return true;
                }
            }
        }
    }
    if want && was != Some(false) {
        eprintln!("frescod: HDR asked for but {}; SDR",
            if !can { "the gpu renderer draws straight to scanout" } else { "the monitor doesn't take HDR10" });
    }
    let _ = dpy.set_hdr(&Hdr::default());
    dpy.set_format(FORMAT_XRGB8888);
    false
}

/// The listed mode matching (width, height, refresh mHz): refresh 0 takes
/// the first at that size, else the closest within 1 Hz.
/// Run `mode` with variable refresh: the monitor has a range that covers the
/// mode's refresh, and FRESCOD_VRR isn't 0. Frames then go up as soon as they
/// are rendered (no waiting for the next fixed refresh slot), and an idle
/// screen refreshes at the monitor's floor.
fn use_vrr(conn: &atrium_gpu::amd::ConnectorV2, mode: &ModeV2) -> bool {
    if std::env::var("FRESCOD_VRR").is_ok_and(|v| v == "0") {
        return false;
    }
    let hz = (mode.refresh_mhz + 500) / 1000;
    conn.vrr.is_some_and(|(lo, hi)| (lo..=hi).contains(&hz))
}

fn find_mode(modes: &[ModeV2], want: Option<(u32, u32, u32)>) -> Option<usize> {
    let (w, h, r) = want?;
    let mut sized = modes.iter().enumerate().filter(|(_, m)| m.width == w && m.height == h);
    if r == 0 {
        return sized.next().map(|(i, _)| i);
    }
    sized.filter(|(_, m)| m.refresh_mhz.abs_diff(r) <= 1000)
        .min_by_key(|(_, m)| m.refresh_mhz.abs_diff(r))
        .map(|(i, _)| i)
}

/// The monitor's preferred mode (flagged; the first listed otherwise).
fn preferred_mode(modes: &[ModeV2]) -> Option<usize> {
    modes.iter().position(|m| m.flags & MODE_PREFERRED != 0)
        .or(if modes.is_empty() { None } else { Some(0) })
}

/// Tell the WM side what the monitor offers (`OP_WM_DISPLAY_MODES`).
fn publish_modes(frontend: &Arc<Mutex<EnvelopeFrontend>>, modes: &[ModeV2], current: Option<usize>) {
    let list = modes.iter().map(|m| fresco_protocol::WmDisplayMode {
        width: m.width,
        height: m.height,
        refresh_mhz: m.refresh_mhz,
        preferred: m.flags & MODE_PREFERRED != 0,
    }).collect();
    frontend.lock().unwrap().set_display_modes(list, current.map_or(u32::MAX, |i| i as u32));
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
