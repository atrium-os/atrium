//! The rest of the C library a C++ PKCS#11 module and the real libstdc++ import
//! (sigillum.md §16.5): strings, wide strings, the stdio and descriptor calls not in
//! [`crate::stdio`], the C locale's `_l` entry points, message catalogues (none), `iconv`
//! (no conversions), time, maths, and syslog (to our log). Everything here is plain
//! computation over the caller's memory or over the virtual filesystem; what a jailed
//! library may never do is in [`crate::deny`].
use crate::printf::{self, Buf};
use crate::stdio::File;
use crate::{abi, mem, set_errno, stdio, string, vfs};
use alloc::vec::Vec;
use core::ffi::{c_void, VaList};

unsafe fn bytes<'a>(s: *const u8) -> &'a [u8] {
    core::slice::from_raw_parts(s, mem::strlen(s))
}

// ---- strings ----------------------------------------------------------------------------

pub unsafe extern "C" fn strcpy(d: *mut u8, s: *const u8) -> *mut u8 {
    core::ptr::copy(s, d, mem::strlen(s) + 1);
    d
}
pub unsafe extern "C" fn stpcpy(d: *mut u8, s: *const u8) -> *mut u8 {
    let n = mem::strlen(s);
    core::ptr::copy(s, d, n + 1);
    d.add(n)
}
pub unsafe extern "C" fn strcat(d: *mut u8, s: *const u8) -> *mut u8 {
    strcpy(d.add(mem::strlen(d)), s);
    d
}
pub unsafe extern "C" fn __strcpy_chk(d: *mut u8, s: *const u8, dlen: usize) -> *mut u8 {
    if mem::strlen(s) >= dlen {
        crate::die("__strcpy_chk: buffer overflow");
    }
    strcpy(d, s)
}
pub unsafe extern "C" fn __strcat_chk(d: *mut u8, s: *const u8, dlen: usize) -> *mut u8 {
    if mem::strlen(d) + mem::strlen(s) >= dlen {
        crate::die("__strcat_chk: buffer overflow");
    }
    strcat(d, s)
}
pub unsafe extern "C" fn __memset_chk(d: *mut c_void, c: i32, n: usize, dlen: usize) -> *mut c_void {
    if n > dlen {
        crate::die("__memset_chk: buffer overflow");
    }
    mem::memset(d, c, n)
}
/// `FD_SET` and friends: the descriptor's word in an `fd_set` (1024 bits).
pub extern "C" fn __fdelt_chk(d: i64) -> i64 {
    if !(0..1024).contains(&d) {
        crate::die("__fdelt_chk: descriptor out of range");
    }
    d / 64
}
pub unsafe extern "C" fn strdup(s: *const u8) -> *mut u8 {
    let n = mem::strlen(s) + 1;
    let d = mem::malloc(n) as *mut u8;
    if !d.is_null() {
        core::ptr::copy_nonoverlapping(s, d, n);
    }
    d
}
fn lower(c: u8) -> u8 {
    c.to_ascii_lowercase()
}
pub unsafe extern "C" fn strncasecmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let (x, y) = (lower(*a.add(i)), lower(*b.add(i)));
        if x != y || x == 0 {
            return x as i32 - y as i32;
        }
    }
    0
}
pub unsafe extern "C" fn strcasecmp(a: *const u8, b: *const u8) -> i32 {
    strncasecmp(a, b, usize::MAX)
}
unsafe fn span(s: *const u8, set: *const u8, inside: bool) -> usize {
    let set = bytes(set);
    let mut i = 0;
    while *s.add(i) != 0 && set.contains(&*s.add(i)) == inside {
        i += 1;
    }
    i
}
pub unsafe extern "C" fn strspn(s: *const u8, set: *const u8) -> usize {
    span(s, set, true)
}
pub unsafe extern "C" fn strcspn(s: *const u8, set: *const u8) -> usize {
    span(s, set, false)
}
pub unsafe extern "C" fn strstr(h: *const u8, n: *const u8) -> *const u8 {
    let (hb, nb) = (bytes(h), bytes(n));
    if nb.is_empty() {
        return h;
    }
    match hb.windows(nb.len()).position(|w| w == nb) {
        Some(i) => h.add(i),
        None => core::ptr::null(),
    }
}
pub unsafe extern "C" fn strtok_r(s: *mut u8, delim: *const u8, save: *mut *mut u8) -> *mut u8 {
    let mut p = if s.is_null() { *save } else { s };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    p = p.add(strspn(p, delim));
    if *p == 0 {
        *save = p;
        return core::ptr::null_mut();
    }
    let tok = p;
    p = p.add(strcspn(p, delim));
    if *p != 0 {
        *p = 0;
        p = p.add(1);
    }
    *save = p;
    tok
}
pub unsafe extern "C" fn strtok(s: *mut u8, delim: *const u8) -> *mut u8 {
    static mut SAVE: *mut u8 = core::ptr::null_mut();
    strtok_r(s, delim, core::ptr::addr_of_mut!(SAVE))
}
/// The XSI `strerror_r`: copies the message, returns 0 or `ERANGE`.
pub unsafe extern "C" fn __xpg_strerror_r(e: i32, buf: *mut u8, len: usize) -> i32 {
    let m = bytes(string::strerror(e));
    if len == 0 {
        return abi::ERANGE;
    }
    let n = m.len().min(len - 1);
    core::ptr::copy_nonoverlapping(m.as_ptr(), buf, n);
    *buf.add(n) = 0;
    if n < m.len() { abi::ERANGE } else { 0 }
}
pub extern "C" fn tolower(c: i32) -> i32 {
    if (b'A' as i32..=b'Z' as i32).contains(&c) { c + 32 } else { c }
}
pub extern "C" fn isalnum(c: i32) -> i32 {
    ((0..128).contains(&c) && (c as u8).is_ascii_alphanumeric()) as i32
}
pub extern "C" fn isspace(c: i32) -> i32 {
    matches!(c, 0x20 | 0x09..=0x0d) as i32
}

