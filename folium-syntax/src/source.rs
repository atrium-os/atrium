//! Where the bytes come from: the descriptor, read with `pread`, never
//! read whole and never mapped (the viewer's descriptor has no mmap right,
//! opening-files §6.3).

use crate::{Error, Result};

pub trait Source: Send + Sync {
    fn len(&self) -> u64;
    /// Read up to `buf.len()` bytes at `offset`; fewer only at the end.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// `len` bytes at `offset`, cut at the end of the file.
    fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let end = self.len();
        if offset >= end {
            return Ok(Vec::new());
        }
        let n = (len as u64).min(end - offset) as usize;
        let mut v = vec![0; n];
        let mut got = 0;
        while got < n {
            let k = self.read_at(offset + got as u64, &mut v[got..])?;
            if k == 0 {
                break;
            }
            got += k;
        }
        v.truncate(got);
        Ok(v)
    }
}

impl Source for std::fs::File {
    fn len(&self) -> u64 { self.metadata().map(|m| m.len()).unwrap_or(0) }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        loop {
            match FileExt::read_at(self, buf, offset) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::from(e)),
            }
        }
    }
}

/// Bytes in memory (tests, and documents handed over as bytes).
pub struct Bytes(pub std::sync::Arc<[u8]>);

impl Source for Bytes {
    fn len(&self) -> u64 { self.0.len() as u64 }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let o = offset.min(self.0.len() as u64) as usize;
        let n = buf.len().min(self.0.len() - o);
        buf[..n].copy_from_slice(&self.0[o..o + n]);
        Ok(n)
    }
}
