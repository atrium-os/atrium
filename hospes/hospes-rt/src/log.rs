//! Unbuffered logging to fd 2.
use core::fmt;

pub struct Stderr;

impl fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::sys::write_all(2, s.as_bytes()).map_err(|_| fmt::Error)
    }
}

/// Format into a fixed buffer first so concurrent threads' lines do not interleave.
pub fn line(args: fmt::Arguments) {
    struct Buf {
        b: [u8; 512],
        n: usize,
    }
    impl fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let take = s.len().min(self.b.len() - self.n);
            self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
            self.n += take;
            Ok(())
        }
    }
    let mut b = Buf { b: [0; 512], n: 0 };
    let _ = fmt::Write::write_fmt(&mut b, args);
    if b.n == b.b.len() {
        b.b[b.n - 1] = b'\n';
    } else {
        b.b[b.n] = b'\n';
        b.n += 1;
    }
    let _ = crate::sys::write_all(2, &b.b[..b.n]);
}

#[macro_export]
macro_rules! hlog {
    ($($t:tt)*) => { $crate::log::line(format_args!($($t)*)) };
}
