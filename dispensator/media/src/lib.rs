//! Removable media on Aqueduct (`CLASS_MEDIA`, device-assignment.md §6.6).
//!
//! dispensatord serves each USB-stick partition for the active, unlocked
//! session (its `media` module). This is how the session learns about them
//! and ejects one:
//!
//! | op | request | reply (`IS_RESPONSE`) |
//! |---|---|---|
//! | `LIST` 0x00 | empty | `u8 status`, then postcard `Vec<Volume>` |
//! | `EJECT` 0x01 | postcard [`Eject`] | `u8 status`, then postcard `String` (a message to show) |
//! | `GRANT` 0x02 | postcard [`Grant`] | `u8 status`, then postcard `String` (where it is in the jail, or why not) |
//! | `REVOKE` 0x03 | postcard `String` (the jail) | `u8 status`, then postcard `String` |
//! | `ATTACH_ALL` 0x04 | empty | `u8 status`, then postcard `String`: the file manager asks for every stick, now and as they come, in its own jail |
//! | `OPEN` 0x05 | postcard [`Open`] | `u8 status`, then postcard `String`; with `OK`, the open file as the message's one descriptor |
//! | `PICKED` 0x06 | postcard [`Picked`] | `u8 status`, then postcard `String` (where it is in the app's jail, or why not): the system picker puts the folder the user picked into the app's jail |
//! | `CONNECT` 0x07 | postcard [`Connect`] | `u8 status`, then postcard `String`: the share's `part` (`share-<uid>`), or why not. The file manager connects a network share it saved in Arca (network-shares.md §6.1); `EJECT` of that part disconnects |
//! | `LEARN` 0x08 | postcard [`Learn`] | `u8 status`, then postcard `String`: SFTP, the server's host key fingerprint (`SHA256:...`) for the user to accept; WebDAV, lines `trusted` or `untrusted <why>`, the certificate key's fingerprint, `subject`, `issuer`, `expires`; SMB, `smb 3.1.1 <cipher>`, the fingerprint of the server's GUID, `guid <text>`; or why not (a server below the SMB floor: why) |
//! | `FORMAT` 0x0A | postcard [`Format`] | `u8 status`, then postcard `String`: erase a stick and make it a sealed one for a persona (tessera-encryption.md §7.1); the key never leaves Arca and the kernel |
//! | `ADOPT` 0x0B | postcard [`Adopt`] | `u8 status`, then postcard `String`: open a Locked stick with its recovery code (tessera-encryption.md §7.4); arcad unwraps its key, keeps it in the persona or loads it for this session only |
//! | `OPEN_OLDER` 0x0C | postcard [`Eject`] (the part) | `u8 status`, then postcard `String`: "Open read-only?" for a Locked stick that is an older copy of one of the person's (tessera-encryption.md §8); Sigillum verifies the person first |
//! | `MAKE_CURRENT` 0x0D | postcard [`Eject`] (the part) | `u8 status`, then postcard `String`: "Use this copy as current" for an older copy opened read-only (§8): Sigillum verifies the person, arcad lowers its key's high-water mark to this copy, and it is mounted writable |
//! | `FORGET` 0x0E | postcard [`Eject`] (the part) | `u8 status`, then postcard `String`: "Forget this stick's key" for an open sealed stick of the person's (tessera-encryption.md §9): Sigillum verifies the person, arcad deletes the key and the kernel drops it; the stick is unmounted and shows Locked (openable again only with its recovery code, if it has one) |
//! | `REMOVE_CODE` 0x0F | postcard [`Eject`] (the part) | `u8 status`, then postcard `String`: remove an open sealed stick's recovery code (tessera-encryption.md §7.4): the kernel zeroes its slot in both superblock copies, so only this machine's key opens it from then on |
//! | `SUBSCRIBE` 0x10 | empty | `u8 status`; then `EV` 0x80 events (`ASYNC_EVENT`), postcard [`Event`]: an `Added` per volume served now, then changes |
//!
//! Only the session's own user and its desktop components (Forum) may ask;
//! the daemon decides who that is ([`server::Host::allowed`]) and checks it
//! again on every request and event, so a subscriber is dropped when the
//! session changes. The files themselves never go over Aqueduct: they stay a
//! FUSE mount. `GRANT` puts a folder of a served stick (or all of it) into an
//! app's jail for as long as that jail runs (§4.8); only root may ask (the
//! picker and the file manager's launch go through portcullisd).

use serde::{Deserialize, Serialize};

pub use aqueduct::classes::CLASS_MEDIA as CLASS;

pub mod op {
    pub const LIST: u16 = 0x00;
    pub const EJECT: u16 = 0x01;
    pub const GRANT: u16 = 0x02;
    pub const REVOKE: u16 = 0x03;
    pub const ATTACH_ALL: u16 = 0x04;
    pub const OPEN: u16 = 0x05;
    pub const PICKED: u16 = 0x06;
    pub const CONNECT: u16 = 0x07;
    pub const LEARN: u16 = 0x08;
    /// Whether a connected share comes back at login, asked or set.
    pub const AT_LOGIN: u16 = 0x09;
    pub const FORMAT: u16 = 0x0A;
    pub const ADOPT: u16 = 0x0B;
    pub const OPEN_OLDER: u16 = 0x0C;
    pub const MAKE_CURRENT: u16 = 0x0D;
    pub const FORGET: u16 = 0x0E;
    pub const REMOVE_CODE: u16 = 0x0F;
    pub const SUBSCRIBE: u16 = 0x10;
    pub const EV: u16 = 0x80;
}

pub mod status {
    pub const OK: u8 = 0;
    pub const MALFORMED: u8 = 1;
    /// Not the session's user or one of its desktop components.
    pub const NOT_ALLOWED: u8 = 2;
    pub const NOT_FOUND: u8 = 3;
    /// A file is open on it: close it first.
    pub const BUSY: u8 = 4;
    pub const FAILED: u8 = 5;
}