// ---- wide strings (wchar_t is 32 bits on both machines) -------------------------------

pub unsafe extern "C" fn wcscmp(a: *const u32, b: *const u32) -> i32 {
    let mut i = 0;
    loop {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return if (x as i32) < (y as i32) { -1 } else { 1 };
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}
pub unsafe extern "C" fn wcsdup(s: *const u32) -> *mut u32 {
    let n = (string::wcslen(s) + 1) * 4;
    let d = mem::malloc(n) as *mut u32;
    if !d.is_null() {
        core::ptr::copy_nonoverlapping(s, d, n / 4);
    }
    d
}
pub unsafe extern "C" fn wcsncpy(d: *mut u32, s: *const u32, n: usize) -> *mut u32 {
    let mut i = 0;
    while i < n && *s.add(i) != 0 {
        *d.add(i) = *s.add(i);
        i += 1;
    }
    while i < n {
        *d.add(i) = 0;
        i += 1;
    }
    d
}
pub unsafe extern "C" fn wmemcpy(d: *mut u32, s: *const u32, n: usize) -> *mut u32 {
    core::ptr::copy_nonoverlapping(s, d, n);
    d
}
pub unsafe extern "C" fn wmemmove(d: *mut u32, s: *const u32, n: usize) -> *mut u32 {
    core::ptr::copy(s, d, n);
    d
}
pub unsafe extern "C" fn wmemset(d: *mut u32, c: u32, n: usize) -> *mut u32 {
    for i in 0..n {
        *d.add(i) = c;
    }
    d
}
pub unsafe extern "C" fn wmemcmp(a: *const u32, b: *const u32, n: usize) -> i32 {
    for i in 0..n {
        let (x, y) = (*a.add(i) as i32, *b.add(i) as i32);
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}
const WEOF: u32 = u32::MAX;
/// The C locale is ASCII: bytes above 0x7f are not characters.
pub extern "C" fn btowc(c: i32) -> u32 {
    if (0..0x80).contains(&c) { c as u32 } else { WEOF }
}
pub extern "C" fn wctob(wc: u32) -> i32 {
    if wc < 0x80 { wc as i32 } else { -1 }
}
pub unsafe extern "C" fn mbstowcs(d: *mut u32, s: *const u8, n: usize) -> usize {
    let mut p = s;
    crate::locale::mbsrtowcs(d, &mut p, n, core::ptr::null_mut())
}

// ---- the C locale's `_l` entry points --------------------------------------------------

pub unsafe extern "C" fn __duplocale(l: *mut c_void) -> *mut c_void {
    l
}
pub unsafe extern "C" fn setlocale(_cat: i32, name: *const u8) -> *const u8 {
    if name.is_null() || matches!(bytes(name), b"" | b"C" | b"POSIX") {
        b"C\0".as_ptr()
    } else {
        core::ptr::null()
    }
}
/// glibc's `struct lconv`: ten strings, then fourteen `char`s (`CHAR_MAX`: not available).
#[repr(C)]
pub struct Lconv {
    s: [*const u8; 10],
    c: [u8; 14],
}
static mut LCONV: Lconv = Lconv { s: [b"\0".as_ptr(); 10], c: [127; 14] };
pub unsafe extern "C" fn localeconv() -> *mut Lconv {
    let l = &mut *core::ptr::addr_of_mut!(LCONV);
    l.s[0] = b".\0".as_ptr();
    l
}
/// `nl_langinfo` items of the C locale (langinfo.h); anything else is "".
pub unsafe extern "C" fn __nl_langinfo_l(item: i32, _l: *mut c_void) -> *const u8 {
    const ABDAY: [&[u8]; 7] = [b"Sun\0", b"Mon\0", b"Tue\0", b"Wed\0", b"Thu\0", b"Fri\0", b"Sat\0"];
    const DAY: [&[u8]; 7] = [b"Sunday\0", b"Monday\0", b"Tuesday\0", b"Wednesday\0", b"Thursday\0", b"Friday\0", b"Saturday\0"];
    const ABMON: [&[u8]; 12] =
        [b"Jan\0", b"Feb\0", b"Mar\0", b"Apr\0", b"May\0", b"Jun\0", b"Jul\0", b"Aug\0", b"Sep\0", b"Oct\0", b"Nov\0", b"Dec\0"];
    const MON: [&[u8]; 12] = [
        b"January\0", b"February\0", b"March\0", b"April\0", b"May\0", b"June\0", b"July\0", b"August\0", b"September\0",
        b"October\0", b"November\0", b"December\0",
    ];
    let s: &[u8] = match item {
        14 => b"ANSI_X3.4-1968\0", // CODESET
        0x10000 => b".\0",          // RADIXCHAR
        0x20000..=0x20006 => ABDAY[(item - 0x20000) as usize],
        0x20007..=0x2000d => DAY[(item - 0x20007) as usize],
        0x2000e..=0x20019 => ABMON[(item - 0x2000e) as usize],
        0x2001a..=0x20025 => MON[(item - 0x2001a) as usize],
        0x20026 => b"AM\0",
        0x20027 => b"PM\0",
        0x20028 => b"%a %b %e %H:%M:%S %Y\0", // D_T_FMT
        0x20029 => b"%m/%d/%y\0",            // D_FMT
        0x2002a => b"%H:%M:%S\0",            // T_FMT
        0x2002b => b"%I:%M:%S %p\0",         // T_FMT_AMPM
        _ => b"\0",
    };
    s.as_ptr()
}

/// Character classes, as `wctype` names them: 1 alnum, 2 alpha, 3 blank, 4 cntrl, 5 digit,
/// 6 graph, 7 lower, 8 print, 9 punct, 10 space, 11 upper, 12 xdigit (0: unknown).
pub unsafe extern "C" fn __wctype_l(name: *const u8, _l: *mut c_void) -> usize {
    const NAMES: [&[u8]; 12] =
        [b"alnum", b"alpha", b"blank", b"cntrl", b"digit", b"graph", b"lower", b"print", b"punct", b"space", b"upper", b"xdigit"];
    let n = bytes(name);
    NAMES.iter().position(|x| *x == n).map_or(0, |i| i + 1)
}
pub extern "C" fn __iswctype_l(wc: u32, desc: usize, _l: *mut c_void) -> i32 {
    if wc >= 0x80 {
        return 0;
    }
    let c = wc as u8;
    (match desc {
        1 => c.is_ascii_alphanumeric(),
        2 => c.is_ascii_alphabetic(),
        3 => c == b' ' || c == b'\t',
        4 => c.is_ascii_control(),
        5 => c.is_ascii_digit(),
        6 => c.is_ascii_graphic(),
        7 => c.is_ascii_lowercase(),
        8 => c.is_ascii_graphic() || c == b' ',
        9 => c.is_ascii_punctuation(),
        10 => isspace(c as i32) != 0,
        11 => c.is_ascii_uppercase(),
        12 => c.is_ascii_hexdigit(),
        _ => false,
    }) as i32
}
pub extern "C" fn __towlower_l(wc: u32, _l: *mut c_void) -> u32 {
    if wc < 0x80 { (wc as u8).to_ascii_lowercase() as u32 } else { wc }
}
pub extern "C" fn __towupper_l(wc: u32, _l: *mut c_void) -> u32 {
    if wc < 0x80 { (wc as u8).to_ascii_uppercase() as u32 } else { wc }
}
/// Collation in the C locale is byte order.
pub unsafe extern "C" fn __strcoll_l(a: *const u8, b: *const u8, _l: *mut c_void) -> i32 {
    string::strcmp(a, b)
}
pub unsafe extern "C" fn __strxfrm_l(d: *mut u8, s: *const u8, n: usize, _l: *mut c_void) -> usize {
    let len = mem::strlen(s);
    if len < n {
        core::ptr::copy_nonoverlapping(s, d, len + 1);
    }
    len
}
pub unsafe extern "C" fn __wcscoll_l(a: *const u32, b: *const u32, _l: *mut c_void) -> i32 {
    wcscmp(a, b)
}
pub unsafe extern "C" fn __wcsxfrm_l(d: *mut u32, s: *const u32, n: usize, _l: *mut c_void) -> usize {
    let len = string::wcslen(s);
    if len < n {
        core::ptr::copy_nonoverlapping(s, d, len + 1);
    }
    len
}
/// Narrow the (ASCII) format, format, widen the result.
pub unsafe extern "C" fn __wcsftime_l(d: *mut u32, max: usize, fmt: *const u32, tm: *const crate::time::Tm, _l: *mut c_void) -> usize {
    let n = string::wcslen(fmt);
    let mut f = Vec::with_capacity(n + 1);
    for i in 0..n {
        let c = *fmt.add(i);
        f.push(if c < 0x80 { c as u8 } else { b'?' });
    }
    f.push(0);
    let mut out = alloc::vec![0u8; max.max(1)];
    let k = crate::time::strftime(out.as_mut_ptr(), max, f.as_ptr(), tm);
    if k == 0 && max > 0 {
        *d = 0;
        return 0;
    }
    for i in 0..=k.min(max.saturating_sub(1)) {
        *d.add(i) = out[i] as u32;
    }
    k
}

// ---- message catalogues and character set conversion: none ------------------------------

pub extern "C" fn gettext(m: *const u8) -> *const u8 {
    m
}
pub extern "C" fn dgettext(_d: *const u8, m: *const u8) -> *const u8 {
    m
}
pub extern "C" fn bindtextdomain(_d: *const u8, dir: *const u8) -> *const u8 {
    if dir.is_null() { b"/usr/share/locale\0".as_ptr() } else { dir }
}
pub extern "C" fn bind_textdomain_codeset(_d: *const u8, cs: *const u8) -> *const u8 {
    cs
}
pub unsafe extern "C" fn iconv_open(_to: *const u8, _from: *const u8) -> usize {
    crate::deny::refuse("iconv_open", abi::EINVAL);
    usize::MAX
}
pub unsafe extern "C" fn iconv(_cd: usize, _i: *mut *mut u8, _il: *mut usize, _o: *mut *mut u8, _ol: *mut usize) -> usize {
    set_errno(abi::EBADF);
    usize::MAX
}
pub extern "C" fn iconv_close(_cd: usize) -> i32 {
    0
}

// ---- stdio and descriptors ----------------------------------------------------------

pub unsafe extern "C" fn close(fd: i32) -> i32 {
    match vfs::close(fd) {
        Ok(()) => 0,
        Err(e) => {
            set_errno(e);
            -1
        }
    }
}
/// Virtual files are streams: no seeking.
pub unsafe extern "C" fn lseek64(fd: i32, _off: i64, _whence: i32) -> i64 {
    set_errno(if vfs::is_virtual(fd) { abi::ESPIPE } else { abi::EBADF });
    -1
}
#[repr(C)]
pub struct Iovec {
    base: *const u8,
    len: usize,
}
pub unsafe extern "C" fn writev(fd: i32, iov: *const Iovec, n: i32) -> isize {
    let mut total = 0isize;
    for i in 0..n.max(0) as usize {
        let v = &*iov.add(i);
        let k = stdio::write(fd, v.base, v.len);
        if k < 0 {
            return if total > 0 { total } else { k };
        }
        total += k;
    }
    total
}
/// `poll`: nothing here is pollable. No descriptors: a sleep.
pub unsafe extern "C" fn poll(fds: *mut [i32; 2], n: u64, timeout: i32) -> i32 {
    if n == 0 {
        if timeout > 0 {
            hospes_rt::sys::nanosleep(timeout as i64 / 1000, (timeout as i64 % 1000) * 1_000_000);
        }
        return 0;
    }
    for i in 0..n as usize {
        (*fds.add(i))[1] = 0x20 << 16; // revents = POLLNVAL
    }
    n as i32
}
pub unsafe extern "C" fn select(n: i32, r: *mut c_void, w: *mut c_void, e: *mut c_void, tv: *const [i64; 2]) -> i32 {
    if n == 0 && r.is_null() && w.is_null() && e.is_null() {
        if !tv.is_null() {
            hospes_rt::sys::nanosleep((*tv)[0], (*tv)[1] * 1000);
        }
        return 0;
    }
    crate::deny::refuse("select", abi::EBADF);
    -1
}
pub unsafe extern "C" fn getcwd(buf: *mut u8, size: usize) -> *mut u8 {
    let b = if buf.is_null() { mem::malloc(size.max(2)) as *mut u8 } else { buf };
    if b.is_null() || (!buf.is_null() && size < 2) {
        set_errno(abi::ERANGE);
        return core::ptr::null_mut();
    }
    *b = b'/';
    *b.add(1) = 0;
    b
}
/// Paths in the virtual filesystem are already canonical.
pub unsafe extern "C" fn realpath(p: *const u8, out: *mut u8) -> *mut u8 {
    let s = bytes(p);
    if let Err(e) = vfs::stat(s) {
        set_errno(e);
        return core::ptr::null_mut();
    }
    let d = if out.is_null() { mem::malloc(s.len() + 1) as *mut u8 } else { out };
    core::ptr::copy_nonoverlapping(p, d, s.len() + 1);
    d
}
/// Nothing in the virtual filesystem is a link.
pub unsafe extern "C" fn readlink(p: *const u8, _b: *mut u8, _n: usize) -> isize {
    set_errno(match vfs::stat(bytes(p)) {
        Ok(_) => abi::EINVAL,
        Err(e) => e,
    });
    -1
}

unsafe fn fd_of(f: *mut File) -> i32 {
    stdio::fileno_of(f)
}
pub unsafe extern "C" fn fileno(f: *mut File) -> i32 {
    fd_of(f)
}
pub unsafe extern "C" fn fputs(s: *const u8, f: *mut File) -> i32 {
    let b = bytes(s);
    if stdio::fwrite(s as *const c_void, 1, b.len(), f) == b.len() { 1 } else { -1 }
}
pub unsafe extern "C" fn fread(p: *mut c_void, size: usize, n: usize, f: *mut File) -> usize {
    let Some(len) = size.checked_mul(n) else { return 0 };
    if len == 0 {
        return 0;
    }
    let out = p as *mut u8;
    let mut i = 0;
    while i < len {
        let c = stdio::getc(f);
        if c < 0 {
            break;
        }
        *out.add(i) = c as u8;
        i += 1;
    }
    i / size
}
pub unsafe extern "C" fn __fread_chk(p: *mut c_void, plen: usize, size: usize, n: usize, f: *mut File) -> usize {
    if size.checked_mul(n).is_none_or(|l| l > plen) {
        crate::die("__fread_chk: buffer overflow");
    }
    fread(p, size, n, f)
}
pub unsafe extern "C" fn ftello64(f: *mut File) -> i64 {
    let _ = f;
    set_errno(abi::ESPIPE);
    -1
}
pub unsafe extern "C" fn fdopen(fd: i32, mode: *const u8) -> *mut File {
    let _ = mode;
    stdio::fdopen_vfs(fd)
}
pub extern "C" fn setbuf(_f: *mut File, _b: *mut u8) {}
pub extern "C" fn setvbuf(_f: *mut File, _b: *mut u8, _mode: i32, _size: usize) -> i32 {
    0
}
/// `sprintf` has no bound; the caller vouches for the buffer.
pub unsafe extern "C" fn sprintf(s: *mut u8, fmt: *const u8, args: ...) -> i32 {
    stdio::vsnprintf(s, isize::MAX as usize, fmt, args)
}
pub unsafe extern "C" fn __sprintf_chk(s: *mut u8, _flag: i32, slen: usize, fmt: *const u8, args: ...) -> i32 {
    let n = stdio::vsnprintf(s, slen, fmt, args);
    if n < 0 || n as usize >= slen {
        crate::die("__sprintf_chk: buffer overflow");
    }
    n
}
pub unsafe extern "C" fn __snprintf_chk(s: *mut u8, maxlen: usize, _flag: i32, slen: usize, fmt: *const u8, args: ...) -> i32 {
    if maxlen > slen {
        crate::die("__snprintf_chk: buffer overflow");
    }
    stdio::vsnprintf(s, maxlen, fmt, args)
}
pub unsafe extern "C" fn __printf_chk(_flag: i32, fmt: *const u8, args: ...) -> i32 {
    stdio::vfprintf(stdio::stdout, fmt, args)
}

// ---- syslog: to our log ----------------------------------------------------------------

pub extern "C" fn openlog(_ident: *const u8, _opt: i32, _fac: i32) {}
pub extern "C" fn closelog() {}
pub unsafe extern "C" fn __syslog_chk(pri: i32, _flag: i32, fmt: *const u8, args: ...) {
    let mut ap: VaList = args;
    let mut line = [0u8; 512];
    let mut b = Buf::new(line.as_mut_ptr(), line.len());
    printf::format(&mut b, fmt, &mut ap);
    let n = b.len().min(line.len() - 1);
    hospes_rt::hlog!("syslog <{}>: {}", pri & 7, core::str::from_utf8(&line[..n]).unwrap_or("?"));
}
pub unsafe extern "C" fn __assert_fail(what: *const u8, file: *const u8, line: u32, _func: *const u8) -> ! {
    hospes_rt::hlog!(
        "hospes: assertion failed in the library: {} ({}:{})",
        core::str::from_utf8(bytes(what)).unwrap_or("?"),
        core::str::from_utf8(bytes(file)).unwrap_or("?"),
        line
    );
    hospes_rt::sys::exit(134)
}

// ---- time and the process -------------------------------------------------------------

pub unsafe extern "C" fn time(t: *mut i64) -> i64 {
    let mut tv = crate::time::Timeval { sec: 0, usec: 0 };
    crate::time::gettimeofday(&mut tv, core::ptr::null_mut());
    if !t.is_null() {
        *t = tv.sec;
    }
    tv.sec
}
pub extern "C" fn tzset() {}
pub unsafe extern "C" fn gmtime(t: *const i64) -> *mut crate::time::Tm {
    crate::time::localtime(t)
}
pub extern "C" fn get_nprocs() -> i32 {
    crate::process::sysconf(84) as i32 // _SC_NPROCESSORS_ONLN
}
pub unsafe extern "C" fn getentropy(b: *mut u8, n: usize) -> i32 {
    if n > 256 {
        set_errno(abi::EIO);
        return -1;
    }
    match hospes_rt::sys::getrandom(core::slice::from_raw_parts_mut(b, n)) {
        Ok(k) if k == n => 0,
        _ => {
            set_errno(abi::EIO);
            -1
        }
    }
}
/// `thread_local` destructors: the module's threads are its own and the process ends with
/// the ceremony, so they are not kept (as [`crate::process::__cxa_atexit`]).
pub extern "C" fn __cxa_thread_atexit_impl(_f: usize, _obj: usize, _dso: usize) -> i32 {
    0
}
/// `pthread_key_delete`: the slot is not reused (keys are few and live for the process).
pub extern "C" fn pthread_key_delete(_k: u32) -> i32 {
    0
}

// ---- network text, without the network --------------------------------------------------

pub unsafe extern "C" fn inet_ntoa(a: u32) -> *const u8 {
    static mut BUF: [u8; 16] = [0; 16];
    let b = &mut *core::ptr::addr_of_mut!(BUF);
    let o = a.to_ne_bytes();
    let mut w = Buf::new(b.as_mut_ptr(), 16);
    for (i, x) in o.iter().enumerate() {
        if i > 0 {
            w.byte(b'.');
        }
        dec(&mut w, *x as u32);
    }
    w.terminate();
    b.as_ptr()
}
fn dec(w: &mut Buf, v: u32) {
    if v >= 10 {
        dec(w, v / 10);
    }
    w.byte(b'0' + (v % 10) as u8);
}
/// IPv4 dotted, IPv6 as eight groups (valid, not shortened).
pub unsafe extern "C" fn inet_ntop(af: i32, src: *const u8, dst: *mut u8, size: u32) -> *const u8 {
    let mut tmp = [0u8; 48];
    let mut w = Buf::new(tmp.as_mut_ptr(), tmp.len());
    match af {
        2 => {
            for i in 0..4 {
                if i > 0 {
                    w.byte(b'.');
                }
                dec(&mut w, *src.add(i) as u32);
            }
        }
        10 => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for i in 0..8 {
                if i > 0 {
                    w.byte(b':');
                }
                let v = u16::from_be_bytes([*src.add(2 * i), *src.add(2 * i + 1)]);
                let mut started = false;
                for sh in [12, 8, 4, 0] {
                    let d = (v >> sh) & 15;
                    if d != 0 || started || sh == 0 {
                        w.byte(HEX[d as usize]);
                        started = true;
                    }
                }
            }
        }
        _ => {
            set_errno(97); // EAFNOSUPPORT
            return core::ptr::null();
        }
    }
    let n = w.len();
    if n + 1 > size as usize {
        set_errno(28); // ENOSPC
        return core::ptr::null();
    }
    core::ptr::copy_nonoverlapping(tmp.as_ptr(), dst, n);
    *dst.add(n) = 0;
    dst
}

// ---- maths --------------------------------------------------------------------------

pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}
pub unsafe extern "C" fn modf(x: f64, ip: *mut f64) -> f64 {
    let (f, i) = libm::modf(x);
    *ip = i;
    f
}

