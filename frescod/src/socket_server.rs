//! Aqueduct/envelope-based connection acceptor for frescod.
//!
//! Each accepted connection gets two threads:
//!
//!   - **reader**: wraps the read half in `aqueduct::Connection`,
//!     pulls envelopes via `recv_message()`, dispatches CLASS_DISPLAY
//!     ops through the shared `EnvelopeFrontend`. Any `Outbound`s
//!     produced (e.g. WINDOW_CREATE's IS_RESPONSE reply) get queued
//!     onto the per-connection writer mpsc.
//!
//!   - **writer**: wraps the write half in its own `aqueduct::Connection`,
//!     drains the mpsc and writes envelopes. The mpsc receives both
//!     reader-produced responses *and* compositor-broadcast async
//!     events.
//!
//! Async events come from `Compositor::set_event_sink(...)`: a single
//! `Sender<DisplayEvent>` is plumbed from `main.rs` into the compositor;
//! a fan-out thread (started by `spawn_event_fanout`) pulls each event,
//! encodes it once, and forwards a clone to every registered writer
//! mpsc. We keep a `Vec<Sender<Outbound>>` behind `EventSubs` for that.
//!
//! Two `aqueduct::Connection`s per UDS connection (one per direction)
//! is intentional: aqueduct's CAS upload/recv state is direction-
//! oriented (incoming uploads on the read side, outgoing publishes on
//! the write side), so splitting on `UnixStream::try_clone()` keeps
//! each direction's state coherent.

use crate::activation;
use aqueduct::{Connection as AqConn, CLASS_DISPLAY};
use aqueduct::envelope::flag;
use fresco_scene_server::command::envelope_frontend::{
    DispatchError, EnvelopeFrontend, Outbound,
};
use fresco_scene_server::window::{Compositor, DisplayEvent, encode_event};

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::laminar::LaneBroker;

/// Set of per-writer mpsc senders. A producer (event fan-out) holding
/// this can broadcast an `Outbound` to every connected client. Senders
/// disconnect when their writer thread exits; the broadcaster prunes
/// failed sends on each pass.
pub type EventSubs = Arc<Mutex<Vec<Sub>>>;

/// One connection's async-event channel.
pub struct Sub {
    pub client_id: u8,
    /// Arrived on the window-management socket (the WM, which is TCB).
    pub wm: bool,
    pub tx: SubTx,
}

/// Where a connection's events go: encoded onto its own socket, or, for a
/// client whose renderer is a separate process (F4b), as events the broker
/// translates into records for that renderer.
pub enum SubTx {
    Wire(Sender<Outbound>),
    Split(Sender<DisplayEvent>),
}

/// Takes over a client connection instead of serving it in-process (the
/// split broker, F4b): the stream, its client id, its peer uid, and the
/// subscriber list. It frees the client id when the client is gone.
pub type HandOff = Arc<dyn Fn(UnixStream, u8, Option<u32>, EventSubs) + Send + Sync>;

#[derive(Clone)]
pub struct Shared {
    pub frontend: Arc<Mutex<EnvelopeFrontend>>,
    /// Deadline broker (Laminar phase J); None when /dev/laminar is
    /// absent or the scheduler is not Laminar.
    pub lane: Option<Arc<LaneBroker>>,
    /// Damage signal (task #25): woken after every client op so the
    /// display loop recomposes only when the scene actually changed.
    pub redraw: crate::redraw::Redraw,
    /// Set in split mode: connections on the client socket (never the WM
    /// socket) go here.
    pub hand_off: Option<HandOff>,
}

/// Bring up frescod's connection acceptor(s).
///
/// `sock_path` is the normal client socket: connections there draw their OWN
/// surfaces (graphics) and are NEVER granted window-management.
///
/// `wm_sock_path`, if given, is the DEDICATED window-management socket. The TCB
/// mounts it into a jail only when that app holds `window-management`
/// (apply_window_management), so REACHING it IS the capability — every connection
/// there is granted window-management by reachability, with no registry/policy
/// read (the forum-control / object-capability model, applied to the WM). Keeping
/// the cross-app ops on a separate socket is what makes reachability == grant:
/// the shared client socket can't distinguish a WM from an ordinary app, this one
/// doesn't have to.
pub fn spawn(shared: Shared, sock_path: &Path, wm_sock_path: Option<&Path>)
    -> io::Result<EventSubs>
{
    let event_subs: EventSubs = Arc::new(Mutex::new(Vec::new()));
    start_listener(sock_path, shared.clone(), event_subs.clone(), false)?;
    if let Some(wm) = wm_sock_path {
        start_listener(wm, shared, event_subs.clone(), true)?;
    }
    Ok(event_subs)
}