/// One served partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// The partition (`da0s1`): the name to eject.
    pub part: String,
    /// Where it is mounted on the host (`/media/da0s1`).
    pub mountpoint: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub read_only: bool,
    /// What to call it: the name a sealed stick was given at format (empty
    /// for a plain stick or an unnamed one, which the file manager names by
    /// its size), `user@host:path` for a network share.
    pub label: String,
    /// A trusted (sealed) stick: the persona whose key opened it, for its
    /// chip (personas.md §5.1). Empty otherwise.
    pub persona: String,
    /// A sealed stick this session holds no key for: shown, not mounted
    /// (`mountpoint` empty). It can be erased with `FORMAT`.
    pub locked: bool,
    /// A Locked stick with a recovery slot: `ADOPT` can open it.
    pub recovery: bool,
    /// A Locked stick that is the person's, but older than the last time
    /// it was used here (§8): `OPEN_OLDER` opens it read-only. `persona`
    /// names whose. Once opened (mounted, `read_only`), still `older`:
    /// `MAKE_CURRENT` makes it the current copy.
    pub older: bool,
}

/// Erase `part` and make it a sealed stick for `persona` (root or the
/// session's user). `label` is what Curia and Scrinium call its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    pub part: String,
    pub persona: String,
    pub label: String,
    /// Also make a recovery code (§7.4): the reply's message carries it,
    /// to be shown once.
    pub recovery: bool,
}

/// Open the Locked stick `part` with its recovery `code`, as `persona`'s:
/// `keep` stores its key in that persona ("Keep on this machine"),
/// otherwise it's loaded for this session only. The session's user only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adopt {
    pub part: String,
    pub persona: String,
    pub keep: bool,
    /// The code as typed (root's command line); empty when `handle` is set.
    pub code: String,
    /// What Arca's recovery-code field gave the app for what was typed
    /// into it (zero: none). The app never sees the code; arcad redeems
    /// the handle.
    pub handle: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eject {
    pub part: String,
}

/// Put `path` (`/` for the whole stick) of `part` into `jail`, owned there
/// by `uid`:`gid` (the app's). It lands at `/media/<part>` in the jail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub part: String,
    pub path: String,
    pub jail: String,
    pub uid: u32,
    pub gid: u32,
    pub read_only: bool,
}

/// Open one file of a served stick for the picker (root only): the
/// descriptor goes to the app, never the path (§4.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    pub part: String,
    pub path: String,
    pub write: bool,
}

/// The system picker (Scrinium, capability `file-picker`) asks for the
/// folder the user picked to go into the jail of the app it was picking for
/// (scrinium.md §4.3). The daemon finds that app's jail and gid itself from
/// `app_uid`, and refuses unless both are apps of the session's user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picked {
    pub part: String,
    pub path: String,
    pub app_uid: u32,
    pub read_only: bool,
}

/// A share's protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareProto {
    Sftp,
    /// WebDAV over HTTPS.
    Webdav,
    /// SMB 3.1.1, encrypted, with a password.
    Smb,
}

/// Connect a share whose credential the file manager saved in Arca for
/// exactly this protocol, host, port, path and server identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connect {
    pub proto: ShareProto,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub user: String,
    /// `SHA256:...`, as [`op::LEARN`] gave it and the user accepted; for
    /// WebDAV, the pinned key's, or `trusted` (the trust store vouches for
    /// the host name); for SMB, the server GUID's.
    pub fingerprint: String,
    pub read_only: bool,
    /// Connect it again at each login (D8): off unless the user ticks it.
    pub at_login: bool,
}

/// A connected share's "connect at login" (D8): `on` sets it, `None`
/// only asks. The reply is `on` or `off`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtLogin {
    pub part: String,
    pub on: Option<bool>,
}

/// Ask a server who it is, before anything secret is involved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Learn {
    pub proto: ShareProto,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Added(Volume),
    /// Gone: `ejected` when the user ejected it (safe to unplug), otherwise
    /// unplugged, the session locked or ended.
    Removed { part: String, ejected: bool },
}

fn enc<T: Serialize>(st: u8, v: &T) -> Vec<u8> {
    let mut p = vec![st];
    p.extend(postcard::to_stdvec(v).unwrap_or_default());
    p
}

pub mod server {
    use std::io;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use aqueduct::{flag, Connection};

    use super::*;

    /// What the daemon provides.
    pub trait Host: Send + Sync {
        /// May `uid` use this service now?
        fn allowed(&self, uid: u32) -> bool;
        /// Eject `part`: `(status, message)`. Runs on the connection's
        /// thread; the daemon hands it to its own loop and waits.
        fn eject(&self, part: &str) -> (u8, String);
        /// Carry out a grant (the caller is root: checked here).
        fn grant(&self, g: &Grant) -> (u8, String);
        /// End every grant into `jail`.
        fn revoke(&self, jail: &str) -> (u8, String);
        /// `uid` (a jailed app) asks for every stick in its own jail; the
        /// daemon checks it is the session's file manager.
        fn attach_all(&self, uid: u32) -> (u8, String);
        /// Open `o.path` of the volume mounted at `mountpoint` (the caller is
        /// root and the path has no `..`: both checked here).
        fn open(&self, mountpoint: &str, o: &Open) -> Result<std::fs::File, (u8, String)>;
        /// `uid` (the picker, from its jail) asks for a picked folder to go
        /// into `p.app_uid`'s jail; the daemon checks who both are. The
        /// request itself is checked here (part served, path absolute, no
        /// `..`).
        fn picked(&self, uid: u32, p: &Picked) -> (u8, String);
        /// `uid` (the file manager) connects a share; the daemon checks who
        /// it is and the request.
        fn connect(&self, uid: u32, c: &Connect) -> (u8, String);
        /// `uid` (the file manager) asks for a server's fingerprint.
        fn learn(&self, uid: u32, l: &Learn) -> (u8, String);
        /// `uid` (the file manager) asks or sets whether a connected share
        /// comes back at login.
        fn at_login(&self, uid: u32, a: &AtLogin) -> (u8, String);
        /// Erase and seal a stick (checked here: who, and the request).
        fn format(&self, f: &Format) -> (u8, String);
        /// Open a Locked stick with its recovery code (checked here: who,
        /// the request, and that it's listed Locked with a slot).
        fn adopt(&self, a: &Adopt) -> (u8, String);
        /// "Open read-only?" for an older copy (checked here: who, and that
        /// it's listed as one).
        fn open_older(&self, part: &str) -> (u8, String);
        /// "Use this copy as current" (checked here: who, and that it's
        /// listed as an older copy opened read-only).
        fn make_current(&self, part: &str) -> (u8, String);
        /// "Forget this stick's key" (checked here: who, and that it's
        /// listed as an open sealed stick).
        fn forget(&self, part: &str) -> (u8, String);
        /// Remove its recovery code (checked here: who, and that it's
        /// listed open, writable and with one).
        fn remove_code(&self, part: &str) -> (u8, String);
    }

