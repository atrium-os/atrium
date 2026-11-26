//! The broker's end of Fenestra (fenestra.md §3.2).
//!
//! `fenestrad` connects to `FRESCOD_FENESTRA_SOCK` (its uid must be one of
//! `FRESCOD_FENESTRA_UIDS`; one link at a time) and speaks
//! [`fenestra_link`] records. While it has a session on, every renderer is
//! given two stream slots (`Msg::Remote`) and streams all its windows; the
//! broker relays a want to the renderer that has the window, and the
//! renderer's answer back, copying the bytes out of slots it created and
//! checking only their length and ticket: it never parses them. Each frame
//! it sends the layout (where the windows are) when it moved.
//!
//! frescod gets no network: the link is a local socket, and the network is
//! fenestrad's session jails' (§3.2, §11.5).

use std::collections::HashMap;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use fenestra_link::{Input, Placed, Rec};
use fresco_scene_server::window::{Compositor, DisplayEvent};
use fresco_split::chan::Shm;
use fresco_split::Msg;

use super::{Broker, Renderer};

/// Each renderer's two stream slots.
pub(super) const STREAM_SLOT_BYTES: usize = 4 << 20;
/// Records queued for fenestrad; a fenestrad that lets this fill is cut
/// off rather than waited on (no renderer or frame waits on it).
const LINK_QUEUE: usize = 4096;
/// Wants outstanding at one renderer.
const MAX_TICKETS: usize = 256;
/// Input events a second from the link, and the burst it may save up.
const INPUT_PER_S: f64 = 2000.0;
const INPUT_BURST: f64 = 400.0;

#[derive(Default)]
pub(super) struct Link {
    tx: Option<SyncSender<Rec>>,
    sock: Option<UnixStream>,
    on: bool,
    layout: Option<Rec>,
    chrome: Option<Rec>,
    /// The chrome could not be carried (logged once until it can again).
    chrome_refused: bool,
    /// The viewer's output (width, height, scale), and whether the frame
    /// loop has yet to act on a change of it.
    output: Option<(u32, u32, u32)>,
    output_moved: bool,
    /// Per-app resizes from the viewer since the last frame, latest a
    /// window; only placed windows get here, so it is bounded by the
    /// layout.
    resizes: HashMap<u32, (u32, u32)>,
}

/// A renderer's side of the remote session.
#[derive(Default)]
pub(super) struct RStream {
    slots: Option<[Shm; 2]>,
    /// Outstanding wants: ticket to the local window asked for.
    tickets: HashMap<u32, u32>,
}