fn start_listener(sock_path: &Path, shared: Shared, event_subs: EventSubs, wm_grant: bool)
    -> io::Result<()>
{
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }
    let listener = UnixListener::bind(sock_path)?;
    /* 0666 so the (non-root, per-app uid) apps that reach this socket can connect
     * — same pattern as portcullisd's control socket. Reachability IS the
     * capability gate: only an app holding `graphics` gets this socket's
     * per-service dir nullfs-mounted into its jail (apply_socket/apply_graphics),
     * so an app without the cap can't see the path at all; the mount, not the
     * file mode, is the boundary. Every app has its own dynamic uid and they all
     * share one frescod, so the socket must be multi-uid-connectable. Without
     * this a jailed app at uid 50000 gets EACCES on a root-owned 0755 socket. */
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o666));
    }
    eprintln!("frescod: listening on {} (window-management={wm_grant})", sock_path.display());

    std::thread::Builder::new()
        .name(if wm_grant { "frescod-accept-wm".into() } else { "frescod-accept".into() })
        .spawn(move || accept_loop(listener, shared, event_subs, wm_grant))
        .map(|_| ())?;
    Ok(())
}

/// Spawn the event fan-out thread. Receives `DisplayEvent` from the
/// compositor's sink, encodes once into an envelope-shaped `Outbound`,
/// and forwards to every writer mpsc. `Compositor::set_event_sink`
/// expects the matching `Sender<DisplayEvent>` — caller hands one to
/// the compositor and the other half to this function.
pub fn spawn_event_fanout(
    rx: Receiver<DisplayEvent>,
    subs: EventSubs,
    redraw: crate::redraw::Redraw,
    comp: Arc<Mutex<Compositor>>,
) {
    std::thread::Builder::new()
        .name("frescod-event-fanout".into())
        .spawn(move || {
            for mut ev in rx.iter() {
                /* Task #25: input + compositor events all funnel through
                 * here — wake the display loop so cursor/focus/window
                 * changes recompose. */
                redraw.wake();
                if popup_gate(&ev, &comp) {
                    continue;
                }
                /* A real press gets its activation token (tabula.md §2),
                 * bound to the window's app; only that app sees it, the
                 * WM gets the event without it. */
                let bare = activation::has_token(&ev).then(|| {
                    let uid = window_uid(&ev, &comp);
                    activation::mint_for(&mut ev, uid);
                    activation::without_token(&ev)
                });
                /* Drags follow the input (tabula.md §4; frescod only). */
                let extra = OBSERVE.get().map(|f| f(&ev, &comp)).unwrap_or_default();
                fan_out(&subs, &comp, ev, bare);
                for e in extra {
                    fan_out(&subs, &comp, e, None);
                }
            }
        })
        .expect("spawn event fan-out");
}

/// Buttons whose press dismissed pop-ups: their releases are not
/// delivered either.
static SWALLOWED: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Pop-ups and presses (fresco-popups.md §1): a press outside an open
/// grabbing pop-up's chain dismisses the chain and is not delivered, nor
/// its release. Every press is noted, so the pressed window may open a
/// pop-up even when it does not take focus (the bar). `true`: drop `ev`.
fn popup_gate(ev: &DisplayEvent, comp: &Arc<Mutex<Compositor>>) -> bool {
    let DisplayEvent::InputPointerButton { window_id, button, pressed, .. } = *ev else { return false };
    let mut swallowed = SWALLOWED.lock().unwrap();
    if !pressed {
        return match swallowed.iter().position(|&b| b == button) {
            Some(i) => {
                swallowed.remove(i);
                true
            }
            None => false,
        };
    }
    let Ok(target) = u16::try_from(window_id) else { return false };
    let mut c = comp.lock().unwrap();
    c.last_press = Some(c.popup_root(target));
    if c.press_dismisses_popups(target) {
        if !swallowed.contains(&button) {
            swallowed.push(button);
        }
        return true;
    }
    false
}

/// Send `ev` to every subscriber its route reaches; `bare` (the event
/// without its token) to those other than the window's owner.
fn fan_out(subs: &EventSubs, comp: &Arc<Mutex<Compositor>>, ev: DisplayEvent, bare: Option<DisplayEvent>) {
    let to = route(&ev, comp);
    let wire = |ev: &DisplayEvent| match encode_event(ev) {
        Ok((op, payload)) => Some(Outbound { op, flags: flag::ASYNC_EVENT, payload, fds: Vec::new() }),
        Err(e) => {
            log::debug!("encode_event failed: {e}");
            None
        }
    };
    let Some(out) = wire(&ev) else { return };
    let out_bare = match &bare {
        Some(b) => match wire(b) {
            Some(o) => o,
            None => return,
        },
        None => out_clone(&out),
    };
    let owner = to.owner();
    let mut s = subs.lock().unwrap();
    if let DisplayEvent::AppearanceChanged(a) = &ev {
        /* Under the subscriber lock: a split renderer that subscribes
         * takes the value and the later changes in one order. */
        *APPEARANCE.lock().unwrap() = *a;
    }
    s.retain(|sub| {
        if !to.reaches(sub) {
            return true;
        }
        let mine = owner == Some(sub.client_id);
        match &sub.tx {
            SubTx::Wire(tx) => tx.send(out_clone(if mine { &out } else { &out_bare })).is_ok(),
            SubTx::Split(tx) => tx.send(match (&bare, mine) {
                (Some(b), false) => b.clone(),
                _ => ev.clone(),
            }).is_ok(),
        }
    });
}

