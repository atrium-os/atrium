//! Host-namespace mount cleanup helpers. FreeBSD doesn't give us
//! per-jail mount namespaces (jails share the global mount table
//! with the host), so jail-applied nullfs/tmpfs mounts survive
//! the jailed process's exit. Unless we explicitly unmount them,
//! they leak and break the next jail that wants the same target.
//!
//! Used by `init_phase` after the init jail terminates and by
//! `bootstrap` at `--once` exit. The supervisor's handle_exit
//! path uses it for permanently-failed services.

#![allow(unsafe_code)]

use std::io;

/// `close(2)` wrapper. Used at `--once` teardown to explicitly
/// close held procdesc fds in a controlled order before cleanup,
/// rather than letting Rust's drop close them at process exit
/// after we've already done the cleanup work.
pub fn close_fd(fd: i32) -> io::Result<()> {
    // SAFETY: caller owns the fd; libc::close on a valid fd is
    // benign even if the close itself fails (EINTR, EIO, ...).
    let rc = unsafe { libc::close(fd) };
    if rc < 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}

/// `unmount(2)` wrapper. Returns Ok if the mount was removed or
/// was never there (EINVAL "target not mounted"); Err on busy
/// or other failures the caller should log.
#[cfg(target_os = "freebsd")]
pub fn unmount(target: &str) -> io::Result<()> {
    let c = std::ffi::CString::new(target)
        .map_err(|_| io::Error::other("unmount: NUL in path"))?;
    // SAFETY: c is a valid CStr; flags=0 = default (don't force).
    let rc = unsafe { libc::unmount(c.as_ptr(), 0) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        // EINVAL on a path that isn't a mountpoint is "already
        // unmounted" — fine.
        if e.raw_os_error() == Some(libc::EINVAL) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn unmount(_target: &str) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
}

/// Unmount a jail mount given the jail root and the mount's `dest`
/// field as it appears in the CreateJailRequest. The `dest` is what
/// the manifest wrote — usually RELATIVE ("libexec"), occasionally
/// absolute. jaild's child resolves it against the jail path before
/// mounting (server.rs `resolved_mounts`: absolute if it starts with
/// '/', else `jail_path.join(dest)`), so the live mountpoint is at
/// `<jail_path>/<dest>`, NOT `<dest>`. Callers MUST unmount that
/// resolved path: `unmount("libexec")` is a relative path that
/// `libc::unmount` rejects with EINVAL, which we treat as "already
/// gone" — so the REAL mount leaks and the next launch nullfs-mounts
/// on top of it → EDEADLK ("Resource deadlock avoided") → the service
/// crash-loops to a permanent fail. This helper is the single place
/// that resolution lives; it MUST match jaild's `resolved_mounts`.
pub fn unmount_jail_dest(jail_path: &str, dest: &str) -> io::Result<()> {
    unmount(&resolve_jail_dest(jail_path, dest))
}

/// The resolution shared with jaild's `resolved_mounts`. Split out so
/// it can be unit-tested without a live mount.
fn resolve_jail_dest(jail_path: &str, dest: &str) -> String {
    let target = if dest.starts_with('/') {
        std::path::PathBuf::from(dest)
    } else {
        std::path::Path::new(jail_path).join(dest)
    };
    target.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::resolve_jail_dest;

    #[test]
    fn relative_dest_resolves_under_the_jail_root() {
        // The bug: unmounting the raw relative "libexec" no-ops (EINVAL),
        // leaking the real mount at <root>/libexec → next launch EDEADLKs.
        assert_eq!(
            resolve_jail_dest("/var/lib/atrium/jails/memoryd", "libexec"),
            "/var/lib/atrium/jails/memoryd/libexec"
        );
        assert_eq!(
            resolve_jail_dest("/var/lib/atrium/jails/memoryd", "usr/local/bin"),
            "/var/lib/atrium/jails/memoryd/usr/local/bin"
        );
    }

    #[test]
    fn absolute_dest_is_used_verbatim() {
        // Matches jaild: a dest beginning with '/' is NOT re-rooted.
        assert_eq!(resolve_jail_dest("/var/lib/atrium/jails/x", "/dev"), "/dev");
    }
}