    pub struct Shared {
        host: Box<dyn Host>,
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        volumes: Vec<Volume>,
        subscribers: Vec<(u32, mpsc::Sender<Event>)>,
    }

    impl Shared {
        pub fn new(host: Box<dyn Host>) -> Arc<Self> {
            Arc::new(Self { host, state: Mutex::new(State::default()) })
        }

        /// The volumes served now; subscribers are told what changed.
        /// `ejected`: parts that went because the user ejected them.
        pub fn update(&self, volumes: Vec<Volume>, ejected: &[String]) {
            let mut st = self.state.lock().unwrap();
            let mut evs = Vec::new();
            for old in &st.volumes {
                if !volumes.iter().any(|v| v.part == old.part) {
                    evs.push(Event::Removed { part: old.part.clone(), ejected: ejected.contains(&old.part) });
                }
            }
            // Free space alone changing is no event: `list` has it fresh.
            let same = |a: &Volume, b: &Volume| Volume { free_bytes: 0, ..a.clone() } == Volume { free_bytes: 0, ..b.clone() };
            for v in &volumes {
                if !st.volumes.iter().any(|o| same(o, v)) {
                    if st.volumes.iter().any(|o| o.part == v.part) {
                        evs.push(Event::Removed { part: v.part.clone(), ejected: false });
                    }
                    evs.push(Event::Added(v.clone()));
                }
            }
            st.volumes = volumes;
            let host = &self.host;
            st.subscribers.retain(|(uid, tx)| host.allowed(*uid) && evs.iter().all(|e| tx.send(e.clone()).is_ok()));
        }

        pub fn list(&self, uid: u32) -> Option<Vec<Volume>> {
            self.host.allowed(uid).then(|| self.state.lock().unwrap().volumes.clone())
        }

        fn subscribe(&self, uid: u32) -> Option<(Vec<Volume>, mpsc::Receiver<Event>)> {
            if !self.host.allowed(uid) {
                return None;
            }
            let (tx, rx) = mpsc::channel();
            let mut st = self.state.lock().unwrap();
            st.subscribers.push((uid, tx));
            Some((st.volumes.clone(), rx))
        }

        /// Root, or the session's user (the same rule as eject). The part
        /// need not be listed: a blank stick never is. The daemon checks it
        /// is a stick it serves.
        pub fn format(&self, uid: u32, f: &Format) -> (u8, String) {
            if uid != 0 && !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may format".into());
            }
            let ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.contains('\n') && !s.contains('\0');
            if !ok(&f.part, 32) || !ok(&f.persona, 64) || !ok(&f.label, 128) || f.part.starts_with("share-") {
                return (status::MALFORMED, "malformed".into());
            }
            self.host.format(f)
        }

