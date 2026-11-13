//! Tessera volumes, read-only (docs/spec/device-assignment.md §4.6).
//!
//! A Tessera stick is untrusted like any other until Tessera's at-rest
//! encryption can tell this machine's own disks apart, so it is never
//! mounted by the kernel's `tessera_fs`: this server reads it through the
//! Tessera core's reader (`tessera/reader.h`, the code the loader boots
//! from), whose on-disk decoders are Rust (`tessera-decode`), in the same
//! jail and capability mode as the FAT server. Read-only, and the committed
//! state only: a journal a crash left behind is not replayed.
//!
//! Owners and modes on the stick belong to another machine's users, so
//! they are not shown: everything is the session user's, as on FAT.
//! Symlinks and special files are left out.

use std::ffi::c_void;

use capsa_fat::{BlockDev, Error, Result};
use tessera_sys as sys;

use crate::backend::{Attr, Backend};

const SECTOR: u64 = 4096;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
/// Entries listed from one directory, at most.
const MAX_ENTRIES: usize = 1 << 20;

/// Does this device start like a Tessera volume (either superblock copy)?
pub fn is_tessera<D: BlockDev>(dev: &mut D) -> bool {
    let mut m = [0u8; 8];
    [0, SECTOR].iter().any(|&o| dev.read_at(o, &mut m).is_ok() && &m == b"TESSERA1")
}

#[derive(Debug, Clone)]
pub struct Node {
    pub ino: u32,
    pub name: String,
    pub attr: Attr,
}

/// The device, at a fixed address for the C reader's callbacks.
struct Io<D> {
    dev: D,
}

extern "C" fn read_block<D: BlockDev>(ctx: *mut c_void, sector: u64, out: *mut u8) -> i32 {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: ctx is the Box<Io<D>> this volume owns for as long as the
        // reader lives; out is a 4096-byte buffer (reader.h's contract).
        let io = unsafe { &mut *(ctx as *mut Io<D>) };
        let buf = unsafe { std::slice::from_raw_parts_mut(out, SECTOR as usize) };
        match sector.checked_mul(SECTOR) {
            Some(off) => io.dev.read_at(off, buf).is_ok(),
            None => false,
        }
    }));
    if matches!(r, Ok(true)) { 0 } else { -1 }
}

pub struct TesseraVolume<D> {
    rd: *mut sys::tessera_reader_t,
    // The device; the reader reaches it through the vtable's ctx.
    _io: Box<Io<D>>,
    // Kept alive: the reader holds a copy of the vtable, whose ctx points
    // into `io`.
    _vt: Box<sys::tessera_block_io_t>,
    root: u32,
    sectors: u64,
}

// SAFETY: the reader and its device are used only through &mut self, one
// thread at a time; nothing in them is tied to the thread that made them.
unsafe impl<D: Send> Send for TesseraVolume<D> {}

impl<D: BlockDev> TesseraVolume<D> {
    pub fn open(dev: D) -> Result<Self> {
        let sectors = dev.len() / SECTOR;
        let mut io = Box::new(Io { dev });
        let vt = Box::new(sys::tessera_block_io_t {
            read_block: Some(read_block::<D>),
            write_block: None,
            alloc: None,
            free: None,
            ctx: &mut *io as *mut Io<D> as *mut c_void,
            reader_enter: None,
            reader_exit: None,
            root_published: None,
        });
        // SAFETY: vt is a valid vtable whose ctx outlives the reader.
        let rd = unsafe { sys::tessera_reader_open(&*vt) };
        if rd.is_null() {
            return Err(Error::NotFat("not a readable Tessera volume"));
        }
        // SAFETY: rd is live.
        let root = unsafe { sys::tessera_reader_root_ino(rd) };
        Ok(Self { rd, _io: io, _vt: vt, root, sectors })
    }

    fn stat(&mut self, ino: u32) -> Result<Attr> {
        let (mut mode, mut size, mut mt, mut ct, mut nl) = (0u32, 0u64, 0u64, 0u64, 0u32);
        // SAFETY: rd is live; every out pointer is a local.
        let rc = unsafe { sys::tessera_reader_stat_ex(self.rd, ino, &mut mode, &mut size, &mut mt, &mut ct, &mut nl) };
        if rc != sys::TESSERA_OK {
            return Err(if rc == sys::TESSERA_ENOENT { Error::NotFound } else { Error::Io(format!("tessera error {rc}")) });
        }
        let secs = |ns: u64| i64::try_from(ns / 1_000_000_000).unwrap_or(0);
        Ok(Attr { size, dir: mode & S_IFMT == S_IFDIR, read_only: true, mtime: secs(mt), ctime: secs(ct), atime: secs(mt) })
    }

