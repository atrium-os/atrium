//! limend (limen.md §1, §4, §5, §7).
//!
//! One thread per connection. Each connection has an outbox, drained by its
//! own thread between requests, so events never interleave with a reply.
//! frescod and portcullisd sit behind [`Compositor`] and [`Launcher`], so
//! the whole broker runs in tests with fakes.
//!
//! Identity is the connection: a parent's uid comes from the socket, and
//! its app id from the launch registry. A spawned child's connection is
//! one limend made and handed to portcullisd, so limend knows which embed
//! it is. A system provider is recognised by its registry app id.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aqueduct::{flag, Connection};
use serde::Serialize;

use crate::catalogue::{self, Provider, SlotKind};
use crate::proto::{self, op, refusal, ChildEvent, EmbedId, ParentEvent, Reply, WindowToken};

/// frescod's slot op (fresco_protocol::slot, on the activation socket).
pub trait Compositor: Send {
    /// Bind `child` into `parent` at `rect` (parent surface px), or release
    /// it with `None`. A bind answers the two windows' owner uids.
    fn bind(&mut self, parent: WindowToken, child: WindowToken, kind: SlotKind, rect: Option<[f32; 4]>, z: u8)
        -> Result<(u32, u32), u8>;

    /// A new embed-only frescod connection whose windows belong to
    /// `owner_uid` (fresco-video.md §8 S1): every window it opens stays
    /// hidden until bound, so a child can't put a top-level window on screen.
    fn embed_connection(&mut self, owner_uid: u32) -> Result<UnixStream, u8>;
}

