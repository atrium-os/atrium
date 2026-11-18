//! Interop on the host (vpn.md §11, V0): crypta-wg against boringtun, an
//! independent WireGuard implementation, over real UDP sockets on
//! localhost. Both ways: we initiate, they initiate; data both ways; the
//! preshared key; a cookie when boringtun is "under load".

mod common;
use common::*;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use boringtun::noise::rate_limiter::RateLimiter;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519;
use crypta_wg::tunnel::Event;
use crypta_wg::{Action, Device, LocalKey, Peer, StaticKey};

struct Them {
    tunn: Tunn,
    sock: UdpSocket,
    got: Vec<Vec<u8>>,
}

impl Them {
    /// Feed boringtun one datagram (or a timer pass if `None`) and send
    /// whatever it says to.
    fn take(&mut self, d: Option<&[u8]>, us: &UdpSocket) {
        let mut buf = vec![0u8; 2048];
        let to = us.local_addr().unwrap();
        let mut first = true;
        loop {
            let r = match (d, first) {
                (Some(d), true) => self.tunn.decapsulate(Some(to.ip()), d, &mut buf),
                (Some(_), false) => self.tunn.decapsulate(None, &[], &mut buf),
                (None, _) => self.tunn.update_timers(&mut buf),
            };
            first = false;
            match r {
                TunnResult::WriteToNetwork(p) => {
                    self.sock.send_to(p, to).unwrap();
                    if d.is_none() {
                        break;
                    }
                }
                TunnResult::WriteToTunnelV4(p, _) | TunnResult::WriteToTunnelV6(p, _) => {
                    self.got.push(p.to_vec());
                    break;
                }
                TunnResult::Done => break,
                TunnResult::Err(e) => panic!("boringtun: {e:?}"),
            }
        }
    }

    fn send(&mut self, p: &[u8], us: &UdpSocket) {
        let mut buf = vec![0u8; 2048];
        if let TunnResult::WriteToNetwork(d) = self.tunn.encapsulate(p, &mut buf) {
            self.sock.send_to(d, us.local_addr().unwrap()).unwrap();
        }
    }
}

struct Us {
    dev: Device,
    sock: UdpSocket,
    start: Instant,
    got: Vec<Vec<u8>>,
    events: Vec<Event>,
}

impl Us {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn act(&mut self, acts: Vec<Action>, them: &UdpSocket) {
        for a in acts {
            match a {
                Action::Send(d) => {
                    self.sock.send_to(&d, them.local_addr().unwrap()).unwrap();
                }
                Action::Deliver(p) => self.got.push(p),
                Action::Event(e) => self.events.push(e),
            }
        }
    }
}

fn setup(psk: Option<[u8; 32]>, them_limit: Option<u64>) -> (Us, Them) {
    let ours = Arc::new(LocalKey::generate());
    let their_secret = x25519::StaticSecret::from(crypta_wg::crypto::random::<32>());
    let their_public = x25519::PublicKey::from(&their_secret);
    let our_public = x25519::PublicKey::from(ours.public());
    let limiter = them_limit.map(|l| Arc::new(RateLimiter::new(&their_public, l)));
    let tunn = Tunn::new(their_secret, our_public, psk, None, 7, limiter);
    let mut dev = Device::new(ours);
    dev.add_peer(Peer { public: their_public.to_bytes(), psk, persistent_keepalive: None });
    let us_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let them_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    for s in [&us_sock, &them_sock] {
        s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    }
    (
        Us { dev, sock: us_sock, start: Instant::now(), got: Vec::new(), events: Vec::new() },
        Them { tunn, sock: them_sock, got: Vec::new() },
    )
}

/// Move datagrams both ways until neither side has anything for a while.
fn pump(us: &mut Us, them: &mut Them) {
    let mut buf = vec![0u8; 2048];
    let mut quiet = 0;
    while quiet < 5 {
        let mut moved = false;
        if let Ok((n, from)) = us.sock.recv_from(&mut buf) {
            let src = match from {
                std::net::SocketAddr::V4(a) => [a.ip().octets().to_vec(), a.port().to_be_bytes().to_vec()].concat(),
                std::net::SocketAddr::V6(a) => [a.ip().octets().to_vec(), a.port().to_be_bytes().to_vec()].concat(),
            };
            let now = us.now();
            let (_, acts) = us.dev.decapsulate(&src, &buf[..n], now);
            us.act(acts, &them.sock);
            moved = true;
        }
        if let Ok((n, _)) = them.sock.recv_from(&mut buf) {
            let d = buf[..n].to_vec();
            them.take(Some(&d), &us.sock);
            moved = true;
        }
        quiet = if moved { 0 } else { quiet + 1 };
    }
}