/// The system appearance as last told to clients, for a split renderer
/// that starts after the change. Written under the subscriber lock.
pub static APPEARANCE: Mutex<fresco_protocol::AppearancePayload> = Mutex::new(fresco_protocol::AppearancePayload { scheme: 0, high_contrast: false, accent: 0 });

/// Set by frescod (not the renderer, which shares this file): turns each
/// event into the drag events it causes.
pub static OBSERVE: std::sync::OnceLock<fn(&DisplayEvent, &Arc<Mutex<Compositor>>) -> Vec<DisplayEvent>> = std::sync::OnceLock::new();

/// What `OP_DRAG_BEGIN` from a client does: frescod checks and starts the
/// drag, a renderer passes it to frescod. `false`: refused.
pub type DragBeginFn = Box<dyn Fn(u8, &fresco_protocol::DragBegin) -> bool + Send + Sync>;
pub static DRAG_BEGIN: std::sync::OnceLock<DragBeginFn> = std::sync::OnceLock::new();

/// What `OP_PAD_CLAIM` from a client does: frescod decides it (answering
/// with `EV_GAMEPAD_CLAIM`), a renderer passes it to frescod.
pub type PadClaimFn = Box<dyn Fn(u8, &fresco_protocol::PadClaim) + Send + Sync>;
pub static PAD_CLAIM: std::sync::OnceLock<PadClaimFn> = std::sync::OnceLock::new();

/// Run after `OP_POPUP_GRANT` is dispatched and before its answer goes
/// out: a renderer sends the grant on to frescod now, so it is there
/// before the token can reach the app that claims with it.
pub static POPUP_GRANTED: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// The WM's override of exclusive claims (fresco-capabilities.md §4), set
/// by frescod where the pads are: list them (`OP_WM_PADS`), and take one
/// back (`OP_WM_PAD_RELEASE`). Unset (a renderer, which has no WM socket):
/// no pads.
pub static WM_PADS: std::sync::OnceLock<fn() -> Vec<fresco_protocol::WmPad>> = std::sync::OnceLock::new();
pub static WM_PAD_RELEASE: std::sync::OnceLock<fn(u32)> = std::sync::OnceLock::new();

/// Who an event goes to (threat model §2.5.2 "input splits the same way").
/// Every event names a window. It reaches that window's owner and the WM
/// (TCB) only: a client never sees another client's keystrokes, pointer
/// or window events. Window 0, the background under every window, belongs
/// to the WM: keys with no focused window and pointer events over no
/// window go to the WM alone (F4c; the desktop shell draws in a window of
/// its own, role Background).
enum Route {
    OwnerAndWm(Option<u8>),
    /// Only the window's owner: what happened to its own drawing.
    Owner(Option<u8>),
    WmOnly,
    /// Everyone: a change to the screen itself (EV_DISPLAY_CHANGED).
    All,
}

impl Route {
    fn owner(&self) -> Option<u8> {
        match self {
            Route::OwnerAndWm(o) | Route::Owner(o) => *o,
            _ => None,
        }
    }

    fn reaches(&self, sub: &Sub) -> bool {
        match self {
            Route::WmOnly => sub.wm,
            Route::All => true,
            Route::OwnerAndWm(owner) => sub.wm || *owner == Some(sub.client_id),
            Route::Owner(owner) => *owner == Some(sub.client_id),
        }
    }
}

fn route(ev: &DisplayEvent, comp: &Arc<Mutex<Compositor>>) -> Route {
    use DisplayEvent as E;
    let owner_of = |window_id: u32| u16::try_from(window_id).ok()
        .and_then(|id| comp.lock().unwrap().windows.get(&id).map(|w| w.owner))
        .and_then(|o| u8::try_from(o).ok());
    let window_id = match *ev {
        E::WindowResized { window_id, .. }
        | E::WindowFocusChanged { window_id, .. }
        | E::WindowCloseRequested { window_id }
        | E::WindowDpiChanged { window_id, .. }
        | E::WindowCreated { window_id }
        | E::WindowDestroyed { window_id }
        | E::InputKey { window_id, .. }
        | E::InputPointerMotion { window_id, .. }
        | E::InputPointerButton { window_id, .. }
        | E::InputPointerScroll { window_id, .. }
        | E::InputPointerMagnify { window_id, .. }
        | E::InputSwipe { window_id, .. } => window_id,
        E::DisplayChanged { .. } | E::AppearanceChanged(_) => return Route::All,
        E::WindowFillsDropped { window_id, .. }
        | E::Drag { window_id, .. }
        | E::Drop { window_id, .. }
        | E::DragEnd { window_id, .. }
        | E::EmbedToken { window_id, .. }
        | E::VideoReleased { window_id, .. }
        | E::VideoPresented { window_id, .. }
        | E::WindowSpan { window_id, .. }
        | E::PopupPlaced { window_id, .. }
        | E::PopupDismissed { window_id, .. }
        | E::PopupHosted { window_id, .. }
        // A gamepad is the focused window's alone (not the WM's too).
        | E::GamepadConnect { window_id, .. }
        | E::Gamepad { window_id, .. } => return Route::Owner(owner_of(window_id)),
    };
    /* While locked (lock-screen.md §3) input goes to the window's owner
     * alone (the lock screen, a sheet), never to the WM, whose session is
     * the one locked; input over the background goes nowhere. */
    let input = matches!(ev, E::InputKey { .. } | E::InputPointerMotion { .. } | E::InputPointerButton { .. }
        | E::InputPointerScroll { .. } | E::InputPointerMagnify { .. } | E::InputSwipe { .. });
    if input && crate::lock::locked() {
        return if window_id == 0 { Route::Owner(None) } else { Route::Owner(owner_of(window_id)) };
    }
    if window_id == 0 {
        return Route::WmOnly;
    }
    Route::OwnerAndWm(owner_of(window_id))
}

