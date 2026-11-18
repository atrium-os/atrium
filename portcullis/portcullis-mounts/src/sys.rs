//! The mount and jail system calls the teardown and setup paths need,
//! called directly. Each of them used to be a program run per call (mount,
//! umount, jls, jail -r): on a slow machine that was most of a one-shot's
//! time (portcullis.md §9.1c). Off FreeBSD every call says "unsupported"
//! (`None`/`Err`) and callers fall back to the programs.

use std::path::{Path, PathBuf};

/// Every mount point on the machine, from getfsstat(2). `None` if the call
/// failed or isn't here.
#[cfg(target_os = "freebsd")]
pub fn mount_points() -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    /// struct statfs as FreeBSD 12 and later lay it out (STATFS_VERSION
    /// 0x20140518). Declared here because the libc crate, built for a
    /// FreeBSD 11 target, has the old layout.
    #[repr(C)]
    struct Statfs {
        f_version: u32, f_type: u32, f_flags: u64, f_bsize: u64, f_iosize: u64,
        f_blocks: u64, f_bfree: u64, f_bavail: i64, f_files: u64, f_ffree: i64,
        f_syncwrites: u64, f_asyncwrites: u64, f_syncreads: u64, f_asyncreads: u64,
        f_nvnodelistsize: u32, f_spare0: u32, f_spare: [u64; 9], f_namemax: u32,
        f_owner: u32, f_fsid: [i32; 2], f_charspare: [u8; 80], f_fstypename: [u8; 16],
        f_mntfromname: [u8; 1024], f_mntonname: [u8; 1024],
    }
    const SYS_GETFSSTAT: libc::c_long = 557;
    const STATFS_VERSION: u32 = 0x2014_0518;
    for _ in 0..4 {
        // SAFETY: a size query with no buffer.
        let n = unsafe { libc::syscall(SYS_GETFSSTAT as _, std::ptr::null_mut::<Statfs>(), 0 as libc::c_long, libc::MNT_NOWAIT) };
        if n < 0 { return None }
        let cap = n as usize + 8;
        let mut buf: Vec<Statfs> = Vec::with_capacity(cap);
        let bytes = (cap * std::mem::size_of::<Statfs>()) as libc::c_long;
        // SAFETY: buf has room for `cap` entries; the kernel writes at most
        // `bytes` and returns how many it wrote.
        let got = unsafe { libc::syscall(SYS_GETFSSTAT as _, buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        if got < 0 { return None }
        let got = got as usize;
        if got >= cap { continue } // mounts appeared meanwhile: ask again
        // SAFETY: the kernel filled `got` entries.
        unsafe { buf.set_len(got) };
        if buf.iter().any(|s| s.f_version != STATFS_VERSION) { return None }
        return Some(buf.iter().map(|s| {
            let end = s.f_mntonname.iter().position(|b| *b == 0).unwrap_or(s.f_mntonname.len());
            PathBuf::from(std::ffi::OsStr::from_bytes(&s.f_mntonname[..end]))
        }).collect());
    }
    None
}

#[cfg(not(target_os = "freebsd"))]
pub fn mount_points() -> Option<Vec<PathBuf>> { None }

#[cfg(target_os = "freebsd")]
fn cpath(p: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::other("NUL in path"))
}

/// unmount(2), with MNT_FORCE when `force`.
#[cfg(target_os = "freebsd")]
pub fn unmount(p: &Path, force: bool) -> std::io::Result<()> {
    let c = cpath(p)?;
    // SAFETY: c is a NUL-terminated path.
    let r = unsafe { libc::unmount(c.as_ptr(), if force { libc::MNT_FORCE } else { 0 }) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(target_os = "freebsd"))]
pub fn unmount(_p: &Path, _force: bool) -> std::io::Result<()> { Err(std::io::ErrorKind::Unsupported.into()) }

/// nmount(2) with string options, as mount(8)'s helpers build them.
#[cfg(target_os = "freebsd")]
pub fn nmount(opts: &[(&str, &str)], flags: i32) -> std::io::Result<()> {
    let mut store: Vec<std::ffi::CString> = Vec::new();
    for (k, v) in opts {
        store.push(std::ffi::CString::new(*k).map_err(|_| std::io::Error::other("NUL in option"))?);
        store.push(std::ffi::CString::new(*v).map_err(|_| std::io::Error::other("NUL in option"))?);
    }
    let mut errmsg = [0u8; 255];
    let mut iov: Vec<libc::iovec> = store.iter().map(|c| libc::iovec {
        iov_base: c.as_ptr() as *mut libc::c_void, iov_len: c.as_bytes_with_nul().len() }).collect();
    let key = c"errmsg";
    iov.push(libc::iovec { iov_base: key.as_ptr() as *mut libc::c_void, iov_len: key.to_bytes_with_nul().len() });
    iov.push(libc::iovec { iov_base: errmsg.as_mut_ptr().cast(), iov_len: errmsg.len() });
    // SAFETY: every iovec points into `store`, `key` or `errmsg`, all alive
    // across the call; the kernel writes only into errmsg.
    let r = unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as u32, flags) };
    if r == 0 { return Ok(()) }
    let e = std::io::Error::last_os_error();
    let end = errmsg.iter().position(|b| *b == 0).unwrap_or(0);
    let what: Vec<String> = opts.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Err(std::io::Error::new(e.kind(), format!("nmount {}: {e}{}{}", what.join(" "),
        if end > 0 { ": " } else { "" }, String::from_utf8_lossy(&errmsg[..end]))))
}

#[cfg(not(target_os = "freebsd"))]
pub fn nmount(_opts: &[(&str, &str)], _flags: i32) -> std::io::Result<()> { Err(std::io::ErrorKind::Unsupported.into()) }

/// MNT_RDONLY, for [`nmount`].
pub const RDONLY: i32 = 0x1;

/// The jid of the jail called `name`, dying ones too when `dying`.
/// `Err` when the call isn't here.
#[cfg(target_os = "freebsd")]
pub fn jail_id(name: &str, dying: bool) -> Result<Option<i32>, ()> {
    let Ok(cname) = std::ffi::CString::new(name) else { return Ok(None) };
    let key = c"name";
    let mut iov = [
        libc::iovec { iov_base: key.as_ptr() as *mut libc::c_void, iov_len: key.to_bytes_with_nul().len() },
        libc::iovec { iov_base: cname.as_ptr() as *mut libc::c_void, iov_len: cname.as_bytes_with_nul().len() },
    ];
    // SAFETY: both entries point at live NUL-terminated buffers the kernel only reads.
    let jid = unsafe { libc::jail_get(iov.as_mut_ptr(), 2, if dying { libc::JAIL_DYING } else { 0 }) };
    Ok((jid > 0).then_some(jid))
}

#[cfg(not(target_os = "freebsd"))]
pub fn jail_id(_name: &str, _dying: bool) -> Result<Option<i32>, ()> { Err(()) }

/// jail_remove(2) of the live jail called `name`: its processes are killed
/// and it is removed. True if there was one.
#[cfg(target_os = "freebsd")]
pub fn jail_remove_by_name(name: &str) -> bool {
    match jail_id(name, false) {
        // SAFETY: plain syscall on a jid.
        Ok(Some(jid)) => unsafe { libc::jail_remove(jid) == 0 },
        _ => false,
    }
}

#[cfg(not(target_os = "freebsd"))]
pub fn jail_remove_by_name(_name: &str) -> bool { false }
