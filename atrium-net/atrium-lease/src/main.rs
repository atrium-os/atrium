//! atrium-lease: one per interface, started by atrium-net with a stream
//! socketpair on fd 3 (docs/spec/atrium-net.md §3). It sandboxes itself
//! before reading anything: chroot to an empty directory, drop to its uid,
//! Capsicum capability mode. From then on it holds one descriptor and can
//! neither reach the network nor open a file.
//!
//! atrium-net -> lease: `hello mac=M`, `up`, `down`, `frame hex=..`,
//!                      `refused why=..`
//! lease -> atrium-net: `send dest=bcast|A.B.C.D dmac=M hex=..` (a DHCP
//!                      payload; atrium-net wraps it), `lease4 ..`,
//!                      `lost why=..`, `state s=..`
//!
//! usage: atrium-lease [-v] [--no-sandbox] [--uid UID]

use atrium_lease::client::{Client, Dest, Out};
use atrium_lease::frame;
use aura_ipc::sock::{poll_read, Conn};
use aura_ipc::{fmt_mac, Got, Line};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::Instant;

const LEASE_UID: u32 = 65034;

fn die(s: &str) -> ! {
    eprintln!("atrium-lease: {s}");
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
            die("drop to the lease uid");
        }
        if libc::setuid(0) == 0 {
            die("could regain root");
        }
        if libc::cap_enter() != 0 {
            die("cap_enter");
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn sandbox(_uid: u32) {
    die("the sandbox needs FreeBSD; use --no-sandbox for a host run");
}

fn rand() -> u32 {
    let mut b = [0u8; 4];
    getrandom(&mut b);
    u32::from_ne_bytes(b)
}

fn getrandom(b: &mut [u8]) {
    let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    if n != b.len() as isize {
        die("getrandom");
    }
}

fn main() {
    let mut verbose = false;
    let mut sandboxed = true;
    let mut uid = LEASE_UID;
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "-v" => verbose = true,
            "--no-sandbox" => sandboxed = false,
            "--uid" => uid = a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--uid")),
            _ => die("usage: atrium-lease [-v] [--no-sandbox] [--uid UID]"),
        }
    }
    let s = unsafe { UnixStream::from_raw_fd(3) };
    if sandboxed {
        sandbox(uid);
    }
    let mut c = Conn::new(s).unwrap_or_else(|_| die("fd 3 is not a socket"));
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut client: Option<Client> = None;
    let mut dmac = [0xffu8; 6];

    loop {
        let wait = match client.as_ref().and_then(|c| c.deadline()) {
            Some(d) => d.saturating_sub(now()).min(60_000) as i32,
            None => 60_000,
        };
        let ready = poll_read(&[c.fd()], wait);
        let mut outs = Vec::new();
        if ready.first() == Some(&true) {
            let lines = c.lines().unwrap_or_else(|_| std::process::exit(0));
            for g in lines {
                let l = match g {
                    Got::Line(l) => l,
                    _ => die("bad line from atrium-net"),
                };
                match (l.verb.as_str(), client.as_mut()) {
                    ("hello", None) => {
                        let mac = l.mac("mac").unwrap_or_else(|| die("hello without mac"));
                        client = Some(Client::new(mac, Box::new(rand)));
                    }
                    ("up", Some(cl)) => outs.extend(cl.start(now())),
                    ("down", Some(cl)) => cl.stop(),
                    ("refused", Some(cl)) => {
                        if verbose {
                            eprintln!("atrium-lease: atrium-net refused the lease ({})", l.get("why").unwrap_or("?"));
                        }
                        cl.refused(now())
                    }
                    ("frame", Some(cl)) => {
                        let Some(f) = l.hex("hex") else { continue };
                        if let Some((src_mac, _src, p)) = frame::dhcp_from_frame_mac(&f) {
                            let o = cl.rx(p, now());
                            if o.iter().any(|x| matches!(x, Out::Bound(_))) {
                                dmac = src_mac;
                            }
                            outs.extend(o);
                        }
                    }
                    _ => die("unexpected message from atrium-net"),
                }
            }
        }
        if let Some(cl) = client.as_mut() {
            outs.extend(cl.tick(now()));
        }
        for o in outs {
            let line = match o {
                Out::Send { to, payload } => {
                    let (dest, m) = match to {
                        Dest::Broadcast => ("bcast".to_string(), [0xff; 6]),
                        Dest::Server(a) => (a.to_string(), dmac),
                    };
                    Line::new("send").with("dest", dest).with("dmac", fmt_mac(&m)).with_hex("hex", &payload)
                }
                Out::Bound(l) => l.to_line(),
                Out::Lost(why) => Line::new("lost").with("why", why),
            };
            if c.send(&line).is_err() {
                std::process::exit(0);
            }
        }
    }
}
