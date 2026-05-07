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
