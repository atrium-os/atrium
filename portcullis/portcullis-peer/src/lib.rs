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

/// The base of the reserved per-app uid range. Each app launch gets a dedicated
/// uid at or above this — never the human's (those are low), never shared. The
/// range sits inside jaild's policy uid window (`1000..=65000`); 50000+ is the
/// reserved app sub-range, well above typical human uids.
pub const APP_UID_BASE: u32 = 50_000;

/// Allocate a dedicated uid for a new app launch: the lowest uid ≥
/// [`APP_UID_BASE`] that is neither bound in the registry nor already a host
/// account other than an app account. Portcullis calls this, then
/// [`register`]s the binding.
///
/// The second rule matters: system daemons (`_memoryd` 50090, `_memfed`
/// 50091, `_ostiarius` 50093, `_stoad` 50094) have uids inside this range.
/// Without it, the 91st app launched on a machine would be given 50090 and
/// run as `_memoryd`, and services that trust that uid (Arca trusts
/// `_ostiarius` to unlock stores) would trust the app.
pub fn allocate(registry_path: &str) -> u32 {
    let reg = AppRegistry::load(registry_path).unwrap_or_default();
    allocate_in(&reg, |uid| match username(uid) {
        None => false,
        Some(name) => name != app_username(uid),
    })
}

fn allocate_in(reg: &AppRegistry, system_account: impl Fn(u32) -> bool) -> u32 {
    let mut uid = APP_UID_BASE;
    while reg.map.contains_key(&uid) || system_account(uid) {
        uid += 1;
    }
    uid
}

// ── getpeereid: the unforgeable handle ────────────────────────────────────────

/// The connected peer's `(uid, gid)` — the kernel's authenticated answer.
#[cfg(target_os = "linux")]
pub fn uid_gid(fd: RawFd) -> io::Result<(u32, u32)> {
    // Linux has no getpeereid; SO_PEERCRED gives the same kernel answer.
    // Only for building and testing off-target (the cloud containers).
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len)
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((cred.uid, cred.gid))
}

/// The connected peer's `(uid, gid)` — the kernel's authenticated answer.
#[cfg(not(target_os = "linux"))]
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
/// This process's own user name, from its REAL uid.
///
/// The counterpart to [`username`] for a client that needs to agree with what
/// portcullisd will see: the daemon identifies callers with getpeereid(2), so
/// anything deriving the user from $USER can disagree with the daemon about
/// whose policy is in play (see the note on the CLI's `current_user`).
pub fn current_username() -> Option<String> {
    username(unsafe { libc::getuid() })
}

pub fn username(uid: u32) -> Option<String> {
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) };
    Some(name.to_string_lossy().into_owned())
}

// ── the launch registry: uid → (owning user, app-id, publisher) ───────────────

/// One registry binding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    user: String,
    app_id: String,
    /// The manifest's verified publisher id (SHA-256 of the signing key's SPKI
    /// DER, `portcullis_sig::publisher_id`), or `None` for an app launched
    /// unsigned (trust unconfigured) or a system session component.
    publisher: Option<[u8; 32]>,
}

#[derive(Default)]
pub struct AppRegistry {
    map: HashMap<u32, Entry>,
}

