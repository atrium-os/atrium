//! DNS-SD over mDNS (RFC 6762, RFC 6763), the query side only, for
//! `prelum-browse` (printing.md §7.1).
//!
//! [`query`] builds the questions Prelum asks. [`Packet::parse`] reads one
//! datagram from the LAN, which is hostile: name compression is followed only
//! backwards with a jump limit, names and labels are bounded, counts are
//! bounded, and nothing panics. [`Browser`] keeps what the answers said in a
//! bounded cache that forgets entries when their TTL runs out, and gives
//! complete services (instance, host, port, addresses, TXT keys Prelum reads).
//! It never answers anyone.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const MDNS_PORT: u16 = 5353;
pub const MDNS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);

pub const IPP: &str = "_ipp._tcp.local";
pub const IPPS: &str = "_ipps._tcp.local";

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;

/// Largest datagram looked at (RFC 6762 §17 allows up to 9000 bytes).
pub const MAX_PACKET: usize = 9000;
const MAX_RECORDS: usize = 256;
const MAX_JUMPS: usize = 16;
const MAX_NAME: usize = 255;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    TooLarge,
    BadName,
    NotResponse,
}

/// A domain name as its labels. Labels are bytes; comparisons are ASCII
/// case-insensitive (RFC 6762 §16).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Name(pub Vec<Vec<u8>>);

