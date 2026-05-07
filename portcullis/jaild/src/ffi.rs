//! libjail / libc syscall wrappers. The **only** module in this
//! crate that uses `unsafe`. Everything else stays under the
//! crate-root `#![forbid(unsafe_code)]`.
//!
//! Each function here:
//!   1. Takes safe Rust types only (`&str`, integers, owned `String`).
//!   2. Builds the C-side iovec / null-terminated strings inside.
//!   3. Calls the syscall.
//!   4. Returns a typed `Result`, never `errno` as a magic global.
//!
//! No memory is ever returned to the caller from these functions
//! (no raw pointers, no `Vec<u8>` aliasing C buffers). That keeps
//! the unsafe surface bounded to the body of each function.
//!
//! ## Why direct libc rather than a `jail` crate
//!
//! There are existing crates (`jail`, `freebsd-jail`) but the
//! smallest-TCB carve-out (LANGUAGE-POLICY.md) wants us to keep
//! deps to the auditable minimum. The full surface jaild needs
//! is `jail_set`, `jail_remove`, and (V1) `pdfork` + `execve` +
//! `setuid` — all small and stable. Wrapping them ourselves keeps
//! the trusted-code dep set to `libc + serde + serde_json + toml +
//! thiserror + log`.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;

/// Flags for `jail_set(2)`. Mirrors `<sys/jail.h>` values.
pub const JAIL_CREATE: i32 = 0x01;
pub const JAIL_UPDATE: i32 = 0x02;
pub const JAIL_ATTACH: i32 = 0x04;
#[allow(dead_code)]
pub const JAIL_DYING:  i32 = 0x08;

#[cfg(target_os = "freebsd")]
extern "C" {
    /// `int jail_set(struct iovec *iov, unsigned int niov, int flags);`
    fn jail_set(iov: *mut libc::iovec, niov: u32, flags: i32) -> i32;

    /// `int jail_remove(int jid);`
    fn jail_remove(jid: i32) -> i32;
}

/* Stubs on non-FreeBSD (macOS host build for `cargo test` of
 * validator + protocol). Production targets are FreeBSD only. */
#[cfg(not(target_os = "freebsd"))]
#[allow(non_snake_case)]
unsafe fn jail_set(_iov: *mut libc::iovec, _niov: u32, _flags: i32) -> i32 {
    libc::__error().write(libc::ENOSYS);
    -1
}

#[cfg(not(target_os = "freebsd"))]
unsafe fn jail_remove(_jid: i32) -> i32 {
    libc::__error().write(libc::ENOSYS);
    -1
}

/// Spec for the V0 `jail_set` call. Mirrors the fields supported
/// in `protocol::CreateJailRequest`. Future fields (mounts,
/// devfs_ruleset, ip4, exec) will be added in V1 by adding more
/// iovec pairs in the `create_jail` body.
pub struct JailCreateSpec<'a> {
    pub name:         &'a str,
    pub path:         &'a str,
    pub persist:      i32,    // 0 or 1
    pub children_max: i32,
}

/// Result of a successful `jail_set(JAIL_CREATE)`. The `jid` is
/// kernel-assigned and stable for the lifetime of the jail.
pub struct CreatedJail {
    pub jid: i32,
}

