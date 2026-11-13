//! `sigillum-fido`: one FIDO authenticator's driver (sigillum.md §6.1).
//!
//! portcullisd starts one per security key, in its own jail whose devfs
//! shows only that key's `hidraw` node (ruleset 27), as `_sigillum-dev`,
//! with no network, and connects to `sigillumd`'s driver socket itself.
//!
//! ```text
//! sigillum-fido --dev /dev/hidrawN|/dev/u2f/N [--socket-fd 3 | --connect PATH]
//! ```
//!
//! A key that FreeBSD's `u2f(4)` drives is reached through `/dev/u2f/N`:
//! that driver turns off the bus's read-ahead, so the key's `hidraw` node
//! would see only the first report of each answer. `u2f(4)` takes and
//! gives bare 64-byte reports and names the device by `USB_GET_DEVICEINFO`.
//!
//! `--connect` dials `sigillumd`'s driver socket
//! (`/atrium/sockets/sigillum-dev/drivers.sock`), where the broker checks
//! the peer uid is `_sigillum-dev`; `--socket-fd` takes an inherited end.
//!
//! It opens the node, reads the vendor and product, enters Capsicum
//! capability mode (no new files or sockets from then on), says `Hello`,
//! and answers `ToDriver` requests until the socket or the device closes.
//! A `Cancel` from the broker is read on a second thread and reaches the
//! authenticator as `CTAPHID_CANCEL` within one poll (100 ms).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use sigillum_ctap::hid::{HidDevice, REPORT};
use sigillum_ctap::Error;
use sigillum_fido::Driver;

#[repr(C)]
#[derive(Default)]
struct HidrawDevinfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

// hidraw(4)'s Linux-compatible ioctls (FreeBSD sys/dev/hid/hidraw.h); the
// Linux values let the driver run against a key on a development host.
#[cfg(target_os = "freebsd")]
mod ioc {
    pub const HIDIOCGRAWINFO: libc::c_ulong = 0x4008_5520;
    pub const HIDIOCGRAWNAME_128: libc::c_ulong = 0x4080_5521;
}
#[cfg(not(target_os = "freebsd"))]
mod ioc {
    pub const HIDIOCGRAWINFO: libc::c_ulong = 0x8008_4803;
    pub const HIDIOCGRAWNAME_128: libc::c_ulong = 0x8080_4804;
}

/// A hidraw or u2f node. hidraw: reports are written with a leading report
/// id 0 (FIDO descriptors use no ids; hidraw strips it). u2f(4): written
/// bare, exactly 64 bytes. Both read as 64 bytes.
struct Hidraw(File, Kind);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Hidraw,
    U2f,
}

impl HidDevice for Hidraw {
    fn write_report(&mut self, report: &[u8; REPORT]) -> Result<(), Error> {
        let r = match self.1 {
            Kind::Hidraw => {
                let mut b = [0u8; REPORT + 1];
                b[1..].copy_from_slice(report);
                self.0.write_all(&b)
            }
            Kind::U2f => self.0.write_all(report),
        };
        r.map_err(|e| Error::Io(e.to_string()))
    }

    fn read_report(&mut self, timeout_ms: u32) -> Result<[u8; REPORT], Error> {
        let mut p = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut p, 1, timeout_ms as libc::c_int) };
        if r < 0 {
            return Err(Error::Io(std::io::Error::last_os_error().to_string()));
        }
        if r == 0 {
            return Err(Error::Timeout);
        }
        if p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && p.revents & libc::POLLIN == 0 {
            return Err(Error::Io("device gone".into()));
        }
        let mut b = [0u8; REPORT];
        match self.0.read(&mut b) {
            Ok(0) => Err(Error::Io("device gone".into())),
            Ok(_) => Ok(b), // a short report is zero-padded, as CTAPHID allows
            Err(e) => Err(Error::Io(e.to_string())),
        }
    }
}

/// `struct usb_device_info` (sys/dev/usb/usb_ioctl.h), 368 bytes.
#[repr(C)]
struct UsbDeviceInfo {
    product_no: u16,
    vendor_no: u16,
    rest: [u8; 36],
    product: [u8; 128],
    vendor: [u8; 128],
    serial: [u8; 64],
    release: [u8; 8],
}

/// `_IOR('U', 112, struct usb_device_info)`.
#[cfg(target_os = "freebsd")]
const USB_GET_DEVICEINFO: libc::c_ulong = 0x4170_5570;

