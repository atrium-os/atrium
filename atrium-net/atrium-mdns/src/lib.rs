//! `.local` names for unbound (docs/spec/atrium-net.md §6.1).
//!
//! unbound sends queries for `local.` here over TCP (its stub zone); each
//! A question for a name of one to four labels under `local.` becomes one
//! one-shot mDNS query (RFC 6762 §5.1) from a port of its own, so answers
//! come back unicast and never compete with printer discovery for 5353.
//! Answers read off the receive socket are checked here,
//! the only place they are parsed: same name, address on the active
//! interface's subnet or link-local, at most four, TTL clamped.
//!
//! Everything in this file is pure (no sockets), so it is tested on any host.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;

/// How long answers are collected for a question.
pub const WAIT_MS: u64 = 1000;
/// A name nobody answered is not asked again for this long.
pub const NEGATIVE_MS: u64 = 10_000;
/// Questions on the wire at once; more are refused (SERVFAIL).
pub const MAX_PENDING: usize = 8;
pub const MAX_ADDRS: usize = 4;
pub const MAX_TTL: u32 = 120;
/// Records looked at in one mDNS message.
const MAX_RECORDS: usize = 64;

/// A name as lower-case labels, without the root.
pub type Name = Vec<Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    pub rd: bool,
    pub name: Name,
    pub qtype: u16,
    /// The question's bytes as sent (echoed in the answer).
    pub question: Vec<u8>,
}

fn u16at(m: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?]))
}

fn u32at(m: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(m.get(i..i + 4)?.try_into().ok()?))
}

/// A name at `i`: (labels lower-cased, offset after it). Pointers followed
/// (at most 16, each strictly backwards), 255 bytes at most.
pub fn read_name(m: &[u8], mut i: usize) -> Option<(Name, usize)> {
    let mut out: Name = Vec::new();
    let (mut end, mut jumps, mut total) = (None, 0, 0usize);
    loop {
        let l = *m.get(i)? as usize;
        match l & 0xc0 {
            0 if l == 0 => return Some((out, end.unwrap_or(i + 1))),
            0 => {
                let lab = m.get(i + 1..i + 1 + l)?;
                total += l + 1;
                if total > 255 {
                    return None;
                }
                out.push(lab.to_ascii_lowercase());
                i += 1 + l;
            }
            0xc0 => {
                let to = ((l & 0x3f) << 8) | *m.get(i + 1)? as usize;
                jumps += 1;
                if jumps > 16 || to >= i {
                    return None;
                }
                end.get_or_insert(i + 2);
                i = to;
            }
            _ => return None,
        }
    }
}

/// A query from unbound: one question, class IN, not a response.
pub fn parse_query(m: &[u8]) -> Option<Query> {
    let id = u16at(m, 0)?;
    let flags = u16at(m, 2)?;
    if flags & 0x8000 != 0 || (flags >> 11) & 0xf != 0 || u16at(m, 4)? != 1 {
        return None;
    }
    let (name, e) = read_name(m, 12)?;
    let qtype = u16at(m, e)?;
    if u16at(m, e + 2)? & 0x7fff != CLASS_IN {
        return None;
    }
    Some(Query { id, rd: flags & 0x0100 != 0, name, qtype, question: m[12..e + 4].to_vec() })
}

/// A name we ask the link for: one to four host-name labels, then `local`.
pub fn is_local_host(n: &Name) -> bool {
    (2..=5).contains(&n.len())
        && n.last().is_some_and(|l| l == b"local")
        && n[..n.len() - 1].iter().all(|l| {
            !l.is_empty() && l.len() <= 63 && l.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') && l[0] != b'-'
        })
}

fn put_name(out: &mut Vec<u8>, n: &Name) {
    for l in n {
        out.push(l.len() as u8);
        out.extend_from_slice(l);
    }
    out.push(0);
}

/// One mDNS question, sent from a port of our own (a one-shot query, RFC
/// 6762 §5.1): responders answer unicast to that port.
pub fn mdns_query(n: &Name, qtype: u16) -> Vec<u8> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    put_name(&mut q, n);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&CLASS_IN.to_be_bytes());
    q
}

/// The interface's networks: its IPv4 address and prefix, and the IPv6
/// /64s it has addresses in (from the RAs; none: AAAA answers nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub v6: Vec<Ipv6Addr>,
}

