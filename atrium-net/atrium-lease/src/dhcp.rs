//! DHCPv4 messages (RFC 2131/2132), only what a client needs. Runs in the
//! jailed atrium-lease, so every length is checked and anything odd is
//! dropped (docs/spec/atrium-net.md §5.1).

use std::net::Ipv4Addr;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;
const MAGIC: [u8; 4] = [99, 130, 83, 99];
const FIXED: usize = 236;
/// Largest message we build or accept (RFC 2131 minimum MTU is 576).
pub const MAX_LEN: usize = 1500;

pub mod opt {
    pub const PAD: u8 = 0;
    pub const SUBNET: u8 = 1;
    pub const ROUTER: u8 = 3;
    pub const DNS: u8 = 6;
    pub const MTU: u8 = 26;
    pub const REQ_ADDR: u8 = 50;
    pub const LEASE: u8 = 51;
    pub const OVERLOAD: u8 = 52;
    pub const MSG_TYPE: u8 = 53;
    pub const SERVER_ID: u8 = 54;
    pub const PARAMS: u8 = 55;
    pub const MAX_SIZE: u8 = 57;
    pub const T1: u8 = 58;
    pub const T2: u8 = 59;
    pub const CLIENT_ID: u8 = 61;
    pub const CLASSLESS: u8 = 121;
    pub const END: u8 = 255;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Decline = 4,
    Ack = 5,
    Nak = 6,
    Release = 7,
    Inform = 8,
}

impl Kind {
    fn from(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Discover,
            2 => Kind::Offer,
            3 => Kind::Request,
            4 => Kind::Decline,
            5 => Kind::Ack,
            6 => Kind::Nak,
            7 => Kind::Release,
            8 => Kind::Inform,
            _ => return None,
        })
    }
}

/// A server's reply, as far as the client cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub kind: Kind,
    pub xid: u32,
    pub chaddr: [u8; 6],
    pub yiaddr: Ipv4Addr,
    pub server_id: Option<Ipv4Addr>,
    pub subnet: Option<Ipv4Addr>,
    pub routers: Vec<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub mtu: Option<u16>,
    pub lease: Option<u32>,
    pub t1: Option<u32>,
    pub t2: Option<u32>,
    /// Option 121 raw bytes, decoded by [`classless`].
    pub classless: Option<Vec<u8>>,
}

fn addrs(v: &[u8]) -> Option<Vec<Ipv4Addr>> {
    if v.is_empty() || v.len() % 4 != 0 {
        return None;
    }
    Some(v.chunks_exact(4).map(|c| Ipv4Addr::new(c[0], c[1], c[2], c[3])).collect())
}

fn addr1(v: &[u8]) -> Option<Ipv4Addr> {
    (v.len() == 4).then(|| Ipv4Addr::new(v[0], v[1], v[2], v[3]))
}

fn u32be(v: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(v.try_into().ok()?))
}

/// Parse a BOOTREPLY. `None` for anything malformed, any option repeated,
/// or option overloading (we never read sname/file).
pub fn parse(b: &[u8]) -> Option<Reply> {
    if b.len() < FIXED + 4 || b.len() > MAX_LEN || b[0] != 2 || b[1] != 1 || b[2] != 6 {
        return None;
    }
    if b[FIXED..FIXED + 4] != MAGIC {
        return None;
    }
    let xid = u32::from_be_bytes(b[4..8].try_into().ok()?);
    let yiaddr = Ipv4Addr::new(b[16], b[17], b[18], b[19]);
    let chaddr: [u8; 6] = b[28..34].try_into().ok()?;
    let mut r = Reply {
        kind: Kind::Ack,
        xid,
        chaddr,
        yiaddr,
        server_id: None,
        subnet: None,
        routers: Vec::new(),
        dns: Vec::new(),
        mtu: None,
        lease: None,
        t1: None,
        t2: None,
        classless: None,
    };
    let mut kind = None;
    let mut seen = [false; 256];
    let mut i = FIXED + 4;
    let mut ended = false;
    while i < b.len() {
        let code = b[i];
        if code == opt::PAD {
            i += 1;
            continue;
        }
        if code == opt::END {
            ended = true;
            break;
        }
        let len = *b.get(i + 1)? as usize;
        let v = b.get(i + 2..i + 2 + len)?;
        i += 2 + len;
        // RFC 3396 concatenation of repeated options is not supported:
        // a repeat is a malformed (or hostile) message here.
        if std::mem::replace(&mut seen[code as usize], true) {
            return None;
        }
        match code {
            opt::MSG_TYPE => kind = Some(Kind::from(*v.first().filter(|_| v.len() == 1)?)?),
            opt::SERVER_ID => r.server_id = Some(addr1(v)?),
            opt::SUBNET => r.subnet = Some(addr1(v)?),
            opt::ROUTER => r.routers = addrs(v)?,
            opt::DNS => r.dns = addrs(v)?,
            opt::MTU => r.mtu = Some(u16::from_be_bytes(v.try_into().ok()?)),
            opt::LEASE => r.lease = Some(u32be(v)?),
            opt::T1 => r.t1 = Some(u32be(v)?),
            opt::T2 => r.t2 = Some(u32be(v)?),
            opt::CLASSLESS => r.classless = Some(v.to_vec()),
            opt::OVERLOAD => return None,
            _ => {}
        }
    }
    if !ended {
        return None;
    }
    r.kind = kind?;
    matches!(r.kind, Kind::Offer | Kind::Ack | Kind::Nak).then_some(r)
}

