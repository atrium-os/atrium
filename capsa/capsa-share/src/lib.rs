//! Network share servers (docs/spec/network-shares.md). S1: SFTP.
//!
//! `SftpFs` serves a remote folder through capsa-fuse's `Backend`: nodes
//! are paths under the share's root, interned to stable ids so the kernel's
//! node ids survive renames (a renamed folder's children are re-keyed with
//! it).

pub mod arca;
pub mod sftp;

use std::collections::HashMap;
use std::io::{Read, Write};

use capsa_fat::Error;
use capsa_fuse::{Attr, Backend};

use sftp::{Attrs, Sftp};

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct Node {
    id: u64,
    name: String,
    attrs: Attrs,
}

pub struct SftpFs<S: Read + Write> {
    s: Sftp<S>,
    /// id -> remote path; None once removed. Index 0 is unused, 1 is root.
    paths: Vec<Option<String>>,
    ids: HashMap<String, u64>,
    writable: bool,
    /// One open read handle and one open write handle, by node id.
    rd: Option<(u64, Vec<u8>)>,
    wr: Option<(u64, Vec<u8>)>,
    /// The connection failed: every operation fails from here on.
    dead: bool,
    /// Leave instead: the share then ends for good, and whoever started
    /// the server sees it go (dispensatord reaps it and unmounts).
    exit_on_loss: bool,
}

fn map(e: sftp::Error) -> Error {
    match e {
        sftp::Error::Status(sftp::status::NO_SUCH_FILE, _) => Error::NotFound,
        sftp::Error::Status(sftp::status::PERMISSION_DENIED, _) => Error::Io("permission denied by the server".into()),
        sftp::Error::Status(c, m) => Error::Io(format!("server status {c}: {m}")),
        other => Error::Io(other.to_string()),
    }
}

impl<S: Read + Write> SftpFs<S> {
    /// `root` is the remote folder the share shows (resolved on the server).
    pub fn new(mut s: Sftp<S>, root: &str, writable: bool) -> std::result::Result<Self, sftp::Error> {
        let root = s.realpath(root)?;
        let a = s.stat(&root)?;
        if !a.is_dir() {
            return Err(sftp::Error::Protocol("the share's root is not a folder"));
        }
        let mut ids = HashMap::new();
        ids.insert(root.clone(), 1);
        Ok(SftpFs { s, paths: vec![None, Some(root)], ids, writable, rd: None, wr: None, dead: false, exit_on_loss: false })
    }

    fn path(&self, id: u64) -> Result<&str> {
        self.paths.get(id as usize).and_then(|p| p.as_deref()).ok_or(Error::NotFound)
    }

    fn intern(&mut self, p: String) -> u64 {
        if let Some(&id) = self.ids.get(&p) {
            return id;
        }
        let id = self.paths.len() as u64;
        self.paths.push(Some(p.clone()));
        self.ids.insert(p, id);
        id
    }

    fn child(&self, d: u64, name: &str) -> Result<String> {
        if !sftp::valid_name(name) {
            return Err(Error::InvalidName);
        }
        let dp = self.path(d)?;
        Ok(if dp.ends_with('/') { format!("{dp}{name}") } else { format!("{dp}/{name}") })
    }

    /// A lost connection ends the process (exit status 4) instead of
    /// leaving a share that fails every request.
    pub fn exit_on_loss(mut self) -> Self {
        self.exit_on_loss = true;
        self
    }

    fn live(&self) -> Result<()> {
        if self.dead {
            Err(Error::Io("the connection to the server is gone".into()))
        } else {
            Ok(())
        }
    }

    /// Turns a connection-level failure into a dead share.
    fn chk<T>(&mut self, r: std::result::Result<T, sftp::Error>) -> Result<T> {
        if let Err(e @ (sftp::Error::Ssh(_) | sftp::Error::Protocol(_))) = &r {
            self.dead = true;
            if self.exit_on_loss {
                eprintln!("atrium-share-sftp: the connection is gone ({e}): ending the share");
                std::process::exit(4);
            }
        }
        r.map_err(map)
    }

    fn drop_handles_for(&mut self, id: u64) {
        if self.rd.as_ref().is_some_and(|(k, _)| *k == id) {
            if let Some((_, h)) = self.rd.take() {
                let _ = self.s.close(&h);
            }
        }
        if self.wr.as_ref().is_some_and(|(k, _)| *k == id) {
            if let Some((_, h)) = self.wr.take() {
                let _ = self.s.fsync(&h);
                let _ = self.s.close(&h);
            }
        }
    }

