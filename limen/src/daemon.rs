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

    /// One compositor-owned control for the currently bound island.
    fn watch_island_stop(&mut self, _parent: WindowToken, _child: WindowToken) -> io::Result<Option<UnixStream>> {
        Ok(None)
    }

    /// Whether frescod routed a pointer or key press to `window` in the
    /// last `ms` (the PiP gesture check). No frescod support: never.
    fn pressed_within(&mut self, _window: WindowToken, _ms: u32) -> bool {
        false
    }

    /// Take `child` out of `parent`'s slot but keep it shown, for Forum to
    /// place in the `pip` layer. A later `bind` puts it back.
    fn release(&mut self, _parent: WindowToken, _child: WindowToken) -> Result<(), u8> {
        Err(u8::MAX)
    }
}

/// portcullisd's embed-mode `ExecInstance` (limen.md §4).
pub trait Launcher: Send {
    /// Start `app_id` for `embed` with `channel` as its limend connection.
    /// Returns the uid the child runs as.
    fn launch(&mut self, app_id: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String>;
    /// [`Launcher::launch`] for the parent running as `parent_uid`: a
    /// launcher whose children run as a uid of the parent's instance
    /// (person and persona) needs it.
    fn launch_for(&mut self, app_id: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32) -> Result<u32, String> {
        let _ = parent_uid;
        self.launch(app_id, embed, channel)
    }
    /// [`Launcher::launch_for`], with the child in `persona` (one of the
    /// parent's person's) rather than the parent's own: a share across
    /// personas (share-sheet.md §7). The launcher's authority decides
    /// whether that crossing is on; one that can't ask refuses.
    fn launch_in(&mut self, app_id: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32, persona: &str) -> Result<u32, String> {
        let _ = (app_id, embed, channel, parent_uid);
        Err(format!("this launcher starts nothing in another persona ({persona})"))
    }
    /// Start the fixed Navigator engine with its private fetch transport.
    fn launch_island(&mut self, _embed: EmbedId, _channel: UnixStream, _parent_uid: u32, _fetch: UnixStream) -> Result<u32, String> {
        Err("this launcher does not support an island fetch channel".into())
    }
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
/// A uid's named persona (`None`: Personal).
pub type PersonaOf = Box<dyn Fn(u32) -> Option<String> + Send + Sync>;
/// (user, role) → the app that provides it (the user's default; limen.md §3.2).
pub type Defaults = Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;
/// (parent app id, role) → whether its manifest lets it embed the role.
pub type EmbedCheck = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;
/// (person, app id) → whether the share sheet may choose the app as a
/// `share-target` for that person (its installed manifest implements the
/// role; a dev build only for its own person).
pub type TargetCheck = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;

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
    binding_generation: u64,
    island: Option<crate::web_island::Lifecycle>,
    media_ready: bool,
    attached: bool,
    /// In picture-in-picture: out of the slot, in Forum's `pip` layer.
    pip: bool,
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
    mail_sheets: HashMap<(String, Option<String>), (ConnId, u32)>,
    /// parent → the app the person chose for its next `share-target`, and
    /// when (share-sheet.md §3).
    choices: HashMap<ConnId, (String, Option<String>, Instant)>,
    /// When each window last opened a print sheet, within the last minute
    /// (printing.md §5.2).
    print_opens: HashMap<WindowToken, Vec<Instant>>,
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
    /// A message with a descriptor for the parent (a picked file).
    fn to_parent_fd(&self, e: &Embed, ev: ParentEvent, fd: OwnedFd) {
        if let Some(tx) = self.conns.get(&e.parent) {
            let _ = tx.send((op::EV_PARENT, postcard::to_stdvec(&ev).expect("encodes"), Some(fd)));
        }
    }
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
    /// A uid's persona, from the launch registry (`None`: Personal).
    persona: Mutex<Option<PersonaOf>>,
    mail_sheet_provider: Mutex<Option<Box<dyn Fn(u32) -> bool + Send + Sync>>>,
    attribution: Mutex<Option<Box<dyn Attribution>>>,
    /// Without one (tests), any app may embed any role.
    embed_check: Mutex<Option<EmbedCheck>>,
    /// Without one (tests), the share sheet may choose any app.
    target_check: Mutex<Option<TargetCheck>>,
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
            persona: Mutex::new(None),
            mail_sheet_provider: Mutex::new(None),
            attribution: Mutex::new(None),
            embed_check: Mutex::new(None),
            target_check: Mutex::new(None),
        })
    }

    /// Recognise the system provider `app` (a catalogue `Provider::System`)
    /// by `uid`, for a service that isn't a registry app.
    pub fn set_system_uid(&self, app: &'static str, uid: u32) {
        self.system_uids.lock().unwrap().insert(app, uid);
    }

    /// How a parent's persona is found, for [`catalogue::PERSONA_ROLES`].
    /// Without one every parent counts as Personal.
    pub fn set_persona_lookup(&self, f: PersonaOf) {
        *self.persona.lock().unwrap() = Some(f);
    }

    /// Only the Navigator signed by the configured web broker publisher can frame Mail sign-in.
    pub fn set_mail_sheet_provider(&self, check: Box<dyn Fn(u32) -> bool + Send + Sync>) {
        *self.mail_sheet_provider.lock().unwrap() = Some(check);
    }

    /// Where attribution tickets come from (choragusd). Without one, an
    /// embedded player gets no ticket and so plays nothing.
    pub fn set_attribution(&self, a: Box<dyn Attribution>) {
        *self.attribution.lock().unwrap() = Some(a);
    }

    /// Who may embed what: limend's is the installed manifests' `embeds`
    /// ([`crate::manifests::Installed::may_embed`]).
    pub fn set_embed_check(&self, c: EmbedCheck) {
        *self.embed_check.lock().unwrap() = Some(c);
    }

    /// Which apps the share sheet may choose: limend's is the installed
    /// manifests' `[roles] implements` ([`crate::manifests::Installed::may_target`]).
    pub fn set_target_check(&self, c: TargetCheck) {
        *self.target_check.lock().unwrap() = Some(c);
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
    let mut fresh = true;
    let mut navigator_ui = false;
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
            // A SEND_FD's (or EMIT_FD's) descriptor came with the message's
            // first byte, so it is queued by now; only those ops take one.
            let takes = m.op == op::SEND_FD || m.op == op::EMIT_FD;
            let fd = if takes { conn.take_fds(1).and_then(|mut v| v.pop()) } else { None };
            // Descriptors sent with anything else are closed, so none waits
            // to be taken for a later message. (Clients wait for each reply,
            // so a later message's descriptor can't be queued yet.)
            if !takes {
                while conn.take_fds(1).is_some() {}
            }
            let reply = if m.op == op::DELEGATE_NAVIGATOR_UI {
                if fresh && m.payload.is_empty() {
                    navigator_ui = true;
                    Reply::Ok
                } else {
                    Reply::Refused { reason: refusal::MALFORMED }
                }
            } else if navigator_ui && !navigator_ui_operation(m.op, &m.payload) {
                Reply::Refused { reason: refusal::NOT_PROVIDER }
            } else {
                handle(&shared, id, uid, m.op, &m.payload, fd)
            };
            fresh = false;
            // Events this request caused go after its reply.
            conn.send_message(proto::CLASS, m.op, flag::IS_RESPONSE, &postcard::to_stdvec(&reply).unwrap())?;
        }
    })();
    gone(&shared, id);
    r
}

