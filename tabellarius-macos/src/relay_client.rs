//! Device-side relay client.
//!
//! When `$INSULA_TABELLARIUSD_RELAY` is set, the daemon
//! opens a long-lived connection to a
//! `tabellarius-relay`, announces every subscription
//! pubkey it holds, and runs a poll loop that turns
//! inbound `RelayMsg::Push` frames into entries on a
//! shared received-push queue (drained by GET_PUSH) and
//! fires the wake-on-push hook.
//!
//! One poll-loop thread owns the stream. Announce /
//! retract from other threads go through an outbound
//! channel the loop drains — there is no second writer
//! thread, because a TLS stream's state is single-owner
//! and can't be split the way a `try_clone`'d TCP
//! socket can.
//!
//! Transport is CBOR frames over TCP, optionally
//! wrapped in mutual-auth TLS (self-signed + key
//! pinning) when `$INSULA_TABELLARIUSD_TLS_IDENTITY` +
//! `_TLS_PEER_PIN` are set.

use crate::substore::SubStore;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tabellarius_relay::proto::{decode_payload, encode_frame, ClientMsg, FrameReader, PushKey, RelayMsg};
use tabellarius_relay::tls::Identity;

/// Poll-loop read timeout — bounds how long an
/// announce/retract waits to reach the wire.
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

/// One push the device received from the relay,
/// resolved against the local substore so the app sees
/// the `key_id` it knows rather than the raw pubkey.
#[derive(Debug, Clone)]
pub struct ReceivedPush {
    pub key_id: String,
    pub ts: u64,
    pub blob: Vec<u8>,
}

/// FIFO of pushes waiting to be drained by GET_PUSH.
pub type PushQueue = Arc<Mutex<VecDeque<ReceivedPush>>>;

/// A live connection to the relay. Announce / retract
/// hand a `ClientMsg` to the poll loop via the channel.
pub struct RelayClient {
    out: Sender<ClientMsg>,
}

impl RelayClient {
    /// Connect to `addr`, announce `initial_keys`, and
    /// spawn the poll-loop thread. The loop resolves
    /// each inbound push's `to_key` against `substore`,
    /// appends a [`ReceivedPush`] to `queue`, and (when
    /// `wake_cmd` is set + the subscription has a known
    /// owning app) spawns `<wake_cmd> <app_id> <key_id>`.
    pub fn connect(
        addr: &str,
        initial_keys: Vec<PushKey>,
        substore: Arc<Mutex<SubStore>>,
        queue: PushQueue,
        wake_cmd: Option<String>,
    ) -> std::io::Result<RelayClient> {
        let tcp = TcpStream::connect(addr)?;
        tcp.set_read_timeout(Some(POLL_TIMEOUT))?;

        let (tx, rx) = mpsc::channel::<ClientMsg>();
        // Queue the opening announce; the loop writes it
        // on its first turn.
        let _ = tx.send(ClientMsg::Subscribe { keys: initial_keys });

        // Optional mutual-TLS wrap.
        let tls = load_tls_config()
            .map_err(|e| std::io::Error::new(
                std::io::ErrorKind::Other, e))?;

        match tls {
            Some(cfg) => {
                let server_name =
                    rustls::pki_types::ServerName::try_from("localhost")
                        .map_err(|e| std::io::Error::new(
                            std::io::ErrorKind::Other, e.to_string()))?;
                let conn = rustls::ClientConnection::new(cfg, server_name)
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::Other, e.to_string()))?;
                let stream = rustls::StreamOwned::new(conn, tcp);
                thread::spawn(move || {
                    poll_loop(stream, rx, substore, queue, wake_cmd);
                });
            }
            None => {
                thread::spawn(move || {
                    poll_loop(tcp, rx, substore, queue, wake_cmd);
                });
            }
        }

        Ok(RelayClient { out: tx })
    }

    /// Announce additional subscription pubkeys to the
    /// relay (called when a new subscription is minted).
    pub fn announce(&self, keys: Vec<PushKey>) -> std::io::Result<()> {
        self.out.send(ClientMsg::Subscribe { keys })
            .map_err(|_| std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "relay poll loop gone"))
    }

    /// Drop interest in a pubkey (called on unsubscribe).
    pub fn retract(&self, key: PushKey) -> std::io::Result<()> {
        self.out.send(ClientMsg::Unsubscribe { keys: vec![key] })
            .map_err(|_| std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "relay poll loop gone"))
    }
}

