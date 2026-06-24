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
    pub name:           &'a str,
    pub path:           &'a str,
    pub persist:        i32,    // 0 or 1
    pub children_max:   i32,
    /// 0 = inherit host devfs (do NOT pass devfs_ruleset key to
    /// jail_set). Non-zero = pass that ruleset id.
    pub devfs_ruleset:  u32,
    /// IPv4 address for the jail in dotted-quad form (no CIDR).
    /// `None` = pass `ip4=disable` to jail_set; `Some(...)` =
    /// pass `ip4.addr=<addr>`. The host-side alias must already
    /// be present; this is just the jail_set parameter.
    pub ip4_addr:       Option<&'a str>,
}

/// Result of a successful `jail_set(JAIL_CREATE)`. The `jid` is
/// kernel-assigned and stable for the lifetime of the jail.
pub struct CreatedJail {
    pub jid: i32,
}

/// Make a persistent jail. Wraps `jail_set(2)` with `JAIL_CREATE`.
/// Returns the kernel-assigned jail id, or a typed error.
pub fn create_persistent_jail(spec: &JailCreateSpec) -> io::Result<CreatedJail> {
    let mut iob = IovBuilder::new();
    iob.add_string("name", spec.name)?;
    iob.add_string("path", spec.path)?;
    iob.add_i32   ("persist",      spec.persist);
    iob.add_i32   ("children.max", spec.children_max);
    if spec.devfs_ruleset != 0 {
        iob.add_u32("devfs_ruleset", spec.devfs_ruleset);
    }
    iob.add_network(spec.ip4_addr)?;
    let mut errmsg = vec![0u8; 256];
    iob.add_buf("errmsg", &mut errmsg);
    let jid = iob.run(JAIL_CREATE, &errmsg)?;
    Ok(CreatedJail { jid })
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
    let mut iob = IovBuilder::new();
    iob.add_string("name", spec.name)?;
    iob.add_string("path", spec.path)?;
    iob.add_i32   ("children.max", spec.children_max);
    if spec.devfs_ruleset != 0 {
        iob.add_u32("devfs_ruleset", spec.devfs_ruleset);
    }
    iob.add_network(spec.ip4_addr)?;
    let mut errmsg = vec![0u8; 256];
    iob.add_buf("errmsg", &mut errmsg);
    iob.run(JAIL_CREATE | JAIL_ATTACH, &errmsg)
}

/// Tiny builder for the iovec array `jail_set` consumes. Owns
/// the CStrings (so their pointers stay valid until `run`).
/// Conditional pairs are easier with this shape than with fixed
/// stack arrays — still all the action lives in one tight `unsafe`
/// block at the bottom.
struct IovBuilder {
    /* Holders so each CString outlives the syscall. */
    keys:   Vec<CString>,
    vals:   Vec<CString>,        // string values
    i32s:   Vec<i32>,            // i32 values
    u32s:   Vec<u32>,            // u32 values
    bufs:   Vec<Vec<u8>>,        // raw byte values (e.g. struct in_addr)
    /* For each entry, what kind of payload it has. The actual
     * iovec is built inside `run` from these arrays. */
    entries: Vec<Entry>,
}

enum Entry {
    /* Indexes into the holder vectors above. */
    KeyVal { key_idx: usize, val_idx: usize, kind: ValKind },
    KeyBuf { key_idx: usize, buf_ptr: *mut u8, buf_len: usize },
}

#[derive(Clone, Copy)]
enum ValKind { Str, I32, U32, Bytes }

impl IovBuilder {
    fn new() -> Self {
        Self {
            keys: Vec::with_capacity(8),
            vals: Vec::with_capacity(4),
            i32s: Vec::with_capacity(2),
            u32s: Vec::with_capacity(2),
            bufs: Vec::with_capacity(2),
            entries: Vec::with_capacity(8),
        }
    }

    fn add_string(&mut self, key: &str, val: &str) -> io::Result<()> {
        self.keys.push(c_string(key)?);
        self.vals.push(c_string(val)?);
        self.entries.push(Entry::KeyVal {
            key_idx: self.keys.len() - 1,
            val_idx: self.vals.len() - 1,
            kind:    ValKind::Str,
        });
        Ok(())
    }

    fn add_i32(&mut self, key: &str, val: i32) {
        self.keys.push(CString::new(key).expect("static key has nul?"));
        self.i32s.push(val);
        self.entries.push(Entry::KeyVal {
            key_idx: self.keys.len() - 1,
            val_idx: self.i32s.len() - 1,
            kind:    ValKind::I32,
        });
    }

    fn add_u32(&mut self, key: &str, val: u32) {
        self.keys.push(CString::new(key).expect("static key has nul?"));
        self.u32s.push(val);
        self.entries.push(Entry::KeyVal {
            key_idx: self.keys.len() - 1,
            val_idx: self.u32s.len() - 1,
            kind:    ValKind::U32,
        });
    }

