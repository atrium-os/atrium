//! Platform peer identity — the shared answer to "**which app** is this
//! connecting peer?", for *any* Atrium service that enforces per-app
//! capabilities.
//!
//! Portcullis launches each app in its own jail under a **dedicated uid** (a
//! reserved range — not the human's, not shared: a dedicated uid is both the
//! isolation boundary and the unforgeable identity) and **writes** the launch
//! registry `uid → (owning user, app-id)`. A service reads `getpeereid` and
//! resolves the uid here — so the app's identity is the kernel's answer via the
//! binding, never the app's own word. **Choragus** uses this for the audio caps;
//! **Fresco** would use it for `graphics`, **Tabula** for `clipboard`, etc. One
//! primitive, written once by the launcher, read by every service — not a copy
//! per service (the mistake this crate corrects: it started life inside choragus).

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;

/// The platform launch registry's default path (Portcullis writes it).
pub const DEFAULT_REGISTRY: &str = "/var/run/atrium/app-registry";

// ── getpeereid: the unforgeable handle ────────────────────────────────────────

/// The connected peer's `(uid, gid)` — the kernel's authenticated answer.
pub fn uid_gid(fd: RawFd) -> io::Result<(u32, u32)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((uid, gid))
}

/// The username for a uid (via the passwd database).
pub fn username(uid: u32) -> Option<String> {
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) };
    Some(name.to_string_lossy().into_owned())
}

// ── the launch registry: uid → (owning user, app-id) ──────────────────────────

#[derive(Default)]
pub struct AppRegistry {
    map: HashMap<u32, (String, String)>,
}

impl AppRegistry {
    /// Parse the registry text. One app per line: `<uid> <user> <app-id>`;
    /// `#` comments and blanks ignored.
    pub fn parse(text: &str) -> AppRegistry {
        let mut map = HashMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(uid), Some(user), Some(app)) = (it.next(), it.next(), it.next()) {
                if let Ok(uid) = uid.parse::<u32>() {
                    map.insert(uid, (user.to_string(), app.to_string()));
                }
            }
        }
        AppRegistry { map }
    }

    pub fn load(path: &str) -> io::Result<AppRegistry> {
        Ok(AppRegistry::parse(&std::fs::read_to_string(path)?))
    }

    /// The verified `(owning user, app-id)` for a uid, if Portcullis launched an
    /// app there. `None` = not a Portcullis-launched app (→ default-deny).
    pub fn resolve(&self, uid: u32) -> Option<(&str, &str)> {
        self.map.get(&uid).map(|(u, a)| (u.as_str(), a.as_str()))
    }
}

/// Append a binding to the registry file (the **write** side — Portcullis calls
/// this when it launches an app at `uid`). Append-only; one line per launch.
pub fn register(path: &str, uid: u32, user: &str, app_id: &str) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{uid} {user} {app_id}")
}

// ── the one call a service makes ──────────────────────────────────────────────

/// A resolved connecting peer.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub uid: u32,
    pub user: String,
    /// The verified app-id, or `None` if the uid isn't in the launch registry
    /// (not a Portcullis-launched app → a service should default-deny).
    pub app_id: Option<String>,
}

/// Resolve a connection's peer to its verified identity: `getpeereid` →
/// `(uid, user)`, then the launch registry → the app-id. This is the single call
/// every capability-enforcing service makes; the app's claimed id (if any) is
/// only advisory next to this.
pub fn resolve(fd: RawFd, registry: &AppRegistry) -> io::Result<Peer> {
    let (uid, _gid) = uid_gid(fd)?;
    let user = username(uid).unwrap_or_else(|| format!("uid{uid}"));
    let app_id = match registry.resolve(uid) {
        Some((reg_user, app)) => {
            // prefer the registry's owning user (authoritative) over getpwuid.
            return Ok(Peer { uid, user: reg_user.to_string(), app_id: Some(app.to_string()) });
        }
        None => None,
    };
    Ok(Peer { uid, user, app_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = "
        # uid  user  app-id  (Portcullis writes this at launch)
        100001  alice  org.atrium.recorder
        100002  alice  org.atrium.player
    ";

    #[test]
    fn resolves_uid_to_verified_app() {
        let r = AppRegistry::parse(REG);
        assert_eq!(r.resolve(100001), Some(("alice", "org.atrium.recorder")));
        assert_eq!(r.resolve(100002), Some(("alice", "org.atrium.player")));
    }

    #[test]
    fn unregistered_uid_is_unknown() {
        let r = AppRegistry::parse(REG);
        assert_eq!(r.resolve(0), None, "a non-Portcullis-launched uid is unknown");
    }

    #[test]
    fn parse_round_trips_a_registered_line() {
        // what `register` writes, `parse` reads back.
        let line = "100003 bob org.atrium.editor\n";
        let r = AppRegistry::parse(line);
        assert_eq!(r.resolve(100003), Some(("bob", "org.atrium.editor")));
    }
}
