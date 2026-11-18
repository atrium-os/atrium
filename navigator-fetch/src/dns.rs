//! ★ DNS LOOKUPS for Nuntius's `Lookup` (nuntius.md §3.6): TXT, SRV and MX
//! records, asked of the system's resolver (local-unbound, which validates
//! DNSSEC and says so with the AD bit) over UDP.
//!
//! The sockets are opened and connected to the resolver BEFORE the sandbox
//! closes (`Udp::system` in `nuntiusd`'s setup): in capability mode a
//! connected socket can still send and receive, and nothing else can be
//! reached. EDNS0 asks for answers up to 4 KiB; a truncated answer is an
//! error (TCP would need a new connection), which a DKIM key never needs.
//!
//! The answer is read here, bounded: names decompressed with a jump limit,
//! only records of the kind asked for, at most [`RECORDS_MAX`].

use nuntius::{ErrorCode, Record, RecordKind, RECORDS_MAX, TXT_MAX};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Sends one query and returns the answer that matches its id.
pub trait Exchange: Send + Sync {
    fn exchange(&self, query: &[u8]) -> io::Result<Vec<u8>>;
}

/// Time to wait for each resolver.
pub const WAIT: Duration = Duration::from_secs(3);

/// Connected UDP sockets, one per resolver, tried in order.
pub struct Udp {
    socks: Mutex<Vec<UdpSocket>>,
}

/// The resolvers in a `resolv.conf`: `nameserver` lines, at most 3.
pub fn nameservers(conf: &str) -> Vec<IpAddr> {
    conf.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("nameserver")).then(|| w.next()).flatten()
        })
        .filter_map(|a| a.split('%').next()?.parse().ok())
        .take(3)
        .collect()
}

impl Udp {
    /// The system's resolvers (`/etc/resolv.conf`, else 127.0.0.1).
    pub fn system() -> io::Result<Udp> {
        let mut ns = std::fs::read_to_string("/etc/resolv.conf").map(|c| nameservers(&c)).unwrap_or_default();
        if ns.is_empty() {
            ns.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        Udp::to(&ns.into_iter().map(|a| SocketAddr::new(a, 53)).collect::<Vec<_>>())
    }

    /// Resolvers at `addrs` (tests use one of their own).
    pub fn to(addrs: &[SocketAddr]) -> io::Result<Udp> {
        let mut socks = Vec::new();
        for a in addrs {
            let any: SocketAddr = if a.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
            let s = UdpSocket::bind(any)?;
            s.connect(a)?;
            s.set_read_timeout(Some(Duration::from_millis(250)))?;
            socks.push(s);
        }
        if socks.is_empty() {
            return Err(io::Error::other("no resolver"));
        }
        Ok(Udp { socks: Mutex::new(socks) })
    }
}

impl Exchange for Udp {
    fn exchange(&self, query: &[u8]) -> io::Result<Vec<u8>> {
        // One query at a time: answers are matched by id on a shared socket.
        let socks = self.socks.lock().unwrap();
        let mut last = io::Error::new(io::ErrorKind::TimedOut, "no answer");
        for s in socks.iter() {
            // Whatever an earlier, abandoned query left behind.
            let mut junk = [0u8; 1];
            s.set_nonblocking(true)?;
            while s.recv(&mut junk).is_ok() {}
            s.set_nonblocking(false)?;
            if let Err(e) = s.send(query) {
                last = e;
                continue;
            }
            let until = Instant::now() + WAIT;
            let mut buf = vec![0u8; 4096];
            while Instant::now() < until {
                match s.recv(&mut buf) {
                    Ok(n) if n >= 2 && buf[..2] == query[..2] => return Ok(buf[..n].to_vec()),
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(e) => {
                        last = e;
                        break;
                    }
                }
            }
        }
        Err(last)
    }
}

fn qtype(k: RecordKind) -> u16 {
    match k {
        RecordKind::Txt => 16,
        RecordKind::Srv => 33,
        RecordKind::Mx => 15,
    }
}

/// A query for `name` (checked with `nuntius::name_ok`), recursion and
/// the AD bit asked for, EDNS0 with a 4 KiB answer size.
pub fn query(id: u16, name: &str, kind: RecordKind) -> Vec<u8> {
    query_type(id, name, qtype(kind))
}

fn query_type(id: u16, name: &str, qt: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(name.len() + 30);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0120u16.to_be_bytes()); // RD, AD
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 1]);
    for l in name.split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qt.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    // OPT: root name, type 41, class = UDP size, no flags, no data.
    q.extend_from_slice(&[0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]);
    q
}

