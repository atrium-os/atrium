//! An SFTP version 3 client (draft-ietf-secsh-filexfer-02, as OpenSSH
//! speaks it) over one capsa-ssh channel. One request at a time; every
//! reply is checked against its request's id and type, and every length
//! against what was asked for.

use std::io::{Read, Write};

use capsa_ssh::wire::{Reader, Writer};
use capsa_ssh::Client;

pub mod fxp {
    pub const INIT: u8 = 1;
    pub const VERSION: u8 = 2;
    pub const OPEN: u8 = 3;
    pub const CLOSE: u8 = 4;
    pub const READ: u8 = 5;
    pub const WRITE: u8 = 6;
    pub const FSTAT: u8 = 8;
    pub const SETSTAT: u8 = 9;
    pub const OPENDIR: u8 = 11;
    pub const READDIR: u8 = 12;
    pub const REMOVE: u8 = 13;
    pub const MKDIR: u8 = 14;
    pub const RMDIR: u8 = 15;
    pub const REALPATH: u8 = 16;
    pub const STAT: u8 = 17;
    pub const RENAME: u8 = 18;
    pub const STATUS: u8 = 101;
    pub const HANDLE: u8 = 102;
    pub const DATA: u8 = 103;
    pub const NAME: u8 = 104;
    pub const ATTRS: u8 = 105;
    pub const EXTENDED: u8 = 200;
    pub const EXTENDED_REPLY: u8 = 201;
}

pub mod status {
    pub const OK: u32 = 0;
    pub const EOF: u32 = 1;
    pub const NO_SUCH_FILE: u32 = 2;
    pub const PERMISSION_DENIED: u32 = 3;
    pub const FAILURE: u32 = 4;
}

pub mod open {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const CREAT: u32 = 8;
    pub const TRUNC: u32 = 0x10;
    pub const EXCL: u32 = 0x20;
}

const A_SIZE: u32 = 1;
const A_UIDGID: u32 = 2;
const A_PERM: u32 = 4;
const A_TIME: u32 = 8;
const A_EXT: u32 = 0x8000_0000;

/// The largest reply we accept, and the most we read or write at once.
const MAX_REPLY: usize = 512 * 1024;
pub const CHUNK: usize = 32 * 1024;
/// Entries per READDIR reply we accept, and names per directory.
const MAX_NAMES: u32 = 4096;
pub const MAX_DIR: usize = 100_000;

