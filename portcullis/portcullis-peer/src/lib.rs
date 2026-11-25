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
/// Services read this copy (and mount it into their jails).
pub const DEFAULT_REGISTRY: &str = "/var/run/atrium/app-registry";

/// The registry kept across boots, root's only (0700 directory, 0600
/// file). Every binding written to [`DEFAULT_REGISTRY`] is written here
/// first, and [`restore_registry`] puts it back after a boot clears
/// /var/run. So a uid stays bound to one person, app and persona for good,
/// and a reused account never carries files or grants between identities.
pub const PERSISTENT_REGISTRY: &str = "/var/db/atrium/app-registry";

/// The base of the reserved per-app uid range. Each app launch gets a dedicated
/// uid at or above this — never the human's (those are low), never shared. The
/// range sits inside jaild's policy uid window (`1000..=65000`); 50000+ is the
/// reserved app sub-range, well above typical human uids.
pub const APP_UID_BASE: u32 = 50_000;

/// Hold across lookup, allocation and registration. Independent launchers
/// share this file, so a process-local mutex cannot protect the UID binding.
pub fn lock_registry(path: &str) -> io::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new().append(true).create(true).open(format!("{path}.lock"))?;
    lock.lock()?;
    Ok(lock)
}

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
///
/// An unreadable registry is an error: allocating from an empty one would
/// hand out a uid already bound to another app, which would then share that
/// app's grants, overlay and secrets.
pub fn allocate(registry_path: &str) -> io::Result<u32> {
    let reg = AppRegistry::load(registry_path)?;
    Ok(allocate_in(&reg, |uid| match username(uid) {
        None => false,
        Some(name) => name != app_username(uid),
    }))
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
    /// Written by portcullisd for a declared session component (the
    /// root-owned session.d set: the Forum bar, dock, WM). Services that
    /// grant shell roles (frescod's bar layer) trust only these; an app
    /// installed under the same id is never one.
    session: bool,
    /// The persona this instance runs in (personas.md §3), `None` for
    /// Personal. Written as a `persona=<name>` field; a line whose persona
    /// name is invalid is void.
    persona: Option<String>,
    document: Option<u8>,
}

/// A named persona's name (personas.md §2): 1-32 of `[a-z0-9-]`, starting
/// with a letter, never `personal` (Personal is the absence of a name).
/// The name becomes part of paths (`persona-apps/<persona>/<id>`) and jail names,
/// so nothing else is allowed.
/// The reserved persona of the Navigator's sign-in window for a captive
/// portal (atrium-net.md §8.1): an instance of its own (uid, jail, overlay)
/// that a person's persona can never be ([`valid_person_persona`]), started
/// only by portcullisd's `PortalWindow`.
pub const PORTAL_PERSONA: &str = "portal";

/// The reserved persona of a corporate VPN's sign-in window (vpn.md
/// §9.3): like [`PORTAL_PERSONA`], an instance of its own, started only by
/// portcullisd's `VpnSignInWindow`.
pub const VPN_PERSONA: &str = "vpn";

/// A name a person may give one of their personas: [`valid_persona`], and
/// not a reserved one ([`PORTAL_PERSONA`], [`VPN_PERSONA`]).
pub fn valid_person_persona(name: &str) -> bool {
    valid_persona(name) && name != PORTAL_PERSONA && name != VPN_PERSONA
}

