#![allow(dead_code)]
//! fresco-renderer: one client connection's window renderer (F4b,
//! docs/spec/fresco-f4-split.md; threat model §2.5.2).
//!
//! frescod in split mode (`FRESCOD_SPLIT=1`) accepts a client and hands
//! the connection to a fresh fresco-renderer, so the process that parses
//! the client's scene graph, textures and fonts holds nothing but that
//! client's own state: no GPU, no display, no input devices, no other
//! client. It speaks to the broker only in fixed records (`fresco_split`):
//!
//! - stdin is the broker channel. Its first record, `CLIENT`, brings the
//!   client's socket and the display mode; the renderer serves the client
//!   with the same reader/writer and scene server frescod runs in-process.
//!   (Under `portcullis exec` a jail gets only its three stdio fds, so
//!   everything else arrives over the channel.)
//! - The renderer declares the client's windows; the broker answers with
//!   two frame slots per window and the size it decided; the renderer packs
//!   each window's scene into a free slot and says `FRAME_READY`.
//!
//! - In pixel mode (`CLIENT.pixels`, frescod's compute renderer) it also
//!   rasters each window with the Tier-2 kernels and sends the pixels, so
//!   a hostile scene is rastered here, in this client's jail, never in
//!   frescod. The kernels come precompiled with the bundle
//!   (`/usr/local/share/fresco-kernels`, made by `--seed-kernels`), or are
//!   compiled into `/tmp` on start.
//!
//! It reads no environment beyond `RUST_LOG` and `FRESCO_COMPUTE_CACHE`,
//! writes only stderr, and exits when either the client or the broker goes.
//! `fresco-renderer --seed-kernels <dir>` compiles the kernels into `dir`
//! and exits (bundle build time).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fresco_protocol::WmRole;
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::{EnvelopeFrontend, Outbound};
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::{Compositor, DisplayEvent};
use fresco_split::chan::{Chan, Recv, Shm};
use fresco_split::{slot, Input, Msg, Role, MAX_WINDOWS, MAX_WINDOW_DIM};

#[path = "../laminar.rs"]
mod laminar;
#[path = "../pack.rs"]
mod pack;
#[path = "../redraw.rs"]
mod redraw;
#[path = "../activation.rs"]
#[allow(dead_code)]
mod activation;
#[path = "../socket_server.rs"]
mod socket_server;

/// The client's id inside this renderer: it is the only one.
const CLIENT: u8 = 1;
const BROKER_FD: i32 = 0;

/// One of the client's windows, as the broker knows it.
#[derive(Default)]
struct Win {
    slots: Option<[Shm; 2]>,
    /// Slots the broker has not given back yet.
    busy: [bool; 2],
    /// The size the broker decided; nothing is drawn before it says.
    size: Option<(u32, u32)>,
    /// Signature of the last frame sent, so an unchanged window is not
    /// sent again.
    sig: Option<u64>,
    seq: u32,
    title: String,
    /// Pixel mode: the window as last drawn, its tiles' signatures, the
    /// drawing each slot holds (seq, 0 none), and recent frames' damage.
    px: Vec<u32>,
    tiles: Option<Vec<u64>>,
    slot_seq: [u32; 2],
    history: std::collections::VecDeque<(u32, Option<slot::Rect>)>,
    /// The scroll viewports of the drawing in `px`, each with its key
    /// (content hash and the uploads of the slots it samples).
    scrolls: Vec<(pack::Scroll, u64)>,
}

impl Win {
    /// Forget what was drawn: the next frame is drawn and sent whole.
    fn forget(&mut self) {
        self.sig = None;
        self.tiles = None;
        self.scrolls.clear();
        self.slot_seq = [0; 2];
        self.history.clear();
    }

    /// What slot `i` lacks of drawing `seq` (the newest in `history`):
    /// the damage since the drawing it holds; `None` for all of it.
    fn stale(&self, i: usize) -> Option<slot::Rect> {
        let had = self.slot_seq[i];
        if had == 0 {
            return None;
        }
        let mut r: Option<slot::Rect> = None;
        let mut reached = false;
        for &(seq, d) in self.history.iter().rev() {
            if seq == had {
                reached = true;
                break;
            }
            let d = d?;
            r = Some(r.map_or(d, |r| union(r, d)));
        }
        if !reached {
            return None;
        }
        Some(r.unwrap_or([0, 0, 1, 1]))
    }
}

