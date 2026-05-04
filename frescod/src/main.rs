//! frescod — first-light native FreeBSD compositor demo.
//!
//! Owns the Atrium GPU + display cdevs, runs a frame loop, renders a
//! desktop-shaped scene with tiny-skia, page-flips. The renderer is
//! `fresco_scene_server::render::tiny_skia_backend::TinySkiaBackend` —
//! the same backend that will eventually drive scene-graph rendering
//! when a real protocol client connects (D1 step 2c.2).
//!
//! Today the compositor draws into the backend's `PixmapMut` directly
//! (imperative mode); tomorrow it will hand a `SceneGraph` + `CasStore`
//! to `backend.render_frame(...)` and the same pixels will appear.
//!
//! Today's scene:
//!   - Subtle vertical gradient background
//!   - A "panel" — rounded rect with soft drop shadow
//!   - An analog clock inside the panel: face, tick marks, hour/minute/second hands
//!   - A small frame counter in the corner (rendered as a stack of small boxes
//!     so we can tell at a glance the loop is alive — no font subsystem yet)
//!
//! Targets ~30 fps via fixed-cadence sleep. virtio-gpu doesn't expose vblank
//! events (D0 step 3.5), so we don't pace to vsync.

mod cursor;
mod input_reader;
mod pointer_dispatch;
mod pointer_reader;
mod scene_build;
mod socket_server;

use atrium_gpu::abi::*;
use atrium_gpu::{Bo, Display, Gpu, Mode};
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::frontend::CommandFrontend;
use fresco_scene_server::command::protocol::{Hash256, NULL_HASH};
use fresco_scene_server::render::backend::{GpuBackend, WindowOverlay};
use fresco_scene_server::render::tiny_skia_backend::TinySkiaBackend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor as WmCompositor;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, PixmapMut,
    Point, Rect, Shader, SpreadMode, Stroke, Transform,
};

const TARGET_FPS: u64 = 30;
const FRAME_NS: u64 = 1_000_000_000 / TARGET_FPS;

/// State that survives across frames.
struct Compositor<'g> {
    bo: Bo<'g>,
    mode: Mode,
    backend: TinySkiaBackend,
    /// Shared with the socket-server thread so a connected Fresco
    /// client can drive the scene from outside.
    cas: Arc<Mutex<CasStore>>,
    scene: Arc<Mutex<SceneGraph>>,
    /// Window manager state — windows + z-order + drag/resize state.
    /// Per-window FBOs in the backend are reconciled from this each
    /// frame.
    wm: Arc<Mutex<WmCompositor>>,
    cursor: Arc<Mutex<cursor::CursorState>>,
    /// Cached primitive meshes — built once at startup, referenced by
    /// hash from every per-frame RenderItem. CAS dedups identical
    /// content so this also matches what the dedup-on-upload protocol
    /// does for real clients.
    rect_mesh: Hash256,
    disk_mesh: Hash256,
    ring_mesh: Hash256,
    started: Instant,
    frame: u64,
}

