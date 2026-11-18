//! The engine's side of one interface: our static key, its peers, and the
//! checks every datagram from the network meets before any secret is used
//! (vpn.md §4.2): the message's shape, mac1 (from our public key, no
//! secret), and under load a cookie for the sender's address.

use std::sync::Arc;
use std::time::Duration;

use crate::crypto;
use crate::key::StaticKey;
use crate::limits::COOKIE_REFRESH_TIME;
use crate::messages::{self, CookieReply, Message};
use crate::noise;
use crate::tunnel::{Action, Event, Peer, Tunnel};

/// Handshake messages per second above which the device asks for cookies.
pub const LOAD_THRESHOLD: u32 = 256;

pub struct Device {
    key: Arc<dyn StaticKey + Send + Sync>,
    mac1_key: [u8; 32],
    cookie_key: [u8; 32],
    secret: ([u8; 32], Duration),
    peers: Vec<Tunnel>,
    /// Handshake messages this second, and which second.
    load: (u32, Duration),
    /// Ask for cookies whatever the load (tests).
    pub force_load: bool,
}

impl Device {
    pub fn new(key: Arc<dyn StaticKey + Send + Sync>) -> Device {
        let public = key.public();
        Device {
            mac1_key: noise::mac1_key(&public),
            cookie_key: noise::cookie_key(&public),
            key,
            secret: (crypto::random(), Duration::ZERO),
            peers: Vec::new(),
            load: (0, Duration::ZERO),
            force_load: false,
        }
    }

    pub fn public(&self) -> [u8; 32] {
        self.key.public()
    }

    /// Add a peer; its number in the device.
    pub fn add_peer(&mut self, peer: Peer) -> usize {
        self.peers.push(Tunnel::new(self.key.clone(), peer));
        self.peers.len() - 1
    }

    pub fn peer(&mut self, i: usize) -> &mut Tunnel {
        &mut self.peers[i]
    }

    pub fn peers(&mut self) -> &mut [Tunnel] {
        &mut self.peers
    }

    fn under_load(&mut self, now: Duration) -> bool {
        let sec = Duration::from_secs(now.as_secs());
        if self.load.1 != sec {
            self.load = (0, sec);
        }
        self.load.0 += 1;
        self.force_load || self.load.0 > LOAD_THRESHOLD
    }

    /// The cookie for a sender at `src` (its address and port, as bytes).
    fn cookie_for(&mut self, src: &[u8], now: Duration) -> [u8; 16] {
        if now.saturating_sub(self.secret.1) >= COOKIE_REFRESH_TIME || self.secret.1 == Duration::ZERO {
            self.secret = (crypto::random(), now.max(Duration::from_nanos(1)));
        }
        crypto::mac(&self.secret.0, &[src])
    }

    fn cookie_reply(&mut self, msg: &[u8], at: usize, receiver: u32, src: &[u8], now: Duration) -> Action {
        let cookie = self.cookie_for(src, now);
        let nonce: [u8; 24] = crypto::random();
        let mac1: [u8; 16] = msg[at..at + 16].try_into().unwrap();
        let sealed = crypto::xseal(&self.cookie_key, &nonce, &mac1, &cookie);
        Action::Send(CookieReply { receiver, nonce, cookie_sealed: sealed }.encode().to_vec())
    }

    /// Check a handshake message's MACs. `Err(action)` when it goes no
    /// further (dropped, or answered with a cookie).
    fn admit(&mut self, msg: &[u8], at: usize, sender: u32, src: &[u8], now: Duration) -> Result<(), Vec<Action>> {
        if !noise::mac1_ok(msg, at, &self.mac1_key) {
            return Err(vec![Action::Event(Event::Dropped("a handshake without a valid mac1"))]);
        }
        if self.under_load(now) {
            let cookie = self.cookie_for(src, now);
            if !noise::mac2_ok(msg, at, &cookie) {
                return Err(vec![self.cookie_reply(msg, at, sender, src, now)]);
            }
        }
        Ok(())
    }

    fn by_index(&self, index: u32) -> Option<usize> {
        self.peers.iter().position(|p| p.owns_index(index))
    }

    /// A datagram from the network, from `src` (its address and port as
    /// bytes, for cookies). The peer it was for, and what to do.
    pub fn decapsulate(&mut self, src: &[u8], datagram: &[u8], now: Duration) -> (Option<usize>, Vec<Action>) {
        let Some(m) = messages::parse(datagram) else {
            return (None, vec![Action::Event(Event::Dropped("not a WireGuard message"))]);
        };
        match m {
            Message::Initiation(i) => {
                if let Err(a) = self.admit(datagram, messages::INITIATION_MAC1, i.sender, src, now) {
                    return (None, a);
                }
                let c = match noise::consume_initiation(&*self.key, &i) {
                    Ok(c) => c,
                    Err(_) => return (None, vec![Action::Event(Event::Dropped("an initiation that does not open"))]),
                };
                match self.peers.iter().position(|p| crypto::eq(&p.peer().public, &c.peer_static)) {
                    Some(n) => (Some(n), self.peers[n].handle_initiation(c, now)),
                    None => (None, vec![Action::Event(Event::Dropped("an initiation from an unknown key"))]),
                }
            }
            Message::Response(r) => {
                if let Err(a) = self.admit(datagram, messages::RESPONSE_MAC1, r.sender, src, now) {
                    return (None, a);
                }
                match self.by_index(r.receiver) {
                    Some(n) => (Some(n), self.peers[n].handle_response(&r, now)),
                    None => (None, vec![Action::Event(Event::Dropped("a response to no initiation of ours"))]),
                }
            }
            Message::CookieReply(c) => match self.by_index(c.receiver) {
                Some(n) => (Some(n), self.peers[n].handle_cookie(&c, now)),
                None => (None, vec![Action::Event(Event::Dropped("a cookie for no initiation of ours"))]),
            },
            Message::Transport(t) => match self.by_index(t.receiver) {
                Some(n) => (Some(n), self.peers[n].handle_transport(&t, now)),
                None => (None, vec![Action::Event(Event::Dropped("no session for that index"))]),
            },
        }
    }
}
