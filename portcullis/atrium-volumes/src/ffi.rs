//! Localised `unsafe` for atrium-volumes. Just `getpeereid` for
//! the per-connection peer-uid check + `chown(2)` (libc's
//! wrappers in `std::fs` don't cover ownership).

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::path::Path;

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
    // Build-host (macOS); jaild peer check is FreeBSD-only.
    Ok(0)
}

/// `chown(path, uid, gid)`. std::fs has no equivalent.
pub fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: c_path outlives the syscall; uid/gid are scalars.
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set a Tessera per-directory quota limit on `path` (the volume root) via
/// the kmod ioctl `TESSERA_IOC_QUOTA_SET = _IOW('T', 1, uint64_t)`. On
/// FreeBSD that encodes to `_IOC(IN=0x8000_0000, 'T', 1, sizeof(u64)=8)` =
/// `0x8000_0000 | (8 << 16) | (0x54 << 8) | 1` = `0x8008_5401`. Returns
/// `ENOTTY` if `path` isn't on a Tessera mount (or the kmod lacks the op),
/// which the caller treats as "this backend doesn't enforce size_max".
#[cfg(target_os = "freebsd")]
pub fn tessera_set_quota(path: &Path, limit_bytes: u64) -> io::Result<()> {
    const TESSERA_IOC_QUOTA_SET: libc::c_ulong = 0x8008_5401;
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: open a directory read-only for the ioctl; close on every path.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut lim = limit_bytes;
    let rc = unsafe {
        libc::ioctl(fd, TESSERA_IOC_QUOTA_SET, &mut lim as *mut u64)
    };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(err);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn tessera_set_quota(_path: &Path, _limit_bytes: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "tessera quota ioctl is FreeBSD-only",
    ))
}

/// Set the dedup policy of the quota domain rooted at `path` via
/// `TESSERA_IOC_DEDUP_POLICY = _IOW('T', 3, uint64_t)` — encoded the same way
/// as the quota ioctl above, with command 3: `0x8008_5403`.
///
/// `path` must ALREADY be a quota-domain root: the kmod returns `EINVAL`
/// otherwise, because the dedup domain and the quota domain are one record
/// (`tessera-quotas.md` §4.2) and setting a policy on a plain directory would
/// silently retarget whatever domain it inherits — in the common case the
/// default domain, i.e. the whole filesystem. Callers mint the domain with
/// `tessera_set_quota` first (a limit of 0 means unlimited, so a volume with
/// no `size_max` can still have its own domain).
#[cfg(target_os = "freebsd")]
pub fn tessera_set_dedup_policy(path: &Path, policy: u64) -> io::Result<()> {
    const TESSERA_IOC_DEDUP_POLICY: libc::c_ulong = 0x8008_5403;
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: open a directory read-only for the ioctl; close on every path.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pol = policy;
    let rc = unsafe {
        libc::ioctl(fd, TESSERA_IOC_DEDUP_POLICY, &mut pol as *mut u64)
    };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(err);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn tessera_set_dedup_policy(_path: &Path, _policy: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "tessera dedup-policy ioctl is FreeBSD-only",
    ))
}

/// Retire the quota/dedup domain rooted at `path` via
/// `TESSERA_IOC_QUOTA_DETACH = _IO('T', 5)`. `_IO` carries no direction bits
/// and no argument, so it encodes as `IOC_VOID(0x2000_0000) | ('T' << 8) | 5`
/// = `0x2000_5405`.
///
/// Must be called BEFORE the directory is removed — after the removal there is
/// nothing left to name the domain, and the record is orphaned for good (well,
/// until a remount reclaims it). `path` must be the domain's root: a directory
/// that merely inherited the domain gets `EPERM`, and `EINVAL` means it is in
/// no domain at all.
#[cfg(target_os = "freebsd")]
pub fn tessera_detach_quota_domain(path: &Path) -> io::Result<()> {
    const TESSERA_IOC_QUOTA_DETACH: libc::c_ulong = 0x2000_5405;
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: open a directory read-only for the ioctl; close on every path.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::ioctl(fd, TESSERA_IOC_QUOTA_DETACH) };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(err);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn tessera_detach_quota_domain(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "tessera quota-detach ioctl is FreeBSD-only",
    ))
}

