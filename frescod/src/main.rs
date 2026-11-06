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
//! at the M2.7 cutover. Several monitors make one desktop (heads.rs,
//! outputs.rs).
//!
//! Multi-window composition: per frame, walk WM z-order; for each
//! window, take its `WindowSceneState` rect/texture nodes, translate
//! by `window.pos`, accumulate into one merged rect/texture set, then
//! issue one `HeadlessRenderer::render_to_buffer()`. Texture slot
//! collisions across windows are not yet handled (M3+); typical
//! single-window apps render correctly.

mod activation;
mod credential;
mod drag;
mod notify;
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
mod heads;
mod outputs;
mod video;
mod span;

use atrium_gpu::amd::{Display, EventWatch, Gpu, ModeV2, OutputCommit, MODE_PREFERRED, NOTE_HOTPLUG, NOTE_RESET};

use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;
use fresco_vulkan::HeadlessRenderer;

use render_backend::{Backend, ScreenInput, WindowFrame};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    /* The monitors (display ABI 3.0): every connected output is a head,
     * lit together in one atomic commit (heads.rs). Output 0 takes
     * FRESCOD_MODE (WxH or WxH@Hz) if its monitor offers it; every monitor
     * else gets the mode remembered for this set of monitors, or its
     * preferred one. The desktop is the bounding box of the heads'
     * viewports (outputs.rs); the renderer draws it once per frame. A WM can
     * switch output 0's mode (OP_WM_SET_DISPLAY_MODE), and hotplugs re-pick
     * once they settle. */
    let cx = heads::Ctx { gpu: &gpu, vm: &vm, dpy: &dpy };
    let mut heads: Vec<heads::Head> = Vec::new();
    let mut remembered: HashMap<outputs::DisplaySet, outputs::Arrangement> = HashMap::new();
    /* Monitor scales the session set (OP_WM_SET_OUTPUT_SCALE), by identity. */
    let mut scales: HashMap<u64, u32> = HashMap::new();
    /* Which monitor mirrors which, remembered per set of monitors
     * (OP_WM_MIRROR_OUTPUT): (mirror, source) identities. */
    let mut mirrors_memo: HashMap<outputs::DisplaySet, Vec<(u64, u64)>> = HashMap::new();
    /* Re-modes still to do: a mirror going to its source's size, or back
     * to its own preferred mode. */
    let mut remodes: Vec<(u32, (u32, u32, u32))> = Vec::new();
    let mut tracker = outputs::Tracker::default();
    /* A fullscreen window spanning monitors (fresco-span.md), and when a
     * held-out member may rejoin. */
    let mut span: Option<span::Span> = None;
    let mut span_recheck: Option<Instant> = None;
    /* Direct scanout of a span (fresco-span.md §5): the strip shows for a
     * while after the span starts or the focus moves (`strip_until`, from
     * the focus last seen); the source of the members' last frame; and
     * what was last logged about it. */
    let mut strip_until: Option<Instant> = None;
    let mut span_focus: Option<Option<u16>> = None;
    let mut last_direct: Option<span::Direct> = None;
    let mut direct_said: Option<&'static str> = None;
    /* The gpu renderer can't encode HDR; corrected below once it exists. */
    let guess_hdr = std::env::var("FRESCOD_RENDERER").map_or(true, |v| v != "gpu");
    let seen = seen_outputs(&dpy)?;
    heads::reconfigure(&cx, &mut heads, &seen, &[], wanted_mode(), &remembered, guess_hdr)?;
    if heads.is_empty() {
        return Err(io_other("display: no monitor, or its EDID lists no modes"));
    }
    remember(&heads, &mut remembered, &mut mirrors_memo);
    let (mut dw, mut dh) = desktop_of(&heads);
    let mut refresh = fastest(&heads);
    screen::set(dw, dh, refresh);

    /* Frame deadline = one vblank interval of the fastest monitor (refresh
     * is milli-Hz, so the period in ns is 1e12 / refresh_mHz). The
     * compositor stamps its scanout copies with this so the GPU scheduler
     * serves them ahead of background work — they make their vblank under
     * contention (the GPU side of the frame-pacing deadline, matching the
     * CPU-side vblank deadline frescod already sponsors). */
    let mut frame_deadline_ns = frame_deadline(refresh);
    /* Reusable CPU framebuffer the renderer's desktop frame is read into;
     * each head's part is cropped from it and uploaded into that head's
     * VRAM scanout (DMA copy). One head uploads it whole. */
    let mut framebuffer = vec![0u8; dw as usize * dh as usize * 4];

    /* ── Renderer (FRESCOD_RENDERER: compute (default) | gpu | vulkan) ── */
    let mut renderer = Backend::from_env(dw, dh, Some(&vm), load_bundles)
        .map_err(io_other)?;
    eprintln!("frescod: renderer {}", renderer.name());
    /* The gpu renderer draws one screen straight into scanout: with several
     * monitors the desktop goes through Tier-2 instead. */
    let gpu_wanted = renderer.is_gpu();
    if heads.len() > 1 && renderer.fall_back_to_cpu(dw, dh).map_err(io_other)? {
        eprintln!("frescod: {} monitors: renderer -> {} (the gpu renderer drives one)", heads.len(), renderer.name());
    }
    if !renderer.can_hdr() {
        for h in heads.iter_mut().filter(|h| h.hdr_on) {
            h.hdr_on = apply_hdr(&dpy, h.output, &h.conn, false, Some(true));
        }
    }

    /* ── Shared scene-server state ───────────────────────────────── */
    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    comp.lock().unwrap().mint_embed_tokens = true; // limen.md §5

    /* DisplayEvent sink: compositor pushes window events, input_reader
     * pushes keyboard events; fan-out thread encodes + broadcasts to
     * all per-connection writer mpscs. Senders are cheaply cloneable
     * so each producer keeps its own. */
    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx.clone());

    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));
    frontend.lock().unwrap().set_notification_uids(Some(notify::uids().to_vec()));
    /* Publish the desktop's size, so clients can ASK (OP_DISPLAY_INFO)
     * rather than each assuming a screen size, and output 0's modes. */
    frontend.lock().unwrap().set_display_mode(dw, dh, refresh);
    publish_output0(&frontend, &dpy, &heads);
    publish_outputs(&frontend, &heads, &scales, span.as_ref());

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
    /* Video layers (fresco-video.md): composed as pixels, so only by
     * the compute renderer; with another, creating one is refused. */
    if renderer.takes_pixels() {
        let _ = fresco_scene_server::video::SINK.set(Box::new(video::Direct { redraw: redraw.clone() }));
    }
    let split = split::Broker::from_env(comp.clone(), redraw.clone(), (dw, dh, refresh), renderer.takes_frames(), renderer.wants_pixels());

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
    socket_server::install_embed_client(socket_server::Shared {
        frontend: frontend.clone(),
        lane: None,
        redraw: redraw.clone(),
        hand_off: split.as_ref().map(|b| b.hand_off()),
    }, event_subs.clone());
    socket_server::spawn_event_fanout(ev_rx, event_subs, redraw.clone(), comp.clone());
    activation::start(comp.clone());
    drag::install(comp.clone());

    /* Native FreeBSD keyboard + pointer input. Both read /dev/hidraw*
     * directly (boot-protocol HID), update server cursor / focus state,
     * and push DisplayEvents through the shared event sink. Fail-soft:
     * frescod runs without input if no matching /dev/hidraw* is found. */
    let ev_main = ev_tx.clone();	/* EV_DISPLAY_CHANGED on a mode switch */
    input_reader::spawn(ev_tx.clone(), comp.clone());
    pointer_reader::spawn(ev_tx.clone(), comp.clone(), dw, dh);
    /* Synthetic input injection (dev/test harness): feeds the same event sink as
     * the HID readers, so scripted input routes through hit-test/focus identically.
     * Lets us drive the input loop without an HID device. */
    if let Ok(sock) = std::env::var("FRESCOD_INPUT_SOCK") {
        injector_reader::spawn(ev_tx, comp.clone(), dw, dh, sock);
    }

    /* ── Device resets (GPU ABI v2.1 §10.3) ──────────────────────────
     * A device reset can wipe VRAM, the frames on screen included. The
     * display tells us with a NOTE_RESET kqueue event; a thread waits on it
     * and wakes the frame loop, which re-presents at once instead of at the
     * next heartbeat. The gpu renderer falls back to Tier-2 for the
     * recovery, since its GPU state may be gone, and goes back to the GPU
     * once no reset has come for GPU_BACK_AFTER. */
    /* Hotplug (display ABI 3.0): a monitor came, went or was swapped on
     * any output. The same thread counts those; the frame loop waits for a
     * burst to settle (outputs::SETTLE), then reconfigures the heads. */
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
    let mut hotplug_pending = false;
    let mut recheck_at: Option<Instant> = None;
    let mut gpu_back_at: Option<Instant> = None;
    let hdr_enc = hdr::Encoder::new();

    /* ── Damage-driven present (task #25) ────────────────────────────
     * Recompose + flip only when a producer signalled `redraw`; otherwise
     * block. The display engine scans out the VRAM BOs on its own, so a
     * static screen needs no frescod work. Bursts of damage between frame
     * boundaries coalesce into one render (last_gen jumps ahead), capping
     * present rate at the fastest monitor's refresh. IDLE_HEARTBEAT
     * re-renders as a safety net against any damage source not wired to
     * `redraw`.
     *
     * Each head is paced on its own: a frame goes to every head whose last
     * flip has landed; a head still showing its previous flip (a slower
     * monitor) is marked stale and gets the newest desktop frame as soon
     * as it lands, without a re-render. */
    const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);
    /* How often to look at stale heads while idle. */
    const STALE_POLL: Duration = Duration::from_millis(2);
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
    let mut last_present_ns = 0u64;
    loop {
        let gen = reset_gen.load(std::sync::atomic::Ordering::SeqCst);
        let reset_due = gen != handled_gen;
        if reset_due {
            handled_gen = gen;
            eprintln!("frescod: device reset ({gen} seen): re-presenting the screen");
            match renderer.fall_back_to_cpu(dw, dh) {
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
            if heads.len() == 1 {
                match renderer.back_to_gpu(&vm, dw, dh) {
                    Ok(()) => eprintln!("frescod: GPU stayed up; renderer -> {}", renderer.name()),
                    Err(e) => eprintln!("frescod: GPU renderer not back ({e}); staying on Tier-2"),
                }
            }
        }

        /* Hotplugs: note which heads lost their monitor at once (they stop
         * flipping), and reconfigure once the burst has settled. A WM's
         * mode request, or a grace running out, reconfigures at once. */
        let now = Instant::now();
        let hp = hotplugs.load(std::sync::atomic::Ordering::SeqCst);
        if hp != handled_hotplugs {
            handled_hotplugs = hp;
            tracker.hotplug(now);
            hotplug_pending = true;
            if let Ok(seen) = seen_outputs(&dpy) {
                mark_present(&mut heads, &seen);
                if let Some(s) = span.as_mut() {
                    span_recheck = span_attach(s, &dpy, &heads, &seen, &ev_main, now);
                }
                publish_outputs(&frontend, &heads, &scales, span.as_ref());
            }
        }
        /* A held-out span member that has stayed up rejoins. */
        if span_recheck.is_some_and(|t| now >= t) {
            span_recheck = None;
            if let (Some(s), Ok(seen)) = (span.as_mut(), seen_outputs(&dpy)) {
                span_recheck = span_attach(s, &dpy, &heads, &seen, &ev_main, now);
            }
            publish_outputs(&frontend, &heads, &scales, span.as_ref());
        }
        let mut changed = false;
        /* A WM mirroring a monitor (OP_WM_MIRROR_OUTPUT), or stopping. */
        for r in frontend.lock().unwrap().take_mirror_requests() {
            if span.is_some() {
                eprintln!("frescod: mirroring refused: a window spans monitors");
                continue;
            }
            match set_mirror(&mut heads, r.identity, r.of) {
                Ok(remode) => {
                    remodes.extend(remode);
                    heads::arrange(&mut heads, &remembered);
                    changed = true;
                }
                Err(e) => eprintln!("frescod: mirroring refused: {e}"),
            }
        }
        let mut requests: Vec<(u32, (u32, u32, u32))> = std::mem::take(&mut remodes);
        if let Some(r) = frontend.lock().unwrap().take_mode_request() {
            requests.retain(|q| q.0 != 0);
            requests.push((0, r));
        }
        let settled = hotplug_pending && tracker.settled(now);
        let regrace = recheck_at.is_some_and(|t| now >= t);
        if settled || regrace || !requests.is_empty() {
            if settled {
                hotplug_pending = false;
            }
            recheck_at = None;
            let before = outputs::display_set(heads.iter().map(|h| h.identity));
            /* Under a span the monitors go back to their places first; the
             * span is laid out again over whatever is left. */
            if let Some(s) = &span {
                s.unplace(&mut heads);
            }
            match reconfigure_heads(&cx, &mut heads, &mut tracker, &remembered, &requests, renderer.can_hdr()) {
                Ok((true, again)) => {
                    recheck_at = again;
                    changed = true;
                    /* Another set of monitors: mirror as this set was left. */
                    let set = outputs::display_set(heads.iter().map(|h| h.identity));
                    if set != before {
                        /* Exactly as remembered: mirrors this set didn't
                         * have stop (first, so a former mirror can be a
                         * source again), then the remembered ones start. */
                        if let Some(pairs) = mirrors_memo.get(&set).cloned() {
                            let ids: Vec<(u64, Option<u64>)> = heads.iter().map(|h| (h.identity, h.mirror_of)).collect();
                            for &(id, cur) in &ids {
                                let want = pairs.iter().find(|p| p.0 == id).map(|p| p.1);
                                if cur.is_some() && cur != want {
                                    if let Ok(remode) = set_mirror(&mut heads, id, 0) {
                                        remodes.extend(remode);
                                    }
                                }
                            }
                            for &(m, src) in &pairs {
                                if heads.iter().any(|h| h.identity == m && h.mirror_of != Some(src)) {
                                    if let Ok(remode) = set_mirror(&mut heads, m, src) {
                                        remodes.extend(remode);
                                    }
                                }
                            }
                            heads::arrange(&mut heads, &remembered);
                        }
                    }
                }
                Ok((false, again)) => recheck_at = again,
                Err(e) => eprintln!("frescod: display reconfiguration failed ({e}); staying"),
            }
            if let Some(mut s) = span.take() {
                match s.relayout(&mut heads).and_then(|()| {
                    s.unify_hdr(&dpy, &mut heads);
                    s.lock(&dpy, &heads)
                }) {
                    Ok(()) => {
                        frontend.lock().unwrap().set_span_moved(s.moved());
                        span = Some(s);
                    }
                    Err(e) => {
                        eprintln!("frescod: span over: {e}");
                        end_span(s, &cx, &mut heads, &mut tracker, &remembered, &frontend, &ev_main, renderer.can_hdr());
                    }
                }
                changed = true;
            }
            publish_output0(&frontend, &dpy, &heads);
        }
        /* A window spanning monitors (OP_WM_SPAN_GROUP), or a span ending:
         * Forum un-zoomed it, or the window is gone. */
        let gone = span.as_ref().is_some_and(|s| {
            u16::try_from(s.window_id).map_or(true, |id| !comp.lock().unwrap().windows.contains_key(&id))
        });
        let span_reqs = frontend.lock().unwrap().take_span_requests();
        if gone || !span_reqs.is_empty() {
            if let Some(s) = span.take() {
                end_span(s, &cx, &mut heads, &mut tracker, &remembered, &frontend, &ev_main, renderer.can_hdr());
                changed = true;
            }
            if let Some(r) = span_reqs.last().filter(|r| r.window_id != 0 && !r.members.is_empty()) {
                match start_span(r, &cx, &mut heads, &mut tracker, &remembered, &frontend, &ev_main, renderer.can_hdr()) {
                    Ok(s) => {
                        let at: Vec<String> = heads.iter().filter(|h| s.is_member(h.identity))
                            .map(|h| format!("output {} at {},{}", h.output, h.vp.x, h.vp.y)).collect();
                        eprintln!("frescod: window {} spans {}", s.window_id, at.join(", "));
                        span = Some(s);
                    }
                    Err(e) => eprintln!("frescod: span refused: {e}"),
                }
                changed = true;
            }
        }
        /* A WM setting a monitor's scale (OP_WM_SET_OUTPUT_SCALE): kept
         * by monitor, wherever it is plugged; 0 goes back to automatic. */
        for r in frontend.lock().unwrap().take_scale_requests() {
            if r.scale_milli == 0 {
                scales.remove(&r.identity);
            } else {
                scales.insert(r.identity, r.scale_milli);
            }
            eprintln!("frescod: monitor {:016x} scale {}", r.identity,
                if r.scale_milli == 0 { "automatic".to_string() } else { format!("{:.2}", f64::from(r.scale_milli) / 1000.0) });
            changed = true;
        }
        /* A WM moving monitors on the desktop (OP_WM_ARRANGE_OUTPUTS). */
        let placements = frontend.lock().unwrap().take_arrange_request();
        if let Some(pl) = placements.filter(|_| {
            if span.is_some() {
                eprintln!("frescod: monitor arrangement refused: a window spans monitors");
            }
            span.is_none()
        }) {
            match place_outputs(&mut heads, &pl) {
                Ok(()) => {
                    let at: Vec<String> = heads.iter().map(|h| format!("output {} at {},{}", h.output, h.vp.x, h.vp.y)).collect();
                    eprintln!("frescod: monitors arranged: {}", at.join(", "));
                    changed = true;
                }
                Err(e) => eprintln!("frescod: monitor arrangement refused: {e}"),
            }
        }
        if changed {
            let (nw, nh) = desktop_of(&heads);
            refresh = fastest(&heads);
            frame_deadline_ns = frame_deadline(refresh);
            if (nw, nh) != (dw, dh) {
                renderer.resize(&vm, nw, nh, load_bundles).map_err(io_other)?;
                framebuffer = vec![0u8; nw as usize * nh as usize * 4];
                (dw, dh) = (nw, nh);
            }
            if heads.len() > 1 && renderer.fall_back_to_cpu(dw, dh).map_err(io_other)? {
                eprintln!("frescod: {} monitors: renderer -> {} (the gpu renderer drives one)",
                    heads.len(), renderer.name());
            } else if heads.len() == 1 && gpu_wanted && !renderer.is_gpu() && gpu_back_at.is_none() {
                match renderer.back_to_gpu(&vm, dw, dh) {
                    Ok(()) => eprintln!("frescod: one monitor again; renderer -> {}", renderer.name()),
                    Err(e) => eprintln!("frescod: GPU renderer not back ({e}); staying on Tier-2"),
                }
            }
            /* A span's moved monitors are not an arrangement to keep. */
            if span.is_none() {
                remember(&heads, &mut remembered, &mut mirrors_memo);
            }
            screen::set(dw, dh, refresh);
            publish_outputs(&frontend, &heads, &scales, span.as_ref());
            let info = {
                let mut f = frontend.lock().unwrap();
                f.set_display_mode(dw, dh, refresh);
                f.display_info()
            };
            let _ = ev_main.send(fresco_scene_server::window::DisplayEvent::DisplayChanged {
                width: info.width, height: info.height,
                refresh_mhz: info.refresh_mhz, reserved_top: info.reserved_top,
            });
            last_gen = 0; /* repaint at once */
        }

        let multi = heads.len() > 1;
        if span.is_some() {
            let f = comp.lock().unwrap().focus;
            if span_focus != Some(f) {
                span_focus = Some(f);
                strip_until = Some(Instant::now() + span::STRIP_SHOWN);
            }
        } else {
            (span_focus, strip_until, last_direct, direct_said) = (None, None, None, None);
        }
        let strip_end_due = strip_until.is_some_and(|t| t <= Instant::now());
        let cur = redraw.current();
        let heartbeat_due = last_render.elapsed() >= heartbeat;
        /* Video (fresco-video.md §4): the next queued frame's time, and with
         * variable refresh, when slow content must go up again so each
         * frame lasts a whole number of refreshes. */
        let now_ns = video::monotonic_ns();
        let vrr_floor = heads.iter().filter(|h| h.present && h.vrr).filter_map(|h| h.conn.vrr.map(|(lo, _)| lo * 1000)).max();
        let video_at = [
            video::VIDEOS.next_due(),
            vrr_floor.and_then(|lo| video::VIDEOS.repeat_ns(lo)).map(|r| last_present_ns + r),
        ].into_iter().flatten().min();
        let video_due = video_at.is_some_and(|t| t <= now_ns);
        if cur == last_gen && !heartbeat_due && !reset_due && !video_due && !strip_end_due {
            /* Heads that missed the newest frame get it once they land. */
            let mut stale = false;
            if multi {
                let spanned = |h: &heads::Head<'_>| span.as_ref().is_some_and(|s| s.is_member(h.identity));
                for h in heads.iter_mut().filter(|h| h.present && h.stale && !spanned(h)) {
                    if h.landed() {
                        present_head(&dpy, h, None, &mut framebuffer, dw, frame_deadline_ns, &hdr_enc)?;
                    } else {
                        stale = true;
                    }
                }
                /* The span's members catch up together, once all landed. */
                let group = span.as_ref().map(|s| s.active_heads(&heads)).unwrap_or_default();
                if group.iter().any(|&k| heads[k].stale) {
                    if group.iter().all(|&k| heads[k].landed()) {
                        present_span(&dpy, &mut heads, &group, last_direct.as_ref(), &mut framebuffer, dw, frame_deadline_ns, &hdr_enc)?;
                    } else {
                        stale = true;
                    }
                }
            }
            /* Nothing changed since the last present — sleep until the
             * next damage signal, or until the heartbeat comes due,
             * consuming no CPU meanwhile. The bounded wait is the safety
             * net: even a damage source not wired to `redraw` refreshes
             * within `heartbeat`. */
            let mut until_beat =
                heartbeat.saturating_sub(last_render.elapsed());
            let now = Instant::now();
            for t in [gpu_back_at, recheck_at, span_recheck, strip_until, hotplug_pending.then(|| tracker.settles_at()).flatten()]
                .into_iter()
                .flatten()
            {
                until_beat = until_beat.min(t.saturating_duration_since(now));
            }
            if stale {
                until_beat = until_beat.min(STALE_POLL);
            }
            if let Some(t) = video_at {
                until_beat = until_beat.min(Duration::from_nanos(t.saturating_sub(now_ns)));
            }
            /* Video frames on screen once the flip carrying them lands. */
            if video::VIDEOS.awaiting() {
                match heads.iter().filter(|h| h.present).max_by_key(|h| h.mode.refresh_mhz) {
                    Some(lead) if !lead.landed() => until_beat = until_beat.min(STALE_POLL),
                    _ => video::VIDEOS.landed(video::monotonic_ns(), &comp),
                }
            }
            redraw.wait_past(last_gen, until_beat);
            continue;
        }

        /* Pace on the fastest monitor: its previous frame must be on screen
         * before this one starts, both because its flip released the buffer
         * we are about to fill and because nothing presents faster than its
         * refresh. Damage that lands meanwhile coalesces into this frame. */
        let flips = |h: &heads::Head<'_>| h.present && span.as_ref().is_none_or(|s| !s.is_member(h.identity) || s.active(h.identity));
        if let Some(lead) = heads.iter().filter(|h| flips(h)).max_by_key(|h| h.mode.refresh_mhz) {
            if let Err(e) = lead.flip_done.wait(lead.frame, flip_timeout_ms()) {
                /* The reset may have taken the last flip's copy with it; the
                 * display kept its buffer, so go on and commit a fresh frame. */
                if !reset_due {
                    return Err(e);
                }
                eprintln!("frescod: frame {} did not land across the reset ({e}); presenting anew", lead.frame);
            }
        }
        video::VIDEOS.landed(video::monotonic_ns(), &comp);

        /* Snapshot the generation AFTER the wait so damage during it is
         * folded into this present rather than triggering an extra one. */
        last_gen = redraw.current();
        let t_render = Instant::now();
        if strip_end_due {
            strip_until = None;
        }
        let spanned = |h: &heads::Head<'_>| span.as_ref().is_some_and(|s| s.is_member(h.identity));
        /* A span's members take the window straight when nothing covers
         * it; the desktop is composed only if some head still shows it. */
        last_direct = match span.as_ref().map(|s| span::direct(s, &comp, split.as_deref(), &heads, strip_until.is_some())) {
            Some(Ok(d)) => {
                if direct_said != Some("") {
                    eprintln!("frescod: span: direct scanout");
                    direct_said = Some("");
                }
                Some(d)
            }
            Some(Err(why)) => {
                if direct_said != Some(why) {
                    eprintln!("frescod: span: composed ({why})");
                    direct_said = Some(why);
                }
                None
            }
            None => None,
        };
        if reset_due {
            heads.iter_mut().for_each(|h| h.direct.clear());
        }
        let compose = last_direct.is_none() || heads.iter().any(|h| h.present && !spanned(h));
        if compose {
            render_one_frame(&mut renderer, &frontend, &comp, split.as_deref(), dw, dh)
                .map_err(io_other)?;
            let damage = renderer.damage();
            for h in heads.iter_mut() {
                h.dirty.add(damage);
                if reset_due {
                    h.dirty.forget();
                }
            }
            if multi {
                renderer.read_pixels(&mut framebuffer).map_err(io_other)?;
            }
        } else if let (Some(d), Some(s), Some(b)) = (&last_direct, &span, split.as_deref()) {
            /* Renderers still hear their sizes; pointer hits go to the
             * window, as compose would have shown it. */
            b.sync();
            let id = s.window_id as u16;
            let rect = [d.at.0, d.at.1, d.at.0 + d.px.width as i32, d.at.1 + d.px.height as i32];
            comp.lock().unwrap().shown = Some(vec![
                fresco_scene_server::window::ShownLayer { id: 0, rect: [0, 0, dw as i32, dh as i32], alpha: None },
                fresco_scene_server::window::ShownLayer { id, rect, alpha: Some(b.alpha_probe(s.window_id)) },
            ]);
        }
        last_present_ns = video::monotonic_ns();
        for h in heads.iter_mut().filter(|h| h.present && !spanned(h)) {
            if !h.landed() {
                if !reset_due {
                    h.stale = true;
                    continue;
                }
                let _ = h.flip_done.wait(h.frame, flip_timeout_ms());
            }
            let r = if multi { None } else { Some(&mut renderer) };
            present_head(&dpy, h, r, &mut framebuffer, dw, frame_deadline_ns, &hdr_enc)?;
        }
        /* A span's members flip together: one commit, once every one of
         * them has landed its last frame (they share one clock, so they
         * land together). Members out of the span's flips are left be. */
        let group = span.as_ref().map(|s| s.active_heads(&heads)).unwrap_or_default();
        if !group.is_empty() {
            if group.iter().all(|&k| heads[k].landed()) || reset_due {
                for &k in &group {
                    let _ = heads[k].flip_done.wait(heads[k].frame, flip_timeout_ms());
                }
                present_span(&dpy, &mut heads, &group, last_direct.as_ref(), &mut framebuffer, dw, frame_deadline_ns, &hdr_enc)?;
            } else {
                group.iter().for_each(|&k| heads[k].stale = true);
            }
        }

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

/// The connected outputs whose monitor has an identity, as OUTPUTS shows.
fn seen_outputs(dpy: &Display) -> std::io::Result<Vec<outputs::Seen>> {
    Ok(dpy
        .outputs()?
        .out
        .iter()
        .enumerate()
        .filter(|(_, o)| o.connected() && o.identity != 0)
        .map(|(n, o)| outputs::Seen { output: n as u32, identity: o.identity })
        .collect())
}

/// A head flips only while its monitor is there.
fn mark_present(heads: &mut [heads::Head<'_>], seen: &[outputs::Seen]) {
    for h in heads.iter_mut() {
        let there = seen.iter().any(|s| s.output == h.output && s.identity == h.identity);
        if there != h.present {
            eprintln!("frescod: output {} monitor {}", h.output, if there { "back" } else { "gone; its place is kept for now" });
        }
        h.present = there;
    }
}

/// Reconfigure the heads for what the outputs show now (Tracker: settle
/// and grace). With no monitor attached at all, the desktop stays as it
/// is: the last monitor going away is not a reason to turn it off.
/// Returns whether the heads changed, and when to look again.
fn reconfigure_heads<'a>(
    cx: &heads::Ctx<'a>,
    heads: &mut Vec<heads::Head<'a>>,
    tracker: &mut outputs::Tracker,
    remembered: &HashMap<outputs::DisplaySet, outputs::Arrangement>,
    requests: &[(u32, (u32, u32, u32))],
    can_hdr: bool,
) -> std::io::Result<(bool, Option<Instant>)> {
    let seen = seen_outputs(cx.dpy)?;
    mark_present(heads, &seen);
    if seen.is_empty() {
        eprintln!("frescod: monitor unplugged; the last mode stays programmed");
        return Ok((false, None));
    }
    let layout: Vec<outputs::Seen> =
        heads.iter().map(|h| outputs::Seen { output: h.output, identity: h.identity }).collect();
    let target = tracker.target(Instant::now(), &seen, &layout);
    /* The last flips must be on screen before a mode set. */
    for h in heads.iter() {
        let _ = h.flip_done.wait(h.frame, flip_timeout_ms());
    }
    let changed = heads::reconfigure(cx, heads, &target.keep, requests, wanted_mode(), remembered, can_hdr)?;
    if !changed && requests.is_empty() {
        let kept: Vec<String> = heads.iter().map(|h| format!("{}x{}@{} mHz", h.mode.width, h.mode.height, h.mode.refresh_mhz)).collect();
        eprintln!("frescod: hotplug settled; the monitors are as they were ({})", kept.join(", "));
    }
    Ok((changed, target.recheck))
}

/// Remember where this set of monitors sits, and which mirror which, for
/// when it comes back. Mirrors have no place of their own.
fn remember(
    heads: &[heads::Head<'_>],
    remembered: &mut HashMap<outputs::DisplaySet, outputs::Arrangement>,
    mirrors: &mut HashMap<outputs::DisplaySet, Vec<(u64, u64)>>,
) {
    let set = outputs::display_set(heads.iter().map(|h| h.identity));
    let vps: Vec<outputs::Viewport> = heads.iter().filter(|h| h.mirror_of.is_none()).map(|h| h.vp).collect();
    let (_, arr) = outputs::remember(&vps);
    remembered.insert(set.clone(), arr);
    mirrors.insert(set, heads.iter().filter_map(|h| h.mirror_of.map(|src| (h.identity, src))).collect());
}

/// Make monitor `identity` mirror monitor `of` (0: stop). Returns the
/// re-mode it wants: a new mirror goes to its source's size if its monitor
/// offers that (a pixel for a pixel, no scaling), a former mirror back to
/// its preferred mode.
fn set_mirror(heads: &mut [heads::Head<'_>], identity: u64, of: u64) -> Result<Option<(u32, (u32, u32, u32))>, String> {
    let k = heads.iter().position(|h| h.identity == identity).ok_or("no such monitor")?;
    if of == 0 {
        if heads[k].mirror_of.take().is_none() {
            return Ok(None);
        }
        let h = &heads[k];
        eprintln!("frescod: output {} stops mirroring", h.output);
        let pref = heads::preferred_mode(&h.conn.modes).map(|i| h.conn.modes[i]);
        return Ok(pref
            .filter(|m| (m.width, m.height, m.refresh_mhz) != (h.mode.width, h.mode.height, h.mode.refresh_mhz))
            .map(|m| (h.output, (m.width, m.height, m.refresh_mhz))));
    }
    let src = heads.iter().find(|h| h.identity == of).ok_or("no such source monitor")?;
    if of == identity || src.mirror_of.is_some() || heads.iter().any(|h| h.mirror_of == Some(identity)) {
        return Err("a monitor mirrors a plain one, not itself or a mirror".into());
    }
    let (sw, sh, sr) = (src.mode.width, src.mode.height, src.mode.refresh_mhz);
    let src_out = src.output;
    let h = &mut heads[k];
    h.mirror_of = Some(of);
    let want = heads::find_mode(&h.conn.modes, Some((sw, sh, sr))).or_else(|| heads::find_mode(&h.conn.modes, Some((sw, sh, 0))));
    let remode = want
        .map(|i| h.conn.modes[i])
        .filter(|m| (m.width, m.height, m.refresh_mhz) != (h.mode.width, h.mode.height, h.mode.refresh_mhz))
        .map(|m| (h.output, (m.width, m.height, m.refresh_mhz)));
    eprintln!(
        "frescod: output {} mirrors output {}{}",
        h.output,
        src_out,
        if want.is_some() { "" } else { " (scaled to fit: its monitor has no mode of that size)" }
    );
    Ok(remode)
}

fn desktop_of(heads: &[heads::Head<'_>]) -> (u32, u32) {
    let vps: Vec<outputs::Viewport> = heads.iter().map(|h| h.vp).collect();
    outputs::desktop_size(&vps)
}

fn fastest(heads: &[heads::Head<'_>]) -> u32 {
    heads.iter().map(|h| h.mode.refresh_mhz).max().unwrap_or(0)
}

/// Put the newest desktop frame on one head and flip it. With `renderer`
/// (one head, the whole desktop), the renderer presents straight into the
/// head's scanout; otherwise the head's part of `desktop` is cropped,
/// HDR-encoded if the head is in HDR, and uploaded. A flip the display
/// refuses stops that head until the next hotplug looks again.
fn present_head(
    dpy: &Display,
    h: &mut heads::Head<'_>,
    renderer: Option<&mut Backend<'_>>,
    desktop: &mut [u8],
    desktop_w: u32,
    deadline_ns: u32,
    enc: &hdr::Encoder,
) -> std::io::Result<()> {
    let (back, next) = upload_head(h, renderer, desktop, desktop_w, deadline_ns, enc)?;
    let mut c = OutputCommit::flip(h.output, Some(h.scan.buffer(back)), true);
    c.format = h.format();
    c.in_fence = Some((&h.copy_done, next));
    c.out_fence = Some((&h.flip_done, next));
    let r = dpy.commit_multi(&[c], false)?;
    if r.failed.is_some() {
        eprintln!("frescod: flip on output {} refused (fault {}); it waits for the next hotplug", h.output, r.reason);
        h.present = false;
        return Ok(());
    }
    h.frame = next;
    h.on_screen = back;
    h.stale = false;
    Ok(())
}

/// Put the newest desktop frame into `h`'s next scanout buffer (see
/// [`present_head`]); copy_done reaches the returned frame number once it
/// is there. Returns (buffer, frame).
fn upload_head(
    h: &mut heads::Head<'_>,
    renderer: Option<&mut Backend<'_>>,
    desktop: &mut [u8],
    desktop_w: u32,
    deadline_ns: u32,
    enc: &hdr::Encoder,
) -> std::io::Result<(usize, u64)> {
    let back = (h.on_screen + 1) % h.scan.len();
    let next = h.frame + 1;
    if let Some(x) = h.direct.get_mut(back) {
        *x = None;
    }
    /* Only the rows this buffer lacks go up. */
    let lacks = heads::Dirty::take(h, back);
    match renderer {
        Some(r) => {
            r.present(&h.scan, back, desktop, lacks, deadline_ns, (&h.copy_done, next), h.hdr_on.then_some(enc))
                .map_err(io_other)?
        }
        None => {
            outputs::crop_fit(desktop, desktop_w, &h.vp, &mut h.staging, h.mode.width, h.mode.height);
            if h.hdr_on {
                enc.encode(&mut h.staging);
            }
            /* Rows map one to one unless the head scales (a mirror). */
            let pitch = h.mode.width as usize * 4;
            let same = h.vp.width == h.mode.width && h.vp.height == h.mode.height;
            let range = match lacks.filter(|_| same) {
                Some(d) => {
                    let y0 = d[1].max(h.vp.y).saturating_sub(h.vp.y).min(h.mode.height);
                    let y1 = d[3].min(h.vp.y + h.vp.height).saturating_sub(h.vp.y).min(h.mode.height);
                    if y1 > y0 { y0 as usize * pitch..y1 as usize * pitch } else { 0..pitch }
                }
                None => 0..h.staging.len(),
            };
            h.scan.upload_range(back, &h.staging, range, deadline_ns, Some((&h.copy_done, next)))?;
        }
    }
    Ok((back, next))
}

/// Cut `h`'s tile out of a spanning window's drawing into its next
/// scanout buffer (direct scanout, fresco-span.md §5): only the rows that
/// changed since the drawing the buffer holds. Returns (buffer, frame).
fn upload_direct(
    h: &mut heads::Head<'_>,
    d: &span::Direct,
    deadline_ns: u32,
    enc: &hdr::Encoder,
) -> std::io::Result<(usize, u64)> {
    let back = (h.on_screen + 1) % h.scan.len();
    let next = h.frame + 1;
    if h.direct.len() != h.scan.len() {
        h.direct = vec![None; h.scan.len()];
    }
    let held = h.direct[back].filter(|&(_, hdr, vp)| hdr == h.hdr_on && vp == h.vp).map(|(g, ..)| g);
    let (mut y0, mut y1) = span::tile_rows(d, &h.vp, held).unwrap_or((0, h.mode.height));
    if y1 <= y0 {
        /* Nothing changed; one row still goes up, for the fence. */
        (y0, y1) = (0, 1.min(h.mode.height));
    }
    span::tile(d, &h.vp, &mut h.staging, (y0, y1));
    let pitch = h.mode.width as usize * 4;
    let range = y0 as usize * pitch..y1 as usize * pitch;
    if h.hdr_on {
        enc.encode(&mut h.staging[range.clone()]);
    }
    h.scan.upload_range(back, &h.staging, range, deadline_ns, Some((&h.copy_done, next)))?;
    h.direct[back] = Some((d.px.gen, h.hdr_on, h.vp));
    /* The buffer no longer holds a desktop frame. */
    h.dirty.forget();
    Ok((back, next))
}

/// Put the newest frame on a span's members (`group`: the heads in its
/// flips; the window straight with `direct`, else the desktop) and flip them in one commit, so every monitor shows the
/// same frame from the same vblank (fresco-span.md §5). A refused commit
/// is logged and tried again next frame: the members stay in the span.
fn present_span(
    dpy: &Display,
    heads: &mut [heads::Head<'_>],
    group: &[usize],
    direct: Option<&span::Direct>,
    desktop: &mut [u8],
    desktop_w: u32,
    deadline_ns: u32,
    enc: &hdr::Encoder,
) -> std::io::Result<()> {
    let mut up = Vec::with_capacity(group.len());
    for &k in group {
        up.push(match direct {
            Some(d) => upload_direct(&mut heads[k], d, deadline_ns, enc)?,
            None => upload_head(&mut heads[k], None, desktop, desktop_w, deadline_ns, enc)?,
        });
    }
    let parts: Vec<OutputCommit<'_, '_>> = group
        .iter()
        .zip(&up)
        .map(|(&k, &(back, next))| {
            let h = &heads[k];
            let mut c = OutputCommit::flip(h.output, Some(h.scan.buffer(back)), true);
            c.format = h.format();
            c.in_fence = Some((&h.copy_done, next));
            c.out_fence = Some((&h.flip_done, next));
            c
        })
        .collect();
    let refused = match dpy.commit_multi(&parts, false) {
        Ok(r) => r.failed.map(|i| format!("at output {} (fault {})", heads[group[i]].output, r.reason)),
        Err(e) => Some(e.to_string()),
    };
    drop(parts);
    if let Some(why) = refused {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("frescod: span flip refused {why}; trying again next frame");
        }
        return Ok(());
    }
    for (&k, &(back, next)) in group.iter().zip(&up) {
        let h = &mut heads[k];
        h.frame = next;
        h.on_screen = back;
        h.stale = false;
    }
    Ok(())
}

/// What OUTPUTS shows to the span (§6): a member that dropped out leaves
/// the flips and its tile goes dark; one back (and not held out for
/// flapping) rejoins. Returns when to look again.
fn span_attach(
    s: &mut span::Span,
    dpy: &Display,
    heads: &[heads::Head<'_>],
    seen: &[outputs::Seen],
    ev: &mpsc::Sender<fresco_scene_server::window::DisplayEvent>,
    now: Instant,
) -> Option<Instant> {
    let ids: Vec<u64> = seen.iter().map(|x| x.identity).collect();
    let (changed, again) = s.attach(now, &ids);
    if changed {
        if let Err(e) = s.lock(dpy, heads) {
            eprintln!("frescod: span: flip lock after a dropout: {e}");
        }
        s.tell(ev);
    }
    again
}

/// Start a span: lay it out and move the monitors for the bezels, VRR and
/// HDR on all members or none, lock their timing, and tell the window.
#[allow(clippy::too_many_arguments)]
fn start_span<'a>(
    r: &fresco_protocol::WmSpanGroupPayload,
    cx: &heads::Ctx<'a>,
    heads: &mut Vec<heads::Head<'a>>,
    tracker: &mut outputs::Tracker,
    remembered: &HashMap<outputs::DisplaySet, outputs::Arrangement>,
    frontend: &Arc<Mutex<EnvelopeFrontend>>,
    ev: &mpsc::Sender<fresco_scene_server::window::DisplayEvent>,
    can_hdr: bool,
) -> Result<span::Span, String> {
    let mut s = span::Span::new(r, heads)?;
    tracker.spanned = s.members.clone();
    if s.vrr_mixed(heads) {
        /* One frame period for the group: fixed refresh on every member. */
        eprintln!("frescod: span: not every member has variable refresh; fixed refresh while it is up");
        s.unplace(heads);
        *span::NO_VRR.lock().unwrap() = s.members.clone();
        s.vrr_off = true;
        reconfigure_heads(cx, heads, tracker, remembered, &[], can_hdr).map_err(|e| e.to_string())?;
        s.relayout(heads)?;
    }
    s.unify_hdr(cx.dpy, heads);
    if let Err(e) = s.lock(cx.dpy, heads) {
        end_span(s, cx, heads, tracker, remembered, frontend, ev, can_hdr);
        return Err(e);
    }
    frontend.lock().unwrap().set_span_moved(s.moved());
    s.tell(ev);
    Ok(s)
}

/// End a span: the monitors back in their places, the sync group gone,
/// the members' own VRR and HDR back, and the window told.
#[allow(clippy::too_many_arguments)]
fn end_span<'a>(
    s: span::Span,
    cx: &heads::Ctx<'a>,
    heads: &mut Vec<heads::Head<'a>>,
    tracker: &mut outputs::Tracker,
    remembered: &HashMap<outputs::DisplaySet, outputs::Arrangement>,
    frontend: &Arc<Mutex<EnvelopeFrontend>>,
    ev: &mpsc::Sender<fresco_scene_server::window::DisplayEvent>,
    can_hdr: bool,
) {
    s.unplace(heads);
    span::Span::unlock(cx.dpy);
    frontend.lock().unwrap().set_span_moved(Vec::new());
    tracker.spanned.clear();
    for h in heads.iter_mut().filter(|h| s.hdr_off.contains(&h.identity)) {
        h.hdr_on = apply_hdr(cx.dpy, h.output, &h.conn, can_hdr, Some(false));
    }
    if s.vrr_off {
        span::NO_VRR.lock().unwrap().clear();
        if let Err(e) = reconfigure_heads(cx, heads, tracker, remembered, &[], can_hdr) {
            eprintln!("frescod: span: variable refresh back: {e}");
        }
    }
    s.tell_ended(ev);
    eprintln!("frescod: window {} no longer spans", s.window_id);
}

/// Tell the WM side where each monitor sits (`OP_WM_OUTPUTS`).
/// Each monitor's scale is the one the session set for it, else the
/// automatic one from its pixel density.
fn publish_outputs(frontend: &Arc<Mutex<EnvelopeFrontend>>, heads: &[heads::Head<'_>], scales: &HashMap<u64, u32>, span: Option<&span::Span>) {
    let list = heads
        .iter()
        .map(|h| {
            let set = scales.get(&h.identity).copied();
            fresco_protocol::WmOutput {
                output: h.output,
                identity: h.identity,
                rect: fresco_protocol::WmRect { x: h.vp.x as i32, y: h.vp.y as i32, w: h.vp.width as i32, h: h.vp.height as i32 },
                refresh_mhz: h.mode.refresh_mhz,
                present: h.present,
                scale_milli: set.unwrap_or_else(|| outputs::auto_scale(&h.conn.edid, h.mode.width, h.mode.height)),
                scale_auto: set.is_none(),
                mirror_of: h.mirror_of.unwrap_or(0),
                width_mm: outputs::size_mm(&h.conn.edid).map_or(0, |s| s.0),
                height_mm: outputs::size_mm(&h.conn.edid).map_or(0, |s| s.1),
                flapping: span.is_some_and(|s| s.flapping(h.identity)),
            }
        })
        .collect();
    frontend.lock().unwrap().set_outputs(list);
}

/// Move monitors on the desktop (a WM's OP_WM_ARRANGE_OUTPUTS): the named
/// ones to their new corners, shifted so the desktop starts at (0, 0). The
/// result must keep every monitor touching another with no overlap
/// (outputs::validate), else nothing changes.
fn place_outputs(heads: &mut [heads::Head<'_>], placements: &[fresco_protocol::WmOutputPlacement]) -> Result<(), String> {
    /* Mirrors have no place of their own: they follow their source. */
    let plain: Vec<usize> = (0..heads.len()).filter(|&k| heads[k].mirror_of.is_none()).collect();
    let mut at: Vec<(i64, i64)> = plain
        .iter()
        .map(|&k| {
            let h = &heads[k];
            placements
                .iter()
                .find(|p| p.identity == h.identity)
                .map_or((i64::from(h.vp.x), i64::from(h.vp.y)), |p| (i64::from(p.x), i64::from(p.y)))
        })
        .collect();
    let (mx, my) = (at.iter().map(|a| a.0).min().unwrap_or(0), at.iter().map(|a| a.1).min().unwrap_or(0));
    for a in at.iter_mut() {
        *a = (a.0 - mx, a.1 - my);
    }
    if at.iter().any(|a| a.0 > i64::from(u16::MAX) || a.1 > i64::from(u16::MAX)) {
        return Err("too far apart".into());
    }
    let vps: Vec<outputs::Viewport> = plain
        .iter()
        .zip(&at)
        .map(|(&k, a)| outputs::Viewport { x: a.0 as u32, y: a.1 as u32, ..heads[k].vp })
        .collect();
    outputs::validate(&vps)?;
    for (&k, v) in plain.iter().zip(vps) {
        heads[k].vp = v;
    }
    heads::place_mirrors(heads);
    Ok(())
}

/// Tell the WM side what output 0's monitor offers (`OP_WM_DISPLAY_MODES`):
/// a WM's mode request applies to output 0.
fn publish_output0(frontend: &Arc<Mutex<EnvelopeFrontend>>, dpy: &Display, heads: &[heads::Head<'_>]) {
    match dpy.connector_at(0) {
        Ok((conn, _)) if conn.connected => {
            let cur = heads.iter().find(|h| h.output == 0 && h.present).map(|h| h.mode_index);
            publish_modes(frontend, &conn.modes, cur);
        }
        _ => publish_modes(frontend, &[], None),
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
    comp.lock().unwrap().mint_embed_tokens = true; // limen.md §5
    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx.clone());
    /* Synthetic input injection — the headless interactive harness. No HID device
     * needed: scripted input over a socket feeds the same event sink + hit-test as
     * real /dev/hidraw input, so we can drive + debug the input→WM→render loop. */
    if let Ok(isock) = std::env::var("FRESCOD_INPUT_SOCK") {
        injector_reader::spawn(ev_tx.clone(), comp.clone(), W, H, isock);
    }
    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));
    frontend.lock().unwrap().set_notification_uids(Some(notify::uids().to_vec()));
    /* Headless has no connector, but clients still need an answer — the surface
     * it renders into IS the screen here. Refresh 0 says "no real mode". */
    frontend.lock().unwrap().set_display_mode(W, H, 0);

    let sock = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco/fresco.sock"));
    let wm_sock = Some(std::env::var("FRESCOD_WM_SOCK").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/atrium/sockets/fresco-wm/fresco-wm.sock")));
    let redraw2 = redraw::RedrawSignal::new();
    /* Video layers (fresco-video.md): composed as pixels, so only by
     * the compute renderer; with another, creating one is refused. */
    if renderer.takes_pixels() {
        let _ = fresco_scene_server::video::SINK.set(Box::new(video::Direct { redraw: redraw2.clone() }));
    }
    video::CAPTURED.store(true, std::sync::atomic::Ordering::Relaxed);
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
    socket_server::install_embed_client(socket_server::Shared {
        frontend: frontend.clone(),
        lane: None,
        redraw: redraw2.clone(),
        hand_off: split.as_ref().map(|b| b.hand_off()),
    }, subs.clone());
    socket_server::spawn_event_fanout(ev_rx, subs, redraw2.clone(), comp.clone());
    activation::start(comp.clone());
    drag::install(comp.clone());
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
        let (now_ns, video_at) = (video::monotonic_ns(), video::VIDEOS.next_due());
        if cur == last_gen && !heartbeat_due && !video_at.is_some_and(|t| t <= now_ns) {
            let mut until_beat = heartbeat.saturating_sub(last_render.elapsed());
            if let Some(t) = video_at {
                until_beat = until_beat.min(Duration::from_nanos(t.saturating_sub(now_ns)));
            }
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
        /* No flip here: on screen once captured. */
        video::VIDEOS.landed(video::monotonic_ns(), &comp);
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
    /* Toasts above every window, inside their band (tabula.md §5),
     * whatever the WM or the client asked; before sync, which passes
     * the sizes on to renderers. */
    let top = frontend.lock().unwrap().reserved_top();
    {
        let band = fresco_protocol::notification_band(screen_w, screen_h, top);
        comp.lock().unwrap().clamp_notifications(band);
    }
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
    /* Credential slots (arca-autofill.md §4): what this frame shows of
     * each, before frescod's own chrome goes on top. */
    let slots = credential::observe_frame(comp, &rects, [0, top as i32, screen_w as i32, screen_h as i32], activation::now_ns());
    /* Trusted chrome (F4d), split mode: frescod's own layer, on top. */
    if let Some(b) = split {
        /* The focus read and released first: origin() takes the broker's
         * window locks, which a renderer's thread takes before comp. */
        let focus = comp.lock().unwrap().focus;
        let mut c = chrome::build(screen_w, focus, |id| b.origin(id), &rects, &slots);
        c.size.1 = c.size.1.min(screen_h);
        rects.push((c.id, [0, 0, c.size.0 as i32, c.size.1 as i32]));
        windows.push(c);
    } else if let Some(mut c) = chrome::slot_frames(&slots) {
        /* Direct mode: no strip, but slots still get their frame. */
        c.size = (c.size.0.min(screen_w), c.size.1.min(screen_h));
        rects.push((c.id, [0, 0, c.size.0 as i32, c.size.1 as i32]));
        windows.push(c);
    }
    /* Video layers (fresco-video.md §3): each the latest frame due, just
     * under its window, clipped to it; a window answers clicks inside its
     * layers itself (they are part of it, §10). */
    let mut video_rects: std::collections::HashMap<u32, Vec<[i32; 4]>> = std::collections::HashMap::new();
    if renderer.takes_pixels() {
        let composed: std::collections::HashSet<u32> = windows.iter().map(|w| w.id).collect();
        let mut due = video::VIDEOS.take(video::monotonic_ns(), &composed);
        if !due.is_empty() {
            let mut out = Vec::with_capacity(windows.len() + due.len());
            for w in windows.drain(..) {
                let (ww, wh) = render_backend::surface_size(&w);
                for v in due.remove(&w.id).unwrap_or_default() {
                    let Some((mut px, at)) = video::clip(&v.pixels, v.at, ww, wh) else { continue };
                    /* Protected (fresco-video.md §6): black in a capture. */
                    if v.protected && video::CAPTURED.load(std::sync::atomic::Ordering::Relaxed) {
                        px = video::pixels(px.width, px.height, vec![0xff00_0000; px.px.len()], px.seq);
                    }
                    video_rects.entry(w.id).or_default().push([at.0, at.1, at.0 + px.width as i32, at.1 + px.height as i32]);
                    out.push(WindowFrame {
                        id: v.id,
                        pos: (w.pos.0.round() + at.0 as f32, w.pos.1.round() + at.1 as f32),
                        size: (px.width, px.height),
                        pixels: Some(px),
                        ..Default::default()
                    });
                }
                out.push(w);
            }
            windows = out;
        }
    }
    /* A window drawn as pixels (split pixel mode) answers from them. */
    let drawn: std::collections::HashSet<u32> =
        windows.iter().filter(|w| w.pixels.is_some()).map(|w| w.id).collect();
    renderer.render(ScreenInput { windows, uploads, clears })?;
    /* Fills packed here that did not draw: their owners hear which. */
    let drops = renderer.take_fill_drops();
    if !drops.is_empty() {
        let mut c = comp.lock().unwrap();
        for (id, d) in drops {
            if let Ok(id) = u16::try_from(id) {
                c.set_fill_drops(id, d);
            }
        }
    }
    /* Each layer's alpha comes from the frame it was drawn from, so a
     * pixel a window left transparent passes the click on (F4c′). */
    let frames: std::collections::HashMap<u32, Arc<fresco_compute::Frame>> =
        renderer.frames().iter().cloned().collect();
    let shown = rects.into_iter().map(|(id, rect)| {
        let alpha = if let (true, Some(b)) = (drawn.contains(&id), split) {
            Some(b.alpha_probe(id))
        } else {
            frames.get(&id).cloned().map(|f| -> fresco_scene_server::window::AlphaProbe {
                Arc::new(move |x, y| fresco_compute::reference::alpha_at(&f, x, y))
            })
        };
        let alpha = match (alpha, video_rects.remove(&id)) {
            (Some(a), Some(vr)) => Some(Arc::new(move |x: u32, y: u32| {
                let (x, y) = (x as i32, y as i32);
                if vr.iter().any(|r| x >= r[0] && y >= r[1] && x < r[2] && y < r[3]) { 255 } else { a(x as u32, y as u32) }
            }) as fresco_scene_server::window::AlphaProbe),
            (a, _) => a,
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
pub(crate) fn apply_hdr(dpy: &Display, output: u32, conn: &atrium_gpu::amd::ConnectorV2, can: bool, was: Option<bool>) -> bool {
    use atrium_gpu::amd::{Hdr, COLORIMETRY_BT2020, EOTF_PQ};
    let want = std::env::var("FRESCOD_HDR").is_ok_and(|v| v == "1");
    if want && conn.hdr && can {
        if let Ok((_, caps)) = dpy.hdr_at(output) {
            if caps.eotfs & (1 << EOTF_PQ) != 0 && caps.colorimetry & (1 << COLORIMETRY_BT2020) != 0 {
                /* SDR content: nothing brighter than reference white. */
                let white = hdr::SDR_WHITE_NITS as u16;
                let peak = if caps.max_nits == 0 { 1000 } else { caps.max_nits.min(10000) as u16 };
                if dpy.set_hdr_at(output, &Hdr::hdr10(peak, white, white)).is_ok() {
                    if was != Some(true) {
                        eprintln!("frescod: output {output}: HDR10 on (monitor peak {} cd/m2): the desktop at {white} cd/m2 white, 10-bit PQ",
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
    let _ = dpy.set_hdr_at(output, &Hdr::default());
    false
}

/// Run `mode` with variable refresh: the monitor has a range that covers the
/// mode's refresh, and FRESCOD_VRR isn't 0. Frames then go up as soon as they
/// are rendered (no waiting for the next fixed refresh slot), and an idle
/// screen refreshes at the monitor's floor.
pub(crate) fn use_vrr(conn: &atrium_gpu::amd::ConnectorV2, mode: &ModeV2, identity: u64) -> bool {
    if std::env::var("FRESCOD_VRR").is_ok_and(|v| v == "0") || span::NO_VRR.lock().unwrap().contains(&identity) {
        return false;
    }
    let hz = (mode.refresh_mhz + 500) / 1000;
    conn.vrr.is_some_and(|(lo, hi)| (lo..=hi).contains(&hz))
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
