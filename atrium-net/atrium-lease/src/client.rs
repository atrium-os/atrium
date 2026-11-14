//! The DHCPv4 client state machine (RFC 2131 §4.4), driven by time and
//! received messages; no I/O here. Timers are milliseconds since an
//! arbitrary start.

use crate::dhcp::{self, Kind, Msg, Reply};
use net_proto::{LeaseV4, Route, MAX_DNS};
use std::net::Ipv4Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    Broadcast,
    Server(Ipv4Addr),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    Send { to: Dest, payload: Vec<u8> },
    /// Apply this lease (atrium-net checks it first).
    Bound(LeaseV4),
    /// Remove the address: lease expired, NAK, or link gone.
    Lost(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum St {
    Idle,
    Selecting,
    Requesting { offer: Ipv4Addr, server: Ipv4Addr },
    Rebooting { addr: Ipv4Addr },
    Bound,
    Renewing,
    Rebinding,
}

#[derive(Clone, Debug)]
struct Held {
    lease: LeaseV4,
    t2: u64,
    expiry: u64,
}

pub struct Client {
    mac: [u8; 6],
    st: St,
    xid: u32,
    tries: u32,
    started: u64,
    deadline: Option<u64>,
    held: Option<Held>,
    rand: Box<dyn FnMut() -> u32 + Send>,
}

const REQUEST_TRIES: u32 = 4;
const REBOOT_TRIES: u32 = 2;
const REFUSED_BACKOFF_MS: u64 = 10_000;

impl Client {
    pub fn new(mac: [u8; 6], rand: Box<dyn FnMut() -> u32 + Send>) -> Client {
        Client { mac, st: St::Idle, xid: 0, tries: 0, started: 0, deadline: None, held: None, rand }
    }

    pub fn state(&self) -> &'static str {
        match self.st {
            St::Idle => "idle",
            St::Selecting => "selecting",
            St::Requesting { .. } => "requesting",
            St::Rebooting { .. } => "rebooting",
            St::Bound => "bound",
            St::Renewing => "renewing",
            St::Rebinding => "rebinding",
        }
    }

    pub fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    pub fn lease(&self) -> Option<&LeaseV4> {
        self.held.as_ref().map(|h| &h.lease)
    }

    /// RFC 2131 §4.1: 4 s, doubling to 64 s, plus or minus 1 s.
    fn backoff(&mut self, now: u64) -> u64 {
        let base = 4000u64 << self.tries.min(4);
        let jitter = (self.rand)() as u64 % 2001;
        self.tries += 1;
        now + base + jitter - 1000
    }

    fn msg(&self, kind: Kind, now: u64) -> Msg {
        Msg {
            kind,
            xid: self.xid,
            secs: ((now - self.started) / 1000).min(u16::MAX as u64) as u16,
            chaddr: self.mac,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            requested: None,
            server_id: None,
        }
    }

    fn fresh(&mut self, now: u64) {
        self.xid = (self.rand)();
        self.tries = 0;
        self.started = now;
    }

    /// Link up. With a lease still valid (a roam, a flap) confirm it first.
    pub fn start(&mut self, now: u64) -> Vec<Out> {
        self.fresh(now);
        match self.held.as_ref().filter(|h| now < h.expiry).map(|h| h.lease.addr) {
            Some(addr) => {
                self.st = St::Rebooting { addr };
                self.send_reboot(now)
            }
            None => {
                self.held = None;
                self.st = St::Selecting;
                self.send_discover(now)
            }
        }
    }

    /// Link down: stop sending; keep the lease to confirm on the next link up.
    pub fn stop(&mut self) {
        self.st = St::Idle;
        self.deadline = None;
    }

    /// atrium-net refused the lease (§5.2): forget it, start over later.
    pub fn refused(&mut self, now: u64) {
        self.held = None;
        self.st = St::Selecting;
        self.fresh(now);
        self.deadline = Some(now + REFUSED_BACKOFF_MS);
    }

    fn send_discover(&mut self, now: u64) -> Vec<Out> {
        let m = self.msg(Kind::Discover, now);
        self.deadline = Some(self.backoff(now));
        vec![Out::Send { to: Dest::Broadcast, payload: dhcp::build(&m) }]
    }

    fn send_request(&mut self, now: u64, offer: Ipv4Addr, server: Ipv4Addr) -> Vec<Out> {
        let mut m = self.msg(Kind::Request, now);
        m.requested = Some(offer);
        m.server_id = Some(server);
        self.deadline = Some(self.backoff(now));
        vec![Out::Send { to: Dest::Broadcast, payload: dhcp::build(&m) }]
    }

    fn send_reboot(&mut self, now: u64) -> Vec<Out> {
        let St::Rebooting { addr } = self.st else { return vec![] };
        let mut m = self.msg(Kind::Request, now);
        m.requested = Some(addr);
        self.deadline = Some(self.backoff(now));
        vec![Out::Send { to: Dest::Broadcast, payload: dhcp::build(&m) }]
    }

    fn send_renew(&mut self, now: u64) -> Vec<Out> {
        let Some(h) = &self.held else { return vec![] };
        let mut m = self.msg(Kind::Request, now);
        m.ciaddr = h.lease.addr;
        let to = if self.st == St::Renewing { Dest::Server(h.lease.server) } else { Dest::Broadcast };
        let next = if self.st == St::Renewing { h.t2 } else { h.expiry };
        // RFC 2131 §4.4.5: half the remaining time, at least 60 s.
        let wait = ((next.saturating_sub(now)) / 2).max(60_000);
        self.deadline = Some((now + wait).min(next));
        vec![Out::Send { to, payload: dhcp::build(&m) }]
    }

    pub fn tick(&mut self, now: u64) -> Vec<Out> {
        if self.deadline.is_none_or(|d| now < d) {
            return vec![];
        }
        // Lease expiry wins over every other timer.
        if let Some(h) = &self.held {
            if now >= h.expiry && matches!(self.st, St::Bound | St::Renewing | St::Rebinding) {
                self.held = None;
                self.st = St::Selecting;
                self.fresh(now);
                let mut out = vec![Out::Lost("expired")];
                out.extend(self.send_discover(now));
                return out;
            }
        }
        match self.st.clone() {
            St::Idle => {
                self.deadline = None;
                vec![]
            }
            St::Selecting => self.send_discover(now),
            St::Requesting { offer, server } => {
                if self.tries >= REQUEST_TRIES {
                    self.st = St::Selecting;
                    self.fresh(now);
                    self.send_discover(now)
                } else {
                    self.send_request(now, offer, server)
                }
            }
            St::Rebooting { .. } => {
                if self.tries >= REBOOT_TRIES {
                    // No answer: the old lease may still be fine but we
                    // cannot tell; start over (RFC 2131 §3.2 allows using
                    // it, we prefer a clean state on a new network).
                    self.held = None;
                    self.st = St::Selecting;
                    self.fresh(now);
                    let mut out = vec![Out::Lost("reboot-unanswered")];
                    out.extend(self.send_discover(now));
                    out
                } else {
                    self.send_reboot(now)
                }
            }
            St::Bound => {
                self.xid = (self.rand)();
                self.st = St::Renewing;
                self.started = now;
                self.send_renew(now)
            }
            St::Renewing => {
                let t2 = self.held.as_ref().map_or(0, |h| h.t2);
                if now >= t2 {
                    self.st = St::Rebinding;
                }
                self.send_renew(now)
            }
            St::Rebinding => self.send_renew(now),
        }
    }

    /// A DHCP payload from `src` (already unwrapped by [`crate::frame`]).
    pub fn rx(&mut self, payload: &[u8], now: u64) -> Vec<Out> {
        let Some(r) = dhcp::parse(payload) else { return vec![] };
        if r.xid != self.xid || r.chaddr != self.mac {
            return vec![];
        }
        match (self.st.clone(), r.kind) {
            (St::Selecting, Kind::Offer) => {
                let Some(server) = r.server_id else { return vec![] };
                if r.yiaddr.is_unspecified() {
                    return vec![];
                }
                self.tries = 0;
                self.st = St::Requesting { offer: r.yiaddr, server };
                self.send_request(now, r.yiaddr, server)
            }
            (St::Requesting { offer, server }, Kind::Ack) if r.yiaddr == offer && r.server_id == Some(server) => self.bind(&r, now),
            (St::Rebooting { addr }, Kind::Ack) if r.yiaddr == addr => self.bind(&r, now),
            (St::Renewing | St::Rebinding, Kind::Ack) if Some(r.yiaddr) == self.held.as_ref().map(|h| h.lease.addr) => self.bind(&r, now),
            (St::Requesting { server, .. }, Kind::Nak) if r.server_id == Some(server) => self.nak(now),
            (St::Rebooting { .. } | St::Renewing | St::Rebinding, Kind::Nak) => self.nak(now),
            _ => vec![],
        }
    }

    fn nak(&mut self, now: u64) -> Vec<Out> {
        let had = self.held.take().is_some();
        self.st = St::Selecting;
        self.fresh(now);
        let mut out = if had { vec![Out::Lost("nak")] } else { vec![] };
        out.extend(self.send_discover(now));
        out
    }

    fn bind(&mut self, r: &Reply, now: u64) -> Vec<Out> {
        let Some(lease) = lease_from(r, self.held.as_ref().map(|h| h.lease.server)) else {
            // An ACK we cannot use: treat like a NAK, without trusting it to
            // remove a working address mid-lease.
            if matches!(self.st, St::Renewing | St::Rebinding) {
                return vec![];
            }
            return self.nak(now);
        };
        let secs = r.lease.unwrap_or(lease.lease_s) as u64 * 1000;
        let t2 = r.t2.map(|t| t as u64 * 1000).filter(|&t| t > 0 && t < secs).unwrap_or(secs * 7 / 8);
        let t1 = r.t1.map(|t| t as u64 * 1000).filter(|&t| t > 0 && t < t2).unwrap_or(secs / 2);
        self.held = Some(Held { lease: lease.clone(), t2: now + t2, expiry: now + secs });
        self.st = St::Bound;
        self.tries = 0;
        self.deadline = Some(now + t1);
        vec![Out::Bound(lease)]
    }
}

