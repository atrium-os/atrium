//! libatrium — the Insula platform library, C ABI surface.
//!
//! Apps (in any language) link against this shared
//! object to talk to the Insula platform. Per
//! `docs/spec/insula.md` §2.3.
//!
//! # v0 surface
//!
//! Three calls — the absolute minimum for a "hello
//! world" Insula app:
//!
//! - [`atrium_init`] — handshake with the platform on
//!   startup. Reports the SDK version the app was built
//!   against; receives back a status (`0` on success).
//! - [`atrium_log`] — log a message at a given severity
//!   level. v0 writes to stderr with a structured
//!   prefix; subsequent slices route via Aqueduct to
//!   the system log service.
//! - [`atrium_exit`] — clean termination with an exit
//!   code. Flushes platform-side state (none yet) and
//!   then `exit`s.
//!
//! Subsequent slices add: `atrium_fresco_*` for window
//! creation, `atrium_storage_*` for the app's Tessera
//! namespace, `atrium_net_connect` for the network
//! broker, `atrium_limen_*` for cross-jail embeds,
//! `atrium_keychain_*` for Vestibulum, ... — each one
//! a thin C-ABI wrapper around an Aqueduct typed
//! message exchange (see `limen.md` §2 layering note).

#![warn(missing_docs)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::Mutex;

use aqueduct::Connection;
use aqueduct::classes::CLASS_LOG;
use aqueduct::envelope::flag;

/// Lazily-initialized platform connection. None until
/// [`atrium_init`] runs; populated to `Some(conn)` if the
/// environment exposes a socket path via
/// `ATRIUM_LOG_SOCKET`; stays `None` otherwise (in which
/// case [`atrium_log`] falls back to stderr).
///
/// Log messages route over [`CLASS_LOG`] (the dedicated
/// log-forwarding opcode class registered in the
/// Aqueduct class registry).
static PLATFORM_CONN: Mutex<Option<Connection>> = Mutex::new(None);

/// Log severity. Matches syslog ordering: lower = more
/// verbose.
pub const ATRIUM_LOG_TRACE: c_uint = 0;
/// Debug-level log.
pub const ATRIUM_LOG_DEBUG: c_uint = 1;
/// Informational log.
pub const ATRIUM_LOG_INFO: c_uint = 2;
/// Warning log.
pub const ATRIUM_LOG_WARN: c_uint = 3;
/// Error log.
pub const ATRIUM_LOG_ERROR: c_uint = 4;

/// Status returned by [`atrium_init`].
pub const ATRIUM_OK: c_int = 0;
/// Status: the platform does not support the requested
/// SDK version.
pub const ATRIUM_ERR_SDK_VERSION: c_int = -1;
/// Status: a required platform service is unreachable.
pub const ATRIUM_ERR_PLATFORM_UNREACHABLE: c_int = -2;

/// Initialize the Atrium platform connection.
///
/// Apps call this exactly once, early in `main` (or
/// runtime startup). Arguments report the SDK major /
/// minor version the app was built against; the
/// platform decides whether it can serve that version
/// and returns:
///
/// - [`ATRIUM_OK`] — handshake succeeded; the app may
///   proceed to use other platform calls.
/// - [`ATRIUM_ERR_SDK_VERSION`] — platform is too old
///   for this SDK version. App should exit gracefully.
/// - [`ATRIUM_ERR_PLATFORM_UNREACHABLE`] — the platform
///   library is loaded but cannot reach the Atrium
///   host (e.g. Aqueduct socket missing). Indicates a
///   broken install or a jail without the required
///   capabilities.
///
/// v0 logs the call to stderr and tries to open a
/// connection to the platform log service if
/// `ATRIUM_LOG_SOCKET` is set in the environment. Any
/// connect failure falls back to stderr-only mode and
/// still returns `ATRIUM_OK` — failed-to-connect is a
/// degraded mode, not a hard error (matches the
/// principle that an app should always be able to
/// initialize and log).
#[no_mangle]
pub extern "C" fn atrium_init(
    sdk_major: c_uint,
    sdk_minor: c_uint,
) -> c_int {
    eprintln!(
        "[insula][libatrium] atrium_init(sdk={}.{})",
        sdk_major, sdk_minor
    );

    // Try to open the platform connection. Best-effort:
    // failure here just keeps us in stderr-only mode.
    if let Ok(path) = std::env::var("ATRIUM_LOG_SOCKET") {
        match Connection::connect(&path) {
            Ok(conn) => {
                if let Ok(mut guard) = PLATFORM_CONN.lock() {
                    *guard = Some(conn);
                }
                eprintln!(
                    "[insula][libatrium] connected to platform log at {}",
                    path
                );
            }
            Err(e) => {
                eprintln!(
                    "[insula][libatrium] could not open ATRIUM_LOG_SOCKET={}: {} \
                     (falling back to stderr-only)",
                    path, e
                );
            }
        }
    }

    ATRIUM_OK
}

