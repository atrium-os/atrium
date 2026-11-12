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
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A video layer's slots (fresco-video.md §10, decision 6): the renderer
/// converts each frame in its jail into one, BGRA at `size`.
struct VRing {
    slots: [Shm; 2],
    size: (u32, u32),
}

/// One renderer process.
struct Renderer {
    client_id: u8,
    /// Names this connection on the chrome channel: unique for frescod's
    /// lifetime, so a stale handle never names a later client.
    handle: u32,
    /// The chrome connection that opened this client, if one did; only it
    /// may set the client's origins.
    opener: Option<u64>,
    /// Origins the opener set, by local window id (set before or after the
    /// renderer declares the window).
    origins: Mutex<HashMap<u32, String>>,
    chan: Chan,
    /// Formats of the drag its next DRAG_BEGIN starts.
    drag_formats: Mutex<Vec<String>>,
    /// Set when the broker has let it go.
    gone: std::sync::atomic::AtomicBool,
    /// Its windows, by its own (local) ids.
    wins: Mutex<HashMap<u32, Win>>,
    /// Its video layers' slots, by (local window, layer).
    videos: Mutex<HashMap<(u32, u32), VRing>>,
    proc_: Mutex<Proc>,
}

impl Renderer {
    /// Send without waiting. A renderer whose channel is full has stopped
    /// reading: it is retired (its channel shut, so its serving thread
    /// reaps it) rather than waited on, because whoever is sending (the
    /// frame loop, an input event) must not wait on a renderer (F3).
    fn tell(&self, m: &Msg, fds: &[std::os::fd::RawFd]) -> Result<(), String> {
        self.chan.send_now(m, fds).map_err(|e| {
            self.chan.shutdown();
            // A full channel: EAGAIN, or on FreeBSD a short write (the
            // partial record goes nowhere, the channel is shut).
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::WriteZero) {
                log::info!("frescod: client {}: renderer stopped reading; retiring it", self.client_id);
                "stopped reading".to_string()
            } else {
                e.to_string()
            }
        })
    }

    fn local_of(&self, global: u32) -> Option<u32> {
        self.wins.lock().unwrap().iter().find(|(_, w)| w.global as u32 == global).map(|(&l, _)| l)
    }
}

pub struct Broker {
    comp: Arc<Mutex<Compositor>>,
    redraw: Redraw,
    screen: (u32, u32, u32),
    launch: Launch,
    /// The last good frame of every renderer-drawn window, by global id.
    frames: Mutex<HashMap<u32, Arc<fresco_compute::Frame>>>,
    /// Pixel mode: renderers raster their own windows (in their jails) and
    /// send pixels; the last good pixels of each window, by global id.
    pixels: bool,
    drawn: Arc<Mutex<HashMap<u32, Arc<crate::pack::Pixels>>>>,
    /// Each window's drawing before its latest, kept to be patched into
    /// the next one while the latest is still being composed (so a frame
    /// costs what changed, not a copy of the window).
    spare: Mutex<HashMap<u32, Arc<crate::pack::Pixels>>>,
    drawings: std::sync::atomic::AtomicU64,
    renderers: Mutex<HashMap<u8, Arc<Renderer>>>,
    subs: Mutex<Option<EventSubs>>,
    next_handle: std::sync::atomic::AtomicU32,
}

/// Most connections one chrome peer may open at once.
const MAX_OPENED: usize = 64;

impl Broker {
    /// Split mode is ON unless `FRESCOD_SPLIT=0`, when the renderer can
    /// take renderer-drawn frames (`takes_frames`: compute, the default,
    /// and gpu; Vulkan cannot, so with it split mode stays off unless
    /// `FRESCOD_SPLIT=1` forces it, and frescod says so).
    ///
    /// How a renderer is started:
    /// - by default frescod asks portcullisd for a one-shot jail of
    ///   `FRESCOD_RENDERER_APP` (default `org.atrium.fresco.renderer`); the
    ///   system policy's worker rule runs it as `_fresco-render` with the
    ///   bundle's memory cap. If portcullisd cannot start it, the client is
    ///   refused: it fails closed, never falls back to an unconfined
    ///   renderer.
    /// - `FRESCOD_RENDERER_LAUNCHER=<command>` runs that command instead,
    ///   with `{client}` replaced by the client id (the fuzz harness);
    /// - `FRESCOD_RENDERER_LAUNCHER=unconfined` runs `FRESCOD_RENDERER_BIN`
    ///   (default: `fresco-renderer` beside frescod) directly, for
    ///   development only.
    pub fn from_env(comp: Arc<Mutex<Compositor>>, redraw: Redraw, screen: (u32, u32, u32), takes_frames: bool, pixels: bool) -> Option<Arc<Broker>> {
        match std::env::var("FRESCOD_SPLIT").ok().as_deref() {
            Some("0") => {
                eprintln!("frescod: WARNING: split mode off (FRESCOD_SPLIT=0): clients are parsed inside frescod");
                return None;
            }
            Some("1") => {
                if !takes_frames {
                    eprintln!("frescod: WARNING: FRESCOD_SPLIT=1 with a renderer that cannot draw renderer frames; their windows stay blank");
                }
            }
            _ if !takes_frames => {
                eprintln!("frescod: WARNING: split mode off: this renderer cannot draw renderer frames \
                           (use FRESCOD_RENDERER=compute or gpu); clients are parsed inside frescod");
                return None;
            }
            _ => {}
        }
        let bin = std::env::var_os("FRESCOD_RENDERER_BIN").map(PathBuf::from).unwrap_or_else(|| {
            let me = std::env::current_exe().unwrap_or_default();
            me.with_file_name("fresco-renderer")
        });
        let launch = match std::env::var("FRESCOD_RENDERER_LAUNCHER").ok().filter(|s| !s.trim().is_empty()) {
            Some(l) if l.trim() == "unconfined" => {
                eprintln!("frescod: WARNING: split mode, renderer {} NOT confined (FRESCOD_RENDERER_LAUNCHER=unconfined)", bin.display());
                Launch::Bin(bin)
            }
            Some(l) => {
                eprintln!("frescod: split mode, renderer launched by: {l}");
                Launch::Command(l)
            }
            None => {
                let app = std::env::var("FRESCOD_RENDERER_APP").ok().filter(|s| !s.is_empty())
                    .unwrap_or_else(|| RENDERER_APP.to_string());
                match portcullis_ipc::resolve_socket_path() {
                    Some(p) => eprintln!("frescod: split mode, renderers are {app} jails from portcullisd ({p})"),
                    None => eprintln!("frescod: WARNING: split mode, but portcullisd's socket is missing: \
                                       every client will be refused until it is there"),
                }
                Launch::Portcullisd { app }
            }
        };
        Some(Arc::new(Broker {
            comp,
            redraw,
            screen,
            launch,
            frames: Mutex::new(HashMap::new()),
            pixels,
            drawn: Arc::new(Mutex::new(HashMap::new())),
            spare: Mutex::new(HashMap::new()),
            drawings: std::sync::atomic::AtomicU64::new(1),
            renderers: Mutex::new(HashMap::new()),
            subs: Mutex::new(None),
            next_handle: std::sync::atomic::AtomicU32::new(1),
        }))
    }