pub fn valid_persona(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && name != "personal"
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
    /// `<uid> <user> <app-id> [publisher-id hex | session] [persona=<name>]`;
    /// `#` comments and blanks ignored; a later line for the same uid replaces
    /// an earlier one. A field that is neither 64 hex digits, `session` nor a
    /// valid `persona=` makes the line void: a binding with a garbled
    /// publisher must not read as "unsigned", and one with a garbled persona
    /// must not read as Personal. A session component has no persona.
    pub fn parse(text: &str) -> AppRegistry {
        let mut map = HashMap::new();
        'lines: for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(uid), Some(user), Some(app)) = (it.next(), it.next(), it.next()) {
                let (mut publisher, mut session, mut persona) = (None, false, None);
                let mut kind_seen = false;
                let mut document = None;
                for f in it {
                    if let Some(slot) = f.strip_prefix("document=") {
                        if document.is_some() {
                            continue 'lines;
                        }
                        let Ok(slot) = slot.parse::<u8>() else { continue 'lines };
                        document = Some(slot);
                    } else if let Some(name) = f.strip_prefix("persona=") {
                        if persona.is_some() || !valid_persona(name) {
                            continue 'lines;
                        }
                        persona = Some(name.to_string());
                    } else if kind_seen || persona.is_some() || document.is_some() {
                        continue 'lines; // order: kind first, persona last; one of each
                    } else if f == "session" {
                        session = true;
                        kind_seen = true;
                    } else {
                        match parse_publisher(f) {
                            Some(p) => publisher = Some(p),
                            None => continue 'lines,
                        }
                        kind_seen = true;
                    }
                }
                if session && (persona.is_some() || document.is_some()) {
                    continue;
                }
                if let Ok(uid) = uid.parse::<u32>() {
                    map.insert(uid, Entry { user: user.to_string(), app_id: app.to_string(), publisher, session, persona, document });
                }
            }
        }
        AppRegistry { map }
    }

    /// Read the registry file. A missing file is an empty registry (no app
    /// launched yet); any other error is returned, never an empty registry,
    /// because "not in the registry" means "not an app", and callers grant
    /// a person more than an app.
    ///
    /// A line that is not UTF-8 is void like any other malformed line and
    /// logged; it no longer empties the whole registry, which
    /// `read_to_string` did.
    pub fn load(path: &str) -> io::Result<AppRegistry> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(AppRegistry::default()),
            Err(e) => return Err(e),
        };
        let mut text = String::with_capacity(bytes.len());
        for (n, line) in bytes.split(|&b| b == b'\n').enumerate() {
            match std::str::from_utf8(line) {
                Ok(l) => { text.push_str(l); text.push('\n'); }
                Err(_) => eprintln!("portcullis-peer: {path}: line {} is not UTF-8; ignored", n + 1),
            }
        }
        Ok(AppRegistry::parse(&text))
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

    /// The persona of the instance at `uid`: `Some(None)` for Personal,
    /// `Some(Some(name))` for a named persona, `None` if the uid isn't in the
    /// registry. Services read the persona here, never from the app
    /// (personas.md §3, §7).
    pub fn persona(&self, uid: u32) -> Option<Option<&str>> {
        self.map.get(&uid).map(|e| e.persona.as_deref())
    }

    pub fn document_slot(&self, uid: u32) -> Option<u8> {
        self.map.get(&uid)?.document
    }

    /// The uid of the instance `(user, app_id, persona)` with this publisher,
    /// if any. Each persona's instance has its own uid (personas.md §3).
    pub fn uid_for_instance(&self, user: &str, app_id: &str, publisher: Option<&[u8; 32]>, persona: Option<&str>) -> Option<u32> {
        self.uid_for_document(user, app_id, publisher, persona, None)
    }

    pub fn uid_for_document(&self, user: &str, app_id: &str, publisher: Option<&[u8; 32]>, persona: Option<&str>, document: Option<u8>) -> Option<u32> {
        self.map.iter()
            .find(|(_, e)| !e.session && e.document == document && e.user == user && e.app_id == app_id
                  && e.publisher.as_ref() == publisher && e.persona.as_deref() == persona)
            .map(|(uid, _)| *uid)
    }

    /// The uid of `(user, app_id)`'s instance in `persona`, whatever its
    /// publisher (for answering "already running"; launching must use
    /// [`Self::uid_for_instance`]).
    pub fn uid_for_persona(&self, user: &str, app_id: &str, persona: Option<&str>) -> Option<u32> {
        self.map.iter()
            .find(|(_, e)| !e.session && e.document.is_none() && e.user == user && e.app_id == app_id && e.persona.as_deref() == persona)
            .map(|(uid, _)| *uid)
    }

    /// `(user, app_id)`'s instances in named personas, `(uid, persona)`,
    /// in persona order (Personal's is [`Self::uid_for`]).
    pub fn named_instances(&self, user: &str, app_id: &str) -> Vec<(u32, &str)> {
        let mut v: Vec<(u32, &str)> = self.map.iter()
            .filter(|(_, e)| !e.session && e.document.is_none() && e.user == user && e.app_id == app_id)
            .filter_map(|(uid, e)| e.persona.as_deref().map(|p| (*uid, p)))
            .collect();
        v.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)));
        v
    }

    /// Every uid of `user`'s persona `persona` (its app instances and
    /// workers), with the app each is for.
    pub fn persona_uids(&self, user: &str, persona: &str) -> Vec<(u32, &str)> {
        self.map.iter()
            .filter(|(_, e)| !e.session && e.user == user && e.persona.as_deref() == Some(persona))
            .map(|(uid, e)| (*uid, e.app_id.as_str()))
            .collect()
    }

    /// Every app uid of `user`, in every persona (not session components).
    pub fn user_uids(&self, user: &str) -> Vec<u32> {
        let mut v: Vec<u32> = self.map.iter().filter(|(_, e)| !e.session && e.user == user).map(|(uid, _)| *uid).collect();
        v.sort_unstable();
        v
    }

    /// Whether `uid` is a declared session component portcullisd launched
    /// (see [`register_session`]).
    pub fn is_session(&self, uid: u32) -> bool {
        self.map.get(&uid).is_some_and(|e| e.session)
    }

    /// The session-component uid bound to `(user, component_id)`, if any.
    pub fn uid_for_session(&self, user: &str, component_id: &str) -> Option<u32> {
        self.map.iter()
            .find(|(_, e)| e.session && e.user == user && e.app_id == component_id)
            .map(|(uid, _)| *uid)
    }

    /// The Personal app uid already bound to `(user, app_id)`, if any (never a
    /// session component's: an app named like one must not inherit it; never
    /// a named persona's instance, which is another identity). Lets a re-launch REUSE
    /// the app's existing dedicated uid (stable identity for services that
    /// peer-cred it) instead of leaking a fresh uid + passwd entry every time.
    pub fn uid_for(&self, user: &str, app_id: &str) -> Option<u32> {
        self.map.iter()
            .find(|(_, e)| !e.session && e.document.is_none() && e.persona.is_none() && e.user == user && e.app_id == app_id)
            .map(|(uid, _)| *uid)
    }

    /// As [`AppRegistry::uid_for`], but only a binding with the same publisher.
    /// The launcher uses this: an app id re-installed under another publisher's
    /// key gets a NEW uid, so it inherits nothing keyed by the old one.
    pub fn uid_for_publisher(&self, user: &str, app_id: &str, publisher: Option<&[u8; 32]>) -> Option<u32> {
        self.uid_for_instance(user, app_id, publisher, None)
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
    register_instance(path, uid, user, app_id, publisher, None)
}