/// Make a persistent jail. Wraps `jail_set(2)` with `JAIL_CREATE`.
/// Returns the kernel-assigned jail id, or a typed error.
pub fn create_persistent_jail(spec: &JailCreateSpec) -> io::Result<CreatedJail> {
    /* Build C strings once; their `as_ptr()` is valid as long as
     * each `CString` is held in scope, which lasts past the
     * jail_set call. */
    let c_name        = c_string(spec.name)?;
    let c_path        = c_string(spec.path)?;
    let mut errmsg    = vec![0u8; 256];

    /* Field-name C strings. These can be 'static — they're string
     * literals — but we still need NUL termination, which Rust
     * string literals don't have. Use `CString::new` to add the
     * NUL once at startup. */
    let key_name         = c_string("name")?;
    let key_path         = c_string("path")?;
    let key_persist      = c_string("persist")?;
    let key_children_max = c_string("children.max")?;
    let key_errmsg       = c_string("errmsg")?;

    let persist = spec.persist;
    let cmax    = spec.children_max;

    /* Build the iovec array. Each (key, value) pair consumes 2
     * iovec slots. iov_len includes the trailing NUL for strings,
     * and is sizeof(T) for scalars. */
    let mut iov: [libc::iovec; 10] = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len:  0,
    }; 10];

    // SAFETY: each `iov_base` points into a CString or local
    // variable that outlives the jail_set call below. iov_len
    // matches each region's size exactly.
    unsafe {
        iov[0].iov_base = key_name.as_ptr() as *mut _;
        iov[0].iov_len  = key_name.as_bytes_with_nul().len();
        iov[1].iov_base = c_name.as_ptr() as *mut _;
        iov[1].iov_len  = c_name.as_bytes_with_nul().len();

        iov[2].iov_base = key_path.as_ptr() as *mut _;
        iov[2].iov_len  = key_path.as_bytes_with_nul().len();
        iov[3].iov_base = c_path.as_ptr() as *mut _;
        iov[3].iov_len  = c_path.as_bytes_with_nul().len();

        iov[4].iov_base = key_persist.as_ptr() as *mut _;
        iov[4].iov_len  = key_persist.as_bytes_with_nul().len();
        iov[5].iov_base = (&persist as *const i32) as *mut _;
        iov[5].iov_len  = std::mem::size_of::<i32>();

        iov[6].iov_base = key_children_max.as_ptr() as *mut _;
        iov[6].iov_len  = key_children_max.as_bytes_with_nul().len();
        iov[7].iov_base = (&cmax as *const i32) as *mut _;
        iov[7].iov_len  = std::mem::size_of::<i32>();

        iov[8].iov_base = key_errmsg.as_ptr() as *mut _;
        iov[8].iov_len  = key_errmsg.as_bytes_with_nul().len();
        iov[9].iov_base = errmsg.as_mut_ptr() as *mut _;
        iov[9].iov_len  = errmsg.len();

        let jid = jail_set(iov.as_mut_ptr(), iov.len() as u32, JAIL_CREATE);
        if jid < 0 {
            // Trim errmsg to the C NUL if any.
            let nul = errmsg.iter().position(|&b| b == 0).unwrap_or(errmsg.len());
            errmsg.truncate(nul);
            let extra = String::from_utf8_lossy(&errmsg).into_owned();
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("jail_set: {} (kernel: {extra})",
                    io::Error::last_os_error()),
            ));
        }
        Ok(CreatedJail { jid })
    }
}

/// Tear down a jail by jid. Idempotent at the wrapper level: ENOENT
/// (jid already gone) is folded to `Ok`. Other errors propagate.
pub fn remove_jail(jid: i32) -> io::Result<()> {
    // SAFETY: jail_remove takes only an integer. No memory aliasing.
    let rc = unsafe { jail_remove(jid) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if let Some(libc::ENOENT) = e.raw_os_error() {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

fn c_string(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("nul in string: {e}"))
    })
}

/// Get the peer UID for a connected unix socket. FreeBSD path uses
/// `getpeereid(3)` from libc. macOS host path (for `cargo test`)
/// returns 0; jaild only runs in production on FreeBSD.
#[cfg(target_os = "freebsd")]
pub fn getpeereid(fd: i32) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: caller passes an open socket fd; getpeereid only
    // writes the two out parameters. uid/gid are stack locals.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(target_os = "freebsd"))]
pub fn getpeereid(_fd: i32) -> io::Result<u32> {
    Ok(0)
}

// ====================================================================
// V1a additions: pdfork, jail_attach (via JAIL_ATTACH flag), nmount,
// SCM_RIGHTS for procdesc fd hand-back.
// ====================================================================

/// Result of `pdfork`. In the parent, `pid > 0` and `procdesc_fd`
/// is the fd. In the child, `pid == 0` and `procdesc_fd == -1`.
pub struct PdforkOutcome {
    pub pid:          libc::pid_t,
    pub procdesc_fd:  libc::c_int,
}

#[cfg(target_os = "freebsd")]
pub fn pdfork() -> io::Result<PdforkOutcome> {
    let mut fd: libc::c_int = -1;
    // SAFETY: pdfork writes to *fd in the parent, leaves it
    // alone in the child. fd lives on this stack frame.
    let pid = unsafe { libc::pdfork(&mut fd, 0) };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PdforkOutcome { pid, procdesc_fd: fd })
}

#[cfg(not(target_os = "freebsd"))]
pub fn pdfork() -> io::Result<PdforkOutcome> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "pdfork: FreeBSD only"))
}

