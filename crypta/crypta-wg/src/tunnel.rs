//! One peer: its sessions, the handshake in flight, the queue of packets
//! waiting for keys, and the timers (WireGuard paper §6). Time comes in as
//! a monotonic `Duration` from any fixed start; nothing here reads a clock
//! except the handshake's wall-clock timestamp.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crate::crypto::{self, TAG_LEN};
use crate::key::StaticKey;
use crate::limits::*;
use crate::messages::{self, CookieReply, Transport};
use crate::noise::{self, ConsumedInitiation, InitiatorState, Keypair};
use crate::replay::Window;

/// What a peer is.
#[derive(Clone)]
pub struct Peer {
    pub public: [u8; 32],
    /// WireGuard's optional preshared key (vpn.md §4.3: released by Arca).
    pub psk: Option<[u8; 32]>,
    /// Send a keepalive this often whatever else happens (behind NAT).
    pub persistent_keepalive: Option<Duration>,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Peer {{ public {}.., psk {}, keepalive {:?} }}", hex(&self.public[..4]), self.psk.is_some(), self.persistent_keepalive)
    }
}

/// What the engine does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this datagram to the peer's endpoint.
    Send(Vec<u8>),
    /// Hand this IP packet to `tun`.
    Deliver(Vec<u8>),
    /// Something to log or show (never secret).
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    HandshakeSent,
    HandshakeComplete { initiator: bool },
    /// No response within REKEY_ATTEMPT_TIME: packets waiting were dropped.
    HandshakeGaveUp,
    CookieReceived,
    /// A message was dropped, and why.
    Dropped(&'static str),
    /// Every key is gone after 3 × REJECT_AFTER_TIME of silence.
    KeysErased,
}

struct Session {
    kp: Keypair,
    created: Duration,
    sent: u64,
    window: Window,
}

impl Session {
    fn new(kp: Keypair, now: Duration) -> Session {
        Session { kp, created: now, sent: 0, window: Window::default() }
    }

    fn age(&self, now: Duration) -> Duration {
        now.saturating_sub(self.created)
    }

    fn can_send(&self, now: Duration) -> bool {
        self.age(now) < REJECT_AFTER_TIME && self.sent < REJECT_AFTER_MESSAGES
    }
}

struct Handshake {
    st: InitiatorState,
    sent_at: Duration,
    /// When this round of attempts began (REKEY_ATTEMPT_TIME).
    first_at: Duration,
    retry_after: Duration,
}

pub struct Tunnel {
    key: Arc<dyn StaticKey + Send + Sync>,
    peer: Peer,
    peer_cookie_key: [u8; 32],
    /// Sessions: the one we send with, the one before it (still received
    /// on), and a responder's new one awaiting the initiator's first packet.
    current: Option<Session>,
    previous: Option<Session>,
    next: Option<Session>,
    handshake: Option<Handshake>,
    /// A cookie the peer gave us, and when.
    cookie: Option<([u8; 16], Duration)>,
    /// The newest initiation timestamp accepted from the peer, and when.
    last_initiation: Option<([u8; 12], Duration)>,
    last_timestamp_sent: [u8; 12],
    last_sent: Option<Duration>,
    last_received: Option<Duration>,
    /// When we last sent data (not a keepalive) with nothing received since.
    data_sent_unanswered: Option<Duration>,
    /// A keepalive is owed: data came in and nothing went out since.
    keepalive_owed: Option<Duration>,
    last_keys_at: Option<Duration>,
    queue: VecDeque<Vec<u8>>,
    wall: Box<dyn Fn() -> Duration + Send + Sync>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn jitter() -> Duration {
    Duration::from_millis(u64::from(u16::from_le_bytes(crypto::random())) % 334)
}

/// The length of the IP packet at the start of `p`, if it is one.
fn ip_len(p: &[u8]) -> Option<usize> {
    match p.first()? >> 4 {
        4 if p.len() >= 20 => Some(u16::from_be_bytes([p[2], p[3]]) as usize).filter(|&n| n >= 20 && n <= p.len()),
        6 if p.len() >= 40 => Some(40 + u16::from_be_bytes([p[4], p[5]]) as usize).filter(|&n| n <= p.len()),
        _ => None,
    }
}

impl Tunnel {
    pub fn new(key: Arc<dyn StaticKey + Send + Sync>, peer: Peer) -> Tunnel {
        Tunnel {
            peer_cookie_key: noise::cookie_key(&peer.public),
            key,
            peer,
            current: None,
            previous: None,
            next: None,
            handshake: None,
            cookie: None,
            last_initiation: None,
            last_timestamp_sent: [0; 12],
            last_sent: None,
            last_received: None,
            data_sent_unanswered: None,
            keepalive_owed: None,
            last_keys_at: None,
            queue: VecDeque::new(),
            wall: Box::new(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()),
        }
    }