fn union(a: slot::Rect, b: slot::Rect) -> slot::Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// Raster `frame` into `w.px`: only the tiles whose signatures changed.
/// `Ok(Some(damage))`: the drawing changed, inside `damage` against the
/// previous one (`None`: anywhere); `Ok(None)`: not a pixel changed.
/// The shifts that bring the scroll viewports of the drawing in `w.px`
/// (`w.scrolls`) to those of the next frame (`now`, with their keys):
/// a viewport in the same place showing the same content moved by whole
/// pixels, less than its size (fresco-scene-groups.md §4).
fn shifts(w: &Win, now: &[(pack::Scroll, u64)]) -> Vec<fresco_compute::damage::Shift> {
    let mut out = Vec::new();
    for (s, key) in now {
        let Some((p, _)) = w.scrolls.iter().find(|(p, k)| p.group == s.group && p.area == s.area && k == key) else {
            continue;
        };
        let d = [s.offset[0] - p.offset[0], s.offset[1] - p.offset[1]];
        let r = d.map(f64::round);
        let (aw, ah) = ((s.area[2] - s.area[0]) as f64, (s.area[3] - s.area[1]) as f64);
        if (d[0] - r[0]).abs() > 1e-4 || (d[1] - r[1]).abs() > 1e-4 || r == [0.0, 0.0] || r[0].abs() >= aw || r[1].abs() >= ah {
            continue;
        }
        out.push(fresco_compute::damage::Shift {
            area: s.area,
            d: [r[0] as i32, r[1] as i32],
            over: s.over.clone(),
            over_before: p.over.clone(),
        });
    }
    out
}

fn raster(
    t2: &fresco_compute::tier2::Tier2,
    w: &mut Win,
    frame: &fresco_compute::Frame,
    width: u32,
    height: u32,
    scrolls: Vec<(pack::Scroll, u64)>,
) -> Result<Option<Option<slot::Rect>>, String> {
    use fresco_compute::damage;
    let n = width as usize * height as usize;
    if w.px.len() != n {
        w.px = vec![0; n];
        w.tiles = None;
    }
    let sig = damage::tile_signatures(frame);
    let h = &frame.header;
    if w.tiles.is_none() {
        t2.render(frame, &mut w.px).map_err(|e| e.to_string())?;
        w.tiles = Some(sig);
        w.scrolls = scrolls;
        return Ok(Some(None));
    }
    let sh = shifts(w, &scrolls);
    w.scrolls = scrolls;
    let held = w.tiles.as_mut().unwrap();
    let out = if sh.is_empty() { damage::Shifted::default() } else { damage::shift(&mut w.px, width, height, &sig, held, &sh) };
    let dirty = damage::dirty(&sig, Some(held));
    let tiles = damage::rects(&dirty, h.tiles_x, h.tiles_y, fresco_compute::tier2::Tier2::MAX_TILE_RECTS);
    let moved = out.changed;
    if tiles.is_empty() && moved.is_none() {
        return Ok(None);
    }
    if let Err(e) = t2.render_rects(frame, &mut w.px, &tiles, &out.pixels) {
        // What `px` holds is now neither frame: draw the next one whole.
        w.tiles = None;
        return Err(e.to_string());
    }
    w.tiles = Some(sig);
    let t = fresco_compute::TILE;
    let r = tiles.iter().map(|r| [r[0] * t, r[1] * t, (r[2] * t).min(width), (r[3] * t).min(height)]).chain(moved).reduce(union);
    Ok(Some(r.filter(|r| r[0] < r[2] && r[1] < r[3])))
}

#[derive(Default)]
struct State {
    wins: HashMap<u32, Win>,
}

/// Where the bundle carries the precompiled kernels.
const BUNDLED_KERNELS: &str = "/usr/local/share/fresco-kernels";

/// The Tier-2 kernels, for pixel mode.
fn kernels() -> Result<fresco_compute::tier2::Tier2, String> {
    let dir = match std::env::var_os("FRESCO_COMPUTE_CACHE") {
        Some(d) => std::path::PathBuf::from(d),
        None if std::path::Path::new(BUNDLED_KERNELS).is_dir() => BUNDLED_KERNELS.into(),
        None => {
            let d = std::path::PathBuf::from("/tmp/fresco-kernels");
            #[cfg(feature = "cranelift")]
            fresco_compute::tier2::Tier2::seed_cache(&d).map_err(|e| format!("compile the kernels: {e}"))?;
            d
        }
    };
    let compiler = atrium_spv_loader::LoaderConfig::production().compile_binary;
    fresco_compute::tier2::Tier2::new(dir.clone(), compiler).map_err(|e| format!("kernels in {}: {e}", dir.display()))
}