/// The uid of the app whose window a press went to: the window's owner
/// uid, as the frontend or the split broker recorded it.
fn window_uid(ev: &DisplayEvent, comp: &Arc<Mutex<Compositor>>) -> Option<u32> {
    use DisplayEvent as E;
    let (E::InputKey { window_id, .. } | E::InputPointerButton { window_id, .. }) = *ev else { return None };
    window_owner_uid(window_id, comp)
}

/// The uid of the app that owns `window_id`.
/// The peer uid a client id was given to, if known (no compositor lock).
pub fn client_uid(owner: u32) -> Option<u32> {
    u8::try_from(owner).ok().and_then(|o| CLIENT_UID.lock().unwrap()[o as usize])
}

pub fn window_owner_uid(window_id: u32, comp: &Mutex<Compositor>) -> Option<u32> {
    let id = u16::try_from(window_id).ok()?;
    let c = comp.lock().unwrap();
    let w = c.windows.get(&id)?;
    let known = u8::try_from(w.owner).ok()
        .and_then(|o| CLIENT_UID.lock().unwrap()[o as usize]);
    Some(known.unwrap_or(w.owner_uid))
}

fn out_clone(o: &Outbound) -> Outbound {
    Outbound { op: o.op, flags: o.flags, payload: o.payload.clone(), fds: o.fds.clone() }
}

/// Build the `IS_ERROR` response for a dispatch failure on op `op`.
fn error_reply(op: u16, e: &DispatchError) -> Outbound {
    use fresco_protocol::error_code as ec;
    let code = match e {
        DispatchError::Forbidden     => ec::FORBIDDEN,
        DispatchError::BadPayload    => ec::BAD_PAYLOAD,
        DispatchError::UnknownWindow => ec::UNKNOWN_WINDOW,
        _                            => ec::GENERIC,
    };
    let payload = fresco_protocol::encode(&fresco_protocol::ErrorReply {
        code,
        message: format!("{e:?}"),
    }).unwrap_or_default();
    Outbound { op, flags: flag::IS_RESPONSE | flag::IS_ERROR, payload, fds: Vec::new() }
}

/// Client ids in use. Ids are a `u8` on the wire and in the window table,
/// so a new connection takes a free one; a wrapping counter would hand a
/// new client the id (and so the windows) of a live one.
static LIVE_CLIENTS: Mutex<[bool; 256]> = Mutex::new([false; 256]);
static NEXT_CLIENT_ID: AtomicU8 = AtomicU8::new(1);

/// The peer uid each live client id was given to (`None`: not known, or a
/// connection frescod opened itself for the chrome channel).
static CLIENT_UID: Mutex<[Option<u32>; 256]> = Mutex::new([None; 256]);

/// Most connections one peer uid may hold on a Fresco socket. Every app runs
/// under its own uid (portcullis.md §9.0), so this is a per-app limit: one
/// app holding connections cannot take every client id from the rest (F3).
pub const MAX_CLIENTS_PER_UID: usize = 32;

/// The last client ids are a reserve: once only this many are free, a uid
/// may take one only while it holds fewer than [`MAX_CLIENTS_IN_RESERVE`].
/// Without it eight colluding apps at 32 each take all 255 ids; with it they
/// stop at 191, and the rest go two at a time to apps that hold few, so a
/// newly started app still connects unless 32 more apps collude.
pub const RESERVE_IDS: usize = 64;
pub const MAX_CLIENTS_IN_RESERVE: usize = 2;

/// Whether a uid holding `held` client ids may take another while `free`
/// ids are free.
fn may_take(held: usize, free: usize) -> bool {
    if free == 0 || held >= MAX_CLIENTS_PER_UID {
        return false;
    }
    free > RESERVE_IDS || held < MAX_CLIENTS_IN_RESERVE
}

pub fn alloc_client_id() -> Option<u8> {
    alloc(None)
}