impl<'g> Compositor<'g> {
    fn render(&mut self) {
        let w = self.mode.width;
        let h = self.mode.height;
        let now = self.started.elapsed().as_secs_f32();

        // ── Phase 1: enumerate floating windows + sync FBOs ─────
        // Snapshot to a Vec so we don't hold the WM lock across
        // per-window scene rendering (that would deadlock against
        // socket threads issuing routable commands which take WM
        // for routing).
        let window_info: Vec<(u16, Arc<Mutex<SceneGraph>>)> = {
            let g = self.wm.lock().unwrap();
            g.windows.iter()
                .filter(|(&id, _)| id != 0)
                .map(|(&id, w)| (id, w.scene.clone()))
                .collect()
        };
        let live: HashMap<u16, (u32, u32)> = {
            let g = self.wm.lock().unwrap();
            g.windows.iter()
                .filter(|(&id, _)| id != 0)
                .map(|(&id, w)| (id, (w.size.0 as u32, w.size.1 as u32)))
                .collect()
        };
        self.backend.sync_fbos(&live);

        // ── Phase 2: render each window's scene into its FBO ─────
        for (id, scene_arc) in &window_info {
            let mut cas = self.cas.lock().unwrap();
            let mut scene = scene_arc.lock().unwrap();
            if scene.root_hash != NULL_HASH && scene.is_dirty() {
                scene.traverse(&mut cas);
            }
            if !scene.render_list().is_empty() {
                self.backend.render_window_to_fbo(*id, &scene, &cas);
            }
        }

        // ── Phase 3: build (overlay, decorations) per window in z-
        // order. Lowest-z first so the backend interleaves blit +
        // decorations correctly: each window's chrome goes above its
        // own content, and below higher-z windows.
        let layered: Vec<(WindowOverlay, Vec<fresco_scene_server::scene::graph::RenderItem>)> = {
            let g = self.wm.lock().unwrap();
            g.z_order.iter()
                .filter_map(|&id| {
                    if id == 0 { return None; }
                    let win = g.windows.get(&id)?;
                    let ov = WindowOverlay {
                        id, x: win.pos.0, y: win.pos.1,
                        w: win.size.0, h: win.size.1,
                    };
                    Some((ov, g.compose_overlay_for(id)))
                })
                .collect()
        };

        // ── Phase 4: screen scene + composite ────────────────────
        // Lock order must match CommandFrontend::handle_set_root:
        // cas first, scene second. Otherwise a socket thread holding
        // cas-then-waiting-on-scene + render loop holding scene-then-
        // waiting-on-cas → deadlock.
        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();
        let socket_driven = scene.root_hash != NULL_HASH
                         || !scene.render_list().is_empty();
        let has_windows = !layered.is_empty();

        if socket_driven || has_windows {
            if scene.root_hash != NULL_HASH && scene.is_dirty() {
                scene.traverse(&mut cas);
            }
            self.backend.render_screen_with_windows(&scene, &cas, &layered);
        } else {
            // Fallback: in-process clock demo.
            scene.clear();

            // Full-screen background gradient.
            scene_build::push_gradient_rect(
                &mut scene, &mut cas, self.rect_mesh,
                0.0, 0.0, w as f32, h as f32,
                (0.0, 0.0), (0.0, h as f32),
                &[
                    (0.0, [0x10, 0x12, 0x1a, 0xff]),
                    (1.0, [0x1c, 0x22, 0x36, 0xff]),
                ],
                0,
            );

            let pw = (w as f32 * 0.50).max(360.0);
            let ph = pw;
            let cx = w as f32 / 2.0;
            let cy = h as f32 / 2.0;
            let px = cx - pw / 2.0;
            let py = cy - ph / 2.0;
            scene_build::push_rect(
                &mut scene, &mut cas, self.rect_mesh,
                px, py, pw, ph, [0x22, 0x28, 0x38, 0xee],
                1,
            );

            let r = (pw * 0.45).max(140.0);

            scene_build::push_disk(
                &mut scene, &mut cas, self.disk_mesh,
                cx, cy, r,
                [0x10, 0x14, 0x1c, 0xff],
                2,
            );
            scene_build::push_ring(
                &mut scene, &mut cas, self.ring_mesh,
                cx, cy, r,
                [0xff, 0xff, 0xff, 0x80],
                3,
            );

            for i in 0..12 {
                let theta = (i as f32) * std::f32::consts::TAU / 12.0
                          - std::f32::consts::FRAC_PI_2;
                let inner = r * 0.86;
                let outer = r * 0.95;
                let nx = cx + theta.cos() * inner;
                let ny = cy + theta.sin() * inner;
                let length = outer - inner;
                let width = if i % 3 == 0 { 3.0 } else { 1.5 };
                scene_build::push_oriented_rect(
                    &mut scene, &mut cas, self.rect_mesh,
                    nx, ny, theta, length, width,
                    [0xff, 0xff, 0xff, 0xc0],
                    4,
                );
            }

            let sec_a  = (now % 60.0)    / 60.0     * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let min_a  = (now % 3600.0)  / 3600.0   * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let hour_a = (now % 43200.0) / 43200.0  * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;

            scene_build::push_oriented_rect(
                &mut scene, &mut cas, self.rect_mesh,
                cx, cy, hour_a, r * 0.50, 6.0,
                [0xff, 0xff, 0xff, 0xff], 5,
            );
            scene_build::push_oriented_rect(
                &mut scene, &mut cas, self.rect_mesh,
                cx, cy, min_a, r * 0.75, 4.0,
                [0xff, 0xff, 0xff, 0xee], 6,
            );
            scene_build::push_oriented_rect(
                &mut scene, &mut cas, self.rect_mesh,
                cx, cy, sec_a, r * 0.85, 1.6,
                [0xff, 0x66, 0x66, 0xff], 7,
            );

            scene_build::push_disk(
                &mut scene, &mut cas, self.disk_mesh,
                cx, cy, 6.0,
                [0xff, 0x44, 0x44, 0xff],
                8,
            );

            scene.mark_dirty();
            self.backend.render_frame(&scene, &cas, self.frame, None);
        }

        drop(scene);
        drop(cas);

        // Frame heartbeat — small imperative overlay.
        draw_frame_indicator(&mut self.backend.pixmap_mut(), self.frame);

        // Software cursor overlay — last so it sits above scene + WM
        // decorations. Cheap; no HW cursor plane on virtio-gpu yet.
        {
            let c = *self.cursor.lock().unwrap();
            if c.visible {
                cursor::draw(&mut self.backend.pixmap_mut(), c.x, c.y);
            }
        }

        self.backend.copy_to_bgra(self.bo.as_mut_slice());
        self.frame = self.frame.wrapping_add(1);
    }
}