/// Option 121 (RFC 3442): (destination, prefix, gateway) triples.
pub fn classless(v: &[u8]) -> Option<Vec<(Ipv4Addr, u8, Ipv4Addr)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let p = v[i];
        if p > 32 {
            return None;
        }
        let n = (p as usize).div_ceil(8);
        let d = v.get(i + 1..i + 1 + n)?;
        let g = v.get(i + 1 + n..i + 5 + n)?;
        let mut dst = [0u8; 4];
        dst[..n].copy_from_slice(d);
        out.push((Ipv4Addr::from(dst), p, Ipv4Addr::new(g[0], g[1], g[2], g[3])));
        i += 5 + n;
    }
    Some(out)
}

/// What the client sends. RFC 7844 anonymity profile: no hostname, no
/// vendor class, client id = the (per-network) MAC.
#[derive(Clone, Debug)]
pub struct Msg {
    pub kind: Kind,
    pub xid: u32,
    pub secs: u16,
    pub chaddr: [u8; 6],
    pub ciaddr: Ipv4Addr,
    pub requested: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
}

pub const PARAMS: [u8; 7] = [opt::SUBNET, opt::ROUTER, opt::DNS, opt::MTU, opt::LEASE, opt::T1, opt::T2];

pub fn build(m: &Msg) -> Vec<u8> {
    let mut b = vec![0u8; FIXED];
    b[0] = 1; // BOOTREQUEST
    b[1] = 1;
    b[2] = 6;
    b[4..8].copy_from_slice(&m.xid.to_be_bytes());
    b[8..10].copy_from_slice(&m.secs.to_be_bytes());
    // Broadcast flag off: we can receive unicast via bpf (RFC 7844 §3.4 says either).
    b[12..16].copy_from_slice(&m.ciaddr.octets());
    b[28..34].copy_from_slice(&m.chaddr);
    b.extend_from_slice(&MAGIC);
    b.extend_from_slice(&[opt::MSG_TYPE, 1, m.kind as u8]);
    let mut cid = vec![1u8];
    cid.extend_from_slice(&m.chaddr);
    b.extend_from_slice(&[opt::CLIENT_ID, cid.len() as u8]);
    b.extend_from_slice(&cid);
    if let Some(a) = m.requested {
        b.extend_from_slice(&[opt::REQ_ADDR, 4]);
        b.extend_from_slice(&a.octets());
    }
    if let Some(s) = m.server_id {
        b.extend_from_slice(&[opt::SERVER_ID, 4]);
        b.extend_from_slice(&s.octets());
    }
    if matches!(m.kind, Kind::Discover | Kind::Request) {
        let mut p = PARAMS.to_vec();
        p.push(opt::CLASSLESS);
        b.extend_from_slice(&[opt::PARAMS, p.len() as u8]);
        b.extend_from_slice(&p);
        b.extend_from_slice(&[opt::MAX_SIZE, 2]);
        b.extend_from_slice(&(MAX_LEN as u16).to_be_bytes());
    }
    b.push(opt::END);
    // Pad to the BOOTP minimum some servers still expect.
    if b.len() < 300 {
        b.resize(300, 0);
    }
    b
}

/// Test helper and the hostile-server fuzzer's starting point.
pub fn build_reply(r: &Reply) -> Vec<u8> {
    let mut b = vec![0u8; FIXED];
    b[0] = 2;
    b[1] = 1;
    b[2] = 6;
    b[4..8].copy_from_slice(&r.xid.to_be_bytes());
    b[16..20].copy_from_slice(&r.yiaddr.octets());
    b[28..34].copy_from_slice(&r.chaddr);
    b.extend_from_slice(&MAGIC);
    b.extend_from_slice(&[opt::MSG_TYPE, 1, r.kind as u8]);
    let mut put = |c: u8, v: &[u8]| {
        b.push(c);
        b.push(v.len() as u8);
        b.extend_from_slice(v);
    };
    if let Some(s) = r.server_id { put(opt::SERVER_ID, &s.octets()); }
    if let Some(s) = r.subnet { put(opt::SUBNET, &s.octets()); }
    if !r.routers.is_empty() { put(opt::ROUTER, &r.routers.iter().flat_map(|a| a.octets()).collect::<Vec<_>>()); }
    if !r.dns.is_empty() { put(opt::DNS, &r.dns.iter().flat_map(|a| a.octets()).collect::<Vec<_>>()); }
    if let Some(m) = r.mtu { put(opt::MTU, &m.to_be_bytes()); }
    if let Some(v) = r.lease { put(opt::LEASE, &v.to_be_bytes()); }
    if let Some(v) = r.t1 { put(opt::T1, &v.to_be_bytes()); }
    if let Some(v) = r.t2 { put(opt::T2, &v.to_be_bytes()); }
    if let Some(v) = &r.classless { put(opt::CLASSLESS, v); }
    b.push(opt::END);
    b
}
