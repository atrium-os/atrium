//! A C `printf` engine for the logging entry points, over a bounded buffer.
use core::ffi::VaList;
use core::fmt::{self, Write};

/// Writes at most `cap - 1` bytes to `p` but counts everything, as `snprintf` reports it.
pub struct Buf {
    p: *mut u8,
    cap: usize,
    n: usize,
}

impl Buf {
    pub fn new(p: *mut u8, cap: usize) -> Buf {
        Buf { p, cap, n: 0 }
    }
    pub fn byte(&mut self, b: u8) {
        if self.n + 1 < self.cap {
            unsafe { *self.p.add(self.n) = b };
        }
        self.n += 1;
    }
    pub fn len(&self) -> usize {
        self.n
    }
    pub fn terminate(&mut self) {
        if self.cap > 0 {
            unsafe { *self.p.add(self.n.min(self.cap - 1)) = 0 };
        }
    }
}

impl Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.byte(b);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

fn pad(out: &mut Buf, n: usize, c: u8) {
    for _ in 0..n {
        out.byte(c);
    }
}

fn emit_num(out: &mut Buf, sp: &Spec, neg: bool, digits: &[u8], prefix: &[u8]) {
    let sign: &[u8] = if neg {
        b"-"
    } else if sp.plus {
        b"+"
    } else if sp.space {
        b" "
    } else {
        b""
    };
    let prec_zeros = sp.prec.map_or(0, |p| p.saturating_sub(digits.len()));
    let body = sign.len() + prefix.len() + prec_zeros + digits.len();
    let fill = sp.width.saturating_sub(body);
    let zero_pad = sp.zero && !sp.left && sp.prec.is_none();
    if !sp.left && !zero_pad {
        pad(out, fill, b' ');
    }
    for &b in sign.iter().chain(prefix) {
        out.byte(b);
    }
    if zero_pad {
        pad(out, fill, b'0');
    }
    pad(out, prec_zeros, b'0');
    for &b in digits {
        out.byte(b);
    }
    if sp.left {
        pad(out, fill, b' ');
    }
}

fn digits(mut v: u64, base: u64, upper: bool, buf: &mut [u8; 24]) -> usize {
    let set = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = set[(v % base) as usize];
        v /= base;
    }
    i
}

