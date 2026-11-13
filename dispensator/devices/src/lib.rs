//! The device list on Aqueduct (`CLASS_DEVICES`, device-assignment.md §6.10).
//!
//! dispensatord gives each device one owner or none (§4). This is how
//! Curia's Devices page sees the result: every device, who has it, and for
//! one nobody has, why.
//!
//! | op | request | reply (`IS_RESPONSE`) |
//! |---|---|---|
//! | `LIST` 0x00 | empty | `u8 status`, then postcard `Vec<Device>` |
//! | `SUBSCRIBE` 0x10 | empty | `u8 status`; then `EV` 0x80 events (`ASYNC_EVENT`), postcard `u64`: the list's generation, sent now and on every change (list again to see it) |
//!
//! Who may ask is the daemon's call ([`server::Host::allowed`]): root, the
//! active session's user and that user's session components. It is checked
//! on every request and change, so a subscriber is dropped when the session
//! changes. Hardware ids identify a machine, so the list never leaves it:
//! no op sends it anywhere, and only Curia's jail is given the socket
//! (capability `device-list`).

use serde::{Deserialize, Serialize};

pub use aqueduct::classes::CLASS_DEVICES as CLASS;

pub mod op {
    pub const LIST: u16 = 0x00;
    pub const SUBSCRIBE: u16 = 0x10;
    pub const EV: u16 = 0x80;
}

pub mod status {
    pub const OK: u8 = 0;
    pub const MALFORMED: u8 = 1;
    /// Not root, the session's user or one of its components.
    pub const NOT_ALLOWED: u8 = 2;
}

/// One device, as dispensatord sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// What the daemon calls it: the kernel's name (`ukbd0`, `hidraw3`,
    /// `pcm1`), `ugenB.A:I` for a USB interface, the GEOM name for a disk
    /// (`da0s1`), or `parent?location` for a child no kernel driver took.
    pub name: String,
    /// The registry's id: new for every attach, so a reused unit number is
    /// a different device. 0 for disks, which GEOM names.
    pub id: u64,
    /// `usb`, `hid`, `pci`, `acpi`, `geom`, ... (from the parent).
    pub bus: String,
    pub parent: String,
    /// The identifying fields the kernel gave (`vendor=0x046d product=...`),
    /// in its order.
    pub ids: String,
    pub owner: Owner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Owner {
    /// A jailed driver from rule `rule`; `running`: its jail is up now (a
    /// stick's server also waits for an unlocked session).
    Driver { rule: String, driver: String, running: bool },
    /// Nodes shown in a service's own devfs mount.
    Service { rule: String, service: String, nodes: Vec<String> },
    /// FreeBSD's own driver has it and no rule asks for it: buses, bridges,
    /// keyboards, disks. `note`: why a rule that matches did not take it.
    Kernel { driver: String, note: Option<String> },
    /// Nobody: no driver and no rule, so it is visible to no one (§8
    /// decision 4). `why` says which.
    Nobody { why: String },
}