        /// The same rule as format; the stick must be listed Locked with a
        /// recovery slot.
        pub fn adopt(&self, uid: u32, a: &Adopt) -> (u8, String) {
            if uid != 0 && !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may open a stick".into());
            }
            let ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.contains('\n') && !s.contains('\0');
            let code_ok = if a.handle == [0; 16] { ok(&a.code, 64) } else { a.code.is_empty() };
            if !ok(&a.part, 32) || !ok(&a.persona, 64) || !code_ok {
                return (status::MALFORMED, "malformed".into());
            }
            if !self.state.lock().unwrap().volumes.iter().any(|v| v.part == a.part && v.locked && v.recovery) {
                return (status::NOT_FOUND, format!("{} is not a Locked stick with a recovery code", a.part));
            }
            self.host.adopt(a)
        }

        pub fn open_older(&self, uid: u32, part: &str) -> (u8, String) {
            if uid != 0 && !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may open a stick".into());
            }
            if !self.state.lock().unwrap().volumes.iter().any(|v| v.part == part && v.locked && v.older) {
                return (status::NOT_FOUND, format!("{part} is not an older copy of a stick of yours"));
            }
            self.host.open_older(part)
        }

        pub fn make_current(&self, uid: u32, part: &str) -> (u8, String) {
            if uid != 0 && !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may change a stick".into());
            }
            if !self.state.lock().unwrap().volumes.iter().any(|v| v.part == part && !v.locked && v.older) {
                return (status::NOT_FOUND, format!("{part} is not an older copy opened read-only"));
            }
            self.host.make_current(part)
        }

        pub fn forget(&self, uid: u32, part: &str) -> (u8, String) {
            if uid != 0 && !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may change a stick".into());
            }
            if !self.state.lock().unwrap().volumes.iter().any(|v| v.part == part && !v.locked && !v.persona.is_empty() && !v.part.starts_with("share-")) {
                return (status::NOT_FOUND, format!("{part} is not an open sealed stick"));
            }
            self.host.forget(part)
        }

        pub fn remove_code(&self, uid: u32, part: &str) -> (u8, String) {
            if uid != 0 && !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may change a stick".into());
            }
            if !self.state.lock().unwrap().volumes.iter().any(|v| v.part == part && !v.locked && !v.read_only && v.recovery && !v.persona.is_empty() && !v.part.starts_with("share-")) {
                return (status::NOT_FOUND, format!("{part} is not an open sealed stick with a recovery code"));
            }
            self.host.remove_code(part)
        }

        pub fn eject(&self, uid: u32, part: &str) -> (u8, String) {
            if !self.host.allowed(uid) {
                return (status::NOT_ALLOWED, "only the session's user may eject".into());
            }
            if !self.state.lock().unwrap().volumes.iter().any(|v| v.part == part) {
                return (status::NOT_FOUND, format!("{part} is not mounted"));
            }
            self.host.eject(part)
        }
    }

    /// Grants are root's alone: portcullisd, for the picker and the file
    /// manager. Not the session's user, whose own mount has it all already.
    pub(crate) fn grant_checked(shared: &Shared, uid: u32, g: &Grant) -> (u8, String) {
        if uid != 0 {
            return (status::NOT_ALLOWED, "only root may grant".into());
        }
        let ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.contains('\n') && !s.contains('\0');
        if !ok(&g.part, 32) || !ok(&g.jail, 255) || !ok(&g.path, 1024) || !g.path.starts_with('/') || g.path.split('/').any(|c| c == "..") || g.uid == 0 {
            return (status::MALFORMED, "malformed grant".into());
        }
        if !shared.state.lock().unwrap().volumes.iter().any(|v| v.part == g.part) {
            return (status::NOT_FOUND, format!("{} is not mounted", g.part));
        }
        shared.host.grant(g)
    }

    pub(crate) fn picked_checked(shared: &Shared, uid: u32, p: &Picked) -> (u8, String) {
        let ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.contains('\n') && !s.contains('\0');
        if !ok(&p.part, 32) || !ok(&p.path, 1024) || !p.path.starts_with('/') || p.path.split('/').any(|c| c == "..") || p.app_uid == 0 || uid == 0 {
            return (status::MALFORMED, "malformed pick".into());
        }
        if !shared.state.lock().unwrap().volumes.iter().any(|v| v.part == p.part) {
            return (status::NOT_FOUND, format!("{} is not mounted", p.part));
        }
        shared.host.picked(uid, p)
    }

    fn send_ev(conn: &mut Connection, ev: &Event) -> io::Result<()> {
        let p = postcard::to_stdvec(ev).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        conn.send_message(CLASS, op::EV, flag::ASYNC_EVENT, &p)
    }

    /// Serve one connection; `uid` is the kernel's answer for the peer.
    pub fn serve(mut conn: Connection, uid: u32, shared: Arc<Shared>) -> io::Result<()> {
        loop {
            let m = conn.recv_message()?;
            if m.opcode_class != CLASS {
                continue;
            }
            match m.op {
                op::LIST => {
                    let p = match shared.list(uid) {
                        Some(v) => enc(status::OK, &v),
                        None => vec![status::NOT_ALLOWED],
                    };
                    conn.send_message(CLASS, op::LIST, flag::IS_RESPONSE, &p)?;
                }
                op::EJECT => {
                    let (st, msg) = match postcard::from_bytes::<Eject>(&m.payload) {
                        Ok(e) if !e.part.is_empty() && e.part.len() <= 32 => shared.eject(uid, &e.part),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::EJECT, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::GRANT => {
                    let (st, msg) = match postcard::from_bytes::<Grant>(&m.payload) {
                        Ok(g) => grant_checked(&shared, uid, &g),
                        Err(_) => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::GRANT, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::REVOKE => {
                    let (st, msg) = match postcard::from_bytes::<String>(&m.payload) {
                        Ok(_) if uid != 0 => (status::NOT_ALLOWED, "only root may revoke".into()),
                        Ok(j) if !j.is_empty() && j.len() <= 255 => shared.host.revoke(&j),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::REVOKE, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::ATTACH_ALL => {
                    let (st, msg) = shared.host.attach_all(uid);
                    conn.send_message(CLASS, op::ATTACH_ALL, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::OPEN => {
                    let r = match postcard::from_bytes::<Open>(&m.payload) {
                        Ok(_) if uid != 0 => Err((status::NOT_ALLOWED, "only root may open".into())),
                        Ok(o) if o.part.is_empty() || o.part.len() > 32 || !o.path.starts_with('/') || o.path.len() > 1024 || o.path.split('/').any(|c| c == "..") || o.path.contains('\0') => {
                            Err((status::MALFORMED, "malformed".into()))
                        }
                        Ok(o) => {
                            let mnt = shared.state.lock().unwrap().volumes.iter().find(|v| v.part == o.part).map(|v| v.mountpoint.clone());
                            match mnt {
                                Some(mnt) => shared.host.open(&mnt, &o),
                                None => Err((status::NOT_FOUND, format!("{} is not mounted", o.part))),
                            }
                        }
                        Err(_) => Err((status::MALFORMED, "malformed".into())),
                    };
                    match r {
                        Ok(f) => {
                            use std::os::fd::AsRawFd;
                            conn.send_message_with_fds(CLASS, op::OPEN, flag::IS_RESPONSE, &enc(status::OK, &String::new()), &[f.as_raw_fd()])?
                        }
                        Err((st, msg)) => conn.send_message(CLASS, op::OPEN, flag::IS_RESPONSE, &enc(st, &msg))?,
                    }
                }
                op::PICKED => {
                    let (st, msg) = match postcard::from_bytes::<Picked>(&m.payload) {
                        Ok(p) => picked_checked(&shared, uid, &p),
                        Err(_) => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::PICKED, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::CONNECT => {
                    let (st, msg) = match postcard::from_bytes::<Connect>(&m.payload) {
                        Ok(c) => shared.host.connect(uid, &c),
                        Err(_) => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::CONNECT, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::LEARN => {
                    let (st, msg) = match postcard::from_bytes::<Learn>(&m.payload) {
                        Ok(l) => shared.host.learn(uid, &l),
                        Err(_) => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::LEARN, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::AT_LOGIN => {
                    let (st, msg) = match postcard::from_bytes::<AtLogin>(&m.payload) {
                        Ok(a) if a.part.starts_with("share-") && a.part.len() <= 32 => shared.host.at_login(uid, &a),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::AT_LOGIN, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::FORMAT => {
                    let (st, msg) = match postcard::from_bytes::<Format>(&m.payload) {
                        Ok(f) => shared.format(uid, &f),
                        Err(_) => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::FORMAT, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::ADOPT => {
                    let (st, msg) = match postcard::from_bytes::<Adopt>(&m.payload) {
                        Ok(a) => shared.adopt(uid, &a),
                        Err(_) => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::ADOPT, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::OPEN_OLDER => {
                    let (st, msg) = match postcard::from_bytes::<Eject>(&m.payload) {
                        Ok(e) if !e.part.is_empty() && e.part.len() <= 32 => shared.open_older(uid, &e.part),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::OPEN_OLDER, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::MAKE_CURRENT => {
                    let (st, msg) = match postcard::from_bytes::<Eject>(&m.payload) {
                        Ok(e) if !e.part.is_empty() && e.part.len() <= 32 => shared.make_current(uid, &e.part),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::MAKE_CURRENT, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::FORGET => {
                    let (st, msg) = match postcard::from_bytes::<Eject>(&m.payload) {
                        Ok(e) if !e.part.is_empty() && e.part.len() <= 32 => shared.forget(uid, &e.part),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::FORGET, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::REMOVE_CODE => {
                    let (st, msg) = match postcard::from_bytes::<Eject>(&m.payload) {
                        Ok(e) if !e.part.is_empty() && e.part.len() <= 32 => shared.remove_code(uid, &e.part),
                        _ => (status::MALFORMED, "malformed".into()),
                    };
                    conn.send_message(CLASS, op::REMOVE_CODE, flag::IS_RESPONSE, &enc(st, &msg))?;
                }
                op::SUBSCRIBE => {
                    let Some((now, rx)) = shared.subscribe(uid) else {
                        conn.send_message(CLASS, op::SUBSCRIBE, flag::IS_RESPONSE, &[status::NOT_ALLOWED])?;
                        continue;
                    };
                    conn.send_message(CLASS, op::SUBSCRIBE, flag::IS_RESPONSE, &[status::OK])?;
                    for v in now {
                        send_ev(&mut conn, &Event::Added(v))?;
                    }
                    loop {
                        match rx.recv_timeout(Duration::from_secs(30)) {
                            Ok(ev) => send_ev(&mut conn, &ev)?,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            // Dropped: the session changed.
                            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                        }
                    }
                }
                o => conn.send_message(CLASS, o, flag::IS_RESPONSE, &[status::MALFORMED])?,
            }
        }
    }
}

pub mod client {
    use std::io;
    use std::path::Path;

    use aqueduct::{flag, Connection};

    use super::*;

    fn call(c: &mut Connection, o: u16, payload: &[u8]) -> io::Result<Vec<u8>> {
        c.send_message(CLASS, o, flag::RESPONSE_EXPECTED, payload)?;
        loop {
            let m = c.recv_message()?;
            if m.opcode_class == CLASS && m.op == o && m.flags & flag::IS_RESPONSE != 0 {
                return Ok(m.payload);
            }
        }
    }

    fn refused(st: u8, msg: &str) -> io::Error {
        let kind = if st == status::NOT_ALLOWED { io::ErrorKind::PermissionDenied } else { io::ErrorKind::Other };
        io::Error::new(kind, format!("{msg} (status {st})"))
    }

    pub fn list(sock: &Path) -> io::Result<Vec<Volume>> {
        let mut c = Connection::connect(sock)?;
        let p = call(&mut c, op::LIST, &[])?;
        match p.split_first() {
            Some((&status::OK, rest)) => postcard::from_bytes(rest).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            Some((&st, _)) => Err(refused(st, "list refused")),
            None => Err(io::Error::new(io::ErrorKind::InvalidData, "empty reply")),
        }
    }

    /// Eject `part`: the daemon's message on success; on failure the error
    /// carries it (`BUSY`: a file is open on it).
    pub fn eject(sock: &Path, part: &str) -> Result<String, (u8, String)> {
        let r = (|| {
            let mut c = Connection::connect(sock)?;
            let p = call(&mut c, op::EJECT, &postcard::to_stdvec(&Eject { part: part.into() }).unwrap_or_default())?;
            let (&st, rest) = p.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty reply"))?;
            Ok::<_, io::Error>((st, postcard::from_bytes::<String>(rest).unwrap_or_default()))
        })();
        match r {
            Ok((status::OK, m)) => Ok(m),
            Ok((st, m)) => Err((st, m)),
            Err(e) => Err((status::FAILED, e.to_string())),
        }
    }

    pub fn format(sock: &Path, f: &Format) -> Result<String, (u8, String)> {
        status_call(sock, op::FORMAT, f)
    }

    pub fn adopt(sock: &Path, a: &Adopt) -> Result<String, (u8, String)> {
        status_call(sock, op::ADOPT, a)
    }

    pub fn open_older(sock: &Path, part: &Eject) -> Result<String, (u8, String)> {
        status_call(sock, op::OPEN_OLDER, part)
    }

    pub fn make_current(sock: &Path, part: &Eject) -> Result<String, (u8, String)> {
        status_call(sock, op::MAKE_CURRENT, part)
    }

    pub fn forget(sock: &Path, part: &Eject) -> Result<String, (u8, String)> {
        status_call(sock, op::FORGET, part)
    }

    pub fn remove_code(sock: &Path, part: &Eject) -> Result<String, (u8, String)> {
        status_call(sock, op::REMOVE_CODE, part)
    }

    fn status_call<T: Serialize>(sock: &Path, o: u16, v: &T) -> Result<String, (u8, String)> {
        let r = (|| {
            let mut c = Connection::connect(sock)?;
            let p = call(&mut c, o, &postcard::to_stdvec(v).unwrap_or_default())?;
            let (&st, rest) = p.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty reply"))?;
            Ok::<_, io::Error>((st, postcard::from_bytes::<String>(rest).unwrap_or_default()))
        })();
        match r {
            Ok((status::OK, m)) => Ok(m),
            Ok((st, m)) => Err((st, m)),
            Err(e) => Err((status::FAILED, e.to_string())),
        }
    }

    /// Grant (root only): where it is in the jail.
    pub fn grant(sock: &Path, g: &Grant) -> Result<String, (u8, String)> {
        status_call(sock, op::GRANT, g)
    }

    /// End every grant into `jail` (root only).
    pub fn revoke(sock: &Path, jail: &str) -> Result<String, (u8, String)> {
        status_call(sock, op::REVOKE, &jail.to_string())
    }

    /// The file manager connects a saved share: its part name.
    pub fn connect(sock: &Path, c: &Connect) -> Result<String, (u8, String)> {
        status_call(sock, op::CONNECT, c)
    }

    /// The file manager asks for a server's host key fingerprint.
    pub fn learn(sock: &Path, l: &Learn) -> Result<String, (u8, String)> {
        status_call(sock, op::LEARN, l)
    }

    /// The file manager asks (`on: None`) or sets whether a connected
    /// share comes back at login: `on` or `off`.
    pub fn at_login(sock: &Path, a: &AtLogin) -> Result<String, (u8, String)> {
        status_call(sock, op::AT_LOGIN, a)
    }

    /// The file manager asks for every stick in its jail.
    pub fn attach_all(sock: &Path) -> Result<String, (u8, String)> {
        status_call(sock, op::ATTACH_ALL, &())
    }

    /// The picker asks for the picked folder in the app's jail: where it is
    /// there.
    pub fn picked(sock: &Path, p: &Picked) -> Result<String, (u8, String)> {
        status_call(sock, op::PICKED, p)
    }

    /// Open a file of a stick (root only): the open file.
    pub fn open(sock: &Path, o: &Open) -> Result<std::fs::File, (u8, String)> {
        let r = (|| {
            let mut c = Connection::connect(sock)?;
            let p = call(&mut c, op::OPEN, &postcard::to_stdvec(o).unwrap_or_default())?;
            let (&st, rest) = p.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty reply"))?;
            let fd = c.take_fds(1).and_then(|v| v.into_iter().next());
            Ok::<_, io::Error>((st, postcard::from_bytes::<String>(rest).unwrap_or_default(), fd))
        })();
        match r {
            Ok((status::OK, _, Some(fd))) => Ok(std::fs::File::from(fd)),
            Ok((status::OK, _, None)) => Err((status::FAILED, "no descriptor came".into())),
            Ok((st, m, _)) => Err((st, m)),
            Err(e) => Err((status::FAILED, e.to_string())),
        }
    }

    /// Subscribe: an `Added` per volume served now, then every change, until
    /// the daemon drops the stream (the session changed) or the socket closes.
    pub struct Events(Connection);

    impl Events {
        pub fn open(sock: &Path) -> io::Result<Self> {
            let mut c = Connection::connect(sock)?;
            match call(&mut c, op::SUBSCRIBE, &[])?.first() {
                Some(&status::OK) => Ok(Self(c)),
                Some(&st) => Err(refused(st, "subscribe refused")),
                None => Err(io::Error::new(io::ErrorKind::InvalidData, "empty reply")),
            }
        }

        pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
            std::os::fd::AsRawFd::as_raw_fd(&self.0)
        }

        /// The next event (blocking).
        pub fn next(&mut self) -> io::Result<Event> {
            loop {
                let m = self.0.recv_message()?;
                if m.opcode_class == CLASS && m.op == op::EV {
                    return postcard::from_bytes(&m.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    struct H(Arc<AtomicU32>);
    impl server::Host for H {
        fn allowed(&self, uid: u32) -> bool {
            uid == self.0.load(Ordering::SeqCst)
        }
        fn eject(&self, part: &str) -> (u8, String) {
            if part == "da1s1" { (status::BUSY, "busy".into()) } else { (status::OK, format!("{part} ejected")) }
        }
        fn grant(&self, g: &Grant) -> (u8, String) {
            (status::OK, format!("/media/{}", g.part))
        }
        fn revoke(&self, j: &str) -> (u8, String) {
            (status::OK, j.into())
        }
        fn attach_all(&self, uid: u32) -> (u8, String) {
            (status::NOT_ALLOWED, format!("{uid}"))
        }
        fn open(&self, mnt: &str, o: &Open) -> Result<std::fs::File, (u8, String)> {
            std::fs::File::open(format!("{mnt}{}", o.path)).map_err(|e| (status::FAILED, e.to_string()))
        }
        fn picked(&self, uid: u32, p: &Picked) -> (u8, String) {
            if uid == 7 { (status::OK, format!("/media/{}", p.part)) } else { (status::NOT_ALLOWED, "not the picker".into()) }
        }        fn connect(&self, _uid: u32, c: &Connect) -> (u8, String) {
            (status::OK, format!("share-{}", c.port))
        }
        fn learn(&self, _uid: u32, _l: &Learn) -> (u8, String) {
            (status::NOT_ALLOWED, "test".into())
        }
        fn at_login(&self, _uid: u32, a: &AtLogin) -> (u8, String) {
            (status::OK, if a.on == Some(true) { "on" } else { "off" }.into())
        }
        fn format(&self, f: &Format) -> (u8, String) {
            (status::OK, format!("{} sealed for {}", f.part, f.persona))
        }
        fn adopt(&self, a: &Adopt) -> (u8, String) {
            (status::OK, format!("{} opened for {}", a.part, a.persona))
        }
        fn open_older(&self, part: &str) -> (u8, String) {
            (status::OK, format!("{part} opened read-only"))
        }
        fn make_current(&self, part: &str) -> (u8, String) {
            (status::OK, format!("{part} is now the current copy"))
        }
        fn forget(&self, part: &str) -> (u8, String) {
            (status::OK, format!("{part}'s key is forgotten"))
        }
        fn remove_code(&self, part: &str) -> (u8, String) {
            (status::OK, format!("{part}'s recovery code is removed"))
        }
    }

    fn vol(p: &str) -> Volume {
        Volume { part: p.into(), mountpoint: format!("/media/{p}"), size_bytes: 1 << 26, free_bytes: 1 << 25, read_only: false, label: String::new(), persona: String::new(), locked: false, recovery: false, older: false }
    }

    /// Over a real socket: list, subscribe, eject, a session change.
    #[test]
    fn over_a_socket() {
        let dir = std::env::temp_dir().join(format!("atrium-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("media.sock");
        let me = unsafe_uid();
        let session = Arc::new(AtomicU32::new(me));
        let shared = server::Shared::new(Box::new(H(session.clone())));
        shared.update(vec![vol("da0s1")], &[]);
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let sh = shared.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let sh = sh.clone();
                std::thread::spawn(move || {
                    let _ = server::serve(aqueduct::Connection::wrap(s).unwrap(), me, sh);
                });
            }
        });
        assert_eq!(client::list(&sock).unwrap(), vec![vol("da0s1")]);
        let mut ev = client::Events::open(&sock).unwrap();
        assert_eq!(ev.next().unwrap(), Event::Added(vol("da0s1")));
        shared.update(vec![vol("da0s1"), vol("da1s1")], &[]);
        assert_eq!(ev.next().unwrap(), Event::Added(vol("da1s1")));
        // Only free space changed: no event, but list has it.
        shared.update(vec![Volume { free_bytes: 7, ..vol("da0s1") }, vol("da1s1")], &[]);
        assert_eq!(client::list(&sock).unwrap()[0].free_bytes, 7);
        assert_eq!(client::eject(&sock, "da1s1").unwrap_err().0, status::BUSY);
        assert_eq!(client::eject(&sock, "da9s1").unwrap_err().0, status::NOT_FOUND);
        assert_eq!(client::eject(&sock, "da0s1").unwrap(), "da0s1 ejected");
        shared.update(vec![vol("da1s1")], &["da0s1".to_string()]);
        assert_eq!(ev.next().unwrap(), Event::Removed { part: "da0s1".into(), ejected: true });
        // The session goes to someone else: refused, and the stream ends.
        session.store(me.wrapping_add(1), Ordering::SeqCst);
        shared.update(vec![], &[]);
        assert!(ev.next().is_err(), "dropped");
        {
            assert_eq!(client::list(&sock).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(client::eject(&sock, "da1s1").unwrap_err().0, status::NOT_ALLOWED);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unsafe_uid() -> u32 {
        std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0)
    }

    #[test]
    fn update_reports_changes_once() {
        let session = Arc::new(AtomicU32::new(5));
        let s = server::Shared::new(Box::new(H(session)));
        s.update(vec![vol("a")], &[]);
        assert_eq!(s.list(5).unwrap().len(), 1);
        assert!(s.list(6).is_none());
        assert_eq!(s.eject(6, "a").0, status::NOT_ALLOWED);
    }

    #[test]
    fn grants_are_roots_and_checked() {
        let s = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(5)))));
        s.update(vec![vol("da0s1")], &[]);
        let g = Grant { part: "da0s1".into(), path: "/Photos".into(), jail: "atrium-app-1".into(), uid: 50001, gid: 50001, read_only: false };
        assert_eq!(server::grant_checked(&s, 0, &g), (status::OK, "/media/da0s1".into()));
        assert_eq!(server::grant_checked(&s, 5, &g).0, status::NOT_ALLOWED, "not even the session's user");
        for bad in [
            Grant { path: "Photos".into(), ..g.clone() },
            Grant { path: "/a/../b".into(), ..g.clone() },
            Grant { path: "/a\nmount 0 0 0 /".into(), ..g.clone() },
            Grant { uid: 0, ..g.clone() },
            Grant { jail: String::new(), ..g.clone() },
        ] {
            assert_eq!(server::grant_checked(&s, 0, &bad).0, status::MALFORMED, "{bad:?}");
        }
        assert_eq!(server::grant_checked(&s, 0, &Grant { part: "da9s1".into(), ..g.clone() }).0, status::NOT_FOUND);
    }

    #[test]
    fn picks_are_checked_before_the_daemon_sees_them() {
        let s = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(5)))));
        s.update(vec![vol("da0s1")], &[]);
        let p = Picked { part: "da0s1".into(), path: "/Photos".into(), app_uid: 50001, read_only: true };
        assert_eq!(server::picked_checked(&s, 7, &p), (status::OK, "/media/da0s1".into()));
        assert_eq!(server::picked_checked(&s, 5, &p).0, status::NOT_ALLOWED, "the daemon decides who the picker is");
        for bad in [
            Picked { path: "Photos".into(), ..p.clone() },
            Picked { path: "/a/../..".into(), ..p.clone() },
            Picked { path: "/a\0".into(), ..p.clone() },
            Picked { app_uid: 0, ..p.clone() },
            Picked { part: String::new(), ..p.clone() },
        ] {
            assert_eq!(server::picked_checked(&s, 7, &bad).0, status::MALFORMED, "{bad:?}");
        }
        assert_eq!(server::picked_checked(&s, 0, &p).0, status::MALFORMED, "root grants with GRANT");
        assert_eq!(server::picked_checked(&s, 7, &Picked { part: "da9s1".into(), ..p.clone() }).0, status::NOT_FOUND);
    }

    /// OPEN hands over a descriptor, for root alone, never above the stick.
    #[test]
    fn open_hands_over_a_file() {
        let dir = std::env::temp_dir().join(format!("atrium-media-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("stick/Docs")).unwrap();
        std::fs::write(dir.join("stick/Docs/a.txt"), b"hello").unwrap();
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(0)))));
        shared.update(vec![Volume { mountpoint: dir.join("stick").to_string_lossy().into_owned(), ..vol("da0s1") }], &[]);
        let (sock, sock5) = (dir.join("root.sock"), dir.join("five.sock"));
        for (path, uid) in [(sock.clone(), 0u32), (sock5.clone(), 5)] {
            let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let sh = shared.clone();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    let sh = sh.clone();
                    std::thread::spawn(move || {
                        let _ = server::serve(aqueduct::Connection::wrap(s).unwrap(), uid, sh);
                    });
                }
            });
        }
        let o = Open { part: "da0s1".into(), path: "/Docs/a.txt".into(), write: false };
        let mut f = client::open(&sock, &o).unwrap();
        let mut got = String::new();
        std::io::Read::read_to_string(&mut f, &mut got).unwrap();
        assert_eq!(got, "hello");
        assert_eq!(client::open(&sock5, &o).unwrap_err().0, status::NOT_ALLOWED);
        assert_eq!(client::open(&sock, &Open { path: "/Docs/../../x".into(), ..o.clone() }).unwrap_err().0, status::MALFORMED);
        assert_eq!(client::open(&sock, &Open { part: "da9s1".into(), ..o.clone() }).unwrap_err().0, status::NOT_FOUND);
        assert_eq!(client::attach_all(&sock5).unwrap_err(), (status::NOT_ALLOWED, "5".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_is_the_session_users_or_roots() {
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(1000)))));
        shared.update(vec![vol("da0p1"), Volume { mountpoint: String::new(), locked: true, ..vol("da1p1") }], &[]);
        let f = |part: &str| Format { part: part.into(), persona: "work".into(), label: "Work stick".into(), recovery: false };
        assert_eq!(shared.format(1000, &f("da0p1")).0, status::OK);
        assert_eq!(shared.format(0, &f("da1p1")).0, status::OK, "a locked stick can be erased");
        assert_eq!(shared.format(2000, &f("da0p1")).0, status::NOT_ALLOWED);
        assert_eq!(shared.format(1000, &Format { persona: String::new(), ..f("da0p1") }).0, status::MALFORMED);
        assert_eq!(shared.format(1000, &f("share-1001")).0, status::MALFORMED);
    }

    #[test]
    fn adopt_needs_a_locked_stick_with_a_slot() {
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(1000)))));
        let locked = |p: &str, recovery: bool| Volume { mountpoint: String::new(), locked: true, recovery, ..vol(p) };
        shared.update(vec![vol("da0p1"), locked("da1p1", true), locked("da2p1", false)], &[]);
        let a = |part: &str| Adopt { part: part.into(), persona: "personal".into(), keep: true, code: "ABCDE".into(), handle: [0; 16] };
        assert_eq!(shared.adopt(1000, &a("da1p1")).0, status::OK);
        assert_eq!(shared.adopt(2000, &a("da1p1")).0, status::NOT_ALLOWED);
        assert_eq!(shared.adopt(1000, &a("da0p1")).0, status::NOT_FOUND, "not Locked");
        assert_eq!(shared.adopt(1000, &a("da2p1")).0, status::NOT_FOUND, "no slot");
        assert_eq!(shared.adopt(1000, &Adopt { code: String::new(), ..a("da1p1") }).0, status::MALFORMED);
        // A handle from Arca's field instead of the code, never both.
        assert_eq!(shared.adopt(1000, &Adopt { code: String::new(), handle: [7; 16], ..a("da1p1") }).0, status::OK);
        assert_eq!(shared.adopt(1000, &Adopt { handle: [7; 16], ..a("da1p1") }).0, status::MALFORMED);
    }

    #[test]
    fn open_older_needs_a_listed_older_copy() {
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(1000)))));
        let locked = |p: &str, older: bool| Volume { mountpoint: String::new(), locked: true, older, ..vol(p) };
        shared.update(vec![vol("da0p1"), locked("da1p1", true), locked("da2p1", false)], &[]);
        assert_eq!(shared.open_older(1000, "da1p1").0, status::OK);
        assert_eq!(shared.open_older(2000, "da1p1").0, status::NOT_ALLOWED);
        assert_eq!(shared.open_older(1000, "da0p1").0, status::NOT_FOUND);
        assert_eq!(shared.open_older(1000, "da2p1").0, status::NOT_FOUND);
    }

    #[test]
    fn make_current_needs_an_opened_older_copy() {
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(1000)))));
        let opened = Volume { read_only: true, older: true, ..vol("da1p1") };
        let locked = Volume { mountpoint: String::new(), locked: true, older: true, ..vol("da2p1") };
        shared.update(vec![vol("da0p1"), opened, locked], &[]);
        assert_eq!(shared.make_current(1000, "da1p1").0, status::OK);
        assert_eq!(shared.make_current(2000, "da1p1").0, status::NOT_ALLOWED);
        assert_eq!(shared.make_current(1000, "da0p1").0, status::NOT_FOUND, "a current stick");
        assert_eq!(shared.make_current(1000, "da2p1").0, status::NOT_FOUND, "not opened yet");
    }

    #[test]
    fn forget_needs_an_open_sealed_stick() {
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(1000)))));
        let mine = Volume { persona: "personal".into(), ..vol("da1p1") };
        let locked = Volume { mountpoint: String::new(), locked: true, persona: "work".into(), ..vol("da2p1") };
        let share = Volume { persona: "personal".into(), ..vol("share-1000") };
        shared.update(vec![vol("da0p1"), mine, locked, share], &[]);
        assert_eq!(shared.forget(1000, "da1p1").0, status::OK);
        assert_eq!(shared.forget(2000, "da1p1").0, status::NOT_ALLOWED);
        assert_eq!(shared.forget(1000, "da0p1").0, status::NOT_FOUND, "a plain stick");
        assert_eq!(shared.forget(1000, "da2p1").0, status::NOT_FOUND, "already Locked");
        assert_eq!(shared.forget(1000, "share-1000").0, status::NOT_FOUND, "a share");
    }

    #[test]
    fn remove_code_needs_an_open_writable_stick_with_one() {
        let shared = server::Shared::new(Box::new(H(Arc::new(AtomicU32::new(1000)))));
        let with = Volume { persona: "personal".into(), recovery: true, ..vol("da1p1") };
        let without = Volume { persona: "personal".into(), ..vol("da2p1") };
        let older = Volume { persona: "personal".into(), recovery: true, read_only: true, older: true, ..vol("da3p1") };
        let locked = Volume { mountpoint: String::new(), locked: true, recovery: true, ..vol("da4p1") };
        shared.update(vec![with, without, older, locked], &[]);
        assert_eq!(shared.remove_code(1000, "da1p1").0, status::OK);
        assert_eq!(shared.remove_code(2000, "da1p1").0, status::NOT_ALLOWED);
        for p in ["da2p1", "da3p1", "da4p1"] {
            assert_eq!(shared.remove_code(1000, p).0, status::NOT_FOUND, "{p}");
        }
    }
}
