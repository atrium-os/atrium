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
        let mut modifiers = 0u8;
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
                Rec::Input(i) => {
                    // A flood only slows this link (its socket fills).
                    inputs.take();
                    if !self.remote_on() {
                        continue;
                    }
                    remote_input(&self.comp, sink, i, &mut modifiers);
                }
                _ => unreachable!("from_broker() covers the rest"),
            }
        }
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
            f.chrome = None;
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
        let windows: Vec<Placed> = {
            let c = self.comp.lock().unwrap();
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
                .collect()
        };
        let rec = Rec::Layout { screen: (screen.0.max(1), screen.1.max(1)), windows };
        let mut f = self.fen.lock().unwrap();
        if f.layout.as_ref() == Some(&rec) {
            return;
        }
        f.layout = Some(rec.clone());
        drop(f);
        self.to_fenestra(rec);
    }
}

/// Route one viewer input event as frescod's input injector routes its
/// own: the same cursor, hit-test and focus. Remote input mints no
/// activation token (fenestra.md §11: what a remote press may authorise is
/// decided separately).
fn remote_input(comp: &Arc<Mutex<Compositor>>, sink: &Sender<DisplayEvent>, i: Input, modifiers: &mut u8) {
    use crate::injector_reader::{cursor_here, cursor_to, focused_window};
    let ev = match i {
        Input::Motion { x, y } => {
            let (w, h, _) = crate::screen::get();
            let (lx, ly, target) = cursor_to(comp, x.min(w.max(1) as f32 - 1.0), y.min(h.max(1) as f32 - 1.0));
            DisplayEvent::InputPointerMotion { window_id: target, x: lx, y: ly }
        }
        Input::Button { button, pressed } => {
            let (lx, ly, target) = cursor_here(comp);
            if pressed && button == 1 {
                comp.lock().unwrap().press_focus(target as u16);
            }
            DisplayEvent::InputPointerButton { window_id: target, x: lx, y: ly, button, pressed, modifiers: *modifiers, activation: [0; 16] }
        }
        Input::Key { usage, pressed, modifiers: m } => {
            *modifiers = m;
            DisplayEvent::InputKey { window_id: focused_window(comp), hid_usage: usage, pressed, modifiers: m, activation: [0; 16], repeat: false }
        }
        Input::Scroll { dx, dy } => {
            let (_, _, target) = cursor_here(comp);
            DisplayEvent::InputPointerScroll { window_id: target, dx, dy, phase: 0 }
        }
    };
    let _ = sink.send(ev);
}
