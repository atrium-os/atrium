//! Network settings on Aqueduct (`CLASS_NETCONF`, docs/spec/atrium-net.md §7.1).
//!
//! atrium-net runs as root and takes requests only as lines on its own
//! control socket. Apps reach it through `atrium-netconf`, an unprivileged
//! front (its own uid, capability mode) that serves one socket per
//! capability and translates. The socket a connection came in on is the
//! grant: Portcullis mounts a socket's directory only into the jail of an
//! app whose manifest asks for that capability and that the user approved.
//!
//! | capability (socket dir) | ops |
//! |---|---|
//! | `network-read` | `STATUS`, `SUBSCRIBE` (no addresses, no network names) |
//! | `network-details` | those, `INTERFACES`, `ROUTES` |
//! | `network-wifi` | those of `network-read` with the network's name, `WIFI_PROFILES`, `WIFI_JOIN`, `WIFI_LEAVE`, `WIFI_FORGET`, `WIFI_SET_PROFILE` |
//! | `wifi-nearby` | those of `network-read`, `WIFI_NEARBY` (SSID, security, signal bars; never BSSIDs). Nearby networks tell where you are, so it is its own capability with its own consent line |
//! | `network-configure` | those of `network-read`, `SET_COUNTRY`, `SET_STATIC`, `SET_DNS`. Restricted to the settings app |
//!
//! Every reply (`IS_RESPONSE`) is `u8 status`, then on `OK` the postcard
//! value, otherwise a postcard `String` saying why. `SUBSCRIBE` answers
//! `OK`, then sends `EV` (`ASYNC_EVENT`) with a [`Status`] now and on every
//! change.

use serde::{Deserialize, Serialize};

pub use aqueduct::classes::CLASS_NETCONF as CLASS;

pub mod op {
    pub const STATUS: u16 = 0x00;
    pub const SUBSCRIBE: u16 = 0x01;
    pub const INTERFACES: u16 = 0x10;
    pub const ROUTES: u16 = 0x11;
    pub const WIFI_PROFILES: u16 = 0x20;
    pub const WIFI_JOIN: u16 = 0x21;
    pub const WIFI_LEAVE: u16 = 0x22;
    pub const WIFI_FORGET: u16 = 0x23;
    pub const WIFI_SET_PROFILE: u16 = 0x24;
    pub const WIFI_NEARBY: u16 = 0x30;
    pub const SET_COUNTRY: u16 = 0x40;
    pub const SET_STATIC: u16 = 0x41;
    pub const SET_DNS: u16 = 0x42;
    pub const EV: u16 = 0x80;
}

pub mod status {
    pub const OK: u8 = 0;
    pub const MALFORMED: u8 = 1;
    /// This socket's capability does not cover the op.
    pub const NOT_ALLOWED: u8 = 2;
    /// Not built yet (static addresses, own DNS servers).
    pub const UNSUPPORTED: u8 = 3;
    /// Tried and did not work (a join refused, a wrong passphrase).
    pub const FAILED: u8 = 4;
    /// The machine has no Wi-Fi.
    pub const NO_WIFI: u8 = 5;
    /// atrium-net did not answer.
    pub const UNAVAILABLE: u8 = 6;
}

/// Which capability a socket stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Read,
    Details,
    Wifi,
    Nearby,
    Configure,
}

impl Grant {
    pub const ALL: [Grant; 5] = [Grant::Read, Grant::Details, Grant::Wifi, Grant::Nearby, Grant::Configure];

