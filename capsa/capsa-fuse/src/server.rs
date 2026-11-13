//! The request loop: one request read from `/dev/fuse`, one reply written,
//! single-threaded (removable media is slow; the kernel queues).
//!
//! One volume can be served on several mounts (device-assignment.md §4.8):
//! the session's own, and a grant for an app, rooted at a folder and owned
//! by the app's uid. dispensatord adds a grant by passing an already
//! mounted `/dev/fuse` descriptor over the control socket; every mount
//! reads and writes the same open volume, so the stick has one writer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use capsa_fat::Error;

use crate::backend::{Attr, Backend};
use crate::proto::{self, cstr, u32le, u64le, FAttr, Out, Request};

/// The largest write the kernel may send us (and read we answer).
pub const MAX_WRITE: u32 = 128 * 1024;
const BUF: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Options {
    /// Owner shown for every file (the session user).
    pub uid: u32,
    pub gid: u32,
    /// Refuse every change even when the volume could take it.
    pub read_only: bool,
    /// How long the kernel may cache names and attributes, seconds.
    pub valid_secs: u64,
    /// Permission bits cleared from every mode shown: `0o077` makes the
    /// volume the owner's alone (with `default_permissions`, the kernel
    /// enforces it).
    pub umask: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self { uid: 0, gid: 0, read_only: false, valid_secs: 1, umask: 0 }
    }
}

/// One volume behind several mounts: each mount's state holds a handle.
pub struct Shared<B>(Rc<RefCell<B>>);

impl<B> Clone for Shared<B> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<B: Backend> Backend for Shared<B> {
    type Dir = B::Dir;
    type Node = B::Node;
    type Key = B::Key;
    fn root(&self) -> B::Dir {
        self.0.borrow().root()
    }
    fn key(n: &B::Node) -> B::Key {
        B::key(n)
    }
    fn name(n: &B::Node) -> &str {
        B::name(n)
    }
    fn attr(n: &B::Node) -> Attr {
        B::attr(n)
    }
    fn as_dir(&self, n: &B::Node) -> capsa_fat::Result<B::Dir> {
        self.0.borrow().as_dir(n)
    }
    fn list(&mut self, d: B::Dir) -> capsa_fat::Result<Vec<B::Node>> {
        self.0.borrow_mut().list(d)
    }
    fn lookup(&mut self, d: B::Dir, name: &str) -> capsa_fat::Result<B::Node> {
        self.0.borrow_mut().lookup(d, name)
    }
    fn refresh(&mut self, k: B::Key) -> capsa_fat::Result<B::Node> {
        self.0.borrow_mut().refresh(k)
    }
    fn read(&mut self, n: &B::Node, off: u64, buf: &mut [u8]) -> capsa_fat::Result<usize> {
        self.0.borrow_mut().read(n, off, buf)
    }
    fn statfs(&mut self) -> capsa_fat::Result<(u32, u64, u64)> {
        self.0.borrow_mut().statfs()
    }
    fn writable(&self) -> bool {
        self.0.borrow().writable()
    }
    fn create(&mut self, d: B::Dir, name: &str, dir: bool, now: i64) -> capsa_fat::Result<B::Node> {
        self.0.borrow_mut().create(d, name, dir, now)
    }
    fn write(&mut self, n: &mut B::Node, off: u64, data: &[u8], now: i64) -> capsa_fat::Result<usize> {
        self.0.borrow_mut().write(n, off, data, now)
    }
    fn truncate(&mut self, n: &mut B::Node, len: u64, now: i64) -> capsa_fat::Result<()> {
        self.0.borrow_mut().truncate(n, len, now)
    }
    fn set_mtime(&mut self, n: &mut B::Node, t: i64) -> capsa_fat::Result<()> {
        self.0.borrow_mut().set_mtime(n, t)
    }
    fn unlink(&mut self, n: &B::Node) -> capsa_fat::Result<()> {
        self.0.borrow_mut().unlink(n)
    }
    fn rmdir(&mut self, n: &B::Node) -> capsa_fat::Result<()> {
        self.0.borrow_mut().rmdir(n)
    }
    fn rename(&mut self, n: &B::Node, to: B::Dir, name: &str, now: i64) -> capsa_fat::Result<B::Node> {
        self.0.borrow_mut().rename(n, to, name, now)
    }
    fn sync(&mut self) -> capsa_fat::Result<()> {
        self.0.borrow_mut().sync()
    }
}

