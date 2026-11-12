//! The per-origin store behind the CDM's `FileIO` (docs/spec/hospes.md §8.4).
//!
//! A directory fd from the supervisor, limited to lookup, create, read, write, rename and
//! unlink. Nothing else of the filesystem is reachable (capability mode).
//!
//! - A CDM record `NAME` (`[A-Za-z0-9._-]{1,64}`) is the file `r-NAME`. Writes go to
//!   `w-NAME` and are renamed over it, so a crash leaves the old record or the new one.
//!   Writing zero bytes deletes the record (Chromium's FileIO semantics).
//! - `index` lists `NAME SIZE` per line, so the total size and the record count can be
//!   capped without listing the directory: 1 MiB and 64 records.
//! - `storage-id` holds the random per-origin id `RequestStorageId` returns (§14 decision 6).
//!   It cannot collide with a record: records all start with `r-`.
use alloc::vec::Vec;
use hospes_rt::sys;

pub const MAX_TOTAL: usize = 1 << 20;
pub const MAX_RECORDS: usize = 64;
pub const MAX_NAME: usize = 64;

pub fn name_ok(n: &[u8]) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && n.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-')
}

/// `prefix` + `name` + NUL.
fn path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(prefix.len() + name.len() + 1);
    p.extend_from_slice(prefix);
    p.extend_from_slice(name);
    p.push(0);
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Quota,
    Io,
}

pub struct Store {
    dir: i32,
    index: Vec<(Vec<u8>, usize)>,
}

impl Store {
    pub fn open(dir: i32) -> Store {
        let mut s = Store { dir, index: Vec::new() };
        if let Ok(Some(b)) = s.read_file(b"index\0") {
            for line in b.split(|&c| c == b'\n') {
                let mut it = line.splitn(2, |&c| c == b' ');
                let (Some(n), Some(z)) = (it.next(), it.next()) else { continue };
                let z = core::str::from_utf8(z).ok().and_then(|z| z.parse::<usize>().ok());
                if let Some(z) = z.filter(|_| name_ok(n)) {
                    s.index.push((n.to_vec(), z));
                }
            }
        }
        s
    }

    fn read_file(&self, p: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let fd = match sys::open_beneath(self.dir, p, false) {
            Ok(fd) => fd,
            Err(sys::Errno(e)) if e == sys::ENOENT => return Ok(None),
            Err(_) => return Err(Error::Io),
        };
        let mut v = Vec::new();
        let mut chunk = [0u8; 8192];
        let r = loop {
            match sys::read(fd, &mut chunk) {
                Ok(0) => break Ok(Some(v)),
                Ok(n) if v.len() + n <= MAX_TOTAL => v.extend_from_slice(&chunk[..n]),
                _ => break Err(Error::Io),
            }
        };
        let _ = sys::close(fd);
        r
    }

    /// Write `data` to `tmp` and rename it over `dst` (both NUL-terminated).
    fn replace(&self, tmp: &[u8], dst: &[u8], data: &[u8]) -> Result<(), Error> {
        let fd = sys::open_beneath(self.dir, tmp, true).map_err(|_| Error::Io)?;
        let w = sys::write_all(fd, data);
        let _ = sys::close(fd);
        w.map_err(|_| Error::Io)?;
        sys::rename_beneath(self.dir, tmp, dst).map_err(|_| Error::Io)
    }

    fn save_index(&self) -> Result<(), Error> {
        let mut b = Vec::new();
        for (n, z) in &self.index {
            b.extend_from_slice(n);
            b.push(b' ');
            let mut d = [0u8; 20];
            let mut i = d.len();
            let mut v = *z;
            loop {
                i -= 1;
                d[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            b.extend_from_slice(&d[i..]);
            b.push(b'\n');
        }
        self.replace(b"w-index\0", b"index\0", &b)
    }

    /// A record's bytes; empty if it does not exist (Chromium reads a missing file as empty).
    pub fn read(&self, name: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.read_file(&path(b"r-", name))?.unwrap_or_default())
    }

    pub fn write(&mut self, name: &[u8], data: &[u8]) -> Result<(), Error> {
        let old = self.index.iter().position(|(n, _)| n == name);
        if data.is_empty() {
            match sys::unlink_beneath(self.dir, &path(b"r-", name)) {
                Ok(()) => {}
                Err(sys::Errno(e)) if e == sys::ENOENT => {}
                Err(_) => return Err(Error::Io),
            }
            if let Some(i) = old {
                self.index.remove(i);
                self.save_index()?;
            }
            return Ok(());
        }
        let others: usize = self.index.iter().filter(|(n, _)| n != name).map(|(_, z)| z).sum();
        if others + data.len() > MAX_TOTAL || (old.is_none() && self.index.len() >= MAX_RECORDS) {
            return Err(Error::Quota);
        }
        self.replace(&path(b"w-", name), &path(b"r-", name), data)?;
        match old {
            Some(i) => self.index[i].1 = data.len(),
            None => self.index.push((name.to_vec(), data.len())),
        }
        self.save_index()
    }

    /// The origin's storage id: 32 random bytes, made on first use.
    pub fn storage_id(&self) -> Result<Vec<u8>, Error> {
        if let Some(id) = self.read_file(b"storage-id\0")? {
            if id.len() == 32 {
                return Ok(id);
            }
        }
        let mut id = alloc::vec![0u8; 32];
        let mut got = 0;
        while got < 32 {
            got += sys::getrandom(&mut id[got..]).map_err(|_| Error::Io)?;
        }
        self.replace(b"w-storage-id\0", b"storage-id\0", &id)?;
        Ok(id)
    }
}
