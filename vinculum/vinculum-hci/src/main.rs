//! vinculum-hci: relays one controller to one host (docs/spec/bluetooth.md §5).
//!
//! ```text
//! vinculum-hci --controller tcp:HOST:PORT --host-listen PATH
//! ```
//!
//! Transports: `tcp:` (H4 over TCP, Rootcanal), `uart:PATH` (H4 on a serial
//! line, raw mode) and `usb:` / `usb:VVVV:PPPP` (the first Bluetooth
//! controller on USB, or the one with those ids, through the libusb-1.0 API;
//! see `usb_dev.rs`). A Realtek controller gets its patch from `--firmware-dir` first. The host side is
//! a Unix socket the host connects to (in production portcullisd passes a
//! socketpair instead). When the host disconnects, the controller is reset,
//! so no link outlives the host that knows its keys (§4).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use vinculum_hci::relay::{Out, Relay};

#[cfg(feature = "usb")]
mod usb_dev;

enum Controller {
    Tcp(TcpStream),
    Uart(std::fs::File),
    /// The H4 end of the USB threads' socketpair.
    #[cfg_attr(not(feature = "usb"), allow(dead_code))]
    Usb(UnixStream),
}

impl Controller {
    fn open(spec: &str, firmware_dir: &std::path::Path) -> std::io::Result<Controller> {
        if let Some(addr) = spec.strip_prefix("tcp:") {
            let s = TcpStream::connect(addr)?;
            s.set_nodelay(true)?;
            Ok(Controller::Tcp(s))
        } else if let Some(path) = spec.strip_prefix("uart:") {
            // The line must already be in raw mode at the controller's rate
            // (stty -f PATH raw 115200); setting termios here comes with H5.
            Ok(Controller::Uart(std::fs::OpenOptions::new().read(true).write(true).open(path)?))
        } else if let Some(rest) = spec.strip_prefix("usb") {
            Controller::open_usb(rest.strip_prefix(':').unwrap_or(rest), firmware_dir)
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "controller must be tcp:HOST:PORT, uart:PATH or usb[:VVVV:PPPP]"))
        }
    }
    #[cfg(feature = "usb")]
    fn open_usb(rest: &str, firmware_dir: &std::path::Path) -> std::io::Result<Controller> {
        let sel = vinculum_hci::usb::parse_select(rest)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "usb controller must be usb or usb:VVVV:PPPP (hex ids)"))?;
        let (s, name) = usb_dev::start(sel, firmware_dir)?;
        eprintln!("vinculum-hci: USB controller {name}");
        Ok(Controller::Usb(s))
    }
    #[cfg(not(feature = "usb"))]
    fn open_usb(_rest: &str, _firmware_dir: &std::path::Path) -> std::io::Result<Controller> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "built without the usb feature"))
    }
    fn fd(&self) -> RawFd {
        match self {
            Controller::Tcp(s) => s.as_raw_fd(),
            Controller::Uart(f) => f.as_raw_fd(),
            Controller::Usb(s) => s.as_raw_fd(),
        }
    }
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Controller::Tcp(s) => s.read(b),
            Controller::Uart(f) => f.read(b),
            Controller::Usb(s) => s.read(b),
        }
    }
    fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Controller::Tcp(s) => s.write_all(b),
            Controller::Uart(f) => f.write_all(b),
            Controller::Usb(s) => s.write_all(b),
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

/// Where vendor firmware files live (spec §5.2), read-only in the jail.
const DEFAULT_FIRMWARE_DIR: &str = "/usr/local/share/atrium/firmware/bluetooth";

/// How long to wait for the controller to finish a reset before taking a
/// new host anyway (the new host's own Reset then sorts it out).
const RESET_WAIT_MS: u64 = 3000;

