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
    EnvelopeFrontend, Outbound,
};
use fresco_scene_server::window::{DisplayEvent, encode_event};

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Set of per-writer mpsc senders. A producer (event fan-out) holding
/// this can broadcast an `Outbound` to every connected client. Senders
/// disconnect when their writer thread exits; the broadcaster prunes
/// failed sends on each pass.
pub type EventSubs = Arc<Mutex<Vec<Sender<Outbound>>>>;

pub struct Shared {
    pub frontend: Arc<Mutex<EnvelopeFrontend>>,
}

pub fn spawn(shared: Shared, sock_path: &Path) -> io::Result<EventSubs> {
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }
    let listener = UnixListener::bind(sock_path)?;
    eprintln!("frescod: listening on {}", sock_path.display());

    let event_subs: EventSubs = Arc::new(Mutex::new(Vec::new()));
    let subs_for_loop = event_subs.clone();

    std::thread::Builder::new()
        .name("frescod-accept".into())
        .spawn(move || accept_loop(listener, shared, subs_for_loop))
        .map(|_| ())?;

    Ok(event_subs)
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

fn accept_loop(listener: UnixListener, shared: Shared, event_subs: EventSubs) {
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
                std::thread::Builder::new()
                    .name(format!("frescod-writer-{client_id}"))
                    .spawn(move || writer_loop(writer_stream, rx, client_id))
                    .ok();

                /* Reader thread. */
                let frontend = shared.frontend.clone();
                std::thread::Builder::new()
                    .name(format!("frescod-reader-{client_id}"))
                    .spawn(move || {
                        if let Err(e) = reader_loop(stream, frontend.clone(), client_id, tx) {
                            eprintln!("frescod: reader {client_id}: {e}");
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
) -> io::Result<()> {
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

        let outs = match frontend.lock().unwrap().dispatch(&msg, client_id) {
            Ok(v)  => v,
            Err(e) => {
                eprintln!("frescod: client {client_id} op={:#x}: {e:?}", msg.op);
                Vec::new()
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

fn writer_loop(stream: UnixStream, rx: Receiver<Outbound>, client_id: u8) {
    let mut conn = match AqConn::wrap(stream) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("frescod: writer {client_id} wrap: {e}");
            return;
        }
    };
    while let Ok(o) = rx.recv() {
        if let Err(e) = conn.send_message(CLASS_DISPLAY, o.op, o.flags, &o.payload) {
            log::debug!("frescod: writer {client_id} send: {e}");
            break;
        }
    }
}

/// On client disconnect, drop every window the client owned and forget
/// per-window scene state. Without this the renderer would keep
/// pointing at the dead client's last frame indefinitely.
fn cleanup_client(frontend: &Arc<Mutex<EnvelopeFrontend>>, client_id: u8) {
    let mut fe = frontend.lock().unwrap();
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
}
