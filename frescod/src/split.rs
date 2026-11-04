//! The broker side of the process split (F4b, docs/spec/fresco-f4-split.md;
//! threat model §2.5.1–2.5.2).
//!
//! In split mode frescod never reads a byte a client wrote. Each connection
//! on the client socket goes to a fresh `fresco-renderer` process, whose
//! stdin is a record channel; the client's socket follows over it in the
//! first record (`CLIENT`). The broker keeps the window table, the WM
//! socket, input and the GPU, and takes from a renderer only fixed records
//! and frames it copies out of slots it created and checks against the size
//! it decided ([`fresco_split::slot::read`]).
//!
//! A renderer that sends anything malformed, names a window it does not
//! have, or opens more than [`MAX_WINDOWS`] is killed and its windows go.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use fresco_protocol::WmRole;
use fresco_scene_server::window::{Compositor, DisplayEvent};
use fresco_split::chan::{Chan, Recv, Shm};
use fresco_split::slot::{self, DEFAULT_SLOT_BYTES};
use fresco_split::{Input, Msg, Role, MAX_WINDOWS, MAX_WINDOW_DIM};

use crate::redraw::Redraw;
use crate::socket_server::{self, EventSubs, Sub, SubTx};

/// One window of a renderer's, as the broker holds it.
struct Win {
    global: u16,
    slots: [Shm; 2],
    /// The size last sent in `GEOMETRY`: the only size a frame may have.
    told: (u32, u32),
}

/// One renderer process.
struct Renderer {
    client_id: u8,
    chan: Chan,
    /// Its windows, by its own (local) ids.
    wins: Mutex<HashMap<u32, Win>>,
    child: Mutex<Child>,
}

impl Renderer {
    fn local_of(&self, global: u32) -> Option<u32> {
        self.wins.lock().unwrap().iter().find(|(_, w)| w.global as u32 == global).map(|(&l, _)| l)
    }
}

pub struct Broker {
    comp: Arc<Mutex<Compositor>>,
    redraw: Redraw,
    screen: (u32, u32, u32),
    bin: PathBuf,
    launcher: Option<String>,
    /// The last good frame of every renderer-drawn window, by global id.
    frames: Mutex<HashMap<u32, fresco_compute::Frame>>,
    renderers: Mutex<HashMap<u8, Arc<Renderer>>>,
}

impl Broker {
    /// `FRESCOD_SPLIT=1` turns split mode on. `FRESCOD_RENDERER_BIN` names
    /// the renderer (default: `fresco-renderer` beside frescod), and
    /// `FRESCOD_RENDERER_LAUNCHER`, if set, is a command line that runs it
    /// confined, with `{client}` replaced by the client id, e.g.
    /// `portcullis exec --daemon --instance fresco{client} org.atrium.fresco.renderer`.
    pub fn from_env(comp: Arc<Mutex<Compositor>>, redraw: Redraw, screen: (u32, u32, u32)) -> Option<Arc<Broker>> {
        if std::env::var("FRESCOD_SPLIT").ok().as_deref() != Some("1") {
            return None;
        }
        let bin = std::env::var_os("FRESCOD_RENDERER_BIN").map(PathBuf::from).unwrap_or_else(|| {
            let me = std::env::current_exe().unwrap_or_default();
            me.with_file_name("fresco-renderer")
        });
        let launcher = std::env::var("FRESCOD_RENDERER_LAUNCHER").ok().filter(|s| !s.trim().is_empty());
        match &launcher {
            Some(l) => eprintln!("frescod: split mode, renderer launched by: {l}"),
            None => eprintln!("frescod: split mode, renderer {} (not confined)", bin.display()),
        }
        Some(Arc::new(Broker {
            comp,
            redraw,
            screen,
            bin,
            launcher,
            frames: Mutex::new(HashMap::new()),
            renderers: Mutex::new(HashMap::new()),
        }))
    }

    /// The hand-off for `socket_server::Shared`.
    pub fn hand_off(self: &Arc<Self>) -> socket_server::HandOff {
        let me = self.clone();
        Arc::new(move |stream, client_id, uid, subs| {
            if let Err(e) = me.clone().start(stream, client_id, uid, subs) {
                eprintln!("frescod: client {client_id}: renderer: {e}");
                socket_server::free_client_id(client_id);
            }
        })
    }

    /// The frame to compose for window `id`, if a renderer draws it.
    pub fn frame(&self, id: u32) -> Option<fresco_compute::Frame> {
        self.frames.lock().unwrap().get(&id).cloned()
    }

