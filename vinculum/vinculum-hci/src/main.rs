//! vinculum-hci: relays one controller to one host (docs/spec/bluetooth.md §5).
//!
//! ```text
//! vinculum-hci --controller tcp:HOST:PORT --host-listen PATH
//! ```
//!
//! B1 transports: `tcp:` (H4 over TCP, Rootcanal) and `uart:PATH` (H4 on a
//! serial line, raw mode). USB through libusb on `ugen`, and the vendor
//! firmware loaders, come with the first real dongle (B1b). The host side is
//! a Unix socket the host connects to (in production portcullisd passes a
//! socketpair instead). When the host disconnects, the controller is reset,
//! so no link outlives the host that knows its keys (§4).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use vinculum_hci::relay::{Out, Relay};

enum Controller {
    Tcp(TcpStream),
    Uart(std::fs::File),
}

impl Controller {
    fn open(spec: &str) -> std::io::Result<Controller> {
        if let Some(addr) = spec.strip_prefix("tcp:") {
            let s = TcpStream::connect(addr)?;
            s.set_nodelay(true)?;
            Ok(Controller::Tcp(s))
        } else if let Some(path) = spec.strip_prefix("uart:") {
            // The line must already be in raw mode at the controller's rate
            // (stty -f PATH raw 115200); setting termios here comes with H5.
            Ok(Controller::Uart(std::fs::OpenOptions::new().read(true).write(true).open(path)?))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "controller must be tcp:HOST:PORT or uart:PATH"))
        }
    }
    fn fd(&self) -> RawFd {
        match self {
            Controller::Tcp(s) => s.as_raw_fd(),
            Controller::Uart(f) => f.as_raw_fd(),
        }
    }
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Controller::Tcp(s) => s.read(b),
            Controller::Uart(f) => f.read(b),
        }
    }
    fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Controller::Tcp(s) => s.write_all(b),
            Controller::Uart(f) => f.write_all(b),
        }
    }
}

/// poll(2) on up to two fds, `timeout_ms` (-1 forever); returns which are
/// readable.
fn wait(fds: &[RawFd], timeout_ms: i32) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    loop {
        // SAFETY: `p` is a valid, exclusively borrowed array of pollfd for
        // the duration of the call; its length is passed alongside.
        let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
        if r >= 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            panic!("poll: {}", std::io::Error::last_os_error());
        }
    }
    p.iter().map(|x| x.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0).collect()
}

/// How long to wait for the controller to finish a reset before taking a
/// new host anyway (the new host's own Reset then sorts it out).
const RESET_WAIT_MS: u64 = 3000;

fn main() {
    let mut controller_spec = None;
    let mut listen = None;
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        match k.as_str() {
            "--controller" => controller_spec = a.next(),
            "--host-listen" => listen = a.next(),
            _ => {
                eprintln!("usage: vinculum-hci --controller tcp:HOST:PORT|uart:PATH --host-listen PATH");
                std::process::exit(2);
            }
        }
    }
    let (Some(spec), Some(path)) = (controller_spec, listen) else {
        eprintln!("usage: vinculum-hci --controller tcp:HOST:PORT|uart:PATH --host-listen PATH");
        std::process::exit(2);
    };
    let mut ctl = Controller::open(&spec).unwrap_or_else(|e| {
        eprintln!("vinculum-hci: controller {spec}: {e}");
        std::process::exit(1)
    });
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| {
        eprintln!("vinculum-hci: listen {path}: {e}");
        std::process::exit(1)
    });
    eprintln!("vinculum-hci: controller {spec}, host socket {path}");

    let mut relay = Relay::new();
    let mut host: Option<UnixStream> = None;
    let mut buf = vec![0u8; 8192];
    let mut out = Vec::new();
    let mut reset_since: Option<std::time::Instant> = None;
    loop {
        if relay.resetting && reset_since.is_some_and(|t| t.elapsed().as_millis() as u64 >= RESET_WAIT_MS) {
            eprintln!("vinculum-hci: controller did not complete the reset; taking the next host anyway");
            relay.resetting = false;
        }
        if !relay.resetting {
            reset_since = None;
        }
        let host_fd = host.as_ref().map(|h| h.as_raw_fd()).unwrap_or(listener.as_raw_fd());
        // While the controller resets, a waiting host is not accepted yet.
        let ready = if relay.resetting && host.is_none() {
            let mut r = wait(&[ctl.fd()], 100);
            r.push(false);
            r
        } else {
            wait(&[ctl.fd(), host_fd], -1)
        };
        if ready[0] {
            let n = ctl.read(&mut buf).unwrap_or(0);
            if n == 0 {
                eprintln!("vinculum-hci: controller closed");
                std::process::exit(1);
            }
            if let Err(e) = relay.from_controller(&buf[..n], &mut out) {
                eprintln!("vinculum-hci: controller stream: {e}");
                std::process::exit(1);
            }
        }
        if ready[1] {
            match host.as_mut() {
                None => {
                    if let Ok((s, _)) = listener.accept() {
                        eprintln!("vinculum-hci: host connected");
                        relay.host_reset();
                        host = Some(s);
                    }
                }
                Some(h) => {
                    let n = h.read(&mut buf).unwrap_or(0);
                    let bad = n > 0 && relay.from_host(&buf[..n], &mut out).is_err();
                    if n == 0 || bad {
                        eprintln!("vinculum-hci: host {}; resetting the controller", if bad { "sent a malformed stream" } else { "left" });
                        host = None;
                        relay.host_left(&mut out);
                        reset_since = Some(std::time::Instant::now());
                    }
                }
            }
        }
        for o in out.drain(..) {
            match o {
                Out::ToController(b) => {
                    if let Err(e) = ctl.write_all(&b) {
                        eprintln!("vinculum-hci: controller write: {e}");
                        std::process::exit(1);
                    }
                }
                Out::ToHost(b) => {
                    if let Some(h) = host.as_mut() {
                        if h.write_all(&b).is_err() {
                            host = None;
                        }
                    }
                }
            }
        }
    }
}
