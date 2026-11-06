//! choragusd: the front door apps use for sound (lyra-v1.md §3–5).
//!
//! For each app connection: identity from the peer uid ([`crate::identity`]),
//! `Hello` → `Welcome`, then the app's streams. choragusd owns policy (roles,
//! ducking, the seat, the app's own volume) and composes each stream's level
//! into one `SetGain` for lyrad. It brokers each stream's ring fd from lyrad
//! to the app, and forwards lyrad's events (flushed, drained, underrun) to the
//! app that owns the stream. An app can name only its own streams.
//!
//! The volume UI connects to `audio-control/` ([`serve_control`]): it lists its
//! user's apps that are playing and sets the user's level per app, the master
//! and the output (lyra-v1.md §7). Those levels persist ([`crate::volumes`]).
//! A stream plays at `policy × app's own volume × user's app level × master`.
//!
//! limend connects to `audio-attribution/` ([`serve_attribution`]) for
//! tickets: an embedded player that says one in `Hello` plays as its parent
//! app, and only if the parent holds `audio` (lyra-v1.md §4, decision 4).

use crate::app::{role_from_wire, CAP_ATTRIBUTION, CAP_AUDIO, CAP_CONTROL};
use crate::grant::GrantStore;
use crate::policy::{Device, DeviceKind, Session, Stream};
use crate::volumes::{clamp, Level, OutputSettings, VolumeStore};
use lyra_protocol::fdpass::{recv_fd, send_fd};
use lyra_protocol::wire::{
    self, AppEntry, DenyCode, FromAttribution, FromControl, FromEngine, Output, Role as WireRole, Ticket, ToApp,
    ToAttribution, ToChoragus, ToControl, ToEngine,
};
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
    /// Where users' volumes persist (`<dir>/<user>.toml`); `None`: memory only.
    pub state_dir: Option<String>,
    /// limend's uid: the only peer served on the attribution socket.
    pub limen_uid: Option<u32>,
}

/// Tickets one limend connection may hold at once.
pub const MAX_TICKETS: usize = 256;

/// A ticket limend asked for: who may present it, and whom it plays as.
struct TicketRec {
    child_uid: u32,
    user: String,
    app_id: String,
    /// The limend connection that holds it.
    limen: u64,
    /// Presented already (it works once), and by which app connection.
    used: bool,
    conn: Option<u64>,
}

struct StreamRec {
    conn: u64,
    user: String,
    app_id: String,
    label: String,
    role: WireRole,
    tag: u32,
    ring_bytes: usize,
    policy_db: f32,
    app_gain: f32,
    /// The user's level × master for this app, linear (from [`VolumeStore`]).
    user_gain: f32,
    held: bool,
    sent_db: Option<f32>,
    /// Whether lyrad was told the stream goes through virtual surround.
    sent_spatial: Option<bool>,
    open: bool,
}

struct AppConn {
    writer: UnixStream,
    streams: Vec<u32>,
}

/// A volume UI connection.
struct ControlConn {
    writer: UnixStream,
    user: String,
}

struct State {
    sess: Session,
    volumes: VolumeStore,
    /// The outputs' balance (system-wide), and where it is kept.
    outputs: OutputSettings,
    state_dir: Option<String>,
    /// The device's channel count, as lyrad last reported it.
    device_channels: u8,
    /// The seat file, when streams are gated on the active session.
    seat: Option<String>,
    controls: HashMap<u64, ControlConn>,
    tickets: HashMap<Ticket, TicketRec>,
    lyrad: UnixStream,
    next_stream: u32,
    next_conn: u64,
    streams: HashMap<u32, StreamRec>,
    apps: HashMap<u64, AppConn>,
}

impl State {
    fn total_db(r: &StreamRec) -> f32 {
        let g = r.app_gain * r.user_gain;
        if r.held || g <= 0.0 {
            return HELD_DB;
        }
        (r.policy_db + 20.0 * g.log10()).max(HELD_DB)
    }