/// A client id for a connection from `uid`, unless that uid already holds
/// its share ([`may_take`]).
pub fn alloc_client_id_for(uid: Option<u32>) -> Result<u8, &'static str> {
    if let Some(u) = uid {
        let free = LIVE_CLIENTS.lock().unwrap()[1..].iter().filter(|&&l| !l).count();
        let held = CLIENT_UID.lock().unwrap().iter().filter(|&&x| x == Some(u)).count();
        if !may_take(held, free) {
            return Err(if held >= MAX_CLIENTS_PER_UID || free > RESERVE_IDS {
                "this uid holds too many connections"
            } else {
                "client ids are scarce and this uid already holds its share"
            });
        }
    }
    alloc(uid).ok_or("255 clients connected")
}

#[cfg(test)]
mod id_share_tests {
    use super::*;

    /// Apps (uids) take ids greedily, in turn; returns what each got.
    fn run(apps: usize) -> Vec<usize> {
        let mut held = vec![0usize; apps];
        let mut free = 255usize;
        loop {
            let mut took = false;
            for h in held.iter_mut() {
                if may_take(*h, free) {
                    *h += 1;
                    free -= 1;
                    took = true;
                }
            }
            if !took {
                return held;
            }
        }
    }

    #[test]
    fn eight_colluding_apps_leave_the_reserve() {
        let held = run(8);
        assert_eq!(held.iter().sum::<usize>(), 255 - RESERVE_IDS);
        // A ninth app still gets in.
        assert!(may_take(0, RESERVE_IDS));
    }

    #[test]
    fn the_reserve_goes_two_at_a_time() {
        assert!(may_take(1, 10));
        assert!(!may_take(2, 10));
        assert!(may_take(31, RESERVE_IDS + 1));
        assert!(!may_take(32, 200));
        assert!(!may_take(0, 0));
    }
}

fn alloc(uid: Option<u32>) -> Option<u8> {
    let mut live = LIVE_CLIENTS.lock().unwrap();
    for _ in 0..255 {
        let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed).max(1);
        if !live[id as usize] {
            live[id as usize] = true;
            CLIENT_UID.lock().unwrap()[id as usize] = uid;
            return Some(id);
        }
    }
    None
}

/// The live clients: id, peer uid, and whether embed-only.
#[allow(dead_code)]
pub fn live_clients() -> Vec<(u8, Option<u32>, bool)> {
    let live = *LIVE_CLIENTS.lock().unwrap();
    let uids = *CLIENT_UID.lock().unwrap();
    let embed = *EMBED_ONLY.lock().unwrap();
    (1..=255u8).filter(|&i| live[i as usize]).map(|i| (i, uids[i as usize], embed[i as usize])).collect()
}

pub fn free_client_id(id: u8) {
    EMBED_ONLY.lock().unwrap()[id as usize] = false;
    crate::caps::forget(id);
    let mut live = LIVE_CLIENTS.lock().unwrap();
    live[id as usize] = false;
    CLIENT_UID.lock().unwrap()[id as usize] = None;
}

fn accept_loop(listener: UnixListener, shared: Shared, event_subs: EventSubs, wm_grant: bool) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let uid = peer_xucred(&stream).map(|(_, uid)| uid);
                serve_client(stream, uid, wm_grant, false, &shared, &event_subs);
            }
            Err(e) => {
                eprintln!("frescod: accept: {e}");
                break;
            }
        }
    }
}

/// Serves a connection that arrived some other way: an embed-only one
/// (fresco-video.md §8, S1), handed over by the activation socket.
pub type EmbedClientFn = Box<dyn Fn(UnixStream, u32) + Send + Sync>;
pub static EMBED_CLIENT: std::sync::OnceLock<EmbedClientFn> = std::sync::OnceLock::new();

static EMBED_ONLY: Mutex<[bool; 256]> = Mutex::new([false; 256]);

/// Whether client `id`'s windows must stay embeds (S1).
pub fn is_embed_only(id: u8) -> bool {
    EMBED_ONLY.lock().unwrap()[id as usize]
}

/// Serve the client socket's connections and embed-only ones alike.
pub fn install_embed_client(shared: Shared, event_subs: EventSubs) {
    let _ = EMBED_CLIENT.set(Box::new(move |stream, uid| {
        serve_client(stream, Some(uid), false, true, &shared, &event_subs);
    }));
}

