//! The virtual filesystem's path handling and descriptor table (docs/spec/hospes.md §7,
//! §11.6), driven by a hostile library's sequence of opens, reads and closes: never a panic,
//! only paths in the table open, and reads stay inside what was opened.
#![no_main]
use hospes_glibc::vfs;
use libfuzzer_sys::fuzz_target;

const KNOWN: &[&[u8]] = &[b"/proc/self/maps", b"/proc/thread-self/maps", b"/proc/net/unix", b"/proc/self/net/unix", b"/dev/urandom", b"/dev/random"];

fuzz_target!(|data: &[u8]| {
    let mut fds = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let op = data[i];
        i += 1;
        let arg = |i: usize| data.get(i).copied().unwrap_or(0);
        match op % 6 {
            0 | 1 => {
                // A path: a known one, or raw bytes up to a 0 (or the end).
                let path: Vec<u8> = if op % 2 == 0 {
                    KNOWN[arg(i) as usize % KNOWN.len()].to_vec()
                } else {
                    let end = data[i..].iter().position(|&b| b == 0).map_or(data.len(), |p| i + p);
                    let p = data[i..end].to_vec();
                    i = end;
                    p
                };
                i += 1;
                match vfs::open(&path, op & 0x80 != 0) {
                    Ok(fd) => {
                        assert!(KNOWN.contains(&&path[..]) || path.starts_with(b"/proc/"), "{path:?} opened");
                        fds.push(fd);
                    }
                    Err(_) => {}
                }
                let _ = vfs::stat(&path);
            }
            2 => {
                let fd = fds.get(arg(i) as usize % fds.len().max(1)).copied().unwrap_or(arg(i) as i32 + 890);
                let mut buf = vec![0u8; arg(i + 1) as usize * 64];
                if let Ok(n) = vfs::read(fd, &mut buf) {
                    assert!(n <= buf.len());
                }
                i += 2;
            }
            3 => {
                let fd = fds.get(arg(i) as usize % fds.len().max(1)).copied().unwrap_or(-1);
                let mut buf = vec![0u8; arg(i + 1) as usize];
                let off = u64::from(arg(i + 2)) << (arg(i + 3) % 64);
                if let Ok(n) = vfs::pread(fd, &mut buf, off) {
                    assert!(n <= buf.len());
                }
                let _ = vfs::fstat(fd);
                i += 4;
            }
            4 => {
                if !fds.is_empty() {
                    let fd = fds.swap_remove(arg(i) as usize % fds.len());
                    assert!(vfs::close(fd).is_ok());
                    assert!(vfs::close(fd).is_err());
                }
                i += 1;
            }
            _ => {
                let fd = i32::from_le_bytes([arg(i), arg(i + 1), arg(i + 2), arg(i + 3)]);
                let _ = vfs::is_virtual(fd);
                if !fds.contains(&fd) {
                    assert!(vfs::read(fd, &mut [0u8; 8]).is_err());
                }
                i += 4;
            }
        }
    }
    for fd in fds {
        assert!(vfs::close(fd).is_ok());
    }
});