    /// Re-run policy and send lyrad the level of every stream whose level changed.
    fn recompute(&mut self) {
        let keys: Vec<(u32, String, String)> =
            self.streams.iter().map(|(id, r)| (*id, r.user.clone(), r.app_id.clone())).collect();
        for (id, user, app) in keys {
            let v = self.volumes.user(&user);
            let g = v.app(&app).gain() * v.master_gain();
            self.streams.get_mut(&id).unwrap().user_gain = g;
        }
        for d in self.sess.resolve() {
            if let Some(r) = self.streams.get_mut(&d.stream) {
                r.policy_db = d.gain_db;
            }
        }
        let mut sends = Vec::new();
        for (id, r) in self.streams.iter_mut() {
            // Pro streams (production, measurement) never go through virtual
            // surround, nor do the apps the user opted out. Sent before the
            // stream's first level, so it is never heard the other way.
            let spatial = r.role != WireRole::Pro && self.volumes.user(&r.user).app(&r.app_id).spatial;
            if r.sent_spatial != Some(spatial) {
                r.sent_spatial = Some(spatial);
                sends.push(ToEngine::SetStreamSpatial { stream: *id, on: spatial });
            }
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
            if r.open {
                self.notify_controls(Some(&r.user));
            }
        }
    }

    fn owns(&self, conn: u64, stream: u32) -> bool {
        self.streams.get(&stream).is_some_and(|r| r.conn == conn && r.open)
    }

    fn outputs_msg(&self) -> FromControl {
        let outputs = self.sess.devices.iter()
            .map(|d| {
                let name = output_name(d);
                Output {
                    id: d.id,
                    balance: self.outputs.balance(&name),
                    channels: self.outputs.layout(&name),
                    spatial: self.outputs.spatial(&name),
                    name,
                }
            })
            .collect();
        FromControl::Outputs { current: self.sess.default_device, outputs }
    }

    /// Tell lyrad the current output's balance, speaker layout and virtual
    /// surround.
    fn send_balance(&mut self) {
        let sink = self.sess.default_device;
        let name = self.sess.devices.iter().find(|d| d.id == sink).map(output_name).unwrap_or_default();
        let (balance, channels) = (self.outputs.balance(&name), self.outputs.layout(&name));
        let _ = wire::send(&mut self.lyrad, &ToEngine::SetBalance { sink, balance });
        let _ = wire::send(&mut self.lyrad, &ToEngine::SetLayout { sink, channels });
        let on = self.outputs.spatial(&name);
        let _ = wire::send(&mut self.lyrad, &ToEngine::SetSpatial { sink, on });
    }

    /// Whether `user`'s session is the active one (always, without a seat).
    fn active(&self, user: &str) -> bool {
        match &self.seat {
            Some(p) => portcullis_peer::seat::active_at(p).as_deref() == Some(user),
            None => true,
        }
    }

    /// `user`'s apps with a stream open, for the volume UI.
    fn apps_for(&mut self, user: &str) -> FromControl {
        let mut apps: Vec<AppEntry> = Vec::new();
        if self.active(user) {
            let mut ids: Vec<&u32> = self.streams.iter().filter(|(_, r)| r.user == user && r.open).map(|(id, _)| id).collect();
            ids.sort();
            let recs: Vec<(String, String, WireRole)> = ids.iter()
                .map(|id| { let r = &self.streams[id]; (r.app_id.clone(), r.label.clone(), r.role) }).collect();
            for (app_id, label, role) in recs {
                if let Some(e) = apps.iter_mut().find(|e| e.app_id == app_id) {
                    e.streams = e.streams.saturating_add(1);
                    continue;
                }
                if apps.len() >= wire::MAX_CONTROL_APPS {
                    continue;
                }
                let l = self.volumes.user(user).app(&app_id);
                let mut shown = app_id.clone();
                shown.truncate(wire::MAX_APP_ID);
                apps.push(AppEntry { app_id: shown, label, role, volume: l.volume, muted: l.muted, streams: 1, spatial: l.spatial });
            }
        }
        let v = self.volumes.user(user);
        FromControl::Apps { master: v.master, master_muted: v.master_muted, apps }
    }