/// Serve one client connection, owned by `uid`: in process, or handed to
/// a renderer (split mode). `embed_only`: see [`is_embed_only`].
fn serve_client(stream: UnixStream, uid: Option<u32>, wm_grant: bool, embed_only: bool, shared: &Shared, event_subs: &EventSubs) {
    let client_id = match alloc_client_id_for(uid) {
        Ok(id) => id,
        Err(why) => {
            eprintln!("frescod: refusing a connection from uid {uid:?}: {why}");
            return;
        }
    };
    EMBED_ONLY.lock().unwrap()[client_id as usize] = embed_only;
    crate::caps::admit(client_id, uid, embed_only);
    if embed_only {
        shared.frontend.lock().unwrap().mark_embed_only(client_id);
    }
    if let (false, Some(h)) = (wm_grant, &shared.hand_off) {
        h(stream, client_id, uid, event_subs.clone());
        return;
    }
    let (tx, rx) = mpsc::channel::<Outbound>();

    /* Register writer's tx for async-event broadcast. */
    event_subs.lock().unwrap().push(Sub { client_id, wm: wm_grant, tx: SubTx::Wire(tx.clone()) });

    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("frescod: try_clone for client {client_id}: {e}");
            return;
        }
    };

    /* Writer thread. */
    let wlane = shared.lane.clone();
    std::thread::Builder::new()
        .name(format!("frescod-writer-{client_id}"))
        .spawn(move || {
            writer_loop(writer_stream, rx, client_id, wlane);
        })
        .ok();

    /* Reader thread. */
    let frontend = shared.frontend.clone();
    let lane = shared.lane.clone();
    let redraw = shared.redraw.clone();
    std::thread::Builder::new()
        .name(format!("frescod-reader-{client_id}"))
        .spawn(move || {
            if let Some(uid) = uid {
                frontend.lock().unwrap().register_client_uid(client_id, uid);
            }
            if let Err(e) = reader_loop(
                stream, frontend.clone(), client_id, tx,
                lane.clone(), wm_grant, redraw,
            ) {
                eprintln!("frescod: reader {client_id}: {e}");
            }
            if let Some(l) = &lane {
                l.drop_self();
                l.client_gone(client_id);
            }
            cleanup_client(&frontend, client_id);
            free_client_id(client_id);
        })
        .ok();
}