// `frexpl` on x86_64: `long double` is the x87 80-bit format, passed in memory and
// returned in `st(0)`. The stub hands the argument's bytes to `frexp80`, which rewrites
// them in place, and loads the result.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl hospes_frexpl",
    ".type hospes_frexpl,@function",
    "hospes_frexpl:",
    "sub rsp, 8",
    "mov rsi, rdi",
    "lea rdi, [rsp + 16]",
    "call {f}",
    "fld tbyte ptr [rsp + 16]",
    "add rsp, 8",
    "ret",
    f = sym frexp80,
);
#[cfg(target_arch = "x86_64")]
extern "C" {
    pub fn hospes_frexpl();
}

/// The 80-bit value at `p` becomes its significand in [0.5, 1), `*e` the exponent.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(crate) unsafe extern "C" fn frexp80(p: *mut u8, e: *mut i32) {
    let mut m = u64::from_le_bytes(core::slice::from_raw_parts(p, 8).try_into().unwrap());
    let se = u16::from_le_bytes([*p.add(8), *p.add(9)]);
    let (sign, mut exp) = (se & 0x8000, (se & 0x7fff) as i32);
    *e = 0;
    if exp == 0x7fff || m == 0 {
        return; // inf, nan, zero
    }
    if exp == 0 {
        // Denormal: normalise the explicit-integer-bit significand.
        let z = m.leading_zeros() as i32;
        m <<= z;
        exp = 1 - z;
    }
    *e = exp - 0x3ffe;
    core::ptr::copy_nonoverlapping(m.to_le_bytes().as_ptr(), p, 8);
    let ne = sign | 0x3ffe;
    *p.add(8) = ne as u8;
    *p.add(9) = (ne >> 8) as u8;
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn frexpl_addr() -> usize {
    hospes_frexpl as *const () as usize
}
#[cfg(target_arch = "aarch64")]
pub(crate) fn frexpl_addr() -> usize {
    frexpl as *const () as usize
}

