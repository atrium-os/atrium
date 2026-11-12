//! Strings, numbers from text, sorting, error strings (H4: Widevine's imports). The `_l`
//! variants take a `locale_t` and ignore it: the only locale is "C" ([`crate::locale`]).
use crate::{abi, die, set_errno};
use core::ffi::c_void;

pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: usize) -> *const u8 {
    let c = c as u8;
    for i in 0..n {
        if *s.add(i) == c {
            return s.add(i);
        }
    }
    core::ptr::null()
}

pub unsafe extern "C" fn strchr(s: *const u8, c: i32) -> *const u8 {
    let c = c as u8;
    let mut p = s;
    loop {
        if *p == c {
            return p;
        }
        if *p == 0 {
            return core::ptr::null();
        }
        p = p.add(1);
    }
}

pub unsafe extern "C" fn strrchr(s: *const u8, c: i32) -> *const u8 {
    let c = c as u8;
    let mut last = core::ptr::null();
    let mut p = s;
    loop {
        if *p == c {
            last = p;
        }
        if *p == 0 {
            return last;
        }
        p = p.add(1);
    }
}

pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    strncmp(a, b, usize::MAX)
}

pub unsafe extern "C" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
    }
    0
}

/// `wchar_t` is 32 bits on both machines.
pub unsafe extern "C" fn wcslen(s: *const u32) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

pub unsafe extern "C" fn wmemchr(s: *const u32, c: u32, n: usize) -> *const u32 {
    for i in 0..n {
        if *s.add(i) == c {
            return s.add(i);
        }
    }
    core::ptr::null()
}

pub unsafe extern "C" fn __memcpy_chk(d: *mut u8, s: *const u8, n: usize, dlen: usize) -> *mut u8 {
    if n > dlen {
        die("__memcpy_chk: buffer overflow");
    }
    core::ptr::copy_nonoverlapping(s, d, n);
    d
}

pub unsafe extern "C" fn __strncat_chk(d: *mut u8, s: *const u8, n: usize, dlen: usize) -> *mut u8 {
    let dl = crate::mem::strlen(d);
    let mut i = 0;
    while i < n && *s.add(i) != 0 {
        i += 1;
    }
    if dl + i + 1 > dlen {
        die("__strncat_chk: buffer overflow");
    }
    core::ptr::copy_nonoverlapping(s, d.add(dl), i);
    *d.add(dl + i) = 0;
    d
}

// ---- sorting, random ------------------------------------------------------------------

type Cmp = unsafe extern "C" fn(*const c_void, *const c_void) -> i32;

unsafe fn swap(a: *mut u8, b: *mut u8, size: usize) {
    for i in 0..size {
        core::ptr::swap(a.add(i), b.add(i));
    }
}

/// Heapsort: in place, no allocation, O(n log n). glibc's `qsort` is not stable either.
pub unsafe extern "C" fn qsort(base: *mut c_void, n: usize, size: usize, cmp: Cmp) {
    if n < 2 || size == 0 {
        return;
    }
    let b = base as *mut u8;
    let at = |i: usize| b.add(i * size);
    let sift = |mut root: usize, end: usize| {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                break;
            }
            if child + 1 < end && cmp(at(child) as _, at(child + 1) as _) < 0 {
                child += 1;
            }
            if cmp(at(root) as _, at(child) as _) >= 0 {
                break;
            }
            swap(at(root), at(child), size);
            root = child;
        }
    };
    for start in (0..n / 2).rev() {
        sift(start, n);
    }
    for end in (1..n).rev() {
        swap(at(0), at(end), size);
        sift(0, end);
    }
}

static RAND: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// `rand(3)`: an LCG seeded at 1, as `srand` was never called (the CDM imports no `srand`).
/// Not glibc's sequence; nothing may depend on it being random.
pub extern "C" fn rand() -> i32 {
    use core::sync::atomic::Ordering;
    let mut s = RAND.load(Ordering::Relaxed);
    loop {
        let n = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
        match RAND.compare_exchange_weak(s, n, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ((n >> 1) & 0x7fff_ffff) as i32,
            Err(v) => s = v,
        }
    }
}

// ---- error strings ----------------------------------------------------------------------

fn errname(e: i32) -> &'static [u8] {
    match e {
        0 => b"Success\0",
        abi::EPERM => b"Operation not permitted\0",
        abi::ENOENT => b"No such file or directory\0",
        abi::EINTR => b"Interrupted system call\0",
        abi::EIO => b"Input/output error\0",
        abi::EBADF => b"Bad file descriptor\0",
        abi::EAGAIN => b"Resource temporarily unavailable\0",
        abi::ENOMEM => b"Cannot allocate memory\0",
        abi::EACCES => b"Permission denied\0",
        abi::EFAULT => b"Bad address\0",
        abi::EBUSY => b"Device or resource busy\0",
        abi::EINVAL => b"Invalid argument\0",
        abi::EMFILE => b"Too many open files\0",
        abi::ERANGE => b"Numerical result out of range\0",
        abi::ENAMETOOLONG => b"File name too long\0",
        abi::ENOSYS => b"Function not implemented\0",
        abi::EILSEQ => b"Invalid or incomplete multibyte or wide character\0",
        abi::ETIMEDOUT => b"Connection timed out\0",
        _ => b"Unknown error\0",
    }
}