    fn to_control(&mut self, conn: u64, msg: &FromControl) {
        if let Some(c) = self.controls.get_mut(&conn) {
            if wire::send(&mut c.writer, msg).is_err() {
                let _ = c.writer.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// Tell `user`'s volume UIs (or everyone's, for `None`) the list changed.
    fn notify_controls(&mut self, user: Option<&str>) {
        let conns: Vec<(u64, String)> = self.controls.iter()
            .filter(|(_, c)| user.map_or(true, |u| c.user == u))
            .map(|(id, c)| (*id, c.user.clone())).collect();
        for (conn, u) in conns {
            let m = self.apps_for(&u);
            self.to_control(conn, &m);
        }
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
    let ticket = match wire::recv(&mut app) {
        Ok(ToChoragus::Hello { ticket, .. }) => ticket,
        _ => return,
    };
    let _ = app.set_read_timeout(None);
    let registry = cfg.registry.as_deref().map(|p| AppRegistry::load(p).unwrap_or_default());
    let grant = |user: &str, app_id: &str| match &cfg.policy_base {
        Some(base) => GrantStore::load_portcullis(base, user).granted(app_id),
        None => cfg.grants.granted(app_id),
    };
    let verified = match ticket {
        // An embedded player: it plays as the parent the ticket names, with
        // the parent's grant. (Its own jail reached this socket only because
        // its own manifest holds `audio`.) A ticket works once, only for the
        // uid limend said, and only while limend's embed lives.
        Some(t) => {
            let mut st = state.lock().unwrap();
            match st.tickets.get_mut(&t) {
                Some(r) if r.child_uid == uid && !r.used => {
                    r.used = true;
                    let (user, app_id) = (r.user.clone(), r.app_id.clone());
                    let caps = grant(&user, &app_id) & socket_caps;
                    eprintln!("choragusd: uid {uid} plays as '{app_id}' (user {user}) by attribution ticket");
                    Some(crate::identity::Verified { app_id, user, caps })
                }
                _ => None,
            }
        }
        None => {
            // In development (no registry, not strict) the "hello" app id is the uid's user.
            let claimed = portcullis_peer::username(uid).unwrap_or_default();
            crate::identity::resolve(uid, &claimed, registry.as_ref(), cfg.strict, socket_caps, grant)
        }
    };
    let Some(v) = verified.filter(|v| v.caps & CAP_AUDIO != 0) else {
        eprintln!("choragusd: uid {uid} refused (not in the registry, or no audio grant)");
        let _ = wire::send(&mut app, &ToApp::Denied { tag: 0, code: DenyCode::NotGranted });
        return;
    };
    let device_channels = state.lock().unwrap().device_channels;
    let welcome = ToApp::Welcome { device_rate: cfg.device_rate, device_channels, period_frames: cfg.period_frames };
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
        if let Some(r) = ticket.and_then(|t| st.tickets.get_mut(&t)) {
            r.conn = Some(c);
        } else if ticket.is_some() {
            // Cancelled between the check and now: the embed is gone.
            st.apps.remove(&c);
            return;
        }
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
                    conn, user: v.user.clone(), app_id: v.app_id.clone(), label: params.label.clone(),
                    role: params.role, tag, ring_bytes: params.ring_bytes(),
                    policy_db: 0.0, app_gain: 1.0, user_gain: 1.0, held, sent_db: None, sent_spatial: None, open: false,
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

fn output_name(d: &Device) -> String {
    match d.kind {
        DeviceKind::Speakers => "Speakers",
        DeviceKind::Headset => "Headset",
        DeviceKind::Hdmi => "HDMI",
    }
    .to_string()
}

/// Handle one volume UI connection (`audio-control/`, lyra-v1.md §7). It may
/// change only its own user's levels, and sees only that user's apps, and
/// only while that user's session is the active one.
fn serve_control(mut c: UnixStream, state: Shared, cfg: Arc<Config>, socket_caps: u8) {
    let uid = match lyra_protocol::peer::peer_uid(c.as_raw_fd()) {
        Ok(u) => u,
        Err(_) => return,
    };
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    if !matches!(wire::recv::<_, ToControl>(&mut c), Ok(ToControl::Hello { .. })) {
        return;
    }
    let _ = c.set_read_timeout(None);
    let registry = cfg.registry.as_deref().map(|p| AppRegistry::load(p).unwrap_or_default());
    let grant = |user: &str, app_id: &str| match &cfg.policy_base {
        Some(base) => GrantStore::load_portcullis(base, user).granted(app_id),
        None => cfg.grants.granted(app_id),
    };
    let claimed = portcullis_peer::username(uid).unwrap_or_default();
    let verified = crate::identity::resolve(uid, &claimed, registry.as_ref(), cfg.strict, socket_caps, grant);
    let Some(v) = verified.filter(|v| v.caps & CAP_CONTROL != 0) else {
        eprintln!("choragusd: uid {uid} refused on audio-control (no audio-control grant)");
        let _ = wire::send(&mut c, &FromControl::Denied { code: DenyCode::NotGranted });
        return;
    };
    if wire::send(&mut c, &FromControl::Welcome).is_err() {
        return;
    }
    let Ok(writer) = c.try_clone() else { return };
    let _ = writer.set_write_timeout(Some(Duration::from_secs(1)));
    let conn = {
        let mut st = state.lock().unwrap();
        st.next_conn += 1;
        let id = st.next_conn;
        st.controls.insert(id, ControlConn { writer, user: v.user.clone() });
        id
    };
    eprintln!("choragusd: volume UI '{}' (user {}, uid {uid}) connected", v.app_id, v.user);
    let user = v.user.as_str();
    while let Ok(msg) = wire::recv::<_, ToControl>(&mut c) {
        let mut st = state.lock().unwrap();
        let set_app = |st: &mut State, app_id: &str, f: &dyn Fn(&mut Level)| {
            if app_id.is_empty() || app_id.len() > wire::MAX_APP_ID {
                st.to_control(conn, &FromControl::Denied { code: DenyCode::BadFormat });
                return;
            }
            if let Err(e) = st.volumes.update(user, |u| f(u.apps.entry(app_id.to_string()).or_default())) {
                eprintln!("choragusd: saving {user}'s volumes: {e}");
            }
            st.recompute();
            st.notify_controls(Some(user));
        };
        match msg {
            ToControl::Hello { .. } => {}
            ToControl::List => {
                let m = st.apps_for(user);
                st.to_control(conn, &m);
            }
            ToControl::SetAppVolume { app_id, volume } => set_app(&mut st, &app_id, &|l| l.volume = clamp(volume)),
            ToControl::SetAppMute { app_id, muted } => set_app(&mut st, &app_id, &|l| l.muted = muted),
            ToControl::SetAppSpatial { app_id, on } => set_app(&mut st, &app_id, &|l| l.spatial = on),
            ToControl::SetMaster { .. } | ToControl::SetMasterMute { .. } => {
                let r = st.volumes.update(user, |u| match msg {
                    ToControl::SetMaster { volume } => u.master = clamp(volume),
                    ToControl::SetMasterMute { muted } => u.master_muted = muted,
                    _ => {}
                });
                if let Err(e) = r {
                    eprintln!("choragusd: saving {user}'s volumes: {e}");
                }
                st.recompute();
                st.notify_controls(Some(user));
            }
            ToControl::ListOutputs => {
                let m = st.outputs_msg();
                st.to_control(conn, &m);
            }
            ToControl::SetLayout { output, channels } => {
                let Some(d) = st.sess.devices.iter().find(|d| d.id == output).copied() else {
                    st.to_control(conn, &FromControl::Denied { code: DenyCode::NotGranted });
                    continue;
                };
                if !st.active(user) || !crate::volumes::LAYOUTS.contains(&channels) {
                    let code = if st.active(user) { DenyCode::BadFormat } else { DenyCode::NotGranted };
                    st.to_control(conn, &FromControl::Denied { code });
                    continue;
                }
                let dir = st.state_dir.clone();
                if let Err(e) = st.outputs.set_layout(&dir, &output_name(&d), channels) {
                    eprintln!("choragusd: saving output settings: {e}");
                }
                st.send_balance();
                let m = st.outputs_msg();
                for c in st.controls.keys().copied().collect::<Vec<_>>() {
                    st.to_control(c, &m);
                }
            }
            ToControl::SetSpatial { output, on } => {
                // A system setting of the output, like its balance.
                let Some(d) = st.sess.devices.iter().find(|d| d.id == output).copied() else {
                    st.to_control(conn, &FromControl::Denied { code: DenyCode::NotGranted });
                    continue;
                };
                if !st.active(user) {
                    st.to_control(conn, &FromControl::Denied { code: DenyCode::NotGranted });
                    continue;
                }
                let dir = st.state_dir.clone();
                if let Err(e) = st.outputs.set_spatial(&dir, &output_name(&d), on) {
                    eprintln!("choragusd: saving output settings: {e}");
                }
                st.send_balance();
                let m = st.outputs_msg();
                for c in st.controls.keys().copied().collect::<Vec<_>>() {
                    st.to_control(c, &m);
                }
            }
            ToControl::SetBalance { output, balance } => {
                // A system setting of the output: only the active session.
                let Some(d) = st.sess.devices.iter().find(|d| d.id == output).copied() else {
                    st.to_control(conn, &FromControl::Denied { code: DenyCode::NotGranted });
                    continue;
                };
                if !st.active(user) {
                    st.to_control(conn, &FromControl::Denied { code: DenyCode::NotGranted });
                    continue;
                }
                let dir = st.state_dir.clone();
                if let Err(e) = st.outputs.set_balance(&dir, &output_name(&d), balance) {
                    eprintln!("choragusd: saving output settings: {e}");
                }
                st.send_balance();
                let m = st.outputs_msg();
                for c in st.controls.keys().copied().collect::<Vec<_>>() {
                    st.to_control(c, &m);
                }
            }
            ToControl::SetOutput { output } => {
                // The output is the seat's, so only the active session moves it.
                if !st.active(user) || !st.sess.devices.iter().any(|d| d.id == output) {
                    st.to_control(conn, &FromControl::Denied { code: DenyCode::NotGranted });
                    continue;
                }
                let before = st.sess.resolve();
                st.sess.default_device = output;
                st.send_balance();
                for ch in crate::policy::diff(&before, &st.sess.resolve()) {
                    if let crate::policy::Change::Reroute { stream, to_sink } = ch {
                        let _ = wire::send(&mut st.lyrad, &ToEngine::Reroute { stream, sink: to_sink });
                    }
                }
                st.recompute();
            }
        }
    }
    state.lock().unwrap().controls.remove(&conn);
    eprintln!("choragusd: volume UI '{}' (uid {uid}) disconnected", v.app_id);
}

fn new_ticket() -> io::Result<Ticket> {
    use std::io::Read;
    let mut t = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut t)?;
    Ok(t)
}

/// Serve limend on `audio-attribution/` (lyra-v1.md §4). Only limend's uid;
/// its tickets die with its connection.
fn serve_attribution(mut c: UnixStream, state: Shared, cfg: Arc<Config>) {
    let uid = match lyra_protocol::peer::peer_uid(c.as_raw_fd()) {
        Ok(u) => u,
        Err(_) => return,
    };
    if cfg.limen_uid != Some(uid) {
        eprintln!("choragusd: uid {uid} refused on audio-attribution (not limend)");
        return;
    }
    let conn = {
        let mut st = state.lock().unwrap();
        st.next_conn += 1;
        st.next_conn
    };
    let grant = |user: &str, app_id: &str| match &cfg.policy_base {
        Some(base) => GrantStore::load_portcullis(base, user).granted(app_id),
        None => cfg.grants.granted(app_id),
    };
    while let Ok(msg) = wire::recv::<_, ToAttribution>(&mut c) {
        let reply = match msg {
            ToAttribution::Issue { child_uid, parent_uid } => {
                let registry = cfg.registry.as_deref().map(|p| AppRegistry::load(p).unwrap_or_default());
                let parent = registry.as_ref()
                    .and_then(|r| r.resolve(parent_uid))
                    .map(|(u, a)| (u.to_string(), a.to_string()));
                let mut st = state.lock().unwrap();
                let held = st.tickets.values().filter(|r| r.limen == conn).count();
                match parent {
                    Some((user, app_id)) if grant(&user, &app_id) & CAP_AUDIO != 0 && held < MAX_TICKETS => {
                        match new_ticket() {
                            Ok(t) => {
                                st.tickets.insert(t, TicketRec { child_uid, user, app_id, limen: conn, used: false, conn: None });
                                FromAttribution::Issued { ticket: t }
                            }
                            Err(_) => FromAttribution::Refused,
                        }
                    }
                    other => {
                        eprintln!("choragusd: no ticket for uid {child_uid} under uid {parent_uid} ({:?} lacks audio, or limend holds {held})",
                            other.map(|(_, a)| a));
                        FromAttribution::Refused
                    }
                }
            }
            ToAttribution::Cancel { ticket } => {
                let mut st = state.lock().unwrap();
                if st.tickets.get(&ticket).is_some_and(|r| r.limen == conn) {
                    cancel(&mut st, &ticket);
                }
                FromAttribution::Cancelled
            }
        };
        if wire::send(&mut c, &reply).is_err() {
            break;
        }
    }
    let mut st = state.lock().unwrap();
    let mine: Vec<Ticket> = st.tickets.iter().filter(|(_, r)| r.limen == conn).map(|(t, _)| *t).collect();
    for t in mine {
        cancel(&mut st, &t);
    }
}

/// End a ticket: an app connection that played with it is hung up (its
/// reader then closes its streams).
fn cancel(st: &mut State, t: &Ticket) {
    if let Some(r) = st.tickets.remove(t) {
        if let Some(a) = r.conn.and_then(|c| st.apps.get(&c)) {
            let _ = a.writer.shutdown(std::net::Shutdown::Both);
        }
    }
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
                let user = st.streams[&stream].user.clone();
                st.notify_controls(Some(&user));
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
            FromEngine::Layout { channels } => {
                let mut st = state.lock().unwrap();
                // The device refused a layout: keep what it has, and show it.
                let sink = st.sess.default_device;
                if let Some(name) = st.sess.devices.iter().find(|d| d.id == sink).map(output_name) {
                    if st.outputs.layout(&name) != channels {
                        eprintln!("choragusd: {name} can't take {} channels; kept at {channels}", st.outputs.layout(&name));
                        let dir = st.state_dir.clone();
                        let _ = st.outputs.set_layout(&dir, &name, channels);
                        let m = st.outputs_msg();
                        for c in st.controls.keys().copied().collect::<Vec<_>>() {
                            st.to_control(c, &m);
                        }
                    }
                }
                if channels != st.device_channels {
                    eprintln!("choragusd: output now {channels} channels");
                    st.device_channels = channels;
                    for conn in st.apps.keys().copied().collect::<Vec<_>>() {
                        st.to_app(conn, &ToApp::OutputChanged { device_channels: channels });
                    }
                }
            }
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
    st.notify_controls(None);
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
        volumes: VolumeStore::new(cfg.state_dir.clone()),
        outputs: OutputSettings::load(&cfg.state_dir),
        state_dir: cfg.state_dir.clone(),
        device_channels: cfg.device_channels,
        seat: cfg.seat.then(|| cfg.seat_path.clone()),
        controls: HashMap::new(),
        tickets: HashMap::new(),
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
                if caps == CAP_ATTRIBUTION {
                    std::thread::spawn(move || serve_attribution(app, state, cfg));
                } else if caps == CAP_CONTROL {
                    std::thread::spawn(move || serve_control(app, state, cfg, caps));
                } else {
                    std::thread::spawn(move || serve_app(app, state, cfg, caps));
                }
            }
        });
    }
    // lyrad starts centred; give it the kept balance.
    state.lock().unwrap().send_balance();
    let e = serve_lyrad(rd, state.clone());
    // Tell every app its streams are gone before we go.
    let mut st = state.lock().unwrap();
    let all: Vec<(u64, u32)> = st.streams.iter().map(|(id, r)| (r.conn, *id)).collect();
    for (conn, stream) in all {
        st.to_app(conn, &ToApp::StreamLost { stream });
    }
    e
}
