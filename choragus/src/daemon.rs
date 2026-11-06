//! choragusd: the front door apps use for sound (lyra-v1.md §3–5).
//!
//! For each app connection: identity from the peer uid ([`crate::identity`]),
//! `Hello` → `Welcome`, then the app's streams. choragusd owns policy (roles,
//! ducking, the seat, the app's own volume) and composes each stream's level
//! into one `SetGain` for lyrad. It brokers each stream's ring fd from lyrad
//! to the app, and forwards lyrad's events (flushed, drained, underrun) to the
//! app that owns the stream. An app can name only its own streams.

use crate::app::{role_from_wire, CAP_AUDIO};
use crate::grant::GrantStore;
use crate::policy::{Device, DeviceKind, Session, Stream};
use lyra_protocol::fdpass::{recv_fd, send_fd};
use lyra_protocol::wire::{self, DenyCode, FromEngine, ToApp, ToChoragus, ToEngine};
use portcullis_peer::AppRegistry;
use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Level for a stream held silent (seat switch), in dB.
pub const HELD_DB: f32 = -120.0;

pub struct Config {
    /// `(socket path, capability bits it allows)`.
    pub listeners: Vec<(String, u8)>,
    pub lyrad: String,
    pub registry: Option<String>,
    /// Users' Portcullis policies (`<base>/<user>/policy.toml`); else `grants`.
    pub policy_base: Option<String>,
    pub grants: GrantStore,
    /// Gate streams on the seat's active session (`seat_path`).
    pub seat: bool,
    pub seat_path: String,
    /// Identity only from the registry (the service).
    pub strict: bool,
    /// What lyrad plays at (sent in `Welcome`).
    pub device_rate: u32,
    pub device_channels: u8,
    pub period_frames: u32,
}

struct StreamRec {
    conn: u64,
    user: String,
    tag: u32,
    ring_bytes: usize,
    policy_db: f32,
    app_gain: f32,
    held: bool,
    sent_db: Option<f32>,
    open: bool,
}

struct AppConn {
    writer: UnixStream,
    streams: Vec<u32>,
}

struct State {
    sess: Session,
    lyrad: UnixStream,
    next_stream: u32,
    next_conn: u64,
    streams: HashMap<u32, StreamRec>,
    apps: HashMap<u64, AppConn>,
}

impl State {
    fn total_db(r: &StreamRec) -> f32 {
        if r.held || r.app_gain <= 0.0 {
            return HELD_DB;
        }
        (r.policy_db + 20.0 * r.app_gain.log10()).max(HELD_DB)
    }

    /// Re-run policy and send lyrad the level of every stream whose level changed.
    fn recompute(&mut self) {
        for d in self.sess.resolve() {
            if let Some(r) = self.streams.get_mut(&d.stream) {
                r.policy_db = d.gain_db;
            }
        }
        let mut sends = Vec::new();
        for (id, r) in self.streams.iter_mut() {
            let db = Self::total_db(r);
            if r.sent_db.map_or(true, |s| (s - db).abs() > 1e-4) {
                r.sent_db = Some(db);
                sends.push(ToEngine::SetGain { stream: *id, db });
            }
        }
        for m in sends {
            let _ = wire::send(&mut self.lyrad, &m);
        }
    }