fn u16_at(m: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?]))
}

/// The name at `i` (lower case, no trailing dot; the root is "."), and
/// where what follows it starts.
fn name_at(m: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *m.get(i)? as usize;
        match len {
            0 => {
                return Some((if out.is_empty() { ".".into() } else { out }, end.unwrap_or(i + 1)));
            }
            l if l & 0xC0 == 0xC0 => {
                jumps += 1;
                if jumps > 32 {
                    return None;
                }
                end.get_or_insert(i + 2);
                i = (u16_at(m, i)? & 0x3FFF) as usize;
            }
            l if l <= 63 => {
                let label = m.get(i + 1..i + 1 + l)?;
                if !label.iter().all(|b| b.is_ascii_graphic() && *b != b'.') {
                    return None;
                }
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&String::from_utf8_lossy(label).to_ascii_lowercase());
                if out.len() > 253 {
                    return None;
                }
                i += 1 + l;
            }
            _ => return None,
        }
    }
}

/// The records of `kind` in answer `m` to query `id` for `name`, and
/// whether the resolver validated them. No such name is no records.
pub fn answer(m: &[u8], id: u16, name: &str, kind: RecordKind) -> Result<(Vec<Record>, bool), ErrorCode> {
    let bad = ErrorCode::Malformed;
    let flags = u16_at(m, 2).ok_or(bad)?;
    if u16_at(m, 0) != Some(id) || flags & 0x8000 == 0 || flags & 0x7800 != 0 {
        return Err(bad);
    }
    if flags & 0x0200 != 0 {
        return Err(ErrorCode::Dns); // truncated
    }
    match flags & 0x000F {
        0 => {}
        3 => return Ok((Vec::new(), flags & 0x0020 != 0)),
        _ => return Err(ErrorCode::Dns),
    }
    let (qd, an) = (u16_at(m, 4).ok_or(bad)?, u16_at(m, 6).ok_or(bad)?);
    if qd != 1 {
        return Err(bad);
    }
    let (qname, mut i) = name_at(m, 12).ok_or(bad)?;
    if !qname.eq_ignore_ascii_case(name) || u16_at(m, i) != Some(qtype(kind)) || u16_at(m, i + 2) != Some(1) {
        return Err(bad);
    }
    i += 4;
    let mut out = Vec::new();
    for _ in 0..an {
        let (_, j) = name_at(m, i).ok_or(bad)?;
        let (t, class, len) = (u16_at(m, j).ok_or(bad)?, u16_at(m, j + 2).ok_or(bad)?, u16_at(m, j + 8).ok_or(bad)? as usize);
        let at = j + 10;
        let data = m.get(at..at + len).ok_or(bad)?;
        i = at + len;
        // Others (a CNAME the resolver followed, say) are passed over.
        if t != qtype(kind) || class != 1 || out.len() >= RECORDS_MAX {
            continue;
        }
        let r = match kind {
            RecordKind::Txt => {
                let mut s = Vec::new();
                let mut k = 0;
                while k < data.len() {
                    let l = data[k] as usize;
                    s.extend_from_slice(data.get(k + 1..k + 1 + l).ok_or(bad)?);
                    k += 1 + l;
                }
                if s.len() > TXT_MAX {
                    continue;
                }
                Record::Txt(String::from_utf8_lossy(&s).chars().filter(|c| !c.is_control()).collect())
            }
            RecordKind::Srv => {
                let (target, _) = name_at(m, at + 6).ok_or(bad)?;
                Record::Srv { priority: u16_at(data, 0).ok_or(bad)?, weight: u16_at(data, 2).ok_or(bad)?, port: u16_at(data, 4).ok_or(bad)?, target }
            }
            RecordKind::Mx => {
                let (host, _) = name_at(m, at + 2).ok_or(bad)?;
                Record::Mx { preference: u16_at(data, 0).ok_or(bad)?, host }
            }
        };
        out.push(r);
    }
    Ok((out, flags & 0x0020 != 0))
}

