//! `listen` from this computer (background-apps.md §4.3): a forwarder on
//! 127.0.0.1 for each such port of a running app.
//!
//! pf skips lo0, so a redirect there can't work, and a redirect could not
//! tell one local user from another anyway. The forwarder asks the kernel
//! which uid opened each connection (`net.inet.tcp.getcred`) and lets
//! through only the person's login uid and their own apps' uids from the
//! launch registry. Everything else is closed at once. The app's port is
//! closed to every other local process by its anchor (only root, which is
//! this forwarder, may connect from the host).
//!
//! It copies bytes and parses nothing. At most [`MAX_CONNS`] connections
//! per port; a connection idle for [`IDLE`] is closed.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::protocol::{ListenFrom, NetListen};

pub const MAX_CONNS: usize = 16;
const IDLE: Duration = Duration::from_secs(300);
const CONNECT: Duration = Duration::from_secs(5);

struct Running {
    port: u16,
    stop: Arc<AtomicBool>,
}

fn running() -> &'static Mutex<HashMap<String, Vec<Running>>> {
    static R: OnceLock<Mutex<HashMap<String, Vec<Running>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Start the forwarders `listen` asks for, for `jail` at `app` (its
/// address). Replaces any it had. A port that can't be bound is logged and
/// skipped: the app runs, unreachable there.
pub fn start(jail: &str, app: &str, listen: &[NetListen]) {
    stop(jail);
    let Ok(app) = app.parse::<Ipv4Addr>() else { return };
    let mut started = Vec::new();
    for l in listen {
        let ListenFrom::ThisComputer { owner_uid, owner } = &l.from else { continue };
        // A forwarder just stopped for this port lets go within its poll.
        let mut bound = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, l.port));
        for _ in 0..10 {
            match &bound {
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    std::thread::sleep(Duration::from_millis(100));
                    bound = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, l.port));
                }
                _ => break,
            }
        }
        let lst = match bound {
            Ok(s) => s,
            Err(e) => {
                log::warn!("jaild: listen {jail}: 127.0.0.1:{} not bound: {e}", l.port);
                continue;
            }
        };
        if lst.set_nonblocking(true).is_err() {
            continue;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (s, j, uid, who, port) = (stop.clone(), jail.to_string(), *owner_uid, owner.clone(), l.port);
        std::thread::spawn(move || serve(lst, &j, SocketAddrV4::new(app, port), uid, &who, &s));
        log::info!("jaild: listen {jail}: 127.0.0.1:{port} for {owner}");
        started.push(Running { port, stop });
    }
    if !started.is_empty() {
        running().lock().unwrap_or_else(|e| e.into_inner()).insert(jail.into(), started);
    }
}

/// Stop `jail`'s forwarders. Connections already through end when either
/// side closes (the jail going closes them).
pub fn stop(jail: &str) {
    if let Some(v) = running().lock().unwrap_or_else(|e| e.into_inner()).remove(jail) {
        for r in v {
            r.stop.store(true, Ordering::SeqCst);
            log::info!("jaild: listen {jail}: 127.0.0.1:{} closed", r.port);
        }
    }
}

fn serve(lst: TcpListener, jail: &str, to: SocketAddrV4, owner_uid: u32, owner: &str, stop: &AtomicBool) {
    let live = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        let (c, peer) = match lst.accept() {
            Ok(x) => x,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let uid = match (peer, c.local_addr()) {
            (SocketAddr::V4(p), Ok(SocketAddr::V4(l))) => tcp_uid(p, l),
            _ => Err(io::Error::other("not IPv4")),
        };
        let allowed = match &uid {
            Ok(u) => *u == owner_uid || owners_app(*u, owner),
            Err(_) => false,
        };
        if !allowed {
            log::warn!("jaild: listen {jail}: refused 127.0.0.1:{} to uid {:?}: not {owner}'s", to.port(), uid.ok());
            continue; // dropping `c` closes it
        }
        if live.load(Ordering::SeqCst) >= MAX_CONNS {
            log::warn!("jaild: listen {jail}: {MAX_CONNS} connections already; one refused");
            continue;
        }
        live.fetch_add(1, Ordering::SeqCst);
        let live = live.clone();
        std::thread::spawn(move || {
            let _ = splice(c, to);
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Whether `uid` is one of `owner`'s app instances (not a session
/// component, not another person's). An unreadable registry denies.
fn owners_app(uid: u32, owner: &str) -> bool {
    match portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY) {
        Ok(r) => !r.is_session(uid) && r.resolve(uid).is_some_and(|(o, _)| o == owner),
        Err(_) => false,
    }
}

fn splice(c: TcpStream, to: SocketAddrV4) -> io::Result<()> {
    c.set_nonblocking(false)?;
    let a = TcpStream::connect_timeout(&SocketAddr::V4(to), CONNECT)?;
    for s in [&c, &a] {
        s.set_read_timeout(Some(IDLE))?;
        s.set_write_timeout(Some(IDLE))?;
    }
    let (c2, a2) = (c.try_clone()?, a.try_clone()?);
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut &c2, &mut &a2);
        let _ = a2.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut &a, &mut &c);
    let _ = c.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// The uid that owns the connecting socket of the TCP connection from
/// `peer` to `local` (both on this computer).
#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)]
fn tcp_uid(peer: SocketAddrV4, local: SocketAddrV4) -> io::Result<u32> {
    fn sin(a: SocketAddrV4) -> libc::sockaddr_in {
        // SAFETY: sockaddr_in is plain old data; all-zero is a valid value,
        // and every field that matters is set below.
        let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        s.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        s.sin_family = libc::AF_INET as u8;
        s.sin_port = a.port().to_be();
        s.sin_addr.s_addr = u32::from(*a.ip()).to_be();
        s
    }
    // tcp_getcred looks up the pcb whose local end is addrs[0] and whose
    // foreign end is addrs[1]: the connecting socket's.
    let addrs = [sin(peer), sin(local)];
    // SAFETY: xucred is plain old data; the kernel fills it.
    let mut cred: libc::xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xucred>();
    let name = c"net.inet.tcp.getcred";
    // SAFETY: name is NUL-terminated; oldp/oldlenp point at `cred` and its
    // size, newp/newlen at `addrs` and its size, all live for the call.
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr(), (&mut cred as *mut libc::xucred).cast(), &mut len,
                           addrs.as_ptr().cast(), std::mem::size_of_val(&addrs))
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len != std::mem::size_of::<libc::xucred>() || cred.cr_version != libc::XUCRED_VERSION {
        return Err(io::Error::other("net.inet.tcp.getcred: unexpected xucred"));
    }
    Ok(cred.cr_uid)
}

#[cfg(not(target_os = "freebsd"))]
fn tcp_uid(_: SocketAddrV4, _: SocketAddrV4) -> io::Result<u32> {
    Err(io::Error::other("net.inet.tcp.getcred is FreeBSD's"))
}
