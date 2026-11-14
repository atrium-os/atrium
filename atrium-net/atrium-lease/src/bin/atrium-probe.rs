//! atrium-probe: the connectivity and captive-portal probe
//! (docs/spec/atrium-net.md §4.2). atrium-net starts one before it enters
//! capability mode (where it can no longer connect anywhere) with a stream
//! socketpair on fd 3. It sandboxes itself before reading anything: chroot to
//! an empty directory, its own uid, no new processes, few descriptors. It
//! needs the network, so it cannot use capability mode; what it parses (one
//! DNS reply, one HTTP reply's head) and what it may say back are small, and
//! atrium-net checks every answer.
//!
//! One request at a time, in two steps, so atrium-net can pin a host route
//! through the interface being probed before each one:
//!   net -> probe: `resolve id=N dns=A.B.C.D name=HOST`
//!   probe -> net: `addr id=N a=A.B.C.D` | `fail id=N why=..`
//!   net -> probe: `fetch id=N a=A.B.C.D host=HOST path=/P [expect=HEX]`
//!   probe -> net: `http id=N status=NNN match=0|1 [location=HEX]` | `fail id=N why=..`
//!
//! usage: atrium-probe [-v] [--no-sandbox] [--uid UID]

use atrium_lease::probe;
use aura_ipc::sock::Conn;
use aura_ipc::{Got, Line};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const PROBE_UID: u32 = 65035;
const DNS_TRY: Duration = Duration::from_secs(2);
const CONNECT: Duration = Duration::from_secs(4);
const HTTP_TOTAL: Duration = Duration::from_secs(5);

fn die(s: &str) -> ! {
    eprintln!("atrium-probe: {s}");
    std::process::exit(1)
}