pub extern "C" fn strerror(e: i32) -> *const u8 {
    errname(e).as_ptr()
}

/// The GNU `strerror_r` (the default `strerror_r` symbol): returns the string, which may
/// be a static one rather than `buf`.
pub extern "C" fn strerror_r(e: i32, _buf: *mut u8, _len: usize) -> *const u8 {
    errname(e).as_ptr()
}

// ---- numbers from text ------------------------------------------------------------------

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn digit(c: u8, base: u32) -> Option<u32> {
    let d = match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'z' => (c - b'a') as u32 + 10,
        b'A'..=b'Z' => (c - b'A') as u32 + 10,
        _ => return None,
    };
    (d < base).then_some(d)
}

/// Parse as `strtoull` does: `(magnitude, negative, overflowed, end)`. `end` is `s` when
/// no digits were found.
pub(crate) unsafe fn parse_uint(s: *const u8, base: i32) -> Option<(u64, bool, bool, *const u8)> {
    if base != 0 && !(2..=36).contains(&base) {
        return None;
    }
    let mut p = s;
    while is_space(*p) {
        p = p.add(1);
    }
    let mut neg = false;
    if *p == b'+' || *p == b'-' {
        neg = *p == b'-';
        p = p.add(1);
    }
    let mut base = base as u32;
    if (base == 0 || base == 16) && *p == b'0' && (*p.add(1) | 0x20) == b'x' && digit(*p.add(2), 16).is_some() {
        p = p.add(2);
        base = 16;
    } else if base == 0 {
        base = if *p == b'0' { 8 } else { 10 };
    }
    let start = p;
    let mut v: u64 = 0;
    let mut over = false;
    while let Some(d) = digit(*p, base) {
        match v.checked_mul(base as u64).and_then(|x| x.checked_add(d as u64)) {
            Some(x) => v = x,
            None => over = true,
        }
        p = p.add(1);
    }
    if p == start {
        return Some((0, false, false, s));
    }
    Some((v, neg, over, p))
}

unsafe fn set_end(end: *mut *const u8, p: *const u8) {
    if !end.is_null() {
        *end = p;
    }
}

pub unsafe extern "C" fn strtoull(s: *const u8, end: *mut *const u8, base: i32) -> u64 {
    let Some((v, neg, over, p)) = parse_uint(s, base) else {
        set_errno(abi::EINVAL);
        return 0;
    };
    set_end(end, p);
    if over {
        set_errno(abi::ERANGE);
        return u64::MAX;
    }
    if neg { v.wrapping_neg() } else { v }
}

pub unsafe extern "C" fn strtoll(s: *const u8, end: *mut *const u8, base: i32) -> i64 {
    let Some((v, neg, over, p)) = parse_uint(s, base) else {
        set_errno(abi::EINVAL);
        return 0;
    };
    set_end(end, p);
    let limit = if neg { 1u64 << 63 } else { i64::MAX as u64 };
    if over || v > limit {
        set_errno(abi::ERANGE);
        return if neg { i64::MIN } else { i64::MAX };
    }
    if neg { (v as i64).wrapping_neg() } else { v as i64 }
}

// `long` is 64 bits on both machines.
pub unsafe extern "C" fn strtol(s: *const u8, end: *mut *const u8, base: i32) -> i64 {
    strtoll(s, end, base)
}
pub unsafe extern "C" fn strtoul(s: *const u8, end: *mut *const u8, base: i32) -> u64 {
    strtoull(s, end, base)
}
pub unsafe extern "C" fn strtoll_l(s: *const u8, end: *mut *const u8, base: i32, _l: *mut c_void) -> i64 {
    strtoll(s, end, base)
}
pub unsafe extern "C" fn strtoull_l(s: *const u8, end: *mut *const u8, base: i32, _l: *mut c_void) -> u64 {
    strtoull(s, end, base)
}