    /// Replace the wall clock (tests).
    pub fn set_wall_clock(&mut self, f: impl Fn() -> Duration + Send + Sync + 'static) {
        self.wall = Box::new(f);
    }

    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Whether `index` is one of ours for this peer.
    pub fn owns_index(&self, index: u32) -> bool {
        [&self.current, &self.previous, &self.next].iter().any(|s| s.as_ref().is_some_and(|s| s.kp.local_index == index))
            || self.handshake.as_ref().is_some_and(|h| h.st.sender == index)
    }

    /// Whether we have keys to send with now.
    pub fn is_up(&self, now: Duration) -> bool {
        self.current.as_ref().is_some_and(|s| s.can_send(now))
    }

    fn psk(&self) -> [u8; 32] {
        self.peer.psk.unwrap_or([0; 32])
    }

    fn cookie(&self, now: Duration) -> Option<&[u8; 16]> {
        self.cookie.as_ref().filter(|(_, at)| now.saturating_sub(*at) < COOKIE_REFRESH_TIME).map(|(c, _)| c)
    }

    fn timestamp(&mut self) -> [u8; 12] {
        let mut t = crypto::tai64n((self.wall)());
        if t <= self.last_timestamp_sent {
            // Never repeat or go back: the peer would drop it as a replay.
            let mut b = [0u8; 16];
            b[4..].copy_from_slice(&self.last_timestamp_sent);
            let n = u128::from_be_bytes(b) + 1;
            t.copy_from_slice(&n.to_be_bytes()[4..]);
        }
        self.last_timestamp_sent = t;
        t
    }

    fn sent(&mut self, now: Duration) {
        self.last_sent = Some(now);
        self.keepalive_owed = None;
    }

    /// Send a new initiation (a first one, a retry or a rekey).
    fn initiate(&mut self, now: Duration, out: &mut Vec<Action>) {
        let first_at = self.handshake.as_ref().map_or(now, |h| h.first_at);
        let ts = self.timestamp();
        let index = u32::from_le_bytes(crypto::random());
        let cookie = self.cookie(now).copied();
        match noise::create_initiation(&*self.key, &self.peer.public, index, ts, cookie.as_ref()) {
            Ok((msg, st)) => {
                self.handshake = Some(Handshake { st, sent_at: now, first_at, retry_after: REKEY_TIMEOUT + jitter() });
                out.push(Action::Send(msg.to_vec()));
                out.push(Action::Event(Event::HandshakeSent));
                self.sent(now);
            }
            Err(_) => out.push(Action::Event(Event::Dropped("the static key could not be used"))),
        }
    }

    /// A packet from `tun` for this peer.
    pub fn encapsulate(&mut self, packet: &[u8], now: Duration) -> Vec<Action> {
        let mut out = Vec::new();
        if self.current.as_ref().is_some_and(|s| s.can_send(now)) {
            self.seal(packet, now, &mut out);
            self.data_sent_unanswered.get_or_insert(now);
            let s = self.current.as_ref().unwrap();
            if s.kp.initiator && (s.age(now) >= REKEY_AFTER_TIME || s.sent >= REKEY_AFTER_MESSAGES) && self.handshake.is_none() {
                self.initiate(now, &mut out);
            }
        } else {
            if self.queue.len() >= MAX_QUEUED {
                self.queue.pop_front();
            }
            self.queue.push_back(packet.to_vec());
            if self.handshake.is_none() {
                self.initiate(now, &mut out);
            }
        }
        out
    }