// ---------------------------------------------------------------------
// Key groups (tessera-encryption.md §5.4, E4.6): the ioctls in
// atrium-tessera/core/include/tessera/keyring.h, on a directory's
// descriptor, unjailed root only.
// ---------------------------------------------------------------------

/// `struct tessera_ioc_group_info`: what a directory is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct GroupInfo {
    pub volume_uuid: [u8; 16],
    /// The group's key id; zero for no group.
    pub key_id:      [u8; 16],
    /// 0: in no quota domain.
    pub domain_id:   u64,
    /// 0: the domain is in no key group.
    pub group_id:    u64,
    pub flags:       u32,
    /// In: the folder's inode number (st_ino), asked of another directory
    /// of the volume; 0 asks about the directory the ioctl is on.
    pub inode:       u32,
}

impl GroupInfo {
    pub const ROOT: u32 = 0x1;
    pub const OPEN: u32 = 0x2;
    pub const LOCKED: u32 = 0x4;
    pub const BAD: u32 = 0x8;
    pub const NO_RECORD: u32 = 0x10;
    pub fn has(&self, f: u32) -> bool { self.flags & f != 0 }
}

/// `struct tessera_ioc_group`.
#[repr(C)]
struct IocGroup {
    key_id:   [u8; 16],
    group_id: u64,
    suite:    u32,
    pad:      u32,
}

/// `_IOWR('T', 10, struct tessera_ioc_group_info)`: INOUT | 56 << 16 | 'T' << 8 | 10.
pub const TESSERA_IOC_GROUP_INFO: u64 = 0xC038_540A;
/// `_IOW('T', 7, struct tessera_ioc_group)`: IN | 32 << 16 | 'T' << 8 | 7.
pub const TESSERA_IOC_GROUP_CREATE: u64 = 0x8020_5407;
/// `_IOW('T', 8, uint64_t)`.
pub const TESSERA_IOC_QUOTA_GROUP: u64 = 0x8008_5408;
/// `_IOW('T', 9, uint64_t)`.
pub const TESSERA_IOC_GROUP_ERASE: u64 = 0x8008_5409;

#[cfg(target_os = "freebsd")]
fn dir_ioctl<T>(path: &Path, cmd: u64, arg: &mut T) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: open a directory (never through a link) read-only for the
    // ioctl; `arg` is a #[repr(C)] value of the size the command encodes;
    // close on every path.
    let fd = unsafe {
        libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::ioctl(fd, cmd as libc::c_ulong, arg as *mut T) };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(err);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn dir_ioctl<T>(_path: &Path, _cmd: u64, _arg: &mut T) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTTY))
}

/// What `path` (a directory) is: its volume, domain and key group. A
/// locked group's folder can't be opened (tessera-encryption E4.4), so the
/// question goes to its parent, naming the folder by inode number; a mount
/// point is asked directly.
pub fn tessera_group_info(path: &Path) -> io::Result<GroupInfo> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_dir() {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    let mut gi = GroupInfo::default();
    match path.parent() {
        Some(parent) if std::fs::metadata(parent)?.dev() == m.dev() => {
            gi.inode = u32::try_from(m.ino())
                .map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
            dir_ioctl(parent, TESSERA_IOC_GROUP_INFO, &mut gi)?;
        }
        _ => dir_ioctl(path, TESSERA_IOC_GROUP_INFO, &mut gi)?,
    }
    Ok(gi)
}

/// What the directory `path` itself is, asked on its own descriptor
/// (opened `O_DIRECTORY | O_NOFOLLOW`), never through its parent: the
/// answer is about the very folder that is opened, so a link planted in
/// its place cannot redirect it. A locked group's folder can't be opened,
/// so this errors for one (use [`tessera_group_info`] to ask about it).
pub fn tessera_group_info_here(path: &Path) -> io::Result<GroupInfo> {
    let mut gi = GroupInfo::default();
    dir_ioctl(path, TESSERA_IOC_GROUP_INFO, &mut gi)?;
    Ok(gi)
}

