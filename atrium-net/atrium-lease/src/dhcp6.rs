//! DHCPv6 (RFC 8415), for networks whose routers say so in their RAs:
//! the O flag ("other configuration": DNS servers by Information-Request)
//! and the M flag ("managed": addresses by Solicit/Request, IA_NA only).
//! No prefix delegation, no rapid commit, no reconfigure, no relay or
//! authentication options: what is not read cannot be attacked.
//!
//! Like client.rs this is a state machine without I/O. Messages go out as
//! `send6` payloads that atrium-net checks and puts on the wire itself (to
//! ff02::1:2 from the interface's link-local address); replies come in as
//! frames that atrium-net's bpf filter passes (UDP 547 -> 546).

use std::net::Ipv6Addr;

pub const CLIENT_PORT: u16 = 546;
pub const SERVER_PORT: u16 = 547;

pub const SOLICIT: u8 = 1;
pub const ADVERTISE: u8 = 2;
pub const REQUEST: u8 = 3;
pub const RENEW: u8 = 5;
pub const REBIND: u8 = 6;
pub const REPLY: u8 = 7;
pub const INFO_REQUEST: u8 = 11;

pub const OPT_CLIENTID: u16 = 1;
pub const OPT_SERVERID: u16 = 2;
pub const OPT_IA_NA: u16 = 3;
pub const OPT_IAADDR: u16 = 5;
pub const OPT_ORO: u16 = 6;
pub const OPT_ELAPSED: u16 = 8;
const OPT_STATUS: u16 = 13;
pub const OPT_DNS: u16 = 23;
const OPT_INFO_REFRESH: u16 = 32;

/// RFC 8415 §11.1: a DUID is at most 128 octets.
const MAX_DUID: usize = 128;
/// Servers kept from one reply (as many as RDNSS keeps).
pub const MAX_DNS6: usize = 3;
/// Address lifetimes are capped; the server says again at renewal.
pub const MAX_LIFE_S: u32 = 7 * 86400;
/// Information refresh (RFC 8415 §21.23): default 1 day, at least 10 min.
const INFO_REFRESH_S: u32 = 86400;
const INFO_REFRESH_MIN_S: u32 = 600;
/// Largest message we build; atrium-net refuses bigger ones.
pub const MAX_SEND: usize = 256;

