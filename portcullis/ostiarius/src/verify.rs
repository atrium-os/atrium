//! Passcode checks for Sigillum (sigillum.md §8): the `VERIFY_USER` sheet's
//! "use your passcode", for Arca's verify-each-use items and for passkeys
//! that need user verification.
//!
//! Its own socket (`/atrium/sockets/ostiarius/verify.sock`), its own thread:
//! the login control loop serves vestibulum one connection at a time, and a
//! passcode check must not wait behind a login (or the other way round).
//! Only one peer uid is served: `_sigillum` (65028), checked with
//! `getpeereid` on every connection.
//!
//! The check is the login one, the same `VerifyCredential` through the
//! portcullisd broker (PAM runs there, as root); this jailed ostiarius never
//! reads shadow and Sigillum never sees a hash. ostiarius adds the limit:
//! after [`FREE_FAILURES`] failures in a row for a user, that user's checks
//! are refused unasked for a lockout that doubles each time (from
//! [`FIRST_LOCKOUT`] up to [`MAX_LOCKOUT`]); a success resets it. A locked
//! answer is immediate, so sigillumd never waits on a sleep here.
//!
//! The wire is one JSON line each way:
//! `{"user": "alice", "passcode": "…"}` → `{"ok": true}` or
//! `{"ok": false, "locked_secs": 0}`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/atrium/sockets/ostiarius/verify.sock";
/// `_sigillum`.
pub const SIGILLUM_UID: u32 = 65028;
pub const FREE_FAILURES: u32 = 5;
pub const FIRST_LOCKOUT: Duration = Duration::from_secs(60);
pub const MAX_LOCKOUT: Duration = Duration::from_secs(3600);
/// Longest request line (a user name and a passcode).
const MAX_LINE: usize = 1024;

#[derive(Debug, Deserialize)]
pub struct Ask {
    pub user: String,
    pub passcode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    /// Seconds until this user may try again (0: not locked).
    #[serde(default)]
    pub locked_secs: u64,
}

#[derive(Default)]
struct Tries {
    failures: u32,
    lockouts: u32,
    until: Option<Instant>,
}

/// The rate limit around a credential check.
pub struct Verifier<F: Fn(&str, &str) -> Result<String, String>> {
    check: F,
    tries: HashMap<String, Tries>,
}

impl<F: Fn(&str, &str) -> Result<String, String>> Verifier<F> {
    pub fn new(check: F) -> Verifier<F> {
        Verifier { check, tries: HashMap::new() }
    }

    pub fn ask(&mut self, user: &str, passcode: &str, now: Instant) -> Answer {
        if user.is_empty() || user.len() > 64 || passcode.is_empty() {
            return Answer { ok: false, locked_secs: 0 };
        }
        let t = self.tries.entry(user.to_string()).or_default();
        if let Some(until) = t.until {
            if now < until {
                return Answer { ok: false, locked_secs: (until - now).as_secs().max(1) };
            }
            t.until = None;
        }
        // The canonical name PAM answers must be the one asked about: a
        // check for alice is not passed by bob's passcode.
        if (self.check)(user, passcode).is_ok_and(|u| u == user) {
            *t = Tries::default();
            return Answer { ok: true, locked_secs: 0 };
        }
        t.failures += 1;
        if t.failures >= FREE_FAILURES {
            let lock = FIRST_LOCKOUT.saturating_mul(1 << t.lockouts.min(10)).min(MAX_LOCKOUT);
            t.lockouts += 1;
            t.failures = 0;
            t.until = Some(now + lock);
            return Answer { ok: false, locked_secs: lock.as_secs() };
        }
        Answer { ok: false, locked_secs: 0 }
    }
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    portcullis_peer::uid_gid(s.as_raw_fd()).ok().map(|(u, _)| u)
}