impl Net {
    fn mask(&self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix.min(32))
        }
    }

    /// May a `.local` name point here? IPv4: on our subnet (not its network
    /// or broadcast address, not ours) or link-local. IPv6: in one of our
    /// /64s, not its subnet-router anycast address. (IPv6 link-local
    /// needs a zone, which a DNS answer can't carry.)
    pub fn allows_ip(&self, a: IpAddr) -> bool {
        match a {
            IpAddr::V4(a) => self.allows(a),
            IpAddr::V6(a) => {
                let p = |x: &Ipv6Addr| u64::from_be_bytes(x.octets()[..8].try_into().unwrap());
                let iid = u64::from_be_bytes(a.octets()[8..].try_into().unwrap());
                iid != 0 && self.v6.iter().any(|n| p(n) == p(&a))
            }
        }
    }

    pub fn allows(&self, a: Ipv4Addr) -> bool {
        let (m, x, ours) = (self.mask(), u32::from(a), u32::from(self.addr));
        if a.is_link_local() {
            return true;
        }
        if self.prefix < 8 || self.prefix > 30 || x & m != ours & m || x == ours {
            return false;
        }
        let host = x & !m;
        host != 0 && host != !m
    }
}

/// The `qtype` (A or AAAA) records for `want` in an mDNS message that
/// passed the checks: (address, TTL clamped to [`MAX_TTL`]). Responses
/// only; a record that runs past the message ends the reading.
pub fn answers(m: &[u8], want: &Name, qtype: u16, net: &Net) -> Vec<(IpAddr, u32)> {
    let mut out: Vec<(IpAddr, u32)> = Vec::new();
    let Some(flags) = u16at(m, 2) else { return out };
    if flags & 0x8000 == 0 || flags & 0x000f != 0 {
        return out;
    }
    let count = |i| u16at(m, i).unwrap_or(0) as usize;
    let (qd, rrs) = (count(4), count(6) + count(8) + count(10));
    let mut i = 12;
    for _ in 0..qd.min(MAX_RECORDS) {
        let Some((_, e)) = read_name(m, i) else { return out };
        i = e + 4;
    }
    for _ in 0..rrs.min(MAX_RECORDS) {
        let Some((name, e)) = read_name(m, i) else { break };
        let (Some(t), Some(c), Some(ttl), Some(len)) = (u16at(m, e), u16at(m, e + 2), u32at(m, e + 4), u16at(m, e + 8)) else { break };
        let rd = e + 10;
        let Some(data) = m.get(rd..rd + len as usize) else { break };
        i = rd + len as usize;
        if t != qtype || c & 0x7fff != CLASS_IN || &name != want || ttl == 0 {
            continue;
        }
        let a = match (t, data.len()) {
            (TYPE_A, 4) => IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3])),
            (TYPE_AAAA, 16) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(data).unwrap())),
            _ => continue,
        };
        if net.allows_ip(a) && !out.iter().any(|(b, _)| *b == a) && out.len() < MAX_ADDRS {
            out.push((a, ttl.min(MAX_TTL)));
        }
    }
    out
}

pub mod rcode {
    pub const NOERROR: u16 = 0;
    pub const SERVFAIL: u16 = 2;
    pub const NXDOMAIN: u16 = 3;
    pub const REFUSED: u16 = 5;
}

/// unbound's answer: the question echoed, `addrs` as A or AAAA records.
pub fn response(q: &Query, rc: u16, addrs: &[(IpAddr, u32)]) -> Vec<u8> {
    let flags = 0x8000 | 0x0400 | if q.rd { 0x0100 } else { 0 } | rc;
    let mut r = Vec::with_capacity(64);
    r.extend_from_slice(&q.id.to_be_bytes());
    r.extend_from_slice(&flags.to_be_bytes());
    r.extend_from_slice(&[0, 1]);
    r.extend_from_slice(&(addrs.len() as u16).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&q.question);
    for (a, ttl) in addrs {
        let (t, data) = match a {
            IpAddr::V4(a) => (TYPE_A, a.octets().to_vec()),
            IpAddr::V6(a) => (TYPE_AAAA, a.octets().to_vec()),
        };
        r.extend_from_slice(&[0xc0, 0x0c]);
        r.extend_from_slice(&t.to_be_bytes());
        r.extend_from_slice(&CLASS_IN.to_be_bytes());
        r.extend_from_slice(&ttl.to_be_bytes());
        r.extend_from_slice(&(data.len() as u16).to_be_bytes());
        r.extend_from_slice(&data);
    }
    r
}

