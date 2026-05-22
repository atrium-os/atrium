//! Device-side relay client.
//!
//! When `$INSULA_TABELLARIUSD_RELAY` is set, the daemon
//! opens a long-lived TCP connection to a
//! `tabellarius-relay`, announces every subscription
//! pubkey it holds, and spawns a reader thread that
//! turns inbound `RelayMsg::Push` frames into entries
//! on a shared received-push queue. Apps drain that
//! queue via the GET_PUSH op.
//!
//! v0 transport is plaintext TCP + postcard framing
//! (see `tabellarius-relay`'s `proto` module). Mutual-
//! auth TLS is a follow-up.

use crate::substore::SubStore;
use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use tabellarius_relay::proto::{read_msg, write_msg, ClientMsg, PushKey, RelayMsg};

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

/// A live connection to the relay. Holds the write
/// half so the daemon can announce newly-minted
/// subscriptions incrementally.
pub struct RelayClient {
    writer: Mutex<TcpStream>,
}

impl RelayClient {
    /// Connect to `addr`, announce `initial_keys`, and
    /// spawn the reader thread. The reader resolves
    /// each inbound push's `to_key` against `substore`
    /// and appends a [`ReceivedPush`] to `queue`.
    ///
    /// `wake_cmd`, when set, is the wake-on-push hook:
    /// for every push that lands for a subscription with
    /// a known owning app, the reader spawns
    /// `<wake_cmd> <app_id> <key_id>`. In production
    /// that command launches the app's
    /// `[background.triggered]` entry point; tests point
    /// it at a stub.
    pub fn connect(
        addr: &str,
        initial_keys: Vec<PushKey>,
        substore: Arc<Mutex<SubStore>>,
        queue: PushQueue,
        wake_cmd: Option<String>,
    ) -> std::io::Result<RelayClient> {
        let stream = TcpStream::connect(addr)?;
        let mut writer = stream.try_clone()?;

        // Announce everything we already hold.
        write_msg(&mut writer, &ClientMsg::Subscribe { keys: initial_keys })?;

        let reader_stream = stream;
        thread::spawn(move || {
            let mut r = BufReader::new(reader_stream);
            loop {
                let msg: RelayMsg = match read_msg(&mut r) {
                    Ok(m) => m,
                    Err(_) => break, // relay closed / error
                };
                match msg {
                    RelayMsg::Push { to_key, ts, blob, .. } => {
                        // Resolve to_key -> the key_id +
                        // owning app the local substore
                        // records.
                        let resolved: Option<(String, Option<String>)> = {
                            let s = substore.lock().unwrap();
                            let found = s.iter()
                                .find(|sub| sub.pubkey_bytes() == to_key)
                                .map(|sub| {
                                    (sub.key_id.clone(), sub.app_id.clone())
                                });
                            found
                        };
                        match resolved {
                            Some((key_id, app_id)) => {
                                queue.lock().unwrap().push_back(ReceivedPush {
                                    key_id: key_id.clone(), ts, blob,
                                });
                                // Wake-on-push: if the
                                // subscription has a
                                // known owner and a hook
                                // is configured, fire it.
                                if let (Some(cmd), Some(app)) =
                                    (&wake_cmd, &app_id)
                                {
                                    fire_wake(cmd, app, &key_id);
                                }
                            }
                            None => {
                                // Push for a key we don't
                                // hold — relay routing
                                // glitch. Drop it.
                                eprintln!(
                                    "tabellarius-macos: dropped push for \
                                     unknown key"
                                );
                            }
                        }
                    }
                    RelayMsg::Pong => {}
                    // PublishAccepted / PublishRejected are
                    // publisher-side; a device shouldn't
                    // see them. Ignore defensively.
                    _ => {}
                }
            }
        });

        Ok(RelayClient { writer: Mutex::new(writer) })
    }

    /// Announce additional subscription pubkeys to the
    /// relay (called when a new subscription is minted).
    pub fn announce(&self, keys: Vec<PushKey>) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        write_msg(&mut *w, &ClientMsg::Subscribe { keys })
    }

    /// Drop interest in a pubkey (called on unsubscribe).
    pub fn retract(&self, key: PushKey) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        write_msg(&mut *w, &ClientMsg::Unsubscribe { keys: vec![key] })
    }
}

/// Spawn the wake-on-push hook: `<cmd> <app_id> <key_id>`.
/// Detached + fire-and-forget — the reader thread must
/// not block on the woken process. Failures are logged,
/// not fatal (a missing hook just means no wake).
fn fire_wake(cmd: &str, app_id: &str, key_id: &str) {
    // Invoke as `/bin/sh <cmd> <app_id> <key_id>` rather
    // than exec'ing <cmd> directly: directly spawning a
    // script by its shebang via posix_spawn hangs on
    // macOS for freshly-written temp-dir scripts. Going
    // through /bin/sh sidesteps the shebang-exec path.
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
            // Reap on a detached thread so a slow wake
            // hook can't stall the reader (and the
            // process doesn't linger as a zombie).
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            eprintln!(
                "tabellarius-macos: wake hook {:?} failed for {}: {}",
                cmd, app_id, e
            );
        }
    }
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