// The UI receives a broker-opened stream. Keep the restriction on the
// server end so it applies to every duplicate of the delegated descriptor.
fn navigator_ui_operation(opc: u16, payload: &[u8]) -> bool {
    match opc {
        op::REQUEST => postcard::from_bytes::<proto::Request>(payload)
            .is_ok_and(|req| matches!(req.role.as_str(), "doc-viewer" | "vpn-sign-in-page" | "credential" | "print")),
        op::PLACE | op::SEND | op::SEND_FD | op::DETACH | op::SET_VISIBLE => true,
        _ => false,
    }
}

fn handle(shared: &Arc<Shared>, conn: ConnId, uid: u32, opc: u16, payload: &[u8], fd: Option<OwnedFd>) -> Reply {
    fn de<'a, T: serde::Deserialize<'a>>(b: &'a [u8]) -> Result<T, Reply> {
        postcard::from_bytes(b).map_err(|_| Reply::Refused { reason: refusal::MALFORMED })
    }
    let r = (|| -> Result<Reply, Reply> {
        match opc {
            op::REQUEST => request(shared, conn, uid, de(payload)?, false),
            op::REQUEST_ISLAND => {
                let (window, z) = de(payload)?;
                request(shared, conn, uid, proto::Request { role: "web-island".into(), window, z }, true)
            }
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
                ready(shared, conn, r.embed, r.window)
            }
            op::EMIT => emit(shared, conn, de(payload)?, None),
            op::EMIT_FD => {
                let m: proto::Emit = de(payload)?;
                let fd = fd.ok_or(Reply::Refused { reason: refusal::MALFORMED })?;
                emit(shared, conn, m, Some(fd))
            }
            op::PIP => pip(shared, conn, de(payload)?),
            op::CHOOSE => choose(shared, conn, de(payload)?),
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

fn request(shared: &Arc<Shared>, conn: ConnId, uid: u32, req: proto::Request, with_fetch: bool) -> Result<Reply, Reply> {
    let refuse = |reason| Err(Reply::Refused { reason });
    let print_pin = req.role == "credential"
        && shared.system_uids.lock().unwrap().get(catalogue::PRELUM) == Some(&uid);
    let (user, app) = if print_pin {
        (String::new(), catalogue::PRELUM.into())
    } else {
        let Some(identity) = (shared.resolve)(uid) else { return refuse(refusal::NOT_AN_APP) };
        identity
    };
    let Some(role) = catalogue::role(&req.role) else { return refuse(refusal::UNKNOWN_ROLE) };
    // A chosen role needs no `embeds` of its own: the choice can only come
    // from a share sheet the parent opened, which did need it.
    if let (Provider::System(_) | Provider::Worker(_) | Provider::User, Some(check)) = (role.provider, shared.embed_check.lock().unwrap().as_ref()) {
        if !print_pin && !check(&app, role.name) {
            eprintln!("limend: {app} (uid {uid}) may not embed {}: not in its manifest's embeds", role.name);
            return refuse(refusal::NOT_EMBEDDABLE);
        }
    }
    let mut st = shared.state.lock().unwrap();
    if st.embeds.values().filter(|e| e.parent == conn).count() >= catalogue::MAX_EMBEDS_PER_PARENT {
        return refuse(refusal::TOO_MANY);
    }
    if (role.name == "navigator-inbox-page" || with_fetch) && (app != "org.atrium.navigator" ||
        !shared.mail_sheet_provider.lock().unwrap().as_ref().is_some_and(|check| check(uid))) {
        return refuse(refusal::NOT_PROVIDER);
    }
    if role.name == "vpn-sign-in-page" {
        let signed = shared.mail_sheet_provider.lock().unwrap().as_ref().is_some_and(|check| check(uid));
        let persona = shared.persona.lock().unwrap().as_ref().and_then(|lookup| lookup(uid));
        if app != "org.atrium.navigator" || !signed || persona.as_deref() != Some("vpn") {
            return refuse(refusal::NOT_PROVIDER);
        }
    }
    if role.slot == SlotKind::SignInPage && !st.embeds.values().any(|sheet|
        sheet.role.name == "mail-sign-in" && sheet.child_uid == uid && sheet.child_window == Some(req.window)
    ) {
        return refuse(refusal::NOT_PROVIDER);
    }
    if role.name == "print" {
        // One print sheet per window, and a window that keeps opening it
        // is slowed (printing.md §5.2).
        if st.embeds.values().any(|e| e.role.name == "print" && e.parent_window == req.window) {
            return refuse(refusal::TOO_MANY);
        }
        let now = Instant::now();
        let opens = st.print_opens.entry(req.window).or_default();
        opens.retain(|t| now.duration_since(*t) < catalogue::PRINT_WINDOW);
        if opens.len() >= catalogue::PRINT_FREE && opens.last().is_some_and(|t| now.duration_since(*t) < catalogue::PRINT_SLOWED) {
            return refuse(refusal::RATE);
        }
        let stale: Vec<WindowToken> = st.print_opens.iter().filter(|(_, v)| v.iter().all(|t| now.duration_since(*t) >= catalogue::PRINT_WINDOW)).map(|(k, _)| *k).collect();
        for k in stale {
            st.print_opens.remove(&k);
        }
    }
    st.next_embed = st.next_embed.wrapping_add(1);
    let embed = st.next_embed;
    let mut broker_fetch = None;
    let (child, child_uid, spawned) = match role.provider {
        Provider::System(_) if role.name == "mail-sign-in" => {
            if app != "org.atrium.mail" { return refuse(refusal::NOT_AN_APP); }
            let persona = shared.persona.lock().unwrap().as_ref().and_then(|f| f(uid));
            match st.mail_sheets.get(&(user.clone(), persona)) {
                Some(&(c, u)) => (c, u, false),
                None => return refuse(refusal::NO_PROVIDER),
            }
        }
        Provider::System(_) => match st.providers.get(role.name) {
            Some(&(c, u)) => (c, u, false),
            None => return refuse(refusal::NO_PROVIDER),
        },
        Provider::Worker(_) | Provider::User | Provider::Chosen => {
            let provider = match role.provider {
                Provider::Worker(app) => Some((app.to_string(), None)),
                Provider::Chosen => match st.choices.remove(&conn) {
                    Some((app, persona, at)) if at.elapsed() <= Duration::from_millis(catalogue::CHOICE_MS) => Some((app, persona)),
                    _ => None,
                },
                _ => (shared.defaults)(&user, role.name).map(|a| (a, None)),
            };
            let Some((provider, persona)) = provider else { return refuse(refusal::NO_PROVIDER) };
            let (ours, theirs) = UnixStream::pair().map_err(|_| Reply::Refused { reason: refusal::LAUNCH_FAILED })?;
            let (tx, rx) = mpsc::channel();
            st.next_conn += 1;
            let id = st.next_conn;
            st.conns.insert(id, tx);
            let launched = if with_fetch {
                UnixStream::pair().map_err(|e| e.to_string()).and_then(|(broker, worker)| {
                    broker_fetch = Some(broker);
                    shared.launcher.lock().unwrap().launch_island(embed, theirs, uid, worker)
                })
            } else {
                match &persona {
                    Some(p) => shared.launcher.lock().unwrap().launch_in(&provider, embed, theirs, uid, p),
                    None => shared.launcher.lock().unwrap().launch_for(&provider, embed, theirs, uid),
                }
            };
            let child_uid = match launched {
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
        child_window: None, rect: None, visible: true, bound: false, binding_generation: 0, island: with_fetch.then(Default::default), media_ready: false, attached: false, pip: false,
        to_child: Rate::default(), to_parent: Rate::default(),
    };
    st.to_child(&e, ChildEvent::Attach { embed, role: role.name.into(), parent_app: app, parent_uid: uid });
    if let Some(ticket) = audio_ticket {
        st.to_child(&e, ChildEvent::AudioTicket { embed, ticket });
    }
    if catalogue::PERSONA_ROLES.contains(&role.name) {
        let persona = shared.persona.lock().unwrap().as_ref().and_then(|f| f(uid));
        st.to_child(&e, ChildEvent::ParentPersona { embed, persona });
    }
    if role.name == "print" {
        st.print_opens.entry(req.window).or_default().push(Instant::now());
    }
    if let Some(socket) = broker_fetch {
        st.to_parent_fd(&e, ParentEvent::IslandFetch { embed }, socket.into());
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

fn ready(shared: &Arc<Shared>, conn: ConnId, embed: EmbedId, window: WindowToken) -> Result<Reply, Reply> {
    let mut st = shared.state.lock().unwrap();
    match st.embeds.get_mut(&embed) {
        Some(e) if e.child == conn => {
            if e.role.name == "web-island" && e.child_window.is_some_and(|old| old != window) {
                return Err(Reply::Refused { reason: refusal::MALFORMED });
            }
            e.child_window = Some(window);
        }
        _ => return Err(Reply::Refused { reason: refusal::NOT_YOURS }),
    }
    rebind(shared, &mut st, embed);
    Ok(Reply::Ok)
}

/// Bring frescod's slot in line with what the parent and child have said.
fn rebind(shared: &Arc<Shared>, st: &mut State, embed: EmbedId) {
    let Some(e) = st.embeds.get(&embed) else { return };
    // In PiP the window is Forum's: the slot's place and visibility are
    // kept for its return, and a hidden slot suspends nothing (§3.2).
    if e.pip {
        return;
    }
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
                if !was_bound { e.binding_generation += 1; }
                let generation = e.binding_generation;
                e.bound = true;
                e.attached = true;
                let e = &st.embeds[&embed];
                if !was_bound && e.role.name == "web-island" {
                    match comp.watch_island_stop(e.parent_window, child) {
                        Ok(Some(mut stop)) => {
                            let shared = shared.clone();
                            std::thread::spawn(move || {
                                use std::io::Read;
                                let mut byte = [0];
                                if stop.read_exact(&mut byte).is_err() || byte != [1] { return; }
                                let mut st = shared.state.lock().unwrap();
                                if st.embeds.get(&embed).is_some_and(|e| e.bound && e.binding_generation == generation && e.child_window == Some(child)) {
                                    let e = st.embeds.remove(&embed).unwrap();
                                    st.to_parent(&e, ParentEvent::IslandStopped { embed });
                                    end(&shared, &st, embed, e, false);
                                }
                            });
                        }
                        Err(err) => eprintln!("limend: island stop control: {err}"),
                        Ok(None) => {}
                    }
                }
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
    if e.role.name == "web-island" && e.island.is_none() && s.bytes != b"advance" {
        return Err(Reply::Refused { reason: refusal::MALFORMED });
    }
    if fd.is_some() && !e.role.takes_fd {
        return Err(Reply::Refused { reason: refusal::MALFORMED });
    }
    if let Some(f) = fd.as_ref() {
        if e.role.name == "picker" {
            use crate::picker::{Kind, ToChild};
            let save = match ToChild::decode(&s.bytes) {
                Some(ToChild::Open(filter)) => filter.kind == Kind::Save,
                Some(ToChild::Commit(_)) => true,
                _ => false,
            };
            if !save { return Err(Reply::Refused { reason: refusal::MALFORMED }); }
            if !crate::fdpass::sealed(f) {
                return Err(Reply::Refused { reason: refusal::BAD_FD });
            }
        }
        if crate::fdpass::check(f).is_err() || crate::fdpass::narrow(f).is_err() {
            return Err(Reply::Refused { reason: refusal::BAD_FD });
        }
    }
    // A descriptor counts like any message.
    if !e.to_child.take(Instant::now()).0 {
        return Err(Reply::Refused { reason: refusal::RATE });
    }
    if let Some(state) = &mut e.island {
        let accepted = e.bound && state.accepts_control(&s.bytes, e.media_ready);
        if !accepted { return Err(Reply::Refused { reason: refusal::MALFORMED }); }
    }
    let e = &st.embeds[&s.embed];
    match fd {
        Some(fd) => st.to_child_fd(e, ChildEvent::MessageFd { embed: s.embed, bytes: s.bytes }, fd),
        None => st.to_child(e, ChildEvent::Message { embed: s.embed, bytes: s.bytes }),
    }
    Ok(Reply::Ok)
}

/// A child's message for its parent, and on a role that returns one (the
/// picker), a regular file. Every refusal closes the descriptor.
fn emit(shared: &Arc<Shared>, conn: ConnId, m: proto::Emit, fd: Option<OwnedFd>) -> Result<Reply, Reply> {
    let mut st = shared.state.lock().unwrap();
    let Some(e) = st.embeds.get_mut(&m.embed).filter(|e| e.child == conn) else {
        return Err(Reply::Refused { reason: refusal::NOT_YOURS });
    };
    if m.bytes.len() > catalogue::MAX_MESSAGE {
        return Err(Reply::Refused { reason: refusal::TOO_BIG });
    }
    if fd.is_some() && !e.role.returns_fd {
        return Err(Reply::Refused { reason: refusal::MALFORMED });
    }
    // Only once the child's window is in the slot: a pick is something the
    // user did there.
    if fd.is_some() && !e.attached {
        return Err(Reply::Refused { reason: refusal::NOT_ATTACHED });
    }
    if let Some(f) = fd.as_ref() {
        match crate::fdpass::check_returned(f) {
            Ok(w) if crate::fdpass::narrow_returned(f, w).is_ok() => {}
            _ => return Err(Reply::Refused { reason: refusal::BAD_FD }),
        }
    }
    if e.role.name == "web-island" {
        if e.island.is_some() && !e.bound {
            return Err(Reply::Refused { reason: refusal::NOT_ATTACHED });
        }
        if e.island.is_some() && m.bytes.len() > crate::web_island::MAX_FRAME {
            return Err(Reply::Refused { reason: refusal::TOO_BIG });
        }
        if e.island.is_none() && m.bytes != b"frame" {
            return Err(Reply::Refused { reason: refusal::MALFORMED });
        }
    }
    let (ok, first_drop) = e.to_parent.take(Instant::now());
    if ok {
        if let Some(lifecycle) = &mut e.island {
            match lifecycle.receive(&m.bytes) {
                Some(crate::web_island::Report::MediaReady) => e.media_ready = true,
                Some(_) => {}
                None => return Err(Reply::Refused { reason: refusal::MALFORMED }),
            }
        }
    }
    let e = &st.embeds[&m.embed];
    if !ok {
        if first_drop {
            st.to_parent(e, ParentEvent::Dropped { embed: m.embed });
        }
        return Err(Reply::Refused { reason: refusal::RATE });
    }
    match fd {
        Some(fd) => st.to_parent_fd(e, ParentEvent::MessageFd { embed: m.embed, bytes: m.bytes }, fd),
        None => st.to_parent(e, ParentEvent::Message { embed: m.embed, bytes: m.bytes }),
    }
    Ok(Reply::Ok)
}

/// The child's PiP request (atrium-player-pip-keys.md §3).
fn pip(shared: &Arc<Shared>, conn: ConnId, p: proto::Pip) -> Result<Reply, Reply> {
    let no = Err(Reply::Refused { reason: refusal::NO_PIP });
    let mut st = shared.state.lock().unwrap();
    let Some(e) = st.embeds.get(&p.embed).filter(|e| e.child == conn) else {
        return Err(Reply::Refused { reason: refusal::NOT_YOURS });
    };
    if !p.on {
        if !e.pip {
            return Ok(Reply::Ok);
        }
        pip_off(shared, &mut st, p.embed);
        return Ok(Reply::Ok);
    }
    if e.pip {
        return Ok(Reply::Ok);
    }
    // Only a shown player: bound into a visible slot.
    if !catalogue::PIP_ROLES.contains(&e.role.name) || !e.bound || p.aspect.contains(&0) || p.aspect.iter().any(|&v| v > 100_000) {
        return no;
    }
    let (parent_w, child_w) = (e.parent_window, e.child_window.expect("bound"));
    {
        let mut comp = shared.compositor.lock().unwrap();
        // A real press, routed by frescod, on the player or its page.
        if !comp.pressed_within(child_w, catalogue::PIP_GESTURE_MS) && !comp.pressed_within(parent_w, catalogue::PIP_GESTURE_MS) {
            return no;
        }
    }
    // One PiP at a time: the new one replaces the last.
    let last: Vec<EmbedId> = st.embeds.iter().filter(|(_, e)| e.pip).map(|(k, _)| *k).collect();
    for k in last {
        pip_off(shared, &mut st, k);
    }
    // Released with keep_shown, frescod tells forum-wm it is a PiP window
    // (WmRole::Pip), and forum-wm puts it in the pip layer.
    if let Err(r) = shared.compositor.lock().unwrap().release(parent_w, child_w) {
        eprintln!("limend: embed {}: frescod would not release the slot for PiP ({r})", p.embed);
        return no;
    }
    let e = st.embeds.get_mut(&p.embed).unwrap();
    e.pip = true;
    e.bound = false;
    let e = &st.embeds[&p.embed];
    st.to_parent(e, ParentEvent::Pip { embed: p.embed, on: true });
    st.to_child(e, ChildEvent::Pip { embed: p.embed, on: true });
    Ok(Reply::Ok)
}

/// Out of PiP: Forum drops the window, and it goes back into its slot as
/// the parent last placed it, or is suspended if the slot is hidden now.
fn pip_off(shared: &Arc<Shared>, st: &mut State, embed: EmbedId) {
    let Some(e) = st.embeds.get_mut(&embed) else { return };
    e.pip = false;
    // Bound into its slot again (below), it is no longer frescod's PiP
    // window, and forum-wm drops it from the pip layer.
    let e = &st.embeds[&embed];
    st.to_parent(e, ParentEvent::Pip { embed, on: false });
    st.to_child(e, ChildEvent::Pip { embed, on: false });
    let shown = e.visible && e.rect.is_some();
    if shown {
        rebind_quiet(shared, st, embed);
    } else {
        st.to_parent(e, ParentEvent::Suspended { embed });
        st.to_child(e, ChildEvent::Suspended { embed });
    }
}

/// `rebind`, without the `Resumed` a return to the slot would otherwise
/// send (the `Pip` event said it).
fn rebind_quiet(shared: &Arc<Shared>, st: &mut State, embed: EmbedId) {
    if let Some(e) = st.embeds.get_mut(&embed) {
        e.bound = true;
    }
    // Bound already as far as rebind knows, so it binds without telling.
    let Some(e) = st.embeds.get(&embed) else { return };
    let (Some(rect), Some(child)) = (e.rect, e.child_window) else { return };
    let r = shared.compositor.lock().unwrap().bind(e.parent_window, child, e.role.slot, Some(rect), e.z);
    if !matches!(r, Ok((pu, cu)) if pu == e.parent_uid && cu == e.child_uid) {
        eprintln!("limend: embed {embed}: rebinding after PiP failed ({r:?})");
        if let Some(e) = st.embeds.get_mut(&embed) {
            e.bound = false;
        }
    }
}

/// The share sheet's `CHOOSE` (share-sheet.md §3): only the `share`
/// provider, only for an embed it serves, only an app that is a target for
/// the parent's person. The choice is the parent's, for one request.
fn choose(shared: &Arc<Shared>, conn: ConnId, c: proto::Choose) -> Result<Reply, Reply> {
    let (parent, parent_uid) = {
        let st = shared.state.lock().unwrap();
        match st.embeds.get(&c.embed) {
            Some(e) if e.child == conn && e.role.name == "share" && !e.spawned => (e.parent, e.parent_uid),
            _ => return Err(Reply::Refused { reason: refusal::NOT_YOURS }),
        }
    };
    let person = (shared.resolve)(parent_uid).map(|(u, _)| u).unwrap_or_default();
    let ok = crate::manifests::plausible_id(&c.app)
        && c.persona.as_deref().is_none_or(plausible_persona)
        && shared.target_check.lock().unwrap().as_ref().is_none_or(|check| check(&person, &c.app));
    if !ok {
        eprintln!("limend: the share sheet chose {:?}, not a share target for {person}", c.app);
        return Err(Reply::Refused { reason: refusal::NOT_A_TARGET });
    }
    let mut st = shared.state.lock().unwrap();
    // The parent may have gone meanwhile; a choice for nobody is dropped.
    if st.embeds.get(&c.embed).is_some_and(|e| e.parent == parent) {
        st.choices.insert(parent, (c.app, c.persona, Instant::now()));
    }
    Ok(Reply::Ok)
}

/// A persona's name as policy.toml has them ("personal", "work",
/// "side-project"). Whether it is one of the person's, and whether the
/// crossing is on, is portcullisd's to say.
fn plausible_persona(p: &str) -> bool {
    !p.is_empty() && p.len() <= 32 && p.starts_with(|c: char| c.is_ascii_lowercase())
        && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
    if role.name == "mail-sign-in" {
        let Some((owner, _)) = (shared.resolve)(uid) else { return Err(Reply::Refused { reason: refusal::NOT_PROVIDER }); };
        if !shared.mail_sheet_provider.lock().unwrap().as_ref().is_some_and(|f| f(uid)) {
            return Err(Reply::Refused { reason: refusal::NOT_PROVIDER });
        }
        let persona = shared.persona.lock().unwrap().as_ref().and_then(|f| f(uid));
        shared.state.lock().unwrap().mail_sheets.insert((owner, persona), (conn, uid));
    } else {
        shared.state.lock().unwrap().providers.insert(role.name, (conn, uid));
    }
    Ok(Reply::Ok)
}

/// Take `e` (already removed) down: release the slot, tell the child, end
/// a spawned child's jail after the grace period.
fn end(shared: &Arc<Shared>, st: &State, embed: EmbedId, e: Embed, tell_parent: bool) {
    // PiP doesn't outlive the embed: the child's window goes with it.
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
    st.mail_sheets.retain(|_, (c, _)| *c != conn);
    st.choices.remove(&conn);
    let mine: Vec<EmbedId> = st.embeds.iter().filter(|(_, e)| e.parent == conn || e.child == conn).map(|(k, _)| *k).collect();
    for id in mine {
        let e = st.embeds.remove(&id).unwrap();
        if e.child == conn {
            st.to_parent(&e, ParentEvent::ChildLost { embed: id });
        }
        end(shared, &st, id, e, false);
    }
}
