//! vinculumd as a service: one controller, the key store, and the
//! `bluetooth-control` socket (docs/spec/bluetooth.md §13).
//!
//! One poll loop over the controller link, the control listener and its
//! clients. The host core is sans-I/O, so every request and event is
//! handled to completion between polls; the loop only moves bytes.
//!
//! Who may connect: the socket is mode 0660 in a directory the platform
//! guards (Portcullis's `bluetooth-control` capability on Atrium, group
//! permissions on stock FreeBSD), and each client's uid is checked against
//! the allowed list as well. At most [`MAX_CLIENTS`] clients; a client
//! that sends a malformed message, or can't keep up with its events, is
//! dropped.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use vinculum_control::{self as wire, Addr, DeviceInfo, Event, Request};
use vinculum_hci_proto::h4::Decoder;
use vinculum_host::{Host, HostEvent, PairConfig, Peer, Prompt};
use vinculum_keys::{Address, Bond, Id, KeyStore};
use vinculum_smp::pairing::IoCap;

use crate::hid::{self, Hid};
use crate::policy::{self, NewBond, OnSecurityRequest, Pairable};
use crate::Link;

pub const MAX_CLIENTS: usize = 8;
/// An LE connection attempt is cancelled after this long.
pub const LE_CONNECT_TIMEOUT_MS: u64 = 30_000;

struct Client {
    sock: UnixStream,
    reader: wire::Reader,
    hello: bool,
    agent: bool,
    uid: u32,
    /// A write failed (gone, or too slow): dropped by the loop.
    dead: bool,
}

pub struct Config {
    pub control_path: String,
    /// Uids allowed on the control socket, beyond our own.
    pub allowed_uids: Vec<u32>,
    pub io_cap: IoCap,
    /// Our IRK (development: fixed; the real one lives in the key store).
    pub irk: [u8; 16],
    /// The HID hand-off socket for tactusd (§10), and the uids allowed on
    /// it beyond our own (`_tactus`).
    pub hid_path: Option<String>,
    pub hid_uids: Vec<u32>,
}

struct Daemon {
    cfg: Config,
    host: Host,
    store: Box<dyn KeyStore>,
    bonds: Vec<(Id, Bond)>,
    pairable: Pairable,
    clients: Vec<Client>,
    /// The controller is scanning (for a user's scan, or in the
    /// background for bonded LE devices that want to reconnect).
    scanning: bool,
    /// A control client asked for a scan.
    user_scan: bool,
    /// Our LE connection attempt in flight, and when we give up on it.
    le_connecting: Option<(Addr, Option<u64>)>,
    /// A device the user asked to pair, waiting for its connection.
    pending_pair: Option<Addr>,
    conns: HashMap<u16, Addr>,
    names: HashMap<Addr, String>,
    /// Links where we started pairing and `Paired` has not come yet.
    pairing: HashSet<u16>,
    /// Classic devices we accepted a page from, until their link is up;
    /// then their handles.
    paged_by: HashSet<Addr>,
    incoming: HashSet<u16>,
    hid: Hid,
    t0: Instant,
}

fn addr(p: Peer) -> Addr {
    Addr { bytes: p.address, random: p.random }
}

fn peer(a: Addr) -> Peer {
    Peer { address: a.bytes, random: a.random }
}

fn keys_addr(a: Addr) -> Address {
    Address::from_msb(a.bytes, a.random)
}

fn from_keys(a: Address) -> Addr {
    let mut b = a.bytes;
    b.reverse();
    Addr { bytes: b, random: a.kind == 1 }
}

/// The uid on the other end of a Unix socket.
pub(crate) fn peer_uid(s: &UnixStream) -> Option<u32> {
    let fd = s.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for writes of the sizes given;
        // fd is an open socket owned by `s`.
        let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
        (r == 0).then_some(cred.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: valid out-pointers; fd is an open socket owned by `s`.
        let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        (r == 0).then_some(uid)
    }
}