    /// The screen now: the live mode once frescod has set one (it changes
    /// with a mode switch), else the one the broker was made with.
    fn screen(&self) -> (u32, u32, u32) {
        let s = crate::screen::get();
        if s.0 > 0 { s } else { self.screen }
    }

    /// The hand-off for `socket_server::Shared`.
    pub fn hand_off(self: &Arc<Self>) -> socket_server::HandOff {
        let me = self.clone();
        Arc::new(move |stream, client_id, uid, subs| {
            if let Err(e) = me.clone().start(stream, client_id, uid, subs, None) {
                eprintln!("frescod: client {client_id}: renderer: {e}");
                socket_server::free_client_id(client_id);
            }
        })
    }

    /// The subscriber list connections opened over the chrome channel join.
    pub fn set_subs(&self, subs: EventSubs) {
        *self.subs.lock().unwrap() = Some(subs);
    }

    /// The origin the Navigator set for window `global`, if any.
    pub fn origin(&self, global: u16) -> Option<String> {
        let rs: Vec<Arc<Renderer>> = self.renderers.lock().unwrap().values().cloned().collect();
        for r in rs {
            let local = r.wins.lock().unwrap().iter().find(|(_, w)| w.global == global).map(|(&l, _)| l);
            if let Some(l) = local {
                return r.origins.lock().unwrap().get(&l).cloned();
            }
        }
        None
    }

    /// Serve the chrome socket at `path` (F4d). Like the WM socket, reaching
    /// it is the grant: the TCB mounts it only for the Navigator.
    pub fn serve_chrome(self: &Arc<Self>, path: &std::path::Path) -> io::Result<()> {
        let l = fresco_split::chan::listen(path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
        }
        eprintln!("frescod: chrome channel on {}", path.display());
        let me = self.clone();
        std::thread::Builder::new().name("frescod-chrome-accept".into()).spawn(move || {
            let mut next = 0u64;
            loop {
                match fresco_split::chan::accept(&l) {
                    Ok((c, uid)) => {
                        next += 1;
                        let (me, conn) = (me.clone(), next);
                        let _ = std::thread::Builder::new().name(format!("frescod-chrome-{conn}")).spawn(move || {
                            let why = me.chrome(&c, conn, uid);
                            log::info!("frescod: chrome connection {conn}: {why}");
                        });
                    }
                    Err(e) => {
                        eprintln!("frescod: chrome accept: {e}");
                        return;
                    }
                }
            }
        })?;
        Ok(())
    }

    /// One chrome connection; returns why it ended.
    fn chrome(self: &Arc<Self>, c: &Chan, conn: u64, uid: u32) -> String {
        use fresco_split::chan::Raw;
        use fresco_split::chrome::ChromeMsg;
        loop {
            let m = match c.recv_raw() {
                Ok(Raw::Rec(rec, fds)) if fds.is_empty() => match ChromeMsg::decode(&rec) {
                    Ok(m) => m,
                    Err(e) => return format!("refused a record: {}", e.0),
                },
                Ok(Raw::Rec(..)) => return "a record with fds".into(),
                Ok(Raw::Refused(e)) => return format!("refused a record: {}", e.0),
                Ok(Raw::Eof) => return "closed".into(),
                Err(e) => return e.to_string(),
            };
            match m {
                ChromeMsg::OpenClient => {
                    let opened = self.renderers.lock().unwrap().values().filter(|r| r.opener == Some(conn)).count();
                    if opened >= MAX_OPENED {
                        return format!("more than {MAX_OPENED} connections open");
                    }
                    let Some(subs) = self.subs.lock().unwrap().clone() else { return "not ready".into() };
                    let Some(client_id) = socket_server::alloc_client_id() else { return "no client id free".into() };
                    let (ours, theirs) = match UnixStream::pair() {
                        Ok(p) => p,
                        Err(e) => return e.to_string(),
                    };
                    let handle = match self.clone().start(ours, client_id, Some(uid), subs, Some(conn)) {
                        Ok(h) => h,
                        Err(e) => {
                            socket_server::free_client_id(client_id);
                            return format!("renderer: {e}");
                        }
                    };
                    if let Err(e) = c.send_raw(&ChromeMsg::Opened { handle }.encode(), &[theirs.as_raw_fd()]) {
                        return e.to_string();
                    }
                }
                ChromeMsg::SetOrigin { handle, window, origin } => {
                    let r = self.renderers.lock().unwrap().values().find(|r| r.handle == handle).cloned();
                    match r {
                        Some(r) if r.opener == Some(conn) => {
                            let mut o = r.origins.lock().unwrap();
                            if origin.is_empty() {
                                o.remove(&window);
                            } else if o.len() < MAX_WINDOWS as usize || o.contains_key(&window) {
                                o.insert(window, origin);
                            }
                            self.redraw.wake();
                        }
                        // A connection that has gone, or one this peer did
                        // not open: nothing to set.
                        _ => log::debug!("frescod: chrome {conn}: origin for handle {handle} ignored"),
                    }
                }
                ChromeMsg::Opened { .. } => return "sent a broker's record".into(),
            }
        }
    }