/// At most this many addresses from one answer.
pub const ADDRS_MAX: usize = 8;

/// The addresses (A: `qt` 1, AAAA: 28) in answer `m` to query `id` for
/// `name`: records owned by the name, or by a name it is a CNAME for
/// (followed in answer order, at most 8 steps). No such name: none.
pub fn addr_answer(m: &[u8], id: u16, name: &str, qt: u16) -> io::Result<Vec<IpAddr>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed DNS answer");
    let flags = u16_at(m, 2).ok_or_else(bad)?;
    if u16_at(m, 0) != Some(id) || flags & 0x8000 == 0 || flags & 0x7800 != 0 || flags & 0x0200 != 0 {
        return Err(bad());
    }
    match flags & 0x000F {
        0 => {}
        3 => return Ok(Vec::new()),
        _ => return Err(io::Error::other("the DNS server failed")),
    }
    let (qd, an) = (u16_at(m, 4).ok_or_else(bad)?, u16_at(m, 6).ok_or_else(bad)?);
    let (qname, mut i) = name_at(m, 12).ok_or_else(bad)?;
    if qd != 1 || !qname.eq_ignore_ascii_case(name) || u16_at(m, i) != Some(qt) || u16_at(m, i + 2) != Some(1) {
        return Err(bad());
    }
    i += 4;
    let mut names = vec![qname];
    let mut out = Vec::new();
    for _ in 0..an.min(64) {
        let (owner, j) = name_at(m, i).ok_or_else(bad)?;
        let (t, class, len) = (u16_at(m, j).ok_or_else(bad)?, u16_at(m, j + 2).ok_or_else(bad)?, u16_at(m, j + 8).ok_or_else(bad)? as usize);
        let at = j + 10;
        let data = m.get(at..at + len).ok_or_else(bad)?;
        i = at + len;
        if class != 1 || !names.contains(&owner) {
            continue;
        }
        match (t, len) {
            (5, _) if names.len() < 9 => names.push(name_at(m, at).ok_or_else(bad)?.0),
            (1, 4) if qt == 1 => out.push(IpAddr::from(<[u8; 4]>::try_from(data).unwrap())),
            (28, 16) if qt == 28 => out.push(IpAddr::from(<[u8; 16]>::try_from(data).unwrap())),
            _ => {}
        }
        if out.len() >= ADDRS_MAX {
            break;
        }
    }
    Ok(out)
}

/// `name`'s addresses through `ex`: A, then AAAA. Used where unbound must
/// be passed by (a captive portal's sign-in window, atrium-net.md §8.1).
pub fn addresses(ex: &dyn Exchange, name: &str) -> io::Result<Vec<IpAddr>> {
    if !nuntius::name_ok(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a host name"));
    }
    let mut out = Vec::new();
    let mut last = None;
    for qt in [1u16, 28] {
        let id = fresh_id();
        match ex.exchange(&query_type(id, name, qt)).and_then(|m| addr_answer(&m, id, name, qt)) {
            Ok(v) => out.extend(v),
            Err(e) => last = Some(e),
        }
    }
    match (out.is_empty(), last) {
        (true, Some(e)) => Err(e),
        (true, None) => Err(io::Error::new(io::ErrorKind::NotFound, "no such name")),
        _ => Ok(out),
    }
}

/// Resolve with `dns` (the network's own servers), connect with `net`:
/// the fetcher of a portal sign-in window (atrium-net.md §8.1). The
/// address check and pins in `Conversation` apply as ever, and on top of
/// the private-network grant this window needs (portals sit on the LAN), it
/// never reaches the machine itself: loopback, the unspecified address and
/// the machine's own addresses (`own`, read before the sandbox) are refused.
pub struct ViaDns {
    pub net: &'static (dyn crate::Connect + Sync),
    pub dns: std::sync::Arc<dyn Exchange>,
    pub own: Vec<IpAddr>,
}

impl ViaDns {
    /// Whether the window may reach `a`: never the machine itself.
    pub fn off_machine(&self, a: IpAddr) -> bool {
        let a = match a {
            IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(a),
            v4 => v4,
        };
        !(a.is_loopback() || a.is_unspecified() || self.own.contains(&a))
    }
}

