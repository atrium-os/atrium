//! Wall-clock labels for what was put away (§4.5: Earlier, grouped by
//! day), in the machine's local time.

/// Now, in Unix seconds.
pub fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Local (day number since the epoch, weekday 0 = Sunday, day of month,
/// month 0–11, hour, minute) for `t`.
fn local(t: u64) -> (i64, usize, u32, usize, u32, u32) {
    let tt = t as libc::time_t;
    // SAFETY: localtime_r writes only the tm we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    let day = (t as i64 + tm.tm_gmtoff as i64).div_euclid(86_400);
    (day, tm.tm_wday as usize, tm.tm_mday as u32, tm.tm_mon as usize, tm.tm_hour as u32, tm.tm_min as u32)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The day group for `t` seen at `now`: Today, Yesterday, a weekday
/// within the week, else the date.
pub fn day_label(t: u64, now: u64) -> String {
    let (d, wd, md, mo, ..) = local(t);
    let (today, ..) = local(now);
    match today - d {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => DAYS[wd % 7].into(),
        _ => format!("{md} {}", MONTHS[mo % 12]),
    }
}

/// The day group and the time: "Today 18:20", "Wed 21:05".
pub fn when_label(t: u64, now: u64) -> String {
    let (.., h, m) = local(t);
    format!("{} {h:02}:{m:02}", day_label(t, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_group() {
        let now = now_s();
        assert_eq!(day_label(now, now), "Today");
        assert!(when_label(now, now).starts_with("Today "));
        assert_eq!(day_label(now - 86_400, now), "Yesterday");
        assert!(DAYS.contains(&day_label(now - 3 * 86_400, now).as_str()));
        assert!(MONTHS.iter().any(|m| day_label(now - 20 * 86_400, now).ends_with(m)));
    }
}