/// Apply a JAIL_CREATE | JAIL_ATTACH inside the (pdfork-)child
/// process. Same iovec construction as `create_persistent_jail`,
/// just different flags + no `persist` field.
pub fn jail_create_and_attach(spec: &JailCreateSpec) -> io::Result<i32> {
    let c_name        = c_string(spec.name)?;
    let c_path        = c_string(spec.path)?;
    let mut errmsg    = vec![0u8; 256];

    let key_name         = c_string("name")?;
    let key_path         = c_string("path")?;
    let key_children_max = c_string("children.max")?;
    let key_errmsg       = c_string("errmsg")?;

    let cmax = spec.children_max;

    let mut iov: [libc::iovec; 8] = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len:  0,
    }; 8];

    // SAFETY: same as create_persistent_jail.
    unsafe {
        iov[0].iov_base = key_name.as_ptr() as *mut _;
        iov[0].iov_len  = key_name.as_bytes_with_nul().len();
        iov[1].iov_base = c_name.as_ptr() as *mut _;
        iov[1].iov_len  = c_name.as_bytes_with_nul().len();

        iov[2].iov_base = key_path.as_ptr() as *mut _;
        iov[2].iov_len  = key_path.as_bytes_with_nul().len();
        iov[3].iov_base = c_path.as_ptr() as *mut _;
        iov[3].iov_len  = c_path.as_bytes_with_nul().len();

        iov[4].iov_base = key_children_max.as_ptr() as *mut _;
        iov[4].iov_len  = key_children_max.as_bytes_with_nul().len();
        iov[5].iov_base = (&cmax as *const i32) as *mut _;
        iov[5].iov_len  = std::mem::size_of::<i32>();

        iov[6].iov_base = key_errmsg.as_ptr() as *mut _;
        iov[6].iov_len  = key_errmsg.as_bytes_with_nul().len();
        iov[7].iov_base = errmsg.as_mut_ptr() as *mut _;
        iov[7].iov_len  = errmsg.len();

        let jid = jail_set(iov.as_mut_ptr(), iov.len() as u32,
                           JAIL_CREATE | JAIL_ATTACH);
        if jid < 0 {
            let nul = errmsg.iter().position(|&b| b == 0).unwrap_or(errmsg.len());
            errmsg.truncate(nul);
            let extra = String::from_utf8_lossy(&errmsg).into_owned();
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("jail_set(CREATE|ATTACH): {} (kernel: {extra})",
                    io::Error::last_os_error()),
            ));
        }
        Ok(jid)
    }
}

/// Apply one nullfs mount: `source` is the host path, `target`
/// is the destination (typically inside the jail's path).
/// `read_only` selects MNT_RDONLY.
#[cfg(target_os = "freebsd")]
pub fn nullfs_mount(source: &str, target: &str, read_only: bool) -> io::Result<()> {
    let c_fstype = c_string("nullfs")?;
    let c_fspath = c_string(target)?;
    let c_target = c_string(source)?;

    let key_fstype = c_string("fstype")?;
    let key_fspath = c_string("fspath")?;
    let key_target = c_string("target")?;

    let mut iov: [libc::iovec; 6] = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len:  0,
    }; 6];

    // SAFETY: each iov_base points into a CString that outlives nmount.
    unsafe {
        iov[0].iov_base = key_fstype.as_ptr() as *mut _;
        iov[0].iov_len  = key_fstype.as_bytes_with_nul().len();
        iov[1].iov_base = c_fstype.as_ptr() as *mut _;
        iov[1].iov_len  = c_fstype.as_bytes_with_nul().len();

        iov[2].iov_base = key_fspath.as_ptr() as *mut _;
        iov[2].iov_len  = key_fspath.as_bytes_with_nul().len();
        iov[3].iov_base = c_fspath.as_ptr() as *mut _;
        iov[3].iov_len  = c_fspath.as_bytes_with_nul().len();

        iov[4].iov_base = key_target.as_ptr() as *mut _;
        iov[4].iov_len  = key_target.as_bytes_with_nul().len();
        iov[5].iov_base = c_target.as_ptr() as *mut _;
        iov[5].iov_len  = c_target.as_bytes_with_nul().len();

        let flags = if read_only { libc::MNT_RDONLY } else { 0 };
        let rc = libc::nmount(iov.as_mut_ptr(), iov.len() as u32, flags as i32);
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Apply a tmpfs mount at `target`.
#[cfg(target_os = "freebsd")]
pub fn tmpfs_mount(target: &str) -> io::Result<()> {
    let c_fstype = c_string("tmpfs")?;
    let c_fspath = c_string(target)?;

    let key_fstype = c_string("fstype")?;
    let key_fspath = c_string("fspath")?;

    let mut iov: [libc::iovec; 4] = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len:  0,
    }; 4];

    // SAFETY: each iov_base points into a CString that outlives nmount.
    unsafe {
        iov[0].iov_base = key_fstype.as_ptr() as *mut _;
        iov[0].iov_len  = key_fstype.as_bytes_with_nul().len();
        iov[1].iov_base = c_fstype.as_ptr() as *mut _;
        iov[1].iov_len  = c_fstype.as_bytes_with_nul().len();

        iov[2].iov_base = key_fspath.as_ptr() as *mut _;
        iov[2].iov_len  = key_fspath.as_bytes_with_nul().len();
        iov[3].iov_base = c_fspath.as_ptr() as *mut _;
        iov[3].iov_len  = c_fspath.as_bytes_with_nul().len();

        let rc = libc::nmount(iov.as_mut_ptr(), iov.len() as u32, 0);
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn nullfs_mount(_s: &str, _t: &str, _ro: bool) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "nmount: FreeBSD only"))
}
#[cfg(not(target_os = "freebsd"))]
pub fn tmpfs_mount(_t: &str) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "nmount: FreeBSD only"))
}