    /// The frame to compose for window `id`, if a renderer draws it.
    pub fn frame(&self, id: u32) -> Option<Arc<fresco_compute::Frame>> {
        self.frames.lock().unwrap().get(&id).cloned()
    }

    /// The last pixels a renderer drew for window `id` (pixel mode).
    pub fn pixels(&self, id: u32) -> Option<Arc<crate::pack::Pixels>> {
        self.drawn.lock().unwrap().get(&id).cloned()
    }

    /// Hit-testing for window `id`'s pixels: the alpha of its latest
    /// drawing at a point. It looks them up when asked rather than holding
    /// them, so the next drawing can still patch them in place.
    pub fn alpha_probe(&self, id: u32) -> fresco_scene_server::window::AlphaProbe {
        let drawn = Arc::downgrade(&self.drawn);
        Arc::new(move |x, y| {
            drawn.upgrade()
                .and_then(|d| d.lock().unwrap().get(&id).map(|p| p.alpha_at(x, y)))
                .unwrap_or(0)
        })
    }

    /// Tell renderers the sizes the WM gave their windows since the last
    /// frame. Called once per frame, before the windows are snapshot.
    pub fn sync(&self) {
        let sizes: HashMap<u16, (u32, u32)> = {
            let c = self.comp.lock().unwrap();
            let screen = self.screen();
            c.windows.iter().map(|(&id, w)| (id, (self.dim(w.size.0, screen.0), self.dim(w.size.1, screen.1)))).collect()
        };
        let rs: Vec<Arc<Renderer>> = self.renderers.lock().unwrap().values().cloned().collect();
        for r in rs {
            let mut changed: Vec<(Msg, Vec<std::os::fd::RawFd>)> = Vec::new();
            for (&local, w) in r.wins.lock().unwrap().iter_mut() {
                if let Some(&s) = sizes.get(&w.global) {
                    if s != w.told {
                        w.told = s;
                        // Pixel slots hold the window's size: a bigger window
                        // gets new ones, before the size that needs them.
                        let bytes = self.slot_bytes(s);
                        if bytes > w.slots[0].len() {
                            match (Shm::create(bytes), Shm::create(bytes)) {
                                (Ok(a), Ok(b)) => {
                                    changed.push((Msg::Slots { window: local, bytes: bytes as u32 }, vec![a.fd(), b.fd()]));
                                    w.slots = [a, b];
                                }
                                _ => log::warn!("frescod: client {}: cannot create frame slots", r.client_id),
                            }
                        }
                        changed.push((Msg::Geometry { window: local, width: s.0, height: s.1 }, Vec::new()));
                    }
                }
            }
            for (m, fds) in changed {
                if r.tell(&m, &fds).is_err() {
                    break;
                }
            }
        }
    }

    /// A window's slot size: a packed frame's fixed room, or, in pixel mode,
    /// the window's pixels.
    fn slot_bytes(&self, size: (u32, u32)) -> usize {
        if self.pixels { slot::pixel_slot_bytes(size.0, size.1) } else { DEFAULT_SLOT_BYTES as usize }
    }

    fn dim(&self, v: f32, screen: u32) -> u32 {
        if v >= 1.0 { (v as u32).clamp(1, MAX_WINDOW_DIM) } else { screen }
    }