impl Broker {
    /// Listen for fenestrad on `path`.
    /// Viewers' input goes to `sink` as frescod's own devices' does.
    pub fn serve_fenestra(self: &Arc<Self>, path: &std::path::Path, uids: Vec<u32>, sink: Sender<DisplayEvent>) -> io::Result<()> {
        if std::fs::symlink_metadata(path).is_ok_and(|m| {
            use std::os::unix::fs::FileTypeExt;
            m.file_type().is_socket()
        }) {
            std::fs::remove_file(path)?;
        }
        let l = UnixListener::bind(path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            // Anyone may connect; only the listed uids are kept.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
        }
        eprintln!("frescod: fenestra link on {} (uids {uids:?})", path.display());
        let me = self.clone();
        std::thread::Builder::new().name("frescod-fenestra-accept".into()).spawn(move || {
            for s in l.incoming() {
                let s = match s {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("frescod: fenestra accept: {e}");
                        return;
                    }
                };
                let uid = crate::socket_server::peer_xucred(&s).map(|(_, u)| u);
                if !uid.is_some_and(|u| uids.contains(&u)) {
                    log::warn!("frescod: fenestra link from uid {uid:?} refused");
                    continue;
                }
                if me.fen.lock().unwrap().sock.is_some() {
                    log::warn!("frescod: a second fenestra link refused");
                    continue;
                }
                let (me, sink) = (me.clone(), sink.clone());
                let _ = std::thread::Builder::new().name("frescod-fenestra".into()).spawn(move || {
                    let why = me.fenestra(s, &sink);
                    log::info!("frescod: fenestra link ended: {why}");
                    me.end_link();
                });
            }
        })?;
        Ok(())
    }

    fn fenestra(self: &Arc<Self>, s: UnixStream, sink: &Sender<DisplayEvent>) -> String {
        let (tx, rx) = mpsc::sync_channel::<Rec>(LINK_QUEUE);
        let (Ok(mut w), Ok(mut r)) = (s.try_clone(), s.try_clone()) else { return "cannot clone the socket".into() };
        {
            let mut f = self.fen.lock().unwrap();
            f.tx = Some(tx);
            f.sock = Some(s);
            f.layout = None;
            f.resizes.clear();
        }
        let _ = std::thread::Builder::new().name("frescod-fenestra-out".into()).spawn(move || {
            for rec in rx.iter() {
                if fenestra_link::write(&mut w, &rec).is_err() {
                    let _ = w.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        });
        let mut inputs = super::Bucket::new(INPUT_PER_S, INPUT_BURST);
        let mut pointer = Pointer::default();
        loop {
            let rec = match fenestra_link::read(&mut r) {
                Ok(Some(rec)) if !rec.from_broker() => rec,
                Ok(Some(rec)) => return format!("refused {rec:?} (a broker's record)"),
                Ok(None) => return "closed".into(),
                Err(e) => return e.to_string(),
            };
            match rec {
                Rec::Session { on } => self.set_remote(on),
                Rec::Want { window, ticket, base } => self.want(window, ticket, base),
                Rec::Output { width, height, scale_milli } => {
                    let mut f = self.fen.lock().unwrap();
                    if f.on && f.output != Some((width, height, scale_milli)) {
                        f.output = Some((width, height, scale_milli));
                        f.output_moved = true;
                        drop(f);
                        self.redraw.wake();
                    }
                }
                Rec::Input(i) => {
                    // A flood only slows this link (its socket fills).
                    inputs.take();
                    if !self.remote_on() {
                        continue;
                    }
                    // A pointer in, focus on or size for a window is taken only in a per-app
                    // session (no WM: in a desktop the person sees the WM's
                    // stacking), and only for a window this link was last
                    // told of: the session's own check is a jail's that
                    // parses peer bytes, so it is not the one relied on.
                    let i = match i {
                        Input::MotionIn { window, .. } | Input::Focus { window } | Input::Resize { window, .. } if !self.placed_in_apps(window) => continue,
                        Input::Focus { window } => {
                            if let Ok(id) = u16::try_from(window) {
                                if self.comp.lock().unwrap().remote_focus(id, crate::activation::sheet_uids()) {
                                    self.redraw.wake();
                                }
                            }
                            continue;
                        }
                        Input::Resize { window, width, height } => {
                            // Latest wins; the frame applies at most one
                            // a window (`apply_remote_resizes`).
                            self.fen.lock().unwrap().resizes.insert(window, (width, height));
                            self.redraw.wake();
                            continue;
                        }
                        i => i,
                    };
                    let placed = |w: u32| self.placed_in_apps(w);
                    remote_input(&self.comp, sink, i, &mut pointer, &placed);
                }
                _ => unreachable!("from_broker() covers the rest"),
            }
        }
    }

    /// Once a frame: the viewer's latest size for each window it resized,
    /// each side at most the screen's, for a window still placed.
    pub(super) fn apply_remote_resizes(&self) {
        let r = std::mem::take(&mut self.fen.lock().unwrap().resizes);
        if r.is_empty() {
            return;
        }
        let screen = self.screen();
        for (window, (w, h)) in r {
            let Ok(id) = u16::try_from(window) else { continue };
            if self.placed_in_apps(window) {
                self.comp.lock().unwrap().remote_resize(id, w, h, (screen.0.max(1), screen.1.max(1)));
            }
        }
    }

    /// Whether `window` was in the last layout sent on this link, and
    /// that layout was a per-app one with no WM running now.
    fn placed_in_apps(&self, window: u32) -> bool {
        if self.comp.lock().unwrap().wm_running {
            return false;
        }
        let f = self.fen.lock().unwrap();
        matches!(&f.layout, Some(Rec::Layout { desktop: false, windows, .. }) if windows.iter().any(|p| p.window == window))
    }

    fn end_link(&self) {
        {
            let mut f = self.fen.lock().unwrap();
            if let Some(s) = f.sock.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            f.tx = None;
        }
        self.set_remote(false);
    }

    /// Queue a record for fenestrad, or cut it off if it stopped reading.
    pub(super) fn to_fenestra(&self, rec: Rec) {
        let mut f = self.fen.lock().unwrap();
        let Some(tx) = f.tx.as_ref() else { return };
        match tx.try_send(rec) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                log::warn!("frescod: fenestrad stopped reading; cutting the link");
                f.tx = None;
                if let Some(s) = f.sock.take() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }

    fn set_remote(&self, on: bool) {
        {
            let mut f = self.fen.lock().unwrap();
            if f.on == on {
                return;
            }
            f.on = on;
            f.layout = None;
            f.resizes.clear();
            f.chrome = None;
            if !on && f.output.take().is_some() {
                f.output_moved = true;
            }
        }
        log::info!("frescod: remote session {}", if on { "on" } else { "off" });
        let rs: Vec<Arc<Renderer>> = self.renderers.lock().unwrap().values().cloned().collect();
        for r in rs {
            self.give_stream(&r, on);
        }
        self.redraw.wake();
    }

    /// Start or stop `r`'s streams. Called for a new renderer too.
    pub(super) fn give_stream(&self, r: &Renderer, on: bool) {
        let mut st = r.stream.lock().unwrap();
        st.tickets.clear();
        if !on {
            st.slots = None;
            let _ = r.tell(&Msg::Remote { on: false, bytes: 0 }, &[]);
            return;
        }
        let [Ok(a), Ok(b)] = [Shm::create(STREAM_SLOT_BYTES), Shm::create(STREAM_SLOT_BYTES)] else {
            log::warn!("frescod: client {}: no stream slots", r.client_id);
            return;
        };
        if r.tell(&Msg::Remote { on: true, bytes: STREAM_SLOT_BYTES as u32 }, &[a.fd(), b.fd()]).is_ok() {
            st.slots = Some([a, b]);
        }
    }

    pub(super) fn remote_on(&self) -> bool {
        self.fen.lock().unwrap().on
    }

    fn want(&self, global: u32, ticket: u32, base: Option<(u64, u64)>) {
        let rs: Vec<Arc<Renderer>> = self.renderers.lock().unwrap().values().cloned().collect();
        for r in rs {
            let Some(local) = r.local_of(global) else { continue };
            let mut st = r.stream.lock().unwrap();
            if st.slots.is_none() || st.tickets.len() >= MAX_TICKETS || st.tickets.contains_key(&ticket) {
                break;
            }
            st.tickets.insert(ticket, local);
            drop(st);
            let _ = r.tell(&Msg::StreamWant { window: local, ticket, base }, &[]);
            return;
        }
        // No such window (or no room): the want ends with nothing.
        self.to_fenestra(Rec::End { window: global, ticket, now: None });
    }

    /// A renderer's stream record (serve() admitted its kind).
    pub(super) fn stream_record(&self, r: &Renderer, m: Msg) -> Result<(), String> {
        let local = m.window();
        let global = r.wins.lock().unwrap().get(&local).map(|w| w.global as u32);
        match m {
            Msg::StreamChanged { .. } => {
                if let Some(g) = global {
                    self.to_fenestra(Rec::Changed { window: g });
                }
            }
            Msg::StreamData { ticket, slot, len, .. } => {
                let bytes = {
                    let st = r.stream.lock().unwrap();
                    if st.tickets.get(&ticket) != Some(&local) {
                        return Err(format!("stream data for ticket {ticket} it was not asked for"));
                    }
                    let Some(slots) = st.slots.as_ref() else { return Err("stream data with no remote session".into()) };
                    let s = slots[slot as usize].as_slice();
                    if len == 0 || len as usize > s.len() || len as usize > fenestra_link::MAX_DATA {
                        return Err(format!("stream data of {len} bytes"));
                    }
                    s[..len as usize].to_vec()
                };
                r.tell(&Msg::StreamDone { slot }, &[])?;
                if let Some(g) = global {
                    self.to_fenestra(Rec::Data { window: g, ticket, bytes });
                }
            }
            Msg::StreamEnd { ticket, now, .. } => {
                if r.stream.lock().unwrap().tickets.remove(&ticket) != Some(local) {
                    return Err(format!("stream end for ticket {ticket} it was not asked for"));
                }
                // A window that went meanwhile ends with nothing.
                let g = global.unwrap_or(0);
                if g != 0 {
                    self.to_fenestra(Rec::End { window: g, ticket, now });
                }
            }
            _ => unreachable!("only stream records come here"),
        }
        Ok(())
    }

    /// The viewer's output, if it changed since the last call: `Some(None)`
    /// when the session that set it ended (fenestra.md §4.1).
    pub(crate) fn take_output(&self) -> Option<Option<(u32, u32, u32)>> {
        let mut f = self.fen.lock().unwrap();
        std::mem::replace(&mut f.output_moved, false).then_some(f.output)
    }

    /// Once a frame, while a session is on: frescod's own chrome layer
    /// (`c`, at the screen's origin) and the background, if they changed.
    /// A viewer draws them over and under the windows (fenestra.md §11.5).
    pub(crate) fn send_chrome(&self, c: &crate::pack::WindowFrame) {
        if !self.remote_on() {
            return;
        }
        let made = if c.paths.is_empty() && c.batches.is_empty() && c.glyph_batches.is_empty() && c.fills.is_empty() && c.frame.is_none() && c.pixels.is_none() && c.pos == (0.0, 0.0) {
            let lut = fresco_compute::lut();
            let bg = fresco_compute::CLEAR_COLOR;
            let enc = |v: f32| fresco_compute::reference::encode(&lut, v) as u8;
            fenestra_link::chrome::Chrome::from_rects(
                [enc(bg[0]), enc(bg[1]), enc(bg[2])],
                c.size,
                c.rects.iter().map(|n| ([n.position[0], n.position[1], n.size[0], n.size[1]], n.color, n.radius)),
            )
            .map_err(|e| e.to_string())
        } else {
            Err("a chrome layer with more than rects".to_string())
        };
        let mut f = self.fen.lock().unwrap();
        let rec = match made {
            Ok(ch) => {
                f.chrome_refused = false;
                Rec::Chrome(ch)
            }
            Err(e) => {
                if !std::mem::replace(&mut f.chrome_refused, true) {
                    log::warn!("frescod: the chrome is not sent to viewers: {e}");
                }
                return;
            }
        };
        if f.chrome.as_ref() == Some(&rec) {
            return;
        }
        f.chrome = Some(rec.clone());
        drop(f);
        self.to_fenestra(rec);
    }

    /// Once a frame, while a session is on: where the renderers' windows
    /// are, bottom to top, if that moved.
    pub(super) fn send_layout(&self) {
        if !self.remote_on() {
            return;
        }
        let owned: HashMap<u32, ()> = self
            .renderers
            .lock()
            .unwrap()
            .values()
            .flat_map(|r| r.wins.lock().unwrap().values().map(|w| (w.global as u32, ())).collect::<Vec<_>>())
            .collect();
        let screen = self.screen();
        let (desktop, windows): (bool, Vec<Placed>) = {
            let c = self.comp.lock().unwrap();
            let desktop = c.wm_running;
            (desktop,
            c.z_order
                .iter()
                .filter_map(|id| {
                    let w = c.windows.get(id)?;
                    if !owned.contains_key(&(*id as u32)) || !w.rendering {
                        return None;
                    }
                    Some(Placed {
                        window: *id as u32,
                        x: w.pos.0.round() as i32,
                        y: w.pos.1.round() as i32,
                        width: self.dim(w.size.0, screen.0),
                        height: self.dim(w.size.1, screen.1),
                    })
                })
                .take(fenestra_link::MAX_LAYOUT)
                .collect())
        };
        let rec = Rec::Layout { screen: (screen.0.max(1), screen.1.max(1)), desktop, windows };
        let mut f = self.fen.lock().unwrap();
        if f.layout.as_ref() == Some(&rec) {
            return;
        }
        f.layout = Some(rec.clone());
        drop(f);
        self.to_fenestra(rec);
    }
}

/// The viewer's pointer as the link last moved it.
#[derive(Default)]
struct Pointer {
    modifiers: u8,
    /// Per-app sessions: the window the pointer is in and where, which
    /// its buttons and scrolls go to (the host may stack another window
    /// over that point).
    pinned: Option<(u32, f32, f32)>,
}

/// Route one viewer input event as frescod's input injector routes its
/// own: the same cursor, hit-test and focus. Remote input mints no
/// activation token (fenestra.md §11: what a remote press may authorise is
/// decided separately).
fn remote_input(comp: &Arc<Mutex<Compositor>>, sink: &Sender<DisplayEvent>, i: Input, p: &mut Pointer, placed: &dyn Fn(u32) -> bool) {
    use crate::injector_reader::{cursor_here, cursor_to, focused_window};
    // Where a button or scroll lands: in the pinned window (asked again,
    // so one gone, held or no longer placed since takes nothing), else
    // the hit-test's.
    let at = |p: &Pointer| match p.pinned {
        Some((w, _, _)) if !placed(w) => (0.0, 0.0, 0),
        Some((w, x, y)) => {
            let (t, (lx, ly)) = comp.lock().unwrap().pointer_in(w as u16, x, y);
            (lx, ly, t as u32)
        }
        None => cursor_here(comp),
    };
    let ev = match i {
        Input::Motion { x, y } => {
            p.pinned = None;
            let (w, h, _) = crate::screen::get();
            let (lx, ly, target) = cursor_to(comp, x.min(w.max(1) as f32 - 1.0), y.min(h.max(1) as f32 - 1.0));
            DisplayEvent::InputPointerMotion { window_id: target, x: lx, y: ly }
        }
        Input::MotionIn { window, x, y } => {
            let Ok(id) = u16::try_from(window) else { return };
            let (t, (lx, ly)) = comp.lock().unwrap().pointer_in(id, x, y);
            p.pinned = (t != 0).then_some((window, x, y));
            DisplayEvent::InputPointerMotion { window_id: t as u32, x: lx, y: ly }
        }
        Input::Button { button, pressed } => {
            let (lx, ly, target) = at(p);
            if pressed && button == 1 {
                comp.lock().unwrap().press_focus(target as u16);
            }
            DisplayEvent::InputPointerButton { window_id: target, x: lx, y: ly, button, pressed, modifiers: p.modifiers, activation: [0; 16] }
        }
        Input::Key { usage, pressed, modifiers: m } => {
            p.modifiers = m;
            DisplayEvent::InputKey { window_id: focused_window(comp), hid_usage: usage, pressed, modifiers: m, activation: [0; 16], repeat: false }
        }
        Input::Scroll { dx, dy } => {
            let (_, _, target) = at(p);
            DisplayEvent::InputPointerScroll { window_id: target, dx, dy, phase: 0 }
        }
        // Taken by the link loop itself.
        Input::Focus { .. } | Input::Resize { .. } => return,
    };
    let _ = sink.send(ev);
}