    fn seal(&mut self, packet: &[u8], now: Duration, out: &mut Vec<Action>) {
        let s = self.current.as_mut().expect("a session");
        let padded = packet.len().div_ceil(16) * 16;
        let mut buf = vec![0u8; messages::TRANSPORT_HEADER_LEN + padded + TAG_LEN];
        buf[..16].copy_from_slice(&messages::transport_header(s.kp.remote_index, s.sent));
        buf[16..16 + packet.len()].copy_from_slice(packet);
        crypto::seal(&s.kp.send, s.sent, &[], &mut buf[16..], padded);
        s.sent += 1;
        out.push(Action::Send(buf));
        self.sent(now);
    }

    fn send_keepalive(&mut self, now: Duration, out: &mut Vec<Action>) {
        if self.current.as_ref().is_some_and(|s| s.can_send(now)) {
            self.seal(&[], now, out);
        }
    }

    fn install(&mut self, kp: Keypair, now: Duration, out: &mut Vec<Action>) {
        let initiator = kp.initiator;
        let s = Session::new(kp, now);
        self.last_keys_at = Some(now);
        if initiator {
            self.previous = self.current.take();
            self.current = Some(s);
            self.next = None;
            self.handshake = None;
            out.push(Action::Event(Event::HandshakeComplete { initiator: true }));
            // What waited for keys goes now; with nothing waiting, a keepalive
            // confirms the keys to the responder.
            if self.queue.is_empty() {
                self.send_keepalive(now, out);
            }
            while let Some(p) = self.queue.pop_front() {
                self.seal(&p, now, out);
                self.data_sent_unanswered.get_or_insert(now);
            }
        } else {
            // A responder sends with new keys only once the initiator has
            // used them (key confirmation).
            self.next = Some(s);
            out.push(Action::Event(Event::HandshakeComplete { initiator: false }));
        }
    }

    /// An initiation from this peer, opened by the device.
    pub fn handle_initiation(&mut self, c: ConsumedInitiation, now: Duration) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some((ts, at)) = self.last_initiation {
            if c.timestamp <= ts {
                out.push(Action::Event(Event::Dropped("an initiation replayed or out of order")));
                return out;
            }
            if now.saturating_sub(at) < MIN_INITIATION_INTERVAL {
                out.push(Action::Event(Event::Dropped("initiations too fast")));
                return out;
            }
        }
        let index = u32::from_le_bytes(crypto::random());
        let cookie = self.cookie(now).copied();
        match noise::create_response(&c, &self.psk(), index, cookie.as_ref()) {
            Ok((msg, kp)) => {
                self.last_initiation = Some((c.timestamp, now));
                out.push(Action::Send(msg.to_vec()));
                self.sent(now);
                self.install(kp, now, &mut out);
            }
            Err(_) => out.push(Action::Event(Event::Dropped("a response could not be made"))),
        }
        out
    }

