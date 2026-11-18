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

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let t = 1_795_425_660;
        assert_eq!(super::from_local(super::to_local(t)), t);
    }
}
