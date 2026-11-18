//! atrium-mdns: `.local` names for unbound (docs/spec/atrium-net.md §6.1).
//!
//! Started by atrium-net's spawner with fd 3, a socket to atrium-net. The
//! first line there, `start addr=A/P`, comes with three descriptors: the
//! TCP listener unbound's stub zone talks to (127.0.0.1:5354) and the
//! receive and send mDNS sockets of the active interface. Then it chroots
//! to /var/empty, drops to its uid and enters capability mode before it
//! reads anything else. It exits when atrium-net closes fd 3 (another
//! network: a new atrium-mdns with new sockets).
//!
//!   atrium-mdns [--uid N] [-v]

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use atrium_mdns::{Net, Out, Resolver};
use aura_ipc::Line;

const MAX_CONNS: usize = 16;
const CONN_IDLE: Duration = Duration::from_secs(10);
const MAX_MSG: usize = 512;

fn die(s: &str) -> ! {
    eprintln!("atrium-mdns: {s}");
    std::process::exit(1)
}

/// The start line and the descriptors sent with it.
fn start(s: &UnixStream) -> (Line, Vec<OwnedFd>) {
    let mut got = Vec::new();
    let mut fds = Vec::new();
    loop {
        let mut buf = [0u8; 256];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut cbuf = [0u64; 8];
        // SAFETY: plain data; buffers outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
        // SAFETY: valid socket and message.
        let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, 0) };
        if n <= 0 {
            die("atrium-net went away before start");
        }
        // SAFETY: the kernel filled the control buffer.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let k = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                    let d = libc::CMSG_DATA(c) as *const RawFd;
                    for i in 0..k {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(d.add(i))));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        got.extend_from_slice(&buf[..n as usize]);
        if let Some(e) = got.iter().position(|&b| b == b'\n') {
            let l = std::str::from_utf8(&got[..e]).ok().and_then(Line::parse).unwrap_or_else(|| die("bad start line"));
            return (l, fds);
        }
        if got.len() > 1024 {
            die("start line too long");
        }
    }
}