/// `strtod` in the C locale: decimal and hexadecimal floats, `inf`, `infinity`, `nan`,
/// `nan(...)`. Correctly rounded via core's parser for decimal input.
pub(crate) unsafe fn parse_f64(s: *const u8) -> (f64, *const u8) {
    let mut p = s;
    while is_space(*p) {
        p = p.add(1);
    }
    let mut neg = false;
    if *p == b'+' || *p == b'-' {
        neg = *p == b'-';
        p = p.add(1);
    }
    let sign = |v: f64| if neg { -v } else { v };
    let lower = |q: *const u8, w: &[u8]| (0..w.len()).all(|i| (*q.add(i) | 0x20) == w[i]);
    if lower(p, b"inf") {
        let n = if lower(p, b"infinity") { 8 } else { 3 };
        return (sign(f64::INFINITY), p.add(n));
    }
    if lower(p, b"nan") {
        let mut q = p.add(3);
        if *q == b'(' {
            let mut r = q.add(1);
            while (*r).is_ascii_alphanumeric() || *r == b'_' {
                r = r.add(1);
            }
            if *r == b')' {
                q = r.add(1);
            }
        }
        return (sign(f64::NAN), q);
    }
    if *p == b'0' && (*p.add(1) | 0x20) == b'x' {
        if let Some((v, q)) = parse_hex_float(p.add(2)) {
            return (sign(v), q);
        }
    }
    // Decimal: collect [digits][.digits][e[+-]digits] and hand the text to core.
    let start = p;
    let mut q = p;
    let mut digits = 0;
    while (*q).is_ascii_digit() {
        q = q.add(1);
        digits += 1;
    }
    if *q == b'.' {
        q = q.add(1);
        while (*q).is_ascii_digit() {
            q = q.add(1);
            digits += 1;
        }
    }
    if digits == 0 {
        return (0.0, s);
    }
    if (*q | 0x20) == b'e' {
        let mut e = q.add(1);
        if *e == b'+' || *e == b'-' {
            e = e.add(1);
        }
        if (*e).is_ascii_digit() {
            while (*e).is_ascii_digit() {
                e = e.add(1);
            }
            q = e;
        }
    }
    let text = core::slice::from_raw_parts(start, q.offset_from(start) as usize);
    let v = core::str::from_utf8(text).ok().and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0);
    if v.is_infinite() || (v == 0.0 && text.iter().any(|&c| (b'1'..=b'9').contains(&c)) && !mantissa_zero(text)) {
        set_errno(abi::ERANGE);
    }
    (sign(v), q)
}

fn mantissa_zero(t: &[u8]) -> bool {
    t.iter().take_while(|&&c| (c | 0x20) != b'e').all(|&c| c == b'0' || c == b'.')
}

unsafe fn parse_hex_float(p: *const u8) -> Option<(f64, *const u8)> {
    let mut q = p;
    let mut mant: u64 = 0;
    let mut exp: i64 = 0;
    let mut any = false;
    let mut seen_dot = false;
    loop {
        let c = *q;
        if c == b'.' && !seen_dot {
            seen_dot = true;
        } else if let Some(d) = digit(c, 16) {
            any = true;
            if mant >> 60 == 0 {
                mant = mant << 4 | d as u64;
                if seen_dot {
                    exp -= 4;
                }
            } else if !seen_dot {
                exp += 4;
            }
        } else {
            break;
        }
        q = q.add(1);
    }
    if !any {
        return None;
    }
    if (*q | 0x20) == b'p' {
        let mut e = q.add(1);
        let mut eneg = false;
        if *e == b'+' || *e == b'-' {
            eneg = *e == b'-';
            e = e.add(1);
        }
        if (*e).is_ascii_digit() {
            let mut v: i64 = 0;
            while (*e).is_ascii_digit() {
                v = (v * 10 + (*e - b'0') as i64).min(100_000);
                e = e.add(1);
            }
            exp += if eneg { -v } else { v };
            q = e;
        }
    }
    Some((mant as f64 * libm::exp2(exp as f64), q))
}

pub unsafe extern "C" fn strtod(s: *const u8, end: *mut *const u8) -> f64 {
    let (v, p) = parse_f64(s);
    set_end(end, p);
    v
}
pub unsafe extern "C" fn strtod_l(s: *const u8, end: *mut *const u8, _l: *mut c_void) -> f64 {
    strtod(s, end)
}
pub unsafe extern "C" fn strtof_l(s: *const u8, end: *mut *const u8, _l: *mut c_void) -> f32 {
    strtod(s, end) as f32
}

// `long double` is x87 80-bit on x86_64 (returned in `st(0)`) and IEEE quad on aarch64
// (returned in `q0`). We parse to `double` and widen: correct for every value a double
// holds; extra precision in the text is rounded away.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl hospes_strtold_l",
    "hospes_strtold_l:",
    "sub rsp, 8",
    "call {f}",
    "movsd qword ptr [rsp], xmm0",
    "fld qword ptr [rsp]",
    "add rsp, 8",
    "ret",
    f = sym strtod_for_ld,
);
#[cfg(target_arch = "x86_64")]
unsafe extern "C" fn strtod_for_ld(s: *const u8, end: *mut *const u8, _l: *mut c_void) -> f64 {
    strtod(s, end)
}
#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn hospes_strtold_l();
}
#[cfg(target_arch = "x86_64")]
pub fn strtold_l_addr() -> usize {
    hospes_strtold_l as *const () as usize
}