    fn start(self: Arc<Self>, stream: UnixStream, client_id: u8, uid: Option<u32>, subs: EventSubs, opener: Option<u64>) -> io::Result<u32> {
        let (ours, theirs) = Chan::pair()?;
        // About 2700 records each way: input bursts fit, a renderer that
        // has stopped reading fills it and is retired (`Renderer::tell`).
        ours.set_buffer(256 << 10)?;
        theirs.set_buffer(256 << 10)?;
        let handle = self.next_handle.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let proc_ = match &self.launch {
            Launch::Portcullisd { app } => {
                // Named by the handle, not the client id: an id is reused as
                // soon as it is freed, a handle never is, so a jail that is
                // slow to go cannot block the next client given its id.
                Proc::jail(app, &format!("fresco{handle}"), theirs.into_fd()).map_err(|e| {
                    log::error!("frescod: client {client_id}: REFUSED, portcullisd did not start a renderer: {e}");
                    e
                })?
            }
            l => {
                let mut cmd = match l {
                    Launch::Command(l) => {
                        let l = l.replace("{client}", &client_id.to_string());
                        let mut words = l.split_whitespace();
                        let mut c = Command::new(words.next().unwrap_or_default());
                        c.args(words);
                        c
                    }
                    Launch::Bin(b) => Command::new(b),
                    Launch::Portcullisd { .. } => unreachable!(),
                };
                // stdout carries nothing (a launcher's chatter must not reach
                // the channel); stderr is inherited so a failing renderer is heard.
                cmd.stdin(Stdio::from(theirs.into_fd())).stdout(Stdio::null());
                Proc::Child(cmd.spawn()?)
            }
        };
        let screen = self.screen();
        ours.send(&Msg::Client { width: screen.0, height: screen.1, refresh_mhz: screen.2, pixels: self.pixels }, &[stream.as_raw_fd()])?;
        // The broker keeps a copy of the client's socket, never read, only
        // to see the client hang up: a renderer that ignores its client's
        // hang-up must not keep its client id and windows forever.
        let hangup = stream;
        log::info!("frescod: client {client_id} (uid {uid:?}) -> renderer {}", proc_.describe());

        let r = Arc::new(Renderer {
            client_id,
            handle,
            opener,
            origins: Mutex::new(HashMap::new()),
            chan: ours,
            drag_formats: Mutex::new(Vec::new()),
            gone: std::sync::atomic::AtomicBool::new(false),
            wins: Mutex::new(HashMap::new()),
            videos: Mutex::new(HashMap::new()),
            proc_: Mutex::new(proc_),
        });
        self.renderers.lock().unwrap().insert(client_id, r.clone());

        // Its windows' events, as records.
        let (ev_tx, ev_rx) = mpsc::channel::<DisplayEvent>();
        {
            /* The appearance now, then every change after it, in order. */
            let mut s = subs.lock().unwrap();
            let now = *socket_server::APPEARANCE.lock().unwrap();
            let _ = ev_tx.send(DisplayEvent::AppearanceChanged(now));
            s.push(Sub { client_id, wm: false, tx: SubTx::Split(ev_tx) });
        }
        {
            let r = r.clone();
            std::thread::Builder::new().name(format!("frescod-split-ev-{client_id}")).spawn(move || {
                'events: for ev in ev_rx.iter() {
                    for m in event_records(&r, &ev) {
                        if r.tell(&m, &[]).is_err() {
                            break 'events;
                        }
                    }
                }
            })?;
        }