/// The single per-connection poll loop. Generic over
/// the transport so plaintext `TcpStream` and
/// `rustls::StreamOwned` share one path.
fn poll_loop<S: Read + Write>(
    mut stream: S,
    rx: mpsc::Receiver<ClientMsg>,
    substore: Arc<Mutex<SubStore>>,
    queue: PushQueue,
    wake_cmd: Option<String>,
) {
    let mut fr = FrameReader::new();
    let mut scratch = [0u8; 8192];

    'conn: loop {
        // 1. Drain outbound ClientMsgs (announce /
        //    retract / the opening Subscribe).
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    let Ok(bytes) = encode_frame(&msg) else { continue };
                    if stream.write_all(&bytes).is_err() {
                        break 'conn;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // RelayClient dropped — nothing more
                    // to send, but keep reading pushes.
                    break;
                }
            }
        }
        if stream.flush().is_err() {
            break;
        }

        // 2. Read available bytes; dispatch frames.
        match stream.read(&mut scratch) {
            Ok(0) => break, // relay closed
            Ok(n) => {
                fr.feed(&scratch[..n]);
                loop {
                    match fr.next_frame() {
                        Ok(Some(payload)) => {
                            if let Ok(msg) =
                                decode_payload::<RelayMsg>(&payload)
                            {
                                handle_relay_msg(
                                    msg, &substore, &queue, &wake_cmd,
                                );
                            }
                        }
                        Ok(None) => break,
                        Err(_) => break 'conn,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {
                // Idle tick.
            }
            Err(_) => break,
        }
    }
}

fn handle_relay_msg(
    msg: RelayMsg,
    substore: &Arc<Mutex<SubStore>>,
    queue: &PushQueue,
    wake_cmd: &Option<String>,
) {
    let RelayMsg::Push { to_key, ts, blob, .. } = msg else {
        // Pong / publisher-side variants — ignore.
        return;
    };
    // Resolve to_key -> (key_id, owning app).
    let resolved: Option<(String, Option<String>)> = {
        let s = substore.lock().unwrap();
        let found = s.iter()
            .find(|sub| sub.pubkey_bytes() == to_key)
            .map(|sub| (sub.key_id.clone(), sub.app_id.clone()));
        found
    };
    match resolved {
        Some((key_id, app_id)) => {
            queue.lock().unwrap().push_back(ReceivedPush {
                key_id: key_id.clone(), ts, blob,
            });
            if let (Some(cmd), Some(app)) = (wake_cmd, &app_id) {
                fire_wake(cmd, app, &key_id);
            }
        }
        None => {
            eprintln!("tabellarius-macos: dropped push for unknown key");
        }
    }
}

/// Spawn the wake-on-push hook: `<cmd> <app_id> <key_id>`.
fn fire_wake(cmd: &str, app_id: &str, key_id: &str) {
    // Invoke via `/bin/sh <cmd> ...` rather than exec'ing
    // <cmd> directly: posix_spawn of a freshly-written
    // temp-dir shebang script hangs on macOS.
    let child = std::process::Command::new("/bin/sh")
        .arg(cmd)
        .arg(app_id)
        .arg(key_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            thread::spawn(move || { let _ = child.wait(); });
        }
        Err(e) => {
            eprintln!(
                "tabellarius-macos: wake hook {:?} failed for {}: {}",
                cmd, app_id, e
            );
        }
    }
}

/// Build the device-side TLS client config when
/// `$INSULA_TABELLARIUSD_TLS_IDENTITY` + `_TLS_PEER_PIN`
/// are both set. `None` = plaintext.
fn load_tls_config() -> Result<Option<Arc<rustls::ClientConfig>>, String> {
    let id_path = std::env::var_os("INSULA_TABELLARIUSD_TLS_IDENTITY");
    let pin_path = std::env::var_os("INSULA_TABELLARIUSD_TLS_PEER_PIN");
    let (id_path, pin_path) = match (id_path, pin_path) {
        (Some(i), Some(p)) => (i, p),
        (None, None) => return Ok(None),
        _ => return Err(
            "set both INSULA_TABELLARIUSD_TLS_IDENTITY and \
             _TLS_PEER_PIN, or neither".into()
        ),
    };
    let identity = Identity::read_from(Path::new(&id_path))?;
    let pin = std::fs::read(&pin_path)
        .map_err(|e| format!("read peer pin: {e}"))?;
    let cfg = tabellarius_relay::tls::client_config(&identity, &pin)?;
    Ok(Some(cfg))
}

/// Resolve the relay address from the environment.
/// Returns `None` (relay disabled) when unset.
pub fn resolve_relay_addr() -> Option<String> {
    std::env::var("INSULA_TABELLARIUSD_RELAY").ok()
        .filter(|s| !s.is_empty())
}

/// Resolve the wake-on-push hook command from the
/// environment. `None` = wake-on-push disabled.
pub fn resolve_wake_cmd() -> Option<String> {
    std::env::var("INSULA_TABELLARIUSD_WAKE_CMD").ok()
        .filter(|s| !s.is_empty())
}
