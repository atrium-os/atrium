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

mod substore;

use aqueduct::classes::CLASS_TABELLARIUS;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use substore::{resolve_substore_path, SubStore};

const OP_SUBSCRIBE_REQUEST: u16 = 0;
const OP_UNSUBSCRIBE_REQUEST: u16 = 1;
const OP_LIST_REQUEST: u16 = 2;

type SharedStore = Arc<Mutex<SubStore>>;

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

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tabellarius-macos: accept: {}", e);
                continue;
            }
        };
        let store = store.clone();
        thread::spawn(move || handle_connection(stream, store));
    }
    ExitCode::SUCCESS
}

fn handle_connection(stream: UnixStream, store: SharedStore) {
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
                let purpose = match std::str::from_utf8(&msg.payload) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                let (key_id, pubkey) = {
                    let mut s = store.lock().unwrap();
                    match s.mint(&purpose) {
                        Ok(sub) => (sub.key_id.clone(), sub.pubkey_bytes()),
                        Err(e) => {
                            eprintln!("tabellarius-macos: mint: {}", e);
                            continue;
                        }
                    }
                };

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
                let removed = {
                    let mut s = store.lock().unwrap();
                    s.remove(&key_id)
                };
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

Environment:
  INSULA_TABELLARIUSD_SOCKET  listen path (default:
                              $XDG_RUNTIME_DIR/tabellarius-macos.sock)
  INSULA_TABELLARIUSD_STORE   substore dir (default:
                              $XDG_RUNTIME_DIR/tabellarius-macos/subs/)

Substore is file-backed; subscriptions survive daemon restart.
Plaintext on disk (v0); Keychain-Services wrapping is shared
future work with the vestibulum daemon."
    );
}
