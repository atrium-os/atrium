//! Supervisor-bound listening sockets (docs/spec/fast-boot.md §5).
//!
//! The bootstrap binds every enabled service's `[[sockets]]` before it
//! starts any service, keeps the descriptors for the service's whole life,
//! and hands them to jaild with each (re)launch (`ExecSpec::listen`). So a
//! client's `connect` succeeds as soon as the bootstrap has run, whatever
//! order the services start in, and waits in the listen backlog until the
//! service accepts; a crash and restart keeps the same socket.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;

use crate::system_services::ManifestSocket;

/// Bind one `[[sockets]]` entry: replace a stale socket left by an earlier
/// boot (never any other kind of file), bind, then set owner and mode. The
/// directory must already exist (the service's commissioning makes it, with
/// its own owner and mode, which we do not second-guess).
pub fn bind(s: &ManifestSocket) -> io::Result<OwnedFd> {
    let mode = s.mode_bits().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput,
        format!("{}: mode {:?} is not octal 0000-0777", s.path, s.mode)))?;
    let path = Path::new(&s.path);
    if !path.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not absolute", s.path)));
    }
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists,
            format!("{}: exists and is not a socket", s.path))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let l = UnixListener::bind(path)?;
    std::os::unix::fs::chown(path, Some(s.owner), Some(s.group))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(OwnedFd::from(l))
}

/// Bind all of a service's sockets, or none: if one fails, the ones already
/// bound are closed and removed, and the service starts the old way (it
/// binds its own sockets), so a bad entry costs the early-connect guarantee
/// for that service only, never the service itself.
pub fn bind_all(sockets: &[ManifestSocket]) -> io::Result<Vec<OwnedFd>> {
    let mut out = Vec::with_capacity(sockets.len());
    for s in sockets {
        match bind(s) {
            Ok(fd) => out.push(fd),
            Err(e) => {
                for done in &sockets[..out.len()] {
                    let _ = std::fs::remove_file(&done.path);
                }
                return Err(e);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(path: &Path, mode: &str) -> ManifestSocket {
        ManifestSocket {
            path:  path.to_string_lossy().into_owned(),
            owner: nix_uid(),
            group: nix_gid(),
            mode:  mode.into(),
        }
    }
    #[allow(unsafe_code)]
    fn nix_uid() -> u32 { unsafe { libc::getuid() } }
    #[allow(unsafe_code)]
    fn nix_gid() -> u32 { unsafe { libc::getgid() } }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("listeners-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn connect_succeeds_before_anyone_accepts() {
        let d = tmpdir("early");
        let p = d.join("svc.sock");
        let fd = bind(&sock(&p, "0660")).unwrap();
        // Nobody has called accept: the connection waits in the backlog.
        let mut c = std::os::unix::net::UnixStream::connect(&p).unwrap();
        std::io::Write::write_all(&mut c, b"hello").unwrap();
        // Later the "service" takes the inherited descriptor and accepts.
        let l = UnixListener::from(fd);
        let (mut a, _) = l.accept().unwrap();
        let mut buf = [0u8; 5];
        std::io::Read::read_exact(&mut a, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660);
    }

    #[test]
    fn replaces_a_stale_socket_but_never_a_file() {
        let d = tmpdir("stale");
        let p = d.join("svc.sock");
        drop(UnixListener::bind(&p).unwrap()); // left behind by a dead process
        assert!(bind(&sock(&p, "0600")).is_ok());
        let f = d.join("file.sock");
        std::fs::write(&f, b"not a socket").unwrap();
        assert!(bind(&sock(&f, "0600")).is_err());
        assert_eq!(std::fs::read(&f).unwrap(), b"not a socket");
    }

    #[test]
    fn all_or_nothing() {
        let d = tmpdir("all");
        let good = sock(&d.join("a.sock"), "0600");
        let bad = sock(&d.join("missing-dir/b.sock"), "0600");
        assert!(bind_all(&[good.clone(), bad]).is_err());
        assert!(!d.join("a.sock").exists());
        assert!(sock(&d.join("x"), "0999").mode_bits().is_none());
        assert_eq!(sock(&d.join("x"), "0660").mode_bits(), Some(0o660));
    }
}