impl Owner {
    /// One word, for a column.
    pub fn kind(&self) -> &'static str {
        match self {
            Owner::Driver { .. } => "driver",
            Owner::Service { .. } => "service",
            Owner::Kernel { .. } => "kernel",
            Owner::Nobody { .. } => "nobody",
        }
    }
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
        /// May `uid` see the list now?
        fn allowed(&self, uid: u32) -> bool;
    }

    pub struct Shared {
        host: Box<dyn Host>,
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        devices: Vec<Device>,
        generation: u64,
        subscribers: Vec<(u32, mpsc::Sender<u64>)>,
    }

    impl Shared {
        pub fn new(host: Box<dyn Host>) -> Arc<Self> {
            Arc::new(Self { host, state: Mutex::new(State::default()) })
        }

        /// The list now. Subscribers hear a new generation only if it
        /// changed; a session change drops the ones no longer allowed.
        pub fn update(&self, devices: Vec<Device>) {
            let mut st = self.state.lock().unwrap();
            let changed = st.devices != devices;
            if changed {
                st.devices = devices;
                st.generation += 1;
            }
            let g = st.generation;
            let host = &self.host;
            st.subscribers.retain(|(uid, tx)| host.allowed(*uid) && (!changed || tx.send(g).is_ok()));
        }

        pub fn list(&self, uid: u32) -> Option<Vec<Device>> {
            self.host.allowed(uid).then(|| self.state.lock().unwrap().devices.clone())
        }

        fn subscribe(&self, uid: u32) -> Option<(u64, mpsc::Receiver<u64>)> {
            if !self.host.allowed(uid) {
                return None;
            }
            let (tx, rx) = mpsc::channel();
            let mut st = self.state.lock().unwrap();
            st.subscribers.push((uid, tx));
            Some((st.generation, rx))
        }
    }

    fn send_ev(conn: &mut Connection, g: u64) -> io::Result<()> {
        let p = postcard::to_stdvec(&g).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
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
                op::SUBSCRIBE => {
                    let Some((now, rx)) = shared.subscribe(uid) else {
                        conn.send_message(CLASS, op::SUBSCRIBE, flag::IS_RESPONSE, &[status::NOT_ALLOWED])?;
                        continue;
                    };
                    conn.send_message(CLASS, op::SUBSCRIBE, flag::IS_RESPONSE, &[status::OK])?;
                    send_ev(&mut conn, now)?;
                    loop {
                        match rx.recv_timeout(Duration::from_secs(30)) {
                            Ok(g) => send_ev(&mut conn, g)?,
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

    fn call(c: &mut Connection, o: u16) -> io::Result<Vec<u8>> {
        c.send_message(CLASS, o, flag::RESPONSE_EXPECTED, &[])?;
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

    pub fn list(sock: &Path) -> io::Result<Vec<Device>> {
        let mut c = Connection::connect(sock)?;
        let p = call(&mut c, op::LIST)?;
        match p.split_first() {
            Some((&status::OK, rest)) => postcard::from_bytes(rest).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            Some((&st, _)) => Err(refused(st, "list refused")),
            None => Err(io::Error::new(io::ErrorKind::InvalidData, "empty reply")),
        }
    }

    /// Subscribe: the generation now, then a new one on every change, until
    /// the daemon drops the stream (the session changed).
    pub struct Changes(Connection);

    impl Changes {
        pub fn open(sock: &Path) -> io::Result<Self> {
            let mut c = Connection::connect(sock)?;
            match call(&mut c, op::SUBSCRIBE)?.first() {
                Some(&status::OK) => Ok(Self(c)),
                Some(&st) => Err(refused(st, "subscribe refused")),
                None => Err(io::Error::new(io::ErrorKind::InvalidData, "empty reply")),
            }
        }

        /// The next generation (blocking).
        pub fn next(&mut self) -> io::Result<u64> {
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
    }

    fn dev(n: &str, owner: Owner) -> Device {
        Device { name: n.into(), id: 7, bus: "usb".into(), parent: "uhub0".into(), ids: "vendor=0x1234 product=0x5678".into(), owner }
    }

    fn my_uid() -> u32 {
        std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0)
    }

    /// Over a real socket: list, subscribe, a change, no change, a session
    /// change.
    #[test]
    fn over_a_socket() {
        let dir = std::env::temp_dir().join(format!("atrium-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("devices.sock");
        let me = my_uid();
        let session = Arc::new(AtomicU32::new(me));
        let shared = server::Shared::new(Box::new(H(session.clone())));
        let a = dev("ugen0.2:0", Owner::Driver { rule: "usb-ccid".into(), driver: "ccid".into(), running: true });
        shared.update(vec![a.clone()]);
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
        assert_eq!(client::list(&sock).unwrap(), vec![a.clone()]);
        let mut ch = client::Changes::open(&sock).unwrap();
        assert_eq!(ch.next().unwrap(), 1);
        let b = dev("ugen0.3:0", Owner::Nobody { why: "no driver and no rule".into() });
        shared.update(vec![a.clone()]); // the same: no event
        shared.update(vec![a.clone(), b.clone()]);
        assert_eq!(ch.next().unwrap(), 2, "one event, for the change only");
        assert_eq!(client::list(&sock).unwrap(), vec![a, b]);
        session.store(me.wrapping_add(1), Ordering::SeqCst);
        shared.update(vec![]);
        assert!(ch.next().is_err(), "dropped when the session changes");
        assert_eq!(client::list(&sock).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