fn main() {
    let _ = env_logger::try_init();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--seed-kernels") {
        #[cfg(feature = "cranelift")]
        {
            let Some(d) = args.get(2) else { eprintln!("usage: fresco-renderer --seed-kernels <dir>"); std::process::exit(2) };
            if let Err(e) = fresco_compute::tier2::Tier2::seed_cache(std::path::Path::new(d)) {
                eprintln!("fresco-renderer: {e}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        #[cfg(not(feature = "cranelift"))]
        {
            eprintln!("fresco-renderer: built without the cranelift feature");
            std::process::exit(1);
        }
    }
    // SAFETY: stdin is the broker channel; nothing else here owns it.
    let broker = Arc::new(Chan::from_fd(unsafe { OwnedFd::from_raw_fd(BROKER_FD) }));
    let (client, (sw, sh, refresh), pixels) = match broker.recv() {
        Ok(Recv::Msg(Msg::Client { width, height, refresh_mhz, pixels }, fds)) if fds.len() == 1 => {
            (UnixStream::from(fds.into_iter().next().unwrap()), (width, height, refresh_mhz), pixels)
        }
        _ => {
            log::warn!("fresco-renderer: the first record was not CLIENT with one fd");
            std::process::exit(1);
        }
    };

    // Pixel mode: no kernels, no service (frescod then has no window to
    // show for this client, rather than one rastered outside the jail).
    let t2 = if pixels {
        match kernels() {
            Ok(k) => Some(k),
            Err(e) => {
                log::error!("fresco-renderer: pixel mode: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let cas = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    let (ev_tx, ev_rx) = mpsc::channel();
    comp.lock().unwrap().set_event_sink(ev_tx.clone());
    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp.clone())));
    frontend.lock().unwrap().set_display_mode(sw, sh, refresh);
    let redraw = redraw::RedrawSignal::new();
    let state = Arc::new(Mutex::new(State::default()));

    // The client: its reader, its writer, and its events.
    let (tx, rx) = mpsc::channel::<Outbound>();
    let subs: socket_server::EventSubs = Arc::new(Mutex::new(vec![socket_server::Sub {
        client_id: CLIENT,
        wm: false,
        tx: socket_server::SubTx::Wire(tx.clone()),
    }]));
    socket_server::spawn_event_fanout(ev_rx, subs, redraw.clone(), comp.clone());
    let wstream = client.try_clone().expect("clone the client socket");
    std::thread::spawn(move || socket_server::writer_loop(wstream, rx, CLIENT, None));
    {
        let (fe, redraw) = (frontend.clone(), redraw.clone());
        std::thread::spawn(move || {
            if let Err(e) = socket_server::reader_loop(client, fe, CLIENT, tx, None, false, redraw) {
                log::info!("fresco-renderer: client: {e}");
            }
            std::process::exit(0);
        });
    }

    // The broker's records.
    {
        let (state, comp, redraw, broker) = (state.clone(), comp.clone(), redraw.clone(), broker.clone());
        std::thread::spawn(move || {
            from_broker(&broker, &state, &comp, ev_tx, &redraw);
            std::process::exit(0);
        });
    }

    // Draw on damage.
    let mut scene_state = pack::SceneState::default();
    let mut tex_gen = 0u64;
    let mut last = 0;
    loop {
        last = redraw.wait_past(last, Duration::from_secs(3600));
        let (uploads, clears) = frontend.lock().unwrap().take_pending_uploads();
        if !uploads.is_empty() || !clears.is_empty() {
            scene_state.apply(uploads, clears);
            tex_gen += 1;
        }
        declare(&broker, &state, &comp, (sw, sh));
        draw(&broker, &state, &frontend, &scene_state, tex_gen, t2.as_ref());
    }
}

fn role(r: WmRole) -> Role {
    match r {
        WmRole::Background => Role::Background,
        WmRole::Panel => Role::Panel,
        WmRole::Dialog => Role::Dialog,
        WmRole::Chrome => Role::Chrome,
        WmRole::Document | WmRole::Hud => Role::Document,
    }
}

/// Tell the broker about windows the client created, retitled or
/// destroyed since the last pass.
fn declare(broker: &Chan, state: &Mutex<State>, comp: &Mutex<Compositor>, screen: (u32, u32)) {
    let live: Vec<(u32, Msg)> = {
        let c = comp.lock().unwrap();
        c.windows
            .values()
            .filter(|w| w.id != 0)
            .map(|w| {
                let dim = |v: f32, s: u32| if v >= 1.0 { (v as u32).min(MAX_WINDOW_DIM) } else { s };
                let pos = (w.pos != (0.0, 0.0)).then(|| (w.pos.0 as i32, w.pos.1 as i32));
                let m = Msg::WindowCreate {
                    window: w.id as u32,
                    width: dim(w.size.0, screen.0),
                    height: dim(w.size.1, screen.1),
                    role: role(w.role),
                    pos,
                    title: w.title.clone(),
                };
                (w.id as u32, m)
            })
            .collect()
    };
    let mut s = state.lock().unwrap();
    let gone: Vec<u32> = s.wins.keys().copied().filter(|id| !live.iter().any(|(l, _)| l == id)).collect();
    for id in gone {
        s.wins.remove(&id);
        send(broker, &Msg::WindowDestroy { window: id });
    }
    for (id, m) in live {
        let Msg::WindowCreate { title, .. } = &m else { unreachable!() };
        let room = s.wins.len() < MAX_WINDOWS as usize;
        match s.wins.get_mut(&id) {
            Some(w) => {
                if *title != w.title {
                    w.title = title.clone();
                    send(broker, &Msg::WindowTitle { window: id, title: title.clone() });
                }
            }
            None if room => {
                s.wins.insert(id, Win { title: title.clone(), ..Default::default() });
                send(broker, &m);
            }
            None => log::warn!("fresco-renderer: window {id}: past {MAX_WINDOWS} windows; not shown"),
        }
    }
}

/// Pack every window whose scene changed into a free slot.
fn draw(broker: &Chan, state: &Mutex<State>, fe: &Mutex<EnvelopeFrontend>, scene: &pack::SceneState, tex_gen: u64, t2: Option<&fresco_compute::tier2::Tier2>) {
    let mut s = state.lock().unwrap();
    for (&id, w) in s.wins.iter_mut() {
        let (true, Some((width, height))) = (w.slots.is_some(), w.size) else { continue };
        let Some(wf) = pack::window_frame(&fe.lock().unwrap(), id, (0.0, 0.0), (width as f32, height as f32)) else {
            continue;
        };
        let (frame, drops) = scene.pack_report(&wf, width, height);
        let comp = fe.lock().unwrap().compositor_arc().clone();
        comp.lock().unwrap().set_fill_drops(id as u16, drops);
        let sig = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            fresco_compute::bytes_of(&[frame.header]).hash(&mut h);
            fresco_compute::bytes_of(&frame.ops).hash(&mut h);
            fresco_compute::bytes_of(&frame.tex).hash(&mut h);
            tex_gen.hash(&mut h);
            h.finish()
        };
        if w.sig == Some(sig) {
            continue;
        }
        let Some(i) = (0..2).find(|&i| !w.busy[i]) else { continue }; // FRAME_DONE wakes us
        let written = match t2 {
            // Pixel mode: raster here (only what changed), send the pixels.
            Some(t2) => {
                let scrolls = wf.scrolls.iter().map(|s| {
                    let key = {
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        (s.hash, scene.slot_gens(&s.slots)).hash(&mut h);
                        h.finish()
                    };
                    (s.clone(), key)
                }).collect();
                let damage = match raster(t2, w, &frame, width, height, scrolls) {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        // Not a pixel changed.
                        w.sig = Some(sig);
                        continue;
                    }
                    Err(e) => {
                        log::warn!("fresco-renderer: window {id}: {e}; not drawn");
                        w.forget();
                        w.sig = Some(sig);
                        continue;
                    }
                };
                w.seq = w.seq.wrapping_add(1).max(1);
                w.history.push_back((w.seq, damage));
                while w.history.len() > 4 {
                    w.history.pop_front();
                }
                let copy = w.stale(i);
                log::debug!("fresco-renderer: window {id}: drawing {width}x{height} into slot {i}, damage {damage:?}, copy {copy:?}");
                let r = slot::write_pixels_damage(w.slots.as_mut().unwrap()[i].as_mut_slice(), width, height, &w.px, w.seq, copy, damage)
                    .map_err(str::to_string);
                if r.is_ok() {
                    w.slot_seq[i] = w.seq;
                }
                r
            }
            None => {
                w.seq = w.seq.wrapping_add(1);
                log::debug!("fresco-renderer: window {id}: drawing {width}x{height} into slot {i}");
                slot::write(w.slots.as_mut().unwrap()[i].as_mut_slice(), &frame, w.seq).map_err(str::to_string)
            }
        };
        if let Err(e) = written {
            log::warn!("fresco-renderer: window {id}: {e}; not drawn");
            w.forget();
            w.sig = Some(sig);
            continue;
        }
        w.busy[i] = true;
        w.sig = Some(sig);
        send(broker, &Msg::FrameReady { window: id, slot: i as u32, seq: w.seq });
    }
}

fn send(broker: &Chan, m: &Msg) {
    if let Err(e) = broker.send(m, &[]) {
        log::info!("fresco-renderer: broker: {e}");
        std::process::exit(0);
    }
}

/// Serve the broker's records until it goes.
fn from_broker(
    broker: &Chan,
    state: &Mutex<State>,
    comp: &Mutex<Compositor>,
    events: mpsc::Sender<DisplayEvent>,
    redraw: &redraw::Redraw,
) {
    loop {
        let (m, fds) = match broker.recv() {
            Ok(Recv::Msg(m, fds)) if !m.from_renderer() => (m, fds),
            Ok(Recv::Eof) => return,
            Ok(Recv::Msg(m, _)) => return log::warn!("fresco-renderer: broker sent a renderer's record {m:?}"),
            Ok(Recv::Refused(e)) => return log::warn!("fresco-renderer: broker record refused: {e:?}"),
            Err(e) => return log::warn!("fresco-renderer: broker: {e}"),
        };
        let local = m.window();
        match m {
            Msg::Slots { bytes, .. } => {
                let mut it = fds.into_iter();
                let (Some(a), Some(b), None) = (it.next(), it.next(), it.next()) else {
                    return log::warn!("fresco-renderer: SLOTS without two fds");
                };
                let pair = Shm::open(a, bytes as usize).and_then(|a| Ok([a, Shm::open(b, bytes as usize)?]));
                match pair {
                    Ok(p) => {
                        if let Some(w) = state.lock().unwrap().wins.get_mut(&local) {
                            w.slots = Some(p);
                            w.busy = [false; 2];
                            w.forget();
                        }
                    }
                    Err(e) => return log::warn!("fresco-renderer: slots: {e}"),
                }
            }
            Msg::FrameDone { slot, .. } => {
                if let Some(w) = state.lock().unwrap().wins.get_mut(&local) {
                    w.busy[slot as usize] = false;
                }
            }
            Msg::Geometry { width, height, .. } => {
                if let Some(w) = state.lock().unwrap().wins.get_mut(&local) {
                    w.size = Some((width, height));
                    w.forget();
                }
                // Tells the client, which lays out again.
                comp.lock().unwrap().resize_window(local as u16, width, height);
            }
            Msg::Focus { gained, .. } => {
                let _ = events.send(DisplayEvent::WindowFocusChanged { window_id: local, gained });
            }
            Msg::CloseRequested { .. } => {
                comp.lock().unwrap().request_close(local as u16);
            }
            Msg::Scale { scale_milli, .. } => {
                comp.lock().unwrap().set_window_dpi(local as u16, scale_milli as f32 / 1000.0);
            }
            Msg::Input { input, .. } => {
                let window_id = local;
                let _ = events.send(match input {
                    Input::Key { usage, pressed, modifiers, activation } => {
                        DisplayEvent::InputKey { window_id, hid_usage: usage, pressed, modifiers, activation }
                    }
                    Input::Motion { x, y } => DisplayEvent::InputPointerMotion { window_id, x, y },
                    Input::Button { x, y, button, pressed, modifiers, activation } => {
                        DisplayEvent::InputPointerButton { window_id, x, y, button, pressed, modifiers, activation }
                    }
                    Input::Scroll { dx, dy } => DisplayEvent::InputPointerScroll { window_id, dx, dy },
                });
            }
            _ => unreachable!("from_renderer() covers the rest"),
        }
        redraw.wake();
    }
}
