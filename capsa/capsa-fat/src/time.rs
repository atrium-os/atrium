//! FAT timestamps (local time, 2 s resolution, 1980..2107) as Unix seconds.
//! The volume has no time zone; callers that know the local offset adjust.

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Out-of-range fields (month 0, day 31 of February...) are clamped, not
/// errors: a timestamp is never worth refusing a file over.
pub fn to_unix(date: u16, time: u16) -> i64 {
    if date == 0 {
        return 0;
    }
    let y = 1980 + i64::from(date >> 9);
    let m = i64::from((date >> 5) & 0x0F).clamp(1, 12);
    let d = i64::from(date & 0x1F).max(1);
    let h = i64::from(time >> 11).min(23);
    let mi = i64::from((time >> 5) & 0x3F).min(59);
    let s = (i64::from(time & 0x1F) * 2).min(59);
    days_from_civil(y, m, d) * 86400 + h * 3600 + mi * 60 + s
}

/// Unix seconds to (date, time), clamped to FAT's range.
pub fn from_unix(t: i64) -> (u16, u16) {
    let t = t.clamp(315532800, 4354819198); // 1980-01-01 .. 2107-12-31 23:59:58
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    let date = (((y - 1980) as u16) << 9) | ((m as u16) << 5) | d as u16;
    let time = (((secs / 3600) as u16) << 11) | ((((secs % 3600) / 60) as u16) << 5) | ((secs % 60 / 2) as u16);
    (date, time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // 2026-11-13 14:20:56 -> even seconds.
        let t = 1794579656;
        let (d, h) = from_unix(t);
        assert_eq!(to_unix(d, h), t);
        assert_eq!(to_unix(0x0021, 0), 315532800, "1980-01-01");
        assert_eq!(to_unix(0, 0), 0);
        assert_eq!(to_unix(0xFFFF, 0xFFFF), to_unix(0xFFFF, 0xFFFF)); // no panic
        assert_eq!(from_unix(0), from_unix(315532800));
    }
}
