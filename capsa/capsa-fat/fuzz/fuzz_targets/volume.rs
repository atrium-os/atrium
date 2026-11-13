//! Any bytes as the start of a FAT volume on a 64 MiB device (the rest
//! reads as zeros): open it, walk every directory, read every file, then
//! create, write, rename, remove and sync. Errors are fine; a panic, a hang
//! or an unbounded allocation is the bug.
#![no_main]

use capsa_fat::{BlockDev, Error, Result, Volume};
use libfuzzer_sys::fuzz_target;

const DEV: u64 = 64 << 20;
const CAP: usize = 8 << 20;

struct Dev(Vec<u8>);

impl BlockDev for Dev {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let end = off.checked_add(buf.len() as u64).filter(|&e| e <= DEV).ok_or(Error::Io("beyond".into()))? as usize;
        let off = off as usize;
        let have = self.0.len().min(end).max(off);
        if off >= self.0.len() {
            buf.fill(0);
            return Ok(());
        }
        buf[..have - off].copy_from_slice(&self.0[off..have]);
        buf[have - off..].fill(0);
        Ok(())
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        let end = off.checked_add(buf.len() as u64).filter(|&e| e <= DEV).ok_or(Error::Io("beyond".into()))? as usize;
        if end > CAP {
            return Err(Error::Io("past the fuzz cap".into()));
        }
        if self.0.len() < end {
            self.0.resize(end, 0);
        }
        self.0[off as usize..end].copy_from_slice(buf);
        Ok(())
    }
    fn len(&self) -> u64 {
        DEV
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(mut v) = Volume::open(Dev(data.to_vec())) else { return };
    let root = v.root();
    let mut stack = vec![(root, 0u32)];
    let mut dirs = 0;
    while let Some((d, depth)) = stack.pop() {
        dirs += 1;
        if dirs > 64 || depth > 6 {
            break;
        }
        let Ok(list) = v.read_dir(d) else { continue };
        for e in list.into_iter().take(256) {
            if e.is_dir() {
                if let Ok(x) = v.dir_of(&e) {
                    stack.push((x, depth + 1));
                }
            } else {
                let mut b = [0u8; 4096];
                let _ = v.read(&e, 0, &mut b);
                let _ = v.read(&e, u64::from(e.size).saturating_sub(100), &mut b);
            }
        }
    }
    let _ = v.stat();
    let now = 1_794_579_656;
    if let Ok(mut f) = v.create(root, "fuzz file.txt", false, now) {
        let _ = v.write(&mut f, 3000, &[1u8; 5000], now);
        let _ = v.truncate(&mut f, 100, now);
        if let Ok(d) = v.create(root, "Fuzz Dir", true, now) {
            if let Ok(dd) = v.dir_of(&d) {
                let _ = v.rename(&f, dd, "moved.bin", now);
            }
            let _ = v.rmdir(&d);
        }
    }
    if let Ok(list) = v.read_dir(root) {
        if let Some(e) = list.into_iter().find(|e| !e.is_dir()) {
            let _ = v.unlink(&e);
        }
    }
    let _ = v.sync();
});