/// Make key group `group_id` on the volume under the loaded key `key_id`.
pub fn tessera_group_create(path: &Path, key_id: &[u8; 16], group_id: u64) -> io::Result<()> {
    let mut g = IocGroup { key_id: *key_id, group_id, suite: 0, pad: 0 };
    dir_ioctl(path, TESSERA_IOC_GROUP_CREATE, &mut g)
}

/// Put the (empty) domain rooted at `path` into key group `group_id`.
pub fn tessera_quota_group(path: &Path, group_id: u64) -> io::Result<()> {
    let mut g = group_id;
    dir_ioctl(path, TESSERA_IOC_QUOTA_GROUP, &mut g)
}

/// Crypto-erase locked key group `group_id` (any directory of the volume).
pub fn tessera_group_erase(path: &Path, group_id: u64) -> io::Result<()> {
    let mut g = group_id;
    dir_ioctl(path, TESSERA_IOC_GROUP_ERASE, &mut g)
}

/// `struct tessera_ioc_scrub` (tessera-encryption.md E4.7).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrubReport {
    pub flags:             u32,
    pub pad:               u32,
    pub snapshots_retired: u64,
    pub packs_freed:       u64,
    pub packs_compacted:   u64,
    pub blobs_dropped:     u64,
    /// Partly dead but inside the GC grace period: scrub again later.
    pub packs_young:       u64,
    pub zeroed_sectors:    u64,
    pub journal_blocks:    u64,
}

/// `_IOWR('T', 11, struct tessera_ioc_scrub)`: INOUT | 64 << 16 | 'T' << 8 | 11.
pub const TESSERA_IOC_SCRUB: u64 = 0xC040_540B;
const _: () = assert!(std::mem::size_of::<ScrubReport>() == 64);

/// Scrub the volume `path` (a directory) is on: retire its snapshots,
/// rebuild partly-dead plain packs, zero its free space and its journal's
/// old records. Slow: it writes the whole free space.
pub fn tessera_scrub(path: &Path) -> io::Result<ScrubReport> {
    let mut r = ScrubReport::default();
    dir_ioctl(path, TESSERA_IOC_SCRUB, &mut r)?;
    Ok(r)
}

/// Copy the `user.atrium.origin` attribute (where a download came from)
/// from one open file to another. Absent: nothing to do.
/// Whether anything is mounted at or below `dir`, or mounted from it (a
/// nullfs of it into a jail): moving such a folder would leave the mount
/// on the old copy (E6).
#[cfg(target_os = "freebsd")]
pub fn mounts_under(dir: &Path) -> bool {
    // SAFETY: a null buffer asks for the count only.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return false;
    }
    let mut v: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; n as usize + 8];
    let size = (v.len() * std::mem::size_of::<libc::statfs>()) as libc::c_long;
    // SAFETY: v holds `size` bytes of statfs records.
    let n = unsafe { libc::getfsstat(v.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    if n <= 0 {
        return false;
    }
    let s = |b: &[libc::c_char]| -> std::path::PathBuf {
        std::path::PathBuf::from(b.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect::<String>())
    };
    v[..n as usize].iter().any(|f| s(&f.f_mntonname).starts_with(dir) || s(&f.f_mntfromname).starts_with(dir))
}

#[cfg(not(target_os = "freebsd"))]
pub fn mounts_under(_dir: &Path) -> bool {
    false
}

#[cfg(target_os = "freebsd")]
pub fn copy_origin(from: &std::fs::File, to: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = CString::new("atrium.origin").unwrap();
    let mut buf = [0u8; 4096];
    // SAFETY: descriptors we hold, a NUL-terminated name, a buffer and its length.
    let n = unsafe {
        libc::extattr_get_fd(from.as_raw_fd(), libc::EXTATTR_NAMESPACE_USER,
            name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOATTR) { Ok(()) } else { Err(e) };
    }
    let r = unsafe {
        libc::extattr_set_fd(to.as_raw_fd(), libc::EXTATTR_NAMESPACE_USER,
            name.as_ptr(), buf.as_ptr().cast(), n as usize)
    };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(target_os = "freebsd"))]
pub fn copy_origin(_from: &std::fs::File, _to: &std::fs::File) -> io::Result<()> {
    Ok(())
}