struct NodeInfo<K> {
    /// `None` for the root.
    key: Option<K>,
    lookups: u64,
    /// Removed or replaced: its slot may hold something else now.
    dead: bool,
}

struct State<B: Backend> {
    fs: B,
    opts: Options,
    nodes: HashMap<u64, NodeInfo<B::Key>>,
    by_key: HashMap<B::Key, u64>,
    next: u64,
    cluster: u32,
    /// Keys this mount removed (`None`) or moved (`Some(new)`), for the
    /// volume's other mounts to follow.
    moved: Vec<(B::Key, Option<B::Key>)>,
}

enum Target<N> {
    Root,
    Node(N),
}

fn errno(e: &Error) -> i32 {
    match e {
        Error::NotFound => libc::ENOENT,
        Error::NotDir => libc::ENOTDIR,
        Error::IsDir => libc::EISDIR,
        Error::Exists => libc::EEXIST,
        Error::NotEmpty => libc::ENOTEMPTY,
        Error::NoSpace => libc::ENOSPC,
        Error::InvalidName => libc::EINVAL,
        Error::NameTooLong => libc::ENAMETOOLONG,
        Error::TooBig => libc::EFBIG,
        Error::Io(m) if m == "read-only" => libc::EROFS,
        Error::Io(_) | Error::Corrupt(_) | Error::NotFat(_) => libc::EIO,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

type R<T> = std::result::Result<T, i32>;

impl<B: Backend> State<B> {
    fn info(&self, id: u64) -> R<&NodeInfo<B::Key>> {
        let n = self.nodes.get(&id).ok_or(libc::ENOENT)?;
        if n.dead {
            return Err(libc::ESTALE);
        }
        Ok(n)
    }

    fn target(&mut self, id: u64) -> R<Target<B::Node>> {
        match self.info(id)?.key {
            None => Ok(Target::Root),
            Some(k) => self.fs.refresh(k).map(Target::Node).map_err(|e| if e == Error::NotFound { libc::ESTALE } else { errno(&e) }),
        }
    }

    fn node(&mut self, id: u64) -> R<B::Node> {
        match self.target(id)? {
            Target::Root => Err(libc::EISDIR),
            Target::Node(n) => Ok(n),
        }
    }

    fn dir(&mut self, id: u64) -> R<B::Dir> {
        match self.target(id)? {
            Target::Root => Ok(self.fs.root()),
            Target::Node(n) => self.fs.as_dir(&n).map_err(|e| errno(&e)),
        }
    }

    /// The id for a node, without a lookup reference.
    fn id_of(&mut self, n: &B::Node) -> u64 {
        let k = B::key(n);
        if let Some(&id) = self.by_key.get(&k) {
            return id;
        }
        self.next += 1;
        let id = self.next;
        self.nodes.insert(id, NodeInfo { key: Some(k), lookups: 0, dead: false });
        self.by_key.insert(k, id);
        id
    }

    fn remember(&mut self, n: &B::Node) -> u64 {
        let id = self.id_of(n);
        if let Some(i) = self.nodes.get_mut(&id) {
            i.lookups += 1;
        }
        id
    }

    fn kill(&mut self, k: B::Key) {
        self.moved.push((k, None));
        self.follow_kill(k);
    }

    fn follow_kill(&mut self, k: B::Key) {
        if let Some(id) = self.by_key.remove(&k) {
            if let Some(i) = self.nodes.get_mut(&id) {
                i.dead = true;
            }
        }
    }

    fn follow_move(&mut self, ok: B::Key, nk: B::Key) {
        if ok == nk {
            return;
        }
        if let Some(id) = self.by_key.remove(&ok) {
            if let Some(i) = self.nodes.get_mut(&id) {
                i.key = Some(nk);
            }
            self.by_key.insert(nk, id);
        }
    }

    fn forget(&mut self, id: u64, n: u64) {
        if id == 1 {
            return;
        }
        let gone = match self.nodes.get_mut(&id) {
            Some(i) => {
                i.lookups = i.lookups.saturating_sub(n);
                i.lookups == 0
            }
            None => false,
        };
        if gone {
            if let Some(i) = self.nodes.remove(&id) {
                if let Some(k) = i.key {
                    if self.by_key.get(&k) == Some(&id) {
                        self.by_key.remove(&k);
                    }
                }
            }
        }
    }

    fn fattr(&self, id: u64, a: Attr) -> FAttr {
        let ro = self.opts.read_only || !self.fs.writable();
        let keep = !self.opts.umask & 0o777;
        let mode = if a.dir {
            libc::S_IFDIR as u32 | (if ro { 0o555 } else { 0o755 } & keep)
        } else {
            libc::S_IFREG as u32 | (if ro || a.read_only { 0o444 } else { 0o644 } & keep)
        };
        FAttr {
            ino: id,
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: a.atime.max(a.mtime),
            mtime: a.mtime,
            ctime: a.mtime.max(a.ctime),
            mode,
            nlink: if a.dir { 2 } else { 1 },
            uid: self.opts.uid,
            gid: self.opts.gid,
            blksize: self.cluster,
        }
    }

    fn attr_of(&mut self, id: u64) -> R<FAttr> {
        let a = match self.target(id)? {
            Target::Root => Attr { size: 0, dir: true, read_only: false, mtime: 0, ctime: 0, atime: 0 },
            Target::Node(n) => B::attr(&n),
        };
        Ok(self.fattr(id, a))
    }

    fn entry(&mut self, n: &B::Node) -> Vec<u8> {
        let id = self.remember(n);
        let a = self.fattr(id, B::attr(n));
        proto::entry_out(&a, self.opts.valid_secs)
    }

    fn writable(&self) -> R<()> {
        if self.opts.read_only || !self.fs.writable() {
            Err(libc::EROFS)
        } else {
            Ok(())
        }
    }

    fn handle(&mut self, r: &Request) -> Option<R<Vec<u8>>> {
        let e = |x: Error| errno(&x);
        let b = r.body;
        let res: R<Vec<u8>> = match r.opcode {
            proto::FORGET => {
                self.forget(r.nodeid, u64le(b, 0).unwrap_or(0));
                return None;
            }
            proto::BATCH_FORGET => {
                let n = u32le(b, 0).unwrap_or(0) as usize;
                for k in 0..n.min(b.len().saturating_sub(8) / 16) {
                    let (Some(id), Some(c)) = (u64le(b, 8 + k * 16), u64le(b, 16 + k * 16)) else { break };
                    self.forget(id, c);
                }
                return None;
            }
            proto::INTERRUPT => return None,
            proto::LOOKUP => (|| {
                let (name, _) = cstr(b).ok_or(libc::EINVAL)?;
                // A grant's root has no parent here: `..` is itself, as in
                // a chroot (the kernel crosses the mount itself anyway).
                if r.nodeid == 1 && (name == "." || name == "..") {
                    let a = self.attr_of(1)?;
                    if let Some(i) = self.nodes.get_mut(&1) {
                        i.lookups += 1;
                    }
                    return Ok(proto::entry_out(&a, self.opts.valid_secs));
                }
                let d = self.dir(r.nodeid)?;
                let n = self.fs.lookup(d, name).map_err(e)?;
                Ok(self.entry(&n))
            })(),
            proto::GETATTR => self.attr_of(r.nodeid).map(|a| proto::attr_out(&a, self.opts.valid_secs)),
            proto::SETATTR => (|| {
                let valid = u32le(b, 0).ok_or(libc::EINVAL)?;
                if valid & (proto::FATTR_SIZE | proto::FATTR_MTIME | proto::FATTR_MTIME_NOW) != 0 {
                    self.writable()?;
                    let mut n = self.node(r.nodeid)?;
                    if valid & proto::FATTR_SIZE != 0 {
                        let size = u64le(b, 16).ok_or(libc::EINVAL)?;
                        self.fs.truncate(&mut n, size, now()).map_err(e)?;
                    }
                    if valid & proto::FATTR_MTIME_NOW != 0 {
                        self.fs.set_mtime(&mut n, now()).map_err(e)?;
                    } else if valid & proto::FATTR_MTIME != 0 {
                        let t = u64le(b, 48).ok_or(libc::EINVAL)? as i64;
                        self.fs.set_mtime(&mut n, t).map_err(e)?;
                    }
                }
                // Mode, owner and atime have nowhere to go on FAT: accepted
                // and dropped, as msdosfs does.
                let _ = (proto::FATTR_MODE, proto::FATTR_UID, proto::FATTR_GID, proto::FATTR_ATIME);
                self.attr_of(r.nodeid).map(|a| proto::attr_out(&a, self.opts.valid_secs))
            })(),
            proto::OPEN | proto::OPENDIR => (|| {
                let flags = u32le(b, 0).ok_or(libc::EINVAL)? as i32;
                if flags & libc::O_ACCMODE != libc::O_RDONLY {
                    self.writable()?;
                }
                self.info(r.nodeid)?;
                Ok(Out::new().u64(0).u32(0).u32(0).0.clone())
            })(),
            proto::READ => (|| {
                let off = u64le(b, 8).ok_or(libc::EINVAL)?;
                let size = u32le(b, 16).ok_or(libc::EINVAL)?.min(MAX_WRITE * 8) as usize;
                let n = self.node(r.nodeid)?;
                let mut buf = vec![0u8; size];
                let got = self.fs.read(&n, off, &mut buf).map_err(e)?;
                buf.truncate(got);
                Ok(buf)
            })(),
            proto::WRITE => (|| {
                self.writable()?;
                let off = u64le(b, 8).ok_or(libc::EINVAL)?;
                let size = u32le(b, 16).ok_or(libc::EINVAL)? as usize;
                let data = b.get(40..40 + size).ok_or(libc::EINVAL)?;
                let mut n = self.node(r.nodeid)?;
                let w = self.fs.write(&mut n, off, data, now()).map_err(e)?;
                Ok(Out::new().u32(w as u32).u32(0).0.clone())
            })(),
            proto::STATFS => (|| {
                let (cs, total, free) = self.fs.statfs().map_err(e)?;
                let mut o = Out::new();
                o.u64(total).u64(free).u64(free).u64(0).u64(0).u32(cs).u32(255).u32(cs).u32(0);
                for _ in 0..6 {
                    o.u32(0);
                }
                Ok(o.0)
            })(),
            proto::RELEASE | proto::FLUSH | proto::FSYNC | proto::FSYNCDIR => {
                if self.fs.writable() && !self.opts.read_only {
                    self.fs.sync().map_err(e).map(|_| Vec::new())
                } else {
                    Ok(Vec::new())
                }
            }
            proto::RELEASEDIR | proto::ACCESS => Ok(Vec::new()),
            proto::MKDIR => (|| {
                self.writable()?;
                let (name, _) = cstr(b.get(8..).ok_or(libc::EINVAL)?).ok_or(libc::EINVAL)?;
                let d = self.dir(r.nodeid)?;
                let n = self.fs.create(d, name, true, now()).map_err(e)?;
                self.fs.sync().map_err(e)?;
                Ok(self.entry(&n))
            })(),
            proto::CREATE => (|| {
                self.writable()?;
                let (name, _) = cstr(b.get(16..).ok_or(libc::EINVAL)?).ok_or(libc::EINVAL)?;
                let d = self.dir(r.nodeid)?;
                let n = self.fs.create(d, name, false, now()).map_err(e)?;
                self.fs.sync().map_err(e)?;
                let mut v = self.entry(&n);
                v.extend_from_slice(&Out::new().u64(0).u32(0).u32(0).0);
                Ok(v)
            })(),
            proto::UNLINK | proto::RMDIR => (|| {
                self.writable()?;
                let (name, _) = cstr(b).ok_or(libc::EINVAL)?;
                let d = self.dir(r.nodeid)?;
                let n = self.fs.lookup(d, name).map_err(e)?;
                if r.opcode == proto::UNLINK { self.fs.unlink(&n) } else { self.fs.rmdir(&n) }.map_err(e)?;
                self.kill(B::key(&n));
                self.fs.sync().map_err(e)?;
                Ok(Vec::new())
            })(),
            proto::RENAME | proto::RENAME2 => (|| {
                self.writable()?;
                let newdir = u64le(b, 0).ok_or(libc::EINVAL)?;
                let (flags, names) = if r.opcode == proto::RENAME2 { (u32le(b, 8).ok_or(libc::EINVAL)?, b.get(16..)) } else { (0, b.get(8..)) };
                let (old, rest) = cstr(names.ok_or(libc::EINVAL)?).ok_or(libc::EINVAL)?;
                let (new, _) = cstr(rest).ok_or(libc::EINVAL)?;
                const NOREPLACE: u32 = 1;
                if flags & !NOREPLACE != 0 {
                    return Err(libc::EINVAL);
                }
                let from = self.dir(r.nodeid)?;
                let to = self.dir(newdir)?;
                let n = self.fs.lookup(from, old).map_err(e)?;
                let replaced = match self.fs.lookup(to, new) {
                    Ok(t) if B::key(&t) != B::key(&n) => {
                        if flags & NOREPLACE != 0 {
                            return Err(libc::EEXIST);
                        }
                        Some(B::key(&t))
                    }
                    _ => None,
                };
                let m = self.fs.rename(&n, to, new, now()).map_err(e)?;
                if let Some(k) = replaced {
                    self.kill(k);
                }
                // The renamed node keeps its id under its new key.
                let (ok, nk) = (B::key(&n), B::key(&m));
                self.moved.push((ok, Some(nk)));
                self.follow_move(ok, nk);
                self.fs.sync().map_err(e)?;
                Ok(Vec::new())
            })(),
            proto::READDIR => (|| {
                let off = u64le(b, 8).ok_or(libc::EINVAL)?;
                let size = u32le(b, 16).ok_or(libc::EINVAL)? as usize;
                let d = self.dir(r.nodeid)?;
                let list = self.fs.list(d).map_err(e)?;
                let mut o = Out::new();
                let all = 2 + list.len() as u64;
                let mut i = off;
                while i < all {
                    let (ino, dtype, name): (u64, u32, String) = match i {
                        0 => (r.nodeid, libc::DT_DIR as u32, ".".into()),
                        1 => (1, libc::DT_DIR as u32, "..".into()),
                        _ => {
                            let n = &list[(i - 2) as usize];
                            let id = self.id_of(n);
                            (id, if B::attr(n).dir { libc::DT_DIR } else { libc::DT_REG } as u32, B::name(n).to_string())
                        }
                    };
                    if o.0.len() + proto::dirent_len(&name) > size {
                        break;
                    }
                    proto::dirent(&mut o, ino, i + 1, dtype, &name);
                    i += 1;
                }
                Ok(o.0)
            })(),
            proto::DESTROY => {
                let _ = self.fs.sync();
                Ok(Vec::new())
            }
            _ => Err(libc::ENOSYS),
        };
        Some(res)
    }
}

fn init_reply(body: &[u8]) -> Option<Vec<u8>> {
    let major = u32le(body, 0)?;
    let minor = u32le(body, 4)?;
    let readahead = u32le(body, 8)?;
    if major < proto::MAJOR {
        return None;
    }
    let minor = if major > proto::MAJOR { proto::MINOR } else { minor.min(proto::MINOR) };
    let mut o = Out::new();
    o.u32(proto::MAJOR).u32(minor).u32(readahead).u32(0);
    o.u16(16).u16(12).u32(MAX_WRITE);
    o.u32(1).u16(0).u16(0).u32(0);
    for _ in 0..7 {
        o.u32(0);
    }
    if minor < 23 {
        o.0.truncate(24);
    }
    Some(o.0)
}

/// Serve `fs` on `dev` (an open `/dev/fuse` that is already mounted) until
/// the kernel unmounts it. Returns why it stopped.
pub fn serve<B: Backend, F: Read + Write + AsRawFd + 'static>(fs: B, dev: F, opts: Options) -> std::io::Result<()> {
    serve_with(fs, dev, opts, None)
}

trait Dev: Read + Write + AsRawFd {}
impl<T: Read + Write + AsRawFd> Dev for T {}

struct Mount<B: Backend> {
    st: State<Shared<B>>,
    dev: Box<dyn Dev>,
    started: bool,
}

impl<B: Backend> Mount<B> {
    fn new(fs: Shared<B>, dev: Box<dyn Dev>, opts: Options, root: Option<B::Key>) -> Self {
        let cluster = fs.0.borrow_mut().statfs().map(|s| s.0).unwrap_or(4096);
        let mut st = State { fs, opts, nodes: HashMap::new(), by_key: HashMap::new(), next: 1, cluster, moved: Vec::new() };
        st.nodes.insert(1, NodeInfo { key: root, lookups: 1, dead: false });
        if let Some(k) = root {
            st.by_key.insert(k, 1);
        }
        Self { st, dev, started: false }
    }

    /// One request. `Ok(false)`: this mount is over (unmounted).
    fn step(&mut self, buf: &mut [u8]) -> std::io::Result<bool> {
        let n = match self.dev.read(buf) {
            Ok(0) => return Ok(false),
            Ok(n) => n,
            Err(e) => match e.raw_os_error() {
                Some(libc::ENODEV) => return Ok(false), // unmounted
                Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::ENOENT) => return Ok(true),
                _ => return Err(e),
            },
        };
        let Some(r) = proto::parse(&buf[..n]) else { return Ok(true) };
        let out = if r.opcode == proto::INIT {
            self.started = true;
            match init_reply(r.body) {
                Some(b) => proto::reply(r.unique, 0, &b),
                None => proto::reply(r.unique, libc::EPROTO, &[]),
            }
        } else if !self.started {
            proto::reply(r.unique, libc::EIO, &[])
        } else {
            match self.st.handle(&r) {
                None => return Ok(true),
                Some(Ok(b)) => proto::reply(r.unique, 0, &b),
                Some(Err(e)) => proto::reply(r.unique, e, &[]),
            }
        };
        let destroy = r.opcode == proto::DESTROY;
        if let Err(e) = self.dev.write_all(&out) {
            // The request was interrupted and is gone: not our problem.
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(e);
            }
        }
        Ok(!destroy)
    }
}