/// As [`register_with_publisher`], for the instance in `persona` (`None`:
/// Personal). Refuses an invalid persona name rather than writing a line
/// every reader would void.
pub fn register_instance(
    path: &str,
    uid: u32,
    user: &str,
    app_id: &str,
    publisher: Option<&[u8; 32]>,
    persona: Option<&str>,
) -> io::Result<()> {
    register_document(path, uid, user, app_id, publisher, persona, None)
}

pub fn register_document(
    path: &str,
    uid: u32,
    user: &str,
    app_id: &str,
    publisher: Option<&[u8; 32]>,
    persona: Option<&str>,
    document: Option<u8>,
) -> io::Result<()> {
    if persona.is_some_and(|p| !valid_persona(p)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid persona name"));
    }
    let mut line = format!("{uid} {user} {app_id}");
    if let Some(p) = publisher {
        line.push(' ');
        line.push_str(&publisher_hex(p));
    }
    if let Some(p) = persona {
        line.push_str(" persona=");
        line.push_str(p);
    }
    if let Some(slot) = document {
        line.push_str(&format!(" document={slot}"));
    }
    append_binding(path, &line)
}

/// Append a declared session component's binding (portcullisd only, for
/// the root-owned session.d set): marked `session`, so shell roles can be
/// granted to it and never to an app installed under the same id.
pub fn register_session(path: &str, uid: u32, user: &str, component_id: &str) -> io::Result<()> {
    append_binding(path, &format!("{uid} {user} {component_id} session"))
}