impl Name {
    pub fn from_dotted(s: &str) -> Name {
        Name(s.trim_end_matches('.').split('.').filter(|l| !l.is_empty()).map(|l| l.as_bytes().to_vec()).collect())
    }
    pub fn key(&self) -> Vec<Vec<u8>> {
        self.0.iter().map(|l| l.to_ascii_lowercase()).collect()
    }
    /// True if `self` is `<one label>.<parent>`.
    pub fn is_child_of(&self, parent: &Name) -> bool {
        self.0.len() == parent.0.len() + 1 && self.key()[1..] == parent.key()[..]
    }
    pub fn dotted(&self) -> String {
        self.0.iter().map(|l| String::from_utf8_lossy(l).into_owned()).collect::<Vec<_>>().join(".")
    }
    fn encode(&self, out: &mut Vec<u8>) {
        for l in &self.0 {
            let l = &l[..l.len().min(63)];
            out.push(l.len() as u8);
            out.extend_from_slice(l);
        }
        out.push(0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rdata {
    Ptr(Name),
    Srv { priority: u16, weight: u16, port: u16, target: Name },
    Txt(Vec<Vec<u8>>),
    Addr(IpAddr),
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: Name,
    pub rtype: u16,
    pub ttl: u32,
    pub data: Rdata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub records: Vec<Record>,
}

/// One query datagram asking for each service type, with the
/// unicast-response bit set so answers come straight back (RFC 6762 §5.4).
pub fn query(service_types: &[&str]) -> Vec<u8> {
    let qs: Vec<(Name, u16)> = service_types.iter().map(|t| (Name::from_dotted(t), TYPE_PTR)).collect();
    query_for(&qs)
}

/// Record types a follow-up question asks for.
pub mod rtype {
    pub const A: u16 = super::TYPE_A;
    pub const PTR: u16 = super::TYPE_PTR;
    pub const TXT: u16 = super::TYPE_TXT;
    pub const SRV: u16 = super::TYPE_SRV;
}

/// One query datagram with these questions (name, type), unicast-response
/// bit set. At most [`MAX_QUESTIONS`] are asked; the rest are left out.
pub fn query_for(qs: &[(Name, u16)]) -> Vec<u8> {
    let qs = &qs[..qs.len().min(MAX_QUESTIONS)];
    let mut out = vec![0, 0, 0, 0];
    out.extend_from_slice(&(qs.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    for (n, t) in qs {
        n.encode(&mut out);
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(&(0x8000 | CLASS_IN).to_be_bytes());
    }
    out
}

/// Questions in one follow-up datagram.
pub const MAX_QUESTIONS: usize = 16;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// Reads a name at the cursor, following compression pointers only to
    /// earlier offsets, at most `MAX_JUMPS` times.
    fn name(&mut self) -> Result<Name, Error> {
        let mut labels = Vec::new();
        let mut total = 0usize;
        let mut pos = self.pos;
        let mut jumped = false;
        let mut jumps = 0;
        let mut floor = self.pos; // a pointer must go below where it was read
        loop {
            let len = *self.buf.get(pos).ok_or(Error::Truncated)? as usize;
            match len {
                0 => {
                    if !jumped {
                        self.pos = pos + 1;
                    }
                    return Ok(Name(labels));
                }
                1..=63 => {
                    let l = self.buf.get(pos + 1..pos + 1 + len).ok_or(Error::Truncated)?;
                    total += len + 1;
                    if total > MAX_NAME {
                        return Err(Error::BadName);
                    }
                    labels.push(l.to_vec());
                    pos += 1 + len;
                }
                0xC0..=0xFF => {
                    let lo = *self.buf.get(pos + 1).ok_or(Error::Truncated)? as usize;
                    let target = ((len & 0x3F) << 8) | lo;
                    if target >= floor.min(pos) {
                        return Err(Error::BadName);
                    }
                    jumps += 1;
                    if jumps > MAX_JUMPS {
                        return Err(Error::BadName);
                    }
                    if !jumped {
                        self.pos = pos + 2;
                        jumped = true;
                    }
                    floor = target;
                    pos = target;
                }
                _ => return Err(Error::BadName),
            }
        }
    }
}

impl Packet {
    /// Parses a response datagram. Queries (QR bit clear) are refused: the
    /// browser does not answer, so it has no use for them.
    pub fn parse(buf: &[u8]) -> Result<Packet, Error> {
        if buf.len() > MAX_PACKET {
            return Err(Error::TooLarge);
        }
        let mut r = Reader { buf, pos: 0 };
        let _id = r.u16()?;
        let flags = r.u16()?;
        if flags & 0x8000 == 0 {
            return Err(Error::NotResponse);
        }
        let qd = r.u16()? as usize;
        let rr = r.u16()? as usize + r.u16()? as usize + r.u16()? as usize;
        if qd > MAX_RECORDS || rr > MAX_RECORDS {
            return Err(Error::TooLarge);
        }
        for _ in 0..qd {
            r.name()?;
            r.take(4)?;
        }
        let mut records = Vec::with_capacity(rr);
        for _ in 0..rr {
            let name = r.name()?;
            let rtype = r.u16()?;
            let class = r.u16()? & 0x7FFF;
            let ttl = r.u32()?;
            let rdlen = r.u16()? as usize;
            let start = r.pos;
            let end = start.checked_add(rdlen).filter(|e| *e <= buf.len()).ok_or(Error::Truncated)?;
            let data = if class != CLASS_IN {
                Rdata::Other
            } else {
                match rtype {
                    TYPE_PTR => Rdata::Ptr(r.name()?),
                    TYPE_SRV if rdlen >= 7 => {
                        let priority = r.u16()?;
                        let weight = r.u16()?;
                        let port = r.u16()?;
                        Rdata::Srv { priority, weight, port, target: r.name()? }
                    }
                    TYPE_TXT => {
                        let mut strings = Vec::new();
                        let mut p = start;
                        while p < end {
                            let n = buf[p] as usize;
                            let s = buf.get(p + 1..p + 1 + n).filter(|_| p + 1 + n <= end).ok_or(Error::Truncated)?;
                            if !s.is_empty() && strings.len() < 64 {
                                strings.push(s.to_vec());
                            }
                            p += 1 + n;
                        }
                        Rdata::Txt(strings)
                    }
                    TYPE_A if rdlen == 4 => {
                        let b = &buf[start..end];
                        Rdata::Addr(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])))
                    }
                    TYPE_AAAA if rdlen == 16 => {
                        let mut a = [0u8; 16];
                        a.copy_from_slice(&buf[start..end]);
                        Rdata::Addr(IpAddr::V6(Ipv6Addr::from(a)))
                    }
                    _ => Rdata::Other,
                }
            };
            // A record's data never extends past its declared length, and the
            // next record starts right after it, whatever the name reader did.
            if r.pos > end {
                return Err(Error::Truncated);
            }
            r.pos = end;
            records.push(Record { name, rtype, ttl, data });
        }
        Ok(Packet { records })
    }
}

/// The TXT keys Prelum reads (spec §7.1), values made safe to show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Txt {
    pub ty: Option<String>,
    pub note: Option<String>,
    pub pdl: Vec<String>,
    pub urf: Vec<String>,
    pub uuid: Option<String>,
    pub tls: Option<String>,
    pub color: Option<bool>,
    pub duplex: Option<bool>,
    pub rp: Option<String>,
    pub kind: Vec<String>,
    pub adminurl: Option<String>,
}