    /// Tell renderers the sizes the WM gave their windows since the last
    /// frame. Called once per frame, before the windows are snapshot.
    pub fn sync(&self) {
        let sizes: HashMap<u16, (u32, u32)> = {
            let c = self.comp.lock().unwrap();
            c.windows.iter().map(|(&id, w)| (id, (self.dim(w.size.0, self.screen.0), self.dim(w.size.1, self.screen.1)))).collect()
        };
        let rs: Vec<Arc<Renderer>> = self.renderers.lock().unwrap().values().cloned().collect();
        for r in rs {
            let mut changed = Vec::new();
            for (&local, w) in r.wins.lock().unwrap().iter_mut() {
                if let Some(&s) = sizes.get(&w.global) {
                    if s != w.told {
                        w.told = s;
                        changed.push(Msg::Geometry { window: local, width: s.0, height: s.1 });
                    }
                }
            }
            for m in changed {
                let _ = r.chan.send(&m, &[]);
            }
        }
    }

    fn dim(&self, v: f32, screen: u32) -> u32 {
        if v >= 1.0 { (v as u32).clamp(1, MAX_WINDOW_DIM) } else { screen }
    }

    fn start(self: Arc<Self>, stream: UnixStream, client_id: u8, uid: Option<u32>, subs: EventSubs) -> io::Result<()> {
        let (ours, theirs) = Chan::pair()?;
        let mut cmd = match &self.launcher {
            Some(l) => {
                let l = l.replace("{client}", &client_id.to_string());
                let mut words = l.split_whitespace();
                let mut c = Command::new(words.next().unwrap_or_default());
                c.args(words);
                c
            }
            None => Command::new(&self.bin),
        };
        // stdout carries nothing (a launcher's chatter must not reach the
        // channel); stderr is inherited so a failing renderer is heard.
        cmd.stdin(Stdio::from(theirs.into_fd())).stdout(Stdio::null());
        let child = cmd.spawn()?;
        ours.send(&Msg::Client { width: self.screen.0, height: self.screen.1, refresh_mhz: self.screen.2 }, &[stream.as_raw_fd()])?;
        drop(stream);
        log::info!("frescod: client {client_id} (uid {uid:?}) -> renderer pid {}", child.id());

        let r = Arc::new(Renderer { client_id, chan: ours, wins: Mutex::new(HashMap::new()), child: Mutex::new(child) });
        self.renderers.lock().unwrap().insert(client_id, r.clone());

        // Its windows' events, as records.
        let (ev_tx, ev_rx) = mpsc::channel::<DisplayEvent>();
        subs.lock().unwrap().push(Sub { client_id, wm: false, tx: SubTx::Split(ev_tx) });
        {
            let r = r.clone();
            std::thread::Builder::new().name(format!("frescod-split-ev-{client_id}")).spawn(move || {
                for ev in ev_rx.iter() {
                    if let Some(m) = event_record(&r, &ev) {
                        if r.chan.send(&m, &[]).is_err() {
                            break;
                        }
                    }
                }
            })?;
        }

        // Its records.
        std::thread::Builder::new().name(format!("frescod-split-{client_id}")).spawn(move || {
            let why = self.serve(&r, uid.unwrap_or(0));
            log::info!("frescod: client {client_id}: renderer gone: {why}");
            self.reap(&r, &subs);
        })?;
        Ok(())
    }

    /// Serve one renderer's records until it goes or misbehaves; returns why.
    fn serve(&self, r: &Renderer, uid: u32) -> String {
        loop {
            let m = match r.chan.recv() {
                Ok(Recv::Msg(m, fds)) if m.from_renderer() && fds.is_empty() => m,
                Ok(Recv::Msg(m, _)) => return format!("refused {m:?} (not a renderer's record, or fds attached)"),
                Ok(Recv::Refused(e)) => return format!("refused a record: {}", e.0),
                Ok(Recv::Eof) => return "closed".into(),
                Err(e) => return e.to_string(),
            };
            if let Err(e) = self.record(r, uid, m) {
                return e;
            }
        }
    }