/// Netmask to prefix; `None` unless the ones are contiguous.
pub fn prefix_of(mask: Ipv4Addr) -> Option<u8> {
    let m = u32::from(mask);
    let p = m.leading_ones();
    (m.checked_shl(p).unwrap_or(0) == 0).then_some(p as u8)
}

fn lease_from(r: &Reply, prev_server: Option<Ipv4Addr>) -> Option<LeaseV4> {
    let prefix = prefix_of(r.subnet?)?;
    let lease_s = r.lease?;
    let server = r.server_id.or(prev_server)?;
    // RFC 3442: with option 121 present, option 3 is ignored.
    let (router, routes) = match &r.classless {
        Some(v) => {
            let all = dhcp::classless(v)?;
            let def = all.iter().find(|(_, p, _)| *p == 0).map(|(_, _, g)| *g);
            let rest = all.into_iter().filter(|(_, p, _)| *p != 0).map(|(dst, prefix, gw)| Route { dst, prefix, gw }).collect();
            (def, rest)
        }
        None => (r.routers.first().copied(), Vec::new()),
    };
    Some(LeaseV4 {
        addr: r.yiaddr,
        prefix,
        router,
        routes,
        dns: r.dns.iter().copied().take(MAX_DNS).collect(),
        mtu: r.mtu,
        lease_s,
        server,
    })
}
