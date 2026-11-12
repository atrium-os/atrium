//! `sscanf` / `vsscanf` (glibc's C99 entry points `__isoc99_*`) over a string, in the C
//! locale: integers (`d i u o x X p`), floats (`f e g a`, as `float` or `double`), `s`, `c`,
//! scan sets `[...]`, `n`, `%%`, field widths and `*` suppression. `long double` (`L`) is
//! refused (logged) rather than stored wrongly.
use crate::string::{parse_f64, parse_uint};
use core::ffi::VaList;

fn space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// Copy at most `width` bytes of `s` (to its NUL) into `tmp`, NUL-terminated.
unsafe fn field<'a>(s: *const u8, width: usize, tmp: &'a mut [u8; 128]) -> &'a [u8; 128] {
    let n = width.min(tmp.len() - 1);
    let mut i = 0;
    while i < n && *s.add(i) != 0 {
        tmp[i] = *s.add(i);
        i += 1;
    }
    tmp[i] = 0;
    tmp
}

unsafe fn store_int(p: *mut u8, size: usize, v: u64) {
    match size {
        1 => *p = v as u8,
        2 => *(p as *mut u16) = v as u16,
        4 => *(p as *mut u32) = v as u32,
        _ => *(p as *mut u64) = v,
    }
}

/// Does `c` belong to the scan set starting at `set` (just after `[`, `^` already taken)?
unsafe fn in_set(set: *const u8, end: *const u8, c: u8) -> bool {
    let mut p = set;
    while p < end {
        if *p.add(1) == b'-' && p.add(2) < end {
            if (*p..=*p.add(2)).contains(&c) {
                return true;
            }
            p = p.add(3);
        } else {
            if *p == c {
                return true;
            }
            p = p.add(1);
        }
    }
    false
}

pub unsafe fn scan(input: *const u8, fmt: *const u8, ap: &mut VaList) -> i32 {
    let mut s = input;
    let mut f = fmt;
    let mut count = 0i32;
    // EOF if the input ends before the first conversion.
    let eof = |s: *const u8, count: i32| if *s == 0 && count == 0 { -1 } else { count };
    while *f != 0 {
        if space(*f) {
            while space(*f) {
                f = f.add(1);
            }
            while space(*s) {
                s = s.add(1);
            }
            continue;
        }
        if *f != b'%' || *f.add(1) == b'%' {
            let lit = if *f == b'%' { f = f.add(1); b'%' } else { *f };
            if *f == b'%' {
                while space(*s) {
                    s = s.add(1);
                }
            }
            if *s != lit {
                return eof(s, count);
            }
            s = s.add(1);
            f = f.add(1);
            continue;
        }
        f = f.add(1);
        let suppress = *f == b'*';
        if suppress {
            f = f.add(1);
        }
        let mut width = 0usize;
        while (*f).is_ascii_digit() {
            width = width * 10 + (*f - b'0') as usize;
            f = f.add(1);
        }
        let mut size = 4usize;
        let mut long_double = false;
        loop {
            match *f {
                b'h' => size = if *f.add(1) == b'h' { f = f.add(1); 1 } else { 2 },
                b'l' => {
                    if *f.add(1) == b'l' {
                        f = f.add(1);
                    }
                    size = 8
                }
                b'j' | b'z' | b't' | b'q' => size = 8,
                b'L' => long_double = true,
                _ => break,
            }
            f = f.add(1);
        }
        let conv = *f;
        if conv == 0 {
            return count;
        }
        f = f.add(1);
        if !matches!(conv, b'c' | b'[' | b'n') {
            while space(*s) {
                s = s.add(1);
            }
            if *s == 0 {
                return eof(s, count);
            }
        }
        match conv {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let base = match conv {
                    b'd' | b'u' => 10,
                    b'i' => 0,
                    b'o' => 8,
                    _ => 16,
                };
                let mut tmp = [0u8; 128];
                let t = field(s, if width == 0 { 127 } else { width }, &mut tmp);
                let Some((v, neg, _, end)) = parse_uint(t.as_ptr(), base) else { return count };
                if end == t.as_ptr() {
                    return count;
                }
                s = s.add(end.offset_from(t.as_ptr()) as usize);
                if !suppress {
                    let v = if neg { v.wrapping_neg() } else { v };
                    store_int(ap.next_arg::<*mut u8>(), if conv == b'p' { 8 } else { size }, v);
                    count += 1;
                }
            }
            b'f' | b'e' | b'g' | b'a' | b'E' | b'G' | b'A' | b'F' => {
                if long_double {
                    hospes_rt::hlog!("hospes-glibc: sscanf %L{} refused", conv as char);
                    return count;
                }
                let mut tmp = [0u8; 128];
                let t = field(s, if width == 0 { 127 } else { width }, &mut tmp);
                let (v, end) = parse_f64(t.as_ptr());
                if end == t.as_ptr() {
                    return count;
                }
                s = s.add(end.offset_from(t.as_ptr()) as usize);
                if !suppress {
                    let p = ap.next_arg::<*mut u8>();
                    if size == 8 {
                        *(p as *mut f64) = v;
                    } else {
                        *(p as *mut f32) = v as f32;
                    }
                    count += 1;
                }
            }
            b's' => {
                let max = if width == 0 { usize::MAX } else { width };
                let dst = if suppress { core::ptr::null_mut() } else { ap.next_arg::<*mut u8>() };
                let mut i = 0;
                while i < max && *s != 0 && !space(*s) {
                    if !dst.is_null() {
                        *dst.add(i) = *s;
                    }
                    s = s.add(1);
                    i += 1;
                }
                if !dst.is_null() {
                    *dst.add(i) = 0;
                    count += 1;
                }
            }
            b'c' => {
                let n = if width == 0 { 1 } else { width };
                let dst = if suppress { core::ptr::null_mut() } else { ap.next_arg::<*mut u8>() };
                for i in 0..n {
                    if *s == 0 {
                        return eof(s, count);
                    }
                    if !dst.is_null() {
                        *dst.add(i) = *s;
                    }
                    s = s.add(1);
                }
                if !dst.is_null() {
                    count += 1;
                }
            }
            b'[' => {
                let negate = *f == b'^';
                if negate {
                    f = f.add(1);
                }
                let set = f;
                if *f == b']' {
                    f = f.add(1);
                }
                while *f != 0 && *f != b']' {
                    f = f.add(1);
                }
                if *f == 0 {
                    return count;
                }
                let set_end = f;
                f = f.add(1);
                let max = if width == 0 { usize::MAX } else { width };
                let dst = if suppress { core::ptr::null_mut() } else { ap.next_arg::<*mut u8>() };
                let mut i = 0;
                while i < max && *s != 0 && in_set(set, set_end, *s) != negate {
                    if !dst.is_null() {
                        *dst.add(i) = *s;
                    }
                    s = s.add(1);
                    i += 1;
                }
                if i == 0 {
                    return eof(s, count);
                }
                if !dst.is_null() {
                    *dst.add(i) = 0;
                    count += 1;
                }
            }
            b'n' => {
                if !suppress {
                    store_int(ap.next_arg::<*mut u8>(), size, s.offset_from(input) as u64);
                }
            }
            other => {
                hospes_rt::hlog!("hospes-glibc: sscanf %{} unsupported", other as char);
                return count;
            }
        }
    }
    count
}