fn clean(v: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(v);
    let mut out = String::new();
    for c in s.chars() {
        if c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}') {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn list(v: &[u8]) -> Vec<String> {
    clean(v, 255).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).take(32).collect()
}

impl Txt {
    pub fn parse(strings: &[Vec<u8>]) -> Txt {
        let mut t = Txt::default();
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for s in strings {
            let (k, v) = match s.iter().position(|b| *b == b'=') {
                Some(i) => (&s[..i], &s[i + 1..]),
                None => (&s[..], &[][..]),
            };
            let key = k.to_ascii_lowercase();
            // RFC 6763 §6.4: only the first occurrence of a key counts.
            if key.is_empty() || seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            let yes = || matches!(v, b"T" | b"t");
            match &key[..] {
                b"ty" => t.ty = Some(clean(v, 128)),
                b"note" => t.note = Some(clean(v, 128)),
                b"pdl" => t.pdl = list(v),
                b"urf" => t.urf = list(v),
                b"uuid" => {
                    let u = clean(v, 64).to_ascii_lowercase();
                    if u.len() == 36 && u.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                        t.uuid = Some(u);
                    }
                }
                b"tls" => t.tls = Some(clean(v, 8)),
                b"color" => t.color = Some(yes()),
                b"duplex" => t.duplex = Some(yes()),
                b"rp" => t.rp = Some(clean(v, 128)),
                b"kind" => t.kind = list(v),
                b"adminurl" => t.adminurl = Some(clean(v, 256)),
                _ => {}
            }
        }
        t
    }
}

/// A complete service: it has an SRV record and the instance is under a
/// type that was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Service {
    /// The instance label as the printer wrote it, made safe to show.
    pub instance: String,
    pub service_type: String,
    pub host: String,
    pub port: u16,
    pub addrs: Vec<IpAddr>,
    pub txt: Txt,
}

pub const MAX_SERVICES: usize = 256;
const MAX_HOSTS: usize = 256;
const MAX_ADDRS: usize = 8;
const MAX_TTL: u64 = 75 * 60;

struct Expiring<T> {
    value: T,
    until: u64,
}

#[derive(Default)]
struct Instance {
    ptr_until: u64,
    service_type: usize,
    srv: Option<Expiring<(u16, Name)>>,
    txt: Option<Expiring<Txt>>,
}

/// The cache of what has been heard. Times are seconds from any monotonic
/// origin the caller chooses.
pub struct Browser {
    types: Vec<Name>,
    instances: HashMap<Vec<Vec<u8>>, (Name, Instance)>,
    hosts: HashMap<Vec<Vec<u8>>, Vec<Expiring<IpAddr>>>,
}

impl Browser {
    pub fn new(service_types: &[&str]) -> Browser {
        Browser { types: service_types.iter().map(|t| Name::from_dotted(t)).collect(), instances: HashMap::new(), hosts: HashMap::new() }
    }

    pub fn query(&self) -> Vec<u8> {
        let t: Vec<String> = self.types.iter().map(Name::dotted).collect();
        let r: Vec<&str> = t.iter().map(String::as_str).collect();
        query(&r)
    }

    fn type_of(&self, instance: &Name) -> Option<usize> {
        self.types.iter().position(|t| instance.is_child_of(t))
    }

    fn expire(&mut self, now: u64) {
        self.instances.retain(|_, (_, i)| i.ptr_until > now);
        for (_, i) in self.instances.values_mut() {
            if i.srv.as_ref().is_some_and(|s| s.until <= now) {
                i.srv = None;
            }
            if i.txt.as_ref().is_some_and(|s| s.until <= now) {
                i.txt = None;
            }
        }
        for a in self.hosts.values_mut() {
            a.retain(|e| e.until > now);
        }
        self.hosts.retain(|_, a| !a.is_empty());
    }

