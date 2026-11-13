//! stdio and file descriptors over the virtual filesystem (spec §6.3, §7: "No real
//! filesystem"). `stderr` and `stdout` reach our log; `stdin` is always at end of file.
//! Files open only through [`crate::vfs`], read-only. The same descriptors serve the raw
//! Linux calls the trap catches ([`crate::syscall`]).
use crate::printf::{self, Buf};
use crate::{abi, set_errno, vfs};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_void, VaList};
use hospes_rt::sys;

const MAGIC: u64 = 0x686f_7370_6573_4649; // "hospesFI"
const EOF: i32 = -1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Log,
    In,
    Vfs(i32),
}

/// A `FILE`. glibc code only passes the pointer back to us (every stdio entry it uses is an
/// import), so the layout is ours; `magic` catches a pointer that is not one of ours.
#[repr(C)]
pub struct File {
    magic: u64,
    target: Target,
    /// One pushed-back character (`ungetc`), or -1.
    unget: i32,
    eof: bool,
    err: bool,
}

static mut STDERR_FILE: File = File { magic: MAGIC, target: Target::Log, unget: EOF, eof: false, err: false };
static mut STDOUT_FILE: File = File { magic: MAGIC, target: Target::Log, unget: EOF, eof: false, err: false };
static mut STDIN_FILE: File = File { magic: MAGIC, target: Target::In, unget: EOF, eof: true, err: false };
/// glibc's `stderr`, `stdout`, `stdin` are data symbols holding a `FILE *`.
pub static mut stderr: *mut File = core::ptr::addr_of_mut!(STDERR_FILE);
pub static mut stdout: *mut File = core::ptr::addr_of_mut!(STDOUT_FILE);
pub static mut stdin: *mut File = core::ptr::addr_of_mut!(STDIN_FILE);

unsafe fn file<'a>(f: *mut File) -> Option<&'a mut File> {
    if f.is_null() || (*f).magic != MAGIC {
        set_errno(abi::EBADF);
        return None;
    }
    Some(&mut *f)
}

fn log_bytes(b: &[u8]) {
    let _ = sys::write_all(2, b);
}

// ---- formatted output --------------------------------------------------------------

/// Format into a heap buffer: two passes, the first only counts.
pub(crate) unsafe fn format_vec(fmt: *const u8, ap: &mut VaList) -> Vec<u8> {
    let mut probe = ap.clone();
    let mut b = Buf::new(core::ptr::null_mut(), 0);
    printf::format(&mut b, fmt, &mut probe);
    let n = b.len();
    let mut v = Vec::with_capacity(n + 1);
    let mut b = Buf::new(v.as_mut_ptr(), n + 1);
    printf::format(&mut b, fmt, ap);
    b.terminate();
    v.set_len(n);
    v
}

unsafe fn put_bytes(f: &mut File, b: &[u8]) -> bool {
    match f.target {
        Target::Log => {
            log_bytes(b);
            true
        }
        _ => {
            f.err = true;
            set_errno(abi::EBADF);
            false
        }
    }
}

pub unsafe extern "C" fn vfprintf(f: *mut File, fmt: *const u8, mut ap: VaList) -> i32 {
    let Some(f) = file(f) else { return -1 };
    let v = format_vec(fmt, &mut ap);
    if put_bytes(f, &v) { v.len() as i32 } else { -1 }
}
pub unsafe extern "C" fn __vfprintf_chk(f: *mut File, _flag: i32, fmt: *const u8, ap: VaList) -> i32 {
    vfprintf(f, fmt, ap)
}
pub unsafe extern "C" fn fprintf(f: *mut File, fmt: *const u8, mut args: ...) -> i32 {
    let Some(f) = file(f) else { return -1 };
    let v = format_vec(fmt, &mut args);
    if put_bytes(f, &v) { v.len() as i32 } else { -1 }
}
pub unsafe extern "C" fn __fprintf_chk(f: *mut File, _flag: i32, fmt: *const u8, mut args: ...) -> i32 {
    let Some(f) = file(f) else { return -1 };
    let v = format_vec(fmt, &mut args);
    if put_bytes(f, &v) { v.len() as i32 } else { -1 }
}