        // Its client's hang-up.
        {
            let r = r.clone();
            std::thread::Builder::new().name(format!("frescod-split-hup-{client_id}")).spawn(move || {
                let mut p = libc::pollfd { fd: hangup.as_raw_fd(), events: 0, revents: 0 };
                while !r.gone.load(std::sync::atomic::Ordering::Relaxed) {
                    // SAFETY: one pollfd, ours, for the duration of the call.
                    let n = unsafe { libc::poll(&mut p, 1, 1000) };
                    if n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                        log::info!("frescod: client {client_id}: hung up; retiring its renderer");
                        r.chan.shutdown();
                        break;
                    }
                }
                drop(hangup);
            })?;
        }

        // Its records.
        std::thread::Builder::new().name(format!("frescod-split-{client_id}")).spawn(move || {
            let why = self.serve(&r, uid.unwrap_or(0));
            log::info!("frescod: client {client_id}: renderer gone: {why}");
            self.reap(&r, &subs);
        })?;
        Ok(handle)
    }

    /// Serve one renderer's records until it goes or misbehaves; returns why.
    ///
    /// Each record costs the broker something (a frame copy and check, two
    /// slots and the compositor lock for a window), so a renderer gets a
    /// budget: [`RECORDS_PER_S`] records, of which [`CREATES_PER_S`] window
    /// opens. Past it this thread just waits, so a renderer that floods
    /// slows only itself: its channel fills and its own sends block.
    fn serve(&self, r: &Renderer, uid: u32) -> String {
        let mut records = Bucket::new(RECORDS_PER_S, RECORDS_PER_S);
        let mut creates = Bucket::new(CREATES_PER_S, CREATE_BURST);
        loop {
            records.take();
            let m = match r.chan.recv() {
                Ok(Recv::Msg(m, fds)) if m.from_renderer() && fds.is_empty() => m,
                Ok(Recv::Msg(m, _)) => return format!("refused {m:?} (not a renderer's record, or fds attached)"),
                Ok(Recv::Refused(e)) => return format!("refused a record: {}", e.0),
                Ok(Recv::Eof) => return "closed".into(),
                Err(e) => return e.to_string(),
            };
            if matches!(m, Msg::WindowCreate { .. }) {
                creates.take();
            }
            if let Err(e) = self.record(r, uid, m) {
                return e;
            }
        }
    }

    fn record(&self, r: &Renderer, uid: u32, m: Msg) -> Result<(), String> {
        let local = m.window();
        match m {
            Msg::WindowCreate { width, height, role, pos, title, embed_pending, .. } => {
                // An embed-only client's windows stay embeds (S1).
                let only = socket_server::is_embed_only(r.client_id);
                let (embed_pending, role) = if only { (true, Role::Document) } else { (embed_pending, role) };
                let mut wins = r.wins.lock().unwrap();
                if wins.contains_key(&local) || wins.len() >= MAX_WINDOWS as usize {
                    return Err(format!("window {local}: already open, or too many windows"));
                }
                let bytes = self.slot_bytes((width, height));
                let slots = [Shm::create(bytes), Shm::create(bytes)];
                let [Ok(a), Ok(b)] = slots else { return Err("cannot create frame slots".into()) };
                let global = {
                    let mut c = self.comp.lock().unwrap();
                    let g = c.create(r.client_id as u32, (width, height));
                    if let Some(w) = c.windows.get_mut(&g) {
                        w.title = title;
                        w.role = match wm_role(role) {
                            WmRole::Notification if !crate::notify::may(uid) => WmRole::Document,
                            WmRole::Chrome if !crate::notify::chrome_may(uid) => WmRole::Document,
                            r => r,
                        };
                        w.owner_uid = uid;
                        if let Some((x, y)) = pos {
                            w.pos = (x as f32, y as f32);
                        }
                        w.embed_pending = embed_pending;
                    }
                    if embed_pending || role == Role::Popup {
                        // limen.md §4: off screen until bound; a pop-up
                        // until its POPUP_PLACE passes (fresco-popups.md §3).
                        c.hide(g);
                    }
                    g
                };
                r.tell(&Msg::Slots { window: local, bytes: bytes as u32 }, &[a.fd(), b.fd()])
                    .and_then(|_| r.tell(&Msg::Geometry { window: local, width, height }, &[]))?;
                wins.insert(local, Win { global, slots: [a, b], told: (width, height) });
                drop(wins);
                // Its embed token, for the client alone (limen.md §5).
                let token = self.comp.lock().unwrap().windows.get(&global).map(|w| w.embed_token);
                if let Some(token) = token.filter(|t| *t != [0; 16]) {
                    r.tell(&Msg::EmbedToken { window: local, token }, &[])?;
                }
            }
            Msg::WindowDestroy { .. } => {
                let w = r.wins.lock().unwrap().remove(&local).ok_or_else(|| format!("destroy of unknown window {local}"))?;
                r.videos.lock().unwrap().retain(|&(win, _), _| win != local);
                self.drop_window(r, w.global);
            }
            Msg::WindowTitle { title, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global).ok_or("title of unknown window")?;
                if let Some(w) = self.comp.lock().unwrap().windows.get_mut(&global) {
                    w.title = title;
                }
            }
            Msg::FrameReady { slot: s, .. } => {
                if self.pixels {
                    let (global, (width, height)) = {
                        let wins = r.wins.lock().unwrap();
                        let w = wins.get(&local).ok_or_else(|| format!("frame for unknown window {local}"))?;
                        (w.global, w.told)
                    };
                    match self.take_pixels(r, local, global, width, height, s as usize) {
                        Ok(()) => self.redraw.wake(),
                        Err(e) => log::debug!("frescod: client {} window {local}: {e}", r.client_id),
                    }
                    r.tell(&Msg::FrameDone { window: local, slot: s }, &[])?;
                    return Ok(());
                }
                let got = {
                    let wins = r.wins.lock().unwrap();
                    let w = wins.get(&local).ok_or_else(|| format!("frame for unknown window {local}"))?;
                    (w.global, slot::read(w.slots[s as usize].as_slice(), w.told.0, w.told.1))
                };
                match got {
                    (global, Ok((_, frame))) => {
                        self.frames.lock().unwrap().insert(global as u32, Arc::new(frame));
                        self.redraw.wake();
                    }
                    // A frame drawn before the latest GEOMETRY arrived has the
                    // old size: dropped, the renderer draws again.
                    (_, Err(e)) => log::debug!("frescod: client {} window {local}: {e}", r.client_id),
                }
                r.tell(&Msg::FrameDone { window: local, slot: s }, &[])?;
            }
            Msg::DragFormat { format, .. } => {
                let mut f = r.drag_formats.lock().unwrap();
                if f.len() >= fresco_protocol::drag::MAX_FORMATS {
                    return Err("too many drag formats".into());
                }
                f.push(format);
            }
            Msg::PopupPlace { parent, anchor, side, align, width, height, grab, .. } => {
                let (global, parent_global) = {
                    let wins = r.wins.lock().unwrap();
                    (wins.get(&local).map(|w| w.global), wins.get(&parent).map(|w| w.global))
                };
                let global = global.ok_or("pop-up place for unknown window")?;
                let refused = {
                    let mut c = self.comp.lock().unwrap();
                    let res = match parent_global {
                        Some(p) if !socket_server::is_embed_only(r.client_id) => {
                            c.attach_popup(r.client_id as u32, global, p, anchor, side, align, (width, height), grab)
                        }
                        _ => Err(fresco_scene_server::window::PopupRefused::UnknownWindow),
                    };
                    // An open pop-up refused is dismissed, which tells
                    // the renderer; otherwise it is told here.
                    let open = c.windows.get(&global).is_some_and(|w| w.popup.is_some() && !w.hidden);
                    if res.is_err() {
                        c.hide(global);
                    }
                    res.err().map(|e| (e, open))
                };
                match refused {
                    None => self.redraw.wake(),
                    Some((e, true)) => log::info!("frescod: client {} window {local}: pop-up refused: {e:?}", r.client_id),
                    Some((e, false)) => {
                        log::info!("frescod: client {} window {local}: pop-up refused: {e:?}", r.client_id);
                        r.tell(&Msg::PopupDismissed { window: local, reason: fresco_protocol::popup_dismiss::PARENT }, &[])?;
                    }
                }
            }
            Msg::PadClaim { device, claim, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global as u32).ok_or("pad claim for unknown window")?;
                crate::tactus_client::claim(r.client_id, global, device, claim);
            }
            Msg::DragBegin { drag_id, activation, .. } => {
                let formats = std::mem::take(&mut *r.drag_formats.lock().unwrap());
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global as u32);
                let b = fresco_protocol::DragBegin { window_id: global.unwrap_or(0), drag_id, activation, formats };
                if let Err(e) = global.ok_or("unknown window").and_then(|_| crate::drag::begin(&self.comp, r.client_id, &b)) {
                    log::info!("frescod: client {}: drag {drag_id} refused: {e}", r.client_id);
                    r.tell(&Msg::DragEnd { window: local, drag_id, dropped: false }, &[])?;
                }
            }
            Msg::VideoRing { layer, width, height, .. } => {
                // Slots no larger than the window (a layer shows only
                // inside it), for at most a client's layers.
                let (global, told) = r.wins.lock().unwrap().get(&local).map(|w| (w.global, w.told)).ok_or("video ring for unknown window")?;
                let mut v = r.videos.lock().unwrap();
                v.remove(&(local, layer));
                if v.len() >= fresco_protocol::video::MAX_LAYERS_PER_CLIENT {
                    return Err("too many video layers".into());
                }
                if u64::from(width) * u64::from(height) > u64::from(told.0) * u64::from(told.1) {
                    // A window resized under it: the next frame asks again.
                    log::debug!("frescod: client {} window {local}: video ring {width}x{height} past the window", r.client_id);
                    crate::video::VIDEOS.destroy(global as u32, layer);
                    self.redraw.wake();
                    return Ok(());
                }
                let bytes = width as usize * height as usize * 4;
                let [Ok(a), Ok(b)] = [Shm::create(bytes), Shm::create(bytes)] else { return Err("cannot create video slots".into()) };
                r.tell(&Msg::VideoSlots { window: local, layer, bytes: bytes as u32 }, &[a.fd(), b.fd()])?;
                v.insert((local, layer), VRing { slots: [a, b], size: (width, height) });
            }
            Msg::VideoFrame { layer, slot: s, seq, present_ns, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global).ok_or("video frame for unknown window")?;
                let px = {
                    let v = r.videos.lock().unwrap();
                    v.get(&(local, layer)).map(|ring| {
                        // Any bytes are pixels; shown opaque.
                        let src = ring.slots[s as usize].as_slice();
                        let px: Vec<u32> = src.chunks_exact(4)
                            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) | 0xff00_0000)
                            .collect();
                        (ring.size, px)
                    })
                };
                if let Some(((w, h), px)) = px {
                    crate::video::VIDEOS.queue(global as u32, layer, crate::video::pixels(w, h, px, seq), seq, present_ns);
                    self.redraw.wake();
                }
                r.tell(&Msg::VideoDone { window: local, layer, slot: s }, &[])?;
            }
            Msg::VideoPlace { layer, rect, protected, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global).ok_or("video place for unknown window")?;
                // Only a layer with slots (so at most a client's layers).
                if !r.videos.lock().unwrap().contains_key(&(local, layer)) {
                    return Ok(());
                }
                crate::video::VIDEOS.place(global as u32, layer, rect, protected);
                self.redraw.wake();
            }
            Msg::VideoRate { layer, hz_milli, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global).ok_or("video rate for unknown window")?;
                let hz = if fresco_protocol::video::RATE_RANGE.contains(&hz_milli) { hz_milli } else { 0 };
                if r.videos.lock().unwrap().contains_key(&(local, layer)) {
                    crate::video::VIDEOS.rate(global as u32, layer, hz);
                }
            }
            Msg::VideoDestroy { layer, .. } => {
                let global = r.wins.lock().unwrap().get(&local).map(|w| w.global).ok_or("video destroy for unknown window")?;
                r.videos.lock().unwrap().remove(&(local, layer));
                crate::video::VIDEOS.destroy(global as u32, layer);
                self.redraw.wake();
            }
            _ => unreachable!("serve() admits only a renderer's records"),
        }
        Ok(())
    }

    /// Take a window's drawing from slot `s`: only its damage when the
    /// drawing it follows is the one held (patched in place when nothing
    /// else holds that one), else all of it.
    fn take_pixels(&self, r: &Renderer, local: u32, global: u16, width: u32, height: u32, s: usize) -> std::result::Result<(), String> {
        use crate::pack::Pixels;
        use std::sync::atomic::Ordering::Relaxed;
        let wins = r.wins.lock().unwrap();
        let w = wins.get(&local).ok_or_else(|| format!("frame for unknown window {local}"))?;
        let slot = w.slots[s].as_slice();
        let (seq, damage) = slot::pixel_header(slot, width, height)?;
        let gen = self.drawings.fetch_add(1, Relaxed);
        let mut drawn = self.drawn.lock().unwrap();
        let follows = |p: &Pixels| p.width == width && p.height == height && p.seq == seq.wrapping_sub(1);
        if let (Some(d), Some(prev)) = (damage, drawn.get_mut(&(global as u32))) {
            if follows(prev) {
                let spare = self.spare.lock().unwrap().remove(&(global as u32));
                let spare = crate::pack::advance(prev, spare, gen, seq, d, |px| slot::copy_pixels(slot, width, height, px, Some(d)))?;
                if let Some(sp) = spare {
                    self.spare.lock().unwrap().insert(global as u32, sp);
                }
                return Ok(());
            }
        }
        drop(drawn);
        let mut px = vec![0u32; width as usize * height as usize];
        slot::copy_pixels(slot, width, height, &mut px, None)?;
        log::debug!("frescod: client {} window {local}: {width}x{height} pixels, whole", r.client_id);
        self.drawn.lock().unwrap().insert(global as u32, Arc::new(Pixels { gen, width, height, px, seq, since: Vec::new() }));
        Ok(())
    }

    fn drop_window(&self, r: &Renderer, global: u16) {
        self.spare.lock().unwrap().remove(&(global as u32));
        self.comp.lock().unwrap().destroy_with_focus_shift(global, r.client_id as u32);
        self.frames.lock().unwrap().remove(&(global as u32));
        self.drawn.lock().unwrap().remove(&(global as u32));
        crate::video::VIDEOS.forget_window(global as u32);
        self.redraw.wake();
    }

    /// The renderer is gone or misbehaved: end it and everything it had.
    fn reap(&self, r: &Arc<Renderer>, subs: &EventSubs) {
        r.gone.store(true, std::sync::atomic::Ordering::Relaxed);
        {
            // End the channel first: the renderer exits on end-of-file and
            // its jail goes with it. The kill is only the backstop for one
            // that does not go: a child is killed; a portcullisd jail is
            // ended by hanging up on portcullisd, which then removes it.
            r.chan.shutdown();
            let mut p = r.proc_.lock().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !p.exited() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !p.exited() {
                log::warn!("frescod: client {}: renderer did not exit; killing it", r.client_id);
                p.kill();
            }
            p.wait();
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

/// The renderer app frescod asks portcullisd for by default.
pub const RENDERER_APP: &str = "org.atrium.fresco.renderer";

/// How a renderer is started (see [`Broker::from_env`]).
enum Launch {
    /// A one-shot jail from portcullisd: the production path.
    Portcullisd { app: String },
    /// A command line, `{client}` replaced by the client id.
    Command(String),
    /// The renderer binary, run directly: not confined.
    Bin(PathBuf),
}

/// A running renderer.
enum Proc {
    Child(Child),
    /// A jail portcullisd runs for us. `sock` is the ExecInstance
    /// connection: portcullisd answers on it when the jail ends, and ends the
    /// jail if we hang up on it.
    Jail { name: String, sock: UnixStream, exited: Arc<AtomicBool> },
}

impl Proc {
    /// Ask portcullisd to run `app` as instance `instance`, on our channel
    /// end as its stdin. It returns once the jail is being started; whether
    /// it then ran is read off the connection by a thread of its own.
    fn jail(app: &str, instance: &str, stdin: OwnedFd) -> io::Result<Proc> {
        use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
        let path = portcullis_ipc::resolve_socket_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "portcullisd's socket is not there"))?;
        let mut s = UnixStream::connect(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
        // A wedged portcullisd must not wedge the accept path for long.
        s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
            Response::Hello { .. } => {}
            other => return Err(io::Error::other(format!("hello: {other:?}"))),
        }
        write_request(&mut s, &Request::ExecInstance {
            app_id: app.to_string(),
            instance: Some(instance.to_string()),
            // The CLI's default scratch space.
            tmpfs_mb: 64,
            // The bundle's [resources] set the caps; requiring one means a
            // machine without racct refuses rather than runs it uncapped.
            memory_mb: None,
            cpu_pct: None,
            require_memory_limit: true,
        })?;
        match read_response(&mut s)? {
            Response::ReadyForFds => {}
            Response::LaunchFailed { stage, message } => return Err(io::Error::other(format!("{stage}: {message}"))),
            Response::Error { message } => return Err(io::Error::other(message)),
            other => return Err(io::Error::other(format!("unexpected reply: {other:?}"))),
        }
        // stdin is the channel; stdout goes nowhere (nothing a renderer
        // prints may reach the channel); stderr is ours, so it is heard.
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
        send_fds(&s, &[stdin.as_raw_fd(), null.as_raw_fd(), 2])?;
        drop(stdin);
        s.set_read_timeout(None)?;
        let exited = Arc::new(AtomicBool::new(false));
        let name = format!("{app}/{instance}");
        {
            let (mut r, exited, name) = (s.try_clone()?, exited.clone(), name.clone());
            std::thread::Builder::new().name(format!("frescod-jail-{instance}")).spawn(move || {
                match read_response(&mut r) {
                    Ok(Response::LaunchExit { code }) => log::info!("frescod: renderer {name} exited ({code:?})"),
                    Ok(Response::LaunchFailed { stage, message }) => log::error!("frescod: renderer {name} failed: {stage}: {message}"),
                    Ok(other) => log::error!("frescod: renderer {name}: unexpected reply {other:?}"),
                    Err(e) => log::info!("frescod: renderer {name}: portcullisd connection: {e}"),
                }
                exited.store(true, Ordering::Release);
            })?;
        }
        Ok(Proc::Jail { name, sock: s, exited })
    }

    fn describe(&self) -> String {
        match self {
            Proc::Child(c) => format!("pid {}", c.id()),
            Proc::Jail { name, .. } => format!("jail {name}"),
        }
    }

    fn exited(&mut self) -> bool {
        match self {
            Proc::Child(c) => !matches!(c.try_wait(), Ok(None)),
            Proc::Jail { exited, .. } => exited.load(Ordering::Acquire),
        }
    }

    fn kill(&mut self) {
        match self {
            Proc::Child(c) => { let _ = c.kill(); }
            Proc::Jail { sock, .. } => { let _ = sock.shutdown(std::net::Shutdown::Both); }
        }
    }

    fn wait(&mut self) {
        match self {
            Proc::Child(c) => { let _ = c.wait(); }
            Proc::Jail { name, exited, .. } => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !exited.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if !exited.load(Ordering::Acquire) {
                    log::error!("frescod: renderer {name}: portcullisd has not reported it gone");
                }
            }
        }
    }
}

