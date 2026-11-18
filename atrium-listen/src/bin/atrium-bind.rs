//! atrium-bind: bind a host daemon's listening socket, then exec the daemon
//! with it (docs/spec/fast-boot.md §4.2, §6.4).
//!
//!   atrium-bind --path SOCK [--mode 0600] [--owner UID] [--group GID] -- DAEMON ARGS...
//!
//! The socket ends up at fd 3 with ATRIUM_LISTEN_FDS=1 and
//! ATRIUM_LISTEN_PATHS=SOCK, the same hand-over the supervisor uses for
//! jailed services, so the daemon takes it with `atrium_listen::listener`.
//! An rc script starts the daemon through this, so the socket exists (and
//! clients' connects queue) from the moment rc runs the script, not from
//! when the daemon has finished starting. If the bind fails, the daemon is
//! started anyway and binds the socket itself.

use std::ffi::OsString;
use std::os::fd::IntoRawFd;
use std::os::unix::process::CommandExt;

fn usage() -> ! {
    eprintln!("usage: atrium-bind --path SOCK [--mode 0600] [--owner UID] [--group GID] -- DAEMON ARGS...");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (mut path, mut mode, mut owner, mut group) = (None::<OsString>, 0o600u32, None::<u32>, None::<u32>);
    let num = |v: Option<OsString>| v.and_then(|v| v.into_string().ok()).unwrap_or_else(|| usage());
    loop {
        match args.next().as_deref().and_then(|a| a.to_str()) {
            Some("--path") => path = Some(args.next().unwrap_or_else(|| usage())),
            Some("--mode") => mode = u32::from_str_radix(num(args.next()).trim_start_matches("0o"), 8).unwrap_or_else(|_| usage()),
            Some("--owner") => owner = Some(num(args.next()).parse().unwrap_or_else(|_| usage())),
            Some("--group") => group = Some(num(args.next()).parse().unwrap_or_else(|_| usage())),
            Some("--") => break,
            _ => usage(),
        }
    }
    let path = path.unwrap_or_else(|| usage());
    let prog = args.next().unwrap_or_else(|| usage());
    let rest: Vec<OsString> = args.collect();

    let mut cmd = std::process::Command::new(&prog);
    cmd.args(&rest);
    match atrium_listen::bind_socket(std::path::Path::new(&path), mode, owner, group) {
        Ok(l) => {
            let fd = l.into_raw_fd();
            // SAFETY: dup2 onto fd 3 (which clears close-on-exec there), then
            // close the original if it was another slot.
            #[allow(unsafe_code)]
            unsafe {
                if fd != 3 {
                    if libc::dup2(fd, 3) < 0 { fail(&prog, "dup2"); }
                    libc::close(fd);
                } else {
                    let fl = libc::fcntl(3, libc::F_GETFD);
                    libc::fcntl(3, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                }
            }
            cmd.env("ATRIUM_LISTEN_FDS", "1").env("ATRIUM_LISTEN_PATHS", &path);
        }
        Err(e) => eprintln!("atrium-bind: {}: {e}; {} will bind it itself", path.to_string_lossy(), prog.to_string_lossy()),
    }
    let e = cmd.exec();
    eprintln!("atrium-bind: exec {}: {e}", prog.to_string_lossy());
    std::process::exit(127)
}

fn fail(prog: &OsString, what: &str) -> ! {
    eprintln!("atrium-bind: {what}: {} (for {})", std::io::Error::last_os_error(), prog.to_string_lossy());
    std::process::exit(126)
}