#[cfg(target_arch = "aarch64")]
pub unsafe extern "C" fn strtold_l(s: *const u8, end: *mut *const u8, _l: *mut c_void) -> f128 {
    strtod(s, end) as f128
}
#[cfg(target_arch = "aarch64")]
pub fn strtold_l_addr() -> usize {
    strtold_l as *const () as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        #[link_name = "strtoll"]
        fn c_strtoll(s: *const u8, e: *mut *const u8, b: i32) -> i64;
        #[link_name = "strtoull"]
        fn c_strtoull(s: *const u8, e: *mut *const u8, b: i32) -> u64;
        #[link_name = "strtod"]
        fn c_strtod(s: *const u8, e: *mut *const u8) -> f64;
    }

    const INTS: &[&[u8]] = &[
        b"0\0", b"42\0", b"  -17x\0", b"+9\0", b"0x1fZ\0", b"0X\0", b"077\0", b"08\0", b"-0\0",
        b"9223372036854775807\0", b"9223372036854775808\0", b"-9223372036854775808\0",
        b"-9223372036854775809\0", b"18446744073709551615\0", b"18446744073709551616\0", b"\0", b"-\0", b"z\0",
    ];

    #[test]
    fn integers_match_glibc() {
        for &s in INTS {
            for base in [0, 8, 10, 16, 36] {
                unsafe {
                    let (mut e1, mut e2) = (core::ptr::null(), core::ptr::null());
                    assert_eq!(strtoll(s.as_ptr(), &mut e1, base), c_strtoll(s.as_ptr(), &mut e2, base), "{s:?} {base}");
                    assert_eq!(e1, e2, "{s:?} {base}");
                    assert_eq!(strtoull(s.as_ptr(), &mut e1, base), c_strtoull(s.as_ptr(), &mut e2, base), "{s:?} {base}");
                    assert_eq!(e1, e2, "{s:?} {base}");
                }
            }
        }
    }

    #[test]
    fn floats_match_glibc() {
        let cases: &[&[u8]] = &[
            b"0\0", b"1.5\0", b"  -2.25e3x\0", b".5\0", b"5.\0", b"1e\0", b"1e+\0", b"inf\0", b"-Infinity\0",
            b"0x1.8p1\0", b"0x10\0", b"1e400\0", b"3.14159265358979323846\0", b"abc\0", b"\0", b"0.1\0",
            b"123456789012345678901234567890\0", b"2.2250738585072014e-308\0", b"0x.8p-2\0",
        ];
        for &s in cases {
            unsafe {
                let (mut e1, mut e2) = (core::ptr::null(), core::ptr::null());
                let (a, b) = (strtod(s.as_ptr(), &mut e1), c_strtod(s.as_ptr(), &mut e2));
                assert!(a == b || (a.is_nan() && b.is_nan()), "{s:?}: {a} vs {b}");
                assert_eq!(e1, e2, "{s:?}");
            }
        }
        unsafe {
            let mut e = core::ptr::null();
            assert!(strtod(b"nan(123)x\0".as_ptr(), &mut e).is_nan());
            assert_eq!(*e, b'x');
        }
    }

    #[test]
    fn qsort_sorts() {
        unsafe extern "C" fn cmp(a: *const c_void, b: *const c_void) -> i32 {
            unsafe { (*(a as *const i32)).cmp(&*(b as *const i32)) as i32 }
        }
        let mut v: [i32; 9] = [5, -1, 9, 3, 3, 0, 8, -7, 2];
        unsafe { qsort(v.as_mut_ptr() as _, v.len(), 4, cmp) };
        assert_eq!(v, [-7, -1, 0, 2, 3, 3, 5, 8, 9]);
    }

    #[test]
    fn strings() {
        unsafe {
            let s = b"hello\0";
            assert_eq!(strchr(s.as_ptr(), b'l' as i32), s.as_ptr().add(2));
            assert_eq!(strrchr(s.as_ptr(), b'l' as i32), s.as_ptr().add(3));
            assert_eq!(strchr(s.as_ptr(), 0), s.as_ptr().add(5));
            assert!(strchr(s.as_ptr(), b'z' as i32).is_null());
            assert!(strcmp(b"abc\0".as_ptr(), b"abd\0".as_ptr()) < 0);
            assert_eq!(strncmp(b"abc\0".as_ptr(), b"abd\0".as_ptr(), 2), 0);
            assert_eq!(memchr(s.as_ptr(), b'o' as i32, 5), s.as_ptr().add(4));
        }
    }
}