/// portcullisd's embed-mode `ExecInstance` (limen.md §4).
pub trait Launcher: Send {
    /// Start `app_id` for `embed` with `channel` as its limend connection.
    /// Returns the uid the child runs as.
    fn launch(&mut self, app_id: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String>;
    /// End the child's jail.
    fn end(&mut self, embed: EmbedId);
}

/// choragusd's attribution tickets (lyra-v1.md §4): an embedded player plays
/// as its parent, and only if the parent holds `audio`.
pub trait Attribution: Send {
    /// A ticket for the instance running as `child_uid` to play for the app
    /// with `parent_uid`, or `None` (refused, or choragusd isn't there).
    fn issue(&mut self, child_uid: u32, parent_uid: u32) -> Option<[u8; 16]>;
    /// The embed ended.
    fn cancel(&mut self, ticket: [u8; 16]);
}

/// uid → (user, app id), from the launch registry.
pub type Resolve = Box<dyn Fn(u32) -> Option<(String, String)> + Send + Sync>;
/// (user, role) → the app that provides it (the user's default; limen.md §3.2).
pub type Defaults = Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

type ConnId = u64;

struct Embed {
    role: &'static catalogue::Role,
    parent: ConnId,
    parent_uid: u32,
    parent_window: WindowToken,
    /// Stacking among the parent's slots (`Request::z`).
    z: u8,
    child: ConnId,
    /// Known once the child connects (spawned: at launch; system: the provider).
    child_uid: u32,
    spawned: bool,
    /// choragusd's ticket, for a role that plays sound.
    audio_ticket: Option<[u8; 16]>,
    child_window: Option<WindowToken>,
    rect: Option<[f32; 4]>,
    visible: bool,
    bound: bool,
    attached: bool,
    to_child: Rate,
    to_parent: Rate,
}

#[derive(Default)]
struct Rate { since: Option<Instant>, n: u32, told: bool }

impl Rate {
    /// Whether one more message fits this second. `first_drop` is set the
    /// first time a message is dropped in a second.
    fn take(&mut self, now: Instant) -> (bool, bool) {
        if self.since.map_or(true, |s| now.duration_since(s) >= Duration::from_secs(1)) {
            *self = Rate { since: Some(now), n: 0, told: false };
        }
        self.n += 1;
        if self.n <= catalogue::MAX_MESSAGES_PER_SEC {
            return (true, false);
        }
        let first = !self.told;
        self.told = true;
        (false, first)
    }
}

/// An event for a connection, with the descriptor that goes with it.
type Out = (u16, Vec<u8>, Option<OwnedFd>);

#[derive(Default)]
struct State {
    next_conn: ConnId,
    next_embed: EmbedId,
    conns: HashMap<ConnId, Sender<Out>>,
    embeds: HashMap<EmbedId, Embed>,
    /// role → the system provider's connection.
    providers: HashMap<&'static str, (ConnId, u32)>,
}

impl State {
    fn push<T: Serialize>(&self, conn: ConnId, op: u16, ev: &T) {
        if let Some(tx) = self.conns.get(&conn) {
            let _ = tx.send((op, postcard::to_stdvec(ev).expect("encodes"), None));
        }
    }
    /// A message with a descriptor; if the child is gone, the descriptor
    /// is closed with the event.
    fn to_child_fd(&self, e: &Embed, ev: ChildEvent, fd: OwnedFd) {
        if let Some(tx) = self.conns.get(&e.child) {
            let _ = tx.send((op::EV_CHILD, postcard::to_stdvec(&ev).expect("encodes"), Some(fd)));
        }
    }
    fn to_parent(&self, e: &Embed, ev: ParentEvent) { self.push(e.parent, op::EV_PARENT, &ev) }
    fn to_child(&self, e: &Embed, ev: ChildEvent) { self.push(e.child, op::EV_CHILD, &ev) }
}

pub struct Shared {
    state: Mutex<State>,
    compositor: Mutex<Box<dyn Compositor>>,
    launcher: Mutex<Box<dyn Launcher>>,
    resolve: Resolve,
    defaults: Defaults,
    /// System providers known by a fixed system uid rather than a registry
    /// app id (arca-ui runs as `_arca-ui`, not as a Portcullis app).
    system_uids: Mutex<HashMap<&'static str, u32>>,
    attribution: Mutex<Option<Box<dyn Attribution>>>,
}

impl Shared {
    pub fn new(compositor: Box<dyn Compositor>, launcher: Box<dyn Launcher>, resolve: Resolve, defaults: Defaults) -> Arc<Self> {
        // Embed ids start somewhere unguessable-ish; ownership is what
        // protects them, the id only names them.
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let state = State { next_embed: seed | 1, ..State::default() };
        Arc::new(Self {
            state: Mutex::new(state),
            compositor: Mutex::new(compositor),
            launcher: Mutex::new(launcher),
            resolve,
            defaults,
            system_uids: Mutex::new(HashMap::new()),
            attribution: Mutex::new(None),
        })
    }

    /// Recognise the system provider `app` (a catalogue `Provider::System`)
    /// by `uid`, for a service that isn't a registry app.
    pub fn set_system_uid(&self, app: &'static str, uid: u32) {
        self.system_uids.lock().unwrap().insert(app, uid);
    }

    /// Where attribution tickets come from (choragusd). Without one, an
    /// embedded player gets no ticket and so plays nothing.
    pub fn set_attribution(&self, a: Box<dyn Attribution>) {
        *self.attribution.lock().unwrap() = Some(a);
    }