/// `ATRIUM_NUNTIUS_OWN=A,B,..`: the host's addresses, from portcullisd (a
/// jail sees only its own interfaces).
pub const OWN_ENV: &str = "ATRIUM_NUNTIUS_OWN";

/// The machine's own addresses, for [`ViaDns::own`]: every interface this
/// process sees, and those portcullisd names in [`OWN_ENV`].
pub fn own_addresses() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = std::env::var(OWN_ENV).unwrap_or_default().split(',').filter_map(|a| a.trim().parse().ok()).collect();
    let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `ifa` with a list freed below; each entry's
    // address is read according to its family.
    unsafe {
        if libc::getifaddrs(&mut ifa) != 0 {
            return out;
        }
        let mut p = ifa;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() {
                match (*a).sa_family as i32 {
                    libc::AF_INET => {
                        let s = &*(a as *const libc::sockaddr_in);
                        out.push(IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(s.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let s = &*(a as *const libc::sockaddr_in6);
                        out.push(IpAddr::V6(std::net::Ipv6Addr::from(s.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}

impl crate::Connect for ViaDns {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let v: Vec<IpAddr> = match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
            Ok(a) => vec![a],
            Err(_) => addresses(&*self.dns, host)?,
        };
        let v: Vec<SocketAddr> = v.into_iter().filter(|a| self.off_machine(*a)).map(|a| SocketAddr::new(a, port)).collect();
        if v.is_empty() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{host}: only this machine's own addresses (a sign-in window never reaches the machine)")));
        }
        Ok(v)
    }
    fn connect_addr(&self, addr: SocketAddr) -> io::Result<std::net::TcpStream> {
        if !self.off_machine(addr.ip()) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{addr}: this machine (a sign-in window never reaches it)")));
        }
        // In the sandbox casper connects only where it resolved
        // (CAPNET_CONNECTDNS), and these addresses came from the network's
        // servers or the URL. Its resolving the literal (numeric: nothing
        // is asked of anyone) is what lets the connect through.
        self.net.resolve(&addr.ip().to_string(), addr.port())?;
        self.net.connect_addr(addr)
    }
}

/// `ATRIUM_NUNTIUS_RESOLVERS=A,B`: the DNS servers a portal window's
/// fetcher asks instead of the system's (set only by portcullisd).
pub const RESOLVERS_ENV: &str = "ATRIUM_NUNTIUS_RESOLVERS";

/// The servers in `RESOLVERS_ENV`'s value: at most 3 addresses, each a
/// usable unicast one; anything else is refused whole.
pub fn resolvers(v: &str) -> io::Result<Vec<SocketAddr>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("{RESOLVERS_ENV}: at most 3 addresses"));
    let v: Vec<IpAddr> = v.split(',').map(|a| a.trim().parse().map_err(|_| bad())).collect::<io::Result<_>>()?;
    if v.is_empty() || v.len() > 3 || v.iter().any(|a| a.is_unspecified() || a.is_multicast() || a.is_loopback()) {
        return Err(bad());
    }
    Ok(v.into_iter().map(|a| SocketAddr::new(a, 53)).collect())
}

/// A query id no one off the path can guess.
fn fresh_id() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    h.finish() as u16
}

/// Look `name` up through `ex`.
pub fn lookup(ex: &dyn Exchange, name: &str, kind: RecordKind) -> Result<(Vec<Record>, bool), ErrorCode> {
    if !nuntius::name_ok(name) {
        return Err(ErrorCode::Malformed);
    }
    let id = fresh_id();
    let m = ex.exchange(&query(id, name, kind)).map_err(|e| if e.kind() == io::ErrorKind::TimedOut { ErrorCode::Timeout } else { ErrorCode::Dns })?;
    answer(&m, id, name, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer to `q` with `records` (type, rdata), flags `extra` added.
    pub(crate) fn reply(q: &[u8], extra: u16, records: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let qend = 12 + q[12..].iter().position(|b| *b == 0).unwrap() + 1 + 4;
        let mut m = q[..qend].to_vec();
        let f = 0x8180 | extra;
        m[2..4].copy_from_slice(&f.to_be_bytes());
        m[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
        m[10..12].copy_from_slice(&[0, 0]);
        for (t, d) in records {
            m.extend_from_slice(&[0xC0, 12]);
            m.extend_from_slice(&t.to_be_bytes());
            m.extend_from_slice(&[0, 1, 0, 0, 0x0E, 0x10]);
            m.extend_from_slice(&(d.len() as u16).to_be_bytes());
            m.extend_from_slice(d);
        }
        m
    }

    #[test]
    fn queries_and_answers() {
        assert_eq!(nameservers("# x\nnameserver 127.0.0.1\nsearch a\nnameserver fe80::1%em0\n"), vec!["127.0.0.1".parse::<IpAddr>().unwrap(), "fe80::1".parse().unwrap()]);
        let q = query(0x1234, "sel._domainkey.example.com", RecordKind::Txt);
        assert_eq!(&q[..4], &[0x12, 0x34, 0x01, 0x20]);
        // A TXT split into two strings, a CNAME passed over, AD set.
        let mut txt = vec![5];
        txt.extend_from_slice(b"v=DKI");
        txt.push(4);
        txt.extend_from_slice(b"M1;x");
        let m = reply(&q, 0x0020, &[(5, vec![0xC0, 12]), (16, txt)]);
        assert_eq!(answer(&m, 0x1234, "SEL._domainkey.example.com", RecordKind::Txt), Ok((vec![Record::Txt("v=DKIM1;x".into())], true)));
        assert_eq!(answer(&m, 0x1235, "sel._domainkey.example.com", RecordKind::Txt), Err(ErrorCode::Malformed), "another id");
        assert_eq!(answer(&m, 0x1234, "other.example.com", RecordKind::Txt), Err(ErrorCode::Malformed), "another question");
        // No such name.
        assert_eq!(answer(&reply(&q, 3, &[]), 0x1234, "sel._domainkey.example.com", RecordKind::Txt), Ok((vec![], false)));
        assert_eq!(answer(&reply(&q, 2, &[]), 0x1234, "sel._domainkey.example.com", RecordKind::Txt), Err(ErrorCode::Dns));
        assert_eq!(answer(&reply(&q, 0x0200, &[]), 0x1234, "sel._domainkey.example.com", RecordKind::Txt), Err(ErrorCode::Dns), "truncated");

        // SRV and MX, names compressed against the question.
        let q = query(7, "_imaps._tcp.example.com", RecordKind::Srv);
        let m = reply(&q, 0, &[(33, vec![0, 0, 0, 5, 0x03, 0xE1, 4, b'i', b'm', b'a', b'p', 0xC0, 24])]);
        assert_eq!(answer(&m, 7, "_imaps._tcp.example.com", RecordKind::Srv).unwrap().0, vec![Record::Srv { priority: 0, weight: 5, port: 993, target: "imap.example.com".into() }]);
        let q = query(8, "example.com", RecordKind::Mx);
        let m = reply(&q, 0, &[(15, vec![0, 10, 2, b'm', b'x', 0xC0, 12])]);
        assert_eq!(answer(&m, 8, "example.com", RecordKind::Mx).unwrap().0, vec![Record::Mx { preference: 10, host: "mx.example.com".into() }]);

        // A pointer loop, and data past the end, are refused.
        let mut l = reply(&q, 0, &[(15, vec![0, 10, 0xC0, 0])]);
        let n = l.len();
        l[n - 2..].copy_from_slice(&[0xC0, (n - 2) as u8]);
        assert!(answer(&l, 8, "example.com", RecordKind::Mx).is_err());
        let mut short = reply(&q, 0, &[(15, vec![0, 10, 0])]);
        short.truncate(short.len() - 1);
        assert!(answer(&short, 8, "example.com", RecordKind::Mx).is_err());
        for cut in 0..m.len() {
            let _ = answer(&m[..cut], 8, "example.com", RecordKind::Mx);
        }
    }

    #[test]
    fn over_a_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let at = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut b = [0u8; 512];
            for _ in 0..2 {
                let (n, from) = server.recv_from(&mut b).unwrap();
                let q = b[..n].to_vec();
                // A stray answer with another id first.
                let mut stray = reply(&q, 0, &[]);
                stray[0] ^= 0xFF;
                server.send_to(&stray, from).unwrap();
                server.send_to(&reply(&q, 0x0020, &[(16, vec![3, b'a', b'b', b'c'])]), from).unwrap();
            }
        });
        let u = Udp::to(&[at]).unwrap();
        for _ in 0..2 {
            assert_eq!(lookup(&u, "_dmarc.example.com", RecordKind::Txt), Ok((vec![Record::Txt("abc".into())], true)));
        }
        assert_eq!(lookup(&u, "nodots", RecordKind::Txt), Err(ErrorCode::Malformed));
    }

    /// A portal's DNS: every name is the portal, sometimes through a CNAME.
    struct Portal;
    impl Exchange for Portal {
        fn exchange(&self, q: &[u8]) -> io::Result<Vec<u8>> {
            let qt = u16::from_be_bytes([q[q.len() - 15], q[q.len() - 14]]);
            // The question's name ends 4 bytes before the OPT record (11).
            Ok(match qt {
                1 => {
                    // CNAME to "p.lan" (written after the question), then
                    // its A; an A owned by something else is ignored.
                    let mut m = reply(q, 0, &[]);
                    let qend = m.len();
                    m[6..8].copy_from_slice(&3u16.to_be_bytes());
                    let target_at = qend + 12;
                    m.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 7, 1, b'p', 3, b'l', b'a', b'n', 0]);
                    m.extend_from_slice(&[0xC0, target_at as u8, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 88, 0, 1]);
                    m.extend_from_slice(&[1, b'x', 0, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 8, 8, 8, 8]);
                    m
                }
                _ => reply(q, 0, &[(28, vec![0xfd, 0x77, 0, 0, 0, 0, 0, 0x88, 0, 0, 0, 0, 0, 0, 0, 1])]),
            })
        }
    }

    #[test]
    fn a_sign_in_window_never_reaches_the_machine() {
        use crate::Connect;
        let via = ViaDns { net: &crate::Direct, dns: std::sync::Arc::new(Portal), own: vec!["10.88.0.50".parse().unwrap()] };
        for a in ["127.0.0.1", "::1", "0.0.0.0", "::", "::ffff:127.0.0.1", "10.88.0.50", "::ffff:10.88.0.50"] {
            assert!(!via.off_machine(a.parse().unwrap()), "{a}");
            assert_eq!(via.resolve(a, 80).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "{a}");
            assert_eq!(via.connect_addr(SocketAddr::new(a.parse().unwrap(), 80)).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "{a}");
        }
        for a in ["10.88.0.1", "192.168.1.1", "fd77:0:0:88::1", "fe80::1"] {
            assert!(via.off_machine(a.parse().unwrap()), "{a}");
        }
        let v = via.resolve("portal.example", 80).unwrap();
        assert_eq!(v.len(), 2, "the portal's own addresses pass: {v:?}");
        assert!(!own_addresses().is_empty(), "this host has a loopback at least");
        std::env::set_var(OWN_ENV, "10.88.0.50, fd77::88:1,junk");
        let own = own_addresses();
        std::env::remove_var(OWN_ENV);
        assert!(own.contains(&"10.88.0.50".parse().unwrap()) && own.contains(&"fd77::88:1".parse().unwrap()));
    }

    #[test]
    fn addresses_through_a_portals_dns() {
        let v = addresses(&Portal, "www.example.com").unwrap();
        assert_eq!(v, vec!["10.88.0.1".parse::<IpAddr>().unwrap(), "fd77:0:0:88::1".parse().unwrap()]);
        assert!(addresses(&Portal, "no dots").is_err());
        let q = query_type(9, "gone.example", 1);
        assert!(addr_answer(&reply(&q, 3, &[]), 9, "gone.example", 1).unwrap().is_empty());
        assert!(addr_answer(&reply(&q, 2, &[]), 9, "gone.example", 1).is_err());
        assert!(addr_answer(&reply(&q, 0, &[(1, vec![1, 2, 3])]), 9, "gone.example", 1).unwrap().is_empty(), "a short A is no address");
        for cut in 0..40 {
            let _ = addr_answer(&reply(&q, 0, &[(1, vec![1, 2, 3, 4])])[..cut], 9, "gone.example", 1);
        }
        assert_eq!(resolvers("10.88.0.53, fd77:0:0:88::53").unwrap().len(), 2);
        for bad in ["", "x", "127.0.0.1", "0.0.0.0", "1.1.1.1,1.0.0.1,9.9.9.9,8.8.8.8", "ff02::1"] {
            assert!(resolvers(bad).is_err(), "{bad}");
        }
    }
}
