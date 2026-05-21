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
use aqueduct::classes::{CLASS_LOG, CLASS_NET, CLASS_NOTIFY, CLASS_VESTIBULUM};
use aqueduct::envelope::{flag, Header};

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

// =====================================================
// Keychain — per-service ed25519 keypairs managed by
// Vestibulum. The private key never crosses this
// boundary; apps obtain only public keys + signatures.
// =====================================================

/// ed25519 public key length.
pub const ATRIUM_KEYCHAIN_PUBKEY_LEN: usize = 32;
/// ed25519 signature length.
pub const ATRIUM_KEYCHAIN_SIG_LEN: usize = 64;

/// Vestibulum daemon is unreachable (env var unset,
/// connect refused, or RPC failure).
pub const ATRIUM_ERR_NO_VESTIBULUM: c_int = -10;
/// Daemon responded but the response was malformed
/// (e.g. wrong byte length).
pub const ATRIUM_ERR_VESTIBULUM_RPC: c_int = -11;

/// Open a fresh connection to the Vestibulum daemon if
/// `$ATRIUM_VESTIBULUM_SOCKET` is set. Each call opens
/// a new connection — keychain ops are infrequent and
/// the overhead is dwarfed by the signature work
/// itself; pooling can come later.
fn vestibulum_connect() -> Option<Connection> {
    let path = std::env::var_os("ATRIUM_VESTIBULUM_SOCKET")?;
    Connection::connect(std::path::Path::new(&path)).ok()
}

/// Send a CLASS_VESTIBULUM request, receive the
/// matching response. Filters out incidental traffic
/// (e.g. async events) and ignores responses to a
/// different op.
fn vestibulum_rpc(
    conn: &mut Connection,
    op: u16,
    request_payload: &[u8],
) -> Option<Vec<u8>> {
    conn.send_message(
        CLASS_VESTIBULUM,
        op,
        flag::RESPONSE_EXPECTED,
        request_payload,
    )
    .ok()?;

    loop {
        let msg = conn.recv_message().ok()?;
        if msg.opcode_class == CLASS_VESTIBULUM
            && msg.op == op
            && (msg.flags & flag::IS_RESPONSE) != 0
        {
            return Some(msg.payload);
        }
        // Otherwise ignore and keep reading.
    }
}

/// Fetch the public key for `service`, minting on the
/// daemon side if no keypair yet exists for the
/// (service, default-persona) pair.
///
/// Writes the 32-byte ed25519 public key into `out`.
/// Returns:
///   - `ATRIUM_KEYCHAIN_PUBKEY_LEN` (32) on success.
///   - [`ATRIUM_ERR_INVALID_PATH`] if `service` is
///     NULL or invalid UTF-8 (the parameter name
///     "path" is a misnomer for this call — kept
///     for the shared error vocabulary).
///   - [`ATRIUM_ERR_BUF_TOO_SMALL`] if `out_len <
///     ATRIUM_KEYCHAIN_PUBKEY_LEN`.
///   - [`ATRIUM_ERR_NO_VESTIBULUM`] if the daemon
///     can't be reached.
///   - [`ATRIUM_ERR_VESTIBULUM_RPC`] if the daemon's
///     response is malformed.
///
/// # Safety
///
/// `service` must be a valid NUL-terminated UTF-8
/// string. `out` must be valid for `out_len` writes.
#[no_mangle]
pub unsafe extern "C" fn atrium_keychain_pubkey(
    service: *const c_char,
    out: *mut u8,
    out_len: usize,
) -> c_int {
    if service.is_null() {
        return ATRIUM_ERR_INVALID_PATH;
    }
    let svc_str = match CStr::from_ptr(service).to_str() {
        Ok(s) => s,
        Err(_) => return ATRIUM_ERR_INVALID_PATH,
    };
    if out_len < ATRIUM_KEYCHAIN_PUBKEY_LEN {
        return ATRIUM_ERR_BUF_TOO_SMALL;
    }
    let Some(mut conn) = vestibulum_connect() else {
        return ATRIUM_ERR_NO_VESTIBULUM;
    };
    let Some(resp) = vestibulum_rpc(&mut conn, 0, svc_str.as_bytes()) else {
        return ATRIUM_ERR_VESTIBULUM_RPC;
    };
    if resp.len() != ATRIUM_KEYCHAIN_PUBKEY_LEN {
        return ATRIUM_ERR_VESTIBULUM_RPC;
    }
    std::ptr::copy_nonoverlapping(resp.as_ptr(), out, ATRIUM_KEYCHAIN_PUBKEY_LEN);
    ATRIUM_KEYCHAIN_PUBKEY_LEN as c_int
}

