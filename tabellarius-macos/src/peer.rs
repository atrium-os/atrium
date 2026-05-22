//! Kernel-attested peer identification for wake-on-push.
//!
//! When a process subscribes, the daemon wants to know
//! *which installed Insula app* it is — that's the app
//! whose `[background.triggered]` entry wake-on-push
//! should spawn. The app reports its own id (derived
//! from `$ATRIUM_CONTAINER_DIR`, threaded through the
//! SUBSCRIBE payload), but a malicious app could lie.
//!
//! This module supplies the kernel-attested answer:
//!   1. `getsockopt(LOCAL_PEERPID)` on the subscribe
//!      connection — the pid of the connecting process,
//!      which the app cannot forge.
//!   2. `proc_pidpath` — that pid's executable path.
//!   3. Walking `<install_root>/apps/*/bundle/` matches
//!      the exe to an installed app id.
//!
//! Same mechanism atrium-netd uses for per-app network
//! enforcement. Any step can fail benignly (peer isn't
//! an installed app, install root unset, process gone)
//! — failure means "no attested id", and the daemon
//! falls back to the app's self-reported id.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// `getsockopt(LOCAL_PEERPID)` — the kernel-attested
/// pid of the process on the other end of `stream`.
pub fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 && pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// `proc_pidpath` — the absolute executable path of
/// `pid`, or `None` if the process is gone / unreadable.
pub fn pid_executable_path(pid: i32) -> Option<PathBuf> {
    // Apple's PROC_PIDPATHINFO_MAXSIZE = 4096.
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::proc_pidpath(
            pid,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let s = std::str::from_utf8(&buf).ok()?;
    Some(PathBuf::from(s))
}

/// Match an exe path to an installed app id by walking
/// `<install_root>/apps/*/bundle/`. Paths are
/// canonicalized first so the macOS `/var` ↔
/// `/private/var` symlink doesn't cause false misses.
pub fn app_id_for_exe(install_root: &Path, exe: &Path) -> Option<String> {
    let exe_canon = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let apps_dir = install_root.join("apps");
    if !apps_dir.is_dir() {
        return None;
    }
    for entry in std::fs::read_dir(&apps_dir).ok()? {
        let entry = entry.ok()?;
        let app_id = entry.file_name().to_string_lossy().to_string();
        let bundle = entry.path().join("bundle");
        let bundle_canon = bundle.canonicalize().unwrap_or_else(|_| bundle.clone());
        if exe_canon.starts_with(&bundle_canon) {
            return Some(app_id);
        }
    }
    None
}

/// Best-effort kernel-attested app id for the peer on
/// `stream`. `None` when the install root is unset, the
/// peer can't be pid-resolved, or its exe doesn't live
/// under any installed bundle.
pub fn attest_app_id(stream: &UnixStream, install_root: Option<&Path>)
    -> Option<String>
{
    let root = install_root?;
    let pid = peer_pid(stream)?;
    let exe = pid_executable_path(pid)?;
    app_id_for_exe(root, &exe)
}

/// Resolve the install root from `$INSULA_INSTALL_ROOT`.
pub fn resolve_install_root() -> Option<PathBuf> {
    std::env::var_os("INSULA_INSTALL_ROOT").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_for_exe_matches_installed_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let app_dir = tmp.path().join("apps").join("com.example.x");
        std::fs::create_dir_all(app_dir.join("bundle/bin")).unwrap();
        let exe = app_dir.join("bundle/bin/the-app");
        std::fs::write(&exe, b"binary").unwrap();

        assert_eq!(
            app_id_for_exe(tmp.path(), &exe).as_deref(),
            Some("com.example.x"),
        );
    }

    #[test]
    fn app_id_for_exe_misses_outside_install_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("apps")).unwrap();
        assert_eq!(app_id_for_exe(tmp.path(), Path::new("/bin/echo")), None);
    }

    #[test]
    fn app_id_for_exe_misses_when_no_apps_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // No apps/ subdir at all.
        assert_eq!(
            app_id_for_exe(tmp.path(), Path::new("/anything")),
            None,
        );
    }

    #[test]
    fn app_id_for_exe_picks_the_right_app_among_several() {
        let tmp = tempfile::tempdir().unwrap();
        for id in ["com.example.a", "com.example.b", "com.example.c"] {
            let d = tmp.path().join("apps").join(id).join("bundle/bin");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("x"), b"bin").unwrap();
        }
        let exe = tmp.path()
            .join("apps/com.example.b/bundle/bin/x");
        assert_eq!(
            app_id_for_exe(tmp.path(), &exe).as_deref(),
            Some("com.example.b"),
        );
    }
}