pub unsafe extern "C" fn vsnprintf(s: *mut u8, maxlen: usize, fmt: *const u8, mut ap: VaList) -> i32 {
    let mut b = Buf::new(s, maxlen);
    printf::format(&mut b, fmt, &mut ap);
    b.terminate();
    b.len() as i32
}
pub unsafe extern "C" fn snprintf(s: *mut u8, maxlen: usize, fmt: *const u8, mut args: ...) -> i32 {
    let mut b = Buf::new(s, maxlen);
    printf::format(&mut b, fmt, &mut args);
    b.terminate();
    b.len() as i32
}
pub unsafe extern "C" fn __vsnprintf_chk(s: *mut u8, maxlen: usize, _flag: i32, slen: usize, fmt: *const u8, ap: VaList) -> i32 {
    if slen < maxlen {
        crate::die("__vsnprintf_chk: buffer overflow");
    }
    vsnprintf(s, maxlen, fmt, ap)
}
/// The result is `malloc`ed (our heap, freed with the shim's `free`).
pub unsafe extern "C" fn vasprintf(out: *mut *mut u8, fmt: *const u8, mut ap: VaList) -> i32 {
    let v = format_vec(fmt, &mut ap);
    let p = crate::mem::malloc(v.len() + 1) as *mut u8;
    if p.is_null() {
        set_errno(abi::ENOMEM);
        return -1;
    }
    core::ptr::copy_nonoverlapping(v.as_ptr(), p, v.len());
    *p.add(v.len()) = 0;
    *out = p;
    v.len() as i32
}

pub unsafe extern "C" fn perror(s: *const u8) {
    let e = (*crate::misc::__errno_location()) as i32;
    if !s.is_null() && *s != 0 {
        log_bytes(core::slice::from_raw_parts(s, crate::mem::strlen(s)));
        log_bytes(b": ");
    }
    let m = crate::string::strerror(e);
    log_bytes(core::slice::from_raw_parts(m, crate::mem::strlen(m)));
    log_bytes(b"\n");
}

// ---- FILE operations ----------------------------------------------------------------

/// Read-only, from the virtual filesystem.
pub unsafe extern "C" fn fopen64(path: *const u8, mode: *const u8) -> *mut File {
    if path.is_null() || mode.is_null() {
        set_errno(abi::EINVAL);
        return core::ptr::null_mut();
    }
    let m = core::slice::from_raw_parts(mode, crate::mem::strlen(mode));
    let write = m.first() != Some(&b'r') || m.contains(&b'+');
    let p = core::slice::from_raw_parts(path, crate::mem::strlen(path));
    match vfs::open(p, write) {
        Ok(fd) => Box::into_raw(Box::new(File { magic: MAGIC, target: Target::Vfs(fd), unget: EOF, eof: false, err: false })),
        Err(e) => {
            set_errno(e);
            core::ptr::null_mut()
        }
    }
}
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> *mut File {
    fopen64(path, mode)
}

fn is_static(f: *mut File) -> bool {
    core::ptr::eq(f, core::ptr::addr_of_mut!(STDERR_FILE))
        || core::ptr::eq(f, core::ptr::addr_of_mut!(STDOUT_FILE))
        || core::ptr::eq(f, core::ptr::addr_of_mut!(STDIN_FILE))
}

