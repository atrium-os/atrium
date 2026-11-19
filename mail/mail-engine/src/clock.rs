//! The person's local time: Mail's day boundaries, "tomorrow at 08:00"
//! and the dates it shows follow the time zone the system gives the app
//! (localtime(3): `TZ`, else `/etc/localtime`). Where there is none, as in
//! a jail without the file, that is UTC.

/// Seconds east of UTC here at `t`.
pub fn local_offset(t: i64) -> i64 {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only the tm passed to it.
    if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

/// `t` as local wall-clock seconds (seconds since 1970 as if the local
/// time were UTC).
pub fn to_local(t: i64) -> i64 {
    t + local_offset(t)
}

/// The moment local wall-clock time `local` happens: the offset of the
/// moment itself, so a daylight-saving change in between is right.
pub fn from_local(local: i64) -> i64 {
    let guess = local - local_offset(local);
    local - local_offset(guess)
}

/// Year, month, day of days since 1970.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Days since 1970 of a date.
fn days_of(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The local day of `t`: "30 Nov 2026".
pub fn day_label(t: i64) -> String {
    let (y, m, d) = civil(to_local(t).div_euclid(86_400));
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

/// The local day of `t` as typed back: "2026-11-30".
pub fn day_iso(t: i64) -> String {
    let (y, m, d) = civil(to_local(t).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// "2026-11-30" (or "30 Nov 2026"): the last second of that local day.
pub fn end_of_day(s: &str) -> Option<i64> {
    let s = s.trim();
    let (y, m, d) = if let Some((y, rest)) = s.split_once('-') {
        let (m, d) = rest.split_once('-')?;
        (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
    } else {
        let mut w = s.split_whitespace();
        let d: u32 = w.next()?.parse().ok()?;
        let mon = w.next()?;
        let m = MONTHS.iter().position(|x| mon.len() >= 3 && x.eq_ignore_ascii_case(&mon[..3]))? as u32 + 1;
        (w.next()?.parse().ok()?, m, d)
    };
    if !(1..=12).contains(&m) || d == 0 || d > 31 || !(1970..=9999).contains(&y) {
        return None;
    }
    let days = days_of(y, m, d);
    if civil(days) != (y, m, d) {
        return None;
    }
    Some(from_local(days * 86_400 + 86_399))
}

#[cfg(test)]
mod tests {
    #[test]
    fn days() {
        // Whatever the zone, a day read back is that day.
        let t = super::end_of_day("2026-11-30").unwrap();
        assert_eq!(super::day_iso(t), "2026-11-30");
        assert_eq!(super::day_label(t), "30 Nov 2026");
        assert_eq!(super::end_of_day("30 nov 2026"), Some(t));
        assert_eq!(super::end_of_day("2026-02-30"), None);
        assert_eq!(super::end_of_day("2028-02-29").map(super::day_iso).as_deref(), Some("2028-02-29"));
        assert_eq!(super::end_of_day("tomorrow"), None);
    }

    #[test]
    fn round_trip() {
        let t = 1_795_425_660;
        assert_eq!(super::from_local(super::to_local(t)), t);
    }
}
