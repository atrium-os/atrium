//! Time: gettimeofday, usleep, localtime (always UTC: no time zone data in the jail),
//! strftime.
use crate::abi;
use core::ffi::c_long;
use hospes_rt::sys;

#[repr(C)]
pub struct Timeval {
    sec: i64,
    usec: i64,
}

#[repr(C)]
pub struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: c_long,
    zone: *const u8,
}
const _: () = assert!(core::mem::size_of::<Tm>() == abi::SIZEOF_TM);
const _: () = assert!(core::mem::offset_of!(Tm, gmtoff) == abi::OFFSETOF_TM_GMTOFF);
const _: () = assert!(core::mem::offset_of!(Tm, zone) == abi::OFFSETOF_TM_ZONE);

pub unsafe extern "C" fn gettimeofday(tv: *mut Timeval, _tz: *mut u8) -> i32 {
    if !tv.is_null() {
        let (s, ns) = sys::realtime();
        *tv = Timeval { sec: s, usec: ns / 1000 };
    }
    0
}

pub extern "C" fn usleep(us: u32) -> i32 {
    sys::nanosleep(us as i64 / 1_000_000, (us as i64 % 1_000_000) * 1000);
    0
}

static mut TM: Tm = Tm { sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: core::ptr::null() };

fn civil(days: i64) -> (i64, i64, i64) {
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

pub unsafe extern "C" fn localtime(t: *const i64) -> *mut Tm {
    let t = *t;
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    const CUM: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = CUM[(m - 1) as usize] + d as i32 - 1 + (leap && m > 2) as i32;
    let tm = &mut *core::ptr::addr_of_mut!(TM);
    *tm = Tm {
        sec: (secs % 60) as i32,
        min: (secs / 60 % 60) as i32,
        hour: (secs / 3600) as i32,
        mday: d as i32,
        mon: (m - 1) as i32,
        year: (y - 1900) as i32,
        wday: (days + 4).rem_euclid(7) as i32,
        yday,
        isdst: 0,
        gmtoff: 0,
        zone: b"UTC\0".as_ptr(),
    };
    tm
}

pub unsafe extern "C" fn strftime(s: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize {
    let tm = &*tm;
    let mut out = crate::printf::Buf::new(s, max);
    let mut p = fmt;
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    use core::fmt::Write;
    while *p != 0 {
        if *p != b'%' {
            out.byte(*p);
            p = p.add(1);
            continue;
        }
        p = p.add(1);
        let c = *p;
        if c == 0 {
            break;
        }
        p = p.add(1);
        let _ = match c {
            b'Y' => write!(out, "{}", tm.year + 1900),
            b'y' => write!(out, "{:02}", (tm.year + 1900) % 100),
            b'm' => write!(out, "{:02}", tm.mon + 1),
            b'd' => write!(out, "{:02}", tm.mday),
            b'e' => write!(out, "{:2}", tm.mday),
            b'H' => write!(out, "{:02}", tm.hour),
            b'M' => write!(out, "{:02}", tm.min),
            b'S' => write!(out, "{:02}", tm.sec),
            b'j' => write!(out, "{:03}", tm.yday + 1),
            b'a' => out.write_str(DAYS[tm.wday.rem_euclid(7) as usize]),
            b'b' | b'h' => out.write_str(MONS[tm.mon.rem_euclid(12) as usize]),
            b'Z' => out.write_str("UTC"),
            b'z' => out.write_str("+0000"),
            b'F' => write!(out, "{}-{:02}-{:02}", tm.year + 1900, tm.mon + 1, tm.mday),
            b'T' => write!(out, "{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec),
            b'%' => out.write_str("%"),
            other => {
                out.byte(b'%');
                out.byte(other);
                Ok(())
            }
        };
    }
    // strftime returns 0 if the result (with NUL) did not fit.
    if out.len() + 1 > max {
        if max > 0 {
            *s = 0;
        }
        return 0;
    }
    out.terminate();
    out.len()
}
