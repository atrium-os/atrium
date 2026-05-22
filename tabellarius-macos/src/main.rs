//! `tabellarius-macos` — push-delivery daemon for Insula
//! apps on macOS.
//!
//! Listens on a unix socket, serves CLASS_TABELLARIUS
//! ops (see `aqueduct/src/classes.rs` and
//! `docs/spec/tabellarius.md` §9.1).
//!
//! v0 scope:
//! - SUBSCRIBE  — mint a per-purpose keypair, persist it,
//!                return key_id + pubkey.
//! - UNSUBSCRIBE — remove a subscription by key_id.
//! - LIST        — enumerate active subscriptions.
//!
//! Out of scope for v0: actual relay connection,
//! wake-on-push delivery to triggered-bg apps,
//! per-app rate limiting. These are Phase B work (see
//! `docs/spec/tabellarius.md` §11.2).
//!
//! # Configuration
//!
//! - `INSULA_TABELLARIUSD_SOCKET` — listen path. Default:
//!   `$XDG_RUNTIME_DIR/tabellarius-macos.sock` or
//!   `$TMPDIR/tabellarius-macos.sock` or
//!   `/tmp/tabellarius-macos.sock`.
//! - `INSULA_TABELLARIUSD_STORE` — substore dir. Default:
//!   under `$XDG_RUNTIME_DIR/tabellarius-macos/subs/`.

mod relay_client;
mod substore;

use aqueduct::classes::CLASS_TABELLARIUS;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use relay_client::{resolve_relay_addr, resolve_wake_cmd, PushQueue, RelayClient};
use std::collections::VecDeque;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use substore::{resolve_substore_path, SubStore};

const OP_SUBSCRIBE_REQUEST: u16 = 0;
const OP_UNSUBSCRIBE_REQUEST: u16 = 1;
const OP_LIST_REQUEST: u16 = 2;
const OP_GET_PUSH_REQUEST: u16 = 3;

type SharedStore = Arc<Mutex<SubStore>>;

/// Shared daemon state threaded into each connection
/// handler.
#[derive(Clone)]
struct Ctx {
    store: SharedStore,
    /// Set when `$INSULA_TABELLARIUSD_RELAY` is
    /// configured; `None` means relay delivery is off.
    relay: Option<Arc<RelayClient>>,
    /// Pushes received from the relay, waiting for an
    /// app to drain them via GET_PUSH.
    queue: PushQueue,
}

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let socket_path = resolve_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tabellarius-macos: bind {}: {}", socket_path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let store_dir = resolve_substore_path();
    let store = match SubStore::open(store_dir.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "tabellarius-macos: cannot open substore at {}: {}",
                store_dir.display(), e
            );
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "tabellarius-macos: listening on {} (substore at {})",
        socket_path.display(),
        store_dir.display()
    );

    let store: SharedStore = Arc::new(Mutex::new(store));
    let queue: PushQueue = Arc::new(Mutex::new(VecDeque::new()));

    // Relay client — only when an address is configured.
    let relay: Option<Arc<RelayClient>> = match resolve_relay_addr() {
        Some(addr) => {
            // Announce every pubkey we already hold.
            let initial_keys: Vec<[u8; 32]> = {
                let s = store.lock().unwrap();
                s.iter().map(|sub| sub.pubkey_bytes()).collect()
            };
            match RelayClient::connect(
                &addr, initial_keys, store.clone(), queue.clone(),
                resolve_wake_cmd(),
            ) {
                Ok(rc) => {
                    eprintln!("tabellarius-macos: connected to relay {}", addr);
                    Some(Arc::new(rc))
                }
                Err(e) => {
                    eprintln!(
                        "tabellarius-macos: WARNING — relay {} unreachable: {} \
                         (subscribe/list still work; no push delivery)",
                        addr, e
                    );
                    None
                }
            }
        }
        None => None,
    };

    let ctx = Ctx { store, relay, queue };

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tabellarius-macos: accept: {}", e);
                continue;
            }
        };
        let ctx = ctx.clone();
        thread::spawn(move || handle_connection(stream, ctx));
    }
    ExitCode::SUCCESS
}

