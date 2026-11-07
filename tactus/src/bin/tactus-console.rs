//! tactus-console — hands the PS/2 keyboard between the console and
//! tactusd (docs/spec/atrium-input.md §3.2). Runs as root, unjailed, and
//! does nothing else.
//!
//! At boot the console's keyboard multiplexer (kbdmux) holds `atkbd0`, so
//! tactusd cannot open `/dev/atkbd0`. While tactusd is up this releases
//! atkbd0 from kbdmux (`KBRELKBD`, what `kbdcontrol -A atkbd0` does) and
//! tactusd opens it in `K_RAW`; when tactusd goes away, its fd closes and
//! this gives atkbd0 back to kbdmux (`KBADDKBD`), so the console always
//! has the built-in keyboard when Tactus is not running.
//!
//! "tactusd is up" is a connection to its socket (root is an allowed
//! peer). The events that arrive on it are read and dropped, never kept
//! or logged.
//!
//! Usage: tactus-console [--socket PATH] [--tty /dev/ttyv0] [--keyboard atkbd0]

use std::os::unix::net::UnixStream;
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/atrium/sockets/tactus/tactus.sock";

#[cfg(target_os = "freebsd")]
mod mux {
    use std::os::unix::io::AsRawFd;

    // sys/kbio.h: _IOW('K', 68/69, keyboard_info_t), 36 bytes.
    const KBADDKBD: libc::c_ulong = 0x8024_4b44;
    const KBRELKBD: libc::c_ulong = 0x8024_4b45;

    #[repr(C)]
    struct KeyboardInfo {
        kb_index: libc::c_int,
        kb_name: [u8; 16],
        kb_unit: libc::c_int,
        kb_type: libc::c_int,
        kb_config: libc::c_int,
        kb_flags: libc::c_int,
    }

    /// `atkbd0` into (true) or out of (false) the console's kbdmux, through
    /// vt's ioctl on `tty`.
    pub fn set(tty: &str, keyboard: &str, add: bool) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let split = keyboard.find(|c: char| c.is_ascii_digit()).unwrap_or(keyboard.len());
        let (name, unit) = keyboard.split_at(split);
        let mut ki = KeyboardInfo { kb_index: 0, kb_name: [0; 16], kb_unit: unit.parse().unwrap_or(0), kb_type: 0, kb_config: 0, kb_flags: 0 };
        let n = name.len().min(15);
        ki.kb_name[..n].copy_from_slice(&name.as_bytes()[..n]);
        let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(tty)?;
        let req = if add { KBADDKBD } else { KBRELKBD };
        // SAFETY: ki is a valid keyboard_info_t for the call.
        if unsafe { libc::ioctl(f.as_raw_fd(), req, &mut ki as *mut KeyboardInfo) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(target_os = "freebsd"))]
mod mux {
    pub fn set(_tty: &str, _keyboard: &str, _add: bool) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "kbdmux is FreeBSD's"))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str, d: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| d.into());
    let sock = arg("--socket", DEFAULT_SOCKET);
    let tty = arg("--tty", "/dev/ttyv0");
    let kbd = arg("--keyboard", "atkbd0");
    let mut said = false;
    // The console has the keyboard until tactusd is seen.
    let _ = mux::set(&tty, &kbd, true);
    loop {
        if let Ok(mut c) = UnixStream::connect(&sock) {
            if tactus::wire::read(&mut c).is_ok() {
                match mux::set(&tty, &kbd, false) {
                    Ok(()) => eprintln!("tactus-console: {kbd} to tactusd"),
                    Err(e) => eprintln!("tactus-console: release {kbd} from kbdmux: {e}"),
                }
                said = false;
                // Drop everything until tactusd goes away.
                while tactus::wire::read(&mut c).is_ok() {}
                match mux::set(&tty, &kbd, true) {
                    Ok(()) => eprintln!("tactus-console: tactusd gone; {kbd} back to the console"),
                    Err(e) => eprintln!("tactus-console: add {kbd} to kbdmux: {e}"),
                }
            }
        } else if !said {
            eprintln!("tactus-console: waiting for tactusd at {sock}");
            said = true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