pub unsafe extern "C" fn __isoc99_sscanf(s: *const u8, fmt: *const u8, mut args: ...) -> i32 {
    scan(s, fmt, &mut args)
}
pub unsafe extern "C" fn __isoc99_vsscanf(s: *const u8, fmt: *const u8, mut ap: VaList) -> i32 {
    scan(s, fmt, &mut ap)
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        #[link_name = "sscanf"]
        fn c_sscanf(s: *const u8, fmt: *const u8, ...) -> i32;
    }

    #[test]
    fn matches_glibc() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"12 34\0", b"%d %d\0"),
            (b"  -7,0x1f\0", b"%d,%x\0"),
            (b"ab12cd\0", b"%2x%2d\0"),
            (b"1.5e3 2\0", b"%lf %f\0"),
            (b"key=value;rest\0", b"%[^=]=%[^;];%s\0"),
            (b"0755 z\0", b"%i %c\0"),
            (b"\0", b"%d\0"),
            (b"x\0", b"%d\0"),
            (b"10 20\0", b"%*d %n%d\0"),
            (b"4294967295\0", b"%u\0"),
            (b"cpu  1 2 3\0", b"cpu %llu %llu %llu\0"),
        ];
        for (inp, fmt) in cases {
            let mut a = [[0u8; 32]; 4];
            let mut b = [[0u8; 32]; 4];
            let (pa, pb): (alloc::vec::Vec<*mut u8>, alloc::vec::Vec<*mut u8>) =
                (a.iter_mut().map(|x| x.as_mut_ptr()).collect(), b.iter_mut().map(|x| x.as_mut_ptr()).collect());
            let r1 = unsafe { __isoc99_sscanf(inp.as_ptr(), fmt.as_ptr(), pa[0], pa[1], pa[2], pa[3]) };
            let r2 = unsafe { c_sscanf(inp.as_ptr(), fmt.as_ptr(), pb[0], pb[1], pb[2], pb[3]) };
            assert_eq!(r1, r2, "{:?}", core::str::from_utf8(fmt));
            assert_eq!(a, b, "{:?}", core::str::from_utf8(fmt));
        }
    }
}
