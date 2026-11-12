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

fn fill(t: i64, tm: &mut Tm) {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    const CUM: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = CUM[(m - 1) as usize] + d as i32 - 1 + (leap && m > 2) as i32;
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
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's days-from-civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub unsafe extern "C" fn localtime(t: *const i64) -> *mut Tm {
    let tm = &mut *core::ptr::addr_of_mut!(TM);
    fill(*t, tm);
    tm
}
pub unsafe extern "C" fn gmtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm {
    fill(*t, &mut *tm);
    tm
}
/// Local time is UTC (no zone data in the jail).
pub unsafe extern "C" fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm {
    gmtime_r(t, tm)
}
/// Out-of-range fields are normalised (month 13 is next January, and so on), and `tm` is
/// rewritten with the normalised date, as glibc does.
pub unsafe extern "C" fn timegm(tm: *mut Tm) -> i64 {
    let r = &*tm;
    let mon = r.mon as i64;
    let y = r.year as i64 + 1900 + mon.div_euclid(12);
    let days = days_from_civil(y, mon.rem_euclid(12) + 1, 1) + r.mday as i64 - 1;
    let t = days * 86_400 + r.hour as i64 * 3600 + r.min as i64 * 60 + r.sec as i64;
    fill(t, &mut *tm);
    t
}
pub unsafe extern "C" fn mktime(tm: *mut Tm) -> i64 {
    timegm(tm)
}
/// The locale is always "C".
pub unsafe extern "C" fn strftime_l(s: *mut u8, max: usize, fmt: *const u8, tm: *const Tm, _loc: *const u8) -> usize {
    strftime(s, max, fmt, tm)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        for t in [0i64, 951_782_400, 1_700_000_000, -86_401, 4_102_444_800] {
            let mut tm: Tm = unsafe { core::mem::zeroed() };
            unsafe { gmtime_r(&t, &mut tm) };
            assert_eq!(unsafe { timegm(&mut tm) }, t);
        }
        // 2000-02-29 from month 13 of 1999 plus 59 days.
        let mut tm: Tm = unsafe { core::mem::zeroed() };
        tm.year = 99;
        tm.mon = 12;
        tm.mday = 60;
        assert_eq!(unsafe { mktime(&mut tm) }, 951_782_400);
        assert_eq!((tm.year, tm.mon, tm.mday, tm.yday, tm.wday), (100, 1, 29, 59, 2));
    }
}
