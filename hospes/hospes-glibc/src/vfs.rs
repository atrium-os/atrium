//! The virtual filesystem (docs/spec/hospes.md §7): a fixed table of paths, each generated
//! when opened. Nothing else exists; any other path is `ENOENT`. Shared by the shim's file
//! functions and the raw Linux `openat`/`read`/`close` the trap hands us ([`crate::syscall`]).
//!
//! K1 serves what the H0 Widevine run opened raw (`/proc/self/maps`, `/proc/net/unix`) and
//! `/dev/urandom`; the rest of the §7 table comes with H4.
use crate::abi;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use hospes_rt::load::Library;
use hospes_rt::lock::Mutex;
use hospes_rt::sys;

/// Virtual descriptors are `FD_BASE + slot`, well above what the host itself opens.
pub const FD_BASE: i32 = 900;
const SLOTS: usize = 16;

enum Content {
    Bytes { data: Vec<u8>, pos: usize },
    Random,
}

struct Vfs {
    open: [Option<Content>; SLOTS],
    /// The `/proc/self/maps` lines of the loaded library, made at load.
    maps: String,
}

static VFS: Mutex<Vfs> = Mutex::new(Vfs { open: [const { None }; SLOTS], maps: String::new() });

const NET_UNIX: &[u8] = b"Num       RefCount Protocol Flags    Type St Inode Path\n";

/// Describe the loaded library for `/proc/self/maps`, under `/opt/hospes/lib/<soname>`
/// (§7). Call after the load (protections are final).
pub fn set_library(lib: &Library) {
    let mut path = String::from("/opt/hospes/lib/");
    path.push_str(lib.img.soname.as_deref().unwrap_or("library.so"));
    let s = maps_for(&path, lib);
    VFS.lock().maps = s;
}

/// Linux's format: `start-end perms offset dev inode` then the path at column 73.
fn maps_for(path: &str, lib: &Library) -> String {
    let page_down = |x: usize| x & !(sys::PAGE - 1);
    let page_up = |x: usize| (x + sys::PAGE - 1) & !(sys::PAGE - 1);
    let relro = lib.img.relro.map(|(a, l)| (page_down(lib.base + a as usize), page_down(lib.base + (a + l) as usize)));
    let mut out = String::new();
    let mut line = |s: usize, e: usize, r: bool, w: bool, x: bool, off: usize, file: bool| {
        if e <= s {
            return;
        }
        let perms = [if r { 'r' } else { '-' }, if w { 'w' } else { '-' }, if x { 'x' } else { '-' }, 'p'];
        let mut l = String::new();
        let _ = write!(l, "{:08x}-{:08x} {}{}{}{} {:08x} ", s, e, perms[0], perms[1], perms[2], perms[3], off);
        if file {
            // A fixed device and inode: the file is the blob fd, which has no path here.
            let _ = write!(l, "fe:01 1048601");
            while l.len() < 73 {
                l.push(' ');
            }
            l.push_str(path);
        } else {
            l.push_str("00:00 0");
        }
        l.push('\n');
        out.push_str(&l);
    };
    for seg in &lib.img.segments {
        let va = lib.base + seg.vaddr as usize;
        let start = page_down(va);
        let file_end = if seg.filesz > 0 { page_up(va + seg.filesz as usize) } else { start };
        let mem_end = page_up(va + seg.memsz as usize);
        let off = page_down(seg.offset as usize);
        let f = seg.flags;
        // Split the file-backed part around RELRO, which is read-only after the load.
        let (rs, re) = relro.unwrap_or((0, 0));
        let cuts = [start, rs.clamp(start, file_end), re.clamp(start, file_end), file_end];
        for i in 0..3 {
            let (s, e) = (cuts[i], cuts[i + 1]);
            let ro = i == 1;
            line(s, e, f.r, f.w && !ro, f.x, off + (s - start), true);
        }
        line(file_end, mem_end, f.r, f.w, f.x, 0, false);
    }
    out
}