/// Append one binding line to `path`; for the platform registry, to the
/// kept copy first. A binding that can't be kept is not made: the next
/// boot could otherwise hand its uid to another identity.
fn append_binding(path: &str, line: &str) -> io::Result<()> {
    if path == DEFAULT_REGISTRY {
        append_line_kept(std::path::Path::new(PERSISTENT_REGISTRY), line)?;
    }
    append_line(std::path::Path::new(path), line)
}

fn append_line(path: &std::path::Path, line: &str) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

/// Append to a root-only file in a root-only directory, synced.
fn append_line_kept(path: &std::path::Path, line: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    writeln!(f, "{line}")?;
    f.sync_data()
}

/// After a boot: put the kept registry back at [`DEFAULT_REGISTRY`] when
/// that is gone or empty, before anything allocates. Call it holding
/// [`lock_registry`] on [`DEFAULT_REGISTRY`]. A kept copy that can't be
/// read is an error, not an empty registry: starting empty would reuse
/// every bound uid.
pub fn restore_registry() -> io::Result<RestoreOutcome> {
    restore_registry_at(std::path::Path::new(PERSISTENT_REGISTRY), std::path::Path::new(DEFAULT_REGISTRY))
}

/// What [`restore_registry`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The registry was already there (portcullisd restarted in this boot).
    Present,
    /// Put back from the kept copy.
    Restored,
    /// No kept copy yet (a first boot, or before the copy existed).
    Fresh,
}

fn restore_registry_at(kept: &std::path::Path, live: &std::path::Path) -> io::Result<RestoreOutcome> {
    // The boot's rc (atrium-portcullisd-bootstrap) makes an empty live file
    // before services nullfs-mount it, so empty means "cleared by the boot"
    // as much as absent does. It is filled in place: a rename would leave
    // those mounts on the old, empty file.
    let empty = match std::fs::symlink_metadata(live) {
        Ok(m) if m.file_type().is_file() && m.len() == 0 => true,
        Ok(_) => return Ok(RestoreOutcome::Present),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    let bytes = match std::fs::read(kept) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RestoreOutcome::Fresh),
        Err(e) => return Err(e),
    };
    if empty {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(live)?;
        // Checked again on the open file: it must still be the empty file.
        if f.metadata()?.len() != 0 {
            return Ok(RestoreOutcome::Present);
        }
        f.write_all(&bytes)?;
        f.sync_all()?;
        return Ok(RestoreOutcome::Restored);
    }
    if let Some(dir) = live.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = live.with_extension("restore");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, live)?;
    Ok(RestoreOutcome::Restored)
}

/// As [`AppRegistry::uid_for_session`], reading the registry file.
pub fn uid_for_session(path: &str, user: &str, component_id: &str) -> Option<u32> {
    AppRegistry::load(path).ok().and_then(|r| r.uid_for_session(user, component_id))
}