/// Format `fmt` with `ap` into `out`.
pub unsafe fn format(out: &mut Buf, fmt: *const u8, ap: &mut VaList) {
    let mut p = fmt;
    while *p != 0 {
        let c = *p;
        p = p.add(1);
        if c != b'%' {
            out.byte(c);
            continue;
        }
        let mut sp = Spec::default();
        loop {
            match *p {
                b'-' => sp.left = true,
                b'+' => sp.plus = true,
                b' ' => sp.space = true,
                b'#' => sp.alt = true,
                b'0' => sp.zero = true,
                _ => break,
            }
            p = p.add(1);
        }
        if *p == b'*' {
            let w: i32 = ap.next_arg::<i32>();
            if w < 0 {
                sp.left = true;
            }
            sp.width = w.unsigned_abs() as usize;
            p = p.add(1);
        } else {
            while (*p).is_ascii_digit() {
                sp.width = sp.width * 10 + (*p - b'0') as usize;
                p = p.add(1);
            }
        }
        if *p == b'.' {
            p = p.add(1);
            let mut v = 0usize;
            if *p == b'*' {
                let w: i32 = ap.next_arg::<i32>();
                p = p.add(1);
                sp.prec = if w < 0 { None } else { Some(w as usize) };
            } else {
                while (*p).is_ascii_digit() {
                    v = v * 10 + (*p - b'0') as usize;
                    p = p.add(1);
                }
                sp.prec = Some(v);
            }
        }
        // Length: on LP64 everything wider than int is 64 bits.
        let mut long = 0u8; // 0 int, 1 char, 2 short, 8 long/long long/size_t/...
        loop {
            match *p {
                b'h' => long = if long == 2 { 1 } else { 2 },
                b'l' | b'z' | b'j' | b't' | b'q' | b'L' => long = 8,
                _ => break,
            }
            p = p.add(1);
        }
        let conv = *p;
        if conv == 0 {
            break;
        }
        p = p.add(1);
        let mut buf = [0u8; 24];
        match conv {
            b'd' | b'i' => {
                let v: i64 = match long {
                    8 => ap.next_arg::<i64>(),
                    1 => ap.next_arg::<i32>() as i8 as i64,
                    2 => ap.next_arg::<i32>() as i16 as i64,
                    _ => ap.next_arg::<i32>() as i64,
                };
                let i = digits(v.unsigned_abs(), 10, false, &mut buf);
                let d = if sp.prec == Some(0) && v == 0 { &buf[24..] } else { &buf[i..] };
                emit_num(out, &sp, v < 0, d, b"");
            }
            b'u' | b'x' | b'X' | b'o' => {
                let v: u64 = match long {
                    8 => ap.next_arg::<u64>(),
                    1 => ap.next_arg::<u32>() as u8 as u64,
                    2 => ap.next_arg::<u32>() as u16 as u64,
                    _ => ap.next_arg::<u32>() as u64,
                };
                let base = match conv {
                    b'u' => 10,
                    b'o' => 8,
                    _ => 16,
                };
                let i = digits(v, base, conv == b'X', &mut buf);
                let prefix: &[u8] = match (sp.alt && v != 0, conv) {
                    (true, b'x') => b"0x",
                    (true, b'X') => b"0X",
                    (true, b'o') => b"0",
                    _ => b"",
                };
                emit_num(out, &sp, false, &buf[i..], prefix);
            }
            b'p' => {
                let v = ap.next_arg::<usize>() as u64;
                let i = digits(v, 16, false, &mut buf);
                if v == 0 {
                    emit_num(out, &Spec { width: sp.width, left: sp.left, ..Default::default() }, false, b"(nil)", b"");
                } else {
                    emit_num(out, &Spec { width: sp.width, left: sp.left, ..Default::default() }, false, &buf[i..], b"0x");
                }
            }
            b'c' => {
                let v = ap.next_arg::<i32>() as u8;
                let fill = sp.width.saturating_sub(1);
                if !sp.left {
                    pad(out, fill, b' ');
                }
                out.byte(v);
                if sp.left {
                    pad(out, fill, b' ');
                }
            }
            b's' => {
                let s = ap.next_arg::<*const u8>();
                let s: &[u8] = if s.is_null() { b"(null)" } else { hospes_rt::start::cstr(s) };
                let s = &s[..sp.prec.map_or(s.len(), |p| p.min(s.len()))];
                let fill = sp.width.saturating_sub(s.len());
                if !sp.left {
                    pad(out, fill, b' ');
                }
                for &b in s {
                    out.byte(b);
                }
                if sp.left {
                    pad(out, fill, b' ');
                }
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let v = ap.next_arg::<f64>();
                let prec = sp.prec.unwrap_or(6);
                let mut tmp = [0u8; 64];
                let mut t = Buf::new(tmp.as_mut_ptr(), tmp.len());
                let _ = match conv {
                    b'e' | b'E' => write!(t, "{:.*e}", prec, v.abs()),
                    b'g' | b'G' => write!(t, "{}", v.abs()),
                    _ => write!(t, "{:.*}", prec, v.abs()),
                };
                let n = t.len().min(63);
                emit_num(out, &sp, v.is_sign_negative() && v != 0.0, &tmp[..n], b"");
            }
            b'%' => out.byte(b'%'),
            b'n' => {
                // Writing through %n is never honoured.
                let _ = ap.next_arg::<*mut i32>();
            }
            other => {
                out.byte(b'%');
                out.byte(other);
            }
        }
    }
}