fn parse_publisher(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn publisher_hex(p: &[u8; 32]) -> String {
    p.iter().map(|b| format!("{b:02x}")).collect()
}

impl AppRegistry {
    /// Parse the registry text. One app per line:
    /// `<uid> <user> <app-id> [publisher-id hex]`; `#` comments and blanks
    /// ignored; a later line for the same uid replaces an earlier one. A
    /// fourth field that isn't 64 hex digits makes the line void: a binding
    /// with a garbled publisher must not read as "unsigned".
    pub fn parse(text: &str) -> AppRegistry {
        let mut map = HashMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(uid), Some(user), Some(app)) = (it.next(), it.next(), it.next()) {
                let publisher = match it.next() {
                    None => None,
                    Some(h) => match parse_publisher(h) {
                        Some(p) => Some(p),
                        None => continue,
                    },
                };
                if it.next().is_some() {
                    continue;
                }
                if let Ok(uid) = uid.parse::<u32>() {
                    map.insert(uid, Entry { user: user.to_string(), app_id: app.to_string(), publisher });
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
        self.map.get(&uid).map(|e| (e.user.as_str(), e.app_id.as_str()))
    }

    /// The verified publisher id for a uid: `Some(Some(id))` for a signed app,
    /// `Some(None)` for one launched without a publisher, `None` if the uid
    /// isn't in the registry.
    pub fn publisher(&self, uid: u32) -> Option<Option<[u8; 32]>> {
        self.map.get(&uid).map(|e| e.publisher)
    }

    /// The uid already bound to `(user, app_id)`, if any. Lets a re-launch REUSE
    /// the app's existing dedicated uid (stable identity for services that
    /// peer-cred it) instead of leaking a fresh uid + passwd entry every time.
    pub fn uid_for(&self, user: &str, app_id: &str) -> Option<u32> {
        self.map.iter()
            .find(|(_, e)| e.user == user && e.app_id == app_id)
            .map(|(uid, _)| *uid)
    }

    /// As [`AppRegistry::uid_for`], but only a binding with the same publisher.
    /// The launcher uses this: an app id re-installed under another publisher's
    /// key gets a NEW uid, so it inherits nothing keyed by the old one.
    pub fn uid_for_publisher(&self, user: &str, app_id: &str, publisher: Option<&[u8; 32]>) -> Option<u32> {
        self.map.iter()
            .find(|(_, e)| e.user == user && e.app_id == app_id && e.publisher.as_ref() == publisher)
            .map(|(uid, _)| *uid)
    }
}

/// As [`AppRegistry::uid_for`], reading the registry file (missing file → `None`).
pub fn uid_for_app(path: &str, user: &str, app_id: &str) -> Option<u32> {
    AppRegistry::load(path).ok().and_then(|r| r.uid_for(user, app_id))
}

/// The conventional host username for a dedicated per-app uid. A nologin
/// "nobody"-class account (`pw useradd <name> -u <uid> -d /nonexistent -s
/// /usr/sbin/nologin`) whose only job is to be the unprivileged identity an app
/// runs as — never root, never a human's account.
pub fn app_username(uid: u32) -> String {
    format!("atrium-app-{uid}")
}

/// Append a binding to the registry file (the **write** side — Portcullis calls
/// this when it launches an app at `uid`). Append-only; one line per launch.
pub fn register(path: &str, uid: u32, user: &str, app_id: &str) -> io::Result<()> {
    register_with_publisher(path, uid, user, app_id, None)
}

/// As [`register`], recording the manifest's verified publisher id.
pub fn register_with_publisher(
    path: &str,
    uid: u32,
    user: &str,
    app_id: &str,
    publisher: Option<&[u8; 32]>,
) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    match publisher {
        None => writeln!(f, "{uid} {user} {app_id}"),
        Some(p) => writeln!(f, "{uid} {user} {app_id} {}", publisher_hex(p)),
    }
}

/// As [`AppRegistry::uid_for_publisher`], reading the registry file.
pub fn uid_for_app_publisher(path: &str, user: &str, app_id: &str, publisher: Option<&[u8; 32]>) -> Option<u32> {
    AppRegistry::load(path).ok().and_then(|r| r.uid_for_publisher(user, app_id, publisher))
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

// ── Seat: which human session owns the shared engines RIGHT NOW ───────────────
//
// The desktop is per-session (each human's WM, Choragus, shell, data), but the
// engines (lyrad/DAC, Fresco/scanout, input) are shared — one device. The seat
// says which session is bound to them now. COMMON case: the boot-time login
// (vestibulum) sets the active session; it owns the hardware until logout — one
// active session, so this is just set-at-login / clear-at-logout. Fast-user-
// switching (uncommon) flips it. A service asks `is_active(owner)` to decide
// whether to drive the engine for that session's apps.
pub mod seat {
    use std::io;

    /// The active session = the human user currently bound to the shared engines.
    ///
    /// Lives in ostiarius's federated runtime dir, NOT `/var/run/atrium` — the
    /// seat WRITER (ostiarius) is jailed + non-root, so the seat must live where
    /// `_ostiarius` can write it. `/var/run/atrium` holds root-only state
    /// (jaild.sock, the launch registry) and is never mounted into a non-TCB
    /// jail. Seat-aware readers (Choragus/Fresco, in their own jails) mount this
    /// subdir read-only — the same per-cap-dir pattern as the IPC sockets.
    pub const ACTIVE_SESSION: &str = "/atrium/sockets/ostiarius/active-session";

    /// Bind a session to the engines (login calls this; FUS re-calls it).
    pub fn set_active(user: &str) -> io::Result<()> {
        set_active_at(ACTIVE_SESSION, user)
    }

    /// The currently-active human session, if any.
    pub fn active() -> Option<String> {
        active_at(ACTIVE_SESSION)
    }

    /// Is `user`'s session the one bound to the engines now?
    pub fn is_active(user: &str) -> bool {
        active().as_deref() == Some(user)
    }

    // path-parameterized cores (the fixed-path API above delegates here; tests
    // drive these against a temp file).
    pub fn set_active_at(path: &str, user: &str) -> io::Result<()> {
        std::fs::write(path, format!("{user}\n"))
    }
    pub fn active_at(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_skips_system_accounts_in_the_range() {
        let reg = AppRegistry::parse("50000 alice a\n50001 alice b\n");
        // 50002 is a system daemon's uid: skipped.
        assert_eq!(allocate_in(&reg, |u| u == 50002 || u == 50003), 50004);
        assert_eq!(allocate_in(&reg, |_| false), 50002);
    }

    #[test]
    fn registry_records_the_publisher() {
        let d = std::env::temp_dir().join(format!("peer-pub-{}", std::process::id()));
        let path = d.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        register(path, 50000, "alice", "org.x.sys").unwrap();
        register_with_publisher(path, 50001, "alice", "org.x.mail", Some(&[0xab; 32])).unwrap();
        let r = AppRegistry::load(path).unwrap();
        assert_eq!(r.publisher(50000), Some(None));
        assert_eq!(r.publisher(50001), Some(Some([0xab; 32])));
        assert_eq!(r.publisher(1), None);
        assert_eq!(r.uid_for_publisher("alice", "org.x.mail", Some(&[0xab; 32])), Some(50001));
        // Same app id, another publisher: no reuse.
        assert_eq!(r.uid_for_publisher("alice", "org.x.mail", Some(&[0xcd; 32])), None);
        assert_eq!(r.uid_for_publisher("alice", "org.x.mail", None), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_garbled_publisher_voids_the_line() {
        let r = AppRegistry::parse("50001 alice org.x.mail nothex\n50002 alice org.x.b 00 extra\n");
        assert_eq!(r.resolve(50001), None);
        assert_eq!(r.resolve(50002), None);
    }

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
    fn seat_binds_and_reads_back_the_active_session() {
        let p = format!("/tmp/atrium-seat-test-{}", std::process::id());
        let _ = std::fs::remove_file(&p);
        assert_eq!(seat::active_at(&p), None, "no file → no active session");
        seat::set_active_at(&p, "alice").unwrap();
        assert_eq!(seat::active_at(&p).as_deref(), Some("alice"));
        seat::set_active_at(&p, "bob").unwrap(); // a switch (FUS)
        assert_eq!(seat::active_at(&p).as_deref(), Some("bob"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn parse_round_trips_a_registered_line() {
        // what `register` writes, `parse` reads back.
        let line = "100003 bob org.atrium.editor\n";
        let r = AppRegistry::parse(line);
        assert_eq!(r.resolve(100003), Some(("bob", "org.atrium.editor")));
    }
}