    /// Live embeds (tests and logs).
    pub fn embeds(&self) -> usize { self.state.lock().unwrap().embeds.len() }
}

/// Serve one client: a parent, or a system provider. `uid` is the peer's.
pub fn serve(conn: Connection, uid: u32, shared: Arc<Shared>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let id = {
        let mut st = shared.state.lock().unwrap();
        st.next_conn += 1;
        let id = st.next_conn;
        st.conns.insert(id, tx);
        id
    };
    run(conn, uid, id, rx, shared)
}

fn run(mut conn: Connection, uid: u32, id: ConnId, rx: Receiver<Out>, shared: Arc<Shared>) -> io::Result<()> {
    let r = (|| -> io::Result<()> {
        loop {
            while let Ok((op, payload, fd)) = rx.try_recv() {
                match fd {
                    Some(fd) => conn.send_message_with_fds(proto::CLASS, op, flag::ASYNC_EVENT, &payload, &[fd.as_raw_fd()])?,
                    None => conn.send_message(proto::CLASS, op, flag::ASYNC_EVENT, &payload)?,
                }
            }
            let Some(m) = conn.recv_message_or_timeout(Duration::from_millis(20))? else { continue };
            if m.opcode_class != proto::CLASS {
                continue;
            }
            // A SEND_FD's descriptor came with the message's first byte, so
            // it is queued by now; only that op takes one.
            let fd = if m.op == op::SEND_FD { conn.take_fds(1).and_then(|mut v| v.pop()) } else { None };
            // Descriptors sent with anything else are closed, so none waits
            // to be taken for a later message. (Clients wait for each reply,
            // so a later message's descriptor can't be queued yet.)
            if m.op != op::SEND_FD {
                while conn.take_fds(1).is_some() {}
            }
            let reply = handle(&shared, id, uid, m.op, &m.payload, fd);
            // Events this request caused go after its reply.
            conn.send_message(proto::CLASS, m.op, flag::IS_RESPONSE, &postcard::to_stdvec(&reply).unwrap())?;
        }
    })();
    gone(&shared, id);
    r
}

fn handle(shared: &Arc<Shared>, conn: ConnId, uid: u32, opc: u16, payload: &[u8], fd: Option<OwnedFd>) -> Reply {
    fn de<'a, T: serde::Deserialize<'a>>(b: &'a [u8]) -> Result<T, Reply> {
        postcard::from_bytes(b).map_err(|_| Reply::Refused { reason: refusal::MALFORMED })
    }
    let r = (|| -> Result<Reply, Reply> {
        match opc {
            op::REQUEST => request(shared, conn, uid, de(payload)?),
            op::PLACE => {
                let p: proto::Place = de(payload)?;
                if p.rect.is_some_and(|r| !r.iter().all(|v| v.is_finite()) || r[2] <= 0.0 || r[3] <= 0.0) {
                    return Err(Reply::Refused { reason: refusal::MALFORMED });
                }
                as_parent(shared, conn, p.embed, |e| e.rect = p.rect)
            }
            op::SET_VISIBLE => {
                let v: proto::SetVisible = de(payload)?;
                as_parent(shared, conn, v.embed, |e| e.visible = v.visible)
            }
            op::SEND => {
                let s: proto::Send = de(payload)?;
                send(shared, conn, s, None)
            }
            op::SEND_FD => {
                let s: proto::Send = de(payload)?;
                // A SEND_FD without its descriptor is malformed.
                let fd = fd.ok_or(Reply::Refused { reason: refusal::MALFORMED })?;
                send(shared, conn, s, Some(fd))
            }
            op::DETACH => {
                let d: proto::Detach = de(payload)?;
                let mut st = shared.state.lock().unwrap();
                if st.embeds.get(&d.embed).map(|e| e.parent) != Some(conn) {
                    return Err(Reply::Refused { reason: refusal::NOT_YOURS });
                }
                let e = st.embeds.remove(&d.embed).unwrap();
                end(shared, &st, d.embed, e, true);
                Ok(Reply::Ok)
            }
            op::PROVIDE => provide(shared, conn, uid, de(payload)?),
            op::READY => {
                let r: proto::Ready = de(payload)?;
                as_child(shared, conn, r.embed, |e| e.child_window = Some(r.window))
            }
            op::EMIT => emit(shared, conn, de(payload)?),
            op::FOCUS_OUT => {
                let f: proto::FocusOut = de(payload)?;
                let st = shared.state.lock().unwrap();
                match st.embeds.get(&f.embed) {
                    Some(e) if e.child == conn => {
                        st.to_parent(e, ParentEvent::FocusOut { embed: f.embed, forward: f.forward });
                        Ok(Reply::Ok)
                    }
                    _ => Err(Reply::Refused { reason: refusal::NOT_YOURS }),
                }
            }
            _ => Err(Reply::Refused { reason: refusal::MALFORMED }),
        }
    })();
    r.unwrap_or_else(|e| e)
}

fn request(shared: &Arc<Shared>, conn: ConnId, uid: u32, req: proto::Request) -> Result<Reply, Reply> {
    let refuse = |reason| Err(Reply::Refused { reason });
    let Some((user, app)) = (shared.resolve)(uid) else { return refuse(refusal::NOT_AN_APP) };
    let Some(role) = catalogue::role(&req.role) else { return refuse(refusal::UNKNOWN_ROLE) };
    let mut st = shared.state.lock().unwrap();
    if st.embeds.values().filter(|e| e.parent == conn).count() >= catalogue::MAX_EMBEDS_PER_PARENT {
        return refuse(refusal::TOO_MANY);
    }
    st.next_embed = st.next_embed.wrapping_add(1);
    let embed = st.next_embed;
    let (child, child_uid, spawned) = match role.provider {
        Provider::System(_) => match st.providers.get(role.name) {
            Some(&(c, u)) => (c, u, false),
            None => return refuse(refusal::NO_PROVIDER),
        },
        Provider::User => {
            let Some(provider) = (shared.defaults)(&user, role.name) else { return refuse(refusal::NO_PROVIDER) };
            let (ours, theirs) = UnixStream::pair().map_err(|_| Reply::Refused { reason: refusal::LAUNCH_FAILED })?;
            let (tx, rx) = mpsc::channel();
            st.next_conn += 1;
            let id = st.next_conn;
            st.conns.insert(id, tx);
            let child_uid = match shared.launcher.lock().unwrap().launch(&provider, embed, theirs) {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("limend: launching {provider} for {}: {e}", role.name);
                    st.conns.remove(&id);
                    return refuse(refusal::LAUNCH_FAILED);
                }
            };
            // The child's only way to frescod: an embed-only connection,
            // handed over on its channel before anything else is sent.
            let handed = shared.compositor.lock().unwrap().embed_connection(child_uid)
                .map_err(|r| format!("frescod refused an embed connection ({r})"))
                .and_then(|f| {
                    use std::os::fd::AsRawFd;
                    portcullis_ipc::send_fds(&ours, &[f.as_raw_fd()]).map_err(|e| e.to_string())
                });
            if let Err(e) = handed {
                eprintln!("limend: {provider} for {}: {e}", role.name);
                shared.launcher.lock().unwrap().end(embed);
                st.conns.remove(&id);
                return refuse(refusal::LAUNCH_FAILED);
            }
            let sh = shared.clone();
            let conn = Connection::wrap(ours).map_err(|_| Reply::Refused { reason: refusal::LAUNCH_FAILED })?;
            std::thread::spawn(move || { let _ = run(conn, child_uid, id, rx, sh); });
            (id, child_uid, true)
        }
    };
    // A player's ticket: choragusd checks the parent's `audio` grant. Only a
    // spawned child, whose uid limend knows from its own launch.
    let audio_ticket = (spawned && catalogue::AUDIO_ROLES.contains(&role.name))
        .then(|| shared.attribution.lock().unwrap().as_mut().and_then(|a| a.issue(child_uid, uid)))
        .flatten();
    let e = Embed {
        role, parent: conn, parent_uid: uid, parent_window: req.window, z: req.z, child, child_uid, spawned, audio_ticket,
        child_window: None, rect: None, visible: true, bound: false, attached: false,
        to_child: Rate::default(), to_parent: Rate::default(),
    };
    st.to_child(&e, ChildEvent::Attach { embed, role: role.name.into(), parent_app: app, parent_uid: uid });
    if let Some(ticket) = audio_ticket {
        st.to_child(&e, ChildEvent::AudioTicket { embed, ticket });
    }
    st.embeds.insert(embed, e);
    Ok(Reply::Embed { embed })
}

