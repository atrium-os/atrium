//! The "C" locale, the only one there is: glibc's character-class tables, `locale_t`
//! objects, and the multibyte functions. In glibc's C locale the character set is ASCII:
//! bytes from 0x80 are not characters (`EILSEQ`), and `MB_CUR_MAX` is 1.
//!
//! The static C++ runtime inside the CDM (libc++ on glibc) reads the tables through
//! `__ctype_b_loc` and friends, and may read them out of a `locale_t` directly, so both
//! carry glibc's layout.
use crate::{abi, set_errno};
use core::ffi::c_void;

// glibc's <ctype.h> bits (little-endian layout of the 16-bit mask).
const UPPER: u16 = 0x100;
const LOWER: u16 = 0x200;
const ALPHA: u16 = 0x400;
const DIGIT: u16 = 0x800;
const XDIGIT: u16 = 0x1000;
const SPACE: u16 = 0x2000;
const PRINT: u16 = 0x4000;
const GRAPH: u16 = 0x8000;
const BLANK: u16 = 0x1;
const CNTRL: u16 = 0x2;
const PUNCT: u16 = 0x4;
const ALNUM: u16 = 0x8;

const fn class(c: u8) -> u16 {
    let mut m = 0;
    if c.is_ascii_uppercase() {
        m |= UPPER | ALPHA | ALNUM;
    }
    if c.is_ascii_lowercase() {
        m |= LOWER | ALPHA | ALNUM;
    }
    if c.is_ascii_digit() {
        m |= DIGIT | ALNUM;
    }
    if c.is_ascii_hexdigit() {
        m |= XDIGIT;
    }
    if matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
        m |= SPACE;
    }
    if c >= 0x20 && c < 0x7f {
        m |= PRINT;
    }
    if c > 0x20 && c < 0x7f {
        m |= GRAPH;
    }
    if c == b' ' || c == b'\t' {
        m |= BLANK;
    }
    if c < 0x20 || c == 0x7f {
        m |= CNTRL;
    }
    if c.is_ascii_punctuation() {
        m |= PUNCT;
    }
    m
}

/// Indexed from -128 (signed `char` values) to 255; `EOF` (-1) is all-zero.
const fn b_table() -> [u16; 384] {
    let mut t = [0u16; 384];
    let mut c = 0;
    while c < 128 {
        t[128 + c] = class(c as u8);
        c += 1;
    }
    t
}
const fn case_table(upper: bool) -> [i32; 384] {
    let mut t = [0i32; 384];
    let mut i = 0;
    while i < 384 {
        let c = i as i32 - 128;
        t[i] = if c >= 0 && c < 128 {
            let b = c as u8;
            if upper { b.to_ascii_uppercase() as i32 } else { b.to_ascii_lowercase() as i32 }
        } else if c < -1 {
            // A signed char above 0x7f maps to its unsigned value.
            c + 256
        } else {
            // EOF, and the bytes above 0x7f, map to themselves.
            c
        };
        i += 1;
    }
    t
}

static B: [u16; 384] = b_table();
static TOLOWER: [i32; 384] = case_table(false);
static TOUPPER: [i32; 384] = case_table(true);

/// The pointers the `__ctype_*_loc` functions hand out (glibc keeps them per thread; ours
/// never change, so one copy serves every thread).
static mut B_PTR: *const u16 = core::ptr::null();
static mut TOLOWER_PTR: *const i32 = core::ptr::null();
static mut TOUPPER_PTR: *const i32 = core::ptr::null();

/// glibc's `struct __locale_struct`: 13 category pointers, the three tables, 13 names.
#[repr(C)]
pub struct Locale {
    locales: [*const c_void; 13],
    ctype_b: *const u16,
    ctype_tolower: *const i32,
    ctype_toupper: *const i32,
    names: [*const u8; 13],
}

static mut C_LOCALE: Locale = Locale {
    locales: [core::ptr::null(); 13],
    ctype_b: core::ptr::null(),
    ctype_tolower: core::ptr::null(),
    ctype_toupper: core::ptr::null(),
    names: [b"C\0".as_ptr(); 13],
};

/// `LC_GLOBAL_LOCALE`.
const GLOBAL: usize = usize::MAX;
/// The locale `uselocale` last set (shared by all threads: every locale is "C").
static CURRENT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(GLOBAL);