/// Records per second one renderer may send: two frames a window at 60 Hz
/// for a handful of busy windows, with room for titles and the rest.
const RECORDS_PER_S: f64 = 480.0;
/// Window opens per second one renderer may make, after a first burst of
/// [`CREATE_BURST`]: a menu or a dialog at the speed a person opens them.
/// Each open makes the WM lay the session out again, so this is what bounds
/// the work a hundred churning renderers can make for the WM and the
/// compositor lock.
const CREATES_PER_S: f64 = 2.0;
const CREATE_BURST: f64 = 16.0;

/// A token bucket that waits when empty.
pub(crate) struct Bucket {
    rate: f64,
    cap: f64,
    tokens: f64,
    at: std::time::Instant,
}

impl Bucket {
    pub(crate) fn new(rate: f64, cap: f64) -> Bucket {
        Bucket { rate, cap, tokens: cap, at: std::time::Instant::now() }
    }

    pub(crate) fn take(&mut self) {
        let now = std::time::Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate).min(self.cap);
        self.at = now;
        if self.tokens < 1.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64((1.0 - self.tokens) / self.rate));
            self.at = std::time::Instant::now();
            self.tokens = 1.0;
        }
        self.tokens -= 1.0;
    }
}

/// The record an event for one of `r`'s windows becomes, in its local id.
/// Window lifetime events are the renderer's own business, and sizes go
/// by [`Broker::sync`]; events over window 0 (the shared background) have
/// no owner yet and do not reach renderers (F4c gives it one).
fn event_records(r: &Renderer, ev: &DisplayEvent) -> Vec<Msg> {
    use fresco_scene_server::window::DragPhase as P;
    use DisplayEvent as E;
    let finite = |x: f32, y: f32| x.is_finite() && y.is_finite();
    match ev {
        E::Drag { phase, window_id, drag_id, x, y, formats } if finite(*x, *y) => {
            let Some(window) = r.local_of(*window_id) else { return Vec::new() };
            let phase = match phase {
                P::Enter => fresco_split::DragPhase::Enter,
                P::Over => fresco_split::DragPhase::Over,
                P::Leave => fresco_split::DragPhase::Leave,
            };
            let mut v: Vec<Msg> = formats.iter().map(|f| Msg::DragOffer { window, format: f.clone() }).collect();
            v.push(Msg::Drag { window, phase, drag_id: *drag_id, x: *x, y: *y });
            v
        }
        E::Drop { window_id, x, y, activation } if finite(*x, *y) => {
            r.local_of(*window_id).map(|window| Msg::Drop { window, x: *x, y: *y, activation: *activation }).into_iter().collect()
        }
        E::DragEnd { window_id, drag_id, dropped } => {
            r.local_of(*window_id).map(|window| Msg::DragEnd { window, drag_id: *drag_id, dropped: *dropped }).into_iter().collect()
        }
        E::AppearanceChanged(a) => vec![Msg::Appearance { scheme: a.scheme, high_contrast: a.high_contrast, accent: a.accent }],
        E::WindowSpan { window_id, tiles, hidden } => {
            let Some(window) = r.local_of(*window_id) else { return Vec::new() };
            let mut v: Vec<Msg> = tiles.iter().map(|t| Msg::SpanTile { window, rect: t.rect, present: t.present }).collect();
            v.extend(hidden.iter().map(|&rect| Msg::SpanHidden { window, rect }));
            v.push(Msg::SpanDone { window });
            v
        }
        _ => event_record(r, ev).into_iter().collect(),
    }
}