fn as_parent(shared: &Arc<Shared>, conn: ConnId, embed: EmbedId, f: impl FnOnce(&mut Embed)) -> Result<Reply, Reply> {
    let mut st = shared.state.lock().unwrap();
    match st.embeds.get_mut(&embed) {
        Some(e) if e.parent == conn => f(e),
        _ => return Err(Reply::Refused { reason: refusal::NOT_YOURS }),
    }
    rebind(shared, &mut st, embed);
    Ok(Reply::Ok)
}

fn as_child(shared: &Arc<Shared>, conn: ConnId, embed: EmbedId, f: impl FnOnce(&mut Embed)) -> Result<Reply, Reply> {
    let mut st = shared.state.lock().unwrap();
    match st.embeds.get_mut(&embed) {
        Some(e) if e.child == conn => f(e),
        _ => return Err(Reply::Refused { reason: refusal::NOT_YOURS }),
    }
    rebind(shared, &mut st, embed);
    Ok(Reply::Ok)
}

/// Bring frescod's slot in line with what the parent and child have said.
fn rebind(shared: &Arc<Shared>, st: &mut State, embed: EmbedId) {
    let Some(e) = st.embeds.get(&embed) else { return };
    let want = match (e.visible, e.rect, e.child_window) {
        (true, Some(r), Some(c)) => Some((r, c)),
        _ => None,
    };
    let mut comp = shared.compositor.lock().unwrap();
    match want {
        Some((rect, child)) => match comp.bind(e.parent_window, child, e.role.slot, Some(rect), e.z) {
            // frescod names the owners: each window must be its side's.
            Ok((pu, cu)) if pu == e.parent_uid && cu == e.child_uid => {
                let e = st.embeds.get_mut(&embed).unwrap();
                let (was_bound, was_attached) = (e.bound, e.attached);
                e.bound = true;
                e.attached = true;
                let e = &st.embeds[&embed];
                if !was_attached {
                    st.to_parent(e, ParentEvent::Attached { embed });
                } else if !was_bound {
                    st.to_parent(e, ParentEvent::Resumed { embed });
                    st.to_child(e, ChildEvent::Resumed { embed });
                }
            }
            Ok((pu, cu)) => {
                eprintln!("limend: embed {embed}: windows owned by {pu}/{cu}, not {}/{}", e.parent_uid, e.child_uid);
                let _ = comp.bind(e.parent_window, child, e.role.slot, None, e.z);
                drop(comp);
                let e = st.embeds.remove(&embed).unwrap();
                st.to_parent(&e, ParentEvent::ChildLost { embed });
                end(shared, st, embed, e, false);
            }
            Err(reason) => eprintln!("limend: embed {embed}: frescod refused the slot ({reason})"),
        },
        None if e.bound => {
            let child = e.child_window.unwrap();
            let _ = comp.bind(e.parent_window, child, e.role.slot, None, e.z);
            let e = st.embeds.get_mut(&embed).unwrap();
            e.bound = false;
            let e = &st.embeds[&embed];
            st.to_parent(e, ParentEvent::Suspended { embed });
            st.to_child(e, ChildEvent::Suspended { embed });
        }
        None => {}
    }
}