/// DUID-LL (RFC 8415 §11.4): hardware type 1, the interface's MAC. A new MAC
/// (Wi-Fi's per-network address) gives a new DUID, so networks cannot link
/// the two.
pub fn duid_ll(mac: &[u8; 6]) -> [u8; 10] {
    let mut d = [0, 3, 0, 1, 0, 0, 0, 0, 0, 0];
    d[4..].copy_from_slice(mac);
    d
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IaAddr {
    pub addr: Ipv6Addr,
    pub pref: u32,
    pub valid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ia {
    pub iaid: u32,
    pub t1: u32,
    pub t2: u32,
    pub status: u16,
    pub addrs: Vec<IaAddr>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Msg6 {
    pub typ: u8,
    pub xid: [u8; 3],
    pub client: Option<Vec<u8>>,
    pub server: Option<Vec<u8>>,
    pub status: u16,
    pub ia: Vec<Ia>,
    pub dns: Vec<Ipv6Addr>,
    pub refresh: Option<u32>,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// (code, body) of each option in `b`, or `None` if any is cut short.
pub fn options(b: &[u8]) -> Option<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if i + 4 > b.len() {
            return None;
        }
        let (code, len) = (be16(&b[i..]), be16(&b[i + 2..]) as usize);
        if i + 4 + len > b.len() || out.len() == 64 {
            return None;
        }
        out.push((code, &b[i + 4..i + 4 + len]));
        i += 4 + len;
    }
    Some(out)
}

fn ia_na(b: &[u8]) -> Option<Ia> {
    if b.len() < 12 {
        return None;
    }
    let mut ia = Ia { iaid: be32(b), t1: be32(&b[4..]), t2: be32(&b[8..]), status: 0, addrs: Vec::new() };
    for (code, o) in options(&b[12..])? {
        match code {
            OPT_IAADDR => {
                if o.len() < 24 {
                    return None;
                }
                let a: [u8; 16] = o[..16].try_into().ok()?;
                let mut st = 0;
                for (c, s) in options(&o[24..])? {
                    if c == OPT_STATUS && s.len() >= 2 {
                        st = be16(s);
                    }
                }
                // An address the server marks failed is no address.
                if st == 0 && ia.addrs.len() < 4 {
                    ia.addrs.push(IaAddr { addr: Ipv6Addr::from(a), pref: be32(&o[16..]), valid: be32(&o[20..]) });
                }
            }
            OPT_STATUS if o.len() >= 2 => ia.status = be16(o),
            _ => {}
        }
    }
    Some(ia)
}

/// A server message (Advertise or Reply), or `None` for anything malformed.
pub fn parse(b: &[u8]) -> Option<Msg6> {
    if b.len() < 4 || !matches!(b[0], ADVERTISE | REPLY) {
        return None;
    }
    let mut m = Msg6 { typ: b[0], xid: [b[1], b[2], b[3]], ..Default::default() };
    for (code, o) in options(&b[4..])? {
        match code {
            OPT_CLIENTID | OPT_SERVERID => {
                if o.is_empty() || o.len() > MAX_DUID {
                    return None;
                }
                let slot = if code == OPT_CLIENTID { &mut m.client } else { &mut m.server };
                if slot.replace(o.to_vec()).is_some() {
                    return None;
                }
            }
            OPT_IA_NA => {
                if m.ia.len() == 4 {
                    return None;
                }
                m.ia.push(ia_na(o)?);
            }
            OPT_STATUS if o.len() >= 2 => m.status = be16(o),
            OPT_DNS => {
                if o.len() % 16 != 0 {
                    return None;
                }
                for a in o.chunks_exact(16) {
                    let a = Ipv6Addr::from(<[u8; 16]>::try_from(a).ok()?);
                    if m.dns.len() < MAX_DNS6 && net_proto::ordinary_unicast6(a) && !m.dns.contains(&a) {
                        m.dns.push(a);
                    }
                }
            }
            OPT_INFO_REFRESH if o.len() == 4 => m.refresh = Some(be32(o)),
            _ => {}
        }
    }
    Some(m)
}

fn opt(out: &mut Vec<u8>, code: u16, body: &[u8]) {
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// Build a client message. `ia`: IAID and the addresses to ask for again
/// (none in a Solicit or Information-Request).
pub fn build(typ: u8, xid: [u8; 3], duid: &[u8], server: Option<&[u8]>, ia: Option<(u32, &[Ipv6Addr])>, elapsed_cs: u16) -> Vec<u8> {
    let mut m = vec![typ, xid[0], xid[1], xid[2]];
    opt(&mut m, OPT_CLIENTID, duid);
    if let Some(s) = server {
        opt(&mut m, OPT_SERVERID, s);
    }
    if let Some((iaid, addrs)) = ia {
        let mut b = Vec::new();
        b.extend_from_slice(&iaid.to_be_bytes());
        b.extend_from_slice(&[0; 8]); // T1, T2: the server's choice
        for a in addrs {
            let mut x = a.octets().to_vec();
            x.extend_from_slice(&[0; 8]);
            opt(&mut b, OPT_IAADDR, &x);
        }
        opt(&mut m, OPT_IA_NA, &b);
    }
    let oro: &[u8] = if typ == INFO_REQUEST { &[0, 23, 0, 32] } else { &[0, 23] };
    opt(&mut m, OPT_ORO, oro);
    opt(&mut m, OPT_ELAPSED, &elapsed_cs.to_be_bytes());
    m
}

// ---- the client ------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Out6 {
    Send(Vec<u8>),
    /// Add this address (/128; on-link prefixes come from RAs).
    Bound(IaAddr),
    /// Remove the address: expired, refused by the server, or link gone.
    Lost(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum St {
    Idle,
    Soliciting,
    Requesting,
    Bound,
    Renewing,
    Rebinding,
    Informing,
    Informed,
}

struct Held {
    addr: IaAddr,
    t1: u64,
    t2: u64,
    end: u64,
}

pub struct Client6 {
    duid: [u8; 10],
    iaid: u32,
    st: St,
    xid: [u8; 3],
    started: u64,
    next: Option<u64>,
    rt: u64,
    tries: u32,
    server: Option<Vec<u8>>,
    offer: Vec<Ipv6Addr>,
    held: Option<Held>,
    dns: Vec<Ipv6Addr>,
    /// When the Information-Request answer goes stale.
    refresh_at: u64,
    rand: Box<dyn FnMut() -> u32 + Send>,
}

/// Retransmission (RFC 8415 §7.6, §15): initial and maximum timeouts, ms.
/// Solicit and Information-Request back off to 120 s (RFC 3315's figure;
/// 8415's hour is a long wait after a server comes back).
fn timing(st: St) -> (u64, u64) {
    match st {
        St::Soliciting | St::Informing => (1000, 120_000),
        St::Requesting => (1000, 30_000),
        _ => (10_000, 600_000),
    }
}
const REQUEST_TRIES: u32 = 10;
/// After a server says no (NoAddrsAvail and the like): wait this long.
const REFUSED_BACKOFF_MS: u64 = 60_000;

impl Client6 {
    pub fn new(mac: [u8; 6], rand: Box<dyn FnMut() -> u32 + Send>) -> Client6 {
        let mut c = Client6 { duid: duid_ll(&mac), iaid: 0, st: St::Idle, xid: [0; 3], started: 0, next: None, rt: 0, tries: 0, server: None, offer: Vec::new(), held: None, dns: Vec::new(), refresh_at: 0, rand };
        c.iaid = (c.rand)();
        c
    }

    pub fn state(&self) -> &'static str {
        match self.st {
            St::Idle => "idle",
            St::Soliciting => "soliciting",
            St::Requesting => "requesting",
            St::Bound => "bound",
            St::Renewing => "renewing",
            St::Rebinding => "rebinding",
            St::Informing => "informing",
            St::Informed => "informed",
        }
    }

    pub fn deadline(&self) -> Option<u64> {
        self.next
    }

    /// DNS servers from the last Reply (empty: none).
    pub fn dns(&self) -> &[Ipv6Addr] {
        &self.dns
    }

    pub fn addr(&self) -> Option<Ipv6Addr> {
        self.held.as_ref().map(|h| h.addr.addr)
    }

    fn jitter(&mut self, t: u64) -> u64 {
        // +-10% (RFC 8415 §15: RAND in -0.1..0.1).
        let r = (self.rand)() as u64 % 2001;
        t * (9000 + r) / 10_000
    }

    fn begin(&mut self, st: St, now: u64) -> Vec<Out6> {
        self.st = st;
        let x = (self.rand)().to_be_bytes();
        self.xid = [x[0], x[1], x[2]];
        self.started = now;
        self.tries = 0;
        self.rt = 0;
        self.transmit(now)
    }

    fn transmit(&mut self, now: u64) -> Vec<Out6> {
        let (irt, mrt) = timing(self.st);
        self.rt = if self.rt == 0 { self.jitter(irt) } else { self.jitter((self.rt * 2).min(mrt)).min(mrt) };
        self.tries += 1;
        self.next = Some(now + self.rt);
        if let Some(h) = &self.held {
            // Renew and Rebind give up at T2 and at the end of the lease.
            let cap = if self.st == St::Renewing { h.t2 } else { h.end };
            if matches!(self.st, St::Renewing | St::Rebinding) {
                self.next = self.next.map(|n| n.min(cap));
            }
        }
        let el = ((now - self.started) / 10).min(0xffff) as u16;
        let held: Vec<Ipv6Addr> = self.held.iter().map(|h| h.addr.addr).collect();
        let (typ, server, ia): (u8, Option<&[u8]>, Option<(u32, &[Ipv6Addr])>) = match self.st {
            St::Soliciting => (SOLICIT, None, Some((self.iaid, &[]))),
            St::Requesting => (REQUEST, self.server.as_deref(), Some((self.iaid, &self.offer))),
            St::Renewing => (RENEW, self.server.as_deref(), Some((self.iaid, &held))),
            St::Rebinding => (REBIND, None, Some((self.iaid, &held))),
            St::Informing => (INFO_REQUEST, None, None),
            _ => return Vec::new(),
        };
        vec![Out6::Send(build(typ, self.xid, &self.duid, server, ia, el))]
    }

    /// What the routers' RAs ask for: `managed` (M) wants addresses by
    /// DHCPv6, `other` (O) DNS servers. Called for each RA.
    pub fn ra(&mut self, managed: bool, other: bool, now: u64) -> Vec<Out6> {
        match self.st {
            St::Idle | St::Informing | St::Informed if managed => self.begin(St::Soliciting, now),
            St::Idle if other => self.begin(St::Informing, now),
            _ => Vec::new(),
        }
    }

    /// Off the network: drop the address and DNS servers.
    pub fn stop(&mut self) -> Vec<Out6> {
        self.st = St::Idle;
        self.next = None;
        self.server = None;
        self.dns.clear();
        match self.held.take() {
            Some(_) => vec![Out6::Lost("link")],
            None => Vec::new(),
        }
    }

    pub fn tick(&mut self, now: u64) -> Vec<Out6> {
        let Some(at) = self.next else { return Vec::new() };
        if now < at {
            return Vec::new();
        }
        if let Some(h) = &self.held {
            if now >= h.end {
                self.held = None;
                self.dns.clear();
                let mut o = vec![Out6::Lost("expired")];
                o.extend(self.begin(St::Soliciting, now));
                return o;
            }
            if matches!(self.st, St::Bound | St::Renewing) && now >= h.t2 {
                return self.begin(St::Rebinding, now);
            }
            if self.st == St::Bound && now >= h.t1 {
                return self.begin(St::Renewing, now);
            }
        }
        match self.st {
            St::Requesting if self.tries >= REQUEST_TRIES => self.begin(St::Soliciting, now),
            St::Informed => {
                if now >= self.refresh_at {
                    self.begin(St::Informing, now)
                } else {
                    self.next = Some(self.refresh_at);
                    Vec::new()
                }
            }
            St::Idle | St::Bound => {
                self.next = None;
                Vec::new()
            }
            _ => self.transmit(now),
        }
    }

    /// A message from a server (the UDP payload).
    pub fn rx(&mut self, b: &[u8], now: u64) -> Vec<Out6> {
        let Some(m) = parse(b) else { return Vec::new() };
        if m.xid != self.xid || m.client.as_deref() != Some(&self.duid[..]) || m.server.is_none() {
            return Vec::new();
        }
        let ia = m.ia.iter().find(|i| i.iaid == self.iaid);
        let usable = |ia: Option<&Ia>| ia.filter(|i| i.status == 0).and_then(|i| i.addrs.iter().find(|a| a.valid > 0 && a.pref <= a.valid && good_addr(a.addr)).cloned());
        match (self.st, m.typ) {
            (St::Soliciting, ADVERTISE) => {
                // The first usable offer wins (no waiting to compare
                // preferences: one server per network is the usual case).
                let Some(a) = usable(ia).filter(|_| m.status == 0) else { return Vec::new() };
                self.server = m.server;
                self.offer = vec![a.addr];
                self.begin(St::Requesting, now)
            }
            (St::Requesting | St::Renewing | St::Rebinding, REPLY) => {
                if self.st != St::Rebinding && m.server != self.server {
                    return Vec::new();
                }
                match usable(ia).filter(|_| m.status == 0) {
                    Some(a) => {
                        let mut out = Vec::new();
                        if let Some(h) = &self.held {
                            if h.addr.addr != a.addr {
                                out.push(Out6::Lost("changed"));
                            }
                        }
                        let ia = ia.unwrap();
                        let valid = a.valid.min(MAX_LIFE_S);
                        let pref = a.pref.min(valid);
                        // RFC 8415 §21.4: T1/T2 0 leaves them to the client
                        // (0.5 and 0.8 of the preferred lifetime).
                        let (mut t1, mut t2) = (ia.t1, ia.t2);
                        if t1 == 0 || t2 == 0 || t1 > t2 {
                            (t1, t2) = (pref / 2, pref / 5 * 4);
                        }
                        let t1 = t1.clamp(60, valid.max(60)) as u64 * 1000;
                        let t2 = (t2 as u64 * 1000).clamp(t1, valid as u64 * 1000);
                        let addr = IaAddr { addr: a.addr, pref, valid };
                        self.held = Some(Held { addr: addr.clone(), t1: now + t1, t2: now + t2, end: now + valid as u64 * 1000 });
                        self.server = m.server;
                        self.dns = m.dns;
                        self.st = St::Bound;
                        self.next = Some(now + t1);
                        out.push(Out6::Bound(addr));
                        out
                    }
                    None => {
                        // The server will not give us an address (now).
                        let mut out = Vec::new();
                        if self.held.take().is_some() {
                            out.push(Out6::Lost("refused"));
                        }
                        self.dns.clear();
                        self.st = St::Soliciting;
                        self.rt = 0;
                        self.next = Some(now + REFUSED_BACKOFF_MS);
                        out
                    }
                }
            }
            (St::Informing, REPLY) => {
                let r = m.refresh.unwrap_or(INFO_REFRESH_S).clamp(INFO_REFRESH_MIN_S, INFO_REFRESH_S);
                self.dns = m.dns;
                self.st = St::Informed;
                self.refresh_at = now + r as u64 * 1000;
                self.next = Some(self.refresh_at);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// An address a server may hand us: global or unique-local unicast.
pub fn good_addr(a: Ipv6Addr) -> bool {
    net_proto::ordinary_unicast6(a) && a.segments()[0] & 0xffc0 != 0xfe80
}

// ---- frames ----------------------------------------------------------------

fn csum(b: &[u8], init: u32) -> u16 {
    let mut s = init;
    for c in b.chunks(2) {
        s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
    }
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

/// The DHCPv6 payload of a frame from a server (UDP 547 -> 546, to our
/// link-local address, checksum required as IPv6 asks), or `None`.
pub fn from_frame(f: &[u8]) -> Option<&[u8]> {
    if f.len() < 14 + 40 + 8 || f[12..14] != [0x86, 0xdd] {
        return None;
    }
    let ip = &f[14..];
    if ip[0] >> 4 != 6 || ip[6] != 17 {
        return None;
    }
    let plen = be16(&ip[4..]) as usize;
    if plen < 8 || 40 + plen > ip.len() {
        return None;
    }
    let dst: [u8; 16] = ip[24..40].try_into().ok()?;
    if Ipv6Addr::from(dst).segments()[0] & 0xffc0 != 0xfe80 {
        return None;
    }
    let udp = &ip[40..40 + plen];
    let ulen = be16(&udp[4..]) as usize;
    if be16(udp) != SERVER_PORT || be16(&udp[2..]) != CLIENT_PORT || ulen < 8 || ulen != plen {
        return None;
    }
    let mut pseudo = 0u32;
    for c in ip[8..40].chunks(2) {
        pseudo += be16(c) as u32;
    }
    pseudo += plen as u32 + 17;
    if be16(&udp[6..]) == 0 || csum(udp, pseudo) != 0 {
        return None;
    }
    Some(&udp[8..])
}

/// A frame from a server to `dst` (tests, fuzzing).
pub fn frame_for_test(src: Ipv6Addr, dst: Ipv6Addr, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0xfe, 0x86, 0xdd, 0x60, 0, 0, 0];
    let ulen = 8 + payload.len();
    f.extend_from_slice(&(ulen as u16).to_be_bytes());
    f.extend_from_slice(&[17, 64]);
    f.extend_from_slice(&src.octets());
    f.extend_from_slice(&dst.octets());
    let mut u = Vec::new();
    u.extend_from_slice(&SERVER_PORT.to_be_bytes());
    u.extend_from_slice(&CLIENT_PORT.to_be_bytes());
    u.extend_from_slice(&(ulen as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(payload);
    let mut pseudo = 0u32;
    for c in f[22..54].chunks(2) {
        pseudo += be16(c) as u32;
    }
    pseudo += ulen as u32 + 17;
    let mut c = csum(&u, pseudo);
    if c == 0 {
        c = 0xffff;
    }
    u[6..8].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&u);
    f
}

/// A server message (tests, the fuzzer): Advertise or Reply for `client`.
pub fn server_msg_for_test(typ: u8, xid: [u8; 3], client: &[u8], server: &[u8], ia: Option<(u32, u32, u32, &[(Ipv6Addr, u32, u32)])>, dns: &[Ipv6Addr]) -> Vec<u8> {
    let mut m = vec![typ, xid[0], xid[1], xid[2]];
    opt(&mut m, OPT_CLIENTID, client);
    opt(&mut m, OPT_SERVERID, server);
    if let Some((iaid, t1, t2, addrs)) = ia {
        let mut b = Vec::new();
        for v in [iaid, t1, t2] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        for (a, p, v) in addrs {
            let mut x = a.octets().to_vec();
            x.extend_from_slice(&p.to_be_bytes());
            x.extend_from_slice(&v.to_be_bytes());
            opt(&mut b, OPT_IAADDR, &x);
        }
        opt(&mut m, OPT_IA_NA, &b);
    }
    if !dns.is_empty() {
        let b: Vec<u8> = dns.iter().flat_map(|a| a.octets()).collect();
        opt(&mut m, OPT_DNS, &b);
    }
    m
}
