//! The request loop: one request read from `/dev/fuse`, one reply written,
//! single-threaded (removable media is slow; the kernel queues).

use std::collections::HashMap;
use std::io::{Read, Write};

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
}

impl Default for Options {
    fn default() -> Self {
        Self { uid: 0, gid: 0, read_only: false, valid_secs: 1 }
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
        if let Some(id) = self.by_key.remove(&k) {
            if let Some(i) = self.nodes.get_mut(&id) {
                i.dead = true;
            }
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
        let mode = if a.dir {
            libc::S_IFDIR as u32 | if ro { 0o555 } else { 0o755 }
        } else {
            libc::S_IFREG as u32 | if ro || a.read_only { 0o444 } else { 0o644 }
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
                if let Some(id) = self.by_key.remove(&ok) {
                    if let Some(i) = self.nodes.get_mut(&id) {
                        i.key = Some(nk);
                    }
                    self.by_key.insert(nk, id);
                }
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
pub fn serve<B: Backend, F: Read + Write>(fs: B, mut dev: F, opts: Options) -> std::io::Result<()> {
    let mut st = State { fs, opts, nodes: HashMap::new(), by_key: HashMap::new(), next: 1, cluster: 4096 };
    st.nodes.insert(1, NodeInfo { key: None, lookups: 1, dead: false });
    st.cluster = st.fs.statfs().map(|s| s.0).unwrap_or(4096);
    let mut buf = vec![0u8; BUF];
    let mut started = false;
    loop {
        let n = match dev.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => match e.raw_os_error() {
                Some(libc::ENODEV) => return Ok(()), // unmounted
                Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::ENOENT) => continue,
                _ => return Err(e),
            },
        };
        let Some(r) = proto::parse(&buf[..n]) else { continue };
        let out = if r.opcode == proto::INIT {
            started = true;
            match init_reply(r.body) {
                Some(b) => proto::reply(r.unique, 0, &b),
                None => proto::reply(r.unique, libc::EPROTO, &[]),
            }
        } else if !started {
            proto::reply(r.unique, libc::EIO, &[])
        } else {
            match st.handle(&r) {
                None => continue,
                Some(Ok(b)) => proto::reply(r.unique, 0, &b),
                Some(Err(e)) => proto::reply(r.unique, e, &[]),
            }
        };
        let destroy = r.opcode == proto::DESTROY;
        if let Err(e) = dev.write_all(&out) {
            // The request was interrupted and is gone: not our problem.
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(e);
            }
        }
        if destroy {
            return Ok(());
        }
    }
}