/// Nobody answered: NXDOMAIN for A; for AAAA only "no data", so a name
/// with an IPv4 address but no IPv6 one is not cached as gone.
fn miss(t: u16) -> u16 {
    if t == TYPE_A {
        rcode::NXDOMAIN
    } else {
        rcode::NOERROR
    }
}

/// Who is waiting for an answer: a connection and its query.
pub type Waiter<C> = (C, Query);

/// A question on the link: a name and A or AAAA.
type Key = (Name, u16);

struct Pending<C> {
    until: u64,
    got: Vec<(IpAddr, u32)>,
    waiters: Vec<Waiter<C>>,
}

/// The resolver without sockets: queries in, mDNS questions and answers out.
pub struct Resolver<C> {
    pub net: Net,
    pending: HashMap<Key, Pending<C>>,
    negative: HashMap<Key, u64>,
}

/// What to do after an event.
#[derive(Debug, PartialEq, Eq)]
pub enum Out<C> {
    /// Send this mDNS query on the send socket.
    Ask(Vec<u8>),
    /// Answer connection `C` with this DNS message.
    Reply(C, Vec<u8>),
}

impl<C: Clone> Resolver<C> {
    pub fn new(net: Net) -> Resolver<C> {
        Resolver { net, pending: HashMap::new(), negative: HashMap::new() }
    }

    /// A query from unbound on connection `c` at `now` (ms).
    pub fn query(&mut self, c: C, m: &[u8], now: u64) -> Vec<Out<C>> {
        let Some(q) = parse_query(m) else { return Vec::new() };
        let reply = |rc, a: &[(IpAddr, u32)]| vec![Out::Reply(c.clone(), response(&q, rc, a))];
        if !is_local_host(&q.name) {
            return reply(rcode::REFUSED, &[]);
        }
        let t = match q.qtype {
            TYPE_A | TYPE_ANY => TYPE_A,
            TYPE_AAAA if !self.net.v6.is_empty() => TYPE_AAAA,
            _ => return reply(rcode::NOERROR, &[]), // no data of that kind
        };
        let key = (q.name.clone(), t);
        self.negative.retain(|_, until| *until > now);
        if self.negative.contains_key(&key) {
            return reply(miss(t), &[]);
        }
        if let Some(p) = self.pending.get_mut(&key) {
            p.waiters.push((c, q));
            return Vec::new();
        }
        if self.pending.len() >= MAX_PENDING {
            return reply(rcode::SERVFAIL, &[]);
        }
        let ask = mdns_query(&q.name, t);
        self.pending.insert(key, Pending { until: now + WAIT_MS, got: Vec::new(), waiters: vec![(c, q)] });
        vec![Out::Ask(ask)]
    }

    /// A datagram from the receive socket.
    pub fn mdns(&mut self, m: &[u8]) {
        for ((name, t), p) in self.pending.iter_mut() {
            for x in answers(m, name, *t, &self.net) {
                if p.got.len() < MAX_ADDRS && !p.got.iter().any(|(a, _)| *a == x.0) {
                    p.got.push(x);
                }
            }
        }
    }

    /// Questions whose time is up: their answers.
    pub fn tick(&mut self, now: u64) -> Vec<Out<C>> {
        let done: Vec<Key> = self.pending.iter().filter(|(_, p)| p.until <= now).map(|(k, _)| k.clone()).collect();
        let mut out = Vec::new();
        for n in done {
            let p = self.pending.remove(&n).unwrap();
            let rc = if p.got.is_empty() { miss(n.1) } else { rcode::NOERROR };
            if p.got.is_empty() {
                self.negative.insert(n, now + NEGATIVE_MS);
            }
            for (c, q) in p.waiters {
                out.push(Out::Reply(c, response(&q, rc, &p.got)));
            }
        }
        out
    }

    /// When the next question is due, if any.
    pub fn next_due(&self) -> Option<u64> {
        self.pending.values().map(|p| p.until).min()
    }
}

#[cfg(test)]
mod tests;