    /// Takes one parsed datagram. Records for types nobody asked about, and
    /// SRV/TXT for instances no PTR announced, are ignored.
    pub fn feed(&mut self, p: &Packet, now: u64) {
        self.expire(now);
        let until = |ttl: u32| now + (ttl as u64).min(MAX_TTL);
        // PTRs first, so SRV and TXT in the same packet find their instance.
        for r in &p.records {
            if let Rdata::Ptr(inst) = &r.data {
                let Some(t) = self.types.iter().position(|t| t.key() == r.name.key()) else { continue };
                if self.type_of(inst) != Some(t) {
                    continue;
                }
                let key = inst.key();
                if r.ttl == 0 {
                    self.instances.remove(&key);
                    continue;
                }
                if !self.instances.contains_key(&key) && self.instances.len() >= MAX_SERVICES {
                    continue;
                }
                let e = self.instances.entry(key).or_insert_with(|| (inst.clone(), Instance::default()));
                e.1.ptr_until = until(r.ttl);
                e.1.service_type = t;
            }
        }
        for r in &p.records {
            match &r.data {
                Rdata::Srv { port, target, .. } => {
                    if let Some((_, i)) = self.instances.get_mut(&r.name.key()) {
                        i.srv = (r.ttl > 0).then(|| Expiring { value: (*port, target.clone()), until: until(r.ttl) });
                    }
                }
                Rdata::Txt(s) => {
                    if let Some((_, i)) = self.instances.get_mut(&r.name.key()) {
                        i.txt = (r.ttl > 0).then(|| Expiring { value: Txt::parse(s), until: until(r.ttl) });
                    }
                }
                Rdata::Addr(a) => {
                    let key = r.name.key();
                    if !self.hosts.contains_key(&key) && self.hosts.len() >= MAX_HOSTS {
                        continue;
                    }
                    let list = self.hosts.entry(key).or_default();
                    list.retain(|e| e.value != *a);
                    if r.ttl > 0 && list.len() < MAX_ADDRS {
                        list.push(Expiring { value: *a, until: until(r.ttl) });
                    }
                }
                _ => {}
            }
        }
        self.expire(now);
    }

    /// Instances heard of, complete or not (at most [`MAX_SERVICES`]).
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// What is missing for instances heard of: their SRV and TXT when only
    /// a PTR came, the address of an SRV's host when none came. Responders
    /// usually send all of it at once; these are the questions to ask when
    /// one didn't. At most [`MAX_QUESTIONS`].
    pub fn wanting(&mut self, now: u64) -> Vec<(Name, u16)> {
        self.expire(now);
        let mut out = Vec::new();
        let mut names: Vec<&(Name, Instance)> = self.instances.values().collect();
        names.sort_by(|a, b| a.0.key().cmp(&b.0.key()));
        for (name, i) in names {
            if out.len() >= MAX_QUESTIONS {
                break;
            }
            match &i.srv {
                None => out.push((name.clone(), TYPE_SRV)),
                Some(s) if !self.hosts.contains_key(&s.value.1.key()) => {
                    let q = (s.value.1.clone(), TYPE_A);
                    if !out.iter().any(|(n, t)| *t == TYPE_A && n.key() == q.0.key()) {
                        out.push(q);
                    }
                }
                Some(_) => {}
            }
            if i.txt.is_none() && out.len() < MAX_QUESTIONS {
                out.push((name.clone(), TYPE_TXT));
            }
        }
        out.truncate(MAX_QUESTIONS);
        out
    }