pub(crate) fn getuid() -> u32 {
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

impl Daemon {
    fn now(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn agents(&self) -> usize {
        self.clients.iter().filter(|c| c.agent && !c.dead).count()
    }

    fn send(&mut self, i: usize, e: &Event) {
        let bytes = wire::encode(e);
        if self.clients[i].sock.write_all(&bytes).is_err() {
            // Too slow or gone: its events are not worth stalling the
            // controller for. Marked here, removed by the loop.
            self.clients[i].dead = true;
        }
    }

    fn broadcast(&mut self, e: &Event) {
        for i in 0..self.clients.len() {
            if self.clients[i].hello && !self.clients[i].dead {
                self.send(i, e);
            }
        }
    }

    fn to_agents(&mut self, e: &Event) {
        for i in 0..self.clients.len() {
            if self.clients[i].agent && !self.clients[i].dead {
                self.send(i, e);
            }
        }
    }

    fn bond_for(&self, a: Addr) -> Option<usize> {
        let at = keys_addr(a);
        self.bonds.iter().position(|(_, b)| vinculum_keys::find([b], &at).is_some())
    }

    fn handle_of(&self, a: Addr) -> Option<u16> {
        self.conns.iter().find(|(_, x)| **x == a).map(|(h, _)| *h)
    }

    fn start_pair(&mut self, handle: u16) -> Result<(), String> {
        if !self.pairable.is_open(self.now()) {
            return Err("not pairable now".into());
        }
        if self.agents() == 0 {
            return Err("no pairing agent: open the pairing UI".into());
        }
        let now = self.now();
        self.host.pair(handle, PairConfig { io_cap: self.cfg.io_cap, irk: self.cfg.irk }, now).map_err(|e| e.to_string())?;
        self.pairing.insert(handle);
        Ok(())
    }

    /// Page scan while there are Classic bonds, so they can reconnect.
    fn update_page_scan(&mut self) {
        if self.host.is_ready() && self.hid.enabled() {
            let on = self.bonds.iter().any(|(_, b)| b.classic);
            let now = self.now();
            self.host.page_scan(on, now);
        }
    }

    /// A link is encrypted with a stored bond: run HID on it (SDP and
    /// L2CAP channels on Classic, HID over GATT on LE).
    fn start_hid(&mut self, handle: u16) {
        let Some(&a) = self.conns.get(&handle) else { return };
        let Some(k) = self.bond_for(a) else { return };
        let b = &self.bonds[k].1;
        let (name, kb) = (b.name.clone(), policy::keyboard_allowed(b));
        let now = self.now();
        if self.host.transport(handle) == Some(vinculum_host::Transport::BrEdr) {
            let incoming = self.incoming.contains(&handle);
            self.hid.start(&mut self.host, handle, a, incoming, &name, kb, now);
        } else {
            self.hid.start_le(&mut self.host, handle, a, &name, kb, now);
        }
    }

    // ---- requests -------------------------------------------------------

    fn request(&mut self, i: usize, r: Request) {
        let now = self.now();
        let c = &self.clients[i];
        if !c.hello && !matches!(r, Request::Hello { .. }) {
            return self.send(i, &Event::Error { why: "say Hello first".into() });
        }
        let reply = match r {
            Request::Hello { agent } => {
                let c = &mut self.clients[i];
                c.hello = true;
                c.agent = agent;
                eprintln!("vinculumd: control client uid {}{}", c.uid, if agent { " (pairing agent)" } else { "" });
                Event::Ok
            }
            Request::Status => Event::Status {
                ready: self.host.is_ready(),
                address: self.host.info().address.map(|a| Addr { bytes: a, random: false }),
                scanning: self.user_scan,
                pairable: self.pairable.is_open(now),
                agents: self.agents() as u32,
                connections: self.conns.len() as u32,
            },
            Request::Scan { on } => {
                if !self.host.is_ready() {
                    Event::Error { why: "controller not ready".into() }
                } else {
                    self.user_scan = on;
                    self.update_scan();
                    Event::Ok
                }
            }
            Request::Pair { address } => self.req_pair(address, false),
            Request::PairClassic { address } => self.req_pair(address, true),
            Request::Connect { address } => {
                if self.bond_for(address).is_none() {
                    Event::Error { why: "not a bonded device; pair it first".into() }
                } else if self.handle_of(address).is_some() {
                    Event::Ok
                } else {
                    let classic = self.bond_for(address).is_some_and(|k| self.bonds[k].1.classic);
                    if classic {
                        self.host.classic_connect(peer(address), now);
                    } else {
                        self.le_connect(address);
                    }
                    Event::Ok
                }
            }
            Request::Disconnect { address } => match self.handle_of(address) {
                Some(h) => {
                    self.host.disconnect(h, now);
                    Event::Ok
                }
                None => Event::Error { why: "not connected".into() },
            },
            Request::Devices => Event::Devices {
                devices: self
                    .bonds
                    .iter()
                    .map(|(_, b)| {
                        let a = from_keys(b.address);
                        DeviceInfo {
                            address: a,
                            identity: b.identity.map(from_keys),
                            name: b.name.clone(),
                            authenticated: b.authenticated,
                            secure_connections: b.secure_connections,
                            keyboard_allowed: policy::keyboard_allowed(b),
                            connected: self.conns.values().any(|x| vinculum_keys::find([b], &keys_addr(*x)).is_some()),
                        }
                    })
                    .collect(),
            },
            Request::Remove { address } => match self.bond_for(address) {
                Some(k) => {
                    let (id, b) = self.bonds.remove(k);
                    let conn = self.conns.iter().find(|(_, x)| vinculum_keys::find([&b], &keys_addr(**x)).is_some()).map(|(h, _)| *h);
                    if let Some(h) = conn {
                        self.host.disconnect(h, now);
                    }
                    let r = self.store.delete(&id);
                    self.update_page_scan();
                    self.update_scan();
                    match r {
                        Ok(()) => Event::Ok,
                        Err(e) => Event::Error { why: format!("key store: {e}") },
                    }
                }
                None => Event::Error { why: "no such device".into() },
            },
            Request::Confirm { address, yes } => {
                if !self.clients[i].agent {
                    Event::Error { why: "only a pairing agent answers prompts".into() }
                } else if let Some(h) = self.handle_of(address) {
                    self.host.pairing_confirm(h, yes, now);
                    Event::Ok
                } else {
                    Event::Error { why: "not connected".into() }
                }
            }
            Request::Passkey { address, passkey } => {
                if !self.clients[i].agent {
                    Event::Error { why: "only a pairing agent answers prompts".into() }
                } else if let Some(h) = self.handle_of(address) {
                    self.host.pairing_passkey(h, passkey, now);
                    Event::Ok
                } else {
                    Event::Error { why: "not connected".into() }
                }
            }
        };
        self.send(i, &reply);
    }

    /// Bonded LE devices reconnect by advertising: while one is away and
    /// HID is on, scan for it in the background (its resolvable address is
    /// matched against the bond's IRK), then connect.
    fn background_scan_wanted(&self) -> bool {
        self.hid.enabled()
            && self.pending_pair.is_none()
            && self.bonds.iter().any(|(_, b)| !b.classic && !self.conns.values().any(|x| vinculum_keys::find([b], &keys_addr(*x)).is_some()))
    }

    fn update_scan(&mut self) {
        let want = self.host.is_ready() && self.le_connecting.is_none() && (self.user_scan || self.background_scan_wanted());
        if want != self.scanning {
            let now = self.now();
            self.host.le_scan(want, now);
            self.scanning = want;
        }
    }

    /// One LE connection attempt at a time; the scan pauses meanwhile.
    fn le_connect(&mut self, a: Addr) {
        let now = self.now();
        self.le_connecting = Some((a, Some(now + LE_CONNECT_TIMEOUT_MS)));
        self.update_scan();
        self.host.le_connect(peer(a), now);
    }

    fn le_connect_done(&mut self) {
        self.le_connecting = None;
        self.update_scan();
    }

    fn req_pair(&mut self, address: Addr, classic: bool) -> Event {
        if self.agents() == 0 {
            return Event::Error { why: "no pairing agent: open the pairing UI".into() };
        }
        if !self.host.is_ready() {
            return Event::Error { why: "controller not ready".into() };
        }
        let now = self.now();
        // The user asked to pair this device: the window opens (§7).
        self.pairable.asked(now);
        if let Some(h) = self.handle_of(address) {
            return match self.start_pair(h) {
                Ok(()) => Event::Ok,
                Err(why) => Event::Error { why },
            };
        }
        if self.pending_pair.is_some() {
            return Event::Error { why: "another pairing is in progress".into() };
        }
        self.pending_pair = Some(address);
        if classic {
            self.host.classic_connect(peer(address), now);
        } else {
            if self.le_connecting.is_some() {
                self.pending_pair = None;
                return Event::Error { why: "another connection is in progress; try again".into() };
            }
            self.le_connect(address);
        }
        Event::Ok
    }

    // ---- host events ----------------------------------------------------

    fn event(&mut self, ev: HostEvent) {
        let now = self.now();
        match ev {
            HostEvent::Ready(info) => {
                eprintln!("vinculumd: controller ready, HCI version {}, {} bonds", info.hci_version, self.bonds.len());
                self.update_page_scan();
                self.update_scan();
            }
            HostEvent::Fatal(why) => {
                // The service manager restarts us with a fresh controller.
                eprintln!("vinculumd: controller unusable: {why}");
                std::process::exit(1);
            }
            HostEvent::Advertisement { peer, rssi, name } => {
                let a = addr(peer);
                if let Some(n) = &name {
                    self.names.insert(a, n.clone());
                }
                if self.user_scan {
                    self.broadcast(&Event::Found { address: a, name, rssi });
                }
                // A bonded LE device that is away, advertising: reconnect.
                let away = self.bond_for(a).is_some_and(|k| !self.bonds[k].1.classic) && self.handle_of(a).is_none();
                if away && self.le_connecting.is_none() && self.pending_pair.is_none() && self.hid.enabled() {
                    self.le_connect(a);
                }
            }
            HostEvent::Connected { handle, peer, transport, .. } => {
                let a = addr(peer);
                self.conns.insert(handle, a);
                if transport == vinculum_host::Transport::Le {
                    self.le_connect_done();
                }
                if self.paged_by.remove(&a) {
                    self.incoming.insert(handle);
                }
                self.broadcast(&Event::Connected { address: a, encrypted: false });
                if self.pending_pair == Some(a) {
                    self.pending_pair = None;
                    if let Err(why) = self.start_pair(handle) {
                        self.broadcast(&Event::PairingFailed { address: a, why });
                        self.host.disconnect(handle, now);
                    }
                } else if let Some(k) = self.bond_for(a) {
                    let ltk = self.bonds[k].1.ltk;
                    if self.host.encrypt(handle, ltk, now).is_err() {
                        self.host.disconnect(handle, now);
                    }
                } else {
                    // Nobody asked for this link: strangers get none (§7).
                    self.host.disconnect(handle, now);
                }
            }
            HostEvent::ConnectFailed { peer, status } => {
                if let Some(p) = peer {
                    self.paged_by.remove(&addr(p));
                    // A cancelled attempt may not name the peer.
                    if self.le_connecting.is_some_and(|(a, _)| a == addr(p)) || status.contains("UnknownConnection") {
                        self.le_connect_done();
                    }
                }
                if let (Some(p), Some(pp)) = (peer, self.pending_pair) {
                    if addr(p) == pp {
                        self.pending_pair = None;
                        self.broadcast(&Event::PairingFailed { address: pp, why: format!("connect failed: {status}") });
                    }
                }
            }
            HostEvent::Disconnected { handle, reason } => {
                self.pairing.remove(&handle);
                self.incoming.remove(&handle);
                self.hid.link_gone(handle);
                if let Some(a) = self.conns.remove(&handle) {
                    self.broadcast(&Event::Disconnected { address: a, reason });
                }
                self.update_scan();
            }
            HostEvent::L2cap { handle, cid, payload } => {
                // ATT on LE links running HID over GATT; nothing else is
                // handed to anyone on a link (§7).
                if cid == vinculum_gatt::ATT_CID {
                    self.hid.on_att(&mut self.host, handle, &payload, now);
                }
            }
            HostEvent::CommandFailed { op, status } => {
                eprintln!("vinculumd: {op} failed: {status}");
                self.broadcast(&Event::Error { why: format!("{op} failed: {status}") });
            }
            HostEvent::PairingPrompt { handle, prompt } => {
                let Some(&a) = self.conns.get(&handle) else { return };
                if self.agents() == 0 {
                    // The UI went away mid-pairing: decline (§7).
                    match prompt {
                        Prompt::EnterPasskey => self.host.pairing_passkey(handle, None, now),
                        Prompt::Confirm { .. } => self.host.pairing_confirm(handle, false, now),
                        Prompt::Display { .. } => self.host.disconnect(handle, now),
                    }
                    return;
                }
                let p = match prompt {
                    Prompt::Confirm { number } => wire::Prompt::Confirm { number },
                    Prompt::Display { passkey } => wire::Prompt::Display { passkey },
                    Prompt::EnterPasskey => wire::Prompt::EnterPasskey,
                };
                self.to_agents(&Event::Prompt { address: a, prompt: p });
            }
            HostEvent::Paired { handle, bond } => {
                let Some(&a) = self.conns.get(&handle) else { return };
                let name = self.names.get(&a).cloned().unwrap_or_default();
                // The pairing user is the agent that confirmed it; with
                // several agents, the first.
                let uid = self.clients.iter().find(|c| c.agent).map_or(getuid(), |c| c.uid);
                let classic = self.host.transport(handle) == Some(vinculum_host::Transport::BrEdr);
                let rec = Bond::from_pairing(&bond, keys_addr(a), classic, uid, &name);
                let old = self.bonds.iter().position(|(_, x)| {
                    vinculum_keys::find([x], &rec.address).is_some() || (x.identity.is_some() && x.identity == rec.identity)
                });
                if let NewBond::Refuse(why) = policy::new_bond(old.map(|k| &self.bonds[k].1), &rec) {
                    self.broadcast(&Event::PairingFailed { address: a, why: why.into() });
                    self.host.disconnect(handle, now);
                    return;
                }
                match self.store.store(&rec) {
                    Ok(id) => {
                        if let Some(k) = old {
                            let (old_id, _) = self.bonds.remove(k);
                            if let Err(e) = self.store.delete(&old_id) {
                                eprintln!("vinculumd: removing the replaced bond: {e}");
                            }
                        }
                        let (authenticated, keyboard_allowed) = (rec.authenticated, policy::keyboard_allowed(&rec));
                        self.bonds.push((id, rec));
                        self.pairable.close();
                        self.pairing.remove(&handle);
                        self.broadcast(&Event::Paired { address: a, authenticated, keyboard_allowed });
                        if classic {
                            self.update_page_scan();
                        }
                        self.update_scan();
                        self.start_hid(handle);
                    }
                    Err(e) => {
                        // A bond we can't keep is no bond: drop the link.
                        self.broadcast(&Event::PairingFailed { address: a, why: format!("key store: {e}") });
                        self.host.disconnect(handle, now);
                    }
                }
            }
            HostEvent::PairingFailed { handle, why, .. } => {
                self.pairing.remove(&handle);
                if let Some(&a) = self.conns.get(&handle) {
                    self.broadcast(&Event::PairingFailed { address: a, why });
                }
                self.host.disconnect(handle, now);
            }
            HostEvent::Encrypted { handle, .. } => {
                if let Some(&a) = self.conns.get(&handle) {
                    self.broadcast(&Event::Connected { address: a, encrypted: true });
                }
                // A fresh pairing starts HID once its bond is stored.
                if !self.pairing.contains(&handle) {
                    self.start_hid(handle);
                }
            }
            HostEvent::EncryptionFailed { handle, status } => {
                if let Some(&a) = self.conns.get(&handle) {
                    self.broadcast(&Event::Error { why: format!("{a}: encryption with the stored bond failed ({status}); pair it again") });
                }
                self.host.disconnect(handle, now);
            }
            e @ (HostEvent::ChannelOpen { .. } | HostEvent::ChannelData { .. } | HostEvent::ChannelClosed { .. } | HostEvent::ChannelFailed { .. }) => {
                self.hid.on_event(&mut self.host, &e, now);
            }
            HostEvent::ConnectionRequest { peer: p, .. } => {
                // A Classic device paging us: only bonded devices get a
                // link, and it is authenticated by us before use (§7).
                let bonded = self.bond_for(addr(p)).is_some_and(|k| self.bonds[k].1.classic);
                if bonded {
                    self.paged_by.insert(addr(p));
                    self.host.accept_connection(p, now);
                } else {
                    self.host.reject_connection(p, now);
                }
            }
            HostEvent::SecurityRequest { handle, .. } => {
                let Some(&a) = self.conns.get(&handle) else { return };
                let bonded = self.bond_for(a);
                let pairable = self.pairable.is_open(now) && self.agents() > 0;
                match policy::on_security_request(bonded.is_some(), pairable) {
                    OnSecurityRequest::Encrypt => {
                        let ltk = self.bonds[bonded.expect("bonded")].1.ltk;
                        let _ = self.host.encrypt(handle, ltk, now);
                    }
                    OnSecurityRequest::Pair => {
                        let _ = self.start_pair(handle);
                    }
                    OnSecurityRequest::Ignore => {}
                }
            }
        }
    }

    fn accept(&mut self, listener: &UnixListener) {
        let Ok((sock, _)) = listener.accept() else { return };
        let Some(uid) = peer_uid(&sock) else { return };
        if uid != getuid() && !self.cfg.allowed_uids.contains(&uid) {
            eprintln!("vinculumd: control client uid {uid} refused");
            return;
        }
        if self.clients.len() >= MAX_CLIENTS {
            eprintln!("vinculumd: control client refused: {MAX_CLIENTS} already connected");
            return;
        }
        // A client that stops reading must not stall the daemon.
        let _ = sock.set_write_timeout(Some(Duration::from_millis(200)));
        self.clients.push(Client { sock, reader: wire::Reader::default(), hello: false, agent: false, uid, dead: false });
    }

    /// Reads from client `i`; false if it should be dropped.
    fn client_readable(&mut self, i: usize) -> bool {
        let mut buf = [0u8; 4096];
        let n = match self.clients[i].sock.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted || e.kind() == ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        };
        self.clients[i].reader.push(&buf[..n]);
        loop {
            match self.clients[i].reader.next::<Request>() {
                Ok(Some(r)) => self.request(i, r),
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }
}

fn poll(fds: &[RawFd], timeout_ms: i32) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: `p` is a valid, exclusively borrowed pollfd array of the length
    // passed.
    let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
    if r < 0 {
        return vec![false; fds.len()];
    }
    p.iter().map(|x| x.revents != 0).collect()
}

pub fn run(cfg: Config, mut link: Link, store: Box<dyn KeyStore>, bonds: Vec<(Id, Bond)>) -> ! {
    let _ = std::fs::remove_file(&cfg.control_path);
    let listener = UnixListener::bind(&cfg.control_path).unwrap_or_else(|e| {
        eprintln!("vinculumd: control socket {}: {e}", cfg.control_path);
        std::process::exit(1)
    });
    let _ = std::fs::set_permissions(&cfg.control_path, std::fs::Permissions::from_mode(0o660));
    let hid = Hid::new(cfg.hid_path.as_deref(), cfg.hid_uids.clone());
    let mut host = Host::new();
    if hid.enabled() {
        host.register_psm(hid::PSM_CONTROL);
        host.register_psm(hid::PSM_INTERRUPT);
    }
    let mut d = Daemon {
        cfg,
        host,
        store,
        bonds,
        pairable: Pairable::default(),
        clients: Vec::new(),
        scanning: false,
        user_scan: false,
        le_connecting: None,
        pending_pair: None,
        conns: HashMap::new(),
        names: HashMap::new(),
        pairing: HashSet::new(),
        paged_by: HashSet::new(),
        incoming: HashSet::new(),
        hid,
        t0: Instant::now(),
    };
    let trace = std::env::var_os("VINCULUM_TRACE").is_some();
    let mut dec = Decoder::new();
    let mut buf = vec![0u8; 8192];
    let now = d.now();
    d.host.start(now);
    loop {
        while let Some(ev) = d.host.poll_event() {
            d.event(ev);
        }
        while let Some(f) = d.host.poll_frame() {
            if trace {
                eprintln!("> {:02x?}", f.to_bytes());
            }
            if let Err(e) = link.write_all(&f.to_bytes()) {
                eprintln!("vinculumd: transport write: {e}");
                std::process::exit(1);
            }
        }
        if d.host.poll_event_pending() {
            continue;
        }
        d.clients.retain(|c| !c.dead);
        let now = d.now();
        let le_deadline = d.le_connecting.and_then(|(_, t)| t);
        let next = [d.host.deadline(), d.pairable.deadline(), d.hid.deadline(), le_deadline].into_iter().flatten().min();
        let wait_ms = next.map_or(-1, |n| n.saturating_sub(now).min(60_000) as i32);
        let mut fds = vec![link.fd(), listener.as_raw_fd()];
        fds.extend(d.clients.iter().map(|c| c.sock.as_raw_fd()));
        let nclients = d.clients.len();
        let hid_fds = d.hid.fds();
        fds.extend(hid_fds.iter().map(|(fd, _)| *fd));
        let ready = poll(&fds, wait_ms);
        if ready[0] {
            let n = link.read(&mut buf).unwrap_or(0);
            if n == 0 {
                eprintln!("vinculumd: transport closed");
                std::process::exit(1);
            }
            let mut frames = Vec::new();
            if let Err(e) = dec.push(&buf[..n], |f| frames.push(f)) {
                eprintln!("vinculumd: transport stream: {e}");
                std::process::exit(1);
            }
            for f in frames {
                if trace {
                    eprintln!("< {:02x?}", f.to_bytes());
                }
                let now = d.now();
                d.host.on_frame(f, now);
            }
        }
        // Clients first (indices are stable until the retain below), then
        // new ones.
        let mut drop = Vec::new();
        for (k, (_, which)) in hid_fds.iter().enumerate() {
            if ready[2 + nclients + k] {
                let now = d.now();
                d.hid.readable(&mut d.host, *which, now);
            }
        }
        for (i, r) in ready[2..2 + nclients].iter().enumerate() {
            if *r && !d.client_readable(i) {
                drop.push(i);
            }
        }
        for i in drop.into_iter().rev() {
            let c = d.clients.remove(i);
            if c.agent {
                eprintln!("vinculumd: pairing agent left");
            }
        }
        if ready[1] {
            d.accept(&listener);
        }
        let now = d.now();
        d.host.tick(now);
        if let Some((a, Some(t))) = d.le_connecting {
            if now >= t {
                // Not there: give up; ConnectFailed then restarts the scan.
                d.le_connecting = Some((a, None));
                d.host.le_connect_cancel(now);
            }
        }
        for h in d.hid.timed_out(now) {
            eprintln!("vinculumd: ATT transaction timed out; dropping the link");
            d.hid.link_gone(h);
            d.host.disconnect(h, now);
        }
    }
}