fn draw_background(pixmap: &mut PixmapMut, w: u32, h: u32) {
    let mut paint = Paint::default();
    paint.shader = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(0.0, h as f32),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(0x10, 0x12, 0x1a, 0xff)),
            GradientStop::new(1.0, Color::from_rgba8(0x1c, 0x22, 0x36, 0xff)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(Shader::SolidColor(Color::from_rgba8(0x14, 0x18, 0x22, 0xff)));
    let bg = PathBuilder::from_rect(Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap());
    pixmap.fill_path(&bg, &paint, FillRule::Winding, Transform::identity(), None);
}

/// Build a rounded-rect path centered on the screen.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().expect("rounded-rect path")
}

fn draw_panel(pixmap: &mut PixmapMut, w: u32, h: u32) {
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let pw = (w as f32 * 0.50).max(360.0);
    let ph = pw;  // square panel for the clock
    let px = cx - pw / 2.0;
    let py = cy - ph / 2.0;

    // Soft drop shadow: a few translucent rounded rects, increasing radius
    // and offset, decreasing alpha. Cheap, looks decent without a real blur.
    for layer in 0..6 {
        let off = (layer as f32) * 1.5 + 4.0;
        let alpha = (24 - layer * 4).max(6) as u8;
        let mut sp = Paint::default();
        sp.shader = Shader::SolidColor(Color::from_rgba8(0, 0, 0, alpha));
        sp.anti_alias = true;
        let path = rounded_rect(px - off / 2.0, py + off, pw + off, ph + off, 24.0 + off);
        pixmap.fill_path(&path, &sp, FillRule::Winding, Transform::identity(), None);
    }

    // Panel body — translucent dark with a faint edge highlight.
    let body = rounded_rect(px, py, pw, ph, 24.0);
    let mut body_paint = Paint::default();
    body_paint.shader = Shader::SolidColor(Color::from_rgba8(0x22, 0x28, 0x38, 0xee));
    body_paint.anti_alias = true;
    pixmap.fill_path(&body, &body_paint, FillRule::Winding, Transform::identity(), None);

    let mut edge = Paint::default();
    edge.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xff, 0xff, 0x18));
    edge.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 1.2;
    pixmap.stroke_path(&body, &edge, &stroke, Transform::identity(), None);
}