    fn kind_ok(&mut self, ino: u32) -> Option<Attr> {
        let (mut mode, mut size) = (0u32, 0u64);
        // SAFETY: as in stat.
        let rc = unsafe { sys::tessera_reader_stat_ex(self.rd, ino, &mut mode, &mut size, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if rc != sys::TESSERA_OK || !matches!(mode & S_IFMT, S_IFDIR | S_IFREG) {
            return None;
        }
        self.stat(ino).ok()
    }

    /// The entries of `dir` as (child inode, name), names that cannot be
    /// file names here left out.
    fn entries(&mut self, dir: u32) -> Result<Vec<(u32, String)>> {
        struct Acc(Vec<(u32, String)>);
        extern "C" fn each(ctx: *mut c_void, child: u64, name: *const std::ffi::c_char, nlen: u16) -> i32 {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: ctx is the Acc below, live for the call; name has
                // nlen bytes (reader.h).
                let acc = unsafe { &mut *(ctx as *mut Acc) };
                let bytes = unsafe { std::slice::from_raw_parts(name as *const u8, usize::from(nlen)) };
                if acc.0.len() >= MAX_ENTRIES {
                    return 1;
                }
                if let (Ok(n), Ok(c)) = (std::str::from_utf8(bytes), u32::try_from(child)) {
                    if !n.is_empty() && !n.contains('/') && !n.contains('\0') && n != "." && n != ".." {
                        acc.0.push((c, n.to_string()));
                    }
                }
                0
            }));
            r.unwrap_or(1)
        }
        let mut acc = Acc(Vec::new());
        // SAFETY: rd is live; acc outlives the call.
        let rc = unsafe { sys::tessera_reader_foreach(self.rd, dir, each, &mut acc as *mut Acc as *mut c_void) };
        if rc != sys::TESSERA_OK {
            return Err(Error::NotFound);
        }
        Ok(acc.0)
    }

    /// The 4 KiB sectors the device holds, for statfs.
    pub fn sectors(&self) -> u64 {
        self.sectors
    }
}

impl<D> Drop for TesseraVolume<D> {
    fn drop(&mut self) {
        // SAFETY: rd came from tessera_reader_open and is closed once.
        unsafe { sys::tessera_reader_close(self.rd) };
    }
}

impl<D: BlockDev> Backend for TesseraVolume<D> {
    type Dir = u32;
    type Node = Node;
    type Key = u32;

    fn root(&self) -> u32 {
        self.root
    }
    fn key(n: &Node) -> u32 {
        n.ino
    }
    fn name(n: &Node) -> &str {
        &n.name
    }
    fn attr(n: &Node) -> Attr {
        n.attr
    }
    fn as_dir(&self, n: &Node) -> Result<u32> {
        if n.attr.dir { Ok(n.ino) } else { Err(Error::NotDir) }
    }
    fn list(&mut self, d: u32) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        for (ino, name) in self.entries(d)? {
            if let Some(attr) = self.kind_ok(ino) {
                out.push(Node { ino, name, attr });
            }
        }
        Ok(out)
    }
    fn lookup(&mut self, d: u32, name: &str) -> Result<Node> {
        let (ino, name) = self.entries(d)?.into_iter().find(|(_, n)| n == name).ok_or(Error::NotFound)?;
        let attr = self.kind_ok(ino).ok_or(Error::NotFound)?;
        Ok(Node { ino, name, attr })
    }
    fn refresh(&mut self, k: u32) -> Result<Node> {
        let attr = self.stat(k)?;
        Ok(Node { ino: k, name: String::new(), attr })
    }
    fn read(&mut self, n: &Node, off: u64, buf: &mut [u8]) -> Result<usize> {
        if n.attr.dir {
            return Err(Error::IsDir);
        }
        let mut got = 0usize;
        // SAFETY: rd is live; buf is valid for buf.len() bytes.
        let rc = unsafe { sys::tessera_reader_pread(self.rd, n.ino, off, buf.as_mut_ptr() as *mut c_void, buf.len(), &mut got) };
        if rc != sys::TESSERA_OK {
            return Err(Error::Io(format!("tessera error {rc}")));
        }
        Ok(got.min(buf.len()))
    }
    fn statfs(&mut self) -> Result<(u32, u64, u64)> {
        Ok((SECTOR as u32, self.sectors, 0))
    }
}