    fn add_buf(&mut self, key: &str, buf: &mut [u8]) {
        self.keys.push(CString::new(key).expect("static key has nul?"));
        self.entries.push(Entry::KeyBuf {
            key_idx: self.keys.len() - 1,
            buf_ptr: buf.as_mut_ptr(),
            buf_len: buf.len(),
        });
    }

    /// Add a raw-bytes value (e.g. a `struct in_addr`).
    fn add_bytes(&mut self, key: &str, bytes: Vec<u8>) {
        self.keys.push(CString::new(key).expect("static key has nul?"));
        self.bufs.push(bytes);
        self.entries.push(Entry::KeyVal {
            key_idx: self.keys.len() - 1,
            val_idx: self.bufs.len() - 1,
            kind:    ValKind::Bytes,
        });
    }

    /// Add the network configuration to the iovec. Either
    /// `ip4=disable` (no addr) or `ip4.addr=<struct in_addr>`.
    fn add_network(&mut self, ip4_addr: Option<&str>) -> io::Result<()> {
        match ip4_addr {
            None => {
                /* `ip4=disable`. Value is the integer constant
                 * JAIL_SYS_DISABLE = 0 (per <sys/jail.h>). */
                self.add_i32("ip4", 0);
                Ok(())
            }
            Some(addr) => {
                /* "ip4.addr" expects a struct in_addr (4 bytes
                 * in network byte order). Ipv4Addr::octets()
                 * returns exactly that. */
                let v4: std::net::Ipv4Addr = addr.parse().map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidInput,
                        format!("ip4.addr {addr:?}: {e}"))
                })?;
                self.add_bytes("ip4.addr", v4.octets().to_vec());
                Ok(())
            }
        }
    }

    fn run(&self, flags: i32, errmsg: &[u8]) -> io::Result<i32> {
        let mut iov: Vec<libc::iovec> = Vec::with_capacity(self.entries.len() * 2);
        for entry in &self.entries {
            match *entry {
                Entry::KeyVal { key_idx, val_idx, kind } => {
                    let k = &self.keys[key_idx];
                    iov.push(libc::iovec {
                        iov_base: k.as_ptr() as *mut _,
                        iov_len:  k.as_bytes_with_nul().len(),
                    });
                    match kind {
                        ValKind::Str => {
                            let v = &self.vals[val_idx];
                            iov.push(libc::iovec {
                                iov_base: v.as_ptr() as *mut _,
                                iov_len:  v.as_bytes_with_nul().len(),
                            });
                        }
                        ValKind::I32 => {
                            iov.push(libc::iovec {
                                iov_base: (&self.i32s[val_idx] as *const i32) as *mut _,
                                iov_len:  std::mem::size_of::<i32>(),
                            });
                        }
                        ValKind::U32 => {
                            iov.push(libc::iovec {
                                iov_base: (&self.u32s[val_idx] as *const u32) as *mut _,
                                iov_len:  std::mem::size_of::<u32>(),
                            });
                        }
                        ValKind::Bytes => {
                            let b = &self.bufs[val_idx];
                            iov.push(libc::iovec {
                                iov_base: b.as_ptr() as *mut _,
                                iov_len:  b.len(),
                            });
                        }
                    }
                }
                Entry::KeyBuf { key_idx, buf_ptr, buf_len } => {
                    let k = &self.keys[key_idx];
                    iov.push(libc::iovec {
                        iov_base: k.as_ptr() as *mut _,
                        iov_len:  k.as_bytes_with_nul().len(),
                    });
                    iov.push(libc::iovec {
                        iov_base: buf_ptr as *mut _,
                        iov_len:  buf_len,
                    });
                }
            }
        }
        // SAFETY: each iov_base points into a holder above (CString
        // / scalar / external buffer) that outlives this call.
        let jid = unsafe {
            jail_set(iov.as_mut_ptr(), iov.len() as u32, flags)
        };
        if jid < 0 {
            let trimmed_len = errmsg.iter().position(|&b| b == 0).unwrap_or(errmsg.len());
            let extra = String::from_utf8_lossy(&errmsg[..trimmed_len]).into_owned();
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("jail_set: {} (kernel: {extra})",
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

// ====================================================================
// Memory-governor broker mechanism: signal a jail's processes (Reap)
// and set a jail's RCTL memoryuse cap (SetRctl). The caller (dispatch)
// has already confirmed the jid/name is one jaild itself created and
// that policy permits the action.
// ====================================================================

/// Conservative jail-name check for shellout safety: the rctl rule embeds the
/// name, so refuse anything outside `[A-Za-z0-9._-]` (Atrium jail names are
/// `app-org-...`-shaped). The caller's state lookup already restricts to
/// jaild-created names; this is belt-and-braces against metacharacter injection.
fn is_safe_jail_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name.bytes().all(|b| b.is_ascii_alphanumeric()
            || b == b'.' || b == b'-' || b == b'_')
}

/// Signal every process in jail `jid` with `sig`. jail_attach(2) is one-way, so
/// we fork a child that attaches and `kill(-1, sig)`s (which, inside the jail,
/// reaches exactly that jail's processes — the kernel excludes the caller and
/// init). The parent waits for the child. This is how the governor's cascade
/// (SIGINFO -> SIGTERM -> SIGKILL) reaches a jailed app from outside its jail.
#[cfg(target_os = "freebsd")]
pub fn reap_jail(jid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: fork(); the child calls only async-signal-safe libc functions
    // (jail_attach, kill, _exit) before exiting.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        if unsafe { libc::jail_attach(jid) } != 0 {
            unsafe { libc::_exit(127) };
        }
        let _ = unsafe { libc::kill(-1, sig) };
        unsafe { libc::_exit(0) };
    }
    let mut status: libc::c_int = 0;
    // SAFETY: waiting on our own child pid.
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 127 {
        return Err(io::Error::new(io::ErrorKind::Other,
            format!("jail_attach({jid}) failed in reap child")));
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn reap_jail(_jid: i32, _sig: i32) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "reap_jail: FreeBSD only"))
}