    /// The capability's name, which is also its socket directory and file
    /// (`/atrium/sockets/<name>/<name>.sock`).
    pub fn name(self) -> &'static str {
        match self {
            Grant::Read => "network-read",
            Grant::Details => "network-details",
            Grant::Wifi => "network-wifi",
            Grant::Nearby => "wifi-nearby",
            Grant::Configure => "network-configure",
        }
    }

    pub fn allows(self, o: u16) -> bool {
        use op::*;
        match o {
            STATUS | SUBSCRIBE => true,
            INTERFACES | ROUTES => self == Grant::Details,
            WIFI_PROFILES | WIFI_JOIN | WIFI_LEAVE | WIFI_FORGET | WIFI_SET_PROFILE => self == Grant::Wifi,
            WIFI_NEARBY => self == Grant::Nearby,
            SET_COUNTRY | SET_STATIC | SET_DNS => self == Grant::Configure,
            _ => false,
        }
    }

    /// Status carries the network's name and a captive portal's page only
    /// for an app that may manage Wi-Fi or see what is around.
    pub fn sees_names(self) -> bool {
        matches!(self, Grant::Wifi | Grant::Nearby)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Online {
    /// Not probed (probing is off until Atrium runs its own endpoint).
    Unknown,
    Yes,
    No,
    /// A sign-in page is in the way.
    Captive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    None,
    Wired,
    Wifi,
}

/// What the bar shows: how the machine reaches the network now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub link: Link,
    pub online: Online,
    pub metered: bool,
    /// Wi-Fi signal, 0 to 4 bars.
    pub bars: Option<u8>,
    /// The Wi-Fi network's name ([`Grant::sees_names`] only).
    pub ssid: Option<Vec<u8>>,
    /// A captive portal's sign-in page ([`Grant::sees_names`] only).
    pub portal: Option<String>,
}

impl Status {
    pub fn offline() -> Status {
        Status { link: Link::None, online: Online::No, metered: false, bars: None, ssid: None, portal: None }
    }