    /// A response for our initiation (mac1, and mac2 under load, checked by
    /// the device).
    pub fn handle_response(&mut self, r: &messages::Response, now: Duration) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(h) = self.handshake.as_ref().filter(|h| h.st.sender == r.receiver) else {
            out.push(Action::Event(Event::Dropped("a response to no initiation of ours")));
            return out;
        };
        match noise::consume_response(&*self.key, &h.st, &self.psk(), r) {
            Ok(kp) => {
                self.last_received = Some(now);
                self.install(kp, now, &mut out);
            }
            Err(_) => out.push(Action::Event(Event::Dropped("a response that does not open"))),
        }
        out
    }

    pub fn handle_cookie(&mut self, c: &CookieReply, now: Duration) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(h) = self.handshake.as_ref().filter(|h| h.st.sender == c.receiver) else {
            out.push(Action::Event(Event::Dropped("a cookie for no initiation of ours")));
            return out;
        };
        match crypto::xopen(&self.peer_cookie_key, &c.nonce, &h.st.mac1, &c.cookie_sealed) {
            Some(cookie) => {
                self.cookie = Some((cookie, now));
                out.push(Action::Event(Event::CookieReceived));
            }
            None => out.push(Action::Event(Event::Dropped("a cookie that does not open"))),
        }
        out
    }

    pub fn handle_transport(&mut self, t: &Transport<'_>, now: Duration) -> Vec<Action> {
        let mut out = Vec::new();
        let slot = [&mut self.current, &mut self.previous, &mut self.next]
            .into_iter()
            .position(|s| s.as_ref().is_some_and(|s| s.kp.local_index == t.receiver));
        let Some(slot) = slot else {
            out.push(Action::Event(Event::Dropped("no session for that index")));
            return out;
        };
        let s = match slot {
            0 => self.current.as_mut(),
            1 => self.previous.as_mut(),
            _ => self.next.as_mut(),
        }
        .unwrap();
        if s.age(now) >= REJECT_AFTER_TIME || !s.window.check(t.counter) {
            out.push(Action::Event(Event::Dropped("an old or repeated packet")));
            return out;
        }
        let mut buf = t.sealed.to_vec();
        let Some(n) = crypto::open(&s.kp.recv, t.counter, &[], &mut buf) else {
            out.push(Action::Event(Event::Dropped("a packet that does not open")));
            return out;
        };
        if !s.window.mark(t.counter) {
            out.push(Action::Event(Event::Dropped("an old or repeated packet")));
            return out;
        }
        buf.truncate(n);
        if slot == 2 {
            // The initiator used the new keys: they are ours to send with.
            self.previous = self.current.take();
            self.current = self.next.take();
        }
        self.last_received = Some(now);
        self.data_sent_unanswered = None;
        let initiated_old = self.current.as_ref().is_some_and(|s| s.kp.initiator && s.age(now) >= REJECT_AFTER_TIME - KEEPALIVE_TIMEOUT - REKEY_TIMEOUT);
        if initiated_old && self.handshake.is_none() {
            self.initiate(now, &mut out);
        }
        if buf.is_empty() {
            return out; // a keepalive
        }
        match ip_len(&buf) {
            Some(len) => {
                buf.truncate(len);
                self.keepalive_owed.get_or_insert(now);
                out.push(Action::Deliver(buf));
            }
            None => out.push(Action::Event(Event::Dropped("not an IP packet"))),
        }
        out
    }

    /// The timers. Call at least every few hundred milliseconds.
    pub fn tick(&mut self, now: Duration) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(h) = &self.handshake {
            if now.saturating_sub(h.sent_at) >= h.retry_after {
                if now.saturating_sub(h.first_at) >= REKEY_ATTEMPT_TIME {
                    self.handshake = None;
                    self.queue.clear();
                    out.push(Action::Event(Event::HandshakeGaveUp));
                } else {
                    self.initiate(now, &mut out);
                }
            }
        }
        // Sent data, heard nothing back: start over.
        if let Some(at) = self.data_sent_unanswered {
            if now.saturating_sub(at) >= KEEPALIVE_TIMEOUT + REKEY_TIMEOUT && self.handshake.is_none() {
                self.data_sent_unanswered = None;
                self.initiate(now, &mut out);
            }
        }
        // Heard data, sent nothing back: a keepalive.
        if let Some(at) = self.keepalive_owed {
            if now.saturating_sub(at) >= KEEPALIVE_TIMEOUT {
                self.keepalive_owed = None;
                self.send_keepalive(now, &mut out);
            }
        }
        if let Some(every) = self.peer.persistent_keepalive {
            if self.last_sent.is_none_or(|t| now.saturating_sub(t) >= every) {
                if self.is_up(now) {
                    self.send_keepalive(now, &mut out);
                } else if self.handshake.is_none() {
                    self.initiate(now, &mut out);
                }
            }
        }
        // Sessions past REJECT_AFTER_TIME are no use; after three times that
        // with no new keys, nothing is kept.
        for s in [&mut self.current, &mut self.previous, &mut self.next] {
            if s.as_ref().is_some_and(|x| x.age(now) >= REJECT_AFTER_TIME * 3) {
                *s = None;
            }
        }
        if let Some(at) = self.last_keys_at {
            if now.saturating_sub(at) >= REJECT_AFTER_TIME * 3 && self.handshake.is_none() {
                self.current = None;
                self.previous = None;
                self.next = None;
                self.cookie = None;
                self.queue.clear();
                self.last_keys_at = None;
                out.push(Action::Event(Event::KeysErased));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_lengths_strip_padding_and_refuse_junk() {
        let mut v4 = vec![0u8; 32];
        v4[0] = 0x45;
        v4[3] = 28;
        assert_eq!(ip_len(&v4), Some(28));
        v4[3] = 40;
        assert_eq!(ip_len(&v4), None, "longer than what arrived");
        let mut v6 = vec![0u8; 48];
        v6[0] = 0x60;
        v6[5] = 8;
        assert_eq!(ip_len(&v6), Some(48));
        assert_eq!(ip_len(&[0x50; 40]), None);
    }
}
