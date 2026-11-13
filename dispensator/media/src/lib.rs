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
//! | `SUBSCRIBE` 0x10 | empty | `u8 status`; then `EV` 0x80 events (`ASYNC_EVENT`), postcard [`Event`]: an `Added` per volume served now, then changes |
//!
//! Only the session's own user and its desktop components (Forum) may ask;
//! the daemon decides who that is ([`server::Host::allowed`]) and checks it
//! again on every request and event, so a subscriber is dropped when the
//! session changes. The files themselves never go over Aqueduct: they stay a
//! FUSE mount that the file manager's jail is given.

use serde::{Deserialize, Serialize};

pub use aqueduct::classes::CLASS_MEDIA as CLASS;

pub mod op {
    pub const LIST: u16 = 0x00;
    pub const EJECT: u16 = 0x01;
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
}