/// A parent's message for its child, and on a role that takes one, a
/// descriptor. Every refusal closes the descriptor (it is dropped here).
fn send(shared: &Arc<Shared>, conn: ConnId, s: proto::Send, fd: Option<OwnedFd>) -> Result<Reply, Reply> {
    let mut st = shared.state.lock().unwrap();
    let Some(e) = st.embeds.get_mut(&s.embed).filter(|e| e.parent == conn) else {
        return Err(Reply::Refused { reason: refusal::NOT_YOURS });
    };
    if s.bytes.len() > catalogue::MAX_MESSAGE {
        return Err(Reply::Refused { reason: refusal::TOO_BIG });
    }
    if fd.is_some() && !e.role.takes_fd {
        return Err(Reply::Refused { reason: refusal::MALFORMED });
    }
    if let Some(f) = fd.as_ref() {
        if crate::fdpass::check(f).is_err() || crate::fdpass::narrow(f).is_err() {
            return Err(Reply::Refused { reason: refusal::BAD_FD });
        }
    }
    // A descriptor counts like any message.
    if !e.to_child.take(Instant::now()).0 {
        return Err(Reply::Refused { reason: refusal::RATE });
    }
    let e = &st.embeds[&s.embed];
    match fd {
        Some(fd) => st.to_child_fd(e, ChildEvent::MessageFd { embed: s.embed, bytes: s.bytes }, fd),
        None => st.to_child(e, ChildEvent::Message { embed: s.embed, bytes: s.bytes }),
    }
    Ok(Reply::Ok)
}