fn printable(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s: String = String::from_utf8_lossy(&raw[..end]).trim().chars().filter(|c| !c.is_control()).take(32).collect();
    (!s.is_empty()).then_some(s)
}

#[cfg(target_os = "freebsd")]
fn info_u2f(f: &File) -> (u16, u16, String) {
    let mut i: UsbDeviceInfo = unsafe { std::mem::zeroed() };
    // SAFETY: the kernel fills a struct usb_device_info, which this mirrors.
    if unsafe { libc::ioctl(f.as_raw_fd(), USB_GET_DEVICEINFO as _, &mut i as *mut UsbDeviceInfo) } < 0 {
        return (0, 0, String::from("Security key"));
    }
    let name = printable(&i.product).unwrap_or_else(|| String::from("Security key"));
    (i.vendor_no, i.product_no, name)
}

#[cfg(not(target_os = "freebsd"))]
fn info_u2f(_: &File) -> (u16, u16, String) {
    (0, 0, String::from("Security key"))
}

const _: () = assert!(std::mem::size_of::<UsbDeviceInfo>() == 368);

fn info(f: &File) -> (u16, u16, String) {
    let fd = f.as_raw_fd();
    let mut i = HidrawDevinfo::default();
    let ok = unsafe { libc::ioctl(fd, ioc::HIDIOCGRAWINFO as _, &mut i as *mut HidrawDevinfo) } >= 0;
    let mut name = [0u8; 128];
    let n = unsafe { libc::ioctl(fd, ioc::HIDIOCGRAWNAME_128 as _, name.as_mut_ptr()) };
    let name = (n > 0).then(|| printable(&name)).flatten().unwrap_or_else(|| String::from("Security key"));
    if ok {
        (i.vendor as u16, i.product as u16, name)
    } else {
        (0, 0, name)
    }
}

#[cfg(target_os = "freebsd")]
fn capsicum() -> std::io::Result<()> {
    extern "C" {
        fn cap_enter() -> libc::c_int;
    }
    if unsafe { cap_enter() } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn capsicum() -> std::io::Result<()> {
    eprintln!("sigillum-fido: no Capsicum on this OS (development only)");
    Ok(())
}

fn main() -> ExitCode {
    let mut dev = None;
    let mut sock_fd: i32 = 3;
    let mut connect: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dev" => dev = args.next(),
            "--connect" => connect = args.next(),
            "--socket-fd" => sock_fd = args.next().and_then(|s| s.parse().ok()).unwrap_or(-1),
            _ => {
                eprintln!("usage: sigillum-fido --dev /dev/hidrawN|/dev/u2f/N [--socket-fd N | --connect PATH]");
                return ExitCode::from(2);
            }
        }
    }
    let Some(dev) = dev else {
        eprintln!("sigillum-fido: --dev is required");
        return ExitCode::from(2);
    };
    let kind = match () {
        _ if dev.starts_with("/dev/hidraw") => Kind::Hidraw,
        _ if dev.starts_with("/dev/u2f/") => Kind::U2f,
        _ => {
            eprintln!("sigillum-fido: --dev must be /dev/hidrawN or /dev/u2f/N");
            return ExitCode::from(2);
        }
    };
    if sock_fd < 0 {
        eprintln!("sigillum-fido: bad arguments");
        return ExitCode::from(2);
    }
    let file = match OpenOptions::new().read(true).write(true).open(&dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("sigillum-fido: {dev}: {e}");
            return ExitCode::from(1);
        }
    };
    let (vendor, product, name) = if kind == Kind::U2f { info_u2f(&file) } else { info(&file) };
    let sock = match connect {
        // Before Capsicum: connecting is a namespace operation.
        Some(path) => match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sigillum-fido: connect {path}: {e}");
                return ExitCode::from(1);
            }
        },
        // SAFETY: the launcher hands us this fd and nothing else owns it.
        None => unsafe { UnixStream::from_raw_fd(sock_fd) },
    };
    if let Err(e) = capsicum() {
        eprintln!("sigillum-fido: cap_enter: {e}");
        return ExitCode::from(1);
    }
    let (driver, hello) = match Driver::open(Hidraw(file, kind), vendor, product, &name) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("sigillum-fido: {dev}: {e}");
            return ExitCode::from(1);
        }
    };
    match sigillum_fido::serve(driver, hello, sock) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sigillum-fido: {dev}: socket: {e}");
            ExitCode::from(1)
        }
    }
}