pub unsafe extern "C" fn fclose(f: *mut File) -> i32 {
    let Some(r) = file(f) else { return EOF };
    if is_static(f) {
        return 0;
    }
    if let Target::Vfs(fd) = r.target {
        let _ = vfs::close(fd);
    }
    r.magic = 0;
    drop(Box::from_raw(f));
    0
}
pub unsafe extern "C" fn fflush(_f: *mut File) -> i32 {
    0
}
pub unsafe extern "C" fn fseek(f: *mut File, _off: i64, _whence: i32) -> i32 {
    let _ = file(f);
    set_errno(abi::ESPIPE);
    -1
}
pub unsafe extern "C" fn fwrite(p: *const c_void, size: usize, n: usize, f: *mut File) -> usize {
    let Some(f) = file(f) else { return 0 };
    let Some(len) = size.checked_mul(n) else { return 0 };
    if put_bytes(f, core::slice::from_raw_parts(p as *const u8, len)) { n } else { 0 }
}
pub unsafe extern "C" fn fputc(c: i32, f: *mut File) -> i32 {
    let Some(f) = file(f) else { return EOF };
    if put_bytes(f, &[c as u8]) { c & 0xff } else { EOF }
}
/// Wide output in the C locale: ASCII only.
pub unsafe extern "C" fn fputwc(wc: u32, f: *mut File) -> u32 {
    if wc >= 0x80 {
        set_errno(abi::EILSEQ);
        return u32::MAX;
    }
    if fputc(wc as i32, f) == EOF { u32::MAX } else { wc }
}

unsafe fn next_byte(f: &mut File) -> i32 {
    if f.unget != EOF {
        let c = f.unget;
        f.unget = EOF;
        return c;
    }
    let Target::Vfs(fd) = f.target else {
        f.eof = true;
        return EOF;
    };
    let mut b = [0u8; 1];
    match vfs::read(fd, &mut b) {
        Ok(1) => b[0] as i32,
        Ok(_) => {
            f.eof = true;
            EOF
        }
        Err(e) => {
            f.err = true;
            set_errno(e);
            EOF
        }
    }
}

pub unsafe extern "C" fn getc(f: *mut File) -> i32 {
    let Some(f) = file(f) else { return EOF };
    next_byte(f)
}
pub unsafe extern "C" fn ungetc(c: i32, f: *mut File) -> i32 {
    let Some(f) = file(f) else { return EOF };
    if c == EOF {
        return EOF;
    }
    f.unget = c & 0xff;
    f.eof = false;
    c & 0xff
}
pub unsafe extern "C" fn getwc(f: *mut File) -> u32 {
    match getc(f) {
        EOF => u32::MAX,
        c if c >= 0x80 => {
            set_errno(abi::EILSEQ);
            u32::MAX
        }
        c => c as u32,
    }
}
pub unsafe extern "C" fn ungetwc(wc: u32, f: *mut File) -> u32 {
    if wc == u32::MAX || wc >= 0x80 {
        return u32::MAX;
    }
    if ungetc(wc as i32, f) == EOF { u32::MAX } else { wc }
}
pub unsafe extern "C" fn fgets(s: *mut u8, n: i32, f: *mut File) -> *mut u8 {
    let Some(f) = file(f) else { return core::ptr::null_mut() };
    if n <= 0 {
        return core::ptr::null_mut();
    }
    let mut i = 0usize;
    while i + 1 < n as usize {
        let c = next_byte(f);
        if c == EOF {
            break;
        }
        *s.add(i) = c as u8;
        i += 1;
        if c == b'\n' as i32 {
            break;
        }
    }
    if i == 0 {
        return core::ptr::null_mut();
    }
    *s.add(i) = 0;
    s
}

/// `fileno`: 0, 1, 2 for the standard streams, the descriptor of a virtual file.
pub(crate) unsafe fn fileno_of(f: *mut File) -> i32 {
    let Some(r) = file(f) else { return -1 };
    match r.target {
        Target::Vfs(fd) => fd,
        Target::In => 0,
        Target::Log if core::ptr::eq(f, core::ptr::addr_of_mut!(STDOUT_FILE)) => 1,
        Target::Log => 2,
    }
}
/// `fdopen` of a virtual file (read-only); any other descriptor is not ours.
pub(crate) unsafe fn fdopen_vfs(fd: i32) -> *mut File {
    if !vfs::is_virtual(fd) {
        set_errno(abi::EBADF);
        return core::ptr::null_mut();
    }
    Box::into_raw(Box::new(File { magic: MAGIC, target: Target::Vfs(fd), unget: EOF, eof: false, err: false }))
}
pub unsafe extern "C" fn clearerr(f: *mut File) {
    if let Some(f) = file(f) {
        f.eof = false;
        f.err = false;
    }
}
pub unsafe extern "C" fn feof(f: *mut File) -> i32 {
    file(f).is_some_and(|f| f.eof && f.unget == EOF) as i32
}
pub unsafe extern "C" fn ferror(f: *mut File) -> i32 {
    file(f).is_some_and(|f| f.err) as i32
}

