//! A directory on this machine as a [`Backend`]: how a trusted (sealed)
//! stick's folders reach app jails (tessera-encryption.md §7.2a,
//! device-assignment.md §4.8).
//!
//! The kernel mounts a trusted stick itself, as its person's; there is no
//! stick server to take grants. This one serves the mounted stick's
//! directory instead, from a jail that holds only that directory, as the
//! person's uid, so the kernel's own permission checks still apply beneath
//! it. Every path is opened relative to the root's descriptor and never
//! follows a symbolic link (they and device nodes are not shown); after
//! `cap_enter` (the binary does it) the kernel also refuses any lookup that
//! would leave the root. Grants are FUSE mounts of a folder, owned by the
//! app's uid, as for every other server.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;

use capsa_fat::{Error, Result};

use crate::backend::{Attr, Backend};

#[derive(Debug, Clone)]
pub struct Node {
    ino: u64,
    name: String,
    /// From the root, `/`-separated, empty for the root.
    rel: String,
    attr: Attr,
}

pub struct LocalDir {
    root: OwnedFd,
    root_ino: u64,
    read_only: bool,
    /// Where each inode seen was last found.
    paths: HashMap<u64, String>,
}

fn errno(e: i32) -> Error {
    match e {
        libc::ENOENT => Error::NotFound,
        libc::ENOTDIR => Error::NotDir,
        libc::EISDIR => Error::IsDir,
        libc::EEXIST => Error::Exists,
        libc::ENOTEMPTY => Error::NotEmpty,
        libc::ENOSPC | libc::EDQUOT => Error::NoSpace,
        libc::ENAMETOOLONG => Error::NameTooLong,
        libc::EFBIG => Error::TooBig,
        e => Error::Io(std::io::Error::from_raw_os_error(e).to_string()),
    }
}

fn last() -> Error {
    errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
}

fn cpath(rel: &str) -> Result<CString> {
    CString::new(if rel.is_empty() { "." } else { rel }).map_err(|_| Error::InvalidName)
}