/// Drop privileges to (gid, uid) in the calling process.
/// Order matters: setgid before setuid.
pub fn drop_privileges(uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: setgid/setuid are leaf syscalls with no memory.
    unsafe {
        if libc::setgid(gid) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::setuid(uid) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Replace the current process image with `path`. `argv` and
/// `env` are owned `Vec<CString>`; their pointers are written into
/// arrays of `*const c_char` and the syscall is invoked. On
/// success, never returns.
pub fn execve(
    path: &str,
    argv: &[String],
    env:  &[(String, String)],
) -> io::Result<std::convert::Infallible> {
    let c_path  = c_string(path)?;
    let c_argv: Vec<CString> = argv.iter()
        .map(|s| c_string(s))
        .collect::<io::Result<_>>()?;
    let c_env: Vec<CString> = env.iter()
        .map(|(k, v)| c_string(&format!("{k}={v}")))
        .collect::<io::Result<_>>()?;

    let mut argv_ptrs: Vec<*const libc::c_char> =
        c_argv.iter().map(|s| s.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    let mut env_ptrs: Vec<*const libc::c_char> =
        c_env.iter().map(|s| s.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    // SAFETY: argv_ptrs / env_ptrs are NULL-terminated and point
    // into c_argv / c_env CStrings that outlive the call. execve
    // either replaces the process or returns -1; on success it
    // never returns.
    unsafe {
        libc::execve(c_path.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(io::Error::last_os_error())
}

/// Send a frame (length-prefixed body) over `socket_fd`, optionally
/// attaching one fd via SCM_RIGHTS. Single `sendmsg` so the body
/// and ancillary cmsg arrive atomically.
pub fn send_frame_with_optional_fd(
    socket_fd: i32,
    body:      &[u8],
    fd:        Option<i32>,
) -> io::Result<()> {
    /* Build the framed payload (4-byte LE length + body). */
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_le_bytes());
    framed.extend_from_slice(body);

    let mut iov = libc::iovec {
        iov_base: framed.as_mut_ptr() as *mut _,
        iov_len:  framed.len(),
    };

    // SAFETY: CMSG_SPACE is a libc macro/inline; safe to invoke.
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) };
    let mut cmsg_buf: Vec<u8> = vec![0u8; cmsg_space as usize];

    let mut msg = libc::msghdr {
        msg_name:        std::ptr::null_mut(),
        msg_namelen:     0,
        msg_iov:         &mut iov,
        msg_iovlen:      1,
        msg_control:     std::ptr::null_mut(),
        msg_controllen:  0,
        msg_flags:       0,
    };

    if let Some(fd_to_send) = fd {
        msg.msg_control    = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_space as _;
        // SAFETY: cmsg_buf is correctly sized for exactly one int.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(io::Error::new(io::ErrorKind::Other, "CMSG_FIRSTHDR null"));
            }
            (*cmsg).cmsg_len   = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type  = libc::SCM_RIGHTS;
            let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
            data.write_unaligned(fd_to_send);
        }
    }

    // SAFETY: msg fields were initialised above; iov points into
    // the local `framed` Vec which outlives sendmsg.
    let rc = unsafe { libc::sendmsg(socket_fd, &msg, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Close an fd (procdesc, etc.) in the caller. Used by the parent
/// after `send_with_fd` to release its reference.
pub fn close_fd(fd: i32) -> io::Result<()> {
    // SAFETY: caller passes an open fd; close is a leaf syscall.
    let rc = unsafe { libc::close(fd) };
    if rc < 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}

/// `_exit(2)` — call from the pdfork-child after a failed mount
/// or jail_attach. Never returns.
pub fn child_exit(code: i32) -> ! {
    // SAFETY: _exit is the safest call there is; never returns.
    unsafe { libc::_exit(code); }
}