fn handle_connection(stream: UnixStream, ctx: Ctx) {
    let store = &ctx.store;
    let mut conn = match Connection::wrap(stream) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tabellarius-macos: wrap: {}", e);
            return;
        }
    };

    loop {
        let msg = match conn.recv_message() {
            Ok(m) => m,
            Err(_) => return,
        };
        if msg.opcode_class != CLASS_TABELLARIUS {
            continue;
        }

        match msg.op {
            OP_SUBSCRIBE_REQUEST => {
                // Payload: [u8 app_id_len | app_id | purpose].
                // app_id_len = 0 when the caller isn't a
                // sandboxed app (e.g. the `insula` CLI).
                if msg.payload.is_empty() {
                    continue;
                }
                let alen = msg.payload[0] as usize;
                if msg.payload.len() < 1 + alen {
                    continue;
                }
                let app_id: Option<String> =
                    std::str::from_utf8(&msg.payload[1..1 + alen])
                        .ok()
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                let purpose = match std::str::from_utf8(&msg.payload[1 + alen..]) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                let (key_id, pubkey) = {
                    let mut s = store.lock().unwrap();
                    match s.mint(&purpose, app_id.as_deref()) {
                        Ok(sub) => (sub.key_id.clone(), sub.pubkey_bytes()),
                        Err(e) => {
                            eprintln!("tabellarius-macos: mint: {}", e);
                            continue;
                        }
                    }
                };

                // Announce the new pubkey to the relay
                // so pushes for it start flowing.
                if let Some(relay) = &ctx.relay {
                    if let Err(e) = relay.announce(vec![pubkey]) {
                        eprintln!(
                            "tabellarius-macos: relay announce failed: {}", e
                        );
                    }
                }

                // Wire: [u8 key_id_len | key_id UTF-8 | 32B pubkey]
                let mut out = Vec::with_capacity(1 + key_id.len() + 32);
                out.push(key_id.len() as u8);
                out.extend_from_slice(key_id.as_bytes());
                out.extend_from_slice(&pubkey);
                let _ = conn.send_message(
                    CLASS_TABELLARIUS,
                    OP_SUBSCRIBE_REQUEST,
                    flag::IS_RESPONSE,
                    &out,
                );
            }
            OP_UNSUBSCRIBE_REQUEST => {
                let key_id = match std::str::from_utf8(&msg.payload) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                // Capture the pubkey before removal so
                // we can retract it from the relay.
                let (removed, pubkey) = {
                    let mut s = store.lock().unwrap();
                    let pk = s.iter()
                        .find(|sub| sub.key_id == key_id)
                        .map(|sub| sub.pubkey_bytes());
                    (s.remove(&key_id), pk)
                };
                if removed {
                    if let (Some(relay), Some(pk)) = (&ctx.relay, pubkey) {
                        if let Err(e) = relay.retract(pk) {
                            eprintln!(
                                "tabellarius-macos: relay retract failed: {}", e
                            );
                        }
                    }
                }
                let status: u8 = if removed { 0 } else { 1 };
                let _ = conn.send_message(
                    CLASS_TABELLARIUS,
                    OP_UNSUBSCRIBE_REQUEST,
                    flag::IS_RESPONSE,
                    &[status],
                );
            }
            OP_LIST_REQUEST => {
                let entries: Vec<(String, [u8; 32])> = {
                    let s = store.lock().unwrap();
                    s.iter()
                        .map(|sub| (sub.key_id.clone(), sub.pubkey_bytes()))
                        .collect()
                };
                // Wire: [u16 n LE | for each: u8 id_len | id | 32B pk]
                let mut out = Vec::with_capacity(2 + entries.len() * (1 + 16 + 32));
                let n: u16 = entries.len().try_into().unwrap_or(u16::MAX);
                out.extend_from_slice(&n.to_le_bytes());
                for (id, pk) in entries.iter().take(n as usize) {
                    out.push(id.len() as u8);
                    out.extend_from_slice(id.as_bytes());
                    out.extend_from_slice(pk);
                }
                let _ = conn.send_message(
                    CLASS_TABELLARIUS,
                    OP_LIST_REQUEST,
                    flag::IS_RESPONSE,
                    &out,
                );
            }
            OP_GET_PUSH_REQUEST => {
                // Pop the oldest queued push, if any.
                let next = ctx.queue.lock().unwrap().pop_front();
                let out: Vec<u8> = match next {
                    Some(push) => {
                        // [0 | u8 id_len | key_id | u64 ts LE | blob]
                        let mut v = Vec::with_capacity(
                            1 + 1 + push.key_id.len() + 8 + push.blob.len(),
                        );
                        v.push(0u8); // status: a push follows
                        v.push(push.key_id.len() as u8);
                        v.extend_from_slice(push.key_id.as_bytes());
                        v.extend_from_slice(&push.ts.to_le_bytes());
                        v.extend_from_slice(&push.blob);
                        v
                    }
                    None => vec![1u8], // status: queue empty
                };
                let _ = conn.send_message(
                    CLASS_TABELLARIUS,
                    OP_GET_PUSH_REQUEST,
                    flag::IS_RESPONSE,
                    &out,
                );
            }
            _ => {}
        }
    }
}

fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_TABELLARIUSD_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("tabellarius-macos.sock")
}

fn print_usage() {
    eprintln!(
        "Usage: tabellarius-macos [--help]

Listens on a unix socket and serves CLASS_TABELLARIUS ops:
  op 0 SUBSCRIBE_REQUEST   : payload = purpose UTF-8
                             response = [u8 id_len|id|32B pk]
  op 1 UNSUBSCRIBE_REQUEST : payload = key_id UTF-8
                             response = 1B status (0=ok, 1=unknown)
  op 2 LIST_REQUEST        : payload = (empty)
                             response = [u16 n|for each: u8 id_len|id|32B pk]
  op 3 GET_PUSH_REQUEST    : payload = (empty)
                             response = 1B status (0=push follows, 1=empty)

Environment:
  INSULA_TABELLARIUSD_SOCKET  listen path (default:
                              $XDG_RUNTIME_DIR/tabellarius-macos.sock)
  INSULA_TABELLARIUSD_STORE   substore dir (default:
                              $XDG_RUNTIME_DIR/tabellarius-macos/subs/)
  INSULA_TABELLARIUSD_RELAY   relay address (host:port). When set,
                              the daemon connects, announces every
                              subscription pubkey, and queues inbound
                              pushes for GET_PUSH. Unset = no relay.
  INSULA_TABELLARIUSD_WAKE_CMD  wake-on-push hook. When set, every
                              push that lands for a subscription with
                              a known owning app runs
                              `<cmd> <app_id> <key_id>` (production:
                              launch the app's [background.triggered]
                              entry). Unset = no wake.

Substore is XChaCha20-Poly1305-encrypted at rest under a per-
installation master key; subscriptions survive daemon restart."
    );
}