#[derive(Debug)]
pub enum Error {
    Ssh(capsa_ssh::Error),
    /// The server's status for a request.
    Status(u32, String),
    /// The server broke SFTP.
    Protocol(&'static str),
}

impl From<capsa_ssh::Error> for Error {
    fn from(e: capsa_ssh::Error) -> Self {
        Error::Ssh(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Ssh(e) => write!(f, "{e}"),
            Error::Status(c, m) => write!(f, "server status {c}: {m}"),
            Error::Protocol(s) => write!(f, "sftp: {s}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub perm: Option<u32>,
    pub mtime: Option<u32>,
    pub atime: Option<u32>,
}

impl Attrs {
    pub fn is_dir(&self) -> bool {
        self.perm.is_some_and(|p| p & 0o170000 == 0o040000)
    }
    pub fn is_file(&self) -> bool {
        self.perm.is_some_and(|p| p & 0o170000 == 0o100000)
    }
}

fn read_attrs(r: &mut Reader<'_>) -> Result<Attrs, capsa_ssh::Error> {
    let flags = r.u32()?;
    let mut a = Attrs::default();
    if flags & A_SIZE != 0 {
        a.size = Some(r.u64()?);
    }
    if flags & A_UIDGID != 0 {
        let _ = (r.u32()?, r.u32()?);
    }
    if flags & A_PERM != 0 {
        a.perm = Some(r.u32()?);
    }
    if flags & A_TIME != 0 {
        a.atime = Some(r.u32()?);
        a.mtime = Some(r.u32()?);
    }
    if flags & A_EXT != 0 {
        let n = r.u32()?;
        if n > 64 {
            return Err(capsa_ssh::Error::Protocol("too many extended attributes"));
        }
        for _ in 0..n {
            let _ = (r.string()?, r.string()?);
        }
    }
    Ok(a)
}

pub struct Sftp<S: Read + Write> {
    ssh: Client<S>,
    next: u32,
    /// posix-rename@openssh.com: rename that replaces, atomically.
    pub posix_rename: bool,
    pub statvfs: bool,
    pub fsync: bool,
}

pub struct Name {
    pub name: String,
    pub attrs: Attrs,
}

impl<S: Read + Write> Sftp<S> {
    /// INIT/VERSION on a channel already running the sftp subsystem.
    pub fn start(mut ssh: Client<S>) -> Result<Self, Error> {
        let mut w = Writer::default();
        w.byte(fxp::INIT).u32(3);
        send_frame(&mut ssh, &w.0)?;
        let v = recv_frame(&mut ssh)?;
        let mut r = Reader::new(&v);
        if r.byte()? != fxp::VERSION {
            return Err(Error::Protocol("expected VERSION"));
        }
        if r.u32()? != 3 {
            return Err(Error::Protocol("server does not speak SFTP 3"));
        }
        let (mut posix_rename, mut statvfs, mut fsync) = (false, false, false);
        let mut n = 0;
        while !r.done() && n < 64 {
            let name = r.utf8()?;
            let _data = r.string()?;
            match name {
                "posix-rename@openssh.com" => posix_rename = true,
                "statvfs@openssh.com" => statvfs = true,
                "fsync@openssh.com" => fsync = true,
                _ => {}
            }
            n += 1;
        }
        Ok(Sftp { ssh, next: 1, posix_rename, statvfs, fsync })
    }

    /// Sends a request and returns its reply's type and body (after the id).
    fn call(&mut self, kind: u8, body: &[u8]) -> Result<(u8, Vec<u8>), Error> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let mut w = Writer::default();
        w.byte(kind).u32(id).raw(body);
        send_frame(&mut self.ssh, &w.0)?;
        let rep = recv_frame(&mut self.ssh)?;
        let mut r = Reader::new(&rep);
        let t = r.byte()?;
        if r.u32()? != id {
            return Err(Error::Protocol("reply for another request"));
        }
        Ok((t, r.rest().to_vec()))
    }

    fn status_of(t: u8, body: &[u8]) -> Result<(), Error> {
        if t != fxp::STATUS {
            return Err(Error::Protocol("expected STATUS"));
        }
        let mut r = Reader::new(body);
        let code = r.u32()?;
        let msg = if r.done() { String::new() } else { r.utf8().unwrap_or("").chars().take(200).collect() };
        if code == status::OK {
            Ok(())
        } else {
            Err(Error::Status(code, msg))
        }
    }

    fn expect_ok(&mut self, kind: u8, body: &[u8]) -> Result<(), Error> {
        let (t, b) = self.call(kind, body)?;
        Self::status_of(t, &b)
    }

    fn handle_reply(t: u8, b: &[u8]) -> Result<Vec<u8>, Error> {
        if t == fxp::STATUS {
            Self::status_of(t, b)?;
            return Err(Error::Protocol("OK status where a handle was due"));
        }
        if t != fxp::HANDLE {
            return Err(Error::Protocol("expected HANDLE"));
        }
        let mut r = Reader::new(b);
        let h = r.string()?;
        if h.is_empty() || h.len() > 256 {
            return Err(Error::Protocol("handle length out of range"));
        }
        Ok(h.to_vec())
    }

    pub fn realpath(&mut self, p: &str) -> Result<String, Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes());
        let (t, b) = self.call(fxp::REALPATH, &w.0)?;
        if t == fxp::STATUS {
            Self::status_of(t, &b)?;
        }
        if t != fxp::NAME {
            return Err(Error::Protocol("expected NAME"));
        }
        let mut r = Reader::new(&b);
        if r.u32()? != 1 {
            return Err(Error::Protocol("REALPATH gave other than one name"));
        }
        Ok(r.utf8()?.to_string())
    }

