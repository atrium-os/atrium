//! `atrium-crypta-wg`: one WireGuard tunnel (docs/spec/vpn.md §3, §4).
//!
//! Started by cryptad through jaild, in a jail of its own whose network is
//! one epair with pf passing only the gateway's address and port. It gets:
//!
//! - fd 0 (and 1, the same socket): cryptad, length-prefixed postcard
//!   frames (crypta_proto::engine). The first frame is `Start`.
//! - fd 2: its log.
//! - fd 3: the tun, made by atrium-net and passed down as a descriptor:
//!   the engine has no device node. Packets carry a 4-byte address family
//!   header (TUNSIFHEAD).
//!
//! It makes one UDP socket, connected to the gateway, and then enters
//! capability mode. Every packet from the gateway goes through crypta-wg's
//! checks (mac1 before anything else); a packet out of the tunnel is
//! delivered only from the peer's allowed sources, and one into it is sent
//! only to the peer's allowed destinations. The static key's work is done
//! by asking cryptad (`Dh`), which asks Arca.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crypta_proto::engine::{self, FromEngine, Start, ToEngine};
use crypta_proto::net::Prefix4;
use crypta_wg::tunnel::Event;
use crypta_wg::{Action, Device, KeyError, Peer, StaticKey};
use zeroize::Zeroizing;

const AF_INET_HDR: [u8; 4] = 2u32.to_be_bytes();
const TICK: Duration = Duration::from_millis(100);
const STATS_EVERY: Duration = Duration::from_secs(10);
const DH_WAIT_MS: i32 = 5000;
const MAX_PACKET: usize = 65535 + 4;

fn log(s: &str) {
    eprintln!("atrium-crypta-wg: {s}");
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor we own.
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if fl < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn poll_one(fd: RawFd, ms: i32) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, ms) > 0 }
}

/// The socket to cryptad, shared by the main loop and the key (which asks
/// in the middle of a handshake and waits for the answer).
struct Ctl {
    s: UnixStream,
    rd: engine::Reader,
    /// Frames that arrived while the key waited for its answer.
    pending: Vec<ToEngine>,
    gone: bool,
}

impl Ctl {
    /// Read what is there without waiting; false once cryptad is gone.
    fn fill(&mut self) -> bool {
        let mut b = [0u8; 4096];
        loop {
            match self.s.read(&mut b) {
                Ok(0) => {
                    self.gone = true;
                    return false;
                }
                Ok(n) => self.rd.push(&b[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.gone = true;
                    return false;
                }
            }
        }
    }

    fn frames(&mut self) -> Vec<ToEngine> {
        let mut out = std::mem::take(&mut self.pending);
        loop {
            match self.rd.next::<ToEngine>() {
                Ok(Some(m)) => out.push(m),
                Ok(None) => return out,
                Err(_) => {
                    // A frame that does not decode: cryptad is not talking
                    // sense, so stop rather than guess.
                    self.gone = true;
                    return out;
                }
            }
        }
    }

    fn send(&mut self, m: &FromEngine) {
        // Small frames on a socketpair: a short write would mean cryptad
        // stopped reading, which ends us at its next look anyway.
        let _ = engine::write(&mut self.s, m);
    }
}

struct CryptadKey {
    public: [u8; 32],
    ctl: Arc<Mutex<Ctl>>,
}

impl StaticKey for CryptadKey {
    fn public(&self) -> [u8; 32] {
        self.public
    }