/// As [`AppRegistry::uid_for_publisher`], reading the registry file.
pub fn uid_for_app_publisher(path: &str, user: &str, app_id: &str, publisher: Option<&[u8; 32]>) -> Option<u32> {
    AppRegistry::load(path).ok().and_then(|r| r.uid_for_publisher(user, app_id, publisher))
}

/// As [`AppRegistry::uid_for_instance`], reading the registry file.
pub fn uid_for_app_instance(path: &str, user: &str, app_id: &str, publisher: Option<&[u8; 32]>, persona: Option<&str>) -> Option<u32> {
    AppRegistry::load(path).ok().and_then(|r| r.uid_for_instance(user, app_id, publisher, persona))
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

    /// The active session's lock: the file names the locked human, and is
    /// empty or absent while the session is in use. Beside the seat, for the
    /// same reason. Readers act on it at once: removable media is unmounted
    /// while it names the active user (device-assignment.md §4.6).
    pub const LOCKED: &str = "/atrium/sockets/ostiarius/session-locked";

    /// The lock file that goes with the seat file at `seat_path`.
    pub fn locked_path(seat_path: &str) -> std::path::PathBuf {
        std::path::Path::new(seat_path).with_file_name("session-locked")
    }

    /// Mark `user`'s session locked, or (`None`) in use.
    pub fn set_locked_at(path: &std::path::Path, user: Option<&str>) -> io::Result<()> {
        std::fs::write(path, format!("{}\n", user.unwrap_or("")))
    }

    /// Is the session at `seat_path` locked? Only a lock naming the seat's
    /// own user counts, so a stale lock from an earlier session is ignored.
    pub fn is_locked_at(seat_path: &str) -> bool {
        match (active_at(seat_path), active_at(&locked_path(seat_path).to_string_lossy())) {
            (Some(a), Some(l)) => a == l,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_lock_names_its_user() {
        let d = std::env::temp_dir().join(format!("peer-seat-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let seat_path = d.join("active-session");
        let seat_s = seat_path.to_str().unwrap();
        let lock = seat::locked_path(seat_s);
        assert_eq!(lock, d.join("session-locked"));
        assert!(!seat::is_locked_at(seat_s), "no seat, no lock");
        seat::set_active_at(seat_s, "alice").unwrap();
        assert!(!seat::is_locked_at(seat_s));
        seat::set_locked_at(&lock, Some("alice")).unwrap();
        assert!(seat::is_locked_at(seat_s));
        seat::set_active_at(seat_s, "bob").unwrap();
        assert!(!seat::is_locked_at(seat_s), "alice's lock is not bob's");
        seat::set_locked_at(&lock, None).unwrap();
        assert!(!seat::is_locked_at(seat_s));
        let _ = std::fs::remove_dir_all(&d);
    }

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
    fn session_components_are_marked_and_never_reused_by_apps() {
        let r = AppRegistry::parse("50000 alice org.atrium.forum-bar session\n50001 alice org.atrium.forum-bar\n");
        assert!(r.is_session(50000) && !r.is_session(50001) && !r.is_session(7));
        assert_eq!(r.resolve(50000), Some(("alice", "org.atrium.forum-bar")));
        assert_eq!(r.uid_for_session("alice", "org.atrium.forum-bar"), Some(50000));
        // An app launched under the bar's id gets its own uid, not the session's.
        assert_eq!(r.uid_for("alice", "org.atrium.forum-bar"), Some(50001));
        assert_eq!(r.uid_for_publisher("alice", "org.atrium.forum-bar", None), Some(50001));
        let only = AppRegistry::parse("50000 alice org.atrium.forum-bar session\n");
        assert_eq!(only.uid_for("alice", "org.atrium.forum-bar"), None);
        assert_eq!(only.uid_for_publisher("alice", "org.atrium.forum-bar", None), None);
    }

    #[test]
    fn a_garbled_publisher_voids_the_line() {
        let r = AppRegistry::parse("50001 alice org.x.mail nothex\n50002 alice org.x.b 00 extra\n");
        assert_eq!(r.resolve(50001), None);
        assert_eq!(r.resolve(50002), None);
    }

    #[test]
    fn persona_instances_have_their_own_uids() {
        let d = std::env::temp_dir().join(format!("peer-persona-{}", std::process::id()));
        let path = d.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        register_with_publisher(path, 50000, "alice", "org.x.mail", Some(&[0xab; 32])).unwrap();
        register_instance(path, 50001, "alice", "org.x.mail", Some(&[0xab; 32]), Some("work")).unwrap();
        register_instance(path, 50002, "alice", "org.x.chat", None, Some("work")).unwrap();
        assert!(register_instance(path, 50003, "alice", "org.x.chat", None, Some("Work")).is_err());
        assert!(register_instance(path, 50003, "alice", "org.x.chat", None, Some("personal")).is_err());
        let r = AppRegistry::load(path).unwrap();
        assert_eq!(r.persona(50000), Some(None));
        assert_eq!(r.persona(50001), Some(Some("work")));
        assert_eq!(r.persona(50002), Some(Some("work")));
        assert_eq!(r.persona(9), None);
        assert_eq!(r.resolve(50001), Some(("alice", "org.x.mail")));
        // Personal and Work are different identities.
        assert_eq!(r.uid_for_publisher("alice", "org.x.mail", Some(&[0xab; 32])), Some(50000));
        assert_eq!(r.uid_for_instance("alice", "org.x.mail", Some(&[0xab; 32]), Some("work")), Some(50001));
        assert_eq!(r.uid_for_instance("alice", "org.x.mail", Some(&[0xab; 32]), Some("home")), None);
        // A Work-only app has no Personal uid to reuse.
        assert_eq!(r.uid_for("alice", "org.x.chat"), None);
        assert_eq!(r.uid_for_instance("alice", "org.x.chat", None, Some("work")), Some(50002));
        assert_eq!(r.named_instances("alice", "org.x.mail"), vec![(50001, "work")]);
        assert!(r.named_instances("bob", "org.x.mail").is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn workbook_slots_have_dedicated_app_uids() {
        let reg = AppRegistry::parse(
            "50000 alice sheets persona=work\n50001 alice sheets persona=work document=0\n\
             50002 alice sheets persona=work document=1\n",
        );
        assert_eq!(reg.resolve(50002), Some(("alice", "sheets")));
        assert_eq!(reg.persona(50002), Some(Some("work")));
        assert_eq!(reg.document_slot(50002), Some(1));
        assert_eq!(reg.uid_for_persona("alice", "sheets", Some("work")), Some(50000));
        assert_eq!(reg.uid_for_document("alice", "sheets", None, Some("work"), Some(0)), Some(50001));
        assert_eq!(reg.uid_for_document("alice", "sheets", None, Some("work"), Some(1)), Some(50002));
        assert_eq!(reg.named_instances("alice", "sheets"), vec![(50000, "work")]);
    }

    #[test]
    fn a_garbled_persona_voids_the_line() {
        let r = AppRegistry::parse(concat!(
            "50001 alice a persona=Work\n",
            "50002 alice a persona=\n",
            "50003 alice a persona=work persona=home\n",
            "50004 alice a persona=work session\n",
            "50005 alice a session persona=work\n",
            "50006 alice a persona=work\n",
            "50007 alice a persona=../x\n",
        ));
        for uid in 50001..=50005 {
            assert_eq!(r.resolve(uid), None, "uid {uid}");
        }
        assert_eq!(r.persona(50006), Some(Some("work")));
        assert_eq!(r.resolve(50007), None);
        assert!(valid_persona(PORTAL_PERSONA) && !valid_person_persona(PORTAL_PERSONA) && valid_person_persona("work"));
        assert!(valid_persona(VPN_PERSONA) && !valid_person_persona(VPN_PERSONA));
        assert!(valid_persona("work") && valid_persona("acme-2") && !valid_persona("2acme")
            && !valid_persona("a.b") && !valid_persona(&"a".repeat(33)));
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

    /// Review 2026-11-24 (H4): one non-UTF-8 byte used to empty the whole
    /// registry (read_to_string failed, callers took the default), and
    /// allocate then reused bound uids. Now only that line is void, a
    /// missing file is empty, and any other read error is an error.
    #[test]
    fn a_bad_registry_fails_closed() {
        let d = std::env::temp_dir().join(format!("peer-h4-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("registry");
        let p = path.to_str().unwrap();
        let mut bytes = b"50000 alice org.a.one\n".to_vec();
        bytes.extend_from_slice(b"50001 alice org.\xff.bad\n50002 alice org.a.two\n");
        std::fs::write(&path, &bytes).unwrap();
        let r = AppRegistry::load(p).unwrap();
        assert_eq!(r.resolve(50000), Some(("alice", "org.a.one")));
        assert_eq!(r.resolve(50001), None);
        assert_eq!(r.resolve(50002), Some(("alice", "org.a.two")));
        assert!(AppRegistry::load(d.join("absent").to_str().unwrap()).unwrap().resolve(50000).is_none());
        /* A directory where the file should be: an error, for load and for
         * allocate, never an empty registry. */
        let dir = d.join("dir-not-file");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(AppRegistry::load(dir.to_str().unwrap()).is_err());
        assert!(allocate(dir.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod kept_registry_tests {
    use super::{append_line, append_line_kept, restore_registry_at, AppRegistry, RestoreOutcome};

    #[test]
    fn a_boot_keeps_every_binding() {
        let d = tempfile_dir();
        let kept = d.join("db/atrium/app-registry");
        let live = d.join("run/atrium/app-registry");
        assert_eq!(restore_registry_at(&kept, &live).unwrap(), RestoreOutcome::Fresh);
        for line in ["50000 alice org.atrium.mail", "50001 bob org.atrium.mail persona=work"] {
            append_line_kept(&kept, line).unwrap();
            std::fs::create_dir_all(live.parent().unwrap()).unwrap();
            append_line(&live, line).unwrap();
        }
        // A boot clears /var/run.
        std::fs::remove_dir_all(d.join("run")).unwrap();
        assert_eq!(restore_registry_at(&kept, &live).unwrap(), RestoreOutcome::Restored);
        let reg = AppRegistry::load(live.to_str().unwrap()).unwrap();
        assert_eq!(reg.resolve(50000), Some(("alice", "org.atrium.mail")));
        assert_eq!(reg.resolve(50001).map(|(u, _)| u), Some("bob"));
        // A restart in the same boot leaves it alone.
        assert_eq!(restore_registry_at(&kept, &live).unwrap(), RestoreOutcome::Present);
        // The boot's rc leaves an empty live file that services have
        // already mounted: it is filled in place, the same inode.
        std::fs::write(&live, b"").unwrap();
        use std::os::unix::fs::MetadataExt;
        let ino = std::fs::metadata(&live).unwrap().ino();
        assert_eq!(restore_registry_at(&kept, &live).unwrap(), RestoreOutcome::Restored);
        assert_eq!(std::fs::metadata(&live).unwrap().ino(), ino);
        let reg = AppRegistry::load(live.to_str().unwrap()).unwrap();
        assert_eq!(reg.resolve(50000), Some(("alice", "org.atrium.mail")));
        assert_eq!(restore_registry_at(&kept, &live).unwrap(), RestoreOutcome::Present);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(kept.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn an_unreadable_kept_copy_is_an_error() {
        let d = tempfile_dir();
        let kept = d.join("kept");
        std::fs::create_dir(&kept).unwrap(); // a directory: read fails, not NotFound
        assert!(restore_registry_at(&kept, &d.join("live")).is_err());
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("peer-kept-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