pub(crate) fn init() {
    unsafe {
        B_PTR = B.as_ptr().add(128);
        TOLOWER_PTR = TOLOWER.as_ptr().add(128);
        TOUPPER_PTR = TOUPPER.as_ptr().add(128);
        let l = &mut *core::ptr::addr_of_mut!(C_LOCALE);
        l.ctype_b = B_PTR;
        l.ctype_tolower = TOLOWER_PTR;
        l.ctype_toupper = TOUPPER_PTR;
    }
}

pub extern "C" fn __ctype_b_loc() -> *const *const u16 {
    core::ptr::addr_of!(B_PTR)
}
pub extern "C" fn __ctype_tolower_loc() -> *const *const i32 {
    core::ptr::addr_of!(TOLOWER_PTR)
}
pub extern "C" fn __ctype_toupper_loc() -> *const *const i32 {
    core::ptr::addr_of!(TOUPPER_PTR)
}
pub extern "C" fn __ctype_get_mb_cur_max() -> usize {
    1
}

/// Every name is the C locale: "C", "POSIX" and "" (the environment's, which is C here).
pub unsafe extern "C" fn newlocale(_mask: i32, name: *const u8, _base: *mut c_void) -> *mut Locale {
    let n = if name.is_null() { &b""[..] } else { core::slice::from_raw_parts(name, crate::mem::strlen(name)) };
    if n.is_empty() || n == b"C" || n == b"POSIX" {
        core::ptr::addr_of_mut!(C_LOCALE)
    } else {
        set_errno(abi::ENOENT);
        core::ptr::null_mut()
    }
}
pub extern "C" fn freelocale(_l: *mut Locale) {}
pub extern "C" fn uselocale(l: *mut Locale) -> *mut Locale {
    use core::sync::atomic::Ordering;
    let prev = CURRENT.load(Ordering::Relaxed);
    if !l.is_null() {
        CURRENT.store(l as usize, Ordering::Relaxed);
    }
    prev as *mut Locale
}

// ---- multibyte, ASCII only -----------------------------------------------------------

/// `mbrtowc`: one byte is one character below 0x80; above, `EILSEQ`.
pub unsafe extern "C" fn mbrtowc(pwc: *mut u32, s: *const u8, n: usize, _ps: *mut c_void) -> usize {
    if s.is_null() {
        return 0;
    }
    if n == 0 {
        return usize::MAX - 1; // (size_t)-2: incomplete
    }
    let c = *s;
    if c >= 0x80 {
        set_errno(abi::EILSEQ);
        return usize::MAX;
    }
    if !pwc.is_null() {
        *pwc = c as u32;
    }
    (c != 0) as usize
}
pub unsafe extern "C" fn __mbrlen(s: *const u8, n: usize, ps: *mut c_void) -> usize {
    mbrtowc(core::ptr::null_mut(), s, n, ps)
}
pub unsafe extern "C" fn mbtowc(pwc: *mut u32, s: *const u8, n: usize) -> i32 {
    if s.is_null() {
        return 0; // stateless
    }
    match mbrtowc(pwc, s, n, core::ptr::null_mut()) {
        r if r >= usize::MAX - 1 => {
            set_errno(abi::EILSEQ);
            -1
        }
        r => r as i32,
    }
}
pub unsafe extern "C" fn wcrtomb(s: *mut u8, wc: u32, _ps: *mut c_void) -> usize {
    if s.is_null() {
        return 1;
    }
    if wc >= 0x80 {
        set_errno(abi::EILSEQ);
        return usize::MAX;
    }
    *s = wc as u8;
    1
}