/// Emit a log message at the given level.
///
/// `msg` is a NUL-terminated UTF-8 string. NULL or
/// invalid UTF-8 produces a structured "invalid log
/// payload" trace rather than crashing.
///
/// v0 writes to stderr with `[insula][LEVEL]` prefix;
/// later slices route via Aqueduct.
///
/// # Safety
///
/// `msg` must be either NULL or a valid pointer to a
/// NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn atrium_log(
    level: c_uint,
    msg: *const c_char,
) {
    let level_str = match level {
        ATRIUM_LOG_TRACE => "TRACE",
        ATRIUM_LOG_DEBUG => "DEBUG",
        ATRIUM_LOG_INFO => "INFO",
        ATRIUM_LOG_WARN => "WARN",
        ATRIUM_LOG_ERROR => "ERROR",
        _ => "?",
    };

    let payload = if msg.is_null() {
        std::borrow::Cow::Borrowed("<null>")
    } else {
        // SAFETY: caller's responsibility per the
        // function's safety doc.
        CStr::from_ptr(msg).to_string_lossy()
    };

    // Always emit to stderr — it's the universal floor
    // and useful for development.
    eprintln!("[insula][{}] {}", level_str, payload);

    // If a platform connection is up, also forward over
    // Aqueduct. Best-effort: a failed send leaves the
    // stderr line as the surviving log.
    //
    // v0 wire payload: [u8 level | utf8 message bytes].
    // This is the smoke-test shape over CLASS_ECHO;
    // the real log service will define a typed schema.
    if let Ok(mut guard) = PLATFORM_CONN.lock() {
        if let Some(conn) = guard.as_mut() {
            let mut payload_bytes = Vec::with_capacity(payload.len() + 1);
            payload_bytes.push(level as u8);
            payload_bytes.extend_from_slice(payload.as_bytes());
            let _ = conn.send_message(
                CLASS_LOG,
                0,                 // op = 0 (log forward; [level_u8 | utf8])
                flag::ASYNC_EVENT, // fire-and-forget; no reply expected
                &payload_bytes,
            );
        }
    }
}

/// Cleanly terminate the Insula app.
///
/// Flushes any platform-side state (none in v0) and
/// then `exit`s the process with the given code.
#[no_mangle]
pub extern "C" fn atrium_exit(code: c_int) -> ! {
    eprintln!("[insula][libatrium] atrium_exit({})", code);
    std::process::exit(code);
}

// =====================================================
// Storage — access to the app's sandbox container.
// =====================================================

/// Returned by [`atrium_storage_open`] on success: the
/// raw OS file descriptor the app uses with standard
/// `read(2)` / `write(2)` / `close(2)`.
pub type AtriumFd = c_int;

/// Open mode: read-only.
pub const ATRIUM_STORAGE_READ: c_uint = 0;
/// Open mode: write-only, truncate existing.
pub const ATRIUM_STORAGE_WRITE: c_uint = 1;
/// Open mode: write-only, append to existing.
pub const ATRIUM_STORAGE_APPEND: c_uint = 2;

/// Open-failure error codes (returned as negative fds).
pub const ATRIUM_ERR_NO_CONTAINER: c_int = -1;
/// The buffer passed to [`atrium_container_path`] was
/// too small for the container path; the function
/// writes nothing in this case and returns this code.
pub const ATRIUM_ERR_BUF_TOO_SMALL: c_int = -2;
/// File I/O failed; libc errno-equivalent.
pub const ATRIUM_ERR_IO: c_int = -3;
/// Invalid mode passed to [`atrium_storage_open`].
pub const ATRIUM_ERR_INVALID_MODE: c_int = -4;
/// `path` argument was NULL or not valid UTF-8 / NUL-
/// terminated.
pub const ATRIUM_ERR_INVALID_PATH: c_int = -5;