// =====================================================
// Notifications — POST to Praeco daemon.
// =====================================================

/// Urgency level for [`atrium_notify_post`].
pub const ATRIUM_NOTIFY_LOW: c_uint = 0;
/// Normal urgency (default for foreground apps).
pub const ATRIUM_NOTIFY_NORMAL: c_uint = 1;
/// High urgency — interrupts even Do-Not-Disturb-ish UX.
pub const ATRIUM_NOTIFY_HIGH: c_uint = 2;

/// Praeco daemon unreachable.
pub const ATRIUM_ERR_NO_PRAECO: c_int = -30;
/// Praeco responded but the response was malformed.
pub const ATRIUM_ERR_PRAECO_RPC: c_int = -31;

/// Post a notification via the Praeco daemon. Returns
/// a positive notification id on success, or a
/// negative error code.
///
/// `title` and `body` are NUL-terminated UTF-8.
/// `urgency` is one of `ATRIUM_NOTIFY_LOW` /
/// `_NORMAL` / `_HIGH`.
///
/// v0 surface — actions / groups / replaces_id from
/// the spec are not exposed yet.
///
/// # Safety
///
/// `title` and `body` must be valid NUL-terminated
/// pointers.
#[no_mangle]
pub unsafe extern "C" fn atrium_notify_post(
    title: *const c_char,
    body: *const c_char,
    urgency: c_uint,
) -> i64 {
    if title.is_null() || body.is_null() {
        return ATRIUM_ERR_INVALID_PATH as i64;
    }
    let title_str = match CStr::from_ptr(title).to_str() {
        Ok(s) => s,
        Err(_) => return ATRIUM_ERR_INVALID_PATH as i64,
    };
    let body_str = match CStr::from_ptr(body).to_str() {
        Ok(s) => s,
        Err(_) => return ATRIUM_ERR_INVALID_PATH as i64,
    };
    if title_str.len() > u16::MAX as usize || body_str.len() > u16::MAX as usize {
        return ATRIUM_ERR_INVALID_PATH as i64;
    }
    let urgency_byte: u8 = match urgency {
        ATRIUM_NOTIFY_LOW => 0,
        ATRIUM_NOTIFY_NORMAL => 1,
        ATRIUM_NOTIFY_HIGH => 2,
        _ => return ATRIUM_ERR_INVALID_MODE as i64,
    };

    let Some(sock_path) = std::env::var_os("ATRIUM_PRAECO_SOCKET") else {
        return ATRIUM_ERR_NO_PRAECO as i64;
    };
    let mut conn =
        match Connection::connect(std::path::Path::new(&sock_path)) {
            Ok(c) => c,
            Err(_) => return ATRIUM_ERR_NO_PRAECO as i64,
        };

    // payload: u8 urgency | u16 title_len | title | u16 body_len | body
    let mut payload = Vec::with_capacity(
        1 + 2 + title_str.len() + 2 + body_str.len()
    );
    payload.push(urgency_byte);
    payload.extend_from_slice(&(title_str.len() as u16).to_le_bytes());
    payload.extend_from_slice(title_str.as_bytes());
    payload.extend_from_slice(&(body_str.len() as u16).to_le_bytes());
    payload.extend_from_slice(body_str.as_bytes());

    if conn.send_message(
        CLASS_NOTIFY,
        0, // OP_POST_NOTIFICATION
        flag::RESPONSE_EXPECTED,
        &payload,
    )
    .is_err()
    {
        return ATRIUM_ERR_PRAECO_RPC as i64;
    }

    // Filter to matching response.
    loop {
        let msg = match conn.recv_message() {
            Ok(m) => m,
            Err(_) => return ATRIUM_ERR_PRAECO_RPC as i64,
        };
        if msg.opcode_class == CLASS_NOTIFY
            && msg.op == 0
            && (msg.flags & flag::IS_RESPONSE) != 0
        {
            // Response: u8 status | u64 id LE
            if msg.payload.len() < 9 {
                return ATRIUM_ERR_PRAECO_RPC as i64;
            }
            let status = msg.payload[0];
            let id = u64::from_le_bytes(msg.payload[1..9].try_into().unwrap());
            if status != 0 {
                return ATRIUM_ERR_PRAECO_RPC as i64;
            }
            return id as i64;
        }
    }
}

