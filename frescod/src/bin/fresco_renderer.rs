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
//! - fd 3 is the client's socket, served with the same reader/writer and
//!   scene server frescod runs in-process;
//! - fd 4 is the broker channel. The renderer declares the client's
//!   windows, the broker answers with two frame slots per window and the
//!   size it decided; the renderer packs each window's scene into a free
//!   slot and says `FRAME_READY`.
//!
//! `FRESCO_SCREEN=W,H,REFRESH_MHZ` gives the display mode clients may ask
//! for. The renderer exits when either the client or the broker goes.

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
#[path = "../socket_server.rs"]
mod socket_server;

/// The client's id inside this renderer: it is the only one.
const CLIENT: u8 = 1;
const CLIENT_FD: i32 = 3;
const BROKER_FD: i32 = 4;

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
}

#[derive(Default)]
struct State {
    wins: HashMap<u32, Win>,
}

fn main() {
    let _ = env_logger::try_init();
    let (sw, sh, refresh) = screen();

    // SAFETY: frescod passed these two fds and nothing else owns them.
    let (client, broker) = unsafe {
        (UnixStream::from_raw_fd(CLIENT_FD), Chan::from_fd(OwnedFd::from_raw_fd(BROKER_FD)))
    };
    let broker = Arc::new(broker);

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
        draw(&broker, &state, &frontend, &scene_state, tex_gen);
    }
}

fn screen() -> (u32, u32, u32) {
    let v: Vec<u32> = std::env::var("FRESCO_SCREEN")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.parse().ok())
        .collect();
    match v[..] {
        [w, h, r] if w > 0 && h > 0 => (w, h, r),
        _ => (1280, 720, 0),
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
fn draw(broker: &Chan, state: &Mutex<State>, fe: &Mutex<EnvelopeFrontend>, scene: &pack::SceneState, tex_gen: u64) {
    let mut s = state.lock().unwrap();
    for (&id, w) in s.wins.iter_mut() {
        let (Some(slots), Some((width, height))) = (&mut w.slots, w.size) else { continue };
        let Some(wf) = pack::window_frame(&fe.lock().unwrap(), id, (0.0, 0.0), (width as f32, height as f32)) else {
            continue;
        };
        let frame = scene.pack(&wf, width, height);
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
        w.seq = w.seq.wrapping_add(1);
        if let Err(e) = slot::write(slots[i].as_mut_slice(), &frame, w.seq) {
            log::warn!("fresco-renderer: window {id}: {e}; not drawn");
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
                    w.sig = None;
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
            Msg::Input { input, .. } => {
                let window_id = local;
                let _ = events.send(match input {
                    Input::Key { usage, pressed, modifiers } => DisplayEvent::InputKey { window_id, hid_usage: usage, pressed, modifiers },
                    Input::Motion { x, y } => DisplayEvent::InputPointerMotion { window_id, x, y },
                    Input::Button { x, y, button, pressed, modifiers } => {
                        DisplayEvent::InputPointerButton { window_id, x, y, button, pressed, modifiers }
                    }
                    Input::Scroll { dx, dy } => DisplayEvent::InputPointerScroll { window_id, dx, dy },
                });
            }
            _ => unreachable!("from_renderer() covers the rest"),
        }
        redraw.wake();
    }
}
