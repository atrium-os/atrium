//! `sigillum-fido`: one FIDO authenticator's driver (sigillum.md §6.1).
//!
//! portcullisd starts one per security key, in its own jail whose devfs
//! shows only that key's `hidraw` node (ruleset 27), as `_sigillum-dev`,
//! with no network. It inherits one end of a socketpair to `sigillumd`.
//!
//! ```text
//! sigillum-fido --dev /dev/hidrawN [--socket-fd 3]
//! ```
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
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use sigillum_ctap::hid::{HidDevice, REPORT};
use sigillum_ctap::Error;
use sigillum_fido::Driver;
use sigillum_proto::driver::{FromDriver, ToDriver, MAX_MESSAGE};
use sigillum_proto::{read_msg, write_msg};

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

/// A hidraw node: reports are written with a leading report id 0 (FIDO
/// descriptors use no ids; hidraw strips it) and read as 64 bytes.
struct Hidraw(File);

impl HidDevice for Hidraw {
    fn write_report(&mut self, report: &[u8; REPORT]) -> Result<(), Error> {
        let mut b = [0u8; REPORT + 1];
        b[1..].copy_from_slice(report);
        self.0.write_all(&b).map_err(|e| Error::Io(e.to_string()))
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

fn info(f: &File) -> (u16, u16, String) {
    let fd = f.as_raw_fd();
    let mut i = HidrawDevinfo::default();
    let ok = unsafe { libc::ioctl(fd, ioc::HIDIOCGRAWINFO as _, &mut i as *mut HidrawDevinfo) } >= 0;
    let mut name = [0u8; 128];
    let n = unsafe { libc::ioctl(fd, ioc::HIDIOCGRAWNAME_128 as _, name.as_mut_ptr()) };
    let name = if n > 0 {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        String::from_utf8_lossy(&name[..end]).trim().chars().filter(|c| !c.is_control()).take(32).collect()
    } else {
        String::from("Security key")
    };
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dev" => dev = args.next(),
            "--socket-fd" => sock_fd = args.next().and_then(|s| s.parse().ok()).unwrap_or(-1),
            _ => {
                eprintln!("usage: sigillum-fido --dev /dev/hidrawN [--socket-fd N]");
                return ExitCode::from(2);
            }
        }
    }
    let Some(dev) = dev else {
        eprintln!("sigillum-fido: --dev is required");
        return ExitCode::from(2);
    };
    if !dev.starts_with("/dev/hidraw") || sock_fd < 0 {
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
    let (vendor, product, name) = info(&file);
    // SAFETY: the launcher hands us this fd and nothing else owns it.
    let sock = unsafe { UnixStream::from_raw_fd(sock_fd) };
    if let Err(e) = capsicum() {
        eprintln!("sigillum-fido: cap_enter: {e}");
        return ExitCode::from(1);
    }
    let (mut driver, hello) = match Driver::open(Hidraw(file), vendor, product, &name) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("sigillum-fido: {dev}: {e}");
            return ExitCode::from(1);
        }
    };
    let mut out = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sigillum-fido: {e}");
            return ExitCode::from(1);
        }
    };
    if write_msg(&mut out, &hello, MAX_MESSAGE).is_err() {
        return ExitCode::from(1);
    }

    // The reader thread: cancels take effect at once, work goes to main.
    let cancel = driver.cancel_flag();
    let (tx, rx) = mpsc::channel::<ToDriver>();
    let mut input = sock;
    std::thread::spawn(move || loop {
        match read_msg::<ToDriver>(&mut input, MAX_MESSAGE) {
            Ok(Some(ToDriver::Cancel { .. })) => cancel.store(true, Ordering::SeqCst),
            Ok(Some(m)) => {
                if tx.send(m).is_err() {
                    return;
                }
            }
            _ => return, // EOF or a malformed message: the broker is gone
        }
    });

    for req in rx {
        let Some(rec) = driver.handle(req) else { continue };
        let gone = matches!(rec, FromDriver::Error { error: sigillum_proto::driver::DriverError::Gone, .. });
        if write_msg(&mut out, &rec, MAX_MESSAGE).is_err() || gone {
            break;
        }
    }
    ExitCode::SUCCESS
}