#[test]
fn we_initiate_and_data_goes_both_ways() {
    let (mut us, mut them) = setup(Some([42; 32]), None);
    let p = ipv4(84, 0xab);
    let now = us.now();
    let acts = us.dev.peer(0).encapsulate(&p, now);
    us.act(acts, &them.sock);
    pump(&mut us, &mut them);
    assert!(us.events.contains(&Event::HandshakeComplete { initiator: true }), "{:?}", us.events);
    assert_eq!(them.got, vec![p], "boringtun got our queued packet");
    for i in 0..50u8 {
        them.send(&ipv4(1280, i), &us.sock);
        let now = us.now();
        let acts = us.dev.peer(0).encapsulate(&ipv4(60 + i as usize, i), now);
        us.act(acts, &them.sock);
    }
    pump(&mut us, &mut them);
    assert_eq!(us.got.len(), 50, "all of boringtun's packets arrived and opened");
    assert!(us.got.iter().enumerate().all(|(i, g)| *g == ipv4(1280, i as u8)));
    assert_eq!(them.got.len(), 51);
    assert_eq!(them.got[50], ipv4(109, 49));
}

#[test]
fn they_initiate_and_we_answer() {
    let (mut us, mut them) = setup(None, None);
    let p = ipv4(100, 3);
    them.send(&p, &us.sock);
    pump(&mut us, &mut them);
    assert!(us.events.contains(&Event::HandshakeComplete { initiator: false }), "{:?}", us.events);
    assert_eq!(us.got, vec![p], "boringtun's queued packet confirmed the keys");
    let back = ipv4(200, 4);
    let now = us.now();
    let acts = us.dev.peer(0).encapsulate(&back, now);
    assert_eq!(acts.len(), 1, "we send with the confirmed keys at once");
    us.act(acts, &them.sock);
    pump(&mut us, &mut them);
    assert_eq!(them.got, vec![back]);
}

#[test]
fn a_cookie_from_boringtun_under_load_is_used_on_the_retry() {
    // boringtun asks for a cookie when it sees more than `limit`
    // handshakes a second; 0 means always.
    let (mut us, mut them) = setup(None, Some(0));
    let p = ipv4(64, 9);
    let now = us.now();
    let acts = us.dev.peer(0).encapsulate(&p, now);
    us.act(acts, &them.sock);
    pump(&mut us, &mut them);
    assert!(us.events.contains(&Event::CookieReceived), "{:?}", us.events);
    // The retry after REKEY_TIMEOUT carries mac2 and gets through.
    let later = us.now() + Duration::from_millis(5_400);
    let acts = us.dev.peer(0).tick(later);
    us.act(acts, &them.sock);
    pump(&mut us, &mut them);
    assert!(us.events.contains(&Event::HandshakeComplete { initiator: true }), "{:?}", us.events);
    assert_eq!(them.got, vec![p]);
}

#[test]
fn boringtun_rekeys_and_we_follow() {
    let (mut us, mut them) = setup(None, None);
    them.send(&ipv4(40, 1), &us.sock);
    pump(&mut us, &mut them);
    // A second handshake from them while the first keys are live (as a
    // rekey would): our sessions move over without a lost packet.
    let mut buf = vec![0u8; 2048];
    if let TunnResult::WriteToNetwork(d) = them.tunn.format_handshake_initiation(&mut buf, true) {
        them.sock.send_to(d, us.sock.local_addr().unwrap()).unwrap();
    }
    std::thread::sleep(Duration::from_millis(60));
    pump(&mut us, &mut them);
    let completes = us.events.iter().filter(|e| **e == Event::HandshakeComplete { initiator: false }).count();
    assert_eq!(completes, 2);
    them.send(&ipv4(44, 2), &us.sock);
    pump(&mut us, &mut them);
    let now = us.now();
    let acts = us.dev.peer(0).encapsulate(&ipv4(48, 3), now);
    us.act(acts, &them.sock);
    pump(&mut us, &mut them);
    assert_eq!(us.got, vec![ipv4(40, 1), ipv4(44, 2)]);
    assert_eq!(them.got, vec![ipv4(48, 3)]);
}
