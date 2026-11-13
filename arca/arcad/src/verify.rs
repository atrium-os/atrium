//! Verify-each-use (arca.md §6.2, sigillum.md §8): before an app's
//! verify-each-use item is used, Sigillum asks the user (`VERIFY_USER`, on
//! its own sheet: the passcode through ostiarius, or a key or sensor with
//! UV) and answers on arcad's own connection. arcad trusts the answer
//! because of the connection (sigillumd knows arcad by its uid,
//! `SIGILLUM_ARCA_UID`); there is no token to replay.
//!
//! The ceremony can last minutes, so it runs with the state unlocked:
//! [`crate::serve_conn_with`] asks the [`Verify`] between a request that
//! answered `NEEDS_VERIFICATION` and its rerun through
//! [`crate::server::State::handle_verified`].

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sigillum_proto::client::{self, Purpose, Reply, Request, Strength, VerifyUser};
use sigillum_proto::{read_msg, write_msg};

/// The weakest method a verify-each-use item accepts (sigillum.md
/// decision 6): a secure sensor, a key with UV, or the passcode.
pub const MIN_STRENGTH: Strength = Strength::BiometricSecure;

pub trait Verify: Send + Sync {
    /// Did the user confirm, on Sigillum's sheet, the use of `description`?
    fn verify(&self, user: &str, description: &str) -> bool;
}

/// sigillumd's client socket.
pub struct Sigillum {
    pub socket: PathBuf,
    next: AtomicU64,
}

impl Sigillum {
    pub fn new(socket: impl Into<PathBuf>) -> Sigillum {
        Sigillum { socket: socket.into(), next: AtomicU64::new(1) }
    }

    fn ask(&self, user: &str, description: &str) -> io::Result<bool> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut s = UnixStream::connect(&self.socket)?;
        // The longest ceremony sigillumd runs, and a margin.
        s.set_read_timeout(Some(Duration::from_secs(11 * 60)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut d = description.to_string();
        if d.len() > 200 {
            let mut end = 200;
            while !d.is_char_boundary(end) {
                end -= 1;
            }
            d.truncate(end);
        }
        let req = Request::VerifyUser(VerifyUser { ceremony: id, user: user.into(), purpose: Purpose::ArcaItem { description: d }, min_strength: MIN_STRENGTH });
        write_msg(&mut s, &req, client::MAX_MESSAGE)?;
        Ok(match read_msg::<Reply>(&mut s, client::MAX_MESSAGE)? {
            Some(Reply::Verified { ceremony, strength, .. }) => ceremony == id && strength >= MIN_STRENGTH,
            _ => false,
        })
    }
}

impl Verify for Sigillum {
    fn verify(&self, user: &str, description: &str) -> bool {
        self.ask(user, description).unwrap_or_else(|e| {
            eprintln!("arcad: sigillumd: {:?}", e.kind());
            false
        })
    }
}