    pub fn for_grant(mut self, g: Grant) -> Status {
        if !g.sees_names() {
            self.ssid = None;
            self.portal = None;
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Iface {
    pub name: String,
    pub mac: String,
    pub wifi: bool,
    /// `up`, `down`, or `lost` (Wi-Fi gone, address kept a little longer).
    pub link: String,
    /// `a.b.c.d/prefix`.
    pub addr: Option<String>,
    pub router: Option<String>,
    pub dns: Vec<String>,
    pub addrs6: Vec<String>,
    pub dns6: Vec<String>,
    pub online: Online,
    pub metered: bool,
    /// Carries the default route.
    pub egress: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routes {
    /// The IPv4 default route: interface and router.
    pub default4: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub ssid: Vec<u8>,
    pub open: bool,
    /// Set by the network (Transition Disable); never cleared.
    pub wpa3_only: bool,
    /// `network` (one address per network), `random` (new each join) or `device`.
    pub mac: String,
    pub auto: bool,
    pub prio: i32,
    pub metered: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Join {
    pub ssid: Vec<u8>,
    /// Remember the network with this passphrase first (passed to aura-keys,
    /// kept nowhere else). None: join a network already remembered.
    pub passphrase: Option<String>,
    pub open: bool,
}

impl std::fmt::Debug for Join {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Join").field("ssid", &self.ssid).field("passphrase", &self.passphrase.as_ref().map(|_| "..")).field("open", &self.open).finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nearby {
    pub ssid: Vec<u8>,
    /// `open`, `wpa2`, `wpa3`, `wpa2/wpa3`, ...
    pub security: String,
    pub bars: u8,
    /// Remembered already.
    pub known: bool,
}

/// Signal bars from dBm.
pub fn bars(rssi: i32) -> u8 {
    match rssi {
        r if r >= -55 => 4,
        r if r >= -67 => 3,
        r if r >= -75 => 2,
        r if r >= -85 => 1,
        _ => 0,
    }
}

/// A refusal: a status other than OK and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fail(pub u8, pub String);

impl Fail {
    pub fn new(st: u8, why: &str) -> Fail {
        Fail(st, why.to_string())
    }
}

pub type Res<T> = Result<T, Fail>;

fn enc<T: Serialize>(r: &Res<T>) -> Vec<u8> {
    match r {
        Ok(v) => {
            let mut p = vec![status::OK];
            p.extend(postcard::to_stdvec(v).unwrap_or_default());
            p
        }
        Err(Fail(st, why)) => {
            let mut p = vec![*st];
            p.extend(postcard::to_stdvec(why).unwrap_or_default());
            p
        }
    }
}

fn dec<T: for<'a> Deserialize<'a>>(p: &[u8]) -> Res<T> {
    match p.split_first() {
        Some((&status::OK, rest)) => postcard::from_bytes(rest).map_err(|e| Fail(status::MALFORMED, e.to_string())),
        Some((&st, rest)) => Err(Fail(st, postcard::from_bytes(rest).unwrap_or_default())),
        None => Err(Fail(status::MALFORMED, "empty reply".into())),
    }
}

pub mod server {
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    use aqueduct::{flag, Connection};

    use super::*;

    /// What the front does for each op (atrium-netconf talks to atrium-net;
    /// tests use a fake).
    pub trait Backend: Send + Sync {
        fn status(&self) -> Res<Status>;
        /// Status now and on every change, until the receiver is dropped.
        fn subscribe(&self) -> Res<mpsc::Receiver<Status>>;
        fn interfaces(&self) -> Res<Vec<Iface>>;
        fn routes(&self) -> Res<Routes>;
        fn profiles(&self) -> Res<Vec<Profile>>;
        fn join(&self, j: Join) -> Res<()>;
        fn leave(&self) -> Res<()>;
        fn forget(&self, ssid: Vec<u8>) -> Res<()>;
        fn set_profile(&self, p: Profile) -> Res<()>;
        fn nearby(&self) -> Res<Vec<Nearby>>;
        fn set_country(&self, cc: String) -> Res<()>;
    }

    fn arg<T: for<'a> Deserialize<'a>>(p: &[u8]) -> Res<T> {
        postcard::from_bytes(p).map_err(|_| Fail::new(status::MALFORMED, "request does not parse"))
    }

    fn valid_ssid(s: &[u8]) -> Res<()> {
        if s.is_empty() || s.len() > 32 {
            return Err(Fail::new(status::MALFORMED, "SSID is 1 to 32 bytes"));
        }
        Ok(())
    }

    /// Serve one connection that came in on `grant`'s socket.
    pub fn serve(mut conn: Connection, grant: Grant, b: &dyn Backend) -> io::Result<()> {
        loop {
            let m = conn.recv_message()?;
            if m.opcode_class != CLASS {
                continue;
            }
            let o = m.op;
            if !grant.allows(o) {
                let r: Res<()> = Err(Fail(status::NOT_ALLOWED, format!("not with {}", grant.name())));
                conn.send_message(CLASS, o, flag::IS_RESPONSE, &enc(&r))?;
                continue;
            }
            let p = &m.payload;
            let reply = match o {
                op::STATUS => enc(&b.status().map(|s| s.for_grant(grant))),
                op::SUBSCRIBE => match b.subscribe() {
                    Err(f) => enc::<()>(&Err(f)),
                    Ok(rx) => {
                        conn.send_message(CLASS, o, flag::IS_RESPONSE, &enc(&Ok(())))?;
                        let mut last: Option<Status> = None;
                        loop {
                            match rx.recv_timeout(Duration::from_secs(30)) {
                                Ok(s) => {
                                    let s = s.for_grant(grant);
                                    if last.as_ref() != Some(&s) {
                                        let ev = postcard::to_stdvec(&s).unwrap_or_default();
                                        conn.send_message(CLASS, op::EV, flag::ASYNC_EVENT, &ev)?;
                                        last = Some(s);
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                            }
                        }
                    }
                },
                op::INTERFACES => enc(&b.interfaces()),
                op::ROUTES => enc(&b.routes()),
                op::WIFI_PROFILES => enc(&b.profiles()),
                op::WIFI_JOIN => enc(&arg::<Join>(p).and_then(|j| {
                    valid_ssid(&j.ssid)?;
                    if let Some(pw) = &j.passphrase {
                        if j.open || !(8..=63).contains(&pw.len()) || !pw.bytes().all(|c| (0x20..0x7f).contains(&c)) {
                            return Err(Fail::new(status::MALFORMED, "a passphrase is 8 to 63 printable characters"));
                        }
                    }
                    b.join(j)
                })),
                op::WIFI_LEAVE => enc(&b.leave()),
                op::WIFI_FORGET => enc(&arg::<Vec<u8>>(p).and_then(|s| {
                    valid_ssid(&s)?;
                    b.forget(s)
                })),
                op::WIFI_SET_PROFILE => enc(&arg::<Profile>(p).and_then(|x| {
                    valid_ssid(&x.ssid)?;
                    if !matches!(x.mac.as_str(), "network" | "random" | "device") {
                        return Err(Fail::new(status::MALFORMED, "mac is network, random or device"));
                    }
                    b.set_profile(x)
                })),
                op::WIFI_NEARBY => enc(&b.nearby()),
                op::SET_COUNTRY => enc(&arg::<String>(p).and_then(|cc| {
                    if cc.len() != 2 || !cc.bytes().all(|c| c.is_ascii_alphabetic()) {
                        return Err(Fail::new(status::MALFORMED, "a country is two letters"));
                    }
                    b.set_country(cc)
                })),
                op::SET_STATIC | op::SET_DNS => enc::<()>(&Err(Fail::new(status::UNSUPPORTED, "not built yet"))),
                _ => enc::<()>(&Err(Fail::new(status::MALFORMED, "unknown op"))),
            };
            conn.send_message(CLASS, o, flag::IS_RESPONSE, &reply)?;
        }
    }
}

pub mod client {
    use std::io;
    use std::path::Path;

    use aqueduct::{flag, Connection};

    use super::*;

    /// A connection to one of the capability sockets.
    pub struct Client(Connection);

    impl Client {
        pub fn connect(sock: &Path) -> io::Result<Client> {
            Ok(Client(Connection::connect(sock)?))
        }

        /// The socket for `g` under `dir` (`/atrium/sockets` in a jail).
        pub fn path(dir: &Path, g: Grant) -> std::path::PathBuf {
            dir.join(g.name()).join(format!("{}.sock", g.name()))
        }

        fn call<A: Serialize, T: for<'a> Deserialize<'a>>(&mut self, o: u16, a: &A) -> Res<T> {
            let p = postcard::to_stdvec(a).map_err(|e| Fail(status::MALFORMED, e.to_string()))?;
            let io = |e: io::Error| Fail(status::UNAVAILABLE, e.to_string());
            self.0.send_message(CLASS, o, flag::RESPONSE_EXPECTED, &p).map_err(io)?;
            loop {
                let m = self.0.recv_message().map_err(io)?;
                if m.opcode_class == CLASS && m.op == o && m.flags & flag::IS_RESPONSE != 0 {
                    return dec(&m.payload);
                }
            }
        }

        pub fn status(&mut self) -> Res<Status> {
            self.call(op::STATUS, &())
        }
        pub fn interfaces(&mut self) -> Res<Vec<Iface>> {
            self.call(op::INTERFACES, &())
        }
        pub fn routes(&mut self) -> Res<Routes> {
            self.call(op::ROUTES, &())
        }
        pub fn profiles(&mut self) -> Res<Vec<Profile>> {
            self.call(op::WIFI_PROFILES, &())
        }
        pub fn join(&mut self, j: &Join) -> Res<()> {
            self.call(op::WIFI_JOIN, j)
        }
        pub fn leave(&mut self) -> Res<()> {
            self.call(op::WIFI_LEAVE, &())
        }
        pub fn forget(&mut self, ssid: &[u8]) -> Res<()> {
            self.call(op::WIFI_FORGET, &ssid.to_vec())
        }
        pub fn set_profile(&mut self, p: &Profile) -> Res<()> {
            self.call(op::WIFI_SET_PROFILE, p)
        }
        pub fn nearby(&mut self) -> Res<Vec<Nearby>> {
            self.call(op::WIFI_NEARBY, &())
        }
        pub fn set_country(&mut self, cc: &str) -> Res<()> {
            self.call(op::SET_COUNTRY, &cc.to_string())
        }
        pub fn set_static(&mut self) -> Res<()> {
            self.call(op::SET_STATIC, &())
        }

        /// Subscribe: the connection then carries only status events.
        pub fn subscribe(mut self) -> Res<Changes> {
            self.call::<(), ()>(op::SUBSCRIBE, &())?;
            Ok(Changes(self.0))
        }
    }

    pub struct Changes(Connection);

    impl Changes {
        /// The next status (blocking).
        pub fn next(&mut self) -> io::Result<Status> {
            loop {
                let m = self.0.recv_message()?;
                if m.opcode_class == CLASS && m.op == op::EV {
                    return postcard::from_bytes(&m.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            }
        }

        /// Wait at most `t` for the next status.
        pub fn next_within(&mut self, t: std::time::Duration) -> io::Result<Option<Status>> {
            self.0.set_read_timeout(Some(t))?;
            let r = match self.next() {
                Ok(s) => Ok(Some(s)),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
                Err(e) => Err(e),
            };
            self.0.set_read_timeout(None)?;
            r
        }
    }
}

pub mod lines;

#[cfg(test)]
mod tests;
