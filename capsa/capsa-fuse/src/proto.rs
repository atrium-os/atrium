//! The FUSE kernel protocol (major 7), as both FreeBSD's fusefs and Linux
//! speak it: request header, the opcodes this server answers, and replies.
//! Little-endian on every platform Atrium runs on.

pub const MAJOR: u32 = 7;
/// The newest minor we speak. FreeBSD 14/15 offers 7.28, Linux more; we
/// answer with min(kernel's, ours).
pub const MINOR: u32 = 31;

pub const LOOKUP: u32 = 1;
pub const FORGET: u32 = 2;
pub const GETATTR: u32 = 3;
pub const SETATTR: u32 = 4;
pub const MKDIR: u32 = 9;
pub const UNLINK: u32 = 10;
pub const RMDIR: u32 = 11;
pub const RENAME: u32 = 12;
pub const OPEN: u32 = 14;
pub const READ: u32 = 15;
pub const WRITE: u32 = 16;
pub const STATFS: u32 = 17;
pub const RELEASE: u32 = 18;
pub const FSYNC: u32 = 20;
pub const FLUSH: u32 = 25;
pub const INIT: u32 = 26;
pub const OPENDIR: u32 = 27;
pub const READDIR: u32 = 28;
pub const RELEASEDIR: u32 = 29;
pub const FSYNCDIR: u32 = 30;
pub const ACCESS: u32 = 34;
pub const CREATE: u32 = 35;
pub const INTERRUPT: u32 = 36;
pub const DESTROY: u32 = 38;
pub const BATCH_FORGET: u32 = 42;
pub const RENAME2: u32 = 45;

pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_MTIME_NOW: u32 = 1 << 8;

pub const IN_HEADER: usize = 40;
pub const OUT_HEADER: usize = 16;

/// A request: header fields and the body after the header.
#[derive(Debug)]
pub struct Request<'a> {
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub body: &'a [u8],
}

/// `None` unless the buffer holds exactly one well-framed request.
pub fn parse(buf: &[u8]) -> Option<Request<'_>> {
    if buf.len() < IN_HEADER {
        return None;
    }
    let len = u32le(buf, 0)? as usize;
    if len != buf.len() {
        return None;
    }
    Some(Request {
        opcode: u32le(buf, 4)?,
        unique: u64le(buf, 8)?,
        nodeid: u64le(buf, 16)?,
        uid: u32le(buf, 24)?,
        gid: u32le(buf, 28)?,
        body: &buf[IN_HEADER..],
    })
}

pub fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

pub fn u64le(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// One NUL-terminated name at the start of `b`, and what follows it.
pub fn cstr(b: &[u8]) -> Option<(&str, &[u8])> {
    let n = b.iter().position(|&c| c == 0)?;
    Some((std::str::from_utf8(&b[..n]).ok()?, &b[n + 1..]))
}

/// A reply under construction.
pub struct Out(pub Vec<u8>);

impl Out {
    pub fn new() -> Self {
        Out(Vec::with_capacity(256))
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
}

impl Default for Out {
    fn default() -> Self {
        Self::new()
    }
}

/// Header + body, ready for one write(2).
pub fn reply(unique: u64, error: i32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(OUT_HEADER + body.len());
    v.extend_from_slice(&((OUT_HEADER + body.len()) as u32).to_le_bytes());
    v.extend_from_slice(&(-error).to_le_bytes());
    v.extend_from_slice(&unique.to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// `struct fuse_attr` (88 bytes).
#[derive(Debug, Clone, Copy, Default)]
pub struct FAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub blksize: u32,
}

impl FAttr {
    pub fn put(&self, o: &mut Out) {
        o.u64(self.ino).u64(self.size).u64(self.blocks);
        o.u64(self.atime.max(0) as u64).u64(self.mtime.max(0) as u64).u64(self.ctime.max(0) as u64);
        o.u32(0).u32(0).u32(0);
        o.u32(self.mode).u32(self.nlink).u32(self.uid).u32(self.gid).u32(0).u32(self.blksize).u32(0);
    }
}

/// `struct fuse_entry_out` (128 bytes): nodeid, generation, validity, attr.
pub fn entry_out(a: &FAttr, valid_secs: u64) -> Vec<u8> {
    let mut o = Out::new();
    o.u64(a.ino).u64(0).u64(valid_secs).u64(valid_secs).u32(0).u32(0);
    a.put(&mut o);
    o.0
}

/// `struct fuse_attr_out` (104 bytes).
pub fn attr_out(a: &FAttr, valid_secs: u64) -> Vec<u8> {
    let mut o = Out::new();
    o.u64(valid_secs).u32(0).u32(0);
    a.put(&mut o);
    o.0
}

/// One `struct fuse_dirent`, padded to 8 bytes.
pub fn dirent(o: &mut Out, ino: u64, next: u64, dtype: u32, name: &str) {
    o.u64(ino).u64(next).u32(name.len() as u32).u32(dtype).bytes(name.as_bytes());
    while o.0.len() % 8 != 0 {
        o.0.push(0);
    }
}

pub fn dirent_len(name: &str) -> usize {
    (24 + name.len()).div_ceil(8) * 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        let mut b = vec![0u8; 45];
        b[0] = 45;
        b[4] = LOOKUP as u8;
        b[40..45].copy_from_slice(b"abc\0x");
        let r = parse(&b).unwrap();
        assert_eq!((r.opcode, r.body.len()), (LOOKUP, 5));
        assert_eq!(cstr(r.body).map(|x| x.0), Some("abc"));
        b[0] = 44;
        assert!(parse(&b).is_none(), "length must match what was read");
        assert!(parse(&b[..39]).is_none());
        assert!(cstr(b"no nul").is_none());
        let mut o = Out::new();
        FAttr::default().put(&mut o);
        assert_eq!(o.0.len(), 88);
        assert_eq!(entry_out(&FAttr::default(), 1).len(), 128);
        assert_eq!(attr_out(&FAttr::default(), 1).len(), 104);
        let mut o = Out::new();
        dirent(&mut o, 1, 1, 4, "abc");
        assert_eq!(o.0.len(), dirent_len("abc"));
        assert_eq!(reply(9, 2, &[]), [16, 0, 0, 0, 254, 255, 255, 255, 9, 0, 0, 0, 0, 0, 0, 0]);
    }
}