    fn dh(&self, point: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, KeyError> {
        let mut c = self.ctl.lock().map_err(|_| KeyError::Unavailable("lock".into()))?;
        c.send(&FromEngine::Dh { point: *point });
        let deadline = Instant::now() + Duration::from_millis(DH_WAIT_MS as u64);
        loop {
            for m in c.frames() {
                match m {
                    ToEngine::Dh(Some(s)) => return Ok(Zeroizing::new(s.0)),
                    ToEngine::Dh(None) => return Err(KeyError::Unavailable("cryptad refused".into())),
                    other => c.pending.push(other),
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if c.gone || left.is_zero() {
                return Err(KeyError::Unavailable("no answer from cryptad".into()));
            }
            let fd = c.s.as_raw_fd();
            if poll_one(fd, left.as_millis() as i32) && !c.fill() {
                return Err(KeyError::Unavailable("cryptad went away".into()));
            }
        }
    }
}

fn in_any(ps: &[Prefix4], a: Ipv4Addr) -> bool {
    ps.iter().any(|p| p.contains(a))
}

/// An IPv4 packet's (source, destination), if it is one: version 4, a
/// header of at least 20 bytes, a total length that matches.
fn v4_addrs(p: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr)> {
    if p.len() < 20 || p[0] >> 4 != 4 || usize::from(p[0] & 15) * 4 < 20 {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([p[2], p[3]]));
    if total != p.len() || usize::from(p[0] & 15) * 4 > total {
        return None;
    }
    Some((Ipv4Addr::new(p[12], p[13], p[14], p[15]), Ipv4Addr::new(p[16], p[17], p[18], p[19])))
}

#[derive(Default)]
struct Counts {
    rx: u64,
    tx: u64,
    dropped: u64,
}

fn limits() {
    // No children, no core files: nothing of a session key on disk.
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: setrlimit with a valid struct.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
        libc::setrlimit(libc::RLIMIT_NPROC, &zero);
    }
}

#[cfg(target_os = "freebsd")]
fn sandbox() -> io::Result<()> {
    // SAFETY: no arguments.
    if unsafe { libc::cap_enter() } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn sandbox() -> io::Result<()> {
    // Host tests only: there is no capability mode here.
    Ok(())
}

fn main() {
    limits();
    // SAFETY: fd 0 is the socket cryptad (or jaild for it) gave us; fd 3 is
    // the tun. Each is owned once, here.
    let ctl_sock = unsafe { UnixStream::from_raw_fd(0) };
    let mut tun = unsafe { std::fs::File::from_raw_fd(3) };
    let start: Start = match engine::read::<ToEngine>(&mut &ctl_sock) {
        Ok(Some(ToEngine::Start(s))) => s,
        Ok(Some(_)) => return log("the first frame was not Start"),
        Ok(None) => return log("cryptad closed before Start"),
        Err(e) => return log(&format!("reading Start: {e}")),
    };
    let udp = match UdpSocket::bind("0.0.0.0:0").and_then(|u| u.connect(SocketAddr::V4(start.endpoint)).map(|_| u)) {
        Ok(u) => u,
        Err(e) => return log(&format!("socket to {}: {e}", start.endpoint)),
    };
    for fd in [ctl_sock.as_raw_fd(), udp.as_raw_fd(), tun.as_raw_fd()] {
        if let Err(e) = nonblocking(fd) {
            return log(&format!("non-blocking: {e}"));
        }
    }
    if let Err(e) = sandbox() {
        return log(&format!("capability mode: {e}"));
    }
    let ctl = Arc::new(Mutex::new(Ctl { s: ctl_sock, rd: engine::Reader::default(), pending: Vec::new(), gone: false }));
    let key = Arc::new(CryptadKey { public: start.our_public, ctl: ctl.clone() });
    let mut dev = Device::new(key);
    dev.add_peer(Peer {
        public: start.peer_public,
        psk: start.psk,
        persistent_keepalive: start.keepalive_s.filter(|s| *s > 0).map(|s| Duration::from_secs(s.into())),
    });
    let src: Vec<u8> = {
        let mut v = start.endpoint.ip().octets().to_vec();
        v.extend_from_slice(&start.endpoint.port().to_be_bytes());
        v
    };
    log(&format!("tunnel to {} ({} allowed prefixes)", start.endpoint, start.allowed.len()));
    let t0 = Instant::now();
    let (mut last_tick, mut last_stats) = (t0, t0);
    let mut up = false;
    let mut n = Counts::default();
    let mut buf = vec![0u8; MAX_PACKET];
    let ctl_fd = ctl.lock().unwrap().s.as_raw_fd();
    loop {
        let mut fds = [
            libc::pollfd { fd: ctl_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: udp.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: tun.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: three valid pollfds.
        unsafe { libc::poll(fds.as_mut_ptr(), 3, TICK.as_millis() as i32) };
        let mut acts: Vec<Action> = Vec::new();
        {
            let mut c = ctl.lock().unwrap();
            if fds[0].revents != 0 {
                c.fill();
            }
            for m in c.frames() {
                match m {
                    ToEngine::Stop => {
                        log("stopping");
                        return;
                    }
                    ToEngine::Rebind => log("network changed: the next packet goes the new way"),
                    _ => log("unexpected frame ignored"),
                }
            }
            if c.gone {
                return log("cryptad went away");
            }
        }
        let now = t0.elapsed();
        if fds[1].revents != 0 {
            for _ in 0..64 {
                match udp.recv(&mut buf) {
                    Ok(len) => {
                        let (_, a) = dev.decapsulate(&src, &buf[..len], now);
                        acts.extend(a);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    // ICMP unreachable from the last send, and the like.
                    Err(_) => break,
                }
            }
        }
        if fds[2].revents != 0 {
            for _ in 0..64 {
                match tun.read(&mut buf) {
                    Ok(0) => return log("the tun is gone"),
                    Ok(len) if len > 4 && buf[..4] == AF_INET_HDR => match v4_addrs(&buf[4..len]) {
                        Some((_, dst)) if in_any(&start.allowed, dst) => acts.extend(dev.peer(0).encapsulate(&buf[4..len], now)),
                        _ => n.dropped += 1,
                    },
                    // IPv6 and anything else: not in V1.
                    Ok(_) => n.dropped += 1,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return log(&format!("tun: {e}")),
                }
            }
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            acts.extend(dev.peer(0).tick(now));
        }
        for a in acts {
            match a {
                Action::Send(d) => {
                    if udp.send(&d).is_ok() {
                        n.tx += d.len() as u64;
                    }
                }
                Action::Deliver(p) => match v4_addrs(&p) {
                    Some((s, _)) if in_any(&start.allowed, s) => {
                        let mut f = Vec::with_capacity(4 + p.len());
                        f.extend_from_slice(&AF_INET_HDR);
                        f.extend_from_slice(&p);
                        if tun.write(&f).is_ok() {
                            n.rx += p.len() as u64;
                        } else {
                            n.dropped += 1;
                        }
                    }
                    _ => n.dropped += 1,
                },
                Action::Event(e) => {
                    let msg = match e {
                        Event::HandshakeComplete { .. } if !up => {
                            up = true;
                            log("handshake complete");
                            Some(FromEngine::Up)
                        }
                        Event::HandshakeGaveUp | Event::KeysErased if up => {
                            up = false;
                            log(&format!("down: {e:?}"));
                            Some(FromEngine::Down { why: format!("{e:?}") })
                        }
                        Event::Dropped(_) => {
                            n.dropped += 1;
                            None
                        }
                        _ => None,
                    };
                    if let Some(m) = msg {
                        ctl.lock().unwrap().send(&m);
                    }
                }
            }
        }
        if last_stats.elapsed() >= STATS_EVERY {
            last_stats = Instant::now();
            ctl.lock().unwrap().send(&FromEngine::Stats { rx: n.rx, tx: n.tx, dropped: n.dropped });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_headers() {
        let mut p = vec![0u8; 28];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&28u16.to_be_bytes());
        p[12..16].copy_from_slice(&[10, 99, 0, 1]);
        p[16..20].copy_from_slice(&[10, 99, 0, 2]);
        assert_eq!(v4_addrs(&p), Some((Ipv4Addr::new(10, 99, 0, 1), Ipv4Addr::new(10, 99, 0, 2))));
        p[2..4].copy_from_slice(&29u16.to_be_bytes());
        assert_eq!(v4_addrs(&p), None);
        p[2..4].copy_from_slice(&28u16.to_be_bytes());
        p[0] = 0x44;
        assert_eq!(v4_addrs(&p), None);
        p[0] = 0x65;
        assert_eq!(v4_addrs(&p), None);
    }
}