    fn forget(&mut self, id: u64) {
        self.drop_handles_for(id);
        if let Some(Some(p)) = self.paths.get(id as usize).cloned() {
            self.ids.remove(&p);
            let prefix = format!("{p}/");
            let under: Vec<(String, u64)> = self.ids.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| (k.clone(), *v)).collect();
            for (k, v) in under {
                self.ids.remove(&k);
                self.paths[v as usize] = None;
            }
            self.paths[id as usize] = None;
        }
    }

    fn node(&mut self, path: String, name: &str, attrs: Attrs) -> Node {
        let id = self.intern(path);
        Node { id, name: name.to_string(), attrs }
    }
}

impl<S: Read + Write> Backend for SftpFs<S> {
    type Dir = u64;
    type Node = Node;
    type Key = u64;

    fn root(&self) -> u64 {
        1
    }
    fn key(n: &Node) -> u64 {
        n.id
    }
    fn name(n: &Node) -> &str {
        &n.name
    }
    fn attr(n: &Node) -> Attr {
        let t = |v: Option<u32>| i64::from(v.unwrap_or(0));
        Attr {
            size: if n.attrs.is_dir() { 0 } else { n.attrs.size.unwrap_or(0) },
            dir: n.attrs.is_dir(),
            read_only: n.attrs.perm.is_some_and(|p| p & 0o200 == 0),
            mtime: t(n.attrs.mtime),
            ctime: t(n.attrs.mtime),
            atime: t(n.attrs.atime),
        }
    }
    fn as_dir(&self, n: &Node) -> Result<u64> {
        if n.attrs.is_dir() {
            Ok(n.id)
        } else {
            Err(Error::NotDir)
        }
    }
    fn list(&mut self, d: u64) -> Result<Vec<Node>> {
        self.live()?;
        let p = self.path(d)?.to_string();
        let r = self.s.list(&p);
        let names = self.chk(r)?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            // Only files and folders; links, devices and the rest are not
            // shown (a link's target is the server's business, not ours).
            if !(n.attrs.is_dir() || n.attrs.is_file()) {
                continue;
            }
            let cp = self.child(d, &n.name)?;
            out.push(self.node(cp, &n.name, n.attrs));
        }
        Ok(out)
    }
    fn lookup(&mut self, d: u64, name: &str) -> Result<Node> {
        self.live()?;
        let cp = self.child(d, name)?;
        let r = self.s.stat(&cp);
        let a = self.chk(r)?;
        if !(a.is_dir() || a.is_file()) {
            return Err(Error::NotFound);
        }
        Ok(self.node(cp, name, a))
    }
    fn refresh(&mut self, k: u64) -> Result<Node> {
        self.live()?;
        let p = self.path(k)?.to_string();
        let r = self.s.stat(&p);
        let a = self.chk(r)?;
        let name = p.rsplit('/').next().unwrap_or("").to_string();
        Ok(Node { id: k, name, attrs: a })
    }
    fn read(&mut self, n: &Node, off: u64, buf: &mut [u8]) -> Result<usize> {
        self.live()?;
        if n.attrs.is_dir() {
            return Err(Error::IsDir);
        }
        // Reads see this share's own writes: flush the write handle first.
        if self.wr.as_ref().is_some_and(|(k, _)| *k == n.id) {
            self.drop_handles_for(n.id);
        }
        if self.rd.as_ref().map(|(k, _)| *k) != Some(n.id) {
            if let Some((_, h)) = self.rd.take() {
                let _ = self.s.close(&h);
            }
            let p = self.path(n.id)?.to_string();
            let r = self.s.open(&p, sftp::open::READ);
            let h = self.chk(r)?;
            self.rd = Some((n.id, h));
        }
        let h = self.rd.as_ref().unwrap().1.clone();
        let mut done = 0;
        while done < buf.len() {
            let r = self.s.read(&h, off + done as u64, (buf.len() - done).min(sftp::CHUNK));
            let d = self.chk(r)?;
            if d.is_empty() {
                break;
            }
            buf[done..done + d.len()].copy_from_slice(&d);
            done += d.len();
        }
        Ok(done)
    }
    fn statfs(&mut self) -> Result<(u32, u64, u64)> {
        self.live()?;
        let p = self.path(1)?.to_string();
        let r = self.s.statvfs(&p);
        self.chk(r)
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn create(&mut self, d: u64, name: &str, dir: bool, _now: i64) -> Result<Node> {
        self.live()?;
        let cp = self.child(d, name)?;
        if dir {
            let r = self.s.mkdir(&cp);
            if let Err(e) = r {
                // SFTP 3 has no "exists" status: tell it apart by looking.
                if self.s.stat(&cp).is_ok() {
                    return Err(Error::Exists);
                }
                return self.chk(Err(e));
            }
        } else {
            let r = self.s.open(&cp, sftp::open::WRITE | sftp::open::CREAT | sftp::open::EXCL);
            match r {
                Ok(h) => {
                    let _ = self.s.close(&h);
                }
                Err(e) => {
                    if self.s.stat(&cp).is_ok() {
                        return Err(Error::Exists);
                    }
                    return self.chk(Err(e));
                }
            }
        }
        let r = self.s.stat(&cp);
        let a = self.chk(r)?;
        Ok(self.node(cp, name, a))
    }
    fn write(&mut self, n: &mut Node, off: u64, data: &[u8], _now: i64) -> Result<usize> {
        self.live()?;
        if self.rd.as_ref().is_some_and(|(k, _)| *k == n.id) {
            if let Some((_, h)) = self.rd.take() {
                let _ = self.s.close(&h);
            }
        }
        if self.wr.as_ref().map(|(k, _)| *k) != Some(n.id) {
            if let Some((k, _)) = self.wr.as_ref() {
                let k = *k;
                self.drop_handles_for(k);
            }
            let p = self.path(n.id)?.to_string();
            let r = self.s.open(&p, sftp::open::WRITE);
            let h = self.chk(r)?;
            self.wr = Some((n.id, h));
        }
        let h = self.wr.as_ref().unwrap().1.clone();
        let r = self.s.write(&h, off, data);
        self.chk(r)?;
        let end = off + data.len() as u64;
        if n.attrs.size.unwrap_or(0) < end {
            n.attrs.size = Some(end);
        }
        Ok(data.len())
    }
    fn truncate(&mut self, n: &mut Node, len: u64, _now: i64) -> Result<()> {
        self.live()?;
        self.drop_handles_for(n.id);
        let p = self.path(n.id)?.to_string();
        let r = self.s.set_size(&p, len);
        self.chk(r)?;
        n.attrs.size = Some(len);
        Ok(())
    }
    fn set_mtime(&mut self, n: &mut Node, t: i64) -> Result<()> {
        self.live()?;
        self.drop_handles_for(n.id);
        let p = self.path(n.id)?.to_string();
        let t = t.clamp(0, i64::from(u32::MAX)) as u32;
        let r = self.s.set_times(&p, n.attrs.atime.unwrap_or(t), t);
        self.chk(r)?;
        n.attrs.mtime = Some(t);
        Ok(())
    }
    fn unlink(&mut self, n: &Node) -> Result<()> {
        self.live()?;
        self.drop_handles_for(n.id);
        let p = self.path(n.id)?.to_string();
        let r = self.s.remove(&p);
        self.chk(r)?;
        self.forget(n.id);
        Ok(())
    }
    fn rmdir(&mut self, n: &Node) -> Result<()> {
        self.live()?;
        let p = self.path(n.id)?.to_string();
        let r = self.s.rmdir(&p);
        if let Err(e) = r {
            // Tell "not empty" apart, which SFTP 3 can't say.
            if self.s.list(&p).is_ok_and(|l| !l.is_empty()) {
                return Err(Error::NotEmpty);
            }
            return self.chk(Err(e));
        }
        self.forget(n.id);
        Ok(())
    }
    fn rename(&mut self, n: &Node, to: u64, name: &str, _now: i64) -> Result<Node> {
        self.live()?;
        self.drop_handles_for(n.id);
        let from = self.path(n.id)?.to_string();
        let dest = self.child(to, name)?;
        if !self.s.posix_rename {
            // Plain SFTP rename refuses an existing target; replacing needs
            // posix-rename. Refuse rather than delete-then-rename (not atomic).
            if self.s.stat(&dest).is_ok() {
                return Err(Error::Exists);
            }
        }
        let r = self.s.rename(&from, &dest);
        self.chk(r)?;
        // A replaced target's id dies; the moved node keeps its id, and so
        // do its children under the new prefix.
        if let Some(&old) = self.ids.get(&dest) {
            if old != n.id {
                self.forget(old);
            }
        }
        self.ids.remove(&from);
        self.ids.insert(dest.clone(), n.id);
        self.paths[n.id as usize] = Some(dest.clone());
        let prefix = format!("{from}/");
        let under: Vec<(String, u64)> = self.ids.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| (k.clone(), *v)).collect();
        for (k, v) in under {
            let nk = format!("{dest}/{}", &k[prefix.len()..]);
            self.ids.remove(&k);
            self.ids.insert(nk.clone(), v);
            self.paths[v as usize] = Some(nk);
        }
        Ok(Node { id: n.id, name: name.to_string(), attrs: n.attrs })
    }
    fn sync(&mut self) -> Result<()> {
        if let Some((_, h)) = self.wr.take() {
            let r = self.s.fsync(&h);
            let _ = self.s.close(&h);
            self.chk(r)?;
        }
        Ok(())
    }
}