/// Development bridge (cargo feature `dev-bridge`, never in an installed
/// build: any local uid reaching it drives the raw controller): serves the raw controller as H4 over TCP on a
/// loopback address, so a stack elsewhere (vinculum-hci in a VM through
/// QEMU's user network, which reaches the host's loopback as 10.0.2.2, or
/// vinculum-probe on the host) can use a dongle plugged into this machine
/// without USB passthrough. No allowlist here: the far side's vinculum-hci
/// applies it. One client at a time; the controller is reset when it leaves.
#[cfg(feature = "dev-bridge")]
fn bridge(spec: &str, addr: &str, firmware_dir: &std::path::Path) -> ! {
    let die = |m: String| -> ! {
        eprintln!("vinculum-hci: {m}");
        std::process::exit(1)
    };
    let sa: std::net::SocketAddr = addr.parse().unwrap_or_else(|_| die(format!("--serve-tcp {addr}: not IP:PORT")));
    if !sa.ip().is_loopback() {
        die(format!("--serve-tcp {addr}: only a loopback address may serve a controller"));
    }
    let mut ctl = Controller::open(spec, firmware_dir).unwrap_or_else(|e| die(format!("controller {spec}: {e}")));
    let listener = std::net::TcpListener::bind(sa).unwrap_or_else(|e| die(format!("listen {addr}: {e}")));
    eprintln!("vinculum-hci: serving controller {spec} as H4 on {addr}");
    let mut buf = vec![0u8; 8192];
    let mut client: Option<TcpStream> = None;
    // Only whole frames reach the controller, so a client leaving mid-frame
    // cannot leave half a command in front of the Reset.
    let mut from_client = vinculum_hci_proto::h4::Decoder::new();
    loop {
        let other = client.as_ref().map(|c| c.as_raw_fd()).unwrap_or(listener.as_raw_fd());
        let ready = wait(&[ctl.fd(), other], -1);
        if ready[0] {
            let n = ctl.read(&mut buf).unwrap_or(0);
            if n == 0 {
                die("controller closed".into());
            }
            if let Some(c) = client.as_mut() {
                if c.write_all(&buf[..n]).is_err() {
                    client = None;
                }
            }
        }
        if ready[1] {
            match client.as_mut() {
                None => {
                    if let Ok((s, peer)) = listener.accept() {
                        let _ = s.set_nodelay(true);
                        eprintln!("vinculum-hci: bridge client {peer}");
                        from_client.reset();
                        client = Some(s);
                    }
                }
                Some(c) => {
                    let n = c.read(&mut buf).unwrap_or(0);
                    let mut frames = Vec::new();
                    let bad = n > 0 && from_client.push(&buf[..n], |f| frames.push(f)).is_err();
                    for f in frames {
                        if let Err(e) = ctl.write_all(&f.to_bytes()) {
                            die(format!("controller write: {e}"));
                        }
                    }
                    if n == 0 || bad {
                        eprintln!("vinculum-hci: bridge client {}; resetting the controller", if bad { "sent a malformed stream" } else { "left" });
                        client = None;
                        if let Err(e) = ctl.write_all(&vinculum_hci::relay::HCI_RESET) {
                            die(format!("controller write: {e}"));
                        }
                    }
                }
            }
        }
    }
}

enum HostSide {
    Listen(String),
    /// Connect to vinculumd's transport socket (how dispensatord's driver
    /// jail runs it: its mounts are read-only, so it cannot bind).
    Connect(String),
}

/// `--report [--controller usb[:VVVV:PPPP]]`: describes each USB controller
/// on stdout and exits (vinculum_hci::report).
fn run_report(spec: &str) -> ! {
    #[cfg(feature = "usb")]
    {
        let Some(sel) = spec.strip_prefix("usb").and_then(|r| vinculum_hci::usb::parse_select(r.strip_prefix(':').unwrap_or(r))) else {
            eprintln!("vinculum-hci: --report reads USB controllers: --controller usb or usb:VVVV:PPPP");
            std::process::exit(2)
        };
        let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
        match usb_dev::report(sel) {
            Ok(r) => {
                print!("{}{r}", vinculum_hci::report::header(&os));
                std::process::exit(0)
            }
            Err(e) => {
                eprintln!("vinculum-hci: report: {e}");
                std::process::exit(1)
            }
        }
    }
    #[cfg(not(feature = "usb"))]
    {
        let _ = spec;
        eprintln!("vinculum-hci: --report needs the usb feature");
        std::process::exit(2)
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: vinculum-hci --controller tcp:HOST:PORT|uart:PATH|usb[:VVVV:PPPP] \
         (--host-listen PATH | --host-connect PATH | --serve-tcp 127.0.0.1:PORT) [--host-uid UID]... [--firmware-dir DIR]\n       vinculum-hci --report [--controller usb[:VVVV:PPPP]]"
    );
    std::process::exit(2)
}

/// The host on `s` is one we serve: its uid is in `uids` (vinculumd's,
/// `_vinculum`), or with none given, our own. A refused peer is logged and
/// closed.
fn peer_allowed(s: &UnixStream, uids: &[u32]) -> bool {
    let uid = peer_uid(s);
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    let own = unsafe { libc::getuid() };
    if uid.is_some_and(|u| if uids.is_empty() { u == own } else { uids.contains(&u) }) {
        return true;
    }
    eprintln!("vinculum-hci: refused a host (uid {uid:?})");
    false
}

#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given; the
    // fd is an open socket owned by `s`.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    (r == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: valid out-pointers; the fd is an open socket owned by `s`.
    let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
    (r == 0).then_some(uid)
}

