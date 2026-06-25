//! # stoa — S0 skeleton shared helpers
//!
//! The S0 slice (stoa.md §15) is the smallest end-to-end skeleton:
//! `stoactl attach` → local Unix socket → `stoad` → a shell on a pty,
//! raw bytes both ways, one window, no prediction. It deliberately omits
//! the parts that come later so the moving pieces (spawner seam, byte
//! pump, client raw-mode) can be proven on the host first:
//!
//! - **No envelope/MAC/replay** — that is S1 (`stoa-proto`, already
//!   built, wired in at S1).
//! - **No multiplexer** — one window per attach; panes/windows are S2.
//! - **No persistence** — an attach spawns a fresh shell; the daemon
//!   session table + Tessera scrollback are S3. (So S0 "reattach" just
//!   means "connect again", not "resume".)
//!
//! What it *does* prove: the [`DirectSpawner`](stoa_spawn::DirectSpawner)
//! path runs a real shell, and a client can drive it interactively
//! through `stoad` over a socket — the spine everything else hangs on.

use std::io;
use std::os::fd::RawFd;

/// Default control-socket path. Production Stoa uses
/// `/var/run/atrium/stoad.sock` (root-owned); for the dev/macOS skeleton
/// we use a per-uid path under the temp dir, overridable via `$STOA_SOCK`.
pub fn default_socket() -> String {
    if let Ok(s) = std::env::var("STOA_SOCK") {
        return s;
    }
    // SAFETY: getuid is always safe.
    let uid = unsafe { libc::getuid() };
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let tmp = tmp.trim_end_matches('/');
    format!("{tmp}/stoad-{uid}.sock")
}

/// Copy bytes from `from` to `to` until `from` reaches EOF (read returns
/// 0) or a hard error. EINTR is retried; a pty master returning EIO after
/// its slave closes is treated as EOF (the BSD/macOS convention). Used by
/// both ends of the byte bridge.
///
/// Returns `Ok(())` on a clean EOF; `Err` only on an unexpected I/O error
/// on the *read* side (write errors end the pump quietly — the peer went
/// away, which is normal teardown).
pub fn pump(from: RawFd, to: RawFd) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        // SAFETY: read into a stack buffer we own, len bytes.
        let n = unsafe { libc::read(from, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EIO) => return Ok(()), // pty slave closed → EOF
                _ => return Err(err),
            }
        }
        if n == 0 {
            return Ok(()); // clean EOF
        }
        if !write_all(to, &buf[..n as usize]) {
            return Ok(()); // peer gone; quiet teardown
        }
    }
}

/// Write the whole slice to `fd`, retrying short writes and EINTR.
/// Returns `false` if the peer is gone (write error other than EINTR).
fn write_all(fd: RawFd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        // SAFETY: write from a slice we own, len bytes.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
        if n == 0 {
            return false;
        }
        data = &data[n as usize..];
    }
    true
}