    /// Services known now, sorted by instance name.
    pub fn services(&mut self, now: u64) -> Vec<Service> {
        self.expire(now);
        let mut out: Vec<Service> = self
            .instances
            .values()
            .filter_map(|(name, i)| {
                let srv = i.srv.as_ref()?;
                let (port, target) = &srv.value;
                let addrs = self.hosts.get(&target.key()).map(|a| a.iter().map(|e| e.value).collect()).unwrap_or_default();
                Some(Service {
                    instance: clean(name.0.first()?, 128),
                    service_type: self.types[i.service_type].dotted(),
                    host: clean(target.dotted().as_bytes(), 255),
                    port: *port,
                    addrs,
                    txt: i.txt.as_ref().map(|t| t.value.clone()).unwrap_or_default(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.instance.cmp(&b.instance).then(a.service_type.cmp(&b.service_type)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a response the way a printer would, with compression.
    struct Resp {
        out: Vec<u8>,
        n: u16,
    }
    impl Resp {
        fn new() -> Resp {
            Resp { out: vec![0, 0, 0x84, 0, 0, 0, 0, 0, 0, 0, 0, 0], n: 0 }
        }
        fn rr(&mut self, name: &[u8], rtype: u16, ttl: u32, rdata: &[u8]) {
            self.out.extend_from_slice(name);
            self.out.extend_from_slice(&rtype.to_be_bytes());
            self.out.extend_from_slice(&0x8001u16.to_be_bytes());
            self.out.extend_from_slice(&ttl.to_be_bytes());
            self.out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            self.out.extend_from_slice(rdata);
            self.n += 1;
        }
        fn done(mut self) -> Vec<u8> {
            self.out[6..8].copy_from_slice(&self.n.to_be_bytes());
            self.out
        }
    }
    fn enc(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        Name::from_dotted(s).encode(&mut v);
        v
    }

    fn printer_packet(ttl: u32) -> Vec<u8> {
        let mut r = Resp::new();
        // PTR _ipp._tcp.local -> "Office Printer._ipp._tcp.local", the
        // instance name compressed against the owner name at offset 12.
        let owner = enc(IPP);
        let mut ptr = vec![14];
        ptr.extend_from_slice(b"Office Printer");
        ptr.extend_from_slice(&[0xC0, 12]);
        r.rr(&owner, TYPE_PTR, ttl, &ptr);
        let inst_off = 12 + owner.len() + 10;
        let inst = [0xC0, inst_off as u8];
        let mut srv = vec![0, 0, 0, 0, 0x02, 0x77];
        srv.extend_from_slice(&enc("office.local"));
        r.rr(&inst, TYPE_SRV, ttl, &srv);
        let mut txt = Vec::new();
        for s in ["txtvers=1", "ty=Acme Laser 9000", "pdl=application/pdf,image/pwg-raster,image/urf", "UUID=4509a320-00a0-008f-00b6-00a0d1d2e3f4", "Color=T", "Duplex=F", "TLS=1.3", "ty=second ignored", "note=\u{202E}3rd floor"] {
            txt.push(s.len() as u8);
            txt.extend_from_slice(s.as_bytes());
        }
        r.rr(&inst, TYPE_TXT, ttl, &txt);
        r.rr(&enc("office.local"), TYPE_A, ttl, &[192, 168, 1, 20]);
        r.done()
    }

    #[test]
    fn asks_for_what_a_bare_ptr_left_out() {
        let mut b = Browser::new(&[IPP, IPPS]);
        let mut r = Resp::new();
        let owner = enc(IPP);
        let mut ptr = vec![14];
        ptr.extend_from_slice(b"Office Printer");
        ptr.extend_from_slice(&[0xC0, 12]);
        r.rr(&owner, TYPE_PTR, 120, &ptr);
        b.feed(&Packet::parse(&r.done()).unwrap(), 0);
        assert!(b.services(0).is_empty(), "no SRV yet");
        let w = b.wanting(0);
        let inst = Name::from_dotted("Office Printer._ipp._tcp.local");
        assert_eq!(w, [(inst.clone(), TYPE_SRV), (inst.clone(), TYPE_TXT)]);
        // The questions parse back as a query for exactly those.
        let q = query_for(&w);
        assert_eq!(q[4..6], [0, 2]);
        assert!(Packet::parse(&q).is_err(), "a query is not an answer");
        // SRV and TXT come, the host's address doesn't: ask for it.
        let mut r = Resp::new();
        let mut srv = vec![0, 0, 0, 0, 0x02, 0x77];
        srv.extend_from_slice(&enc("office.local"));
        r.rr(&enc("Office Printer._ipp._tcp.local"), TYPE_SRV, 120, &srv);
        r.rr(&enc("Office Printer._ipp._tcp.local"), TYPE_TXT, 120, &[6, b'r', b'p', b'=', b'i', b'p', b'p']);
        b.feed(&Packet::parse(&r.done()).unwrap(), 1);
        assert_eq!(b.wanting(1), [(Name::from_dotted("office.local"), TYPE_A)]);
        b.feed(&Packet::parse(&printer_packet(120)).unwrap(), 2);
        assert!(b.wanting(2).is_empty());
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn follow_ups_are_bounded() {
        let mut b = Browser::new(&[IPP]);
        let owner = enc(IPP);
        for i in 0..40 {
            let mut r = Resp::new();
            let label = format!("p{i}");
            let mut ptr = vec![label.len() as u8];
            ptr.extend_from_slice(label.as_bytes());
            ptr.extend_from_slice(&[0xC0, 12]);
            r.rr(&owner, TYPE_PTR, 120, &ptr);
            b.feed(&Packet::parse(&r.done()).unwrap(), 0);
        }
        assert_eq!(b.wanting(0).len(), MAX_QUESTIONS);
        let many: Vec<(Name, u16)> = (0..100).map(|i| (Name::from_dotted(&format!("x{i}.local")), TYPE_A)).collect();
        assert_eq!(query_for(&many)[4..6], [0, MAX_QUESTIONS as u8]);
    }

    #[test]
    fn browse_a_printer() {
        let mut b = Browser::new(&[IPP, IPPS]);
        let p = Packet::parse(&printer_packet(120)).unwrap();
        b.feed(&p, 0);
        let s = b.services(10);
        assert_eq!(s.len(), 1);
        let s = &s[0];
        assert_eq!(s.instance, "Office Printer");
        assert_eq!(s.host, "office.local");
        assert_eq!(s.port, 631);
        assert_eq!(s.addrs, [IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))]);
        assert_eq!(s.txt.ty.as_deref(), Some("Acme Laser 9000"));
        assert_eq!(s.txt.pdl, ["application/pdf", "image/pwg-raster", "image/urf"]);
        assert_eq!(s.txt.color, Some(true));
        assert_eq!(s.txt.duplex, Some(false));
        assert_eq!(s.txt.note.as_deref(), Some("3rd floor"));
        assert!(s.txt.uuid.is_some());
        // expires with its TTL
        assert!(b.services(121).is_empty());
    }

    #[test]
    fn goodbye_removes() {
        let mut b = Browser::new(&[IPP]);
        b.feed(&Packet::parse(&printer_packet(120)).unwrap(), 0);
        b.feed(&Packet::parse(&printer_packet(0)).unwrap(), 1);
        assert!(b.services(2).is_empty());
    }

    #[test]
    fn unasked_types_ignored() {
        let mut b = Browser::new(&[IPPS]);
        b.feed(&Packet::parse(&printer_packet(120)).unwrap(), 0);
        assert!(b.services(1).is_empty());
    }

    #[test]
    fn pointer_loops_and_forward_pointers_refused() {
        let mut r = Resp::new();
        r.rr(&[0xC0, 12], TYPE_A, 10, &[1, 2, 3, 4]); // points at itself
        assert_eq!(Packet::parse(&r.done()), Err(Error::BadName));
        let mut r = Resp::new();
        r.rr(&[0xC0, 40], TYPE_A, 10, &[1, 2, 3, 4]); // forward
        assert_eq!(Packet::parse(&r.done()), Err(Error::BadName));
    }

    #[test]
    fn queries_refused_and_prefixes_safe() {
        let q = query(&[IPP, IPPS]);
        assert_eq!(Packet::parse(&q), Err(Error::NotResponse));
        let p = printer_packet(120);
        for n in 0..p.len() {
            let _ = Packet::parse(&p[..n]);
        }
    }

    #[test]
    fn query_shape() {
        let q = query(&[IPP]);
        assert_eq!(&q[4..6], &[0, 1]);
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0x80, 1]);
    }

    #[test]
    fn cache_is_bounded() {
        let mut b = Browser::new(&[IPP]);
        for i in 0..400u32 {
            let mut r = Resp::new();
            let label = format!("p{i}");
            let mut ptr = vec![label.len() as u8];
            ptr.extend_from_slice(label.as_bytes());
            ptr.extend_from_slice(&[0xC0, 12]);
            r.rr(&enc(IPP), TYPE_PTR, 120, &ptr);
            let inst = enc(&format!("{label}.{IPP}"));
            let mut srv = vec![0, 0, 0, 0, 0x02, 0x77];
            srv.extend_from_slice(&enc("h.local"));
            r.rr(&inst, TYPE_SRV, 120, &srv);
            b.feed(&Packet::parse(&r.done()).unwrap(), 0);
        }
        assert_eq!(b.services(1).len(), MAX_SERVICES);
    }
}