/// Open `path` read-only; a virtual descriptor or a Linux errno.
pub fn open(path: &[u8], write: bool) -> Result<i32, i32> {
    let mut g = VFS.lock();
    let content = match path {
        b"/proc/self/maps" | b"/proc/thread-self/maps" => Content::Bytes { data: g.maps.as_bytes().to_vec(), pos: 0 },
        b"/proc/net/unix" | b"/proc/self/net/unix" => Content::Bytes { data: NET_UNIX.to_vec(), pos: 0 },
        b"/dev/urandom" | b"/dev/random" => Content::Random,
        _ if is_own_pid_maps(path) => Content::Bytes { data: g.maps.as_bytes().to_vec(), pos: 0 },
        _ => return Err(abi::ENOENT),
    };
    if write && !matches!(content, Content::Random) {
        return Err(abi::EACCES);
    }
    let Some(slot) = g.open.iter().position(|s| s.is_none()) else { return Err(abi::EMFILE) };
    g.open[slot] = Some(content);
    Ok(FD_BASE + slot as i32)
}

fn is_own_pid_maps(path: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(b"/proc/") else { return false };
    let Some(num) = rest.strip_suffix(b"/maps") else { return false };
    let mut b = [0u8; 12];
    let mut w = Buf { b: &mut b, n: 0 };
    let _ = write!(w, "{}", sys::getpid());
    let n = w.n;
    num == &b[..n]
}

struct Buf<'a> {
    b: &'a mut [u8],
    n: usize,
}
impl Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let t = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + t].copy_from_slice(&s.as_bytes()[..t]);
        self.n += t;
        Ok(())
    }
}

fn slot(fd: i32) -> Option<usize> {
    let s = fd.checked_sub(FD_BASE)?;
    (0..SLOTS as i32).contains(&s).then_some(s as usize)
}

/// Is `fd` one of ours?
pub fn is_virtual(fd: i32) -> bool {
    slot(fd).is_some_and(|s| VFS.lock().open[s].is_some())
}

/// Read into `out`; bytes read or a Linux errno.
pub fn read(fd: i32, out: &mut [u8]) -> Result<usize, i32> {
    let s = slot(fd).ok_or(abi::EBADF)?;
    let mut g = VFS.lock();
    match g.open[s].as_mut().ok_or(abi::EBADF)? {
        Content::Bytes { data, pos } => {
            let n = out.len().min(data.len() - *pos);
            out[..n].copy_from_slice(&data[*pos..*pos + n]);
            *pos += n;
            Ok(n)
        }
        Content::Random => {
            let n = out.len().min(1 << 20);
            let mut o = 0;
            while o < n {
                o += sys::getrandom(&mut out[o..n]).map_err(|_| abi::EIO)?;
            }
            Ok(n)
        }
    }
}

pub fn close(fd: i32) -> Result<(), i32> {
    let s = slot(fd).ok_or(abi::EBADF)?;
    VFS.lock().open[s].take().map(|_| ()).ok_or(abi::EBADF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_unix_is_the_header_only() {
        let fd = open(b"/proc/net/unix", false).unwrap();
        let mut b = [0u8; 256];
        let n = read(fd, &mut b).unwrap();
        assert_eq!(&b[..n], NET_UNIX);
        assert_eq!(read(fd, &mut b), Ok(0));
        close(fd).unwrap();
        assert_eq!(close(fd), Err(abi::EBADF));
        assert_eq!(read(fd, &mut b), Err(abi::EBADF));
    }

    #[test]
    fn unknown_paths_do_not_exist() {
        assert_eq!(open(b"/etc/passwd", false), Err(abi::ENOENT));
        assert_eq!(open(b"/proc/1/maps", false), Err(abi::ENOENT));
        assert_eq!(open(b"proc/self/maps", false), Err(abi::ENOENT));
        assert_eq!(open(b"/proc/net/unix", true), Err(abi::EACCES));
    }

    #[test]
    fn urandom_fills() {
        let fd = open(b"/dev/urandom", false).unwrap();
        let mut b = [0u8; 64];
        assert_eq!(read(fd, &mut b), Ok(64));
        assert!(b.iter().any(|&x| x != 0));
        close(fd).unwrap();
    }
}