// ---- descriptors ---------------------------------------------------------------------

const O_ACCMODE: i32 = 3;
const O_CREAT: i32 = 0o100;

pub unsafe extern "C" fn open64(path: *const u8, flags: i32, _mode: u32) -> i32 {
    if path.is_null() {
        set_errno(abi::EFAULT);
        return -1;
    }
    let p = core::slice::from_raw_parts(path, crate::mem::strlen(path));
    match vfs::open(p, flags & O_ACCMODE != 0 || flags & O_CREAT != 0) {
        Ok(fd) => fd,
        Err(e) => {
            set_errno(e);
            -1
        }
    }
}
pub unsafe extern "C" fn creat64(_path: *const u8, _mode: u32) -> i32 {
    set_errno(abi::EACCES);
    -1
}

pub unsafe extern "C" fn read(fd: i32, p: *mut u8, n: usize) -> isize {
    match vfs::read(fd, core::slice::from_raw_parts_mut(p, n)) {
        Ok(k) => k as isize,
        Err(e) => {
            set_errno(e);
            -1
        }
    }
}
pub unsafe extern "C" fn pread64(fd: i32, p: *mut u8, n: usize, off: i64) -> isize {
    if off < 0 {
        set_errno(abi::EINVAL);
        return -1;
    }
    match vfs::pread(fd, core::slice::from_raw_parts_mut(p, n), off as u64) {
        Ok(k) => k as isize,
        Err(e) => {
            set_errno(e);
            -1
        }
    }
}
/// Descriptors 1 and 2 are our log; virtual files are read-only.
pub unsafe extern "C" fn write(fd: i32, p: *const u8, n: usize) -> isize {
    match fd {
        1 | 2 => {
            log_bytes(core::slice::from_raw_parts(p, n));
            n as isize
        }
        _ => {
            set_errno(abi::EBADF);
            -1
        }
    }
}

const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const FD_CLOEXEC: i64 = 1;
const O_CLOEXEC_FLAG: i64 = 0o2000000;

pub unsafe extern "C" fn fcntl(fd: i32, cmd: i32, _arg: usize) -> i32 {
    if !vfs::is_virtual(fd) {
        set_errno(abi::EBADF);
        return -1;
    }
    match cmd {
        F_GETFD => FD_CLOEXEC as i32,
        F_SETFD | F_SETFL => 0,
        F_GETFL => (O_CLOEXEC_FLAG & 0) as i32, // O_RDONLY
        _ => {
            set_errno(abi::EINVAL);
            -1
        }
    }
}

/// Linux `struct stat`, filled for a virtual file.
unsafe fn fill_stat(buf: *mut u8, k: vfs::Kind) {
    let (mode, rdev): (u32, u64) = match k {
        vfs::Kind::Proc => (0o100444, 0),
        vfs::Kind::Random { minor } => (0o020666, (1 << 8) | minor as u64),
    };
    #[cfg(target_arch = "x86_64")]
    {
        core::ptr::write_bytes(buf, 0, 144);
        *(buf.add(0) as *mut u64) = if rdev == 0 { 0x16 } else { 5 }; // st_dev: proc / devtmpfs
        *(buf.add(8) as *mut u64) = if rdev == 0 { 4026531842 } else { 4 + rdev }; // st_ino
        *(buf.add(16) as *mut u64) = 1; // st_nlink
        *(buf.add(24) as *mut u32) = mode;
        *(buf.add(40) as *mut u64) = rdev;
        *(buf.add(56) as *mut i64) = 4096; // st_blksize
    }
    #[cfg(target_arch = "aarch64")]
    {
        core::ptr::write_bytes(buf, 0, 128);
        *(buf.add(0) as *mut u64) = if rdev == 0 { 0x16 } else { 5 };
        *(buf.add(8) as *mut u64) = if rdev == 0 { 4026531842 } else { 4 + rdev };
        *(buf.add(16) as *mut u32) = mode;
        *(buf.add(20) as *mut u32) = 1;
        *(buf.add(32) as *mut u64) = rdev;
        *(buf.add(56) as *mut i32) = 4096;
    }
}