/// `mbsnrtowcs`: at most `nms` bytes of `*src`, at most `len` characters into `dst`.
pub unsafe extern "C" fn mbsnrtowcs(dst: *mut u32, src: *mut *const u8, nms: usize, len: usize, _ps: *mut c_void) -> usize {
    let mut s = *src;
    let mut n = 0;
    let mut used = 0;
    while used < nms && (dst.is_null() || n < len) {
        let c = *s;
        if c >= 0x80 {
            set_errno(abi::EILSEQ);
            if !dst.is_null() {
                *src = s;
            }
            return usize::MAX;
        }
        if !dst.is_null() {
            *dst.add(n) = c as u32;
        }
        if c == 0 {
            if !dst.is_null() {
                *src = core::ptr::null();
            }
            return n;
        }
        s = s.add(1);
        used += 1;
        n += 1;
    }
    if !dst.is_null() {
        *src = s;
    }
    n
}
pub unsafe extern "C" fn mbsrtowcs(dst: *mut u32, src: *mut *const u8, len: usize, ps: *mut c_void) -> usize {
    mbsnrtowcs(dst, src, usize::MAX, len, ps)
}
pub unsafe extern "C" fn wcsnrtombs(dst: *mut u8, src: *mut *const u32, nwc: usize, len: usize, _ps: *mut c_void) -> usize {
    let mut s = *src;
    let mut n = 0;
    let mut used = 0;
    while used < nwc && (dst.is_null() || n < len) {
        let c = *s;
        if c >= 0x80 {
            set_errno(abi::EILSEQ);
            if !dst.is_null() {
                *src = s;
            }
            return usize::MAX;
        }
        if !dst.is_null() {
            *dst.add(n) = c as u8;
        }
        if c == 0 {
            if !dst.is_null() {
                *src = core::ptr::null();
            }
            return n;
        }
        s = s.add(1);
        used += 1;
        n += 1;
    }
    if !dst.is_null() {
        *src = s;
    }
    n
}

// ---- message catalogues: there are none ------------------------------------------------

pub extern "C" fn catopen(_name: *const u8, _flag: i32) -> isize {
    set_errno(abi::ENOENT);
    -1
}
pub extern "C" fn catgets(_cat: isize, _set: i32, _num: i32, s: *const u8) -> *const u8 {
    set_errno(abi::EBADF);
    s
}
pub extern "C" fn catclose(_cat: isize) -> i32 {
    set_errno(abi::EBADF);
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        #[link_name = "__ctype_b_loc"]
        fn c_b_loc() -> *const *const u16;
        #[link_name = "__ctype_tolower_loc"]
        fn c_tolower_loc() -> *const *const i32;
        #[link_name = "__ctype_toupper_loc"]
        fn c_toupper_loc() -> *const *const i32;
    }

    /// The test process runs in glibc's C locale (no setlocale call), so its tables are
    /// the reference.
    #[test]
    fn tables_match_glibc_c_locale() {
        init();
        unsafe {
            let (ours, theirs) = (*__ctype_b_loc(), *c_b_loc());
            let (ol, tl) = (*__ctype_tolower_loc(), *c_tolower_loc());
            let (ou, tu) = (*__ctype_toupper_loc(), *c_toupper_loc());
            for c in -128..256isize {
                assert_eq!(*ours.offset(c), *theirs.offset(c), "class of {c}");
                assert_eq!(*ol.offset(c), *tl.offset(c), "tolower of {c}");
                assert_eq!(*ou.offset(c), *tu.offset(c), "toupper of {c}");
            }
        }
    }

    #[test]
    fn multibyte_is_ascii() {
        unsafe {
            let mut w = 0u32;
            assert_eq!(mbrtowc(&mut w, b"A".as_ptr(), 1, core::ptr::null_mut()), 1);
            assert_eq!(w, 65);
            assert_eq!(mbrtowc(&mut w, b"\xc3".as_ptr(), 1, core::ptr::null_mut()), usize::MAX);
            let mut b = [0u8; 2];
            assert_eq!(wcrtomb(b.as_mut_ptr(), 0x7a, core::ptr::null_mut()), 1);
            assert_eq!(wcrtomb(b.as_mut_ptr(), 0xe9, core::ptr::null_mut()), usize::MAX);
            let src = b"hi\0";
            let mut p = src.as_ptr();
            let mut out = [0u32; 4];
            assert_eq!(mbsrtowcs(out.as_mut_ptr(), &mut p, 4, core::ptr::null_mut()), 2);
            assert!(p.is_null());
            assert_eq!(&out[..3], &[104, 105, 0]);
            let l = newlocale(0x1fbf, b"C\0".as_ptr(), core::ptr::null_mut());
            assert!(!l.is_null());
            assert!(newlocale(0x1fbf, b"de_DE\0".as_ptr(), core::ptr::null_mut()).is_null());
        }
    }
}