/// Set jail `name`'s RCTL `memoryuse` cap to `mb` MiB with a sigkill action, via
/// rctl(8) — a shellout in the audited ifconfig style (rctl(8) is stable and the
/// call rate is ~1 per jail per federation tick). Requires `kern.racct.enable=1`.
pub fn set_jail_rctl(name: &str, mb: u64) -> io::Result<()> {
    if !is_safe_jail_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("jail name {name:?} contains unsafe characters")));
    }
    let rule = format!("jail:{name}:memoryuse:sigkill={mb}M");
    let out = std::process::Command::new("rctl").arg("-a").arg(&rule).output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(io::Error::new(io::ErrorKind::Other, format!("rctl -a {rule}: {stderr}")))
}

// ====================================================================
// Network: lo0 alias add/remove via ifconfig(8) shellout.
//
// V0 uses Command shellout for simplicity. The alternative —
// SIOCAIFADDR via ioctl on a raw socket — is the kernel-direct path
// but requires more code. ifconfig(8) is stable, audited, and
// invoked exactly the way an operator would do it manually. The
// per-jail rate is ~1 invocation per launch, so process-spawn
// overhead is irrelevant.
// ====================================================================

/// Add a /32 alias on `lo0`. `addr` is in CIDR form
/// (e.g. "127.10.0.5/32"). Idempotent: an EEXIST result (alias
/// already present) is folded to Ok — happens during a relaunch
/// when the previous jail's alias wasn't cleaned up cleanly.
pub fn ifconfig_lo0_alias_add(addr: &str) -> io::Result<()> {
    /* Refuse args that aren't a clean CIDR string — defends
     * against shell-metacharacter injection through a malformed
     * policy / spec. The validator should already have caught
     * this; belt-and-braces. */
    if !is_safe_cidr(addr) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("addr {addr:?} contains unsafe characters")));
    }
    let out = std::process::Command::new("ifconfig")
        .args(["lo0", "inet", addr, "alias"])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    /* "file already exists" → EEXIST → idempotent. ifconfig
     * spelling: "ifconfig: ioctl SIOCAIFADDR: File exists" */
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("File exists") || stderr.contains("EEXIST") {
        return Ok(());
    }
    Err(io::Error::new(io::ErrorKind::Other,
        format!("ifconfig lo0 inet {addr} alias: {stderr}")))
}

/// Remove a /32 alias from `lo0`. Idempotent: ENOENT (alias
/// already gone) is folded to Ok.
pub fn ifconfig_lo0_alias_del(addr: &str) -> io::Result<()> {
    if !is_safe_cidr(addr) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("addr {addr:?} contains unsafe characters")));
    }
    let out = std::process::Command::new("ifconfig")
        .args(["lo0", "inet", addr, "-alias"])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Can't assign") || stderr.contains("does not exist") {
        return Ok(());
    }
    Err(io::Error::new(io::ErrorKind::Other,
        format!("ifconfig lo0 inet {addr} -alias: {stderr}")))
}

/// Strip the "/<prefix>" suffix to yield a bare dotted-quad
/// suitable for jail_set's "ip4.addr" parameter. Input must be
/// CIDR-validated already.
pub fn strip_cidr_suffix(addr_with_cidr: &str) -> String {
    addr_with_cidr.split('/').next().unwrap_or(addr_with_cidr).to_string()
}

fn is_safe_cidr(s: &str) -> bool {
    /* Accept exactly digits, dots, and a single slash. No
     * shell metacharacters can sneak through. */
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'/')
        && s.matches('/').count() <= 1
}
