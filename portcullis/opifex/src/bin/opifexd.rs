//! opifexd, the installer (docs/spec/opifex.md §7; build step C2).
//!
//! Listens on `/atrium/sockets/opifex/opifex.sock` (0666: anyone may ask the
//! read-only questions). One JSON request per line, one answer per line; the
//! caller's uid decides what it may ask (proto.rs). Runs as root, because it
//! writes `/var/lib/atrium/apps`; it reads no archive itself (opifex-check
//! does, sandboxed) and has no network.
//!
//! Settings: `OPIFEX_SOCKET`, `OPIFEX_ROOT` (/var/lib/atrium), `OPIFEX_STATE`
//! (/var/db/atrium/opifex), `OPIFEX_ROOT_TOML` (/etc/atrium/opifex/root.toml),
//! `OPIFEX_CHECK` (/usr/local/libexec/opifex-check), `OPIFEX_SHEET_UID`
//! (65042, the install sheet's uid, which may send `InstallApproved`).

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opifex::daemon::Daemon;
use opifex::proto::{Request, Response, MAX_LINE, SOCKET};

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid on a connected socket we own, into two locals.
    (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a ucred we own, its size given.
    (unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut n) } == 0).then_some(c.uid)
}

fn env(k: &str, d: &str) -> PathBuf { std::env::var_os(k).map(PathBuf::from).unwrap_or_else(|| d.into()) }

fn serve(d: &Daemon, mut s: UnixStream) {
    let Some(uid) = peer_uid(&s) else { return };
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut line = String::new();
    let mut r = BufReader::new(s.try_clone().expect("clone")).take(MAX_LINE as u64);
    if r.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let answer = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => {
            let what = format!("{req:?}");
            let a = d.handle(uid, req);
            eprintln!("opifexd: uid {uid}: {} -> {}", what.split([' ', '{']).next().unwrap_or(""),
                      match &a { Response::Error { message } => format!("error: {message}"), _ => "ok".into() });
            a
        }
        Err(e) => Response::Error { message: format!("bad request: {e}") },
    };
    let mut out = serde_json::to_string(&answer).unwrap_or_else(|_| "{}".into());
    out.push('\n');
    let _ = s.write_all(out.as_bytes());
}

use std::io::Read;

fn main() {
    let sock = env("OPIFEX_SOCKET", SOCKET);
    let d = Arc::new(Daemon {
        root: env("OPIFEX_ROOT", "/var/lib/atrium"),
        state: env("OPIFEX_STATE", "/var/db/atrium/opifex"),
        root_toml: env("OPIFEX_ROOT_TOML", "/etc/atrium/opifex/root.toml"),
        check_bin: env("OPIFEX_CHECK", "/usr/local/libexec/opifex-check"),
        sandbox: true,
        worker_uid: 65041,
        // The install sheet (_opifexsheet); OPIFEX_SHEET_UID=none turns it off.
        sheet_uid: match std::env::var("OPIFEX_SHEET_UID").ok().as_deref() {
            Some("none") => None,
            Some(u) => u.parse().ok(),
            None => Some(65042),
        },
        trust: portcullis_trust::Trust::load(),
        require_content: opifex::app::require_content(std::path::Path::new(portcullis_trust::TRUST_CONFIG)),
        sysroot: PathBuf::from("/"),
        lock: Mutex::new(()),
    });
    if let Some(dir) = sock.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&sock);
    let l = UnixListener::bind(&sock).unwrap_or_else(|e| {
        eprintln!("opifexd: bind {}: {e}", sock.display());
        std::process::exit(1)
    });
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666));
    eprintln!("opifexd: listening on {}", sock.display());
    for s in l.incoming().flatten() {
        let d = Arc::clone(&d);
        std::thread::spawn(move || serve(&d, s));
    }
}