pub unsafe extern "C" fn __xstat64(_ver: i32, path: *const u8, buf: *mut u8) -> i32 {
    if path.is_null() {
        set_errno(abi::EFAULT);
        return -1;
    }
    match vfs::stat(core::slice::from_raw_parts(path, crate::mem::strlen(path))) {
        Ok(k) => {
            fill_stat(buf, k);
            0
        }
        Err(e) => {
            set_errno(e);
            -1
        }
    }
}
pub unsafe extern "C" fn __fxstat64(_ver: i32, fd: i32, buf: *mut u8) -> i32 {
    match vfs::fstat(fd) {
        Ok(k) => {
            fill_stat(buf, k);
            0
        }
        Err(e) => {
            set_errno(e);
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_over_the_vfs() {
        unsafe {
            let f = fopen64(b"/proc/net/unix\0".as_ptr(), b"re\0".as_ptr());
            assert!(!f.is_null());
            let mut line = [0u8; 128];
            assert!(!fgets(line.as_mut_ptr(), 128, f).is_null());
            assert!(line.starts_with(b"Num       RefCount"));
            assert_eq!(getc(f), EOF);
            assert_eq!(ungetc(b'x' as i32, f), b'x' as i32);
            assert_eq!(getc(f), b'x' as i32);
            assert_eq!(fclose(f), 0);
            assert!(fopen64(b"/proc/net/unix\0".as_ptr(), b"w\0".as_ptr()).is_null());
            assert!(fopen64(b"/etc/passwd\0".as_ptr(), b"r\0".as_ptr()).is_null());
            assert_eq!(getc(stdin), EOF);
            let mut st = [0u8; 144];
            assert_eq!(__xstat64(1, b"/sys/fs/cgroup/cpuset/chrome\0".as_ptr(), st.as_mut_ptr()), -1);
            assert_eq!(__xstat64(1, b"/dev/urandom\0".as_ptr(), st.as_mut_ptr()), 0);
            let fd = open64(b"/dev/urandom\0".as_ptr(), 0, 0);
            assert!(fd >= vfs::FD_BASE);
            let mut b = [0u8; 8];
            assert_eq!(read(fd, b.as_mut_ptr(), 8), 8);
            assert_eq!(fcntl(fd, F_GETFD, 0), 1);
            assert_eq!(__fxstat64(1, fd, st.as_mut_ptr()), 0);
            assert!(vfs::close(fd).is_ok());
        }
    }

    #[test]
    fn formatting() {
        unsafe {
            let mut b = [0u8; 8];
            assert_eq!(snprintf(b.as_mut_ptr(), 8, b"%d-%s\0".as_ptr(), 1234i32, b"abcdef\0".as_ptr()), 11);
            assert_eq!(&b, b"1234-ab\0");
            let mut p: *mut u8 = core::ptr::null_mut();
            unsafe extern "C" fn via(out: *mut *mut u8, fmt: *const u8, args: ...) -> i32 {
                unsafe { vasprintf(out, fmt, args.clone()) }
            }
            assert_eq!(via(&mut p, b"%x/%c\0".as_ptr(), 255u32, b'z' as i32), 4);
            assert_eq!(core::slice::from_raw_parts(p, 5), b"ff/z\0");
            crate::mem::free(p as _);
        }
    }
}