#[cfg(target_os = "freebsd")]
fn sandbox(uid: u32) {
    unsafe {
        if libc::chroot(c"/var/empty".as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
            die("chroot /var/empty");
        }
        let g = uid as libc::gid_t;
        if libc::setgroups(1, &g) != 0 || libc::setgid(g) != 0 || libc::setuid(uid) != 0 {
            die("drop to the probe uid");
        }
        if libc::setuid(0) == 0 {
            die("could regain root");
        }
        // No children, and room for fd 3 plus one socket at a time.
        let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        let few = libc::rlimit { rlim_cur: 8, rlim_max: 8 };
        if libc::setrlimit(libc::RLIMIT_NPROC, &none) != 0 || libc::setrlimit(libc::RLIMIT_NOFILE, &few) != 0 {
            die("setrlimit");
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn sandbox(_uid: u32) {
    die("the sandbox needs FreeBSD; use --no-sandbox for a host run");
}

fn rand16() -> u16 {
    let mut b = [0u8; 2];
    if unsafe { libc::getrandom(b.as_mut_ptr().cast(), 2, 0) } != 2 {
        die("getrandom");
    }
    u16::from_ne_bytes(b)
}

fn resolve(dns: Ipv4Addr, name: &str) -> Result<Ipv4Addr, &'static str> {
    let s = UdpSocket::bind("0.0.0.0:0").map_err(|_| "socket")?;
    s.connect(SocketAddr::from((dns, 53))).map_err(|_| "dns-unreachable")?;
    s.set_read_timeout(Some(DNS_TRY)).map_err(|_| "socket")?;
    let mut buf = [0u8; 1500];
    let mut last = "dns-timeout";
    for _ in 0..2 {
        let q = probe::dns_query(rand16(), name);
        if s.send(&q).is_err() {
            return Err("dns-unreachable");
        }
        let end = Instant::now() + DNS_TRY;
        while Instant::now() < end {
            let Ok(n) = s.recv(&mut buf) else { break };
            match probe::dns_answer(&q, &buf[..n]) {
                Ok(a) => return Ok(a),
                Err(probe::DnsErr::NotOurs) => continue,
                Err(probe::DnsErr::Rcode(3)) => return Err("nxdomain"),
                Err(_) => last = "dns-bad-reply",
            }
        }
    }
    Err(last)
}

fn fetch(a: Ipv4Addr, host: &str, path: &str, expect: Option<&[u8]>) -> Result<probe::Http, &'static str> {
    let mut t = TcpStream::connect_timeout(&SocketAddr::from((a, 80)), CONNECT).map_err(|e| match e.raw_os_error() {
        _ if e.kind() == std::io::ErrorKind::TimedOut => "connect-timeout",
        Some(libc::ECONNREFUSED) => "connect-refused",
        Some(libc::EHOSTUNREACH) | Some(libc::EHOSTDOWN) => "host-unreachable",
        Some(libc::ENETUNREACH) | Some(libc::ENETDOWN) => "net-unreachable",
        _ => "connect-failed",
    })?;
    t.set_write_timeout(Some(CONNECT)).ok();
    t.write_all(&probe::http_request(host, path)).map_err(|_| "send")?;
    let end = Instant::now() + HTTP_TOTAL;
    let mut b = Vec::new();
    let mut chunk = [0u8; 2048];
    while b.len() < probe::HTTP_MAX {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        t.set_read_timeout(Some(left)).ok();
        match t.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => b.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    if b.is_empty() {
        return Err("no-reply");
    }
    probe::http_reply(&b, expect).ok_or("bad-reply")
}

fn main() {
    let mut verbose = false;
    let mut sandboxed = true;
    let mut uid = PROBE_UID;
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "-v" => verbose = true,
            "--no-sandbox" => sandboxed = false,
            "--uid" => uid = a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--uid")),
            _ => die("usage: atrium-probe [-v] [--no-sandbox] [--uid UID]"),
        }
    }
    let s = unsafe { UnixStream::from_raw_fd(3) };
    if sandboxed {
        sandbox(uid);
    }
    let mut c = Conn::new(s).unwrap_or_else(|_| die("fd 3 is not a socket"));
    loop {
        let ready = aura_ipc::sock::poll_read(&[c.fd()], 60_000);
        if ready.first() != Some(&true) {
            continue;
        }
        let lines = c.lines().unwrap_or_else(|_| std::process::exit(0));
        for g in lines {
            let Got::Line(l) = g else { die("bad line from atrium-net") };
            let id = l.get("id").unwrap_or("0").to_string();
            let reply = match l.verb.as_str() {
                "resolve" => {
                    let dns = l.get("dns").and_then(|x| x.parse::<Ipv4Addr>().ok());
                    let name = l.get("name").filter(|n| probe::valid_name(n));
                    match (dns, name) {
                        (Some(d), Some(n)) => match resolve(d, n) {
                            Ok(a) => Line::new("addr").with("id", &id).with("a", a),
                            Err(why) => Line::new("fail").with("id", &id).with("why", why),
                        },
                        _ => Line::new("fail").with("id", &id).with("why", "bad-request"),
                    }
                }
                "fetch" => {
                    let a = l.get("a").and_then(|x| x.parse::<Ipv4Addr>().ok());
                    let host = l.get("host").filter(|n| probe::valid_name(n));
                    let path = l.get("path").filter(|p| probe::valid_path(p));
                    let expect = l.hex("expect");
                    match (a, host, path) {
                        (Some(a), Some(h), Some(p)) => match fetch(a, h, p, expect.as_deref()) {
                            Ok(r) => {
                                let mut x = Line::new("http").with("id", &id).with("status", r.status).with("match", r.matched as u8);
                                if let Some(loc) = r.location {
                                    x = x.with_hex("location", loc.as_bytes());
                                }
                                x
                            }
                            Err(why) => Line::new("fail").with("id", &id).with("why", why),
                        },
                        _ => Line::new("fail").with("id", &id).with("why", "bad-request"),
                    }
                }
                _ => die("unexpected message from atrium-net"),
            };
            if verbose {
                eprintln!("atrium-probe: {} -> {}", l.verb, reply.verb);
            }
            if c.send(&reply).is_err() {
                std::process::exit(0);
            }
        }
    }
}