fn main() {
    let mut controller_spec = None;
    let mut listen = None;
    let mut connect: Option<String> = None;
    let mut host_uids: Vec<u32> = Vec::new();
    let mut serve_tcp: Option<String> = None;
    let mut report = false;
    let mut firmware_dir = std::path::PathBuf::from(DEFAULT_FIRMWARE_DIR);
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        match k.as_str() {
            "--controller" => controller_spec = a.next(),
            "--host-listen" => listen = a.next(),
            "--host-connect" => connect = a.next(),
            "--host-uid" => match a.next().and_then(|v| v.parse().ok()) {
                Some(u) => host_uids.push(u),
                None => usage(),
            },
            "--serve-tcp" => serve_tcp = a.next(),
            "--report" => report = true,
            "--firmware-dir" => firmware_dir = a.next().map(Into::into).unwrap_or(firmware_dir),
            _ => usage(),
        }
    }
    if report {
        run_report(controller_spec.as_deref().unwrap_or("usb"));
    }
    if let (Some(_spec), Some(_addr)) = (controller_spec.as_deref(), serve_tcp.as_deref()) {
        #[cfg(feature = "dev-bridge")]
        bridge(_spec, _addr, &firmware_dir);
        #[cfg(not(feature = "dev-bridge"))]
        {
            eprintln!("vinculum-hci: --serve-tcp needs a development build (cargo feature dev-bridge)");
            std::process::exit(2)
        }
    }
    let Some(spec) = controller_spec else { usage() };
    let side = match (listen, connect) {
        (Some(p), None) => HostSide::Listen(p),
        (None, Some(p)) => HostSide::Connect(p),
        _ => usage(),
    };
    let mut ctl = Controller::open(&spec, &firmware_dir).unwrap_or_else(|e| {
        eprintln!("vinculum-hci: controller {spec}: {e}");
        std::process::exit(1)
    });
    let listener = match &side {
        HostSide::Listen(path) => {
            let _ = std::fs::remove_file(path);
            let l = UnixListener::bind(path).unwrap_or_else(|e| {
                eprintln!("vinculum-hci: listen {path}: {e}");
                std::process::exit(1)
            });
            eprintln!("vinculum-hci: controller {spec}, host socket {path}");
            Some(l)
        }
        HostSide::Connect(path) => {
            eprintln!("vinculum-hci: controller {spec}, host at {path}");
            None
        }
    };

    let mut relay = Relay::new();
    let mut host: Option<UnixStream> = None;
    let mut buf = vec![0u8; 8192];
    let mut out = Vec::new();
    let mut reset_since: Option<std::time::Instant> = None;
    let mut last_connect: Option<std::time::Instant> = None;
    // The last connect failure logged, so a refusal that is not "host not
    // up yet" (EACCES on the socket, say) is said once, not hidden.
    let mut last_connect_err: Option<std::io::ErrorKind> = None;
    loop {
        if relay.resetting && reset_since.is_some_and(|t| t.elapsed().as_millis() as u64 >= RESET_WAIT_MS) {
            eprintln!("vinculum-hci: controller did not complete the reset; taking the next host anyway");
            relay.resetting = false;
        }
        if !relay.resetting {
            reset_since = None;
        }
        // Connect mode: (re)connect to the host once the controller is clean,
        // at most once a second, so a host that refuses us (or keeps
        // leaving) is not hammered.
        if let (HostSide::Connect(path), None, false) = (&side, host.as_ref(), relay.resetting) {
            if last_connect.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) {
                last_connect = Some(std::time::Instant::now());
                let c = UnixStream::connect(path);
                if let Err(e) = &c {
                    let quiet = matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused);
                    if !quiet && last_connect_err != Some(e.kind()) {
                        eprintln!("vinculum-hci: connect to the host at {path}: {e}; retrying");
                    }
                    last_connect_err = Some(e.kind());
                } else {
                    last_connect_err = None;
                }
                if let Ok(s) = c {
                    if peer_allowed(&s, &host_uids) {
                        eprintln!("vinculum-hci: connected to the host");
                        relay.host_reset();
                        host = Some(s);
                    }
                }
            }
        }
        let host_fd = host.as_ref().map(|h| h.as_raw_fd()).or(listener.as_ref().map(|l| l.as_raw_fd()));
        // While the controller resets, a waiting host is not accepted yet;
        // in connect mode, with no host, retry the connect every second.
        let ready = match host_fd {
            Some(fd) if !(relay.resetting && host.is_none()) => wait(&[ctl.fd(), fd], -1),
            _ => {
                let mut r = wait(&[ctl.fd()], if relay.resetting { 100 } else { 1000 });
                r.push(false);
                r
            }
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
                    if let Some(Ok((s, _))) = listener.as_ref().map(|l| l.accept()) {
                        if peer_allowed(&s, &host_uids) {
                            eprintln!("vinculum-hci: host connected");
                            relay.host_reset();
                            host = Some(s);
                        }
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