fn emit(shared: &Arc<Shared>, conn: ConnId, m: proto::Emit) -> Result<Reply, Reply> {
    let mut st = shared.state.lock().unwrap();
    let Some(e) = st.embeds.get_mut(&m.embed).filter(|e| e.child == conn) else {
        return Err(Reply::Refused { reason: refusal::NOT_YOURS });
    };
    if m.bytes.len() > catalogue::MAX_MESSAGE {
        return Err(Reply::Refused { reason: refusal::TOO_BIG });
    }
    let (ok, first_drop) = e.to_parent.take(Instant::now());
    let e = &st.embeds[&m.embed];
    if !ok {
        if first_drop {
            st.to_parent(e, ParentEvent::Dropped { embed: m.embed });
        }
        return Err(Reply::Refused { reason: refusal::RATE });
    }
    st.to_parent(e, ParentEvent::Message { embed: m.embed, bytes: m.bytes });
    Ok(Reply::Ok)
}

fn provide(shared: &Arc<Shared>, conn: ConnId, uid: u32, p: proto::Provide) -> Result<Reply, Reply> {
    let role = catalogue::role(&p.role).ok_or(Reply::Refused { reason: refusal::UNKNOWN_ROLE })?;
    let Provider::System(app) = role.provider else { return Err(Reply::Refused { reason: refusal::NOT_PROVIDER }) };
    // Only the catalogue's own service: by its fixed system uid when one is
    // configured, otherwise by its registry identity.
    let fixed = shared.system_uids.lock().unwrap().get(app).copied();
    let ok = match fixed {
        Some(u) => u == uid,
        None => matches!((shared.resolve)(uid), Some((_, a)) if a == app),
    };
    if !ok {
        return Err(Reply::Refused { reason: refusal::NOT_PROVIDER });
    }
    shared.state.lock().unwrap().providers.insert(role.name, (conn, uid));
    Ok(Reply::Ok)
}

/// Take `e` (already removed) down: release the slot, tell the child, end
/// a spawned child's jail after the grace period.
fn end(shared: &Arc<Shared>, st: &State, embed: EmbedId, e: Embed, tell_parent: bool) {
    if e.bound {
        if let Some(c) = e.child_window {
            let _ = shared.compositor.lock().unwrap().bind(e.parent_window, c, e.role.slot, None, e.z);
        }
    }
    st.to_child(&e, ChildEvent::Detaching { embed });
    if let Some(t) = e.audio_ticket {
        if let Some(a) = shared.attribution.lock().unwrap().as_mut() {
            a.cancel(t);
        }
    }
    if tell_parent {
        st.to_parent(&e, ParentEvent::Detached { embed });
    }
    if e.spawned {
        let sh = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(catalogue::DETACH_GRACE_MS));
            sh.launcher.lock().unwrap().end(embed);
        });
    }
}

/// A connection closed: its embeds end, as parent or as child.
fn gone(shared: &Arc<Shared>, conn: ConnId) {
    let mut st = shared.state.lock().unwrap();
    st.conns.remove(&conn);
    st.providers.retain(|_, (c, _)| *c != conn);
    let mine: Vec<EmbedId> = st.embeds.iter().filter(|(_, e)| e.parent == conn || e.child == conn).map(|(k, _)| *k).collect();
    for id in mine {
        let e = st.embeds.remove(&id).unwrap();
        if e.child == conn {
            st.to_parent(&e, ParentEvent::ChildLost { embed: id });
        }
        end(shared, &st, id, e, false);
    }
}
