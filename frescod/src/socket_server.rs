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

use aqueduct::{Connection as AqConn, CLASS_DISPLAY};
use aqueduct::envelope::flag;
use fresco_scene_server::command::envelope_frontend::{
    DispatchError, EnvelopeFrontend, Outbound,
};
use fresco_scene_server::window::{DisplayEvent, encode_event};

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
pub type EventSubs = Arc<Mutex<Vec<Sender<Outbound>>>>;

#[derive(Clone)]
pub struct Shared {
    pub frontend: Arc<Mutex<EnvelopeFrontend>>,
    /// Deadline broker (Laminar phase J); None when /dev/laminar is
    /// absent or the scheduler is not Laminar.
    pub lane: Option<Arc<LaneBroker>>,
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
) {
    std::thread::Builder::new()
        .name("frescod-event-fanout".into())
        .spawn(move || {
            for ev in rx.iter() {
                let (op, payload) = match encode_event(&ev) {
                    Ok(p) => p,
                    Err(e) => {
                        log::debug!("encode_event failed: {e}");
                        continue;
                    }
                };
                let out = Outbound {
                    op,
                    flags:   flag::ASYNC_EVENT,
                    payload,
                };
                let mut s = subs.lock().unwrap();
                s.retain(|tx| tx.send(out_clone(&out)).is_ok());
            }
        })
        .expect("spawn event fan-out");
}

fn out_clone(o: &Outbound) -> Outbound {
    Outbound { op: o.op, flags: o.flags, payload: o.payload.clone() }
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
    Outbound { op, flags: flag::IS_RESPONSE | flag::IS_ERROR, payload }
}

fn accept_loop(listener: UnixListener, shared: Shared, event_subs: EventSubs, wm_grant: bool) {
    static NEXT_CLIENT_ID: AtomicU8 = AtomicU8::new(1);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed).max(1);
                let (tx, rx) = mpsc::channel::<Outbound>();

                /* Register writer's tx for async-event broadcast. */
                event_subs.lock().unwrap().push(tx.clone());

                let writer_stream = match stream.try_clone() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("frescod: try_clone for client {client_id}: {e}");
                        continue;
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
                std::thread::Builder::new()
                    .name(format!("frescod-reader-{client_id}"))
                    .spawn(move || {
                        if let Err(e) = reader_loop(
                            stream, frontend.clone(), client_id, tx,
                            lane.clone(), wm_grant,
                        ) {
                            eprintln!("frescod: reader {client_id}: {e}");
                        }
                        if let Some(l) = &lane {
                            l.drop_self();
                            l.client_gone(client_id);
                        }
                        cleanup_client(&frontend, client_id);
                    })
                    .ok();
            }
            Err(e) => {
                eprintln!("frescod: accept: {e}");
                break;
            }
        }
    }
}

fn reader_loop(
    stream: UnixStream,
    frontend: Arc<Mutex<EnvelopeFrontend>>,
    client_id: u8,
    out: Sender<Outbound>,
    lane: Option<Arc<LaneBroker>>,
    wm_grant: bool,
) -> io::Result<()> {
    let peer_pid = peer_cred_pid(&stream);

    /* Record the peer's uid (getpeereid) so surfaces this client creates carry
     * their app's stable kernel identity over WM_ENUMERATE — the WM resolves it
     * to an app-id (launch registry) for workspace assignment. Identity only,
     * not a capability decision (window-management stays reachability-gated), so
     * frescod needs no registry/portcullis-peer dependency here. */
    if let Some((_, uid)) = peer_xucred(&stream) {
        frontend.lock().unwrap().register_client_uid(client_id, uid);
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
                payload,
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
        for o in outs {
            if out.send(o).is_err() {
                /* Writer exited; bail. */
                return Ok(());
            }
        }
    }
}

fn writer_loop(
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
        if let Err(e) = conn.send_message(CLASS_DISPLAY, o.op, o.flags, &o.payload) {
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
fn cleanup_client(frontend: &Arc<Mutex<EnvelopeFrontend>>, client_id: u8) {
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

fn peer_xucred(stream: &UnixStream) -> Option<(i32, u32)> {
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