    fn to_app(&mut self, conn: u64, msg: &ToApp) {
        if let Some(a) = self.apps.get_mut(&conn) {
            if wire::send(&mut a.writer, msg).is_err() {
                // A stuck or gone app: hang up; its reader thread cleans up.
                let _ = a.writer.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    fn close_stream(&mut self, id: u32) {
        if let Some(r) = self.streams.remove(&id) {
            if let Some(a) = self.apps.get_mut(&r.conn) {
                a.streams.retain(|s| *s != id);
            }
            self.sess.close(id);
            let _ = wire::send(&mut self.lyrad, &ToEngine::Close { stream: id });
            self.recompute();
        }
    }

    fn owns(&self, conn: u64, stream: u32) -> bool {
        self.streams.get(&stream).is_some_and(|r| r.conn == conn && r.open)
    }
}

type Shared = Arc<Mutex<State>>;

/// Handle one app connection to its end.
fn serve_app(mut app: UnixStream, state: Shared, cfg: Arc<Config>, socket_caps: u8) {
    let uid = match lyra_protocol::peer::peer_uid(app.as_raw_fd()) {
        Ok(u) => u,
        Err(_) => return,
    };
    let _ = app.set_read_timeout(Some(Duration::from_secs(5)));
    let hello: ToChoragus = match wire::recv(&mut app) {
        Ok(m) => m,
        Err(_) => return,
    };
    if !matches!(hello, ToChoragus::Hello { .. }) {
        return;
    }
    let _ = app.set_read_timeout(None);
    let registry = cfg.registry.as_deref().map(|p| AppRegistry::load(p).unwrap_or_default());
    let grant = |user: &str, app_id: &str| match &cfg.policy_base {
        Some(base) => GrantStore::load_portcullis(base, user).granted(app_id),
        None => cfg.grants.granted(app_id),
    };
    // In development (no registry, not strict) the "hello" app id is the uid's user.
    let claimed = portcullis_peer::username(uid).unwrap_or_default();
    let verified = crate::identity::resolve(uid, &claimed, registry.as_ref(), cfg.strict, socket_caps, grant);
    let Some(v) = verified.filter(|v| v.caps & CAP_AUDIO != 0) else {
        eprintln!("choragusd: uid {uid} refused (not in the registry, or no audio grant)");
        let _ = wire::send(&mut app, &ToApp::Denied { tag: 0, code: DenyCode::NotGranted });
        return;
    };
    let welcome = ToApp::Welcome { device_rate: cfg.device_rate, device_channels: cfg.device_channels, period_frames: cfg.period_frames };
    if wire::send(&mut app, &welcome).is_err() {
        return;
    }
    let Ok(writer) = app.try_clone() else { return };
    let _ = writer.set_write_timeout(Some(Duration::from_secs(1)));
    let conn = {
        let mut st = state.lock().unwrap();
        st.next_conn += 1;
        let c = st.next_conn;
        st.apps.insert(c, AppConn { writer, streams: Vec::new() });
        c
    };
    eprintln!("choragusd: '{}' (user {}, uid {uid}) connected", v.app_id, v.user);

    while let Ok(msg) = wire::recv::<_, ToChoragus>(&mut app) {
        let mut st = state.lock().unwrap();
        match msg {
            ToChoragus::Hello { .. } => {}
            ToChoragus::Open { tag, params } => {
                let deny = |st: &mut State, code| st.to_app(conn, &ToApp::Denied { tag, code });
                if let Err(code) = params.check() {
                    deny(&mut st, code);
                    continue;
                }
                let (count, used) = st.streams.values().filter(|r| r.conn == conn)
                    .fold((0usize, 0usize), |(n, b), r| (n + 1, b + r.ring_bytes));
                if count >= wire::MAX_STREAMS_PER_APP {
                    deny(&mut st, DenyCode::TooManyStreams);
                    continue;
                }
                if used + params.ring_bytes() > wire::MAX_RING_BYTES_PER_APP {
                    deny(&mut st, DenyCode::TooMuchMemory);
                    continue;
                }
                let id = st.next_stream;
                st.next_stream = st.next_stream.wrapping_add(1);
                if st.sess.open(Stream::new(id, role_from_wire(params.role))).is_err() {
                    deny(&mut st, DenyCode::EngineUnavailable);
                    continue;
                }
                let held = cfg.seat && portcullis_peer::seat::active_at(&cfg.seat_path).as_deref() != Some(v.user.as_str());
                st.streams.insert(id, StreamRec {
                    conn, user: v.user.clone(), tag, ring_bytes: params.ring_bytes(),
                    policy_db: 0.0, app_gain: 1.0, held, sent_db: None, open: false,
                });
                st.apps.get_mut(&conn).unwrap().streams.push(id);
                let open = ToEngine::Open { stream: id, rate: params.rate, channels: params.channels, ring_frames: params.ring_frames() };
                if wire::send(&mut st.lyrad, &open).is_err() {
                    st.streams.remove(&id);
                    st.sess.close(id);
                    deny(&mut st, DenyCode::EngineUnavailable);
                    continue;
                }
                st.recompute();
                eprintln!("choragusd: '{}' opened stream {id} ({:?}, {} Hz, {} ch, \"{}\")",
                    v.app_id, params.role, params.rate, params.channels, params.label);
            }
            ToChoragus::Start { stream } | ToChoragus::Pause { stream } | ToChoragus::Flush { stream } | ToChoragus::Drain { stream }
                if !st.owns(conn, stream) =>
            {
                st.to_app(conn, &ToApp::Denied { tag: 0, code: DenyCode::NoSuchStream });
            }
            ToChoragus::Start { stream } => { let _ = wire::send(&mut st.lyrad, &ToEngine::Start { stream }); }
            ToChoragus::Pause { stream } => { let _ = wire::send(&mut st.lyrad, &ToEngine::Pause { stream }); }
            ToChoragus::Flush { stream } => { let _ = wire::send(&mut st.lyrad, &ToEngine::Flush { stream }); }
            ToChoragus::Drain { stream } => { let _ = wire::send(&mut st.lyrad, &ToEngine::Drain { stream }); }
            ToChoragus::SetVolume { stream, gain } => {
                if !st.owns(conn, stream) {
                    st.to_app(conn, &ToApp::Denied { tag: 0, code: DenyCode::NoSuchStream });
                    continue;
                }
                let g = if gain.is_finite() { gain.clamp(0.0, 1.0) } else { 0.0 };
                st.streams.get_mut(&stream).unwrap().app_gain = g;
                st.recompute();
            }
            ToChoragus::Close { stream } => {
                if st.streams.get(&stream).is_some_and(|r| r.conn == conn) {
                    st.close_stream(stream);
                }
            }
        }
    }
    let mut st = state.lock().unwrap();
    if let Some(a) = st.apps.remove(&conn) {
        for id in a.streams {
            st.close_stream(id);
        }
    }
    eprintln!("choragusd: '{}' (uid {uid}) disconnected", v.app_id);
}

/// Read lyrad's messages: hand ring fds to apps, forward events. Returns when
/// lyrad hangs up.
fn serve_lyrad(mut rd: UnixStream, state: Shared) -> io::Error {
    loop {
        let msg: FromEngine = match wire::recv(&mut rd) {
            Ok(m) => m,
            Err(e) => return e,
        };
        match msg {
            FromEngine::Opened { stream, ring_frames, latency_ns } => {
                let (ring, clock) = match (recv_fd(rd.as_raw_fd()), recv_fd(rd.as_raw_fd())) {
                    (Ok(r), Ok(c)) => (r, c),
                    (Err(e), _) | (_, Err(e)) => return e,
                };
                let mut st = state.lock().unwrap();
                let Some(r) = st.streams.get_mut(&stream) else { continue };
                r.open = true;
                let (conn, tag) = (r.conn, r.tag);
                st.to_app(conn, &ToApp::Opened { tag, stream, ring_frames, latency_ns });
                if let Some(a) = st.apps.get(&conn) {
                    let _ = send_fd(a.writer.as_raw_fd(), ring.as_raw_fd());
                    let _ = send_fd(a.writer.as_raw_fd(), clock.as_raw_fd());
                }
                // Tell the app at once if its stream starts out held.
                if st.streams.get(&stream).is_some_and(|r| r.held) {
                    st.to_app(conn, &ToApp::Held { stream, held: true });
                }
            }
            FromEngine::OpenFailed { stream } => {
                let mut st = state.lock().unwrap();
                if let Some(r) = st.streams.remove(&stream) {
                    st.sess.close(stream);
                    if let Some(a) = st.apps.get_mut(&r.conn) {
                        a.streams.retain(|s| *s != stream);
                    }
                    st.to_app(r.conn, &ToApp::Denied { tag: r.tag, code: DenyCode::EngineUnavailable });
                    st.recompute();
                }
            }
            FromEngine::Flushed { stream, frame } => forward(&state, stream, ToApp::Flushed { stream, frame }),
            FromEngine::Drained { stream } => forward(&state, stream, ToApp::Drained { stream }),
            FromEngine::Underrun { stream, frames } => forward(&state, stream, ToApp::Underrun { stream, frames }),
        }
    }
}

fn forward(state: &Shared, stream: u32, msg: ToApp) {
    let mut st = state.lock().unwrap();
    if let Some(conn) = st.streams.get(&stream).map(|r| r.conn) {
        st.to_app(conn, &msg);
    }
}

/// Re-check the seat; hold or release streams whose owner's session changed.
fn apply_seat(state: &Shared, seat_path: &str) {
    let active = portcullis_peer::seat::active_at(seat_path);
    let mut st = state.lock().unwrap();
    let mut changed = Vec::new();
    for (id, r) in st.streams.iter_mut() {
        let held = active.as_deref() != Some(r.user.as_str());
        if held != r.held {
            r.held = held;
            if r.open {
                changed.push((r.conn, *id, held));
            }
        }
    }
    if changed.is_empty() {
        return;
    }
    st.recompute();
    for (conn, stream, held) in changed {
        st.to_app(conn, &ToApp::Held { stream, held });
    }
}

/// Run the daemon. Returns only on an error: binding a socket, reaching lyrad,
/// or lyrad hanging up (the caller exits so the supervisor restarts both).
pub fn run(cfg: Config) -> io::Error {
    let lyrad = match UnixStream::connect(&cfg.lyrad) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let rd = match lyrad.try_clone() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let spk = Device { id: 0, kind: DeviceKind::Speakers };
    let state: Shared = Arc::new(Mutex::new(State {
        sess: Session::new(vec![spk], spk.id),
        lyrad,
        next_stream: 1,
        next_conn: 0,
        streams: HashMap::new(),
        apps: HashMap::new(),
    }));
    let cfg = Arc::new(cfg);
    if cfg.seat {
        let (state, path) = (state.clone(), cfg.seat_path.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            apply_seat(&state, &path);
        });
    }
    for (path, caps) in &cfg.listeners {
        let _ = std::fs::remove_file(path);
        let listener = match UnixListener::bind(path) {
            Ok(l) => l,
            Err(e) => return io::Error::new(e.kind(), format!("bind {path}: {e}")),
        };
        {
            // Reaching the socket is not what authorizes; the directory (mounted
            // only for jails holding the capability), the registry identity and
            // the grant are.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
        }
        eprintln!("choragusd: apps at {path} (allows {:?})", crate::app::cap_names(*caps));
        let (state, cfg, caps) = (state.clone(), cfg.clone(), *caps);
        std::thread::spawn(move || {
            for app in listener.incoming() {
                let Ok(app) = app else { continue };
                let (state, cfg) = (state.clone(), cfg.clone());
                std::thread::spawn(move || serve_app(app, state, cfg, caps));
            }
        });
    }
    let e = serve_lyrad(rd, state.clone());
    // Tell every app its streams are gone before we go.
    let mut st = state.lock().unwrap();
    let all: Vec<(u64, u32)> = st.streams.iter().map(|(id, r)| (r.conn, *id)).collect();
    for (conn, stream) in all {
        st.to_app(conn, &ToApp::StreamLost { stream });
    }
    e
}