    fn record(&self, r: &Renderer, uid: u32, m: Msg) -> Result<(), String> {
        let local = m.window();
        match m {
            Msg::WindowCreate { width, height, role, pos, title, .. } => {
                let mut wins = r.wins.lock().unwrap();
                if wins.contains_key(&local) || wins.len() >= MAX_WINDOWS as usize {
                    return Err(format!("window {local}: already open, or too many windows"));
                }
                let slots = [Shm::create(DEFAULT_SLOT_BYTES as usize), Shm::create(DEFAULT_SLOT_BYTES as usize)];
                let [Ok(a), Ok(b)] = slots else { return Err("cannot create frame slots".into()) };
                let global = {
                    let mut c = self.comp.lock().unwrap();
                    let g = c.create(r.client_id as u32, (width, height));
                    if let Some(w) = c.windows.get_mut(&g) {
                        w.title = title;
                        w.role = wm_role(role);
                        w.owner_uid = uid;
                        if let Some((x, y)) = pos {
                            w.pos = (x as f32, y as f32);
                        }
                    }
                    g
                };
                r.chan
                    .send(&Msg::Slots { window: local, bytes: DEFAULT_SLOT_BYTES }, &[a.fd(), b.fd()])
                    .and_then(|_| r.chan.send(&Msg::Geometry { window: local, width, height }, &[]))
                    .map_err(|e| e.to_string())?;
                wins.insert(local, Win { global, slots: [a, b], told: (width, height) });
            }
            Msg::WindowDestroy { .. } => {
                let w = r.wins.lock().unwrap().remove(&local).ok_or_else(|| format!("destroy of unknown window {local}"))?;
                self.drop_window(r, w.global);
            }
            Msg::WindowTitle { title, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global).ok_or("title of unknown window")?;
                if let Some(w) = self.comp.lock().unwrap().windows.get_mut(&global) {
                    w.title = title;
                }
            }
            Msg::FrameReady { slot: s, .. } => {
                let got = {
                    let wins = r.wins.lock().unwrap();
                    let w = wins.get(&local).ok_or_else(|| format!("frame for unknown window {local}"))?;
                    (w.global, slot::read(w.slots[s as usize].as_slice(), w.told.0, w.told.1))
                };
                match got {
                    (global, Ok((_, frame))) => {
                        self.frames.lock().unwrap().insert(global as u32, frame);
                        self.redraw.wake();
                    }
                    // A frame drawn before the latest GEOMETRY arrived has the
                    // old size: dropped, the renderer draws again.
                    (_, Err(e)) => log::debug!("frescod: client {} window {local}: {e}", r.client_id),
                }
                r.chan.send(&Msg::FrameDone { window: local, slot: s }, &[]).map_err(|e| e.to_string())?;
            }
            _ => unreachable!("serve() admits only a renderer's records"),
        }
        Ok(())
    }

    fn drop_window(&self, r: &Renderer, global: u16) {
        self.comp.lock().unwrap().destroy_with_focus_shift(global, r.client_id as u32);
        self.frames.lock().unwrap().remove(&(global as u32));
        self.redraw.wake();
    }

    /// The renderer is gone or misbehaved: end it and everything it had.
    fn reap(&self, r: &Arc<Renderer>, subs: &EventSubs) {
        {
            // End the channel first: the renderer exits on end-of-file, and a
            // launcher (portcullis exec) then tears its jail down. Killing the
            // launcher outright would leave the jail behind; the kill is
            // only the backstop for one that does not go.
            r.chan.shutdown();
            let mut c = r.child.lock().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while matches!(c.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if matches!(c.try_wait(), Ok(None)) {
                log::warn!("frescod: client {}: renderer did not exit; killing it", r.client_id);
                let _ = c.kill();
            }
            let _ = c.wait();
        }
        subs.lock().unwrap().retain(|s| s.wm || s.client_id != r.client_id);
        let wins: Vec<Win> = r.wins.lock().unwrap().drain().map(|(_, w)| w).collect();
        for w in wins {
            self.drop_window(r, w.global);
        }
        self.renderers.lock().unwrap().remove(&r.client_id);
        socket_server::free_client_id(r.client_id);
    }
}

/// The record an event for one of `r`'s windows becomes, in its local id.
/// Window lifetime events are the renderer's own business, and sizes go
/// by [`Broker::sync`]; events over window 0 (the shared background) have
/// no owner yet and do not reach renderers (F4c gives it one).
fn event_record(r: &Renderer, ev: &DisplayEvent) -> Option<Msg> {
    use DisplayEvent as E;
    let (global, input) = match *ev {
        E::WindowFocusChanged { window_id, gained } => return Some(Msg::Focus { window: r.local_of(window_id)?, gained }),
        E::WindowCloseRequested { window_id } => return Some(Msg::CloseRequested { window: r.local_of(window_id)? }),
        E::InputKey { window_id, hid_usage, pressed, modifiers } => (window_id, Input::Key { usage: hid_usage, pressed, modifiers }),
        E::InputPointerMotion { window_id, x, y } => (window_id, Input::Motion { x, y }),
        E::InputPointerButton { window_id, x, y, button, pressed, modifiers } => {
            (window_id, Input::Button { x, y, button, pressed, modifiers })
        }
        E::InputPointerScroll { window_id, dx, dy } => (window_id, Input::Scroll { dx, dy }),
        _ => return None,
    };
    let ok = |v: f32| v.is_finite();
    match input {
        Input::Motion { x, y } | Input::Button { x, y, .. } if !(ok(x) && ok(y)) => return None,
        Input::Scroll { dx, dy } if !(ok(dx) && ok(dy)) => return None,
        _ => {}
    }
    Some(Msg::Input { window: r.local_of(global)?, input })
}

fn wm_role(r: Role) -> WmRole {
    match r {
        Role::Document => WmRole::Document,
        Role::Dialog => WmRole::Dialog,
        Role::Panel => WmRole::Panel,
        Role::Background => WmRole::Background,
        Role::Chrome => WmRole::Chrome,
    }
}