fn ok_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(Error::InvalidName);
    }
    if name.len() > 255 {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

impl LocalDir {
    /// Serve the directory `root` (an open descriptor of it).
    pub fn new(root: OwnedFd, read_only: bool) -> Result<LocalDir> {
        let mut d = LocalDir { root, root_ino: 0, read_only, paths: HashMap::new() };
        let (ino, a) = d.stat("")?.ok_or(Error::NotDir)?;
        if !a.dir {
            return Err(Error::NotDir);
        }
        d.root_ino = ino;
        d.paths.insert(ino, String::new());
        Ok(d)
    }

    pub fn open(path: &str, read_only: bool) -> Result<LocalDir> {
        let c = CString::new(path).map_err(|_| Error::InvalidName)?;
        // SAFETY: a valid path; the descriptor is owned below.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(last());
        }
        // SAFETY: a descriptor we just opened.
        LocalDir::new(unsafe { OwnedFd::from_raw_fd(fd) }, read_only)
    }

    /// `(ino, attr)` of `rel`, not following a link; `None` for anything
    /// but a file or a directory.
    fn stat(&self, rel: &str) -> Result<Option<(u64, Attr)>> {
        let c = cpath(rel)?;
        // SAFETY: a zeroed stat is a valid out-parameter.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor, path and out-parameter.
        if unsafe { libc::fstatat(self.root.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(last());
        }
        let kind = st.st_mode & libc::S_IFMT;
        if kind != libc::S_IFREG && kind != libc::S_IFDIR {
            return Ok(None);
        }
        Ok(Some((
            st.st_ino as u64,
            Attr {
                size: st.st_size as u64,
                dir: kind == libc::S_IFDIR,
                read_only: st.st_mode & 0o200 == 0,
                mtime: st.st_mtime as i64,
                ctime: st.st_ctime as i64,
                atime: st.st_atime as i64,
            },
        )))
    }

    fn node(&mut self, rel: String) -> Result<Node> {
        let (ino, attr) = self.stat(&rel)?.ok_or(Error::NotFound)?;
        let name = rel.rsplit('/').next().unwrap_or("").to_string();
        self.paths.insert(ino, rel.clone());
        Ok(Node { ino, name, rel, attr })
    }

    fn dir_path(&self, d: u64) -> Result<String> {
        self.paths.get(&d).cloned().ok_or(Error::NotFound)
    }

    fn open_at(&self, rel: &str, flags: i32) -> Result<File> {
        let c = cpath(rel)?;
        // SAFETY: a valid descriptor and path; the result is owned below.
        let fd = unsafe { libc::openat(self.root.as_raw_fd(), c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600) };
        if fd < 0 {
            return Err(last());
        }
        // SAFETY: a descriptor we just opened.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn writable_or(&self) -> Result<()> {
        if self.read_only {
            Err(Error::Io("read-only".into()))
        } else {
            Ok(())
        }
    }
}

impl Backend for LocalDir {
    type Dir = u64;
    type Node = Node;
    type Key = u64;

    fn root(&self) -> u64 {
        self.root_ino
    }
    fn key(n: &Node) -> u64 {
        n.ino
    }
    fn name(n: &Node) -> &str {
        &n.name
    }
    fn attr(n: &Node) -> Attr {
        n.attr
    }
    fn as_dir(&self, n: &Node) -> Result<u64> {
        if n.attr.dir {
            Ok(n.ino)
        } else {
            Err(Error::NotDir)
        }
    }
    fn list(&mut self, d: u64) -> Result<Vec<Node>> {
        let dir = self.dir_path(d)?;
        let f = self.open_at(&dir, libc::O_RDONLY | libc::O_DIRECTORY)?;
        // SAFETY: fdopendir takes over the descriptor.
        let dp = unsafe { libc::fdopendir(std::os::fd::IntoRawFd::into_raw_fd(f)) };
        if dp.is_null() {
            return Err(last());
        }
        let mut names = Vec::new();
        loop {
            // SAFETY: dp is a live DIR*.
            let e = unsafe { libc::readdir(dp) };
            if e.is_null() {
                break;
            }
            // SAFETY: d_name is NUL-terminated by the kernel.
            let n = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) };
            if let Ok(s) = n.to_str() {
                if s != "." && s != ".." {
                    names.push(s.to_string());
                }
            }
        }
        // SAFETY: dp is live and closed once.
        unsafe { libc::closedir(dp) };
        let mut out = Vec::new();
        for name in names {
            // Links, devices and the like are left out; so is a name that
            // went away meanwhile.
            if let Ok(n) = self.node(join(&dir, &name)) {
                out.push(n);
            }
        }
        Ok(out)
    }
    fn lookup(&mut self, d: u64, name: &str) -> Result<Node> {
        ok_name(name)?;
        let dir = self.dir_path(d)?;
        self.node(join(&dir, name))
    }
    fn refresh(&mut self, k: u64) -> Result<Node> {
        let rel = self.paths.get(&k).cloned().ok_or(Error::NotFound)?;
        let n = self.node(rel)?;
        if n.ino != k {
            return Err(Error::NotFound);
        }
        Ok(n)
    }
    fn read(&mut self, n: &Node, off: u64, buf: &mut [u8]) -> Result<usize> {
        if n.attr.dir {
            return Err(Error::IsDir);
        }
        let f = self.open_at(&n.rel, libc::O_RDONLY)?;
        let mut got = 0;
        while got < buf.len() {
            match f.read_at(&mut buf[got..], off + got as u64) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) => return Err(errno(e.raw_os_error().unwrap_or(libc::EIO))),
            }
        }
        Ok(got)
    }
    fn statfs(&mut self) -> Result<(u32, u64, u64)> {
        // SAFETY: a zeroed statvfs is a valid out-parameter.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor and out-parameter.
        if unsafe { libc::fstatvfs(self.root.as_raw_fd(), &mut s) } != 0 {
            return Err(last());
        }
        let bs = (s.f_frsize as u64).clamp(512, 1 << 20) as u32;
        Ok((bs, s.f_blocks as u64, s.f_bavail as u64))
    }
    fn writable(&self) -> bool {
        !self.read_only
    }
    fn create(&mut self, d: u64, name: &str, dir: bool, _now: i64) -> Result<Node> {
        self.writable_or()?;
        ok_name(name)?;
        let rel = join(&self.dir_path(d)?, name);
        if dir {
            let c = cpath(&rel)?;
            // SAFETY: a valid descriptor and path.
            if unsafe { libc::mkdirat(self.root.as_raw_fd(), c.as_ptr(), 0o700) } != 0 {
                return Err(last());
            }
        } else {
            drop(self.open_at(&rel, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?);
        }
        self.node(rel)
    }
    fn write(&mut self, n: &mut Node, off: u64, data: &[u8], _now: i64) -> Result<usize> {
        self.writable_or()?;
        let f = self.open_at(&n.rel, libc::O_WRONLY)?;
        f.write_all_at(data, off).map_err(|e| errno(e.raw_os_error().unwrap_or(libc::EIO)))?;
        *n = self.node(n.rel.clone())?;
        Ok(data.len())
    }
    fn truncate(&mut self, n: &mut Node, len: u64, _now: i64) -> Result<()> {
        self.writable_or()?;
        let f = self.open_at(&n.rel, libc::O_WRONLY)?;
        f.set_len(len).map_err(|e| errno(e.raw_os_error().unwrap_or(libc::EIO)))?;
        *n = self.node(n.rel.clone())?;
        Ok(())
    }
    fn set_mtime(&mut self, n: &mut Node, t: i64) -> Result<()> {
        self.writable_or()?;
        let c = cpath(&n.rel)?;
        let ts = [
            libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
            libc::timespec { tv_sec: t as libc::time_t, tv_nsec: 0 },
        ];
        // SAFETY: a valid descriptor, path and two timespecs.
        if unsafe { libc::utimensat(self.root.as_raw_fd(), c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(last());
        }
        *n = self.node(n.rel.clone())?;
        Ok(())
    }
    fn unlink(&mut self, n: &Node) -> Result<()> {
        self.writable_or()?;
        let c = cpath(&n.rel)?;
        // SAFETY: a valid descriptor and path.
        if unsafe { libc::unlinkat(self.root.as_raw_fd(), c.as_ptr(), 0) } != 0 {
            return Err(last());
        }
        self.paths.remove(&n.ino);
        Ok(())
    }
    fn rmdir(&mut self, n: &Node) -> Result<()> {
        self.writable_or()?;
        if n.ino == self.root_ino {
            return Err(Error::Io("the root".into()));
        }
        let c = cpath(&n.rel)?;
        // SAFETY: a valid descriptor and path.
        if unsafe { libc::unlinkat(self.root.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(last());
        }
        self.paths.remove(&n.ino);
        Ok(())
    }
    fn rename(&mut self, n: &Node, to: u64, name: &str, _now: i64) -> Result<Node> {
        self.writable_or()?;
        ok_name(name)?;
        let dest = join(&self.dir_path(to)?, name);
        let (a, b) = (cpath(&n.rel)?, cpath(&dest)?);
        let fd = self.root.as_raw_fd();
        // SAFETY: valid descriptors and paths.
        if unsafe { libc::renameat(fd, a.as_ptr(), fd, b.as_ptr()) } != 0 {
            return Err(last());
        }
        // A directory moved: everything seen under it moves too.
        if n.attr.dir {
            let old = format!("{}/", n.rel);
            for p in self.paths.values_mut() {
                if let Some(rest) = p.strip_prefix(&old) {
                    *p = format!("{dest}/{rest}");
                }
            }
        }
        self.node(dest)
    }
    fn sync(&mut self) -> Result<()> {
        // SAFETY: a valid descriptor.
        unsafe { libc::fsync(self.root.as_raw_fd()) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("capsa-local-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn serves_a_directory_and_nothing_outside_it() {
        let d = tmp();
        std::fs::create_dir(d.join("Docs")).unwrap();
        std::fs::write(d.join("Docs/a.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", d.join("Docs/link")).unwrap();
        let mut l = LocalDir::open(d.to_str().unwrap(), false).unwrap();
        let root = l.root();
        let docs = l.lookup(root, "Docs").unwrap();
        let dd = l.as_dir(&docs).unwrap();
        let names: Vec<String> = l.list(dd).unwrap().iter().map(|n| n.name.clone()).collect();
        assert_eq!(names, ["a.txt"], "the link isn't shown");
        assert!(matches!(l.lookup(dd, "link"), Err(Error::NotFound)));
        assert!(matches!(l.lookup(dd, ".."), Err(Error::InvalidName)));
        assert!(matches!(l.lookup(root, "Docs/a.txt"), Err(Error::InvalidName)));
        let a = l.lookup(dd, "a.txt").unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(l.read(&a, 1, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"ello");

        // Changes land in the directory.
        let mut n = l.create(dd, "new.txt", false, 0).unwrap();
        assert_eq!(l.write(&mut n, 0, b"written", 0).unwrap(), 7);
        assert_eq!(std::fs::read(d.join("Docs/new.txt")).unwrap(), b"written");
        l.truncate(&mut n, 3, 0).unwrap();
        assert_eq!(n.attr.size, 3);
        l.set_mtime(&mut n, 1_000_000).unwrap();
        assert_eq!(n.attr.mtime, 1_000_000);
        let sub = l.create(root, "Sub", true, 0).unwrap();
        let sd = l.as_dir(&sub).unwrap();
        let moved = l.rename(&docs, sd, "Moved", 0).unwrap();
        assert!(d.join("Sub/Moved/a.txt").exists());
        // What was under it is found at its new place.
        assert_eq!(l.refresh(a.ino).unwrap().rel, "Sub/Moved/a.txt");
        let md = l.as_dir(&moved).unwrap();
        let a = l.lookup(md, "a.txt").unwrap();
        l.unlink(&a).unwrap();
        assert!(!d.join("Sub/Moved/a.txt").exists());
        assert!(matches!(l.rmdir(&moved), Err(Error::NotEmpty)));

        // Read-only refuses every change.
        let mut r = LocalDir::open(d.to_str().unwrap(), true).unwrap();
        let rr = r.root();
        assert!(r.create(rr, "x", false, 0).is_err());
        assert!(!d.join("x").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