fn event_record(r: &Renderer, ev: &DisplayEvent) -> Option<Msg> {
    use DisplayEvent as E;
    let (global, input) = match *ev {
        E::WindowFocusChanged { window_id, gained, cause } => return Some(Msg::Focus { window: r.local_of(window_id)?, gained, cause }),
        E::WindowCloseRequested { window_id } => return Some(Msg::CloseRequested { window: r.local_of(window_id)? }),
        E::PopupPlaced { window_id, x, y, width, height, side } => {
            return Some(Msg::PopupPlaced { window: r.local_of(window_id)?, x, y, width, height, side });
        }
        E::PopupDismissed { window_id, reason } => return Some(Msg::PopupDismissed { window: r.local_of(window_id)?, reason }),
        E::VideoPresented { window_id, layer, seq, vblank_ns, dropped } => {
            return Some(Msg::VideoPresented { window: r.local_of(window_id)?, layer, seq, vblank_ns, dropped });
        }
        E::WindowDpiChanged { window_id, scale_factor } => {
            let scale_milli = (scale_factor * 1000.0).round() as u32; /* NaN -> 0 */
            if !(fresco_split::SCALE_MIN..=fresco_split::SCALE_MAX).contains(&scale_milli) {
                return None;
            }
            return Some(Msg::Scale { window: r.local_of(window_id)?, scale_milli });
        }
        E::InputKey { window_id, hid_usage, pressed, modifiers, activation, repeat } => {
            (window_id, Input::Key { usage: hid_usage, pressed, modifiers, activation, repeat })
        }
        E::InputPointerMotion { window_id, x, y } => (window_id, Input::Motion { x, y }),
        E::InputPointerButton { window_id, x, y, button, pressed, modifiers, activation } => {
            (window_id, Input::Button { x, y, button, pressed, modifiers, activation })
        }
        E::InputPointerScroll { window_id, dx, dy, phase } => (window_id, Input::Scroll { dx, dy, phase }),
        E::InputPointerMagnify { window_id, x, y, scale, phase } => (window_id, Input::Magnify { x, y, scale, phase }),
        E::GamepadConnect { window_id, device_id, vendor, product, buttons, axes, ref name } => {
            return Some(Msg::PadConnect { window: r.local_of(window_id)?, device: device_id, vendor, product, buttons, axes, name: name.clone() });
        }
        E::Gamepad { window_id, kind, device_id, usage, value } => {
            use fresco_protocol::control as c;
            let kind = match kind {
                c::EV_GAMEPAD_DISCONNECT => fresco_split::PAD_DISCONNECT,
                c::EV_GAMEPAD_BUTTON => fresco_split::PAD_BUTTON,
                c::EV_GAMEPAD_AXIS => fresco_split::PAD_AXIS,
                c::EV_GAMEPAD_CLAIM => fresco_split::PAD_CLAIMED,
                _ => return None,
            };
            return Some(Msg::Pad { window: r.local_of(window_id)?, kind, device: device_id, usage, value });
        }
        _ => return None,
    };
    let ok = |v: f32| v.is_finite();
    match input {
        Input::Motion { x, y } | Input::Button { x, y, .. } if !(ok(x) && ok(y)) => return None,
        Input::Scroll { dx, dy, .. } if !(ok(dx) && ok(dy)) => return None,
        Input::Magnify { x, y, scale, .. } if !(ok(x) && ok(y) && ok(scale) && scale > 0.0) => return None,
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
        Role::Notification => WmRole::Notification,
        Role::Popup => WmRole::Document,
    }
}