#[cfg(target_os = "freebsd")]
fn sandbox(uid: Option<u32>) {
    // SAFETY: plain system calls.
    unsafe {
        if let Some(uid) = uid {
            if libc::chroot(c"/var/empty".as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
                die("chroot /var/empty");
            }
            let g = uid as libc::gid_t;
            if libc::setgroups(1, &g) != 0 || libc::setgid(g) != 0 || libc::setuid(uid) != 0 {
                die("drop to its uid");
            }
            if libc::setuid(0) == 0 {
                die("could get root back");
            }
        }
        if libc::cap_enter() != 0 {
            die("cap_enter");
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn sandbox(_uid: Option<u32>) {}

/// `v6=P1,P2`: the interface's global and unique-local /64s, each written
/// as an address in it. Anything else is left out.
fn v6_prefixes(s: &str) -> Vec<Ipv6Addr> {
    s.split(',')
        .filter_map(|p| p.trim_end_matches("/64").parse::<Ipv6Addr>().ok())
        .filter(|a| !a.is_loopback() && !a.is_unspecified() && a.segments()[0] & 0xffc0 != 0xfe80)
        .take(4)
        .collect()
}

struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    last: Instant,
}

fn main() {
    let (mut uid, mut v) = (None, false);
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "--uid" => uid = Some(a.next().and_then(|n| n.parse().ok()).unwrap_or_else(|| die("--uid N"))),
            "-v" => v = true,
            _ => die("usage: atrium-mdns [--uid N] [-v]"),
        }
    }
    // SAFETY: fd 3 is ours, from the spawner.
    let ctl = unsafe { UnixStream::from_raw_fd(3) };
    let (l, mut fds) = start(&ctl);
    let net = l
        .get("addr")
        .and_then(|s| s.split_once('/'))
        .and_then(|(a, p)| Some((a.parse::<Ipv4Addr>().ok()?, p.parse().ok()?)))
        .map(|(addr, prefix)| Net { addr, prefix, v6: v6_prefixes(l.get("v6").unwrap_or("")) })
        .unwrap_or_else(|| die("start: addr=A/P"));
    if l.verb != "start" || fds.len() != 3 {
        die("start: three descriptors");
    }
    let send = UdpSocket::from(fds.pop().unwrap());
    let recv = UdpSocket::from(fds.pop().unwrap());
    let lis = TcpListener::from(fds.pop().unwrap());
    sandbox(uid);
    let _ = lis.set_nonblocking(true);
    let _ = recv.set_nonblocking(true);
    let _ = ctl.set_nonblocking(true);
    if v {
        eprintln!("atrium-mdns: serving .local for {}/{} v6 {:?}", net.addr, net.prefix, net.v6);
    }
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut r: Resolver<u64> = Resolver::new(net.clone());
    let mut conns: Vec<(u64, Conn)> = Vec::new();
    let mut next_id = 0u64;
    let mut asked = 0u64;
    loop {
        let mut pfd: Vec<libc::pollfd> = [ctl.as_raw_fd(), lis.as_raw_fd(), recv.as_raw_fd()]
            .iter()
            .chain(conns.iter().map(|(_, c)| c.s.as_raw_fd()).collect::<Vec<_>>().iter())
            .map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
            .collect();
        let wait = r.next_due().map(|d| d.saturating_sub(now()).min(1000) as i32).unwrap_or(1000);
        // SAFETY: valid array of pollfds.
        unsafe { libc::poll(pfd.as_mut_ptr(), pfd.len() as _, wait) };
        let mut outs = Vec::new();
        if pfd[0].revents != 0 {
            let mut b = [0u8; 64];
            match (&ctl).read(&mut b) {
                Ok(0) => std::process::exit(0),
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => std::process::exit(0),
                _ => {}
            }
        }
        if pfd[1].revents != 0 {
            while let Ok((s, _)) = lis.accept() {
                if conns.len() >= MAX_CONNS {
                    continue; // dropped: unbound tries again
                }
                let _ = s.set_nonblocking(true);
                next_id += 1;
                conns.push((next_id, Conn { s, buf: Vec::new(), last: Instant::now() }));
            }
        }
        if pfd[2].revents != 0 {
            let mut b = [0u8; 9000];
            for _ in 0..256 {
                match recv.recv(&mut b) {
                    Ok(n) => r.mdns(&b[..n]),
                    Err(_) => break,
                }
            }
        }
        let mut gone = Vec::new();
        for (k, (id, c)) in conns.iter_mut().enumerate() {
            // Accepted after the poll: nothing to read yet.
            if pfd.get(3 + k).map_or(0, |p| p.revents) == 0 {
                if c.last.elapsed() > CONN_IDLE {
                    gone.push(*id);
                }
                continue;
            }
            let mut b = [0u8; 1024];
            match c.s.read(&mut b) {
                Ok(0) => gone.push(*id),
                Ok(n) => {
                    c.last = Instant::now();
                    c.buf.extend_from_slice(&b[..n]);
                    // TCP DNS: two length bytes, then the message.
                    while c.buf.len() >= 2 {
                        let len = u16::from_be_bytes([c.buf[0], c.buf[1]]) as usize;
                        if len > MAX_MSG {
                            gone.push(*id);
                            break;
                        }
                        if c.buf.len() < 2 + len {
                            break;
                        }
                        let m: Vec<u8> = c.buf.drain(..2 + len).skip(2).collect();
                        outs.extend(r.query(*id, &m, now()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => gone.push(*id),
            }
        }
        outs.extend(r.tick(now()));
        for o in outs {
            match o {
                Out::Ask(q) => {
                    asked += 1;
                    if let Err(e) = send.send(&q) {
                        eprintln!("atrium-mdns: query: {e}");
                    } else if v {
                        eprintln!("atrium-mdns: asked ({asked})");
                    }
                }
                Out::Reply(id, m) => {
                    if let Some((_, c)) = conns.iter_mut().find(|(i, _)| *i == id) {
                        let mut f = (m.len() as u16).to_be_bytes().to_vec();
                        f.extend_from_slice(&m);
                        let _ = c.s.set_nonblocking(false);
                        let _ = c.s.set_write_timeout(Some(Duration::from_millis(200)));
                        if c.s.write_all(&f).is_err() {
                            gone.push(id);
                        }
                        let _ = c.s.set_nonblocking(true);
                    }
                }
            }
        }
        conns.retain(|(id, _)| !gone.contains(id));
    }
}