pub fn reader_loop(
    stream: UnixStream,
    frontend: Arc<Mutex<EnvelopeFrontend>>,
    client_id: u8,
    out: Sender<Outbound>,
    lane: Option<Arc<LaneBroker>>,
    wm_grant: bool,
    redraw: crate::redraw::Redraw,
) -> io::Result<()> {
    let peer_pid = peer_cred_pid(&stream);

    /* Record the peer's uid (getpeereid) so surfaces this client creates carry
     * their app's stable kernel identity over WM_ENUMERATE — the WM resolves it
     * to an app-id (launch registry) for workspace assignment. Identity only,
     * not a capability decision (window-management stays reachability-gated), so
     * frescod needs no registry/portcullis-peer dependency here. */
    if let Some((_, uid)) = peer_xucred(&stream) {
        // An embed-only connection's peer is limend; its owner was named
        // when it was handed over (S1) and is already registered.
        let mut fe = frontend.lock().unwrap();
        if !fe.is_embed_only(client_id) {
            fe.register_client_uid(client_id, uid);
        }
    }

    /* Capability admission for the cross-app window-management ops = REACHABILITY.
     * The connection arrived on the DEDICATED window-management socket, which the
     * TCB mounts into a jail only when that app holds `window-management`
     * (apply_window_management). So reaching this socket IS the grant — possession
     * of the channel, not a registry/policy lookup (the object-capability model;
     * same as forum-control over forum-ctl). The normal client socket
     * (wm_grant=false) never grants it. See docs/spec/portcullis.md §9.0. */
    if wm_grant {
        frontend.lock().unwrap().grant_window_management(client_id);
        log::info!("frescod: granted window-management to client {client_id} (wm socket — reachability)");
    }

    let mut conn = AqConn::wrap(stream)?;
    loop {
        let msg = match conn.recv_message() {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        if msg.opcode_class != CLASS_DISPLAY {
            log::debug!("client {client_id}: ignored class={} op={:#x}",
                msg.opcode_class, msg.op);
            continue;
        }

        /* SLOT_SET references a CAS hash that was uploaded just before
         * over CLASS_CORE; aqueduct auto-handled the UPLOAD_BEGIN/DATA/
         * FINISH frames into its per-connection cache. The scene-server
         * CasStore is separate; pull bytes across now so the renderer
         * can resolve the slot binding when it processes uploads. */
        if msg.op == fresco_protocol::control::OP_SLOT_SET {
            if let Ok(p) = fresco_protocol::decode::<fresco_protocol::SlotSetPayload>(&msg.payload) {
                if let Some(bytes) = conn.cache_get(&p.hash) {
                    frontend.lock().unwrap().ingest_blob(&bytes);
                } else {
                    log::warn!("client {client_id}: slot_set hash not in \
                                connection cache; upload dropped");
                }
            }
        }

        /* A drag out of one of the client's windows (tabula.md §4). No
         * reply; a refusal ends the drag at once. */
        if msg.op == fresco_protocol::control::OP_DRAG_BEGIN {
            let b = fresco_protocol::decode::<fresco_protocol::DragBegin>(&msg.payload);
            let ok = match &b {
                Ok(b) => DRAG_BEGIN.get().is_some_and(|f| f(client_id, b)),
                Err(_) => false,
            };
            if let (false, Ok(b)) = (ok, b) {
                let end = fresco_protocol::DragEndEvent { window_id: b.window_id, drag_id: b.drag_id, dropped: false };
                let _ = out.send(Outbound {
                    op: fresco_protocol::control::EV_DRAG_END,
                    flags: flag::ASYNC_EVENT,
                    payload: fresco_protocol::encode(&end).unwrap_or_default(), fds: Vec::new(),
                });
            }
            continue;
        }

        /* An exclusive gamepad claim (fresco-capabilities.md §4). No reply;
         * the answer is EV_GAMEPAD_CLAIM. */
        if msg.op == fresco_protocol::control::OP_PAD_CLAIM {
            if let (Ok(c), Some(f)) = (fresco_protocol::decode::<fresco_protocol::PadClaim>(&msg.payload), PAD_CLAIM.get()) {
                f(client_id, &c);
            }
            continue;
        }

        /* The user's override of exclusive pad claims, from the WM only
         * (reaching the WM socket is the grant, as for every OP_WM_*). */
        if wm_grant && msg.op == fresco_protocol::control::OP_WM_PADS {
            let pads = WM_PADS.get().map(|f| f()).unwrap_or_default();
            let payload = fresco_protocol::encode(&fresco_protocol::WmPadsReply { pads }).unwrap_or_default();
            let _ = out.send(Outbound { op: msg.op, flags: flag::IS_RESPONSE, payload, fds: Vec::new() });
            continue;
        }
        if wm_grant && msg.op == fresco_protocol::control::OP_WM_PAD_RELEASE {
            let reply = match fresco_protocol::decode::<fresco_protocol::WmPadRelease>(&msg.payload) {
                Ok(r) => {
                    if let Some(f) = WM_PAD_RELEASE.get() {
                        f(r.device_id);
                    }
                    Outbound { op: msg.op, flags: flag::IS_RESPONSE, payload: Vec::new(), fds: Vec::new() }
                }
                Err(_) => error_reply(msg.op, &DispatchError::BadPayload),
            };
            let _ = out.send(reply);
            continue;
        }

        /* Deadline-lane sponsorship (phase J): broker concern, not a
         * scene op — handled here, never reaches the frontend. */
        if msg.op == fresco_protocol::control::OP_LANE_REQUEST {
            let reply = handle_lane_request(
                lane.as_deref(), &msg.payload, client_id, peer_pid,
            );
            /*
             * K-b deadline lending: this thread is the client's READER —
             * from here on, its request handling runs at band priority
             * on the client's CBS budget (charged back; a heavy client
             * throttles itself, not frescod).
             */
            if reply.ok {
                if let (Some(l), Some((pid, tid))) = (
                    lane.as_deref(),
                    lane.as_deref()
                        .and_then(|l| l.sponsored_for(client_id)),
                ) {
                    match l.adopt_self(pid, tid) {
                        Ok(()) => eprintln!(
                            "frescod: lane: reader {client_id} adopted                              client entity (pid {pid} tid {tid})"
                        ),
                        Err(e) => eprintln!(
                            "frescod: lane: reader adopt failed: {e}"
                        ),
                    }
                }
            }
            let payload = fresco_protocol::encode(&reply)
                .unwrap_or_default();
            let _ = out.send(Outbound {
                op: fresco_protocol::control::OP_LANE_REQUEST,
                flags: flag::IS_RESPONSE,
                payload, fds: Vec::new(),
            });
            continue;
        }

        let outs = match frontend.lock().unwrap().dispatch(&msg, client_id) {
            Ok(v)  => v,
            Err(e) => {
                eprintln!("frescod: client {client_id} op={:#x}: {e:?}", msg.op);
                // Reply with an error so a client awaiting a response on this op
                // fails fast with a reason instead of blocking forever. Harmless
                // for fire-and-forget ops (the client buffers unmatched responses).
                vec![error_reply(msg.op, &e)]
            }
        };
        /* Task #25: wake the display loop only when the op can have
         * mutated scene/window state. Read-only queries MUST NOT count
         * as damage: chrome polls them steadily (forum-bar asks the WM
         * for ListSurfaces → WM_ENUMERATE every second), and waking on
         * queries kept an "idle" desktop recomposing forever — under a
         * slow software renderer that treadmill ate every core and
         * starved sshd. Unknown ops still wake (over-signalling is
         * harmless; under-signalling is a stuck frame). */
        if op_can_mutate(msg.op) {
            redraw.wake();
        }
        if msg.op == fresco_protocol::control::OP_POPUP_GRANT {
            if let Some(f) = POPUP_GRANTED.get() {
                f();
            }
        }
        for o in outs {
            if out.send(o).is_err() {
                /* Writer exited; bail. */
                return Ok(());
            }
        }
    }
}

pub fn writer_loop(
    stream: UnixStream,
    rx: Receiver<Outbound>,
    client_id: u8,
    lane: Option<Arc<LaneBroker>>,
) {
    let mut conn = match AqConn::wrap(stream) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("frescod: writer {client_id} wrap: {e}");
            return;
        }
    };
    /* K-b: once this client is sponsored, deliver its events (frame
     * callbacks, input) on its reservation — adopt lazily. */
    let mut adopted = false;
    while let Ok(o) = rx.recv() {
        if !adopted {
            if let Some(l) = &lane {
                if let Some((pid, tid)) = l.sponsored_for(client_id) {
                    adopted = l.adopt_self(pid, tid).is_ok();
                }
            }
        }
        use std::os::fd::AsRawFd;
        let fds: Vec<i32> = o.fds.iter().map(|f| f.as_raw_fd()).collect();
        if let Err(e) = conn.send_message_with_fds(CLASS_DISPLAY, o.op, o.flags, &o.payload, &fds) {
            log::debug!("frescod: writer {client_id} send: {e}");
            break;
        }
    }
    if adopted {
        if let Some(l) = &lane {
            l.drop_self();
        }
    }
}