fn draw_clock(pixmap: &mut PixmapMut, w: u32, h: u32, t: f32) {
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let r = (w as f32 * 0.50 * 0.45).max(140.0);  // 45% of panel half-width

    // Face: circle.
    let mut face_paint = Paint::default();
    face_paint.shader = Shader::SolidColor(Color::from_rgba8(0x10, 0x14, 0x1c, 0xff));
    face_paint.anti_alias = true;
    let face = circle_path(cx, cy, r);
    pixmap.fill_path(&face, &face_paint, FillRule::Winding, Transform::identity(), None);

    // Outer ring.
    let mut ring = Paint::default();
    ring.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xff, 0xff, 0x80));
    ring.anti_alias = true;
    let mut s = Stroke::default();
    s.width = 2.0;
    pixmap.stroke_path(&face, &ring, &s, Transform::identity(), None);

    // Hour ticks (12).
    for i in 0..12 {
        let theta = (i as f32) * std::f32::consts::TAU / 12.0 - std::f32::consts::FRAC_PI_2;
        let inner = r * 0.86;
        let outer = r * 0.95;
        let x1 = cx + theta.cos() * inner;
        let y1 = cy + theta.sin() * inner;
        let x2 = cx + theta.cos() * outer;
        let y2 = cy + theta.sin() * outer;
        let mut pb = PathBuilder::new();
        pb.move_to(x1, y1);
        pb.line_to(x2, y2);
        let path = pb.finish().unwrap();
        let mut tick = Paint::default();
        tick.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xff, 0xff, 0xc0));
        tick.anti_alias = true;
        let mut ts = Stroke::default();
        ts.width = if i % 3 == 0 { 3.0 } else { 1.5 };
        pixmap.stroke_path(&path, &tick, &ts, Transform::identity(), None);
    }

    // Hands. Period: hour=43200s, minute=3600s, second=60s.
    // For animation visibility we use *real elapsed time since start* —
    // not wall clock — so the hands sweep visibly in a short demo.
    let sec_angle  = (t % 60.0) / 60.0      * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
    let min_angle  = (t % 3600.0) / 3600.0  * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
    let hour_angle = (t % 43200.0) / 43200.0 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;

    draw_hand(pixmap, cx, cy, hour_angle, r * 0.50, 6.0, Color::from_rgba8(0xff, 0xff, 0xff, 0xff));
    draw_hand(pixmap, cx, cy, min_angle,  r * 0.75, 4.0, Color::from_rgba8(0xff, 0xff, 0xff, 0xee));
    draw_hand(pixmap, cx, cy, sec_angle,  r * 0.85, 1.6, Color::from_rgba8(0xff, 0x66, 0x66, 0xff));

    // Hub.
    let mut hub = Paint::default();
    hub.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0x44, 0x44, 0xff));
    hub.anti_alias = true;
    let hub_path = circle_path(cx, cy, 6.0);
    pixmap.fill_path(&hub_path, &hub, FillRule::Winding, Transform::identity(), None);
}

fn draw_hand(pixmap: &mut PixmapMut, cx: f32, cy: f32, angle: f32, length: f32, width: f32, color: Color) {
    let x = cx + angle.cos() * length;
    let y = cy + angle.sin() * length;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy);
    pb.line_to(x, y);
    let path = pb.finish().unwrap();
    let mut paint = Paint::default();
    paint.shader = Shader::SolidColor(color);
    paint.anti_alias = true;
    let mut s = Stroke::default();
    s.width = width;
    s.line_cap = tiny_skia::LineCap::Round;
    pixmap.stroke_path(&path, &paint, &s, Transform::identity(), None);
}

fn circle_path(cx: f32, cy: f32, r: f32) -> tiny_skia::Path {
    // tiny-skia has Path::circle? Let me build via PathBuilder cubic.
    // 4 cubics with kappa for accurate circle approximation.
    const K: f32 = 0.5522847498;
    let kr = r * K;
    let mut pb = PathBuilder::new();
    pb.move_to(cx + r, cy);
    pb.cubic_to(cx + r, cy + kr, cx + kr, cy + r, cx, cy + r);
    pb.cubic_to(cx - kr, cy + r, cx - r, cy + kr, cx - r, cy);
    pb.cubic_to(cx - r, cy - kr, cx - kr, cy - r, cx, cy - r);
    pb.cubic_to(cx + kr, cy - r, cx + r, cy - kr, cx + r, cy);
    pb.close();
    pb.finish().expect("circle path")
}

/// 8 small boxes in the top-left, lit in a binary pattern of the frame
/// counter's low byte. A heartbeat that doesn't need a font subsystem.
fn draw_frame_indicator(pixmap: &mut PixmapMut, frame: u64) {
    let lo = (frame & 0xff) as u8;
    for bit in 0..8 {
        let on = (lo >> (7 - bit)) & 1 == 1;
        let alpha = if on { 0xff } else { 0x40 };
        let color = if on {
            Color::from_rgba8(0xff, 0xcc, 0x33, alpha)
        } else {
            Color::from_rgba8(0xff, 0xff, 0xff, alpha / 4)
        };
        let mut p = Paint::default();
        p.shader = Shader::SolidColor(color);
        let r = Rect::from_xywh(24.0 + (bit as f32) * 18.0, 24.0, 14.0, 18.0).unwrap();
        let path = PathBuilder::from_rect(r);
        pixmap.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    }
}

fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    /* M2.4d wiring check: confirm the Vulkan backend dependency links
     * cleanly when --features vulkan is enabled. Real integration
     * follows in M2.5+ when the per-connection SceneState rework
     * redirects scene-graph mutations into HeadlessRenderer's
     * set_rect_nodes / set_texture_batches APIs. */
    #[cfg(feature = "vulkan")]
    eprintln!("frescod: built with Vulkan backend (fresco-vulkan v{})",
        env!("CARGO_PKG_VERSION"));

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
    let bo = gpu.alloc(bytes, flags)?;

    let backend = TinySkiaBackend::new(mode.width, mode.height);
    let mut bootstrap_cas = CasStore::new();
    let rect_mesh = scene_build::store_unit_rect(&mut bootstrap_cas);
    let disk_mesh = scene_build::store_disk(&mut bootstrap_cas, 64);
    let ring_mesh = scene_build::store_ring(&mut bootstrap_cas, 80, 0.96);

    let cas = Arc::new(Mutex::new(bootstrap_cas));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slot_table = Arc::new(Mutex::new(SlotTable::new()));
    let wm = Arc::new(Mutex::new(WmCompositor::new_with_window0(
        scene.clone(),
        slot_table.clone(),
    )));
    wm.lock().unwrap().init_decorations(&mut cas.lock().unwrap());
    let frontend = Arc::new(Mutex::new(CommandFrontend::new(
        cas.clone(),
        scene.clone(),
        slot_table.clone(),
        wm.clone(),
    )));

    // Spawn the Fresco-protocol socket server. Best-effort: if the
    // listener can't bind we still run, just without external clients.
    let sock_path = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/frescod.sock"));
    let event_subs = match socket_server::spawn(
        socket_server::Shared { frontend: frontend.clone() },
        &sock_path,
    ) {
        Ok(subs) => Some(subs),
        Err(e) => {
            eprintln!("frescod: socket server failed: {e}");
            None
        }
    };

    // Native FreeBSD input. Keyboard via /dev/kbd0 K_RAW (AT scan
    // codes → HID Usage Page 0x07); pointer via /dev/uhid0 (raw HID
    // reports). No evdev anywhere in the pipeline.
    let cursor_state = cursor::CursorState::new(
        (mode.width as f32) / 2.0,
        (mode.height as f32) / 2.0,
    );
    let modifiers: input_reader::SharedModifiers = Arc::new(Mutex::new(0));
    if let Some(subs) = event_subs.clone() {
        // Stagger spawn: keyboard and pointer probes both open
        // /dev/hidraw* devices and FreeBSD's hidraw enforces exclusive
        // open. Probing in parallel from two threads races and one
        // gets EBUSY for a device the other was just probing.
        // Pointer first (cursor goes silent first if it's missing),
        // keyboard 200 ms later.

        // Native pointer via /dev/hidraw* (or /dev/uhid* fallback).
        // The compositor does not fall
        // back to evdev — the bring-up VM is expected to have ums
        // detached so /dev/uhid0 exists. See RUNBOOK.md for the one-
        // time setup. If /dev/uhid* is missing the compositor logs
        // and continues without pointer input (keyboard still works).
        let disp = pointer_dispatch::Dispatcher {
            event_subs: subs.clone(),
            cursor:     cursor_state.clone(),
            wm:         wm.clone(),
            modifiers:  modifiers.clone(),
            screen_w:   mode.width,
            screen_h:   mode.height,
        };
        pointer_reader::spawn(disp);
        std::thread::sleep(Duration::from_millis(200));
        input_reader::spawn(subs, modifiers, wm.clone());
    }
    let _ = event_subs;

    let mut comp = Compositor {
        bo,
        mode,
        backend,
        cas,
        scene,
        wm: wm.clone(),
        cursor: cursor_state,
        rect_mesh,
        disk_mesh,
        ring_mesh,
        started: Instant::now(),
        frame: 0,
    };

    // First frame + SET_MODE.
    comp.render();
    dpy.set_mode(conn.id, &comp.bo, mode)?;
    dpy.page_flip(conn.id, &comp.bo)?;

    // Frame loop.
    let mut next = Instant::now() + Duration::from_nanos(FRAME_NS);
    loop {
        comp.render();
        dpy.page_flip(conn.id, &comp.bo)?;

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += Duration::from_nanos(FRAME_NS);
    }
}