/// One connection: answer request lines until it closes or misbehaves.
pub fn serve_conn<F: Fn(&str, &str) -> Result<String, String>>(v: &mut Verifier<F>, s: UnixStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let Ok(mut w) = s.try_clone() else { return };
    let mut r = BufReader::new(s).take(MAX_LINE as u64 * 64);
    let mut line = String::new();
    loop {
        line.clear();
        match (&mut r).take(MAX_LINE as u64).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if !line.ends_with('\n') => return,
            Ok(_) => {}
        }
        let a = match serde_json::from_str::<Ask>(line.trim_end()) {
            Ok(ask) => v.ask(&ask.user, &ask.passcode, Instant::now()),
            Err(_) => return,
        };
        // Wipe the line's copy of the passcode before the next read.
        std::mem::take(&mut line).into_bytes().fill(0);
        let mut body = serde_json::to_vec(&a).unwrap_or_default();
        body.push(b'\n');
        if w.write_all(&body).is_err() {
            return;
        }
    }
}

/// Serve `sock` for `uid` only, one connection at a time (sigillumd has one).
pub fn serve<F: Fn(&str, &str) -> Result<String, String>>(sock: &str, uid: u32, check: F) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(sock);
    let l = UnixListener::bind(sock)?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o666))?;
    let mut v = Verifier::new(check);
    for c in l.incoming().flatten() {
        if peer_uid(&c) != Some(uid) {
            eprintln!("ostiarius: verify socket refused a peer");
            continue;
        }
        serve_conn(&mut v, c);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pam(user: &str, pw: &str) -> Result<String, String> {
        match (user, pw) {
            ("alice", "right") => Ok("alice".into()),
            ("bob", "bobs") => Ok("bob".into()),
            // A PAM stack that canonicalises to another name.
            ("mallory", "x") => Ok("alice".into()),
            _ => Err("authentication failed".into()),
        }
    }

    #[test]
    fn right_and_wrong() {
        let mut v = Verifier::new(pam);
        let t = Instant::now();
        assert!(v.ask("alice", "right", t).ok);
        assert!(!v.ask("alice", "bobs", t).ok);
        assert!(!v.ask("alice", "", t).ok);
        assert!(!v.ask("mallory", "x", t).ok, "the answer names another user");
    }

    #[test]
    fn failures_lock_out_with_doubling_and_success_resets() {
        let mut v = Verifier::new(pam);
        let t = Instant::now();
        for _ in 0..FREE_FAILURES - 1 {
            assert_eq!(v.ask("alice", "no", t), Answer { ok: false, locked_secs: 0 });
        }
        assert_eq!(v.ask("alice", "no", t), Answer { ok: false, locked_secs: 60 });
        // Locked: even the right passcode is refused, without asking PAM.
        assert_eq!(v.ask("alice", "right", t + Duration::from_secs(10)).locked_secs, 50);
        // Bob isn't affected.
        assert!(v.ask("bob", "bobs", t).ok);
        // After the lockout, five more failures lock for twice as long.
        let t2 = t + Duration::from_secs(61);
        for _ in 0..FREE_FAILURES - 1 {
            v.ask("alice", "no", t2);
        }
        assert_eq!(v.ask("alice", "no", t2).locked_secs, 120);
        let t3 = t2 + Duration::from_secs(121);
        assert!(v.ask("alice", "right", t3).ok);
        for _ in 0..FREE_FAILURES - 1 {
            v.ask("alice", "no", t3);
        }
        assert_eq!(v.ask("alice", "no", t3).locked_secs, 60, "a success reset the doubling");
    }

    #[test]
    fn the_wire() {
        let (a, b) = UnixStream::pair().unwrap();
        let h = std::thread::spawn(move || {
            let mut v = Verifier::new(pam);
            serve_conn(&mut v, b);
        });
        let mut w = a.try_clone().unwrap();
        let mut r = BufReader::new(a);
        let mut ask = |l: &str| {
            w.write_all(l.as_bytes()).unwrap();
            let mut s = String::new();
            r.read_line(&mut s).unwrap();
            s
        };
        assert_eq!(ask("{\"user\":\"alice\",\"passcode\":\"right\"}\n"), "{\"ok\":true,\"locked_secs\":0}\n");
        assert_eq!(ask("{\"user\":\"alice\",\"passcode\":\"wrong\"}\n"), "{\"ok\":false,\"locked_secs\":0}\n");
        // Garbage ends the connection.
        assert_eq!(ask("not json\n"), "");
        h.join().unwrap();
    }
}
