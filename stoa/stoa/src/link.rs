//! A session's datagram link: its own UDP port (a client anywhere, the
//! mosh-style path), or a local one — a connected Unix datagram socket
//! handed over at attach (an app's console: the terminal is jailed with no
//! network, and gets its end from portcullisd; docs/spec/homes.md §4).
//!
//! The envelope (MAC, sequence, anti-replay) is the same on both. A local
//! link has no addresses: every datagram is from, and to, [`LOCAL`]. It
//! changes at each attach, and empties when the client's end is closed —
//! the session is then detached, and keeps running.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Room for a full-screen diff: a Unix datagram must fit the sender's
/// buffer whole (FreeBSD's default is 2 KiB), and a few queue at the
/// receiver. Both ends of a local link are set so.
pub const LOCAL_BUF: usize = 1 << 20;

/// Size a local link's socket buffers (best effort).
pub fn bufs(s: &UnixDatagram) {
    use std::os::fd::AsRawFd;
    let v = LOCAL_BUF as libc::c_int;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: an int option on a socket we own.
        unsafe {
            libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, opt,
                             &v as *const _ as *const libc::c_void, std::mem::size_of_val(&v) as libc::socklen_t);
        }
    }
}

/// The one "address" of a local link.
pub const LOCAL: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));

#[derive(Clone, Debug)]
pub enum Link {
    Udp(Arc<UdpSocket>),
    Local(Arc<Mutex<Option<UnixDatagram>>>),
}

impl Link {
    pub fn udp(s: UdpSocket) -> Link {
        Link::Udp(Arc::new(s))
    }

    /// A local link with no client yet.
    pub fn local() -> Link {
        Link::Local(Arc::new(Mutex::new(None)))
    }

    /// Attach a client's end (a local link only).
    pub fn attach(&self, s: UnixDatagram) -> io::Result<()> {
        match self {
            Link::Local(slot) => {
                s.set_read_timeout(Some(Duration::from_millis(200)))?;
                bufs(&s);
                *slot.lock().unwrap() = Some(s);
                Ok(())
            }
            Link::Udp(_) => Err(io::Error::new(io::ErrorKind::Unsupported, "a UDP session attaches by mint")),
        }
    }

    /// A local link with a client on it now.
    pub fn attached(&self) -> bool {
        match self {
            Link::Local(slot) => slot.lock().unwrap().is_some(),
            Link::Udp(_) => true,
        }
    }

    /// Is the client's end still open? A local link drops a closed one
    /// (an empty datagram costs nothing; the client ignores it).
    pub fn check(&self) -> bool {
        match self {
            Link::Local(slot) => {
                let mut g = slot.lock().unwrap();
                if let Some(s) = g.as_ref() {
                    if s.send(&[]).is_err() {
                        *g = None;
                    }
                }
                g.is_some()
            }
            Link::Udp(_) => true,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            Link::Udp(s) => s.local_addr().map(|a| a.port()).unwrap_or(0),
            Link::Local(_) => 0,
        }
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Link::Udp(s) => s.set_read_timeout(d),
            Link::Local(_) => Ok(()), // set at attach
        }
    }

    pub fn try_clone(&self) -> io::Result<Link> {
        Ok(self.clone())
    }

    pub fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        match self {
            Link::Udp(s) => s.send_to(buf, addr),
            Link::Local(slot) => {
                let mut g = slot.lock().unwrap();
                let Some(s) = g.as_ref() else {
                    return Err(io::Error::new(io::ErrorKind::NotConnected, "detached"));
                };
                match s.send(buf) {
                    Ok(n) => Ok(n),
                    // A full queue (a slow client) or a datagram too big
                    // loses it, as UDP would; the client sees the gap in
                    // sequence numbers and asks for a full repaint.
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock
                        || matches!(e.raw_os_error(), Some(libc::EMSGSIZE) | Some(libc::ENOBUFS)) => Ok(0),
                    Err(e) => {
                        *g = None;
                        Err(e)
                    }
                }
            }
        }
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Link::Udp(s) => s.recv_from(buf),
            Link::Local(slot) => {
                let s = slot.lock().unwrap().as_ref().map(|s| s.try_clone()).transpose()?;
                let Some(s) = s else {
                    std::thread::sleep(Duration::from_millis(200));
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "detached"));
                };
                match s.recv(buf) {
                    // Nothing on a datagram socket is a closed peer's
                    // or an empty probe: neither is a message.
                    Ok(0) => Err(io::Error::new(io::ErrorKind::WouldBlock, "empty")),
                    Ok(n) => Ok((n, LOCAL)),
                    Err(e) => Err(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_link_detaches_when_the_client_goes() {
        let l = Link::local();
        assert!(!l.attached());
        assert!(l.send_to(b"x", LOCAL).is_err(), "nobody to send to");
        let (ours, theirs) = UnixDatagram::pair().unwrap();
        l.attach(ours).unwrap();
        assert!(l.attached() && l.check());
        l.send_to(b"out", LOCAL).unwrap();
        let mut b = [0u8; 16];
        assert_eq!(theirs.recv(&mut b).unwrap(), 3);
        theirs.send(b"in").unwrap();
        assert_eq!(l.recv_from(&mut b).unwrap(), (2, LOCAL));
        drop(theirs);
        assert!(!l.check(), "a closed end is noticed");
        assert!(!l.attached());
    }
}