// =====================================================
// Network — outbound connections via the broker
// (atrium-netd-macos). The connection returned to the
// app is a unix-domain socket that byte-proxies to the
// underlying TCP; the daemon manages the actual TCP
// socket. The app uses the returned fd with normal
// libc read(2) / write(2) / close(2).
// =====================================================

/// Network protocol: TCP.
pub const ATRIUM_NET_TCP: c_uint = 0;
/// Network protocol: UDP. (v0 broker does not yet
/// implement UDP; the constant is reserved.)
pub const ATRIUM_NET_UDP: c_uint = 1;

/// Errors specific to the network broker.
pub const ATRIUM_ERR_NO_NETD: c_int = -20;
/// Broker rejected the request (denied, protocol
/// unsupported, malformed, etc.). Detail in the broker's log.
pub const ATRIUM_ERR_NETD_DENIED: c_int = -21;
/// DNS resolution failed at the broker.
pub const ATRIUM_ERR_NETD_DNS: c_int = -22;
/// Upstream TCP connect failed at the broker.
pub const ATRIUM_ERR_NETD_CONNECT: c_int = -23;
/// Broker responded but the response was malformed.
pub const ATRIUM_ERR_NETD_RPC: c_int = -24;

/// Open an outbound network connection to `host:port`
/// via the network broker.
///
/// Returns an OS file descriptor on success — the same
/// shape `socket(2)` would return — usable with libc
/// `read(2)` / `write(2)` / `close(2)`. Under the hood
/// the fd is a unix-domain socket that the broker
/// proxies to a real TCP connection; the app never
/// sees the raw TCP socket.
///
/// `proto` is `ATRIUM_NET_TCP` or `ATRIUM_NET_UDP`.
/// v0 broker supports TCP only.
///
/// Returns:
///   - The fd on success.
///   - `ATRIUM_ERR_NO_NETD` if no broker is reachable
///     (`$ATRIUM_NETD_SOCKET` unset or connect refused).
///   - `ATRIUM_ERR_NETD_DENIED` if the broker rejected
///     the request (hostname not allowlisted, etc.).
///   - `ATRIUM_ERR_NETD_DNS` / `_CONNECT` for upstream
///     failures.
///   - `ATRIUM_ERR_INVALID_PATH` if `host` is NULL or
///     not valid UTF-8.
///
/// # Safety
///
/// `host` must be either NULL or a valid NUL-terminated
/// UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn atrium_net_connect(
    host: *const c_char,
    port: u16,
    proto: c_uint,
) -> c_int {
    if host.is_null() {
        return ATRIUM_ERR_INVALID_PATH;
    }
    let host_str = match CStr::from_ptr(host).to_str() {
        Ok(s) => s,
        Err(_) => return ATRIUM_ERR_INVALID_PATH,
    };
    if proto != ATRIUM_NET_TCP && proto != ATRIUM_NET_UDP {
        return ATRIUM_ERR_INVALID_PATH;
    }

    let Some(sock_path) = std::env::var_os("ATRIUM_NETD_SOCKET") else {
        return ATRIUM_ERR_NO_NETD;
    };

    // Open the unix socket connection to the broker.
    // We use std::os::unix::net::UnixStream directly
    // rather than aqueduct::Connection because once
    // CONNECT succeeds the channel switches to byte-
    // proxy mode (no more aqueduct framing). We just
    // hand-roll the envelope encode/decode for the
    // single handshake.
    use std::os::unix::net::UnixStream;
    use std::io::{Read, Write};
    use std::os::fd::IntoRawFd;

    let mut stream =
        match UnixStream::connect(std::path::Path::new(&sock_path)) {
            Ok(s) => s,
            Err(_) => return ATRIUM_ERR_NO_NETD,
        };

    // Build CONNECT_REQUEST payload: [u8 proto |
    // u16 port LE | utf8 host].
    let proto_byte: u8 = if proto == ATRIUM_NET_UDP { 1 } else { 0 };
    let mut payload = Vec::with_capacity(3 + host_str.len());
    payload.push(proto_byte);
    payload.extend_from_slice(&port.to_le_bytes());
    payload.extend_from_slice(host_str.as_bytes());

    let header = Header::new(
        CLASS_NET,
        0, // OP_CONNECT_REQUEST
        flag::RESPONSE_EXPECTED,
        payload.len() as u32,
    );
    if stream.write_all(&header.encode()).is_err() {
        return ATRIUM_ERR_NETD_RPC;
    }
    if stream.write_all(&payload).is_err() {
        return ATRIUM_ERR_NETD_RPC;
    }
    // Don't flush on Mac — write_all already pushes.

    // Read the response envelope.
    let mut hdr_buf = [0u8; aqueduct::HEADER_LEN];
    if stream.read_exact(&mut hdr_buf).is_err() {
        return ATRIUM_ERR_NETD_RPC;
    }
    let resp_hdr = match Header::decode(&hdr_buf) {
        Ok(h) => h,
        Err(_) => return ATRIUM_ERR_NETD_RPC,
    };
    if resp_hdr.length != 1 {
        return ATRIUM_ERR_NETD_RPC;
    }
    let mut status_byte = [0u8; 1];
    if stream.read_exact(&mut status_byte).is_err() {
        return ATRIUM_ERR_NETD_RPC;
    }
    match status_byte[0] {
        0 => {
            // OK — the broker is now byte-proxying.
            // Hand the unix-socket fd to the caller;
            // they use it as their network connection.
            stream.into_raw_fd() as c_int
        }
        2 => ATRIUM_ERR_NETD_DENIED,
        3 => ATRIUM_ERR_NETD_DNS,
        4 => ATRIUM_ERR_NETD_CONNECT,
        _ => ATRIUM_ERR_NETD_DENIED, // 1 / 5 / unknown
    }
}

