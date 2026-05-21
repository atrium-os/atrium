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

use aqueduct::classes::CLASS_VESTIBULUM;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

const OP_PUBKEY_REQUEST: u16 = 0;
const OP_SIGN_REQUEST: u16 = 1;

/// Shared key store. Maps "service.persona" -> keypair.
type KeyStore = Arc<Mutex<HashMap<String, SigningKey>>>;

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

    eprintln!("vestibulum-macos: listening on {}", socket_path.display());

    let store: KeyStore = Arc::new(Mutex::new(HashMap::new()));

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

fn handle_connection(stream: UnixStream, store: KeyStore) {
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
                let pk = {
                    let mut s = store.lock().unwrap();
                    pubkey_for_service(&mut s, &service)
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
                    let sk = mint_if_needed(&mut s, &service);
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

fn pubkey_for_service(
    store: &mut HashMap<String, SigningKey>,
    service: &str,
) -> VerifyingKey {
    let sk = mint_if_needed(store, service);
    sk.verifying_key()
}

fn mint_if_needed<'a>(
    store: &'a mut HashMap<String, SigningKey>,
    service: &str,
) -> &'a SigningKey {
    store
        .entry(service.to_string())
        .or_insert_with(|| SigningKey::generate(&mut OsRng))
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
  INSULA_VESTIBULUMD_SOCKET   listen path (default:
                              $XDG_RUNTIME_DIR/vestibulum-macos.sock)

v0: in-memory keystore (lost on restart). Persistent storage via
macOS Keychain Services is future work."
    );
}
