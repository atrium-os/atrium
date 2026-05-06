//! FreeBSD backend: parse `devd(8)` notifications from the
//! seqpacket pipe.
//!
//! devd is the FreeBSD userspace daemon that reads kernel newbus
//! events from `/dev/devctl` and republishes them in a parseable
//! line format. We only care about the cdev (devfs) lifecycle
//! subset — when `/dev/hidrawN`, `/dev/dspN`, etc. appear and
//! disappear. The rest (USB attach/detach, PCI probe) is interesting
//! for diagnostics but a layer above what consumers want.
//!
//! Two pipes exist:
//!
//!   /var/run/devd.pipe            — SOCK_STREAM, line-terminated
//!   /var/run/devd.seqpacket.pipe  — SOCK_SEQPACKET, one event per
//!                                   recv()
//!
//! We use the seqpacket variant — message boundaries from the
//! kernel match recv boundaries, no need to scan for newlines.
//!
//! Notification format (the subset we care about):
//!
//!   !system=DEVFS subsystem=CDEV type=CREATE cdev=hidraw0
//!   !system=DEVFS subsystem=CDEV type=DESTROY cdev=hidraw0
//!
//! Plus the older simple form (still emitted for some events):
//!
//!   +ugen0.4 at port=4 ... on usbus0
//!   -ugen0.4 at ...
//!
//! For DEVFS+CDEV events we synthesize `/dev/<cdev>` as the devnode.
//! Other events come through as `Event::Other { raw }`.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;

use crate::Event;

const DEVD_PIPE: &str = "/var/run/devd.seqpacket.pipe";
const MAX_MSG: usize = 8192;

pub struct DeviceWatcher {
    sock: UnixDatagram,
}

impl DeviceWatcher {
    /// Connect to devd's seqpacket pipe. Fails if devd isn't
    /// running (typically only an issue inside heavily-stripped
    /// jails — desktop systems always have it).
    pub fn open() -> io::Result<Self> {
        let sock = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0);
            if fd < 0 { return Err(io::Error::last_os_error()); }
            std::os::fd::FromRawFd::from_raw_fd(fd)
        };
        let sock: UnixDatagram = sock;
        sock.connect(Path::new(DEVD_PIPE))?;
        Ok(Self { sock })
    }

    /// Block for the next event. Use `as_raw_fd` + `kqueue(EVFILT_READ)`
    /// to multiplex with other fds.
    pub fn recv(&self) -> io::Result<Event> {
        let mut buf = vec![0u8; MAX_MSG];
        let n = self.sock.recv(&mut buf)?;
        buf.truncate(n);
        Ok(parse(&buf))
    }

    /// Non-blocking variant. Returns `Ok(None)` if no event is
    /// pending, mirroring `try_recv` semantics elsewhere in Atrium.
    pub fn try_recv(&self) -> io::Result<Option<Event>> {
        self.sock.set_nonblocking(true)?;
        let mut buf = vec![0u8; MAX_MSG];
        let r = self.sock.recv(&mut buf);
        self.sock.set_nonblocking(false)?;
        match r {
            Ok(n) => { buf.truncate(n); Ok(Some(parse(&buf))) }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl AsRawFd for DeviceWatcher {
    fn as_raw_fd(&self) -> RawFd { self.sock.as_raw_fd() }
}

fn parse(bytes: &[u8]) -> Event {
    /* devd messages are ASCII; lossy decode is safe — anything non-
     * ASCII means a corrupt frame and surfaces as Other. */
    let s = String::from_utf8_lossy(bytes).trim_end_matches(['\0', '\n']).to_string();

    /* Notify form: '!system=A subsystem=B type=C k=v...'
     * We extract DEVFS+CDEV CREATE/DESTROY specifically. */
    if let Some(rest) = s.strip_prefix('!') {
        let mut system = None;
        let mut subsystem = None;
        let mut typ = None;
        let mut cdev = None;
        for kv in rest.split_whitespace() {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "system"    => system = Some(v),
                    "subsystem" => subsystem = Some(v),
                    "type"      => typ = Some(v),
                    "cdev"      => cdev = Some(v),
                    _ => {}
                }
            }
        }
        if system == Some("DEVFS") && subsystem == Some("CDEV") {
            if let Some(name) = cdev {
                let devnode = format!("/dev/{name}");
                return match typ {
                    Some("CREATE")  => Event::Added   { devnode },
                    Some("DESTROY") => Event::Removed { devnode },
                    _ => Event::Other { raw: s },
                };
            }
        }
    }
    Event::Other { raw: s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_create() {
        let m = parse(b"!system=DEVFS subsystem=CDEV type=CREATE cdev=hidraw0");
        match m {
            Event::Added { devnode } => assert_eq!(devnode, "/dev/hidraw0"),
            _ => panic!("expected Added, got {m:?}"),
        }
    }

    #[test]
    fn parse_destroy() {
        let m = parse(b"!system=DEVFS subsystem=CDEV type=DESTROY cdev=hidraw0");
        match m {
            Event::Removed { devnode } => assert_eq!(devnode, "/dev/hidraw0"),
            _ => panic!("expected Removed, got {m:?}"),
        }
    }

    #[test]
    fn parse_other() {
        let m = parse(b"+ugen0.4 at port=4 on usbus0");
        match m {
            Event::Other { raw } => assert!(raw.starts_with('+')),
            _ => panic!("expected Other, got {m:?}"),
        }
    }
}