/// Sign `challenge` (`challenge_len` bytes) under the
/// keypair for `service`. The private key never
/// leaves the Vestibulum daemon — only the resulting
/// signature comes back.
///
/// Writes the 64-byte ed25519 signature into `sig_out`.
/// Returns:
///   - `ATRIUM_KEYCHAIN_SIG_LEN` (64) on success.
///   - error codes as in [`atrium_keychain_pubkey`].
///
/// # Safety
///
/// `service` is a NUL-terminated UTF-8 string.
/// `challenge` is valid for `challenge_len` reads.
/// `sig_out` is valid for `sig_out_len` writes.
#[no_mangle]
pub unsafe extern "C" fn atrium_keychain_sign(
    service: *const c_char,
    challenge: *const u8,
    challenge_len: usize,
    sig_out: *mut u8,
    sig_out_len: usize,
) -> c_int {
    if service.is_null() || challenge.is_null() {
        return ATRIUM_ERR_INVALID_PATH;
    }
    let svc_str = match CStr::from_ptr(service).to_str() {
        Ok(s) => s,
        Err(_) => return ATRIUM_ERR_INVALID_PATH,
    };
    if sig_out_len < ATRIUM_KEYCHAIN_SIG_LEN {
        return ATRIUM_ERR_BUF_TOO_SMALL;
    }
    if svc_str.len() > u16::MAX as usize {
        return ATRIUM_ERR_INVALID_PATH;
    }
    let Some(mut conn) = vestibulum_connect() else {
        return ATRIUM_ERR_NO_VESTIBULUM;
    };

    // Build the SIGN_REQUEST payload: [u16 LE name_len
    // | name | challenge_bytes].
    let challenge_slice = std::slice::from_raw_parts(challenge, challenge_len);
    let mut payload =
        Vec::with_capacity(2 + svc_str.len() + challenge_len);
    payload.extend_from_slice(&(svc_str.len() as u16).to_le_bytes());
    payload.extend_from_slice(svc_str.as_bytes());
    payload.extend_from_slice(challenge_slice);

    let Some(resp) = vestibulum_rpc(&mut conn, 1, &payload) else {
        return ATRIUM_ERR_VESTIBULUM_RPC;
    };
    if resp.len() != ATRIUM_KEYCHAIN_SIG_LEN {
        return ATRIUM_ERR_VESTIBULUM_RPC;
    }
    std::ptr::copy_nonoverlapping(resp.as_ptr(), sig_out, ATRIUM_KEYCHAIN_SIG_LEN);
    ATRIUM_KEYCHAIN_SIG_LEN as c_int
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