/// On client disconnect, drop every window the client owned and forget
/// per-window scene state. Without this the renderer would keep
/// pointing at the dead client's last frame indefinitely.
pub fn cleanup_client(frontend: &Arc<Mutex<EnvelopeFrontend>>, client_id: u8) {
    let mut fe = frontend.lock().unwrap();
    fe.revoke_client_caps(client_id);
    let comp_arc = fe.compositor_arc().clone();
    let owned: Vec<u32> = {
        let comp = comp_arc.lock().unwrap();
        comp.windows.iter()
            .filter(|(_, w)| w.owner as u8 == client_id)
            .map(|(&id, _)| id as u32)
            .collect()
    };
    for id in owned {
        {
            let mut comp = comp_arc.lock().unwrap();
            let _ = comp.destroy_with_focus_shift(id as u16, client_id as u32);
        }
        fe.forget_window(id);
    }
    /* Window 0 (the shared screen window) survives client disconnect,
     * but the disconnecting client's nodes + slot bindings on it must
     * be purged or they leak as ghost content under the next client. */
    fe.forget_client_writes(client_id);
}

/// Peer pid of a UDS connection via LOCAL_PEERCRED (struct xucred).
/// Defined by hand: libc's xucred lags the cr_pid union member.
fn peer_cred_pid(stream: &UnixStream) -> Option<i32> {
    peer_xucred(stream).map(|(pid, _uid)| pid)
}

pub fn peer_xucred(stream: &UnixStream) -> Option<(i32, u32)> {
    use std::os::fd::AsRawFd;

    const XU_NGROUPS: usize = 16;
    #[repr(C)]
    struct Xucred {
        cr_version: u32,
        cr_uid: u32,
        cr_ngroups: i16,
        /* 2 bytes implicit padding before cr_groups */
        cr_groups: [u32; XU_NGROUPS],
        /* the trailing union { void *; pid_t } is 8-aligned: groups
         * end at offset 76, the union starts at 80 */
        _pad0: u32,
        cr_pid: i32, /* low word of the union (little-endian) */
        _pad1: i32,
    }
    let mut xu: Xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<Xucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            0, /* SOL_LOCAL */
            1, /* LOCAL_PEERCRED */
            &mut xu as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 {
        Some((xu.cr_pid, xu.cr_uid))
    } else {
        None
    }
}

fn handle_lane_request(
    lane: Option<&LaneBroker>,
    payload: &[u8],
    client_id: u8,
    peer_pid: Option<i32>,
) -> fresco_protocol::LaneReplyPayload {
    use fresco_protocol::{LaneReplyPayload, LaneRequestPayload};

    let fail = |err: &str, t_us: u64| LaneReplyPayload {
        ok: false,
        err: err.to_string(),
        t_us,
    };
    let Some(broker) = lane else {
        return fail("no deadline lane on this system", 0);
    };
    let req: LaneRequestPayload = match fresco_protocol::decode(payload) {
        Ok(r) => r,
        Err(_) => return fail("bad payload", broker.t_us()),
    };
    /* The pid must be the connection's own peer — a client may only
     * ask for ITS threads, never another process's. */
    match peer_pid {
        Some(p) if p == req.pid => {}
        Some(p) => {
            eprintln!(
                "frescod: lane: client {client_id} pid claim {} != peer {p}",
                req.pid
            );
            return fail("pid does not match peer credential", broker.t_us());
        }
        None => return fail("no peer credential", broker.t_us()),
    }
    match broker.sponsor(client_id, req.pid, req.tid, req.q_us) {
        Ok(()) => {
            eprintln!(
                "frescod: lane: sponsored client {client_id} pid {} tid {} q {}us T {}us",
                req.pid, req.tid, req.q_us, broker.t_us()
            );
            LaneReplyPayload { ok: true, err: String::new(), t_us: broker.t_us() }
        }
        Err(e) => fail(&format!("sponsor: {e}"), broker.t_us()),
    }
}

/// Whether an op can change composited output. Pure queries return
/// data and touch nothing the renderer reads — they must not count as
/// damage. Anything not listed here is assumed mutating (the safe
/// direction: a spurious render beats a stuck frame).
fn op_can_mutate(op: u16) -> bool {
    use fresco_protocol::control::*;
    !matches!(
        op,
        OP_FONT_OPEN | OP_FONT_CLOSE | OP_TEXT_MEASURE | OP_DISPLAY_INFO | OP_WM_ENUMERATE
    )
}