    pub fn stat(&mut self, p: &str) -> Result<Attrs, Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes());
        let (t, b) = self.call(fxp::STAT, &w.0)?;
        if t == fxp::STATUS {
            Self::status_of(t, &b)?;
        }
        if t != fxp::ATTRS {
            return Err(Error::Protocol("expected ATTRS"));
        }
        Ok(read_attrs(&mut Reader::new(&b))?)
    }

    pub fn list(&mut self, p: &str) -> Result<Vec<Name>, Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes());
        let (t, b) = self.call(fxp::OPENDIR, &w.0)?;
        let h = Self::handle_reply(t, &b)?;
        let mut out = Vec::new();
        let res = (|| -> Result<(), Error> {
            loop {
                let mut w = Writer::default();
                w.string(&h);
                let (t, b) = self.call(fxp::READDIR, &w.0)?;
                if t == fxp::STATUS {
                    let mut r = Reader::new(&b);
                    if r.u32()? == status::EOF {
                        return Ok(());
                    }
                    Self::status_of(t, &b)?;
                    return Err(Error::Protocol("OK status in READDIR"));
                }
                if t != fxp::NAME {
                    return Err(Error::Protocol("expected NAME"));
                }
                let mut r = Reader::new(&b);
                let n = r.u32()?;
                if n > MAX_NAMES {
                    return Err(Error::Protocol("too many names in one reply"));
                }
                for _ in 0..n {
                    let name = r.utf8()?;
                    let _long = r.string()?;
                    let attrs = read_attrs(&mut r)?;
                    if name == "." || name == ".." {
                        continue;
                    }
                    if !valid_name(name) {
                        // A name with '/' or NUL can't be one entry; skip it.
                        continue;
                    }
                    out.push(Name { name: name.to_string(), attrs });
                    if out.len() > MAX_DIR {
                        return Err(Error::Protocol("directory too large"));
                    }
                }
            }
        })();
        let _ = self.close(&h);
        res.map(|_| out)
    }

    pub fn open(&mut self, p: &str, flags: u32) -> Result<Vec<u8>, Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes()).u32(flags);
        if flags & open::CREAT != 0 {
            w.u32(A_PERM).u32(0o600);
        } else {
            w.u32(0);
        }
        let (t, b) = self.call(fxp::OPEN, &w.0)?;
        Self::handle_reply(t, &b)
    }

    pub fn close(&mut self, h: &[u8]) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(h);
        self.expect_ok(fxp::CLOSE, &w.0)
    }

    /// Up to `len` (≤ CHUNK) bytes at `off`; empty at end of file.
    pub fn read(&mut self, h: &[u8], off: u64, len: usize) -> Result<Vec<u8>, Error> {
        let len = len.min(CHUNK);
        let mut w = Writer::default();
        w.string(h).u64(off).u32(len as u32);
        let (t, b) = self.call(fxp::READ, &w.0)?;
        if t == fxp::STATUS {
            let mut r = Reader::new(&b);
            if r.u32()? == status::EOF {
                return Ok(Vec::new());
            }
            Self::status_of(t, &b)?;
            return Err(Error::Protocol("OK status in READ"));
        }
        if t != fxp::DATA {
            return Err(Error::Protocol("expected DATA"));
        }
        let mut r = Reader::new(&b);
        let d = r.string()?;
        if d.len() > len {
            return Err(Error::Protocol("server sent more than was asked"));
        }
        Ok(d.to_vec())
    }

    pub fn write(&mut self, h: &[u8], off: u64, data: &[u8]) -> Result<(), Error> {
        for (i, c) in data.chunks(CHUNK).enumerate() {
            let mut w = Writer::default();
            w.string(h).u64(off + (i * CHUNK) as u64).string(c);
            self.expect_ok(fxp::WRITE, &w.0)?;
        }
        Ok(())
    }

    pub fn fstat(&mut self, h: &[u8]) -> Result<Attrs, Error> {
        let mut w = Writer::default();
        w.string(h);
        let (t, b) = self.call(fxp::FSTAT, &w.0)?;
        if t == fxp::STATUS {
            Self::status_of(t, &b)?;
        }
        if t != fxp::ATTRS {
            return Err(Error::Protocol("expected ATTRS"));
        }
        Ok(read_attrs(&mut Reader::new(&b))?)
    }

    pub fn set_size(&mut self, p: &str, size: u64) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes()).u32(A_SIZE).u64(size);
        self.expect_ok(fxp::SETSTAT, &w.0)
    }

    pub fn set_times(&mut self, p: &str, atime: u32, mtime: u32) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes()).u32(A_TIME).u32(atime).u32(mtime);
        self.expect_ok(fxp::SETSTAT, &w.0)
    }

    pub fn remove(&mut self, p: &str) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes());
        self.expect_ok(fxp::REMOVE, &w.0)
    }

    pub fn mkdir(&mut self, p: &str) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes()).u32(A_PERM).u32(0o700);
        self.expect_ok(fxp::MKDIR, &w.0)
    }

    pub fn rmdir(&mut self, p: &str) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(p.as_bytes());
        self.expect_ok(fxp::RMDIR, &w.0)
    }

    /// Rename that replaces the target when the server can do it atomically
    /// (posix-rename@openssh.com); plain SFTP rename refuses an existing
    /// target.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        let mut w = Writer::default();
        if self.posix_rename {
            w.string(b"posix-rename@openssh.com").string(from.as_bytes()).string(to.as_bytes());
            self.expect_ok(fxp::EXTENDED, &w.0)
        } else {
            w.string(from.as_bytes()).string(to.as_bytes());
            self.expect_ok(fxp::RENAME, &w.0)
        }
    }

    pub fn fsync(&mut self, h: &[u8]) -> Result<(), Error> {
        if !self.fsync {
            return Ok(());
        }
        let mut w = Writer::default();
        w.string(b"fsync@openssh.com").string(h);
        self.expect_ok(fxp::EXTENDED, &w.0)
    }

    /// (block size, blocks, free blocks) via statvfs@openssh.com.
    pub fn statvfs(&mut self, p: &str) -> Result<(u32, u64, u64), Error> {
        if !self.statvfs {
            return Ok((4096, 0, 0));
        }
        let mut w = Writer::default();
        w.string(b"statvfs@openssh.com").string(p.as_bytes());
        let (t, b) = self.call(fxp::EXTENDED, &w.0)?;
        if t == fxp::STATUS {
            Self::status_of(t, &b)?;
        }
        if t != fxp::EXTENDED_REPLY {
            return Err(Error::Protocol("expected EXTENDED_REPLY"));
        }
        let mut r = Reader::new(&b);
        let (bsize, frsize, blocks, _bfree, bavail) = (r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?);
        let unit = if frsize != 0 { frsize } else { bsize };
        let unit = unit.clamp(512, 1 << 20) as u32;
        Ok((unit, blocks, bavail))
    }

    pub fn into_client(self) -> Client<S> {
        self.ssh
    }
}

