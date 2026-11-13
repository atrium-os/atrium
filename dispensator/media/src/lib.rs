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

    /// The file manager asks for every stick in its jail.
    pub fn attach_all(sock: &Path) -> Result<String, (u8, String)> {
        status_call(sock, op::ATTACH_ALL, &())
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
    }

    fn vol(p: &str) -> Volume {
        Volume { part: p.into(), mountpoint: format!("/media/{p}"), size_bytes: 1 << 26, free_bytes: 1 << 25, read_only: false }
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
}
