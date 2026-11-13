//! ostiarius's side of Sigillum (docs/spec/sigillum.md §12, §16.6): the lock
//! screen's unlock with a security key, and adding a key.
//!
//! ostiarius never talks to a key. It asks `sigillumd`, which shows its own
//! trusted sheet (key or passcode), runs the ceremony through the key's jailed
//! driver and checks the answer itself; ostiarius gets only yes or no. A
//! ceremony can take minutes (PIN, touch), so the reply waits that long.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sigillum_proto::client::{self, EnrolKey, Purpose, Reply, Request, Strength, VerifyUser};
use sigillum_proto::{read_msg, write_msg};

/// What ostiarius asks of Sigillum. A trait so the control wire is tested
/// without a broker.
pub trait KeyCheck {
    /// Show the unlock sheet for `user` (an enrolled key, or the passcode).
    /// `Ok` only when sigillumd answered verified.
    fn verify(&self, user: &str) -> Result<(), String>;
    /// Add a security key for `user`: sigillumd asks the passcode first.
    fn enrol(&self, user: &str, name: &str) -> Result<(), String>;
}

/// The real broker on its client socket.
pub struct SigillumLink {
    socket: PathBuf,
    next: AtomicU64,
}

/// Longer than any ceremony sigillumd runs (its own timeout ends it first).
const WAIT: Duration = Duration::from_secs(300);

impl SigillumLink {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        SigillumLink { socket: socket.into(), next: AtomicU64::new(1) }
    }

    fn ask(&self, req: impl FnOnce(u64) -> Request) -> Result<Reply, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut s = UnixStream::connect(&self.socket).map_err(|e| format!("sigillumd: {e}"))?;
        s.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        write_msg(&mut s, &req(id), client::MAX_MESSAGE).map_err(|e| format!("sigillumd: {e}"))?;
        read_msg::<Reply>(&mut s, client::MAX_MESSAGE)
            .map_err(|e| format!("sigillumd: {e}"))?
            .ok_or_else(|| "sigillumd closed the connection".to_string())
    }
}

fn refused(r: Reply) -> String {
    match r {
        Reply::Refused { reason, .. } => format!("not verified ({reason:?})"),
        other => format!("unexpected answer from sigillumd: {other:?}"),
    }
}

impl KeyCheck for SigillumLink {
    fn verify(&self, user: &str) -> Result<(), String> {
        // key-uv: an enrolled key unlocks; with none, the sheet asks the passcode.
        let r = self.ask(|ceremony| {
            Request::VerifyUser(VerifyUser { ceremony, user: user.into(), purpose: Purpose::Unlock, min_strength: Strength::KeyUv })
        })?;
        match r {
            Reply::Verified { .. } => Ok(()),
            r => Err(refused(r)),
        }
    }

    fn enrol(&self, user: &str, name: &str) -> Result<(), String> {
        let r = self.ask(|ceremony| Request::EnrolKey(EnrolKey { ceremony, user: user.into(), name: name.into() }))?;
        match r {
            Reply::KeyEnrolled { .. } => Ok(()),
            r => Err(refused(r)),
        }
    }
}