/// Write the absolute path of this app's container
/// directory into `buf` (up to `buf_len` bytes
/// including a trailing NUL). Returns:
///
/// - The path length **excluding** the NUL on success.
/// - [`ATRIUM_ERR_NO_CONTAINER`] if no container has
///   been provisioned (i.e. `$ATRIUM_CONTAINER_DIR` is
///   unset — typical when an app is run outside the
///   host adapter).
/// - [`ATRIUM_ERR_BUF_TOO_SMALL`] if `buf` cannot hold
///   the path + NUL.
///
/// # Safety
///
/// `buf` must be valid for writes up to `buf_len`
/// bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_container_path(
    buf: *mut c_char,
    buf_len: usize,
) -> c_int {
    let path = match container_dir() {
        Some(p) => p,
        None => return ATRIUM_ERR_NO_CONTAINER,
    };
    let bytes = path.as_os_str().as_encoded_bytes();
    let needed = bytes.len() + 1; // + NUL
    if buf.is_null() || needed > buf_len {
        return ATRIUM_ERR_BUF_TOO_SMALL;
    }
    // SAFETY: caller's contract — buf valid for buf_len
    // writes; we've checked needed <= buf_len above.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const c_char,
            buf,
            bytes.len(),
        );
        *buf.add(bytes.len()) = 0;
    }
    bytes.len() as c_int
}

/// Open a file at `path` (NUL-terminated UTF-8,
/// relative to the app's container directory) with
/// `mode` ∈ {[`ATRIUM_STORAGE_READ`],
/// [`ATRIUM_STORAGE_WRITE`], [`ATRIUM_STORAGE_APPEND`]}.
/// Returns the OS file descriptor on success or a
/// negative error code.
///
/// The app uses the returned fd with normal libc
/// `read(2)` / `write(2)` / `close(2)`.
///
/// # Safety
///
/// `path` must be either NULL or a valid pointer to a
/// NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn atrium_storage_open(
    path: *const c_char,
    mode: c_uint,
) -> c_int {
    if path.is_null() {
        return ATRIUM_ERR_INVALID_PATH;
    }
    let cstr = unsafe { CStr::from_ptr(path) };
    let rel = match cstr.to_str() {
        Ok(s) => s,
        Err(_) => return ATRIUM_ERR_INVALID_PATH,
    };

    let container = match container_dir() {
        Some(p) => p,
        None => return ATRIUM_ERR_NO_CONTAINER,
    };

    // Trivial path-traversal guard: reject leading `/`
    // (would escape via absolute path) and any `..`
    // component. The sandbox would deny the access
    // anyway, but failing fast is friendlier to apps.
    if rel.starts_with('/') {
        return ATRIUM_ERR_INVALID_PATH;
    }
    if rel.split('/').any(|seg| seg == "..") {
        return ATRIUM_ERR_INVALID_PATH;
    }

    let full = container.join(rel);

    let mut opts = std::fs::OpenOptions::new();
    match mode {
        ATRIUM_STORAGE_READ => {
            opts.read(true);
        }
        ATRIUM_STORAGE_WRITE => {
            opts.write(true).create(true).truncate(true);
        }
        ATRIUM_STORAGE_APPEND => {
            opts.write(true).create(true).append(true);
        }
        _ => return ATRIUM_ERR_INVALID_MODE,
    }

    match opts.open(&full) {
        Ok(file) => {
            use std::os::fd::IntoRawFd;
            file.into_raw_fd() as c_int
        }
        Err(_) => ATRIUM_ERR_IO,
    }
}

/// Resolve the container directory from
/// `$ATRIUM_CONTAINER_DIR`. Re-read each call —
/// cheap, and lets tests vary the env per case
/// without process-wide caching.
fn container_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("ATRIUM_CONTAINER_DIR")
        .map(std::path::PathBuf::from)
}

// ---------------------------------------------------------
// Unit tests — pure-functional pieces only; the FFI
// surface is exercised end-to-end by insula-hello.
// ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_returns_ok() {
        assert_eq!(atrium_init(1, 0), ATRIUM_OK);
    }

    #[test]
    fn log_handles_null() {
        // Doesn't crash. (No way to capture stderr from
        // within Rust tests cleanly without going to
        // external tooling; the smoke test here is
        // that the call returns.)
        unsafe {
            atrium_log(ATRIUM_LOG_INFO, std::ptr::null());
        }
    }
}