/// Most grant mounts one server serves at once.
pub const MAX_GRANTS: usize = 64;

/// A grant request from the control socket: `mount UID GID RO PATH`, with
/// the mounted `/dev/fuse` descriptor attached (SCM_RIGHTS).
fn recv_grant(c: &UnixStream) -> std::io::Result<Option<(String, Option<File>)>> {
    let mut data = [0u8; 4096];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
    // Room for one descriptor (and stray extras, which are closed).
    let mut cbuf = [0u64; 16];
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives
    // the call.
    let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
    m.msg_iov = &mut iov;
    m.msg_iovlen = 1;
    m.msg_control = cbuf.as_mut_ptr().cast();
    m.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    // SAFETY: m describes buffers we own.
    let n = unsafe { libc::recvmsg(c.as_raw_fd(), &mut m, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fds: Vec<File> = Vec::new();
    // SAFETY: walking the control buffer the kernel filled, with its macros.
    unsafe {
        let mut h = libc::CMSG_FIRSTHDR(&m);
        while !h.is_null() {
            if (*h).cmsg_level == libc::SOL_SOCKET && (*h).cmsg_type == libc::SCM_RIGHTS {
                let p = libc::CMSG_DATA(h) as *const RawFd;
                let count = ((*h).cmsg_len as usize - (p as usize - h as usize)) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push(File::from_raw_fd(std::ptr::read_unaligned(p.add(i))));
                }
            }
            h = libc::CMSG_NXTHDR(&m, h);
        }
    }
    if n == 0 {
        return Ok(None);
    }
    let line = String::from_utf8_lossy(&data[..n as usize]).trim_end_matches('\n').to_string();
    let fd = if fds.len() == 1 { fds.pop() } else { None };
    Ok(Some((line, fd)))
}

/// `PATH` from the volume's root, one name at a time: a directory's key.
fn resolve<B: Backend>(fs: &mut Shared<B>, path: &str) -> std::result::Result<Option<B::Key>, String> {
    let mut dir = fs.root();
    let mut key = None;
    for name in path.split('/').filter(|n| !n.is_empty()) {
        if name == "." || name == ".." {
            return Err("no . or .. in a grant's path".into());
        }
        let n = fs.lookup(dir, name).map_err(|e| format!("{name}: {e}"))?;
        dir = fs.as_dir(&n).map_err(|_| format!("{name} is not a folder"))?;
        key = Some(B::key(&n));
    }
    Ok(key)
}

/// [`serve`], plus grants arriving on `control` (device-assignment.md
/// §4.8). The main mount ending ends everything; a grant's ending ends
/// only it.
pub fn serve_with<B: Backend, F: Read + Write + AsRawFd + 'static>(fs: B, dev: F, opts: Options, mut control: Option<UnixStream>) -> std::io::Result<()> {
    let fs = Shared(Rc::new(RefCell::new(fs)));
    let base = opts.clone();
    let mut mounts: Vec<Mount<B>> = vec![Mount::new(fs.clone(), Box::new(dev), opts, None)];
    let mut buf = vec![0u8; BUF];
    loop {
        let mut p: Vec<libc::pollfd> = mounts.iter().map(|m| libc::pollfd { fd: m.dev.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        p.push(libc::pollfd { fd: control.as_ref().map_or(-1, |c| c.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        // SAFETY: p is a valid array of pollfds (a negative fd is ignored).
        let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as _, -1) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        let mut over = Vec::new();
        for (i, pf) in p.iter().enumerate().take(mounts.len()) {
            if pf.revents == 0 {
                continue;
            }
            let res = mounts[i].step(&mut buf);
            // What one mount removed or moved, the others see the same way.
            let moved = std::mem::take(&mut mounts[i].st.moved);
            for (j, m) in mounts.iter_mut().enumerate() {
                if j != i {
                    for &(k, nk) in &moved {
                        match nk {
                            None => m.st.follow_kill(k),
                            Some(nk) => m.st.follow_move(k, nk),
                        }
                    }
                }
            }
            match res {
                Ok(true) => {}
                Ok(false) if i == 0 => return Ok(()),
                Ok(false) => over.push(i),
                Err(e) if i == 0 => return Err(e),
                Err(_) => over.push(i),
            }
        }
        for i in over.into_iter().rev() {
            mounts.remove(i);
        }
        if p.last().is_some_and(|c| c.revents != 0) {
            let Some(c) = control.as_ref() else { continue };
            match recv_grant(c) {
                Ok(Some((line, fd))) => {
                    let reply = match grant(&fs, &base, &line, fd, mounts.len()) {
                        Ok(m) => {
                            mounts.push(m);
                            "ok\n".to_string()
                        }
                        Err(e) => format!("error {e}\n"),
                    };
                    let _ = (&*c).write_all(reply.as_bytes());
                }
                // dispensatord went: no more grants, the mounts go on.
                Ok(None) | Err(_) => control = None,
            }
        }
    }
}

fn grant<B: Backend>(fs: &Shared<B>, base: &Options, line: &str, fd: Option<File>, have: usize) -> std::result::Result<Mount<B>, String> {
    let mut w = line.splitn(5, ' ');
    let (Some("mount"), Some(uid), Some(gid), Some(ro), Some(path)) = (w.next(), w.next(), w.next(), w.next(), w.next()) else {
        return Err("expected: mount UID GID RO PATH".into());
    };
    let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else { return Err("bad uid or gid".into()) };
    let fd = fd.ok_or("no /dev/fuse descriptor with the request")?;
    if have > MAX_GRANTS {
        return Err("too many grants".into());
    }
    let root = resolve(&mut fs.clone(), path)?;
    let opts = Options { uid, gid, read_only: base.read_only || ro == "1", valid_secs: base.valid_secs, umask: 0o077 };
    Ok(Mount::new(fs.clone(), Box::new(fd), opts, root))
}