/// One path component the kernel could have asked for.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && n != "." && n != ".." && !n.contains('/') && !n.contains('\0')
}

fn send_frame<S: Read + Write>(c: &mut Client<S>, body: &[u8]) -> Result<(), capsa_ssh::Error> {
    let mut f = Vec::with_capacity(4 + body.len());
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.extend_from_slice(body);
    c.write_data(&f)
}

fn recv_frame<S: Read + Write>(c: &mut Client<S>) -> Result<Vec<u8>, Error> {
    let l = c.read_exact_data(4)?;
    let n = u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize;
    if n == 0 || n > MAX_REPLY {
        return Err(Error::Protocol("reply length out of range"));
    }
    Ok(c.read_exact_data(n)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_cannot_be_one_component_are_refused() {
        for bad in ["", ".", "..", "a/b", "a\0b", &"x".repeat(256)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name("report.odt"));
    }

    #[test]
    fn attrs_parse_and_bound_extensions() {
        let mut w = Writer::default();
        w.u32(A_SIZE | A_PERM | A_TIME).u64(42).u32(0o100644).u32(1).u32(2);
        let a = read_attrs(&mut Reader::new(&w.0)).unwrap();
        assert_eq!((a.size, a.is_file(), a.mtime), (Some(42), true, Some(2)));
        let mut w = Writer::default();
        w.u32(A_EXT).u32(1_000_000);
        assert!(read_attrs(&mut Reader::new(&w.0)).is_err());
    }
}