/// aarch64: `long double` is IEEE binary128.
#[cfg(target_arch = "aarch64")]
pub unsafe extern "C" fn frexpl(x: f128, e: *mut i32) -> f128 {
    let b = x.to_bits();
    let (sign, exp, man) = (b >> 127, ((b >> 112) & 0x7fff) as i32, b & ((1u128 << 112) - 1));
    *e = 0;
    if exp == 0x7fff || (exp == 0 && man == 0) {
        return x;
    }
    let (exp, man) = if exp == 0 {
        let z = man.leading_zeros() as i32 - 15;
        (1 - z, (man << z) & ((1u128 << 112) - 1))
    } else {
        (exp, man)
    };
    *e = exp - 16382;
    f128::from_bits((sign << 127) | (16382u128 << 112) | man)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        unsafe {
            let mut b = [0u8; 32];
            strcpy(b.as_mut_ptr(), b"ab\0".as_ptr());
            strcat(b.as_mut_ptr(), b"cd\0".as_ptr());
            assert_eq!(&b[..5], b"abcd\0");
            assert_eq!(stpcpy(b.as_mut_ptr(), b"xyz\0".as_ptr()), b.as_mut_ptr().add(3));
            assert_eq!(strcasecmp(b"HeLLo\0".as_ptr(), b"hello\0".as_ptr()), 0);
            assert!(strncasecmp(b"abc\0".as_ptr(), b"abd\0".as_ptr(), 3) < 0);
            assert_eq!(strncasecmp(b"abc\0".as_ptr(), b"abd\0".as_ptr(), 2), 0);
            assert_eq!(strspn(b"  x\0".as_ptr(), b" \0".as_ptr()), 2);
            assert_eq!(strcspn(b"ab=c\0".as_ptr(), b"=\0".as_ptr()), 2);
            let h = b"find the needle\0";
            assert_eq!(strstr(h.as_ptr(), b"needle\0".as_ptr()), h.as_ptr().add(9));
            assert!(strstr(h.as_ptr(), b"pin\0".as_ptr()).is_null());
            let mut t = *b"a, b,,c\0";
            let mut save = core::ptr::null_mut();
            let mut out = Vec::new();
            let mut p = strtok_r(t.as_mut_ptr(), b", \0".as_ptr(), &mut save);
            while !p.is_null() {
                out.push(bytes(p).to_vec());
                p = strtok_r(core::ptr::null_mut(), b", \0".as_ptr(), &mut save);
            }
            assert_eq!(out, [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
            let mut e = [0u8; 4];
            assert_eq!(__xpg_strerror_r(2, e.as_mut_ptr(), 4), abi::ERANGE);
            assert_eq!(e[3], 0);
        }
    }

    #[test]
    fn wide_and_locale() {
        unsafe {
            let a = [b'a' as u32, b'b' as u32, 0];
            let c = [b'a' as u32, b'c' as u32, 0];
            assert!(wcscmp(a.as_ptr(), c.as_ptr()) < 0);
            assert_eq!(wcscmp(a.as_ptr(), a.as_ptr()), 0);
            assert_eq!(btowc(0x41), 0x41);
            assert_eq!(btowc(0xe9), WEOF);
            let alpha = __wctype_l(b"alpha\0".as_ptr(), core::ptr::null_mut());
            assert_eq!(__iswctype_l(b'q' as u32, alpha, core::ptr::null_mut()), 1);
            assert_eq!(__iswctype_l(b'7' as u32, alpha, core::ptr::null_mut()), 0);
            assert_eq!(bytes(__nl_langinfo_l(14, core::ptr::null_mut())), b"ANSI_X3.4-1968");
            assert_eq!(bytes(__nl_langinfo_l(0x2001a, core::ptr::null_mut())), b"January");
            assert_eq!(bytes(setlocale(6, b"\0".as_ptr())), b"C");
            assert!(setlocale(6, b"de_DE.UTF-8\0".as_ptr()).is_null());
            assert_eq!(bytes((*localeconv()).s[0]), b".");
        }
    }

    #[test]
    fn network_text() {
        unsafe {
            assert_eq!(bytes(inet_ntoa(u32::from_ne_bytes([192, 168, 1, 20]))), b"192.168.1.20");
            let mut d = [0u8; 46];
            let v6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            assert!(!inet_ntop(10, v6.as_ptr(), d.as_mut_ptr(), 46).is_null());
            assert_eq!(bytes(d.as_ptr()), b"2001:db8:0:0:0:0:0:1");
            assert!(inet_ntop(2, [1, 2, 3, 4].as_ptr(), d.as_mut_ptr(), 4).is_null());
        }
    }

    #[test]
    fn frexp_80() {
        unsafe {
            // 12.0 as x87 extended: significand 0xC000.., exponent 0x3fff + 3.
            let mut v = [0u8; 10];
            v[..8].copy_from_slice(&0xC000_0000_0000_0000u64.to_le_bytes());
            v[8..].copy_from_slice(&(0x3fffu16 + 3).to_le_bytes());
            let mut e = 0;
            frexp80(v.as_mut_ptr(), &mut e);
            assert_eq!(e, 4); // 12 = 0.75 * 2^4
            assert_eq!(u16::from_le_bytes([v[8], v[9]]), 0x3ffe);
            let mut z = [0u8; 10];
            frexp80(z.as_mut_ptr(), &mut e);
            assert_eq!(e, 0);
        }
    }

    #[test]
    fn writev_to_the_log_and_poll() {
        unsafe {
            let parts = [Iovec { base: b"one \0".as_ptr(), len: 4 }, Iovec { base: b"two\n\0".as_ptr(), len: 4 }];
            assert_eq!(writev(2, parts.as_ptr(), 2), 8);
            assert_eq!(writev(7, parts.as_ptr(), 2), -1);
            let mut p = [[5i32, 1]];
            assert_eq!(poll(p.as_mut_ptr(), 1, 0), 1);
            assert_eq!(p[0][1] >> 16, 0x20);
            let mut buf = [0u8; 8];
            assert!(!getcwd(buf.as_mut_ptr(), 8).is_null());
            assert_eq!(&buf[..2], b"/\0");
        }
    }
}
