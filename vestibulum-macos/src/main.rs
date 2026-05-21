//! `vestibulum-macos` — keychain daemon for Insula
//! apps on macOS.
//!
//! Listens on a unix socket for Aqueduct connections,
//! decodes CLASS_VESTIBULUM ops (see
//! `aqueduct/src/classes.rs` for the schema), manages
//! per-(service, persona) ed25519 keypairs in memory,
//! signs challenges on request.
//!
//! v0 keystore is in-memory only; keys are minted on
//! demand and lost when the daemon exits. Persistence
//! via macOS Keychain Services is future work
//! (`docs/spec/vestibulum.md` §3.1).
//!
//! Persona handling: v0 has a single implicit
//! "default" persona; multi-persona support comes
//! later (Vestibulum spec §3.6).
//!
//! # Configuration
//!
//! - `INSULA_VESTIBULUMD_SOCKET` — listen path. Default:
//!   `$XDG_RUNTIME_DIR/vestibulum-macos.sock` or
//!   `$TMPDIR/vestibulum-macos.sock` or
//!   `/tmp/vestibulum-macos.sock`.

mod keystore;
use keystore::{resolve_keystore_path, Keystore};

use aqueduct::classes::CLASS_VESTIBULUM;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

const OP_PUBKEY_REQUEST: u16 = 0;
const OP_SIGN_REQUEST: u16 = 1;

/// Shared disk-backed key store.
type SharedKeystore = Arc<Mutex<Keystore>>;

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
            eprintln!("vestibulum-macos: bind {}: {}", socket_path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let keystore_dir = resolve_keystore_path();
    let keystore = match Keystore::open(keystore_dir.clone()) {
        Ok(ks) => ks,
        Err(e) => {
            eprintln!(
                "vestibulum-macos: cannot open keystore at {}: {}",
                keystore_dir.display(),
                e
            );
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "vestibulum-macos: listening on {} (keystore at {})",
        socket_path.display(),
        keystore_dir.display()
    );

    let store: SharedKeystore = Arc::new(Mutex::new(keystore));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("vestibulum-macos: accept error: {}", e);
                continue;
            }
        };
        let store = store.clone();
        thread::spawn(move || handle_connection(stream, store));
    }
    ExitCode::SUCCESS
}

fn handle_connection(stream: UnixStream, store: SharedKeystore) {
    let mut conn = match Connection::wrap(stream) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vestibulum-macos: wrap: {}", e);
            return;
        }
    };

    loop {
        let msg = match conn.recv_message() {
            Ok(m) => m,
            Err(_) => return,
        };
        if msg.opcode_class != CLASS_VESTIBULUM {
            // Spec violation — wrong class on a
            // vestibulum socket. Drop the message and
            // keep going.
            continue;
        }

        match msg.op {
            OP_PUBKEY_REQUEST => {
                let service = match std::str::from_utf8(&msg.payload) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                let pk: VerifyingKey = {
                    let mut s = store.lock().unwrap();
                    s.get_or_mint(&service).verifying_key()
                };
                let _ = conn.send_message(
                    CLASS_VESTIBULUM,
                    OP_PUBKEY_REQUEST,
                    flag::IS_RESPONSE,
                    pk.as_bytes(),
                );
            }
            OP_SIGN_REQUEST => {
                // Payload: u16 LE name_len | name | challenge
                if msg.payload.len() < 2 {
                    continue;
                }
                let name_len = u16::from_le_bytes([
                    msg.payload[0],
                    msg.payload[1],
                ]) as usize;
                if msg.payload.len() < 2 + name_len {
                    continue;
                }
                let service =
                    match std::str::from_utf8(&msg.payload[2..2 + name_len]) {
                        Ok(s) => s.to_string(),
                        Err(_) => continue,
                    };
                let challenge = &msg.payload[2 + name_len..];

                let sig: Signature = {
                    let mut s = store.lock().unwrap();
                    let sk = s.get_or_mint(&service);
                    sk.sign(challenge)
                };
                let _ = conn.send_message(
                    CLASS_VESTIBULUM,
                    OP_SIGN_REQUEST,
                    flag::IS_RESPONSE,
                    &sig.to_bytes(),
                );
            }
            _ => {
                // Unknown op; drop.
            }
        }
    }
}

fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_VESTIBULUMD_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("vestibulum-macos.sock")
}

fn print_usage() {
    eprintln!(
        "Usage: vestibulum-macos [--help]

Listens on a unix socket and serves CLASS_VESTIBULUM ops:
  op 0 PUBKEY_REQUEST: payload = service name, response = 32B pubkey
  op 1 SIGN_REQUEST  : payload = [u16 name_len | name | challenge],
                       response = 64B signature

Environment:
  INSULA_VESTIBULUMD_SOCKET     listen path (default:
                                $XDG_RUNTIME_DIR/vestibulum-macos.sock)
  INSULA_VESTIBULUMD_KEYSTORE   keystore dir (default:
                                $XDG_RUNTIME_DIR/vestibulum-macos/keys/)

Keystore is **file-backed** (one .key file per service, raw
32-byte ed25519 secret -- v0 is plaintext on disk; macOS Keychain
Services wrapping is future work). Keys survive daemon restart."
    );
}
