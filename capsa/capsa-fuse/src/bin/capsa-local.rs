//! atrium-capsa-local: serve a trusted (sealed) stick's folders to apps.
//!
//! ```text
//! atrium-capsa-local --root-fd 3 --fuse-fd 4 --control-fd 5 [--ro] [--umask OCTAL]
//! atrium-capsa-local --root PATH --fuse-fd N ...      (testing)
//! ```
//!
//! The kernel mounts a sealed stick as its person's (tessera-encryption.md
//! §7.2a). In Atrium jaild opens that mount's root and `/dev/fuse`, mounts
//! fusefs out of the way (`/media/.sticks/<part>`), and starts this in a
//! jail as the person's uid with only those descriptors and dispensatord's
//! control socket, over which grants of folders to apps arrive
//! (device-assignment.md §4.8). It enters capability mode before serving.

use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use capsa_fuse::local::LocalDir;
use capsa_fuse::server::serve_with;
use capsa_fuse::Options;

fn usage() -> ExitCode {
    eprintln!("usage: atrium-capsa-local (--root PATH | --root-fd N) --fuse-fd N [--control-fd N] [--ro] [--umask OCTAL]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut root: Option<OwnedFd> = None;
    let mut fuse: Option<File> = None;
    let mut control: Option<UnixStream> = None;
    let mut opts = Options::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next();
        let fd = |v: Option<String>| v.and_then(|s| s.parse::<i32>().ok()).filter(|&n| n > 2);
        match a.as_str() {
            // SAFETY: the caller handed us this descriptor and nothing else owns it.
            "--root-fd" => match fd(val()) {
                Some(n) => root = Some(unsafe { OwnedFd::from_raw_fd(n) }),
                None => return usage(),
            },
            "--root" => match val().map(|p| File::open(&p)) {
                Some(Ok(f)) => root = Some(f.into()),
                _ => return usage(),
            },
            // SAFETY: as above.
            "--fuse-fd" => match fd(val()) {
                Some(n) => fuse = Some(unsafe { File::from_raw_fd(n) }),
                None => return usage(),
            },
            // SAFETY: as above; jaild checked it is a socket.
            "--control-fd" => match fd(val()) {
                Some(n) => control = Some(unsafe { UnixStream::from_raw_fd(n) }),
                None => return usage(),
            },
            "--ro" => opts.read_only = true,
            "--umask" => match val().and_then(|s| u32::from_str_radix(&s, 8).ok()).filter(|m| *m <= 0o777) {
                Some(m) => opts.umask = m,
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let (Some(root), Some(fuse)) = (root, fuse) else { return usage() };
    // The files are the person's, as the kernel keeps them: shown as ours.
    // SAFETY: getuid and getgid cannot fail.
    (opts.uid, opts.gid) = unsafe { (libc::getuid(), libc::getgid()) };
    #[cfg(target_os = "freebsd")]
    // SAFETY: cap_enter takes no arguments and only restricts this process.
    if unsafe { libc::cap_enter() } != 0 {
        eprintln!("atrium-capsa-local: cap_enter: {}", std::io::Error::last_os_error());
        return ExitCode::FAILURE;
    }
    let fs = match LocalDir::new(root, opts.read_only) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("atrium-capsa-local: the folder: {e}");
            return ExitCode::FAILURE;
        }
    };
    match serve_with(fs, fuse, opts, control) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atrium-capsa-local: /dev/fuse: {e}");
            ExitCode::FAILURE
        }
    }
}
