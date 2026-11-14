//! Excel date serials (1900 system, with its 29 February 1900 that never was).

/// Days from 1970-01-01 to y-m-d (proleptic Gregorian). Howard Hinnant's algorithm.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const EPOCH_1899_12_30: i64 = -25569; // days_from_civil(1899, 12, 30)

/// DATE(y, m, d) as Excel computes it: years 0..1899 add 1900, months and days roll
/// over. `None` outside 1900-01-00 ..= 9999-12-31.
pub fn date_serial(y: f64, m: f64, d: f64) -> Option<f64> {
    let mut y = y.trunc() as i64;
    let m = m.trunc() as i64;
    let d = d.trunc() as i64;
    if !(0..10000).contains(&y) {
        return None;
    }
    if y < 1900 {
        y += 1900;
    }
    // Roll months into years.
    let mm = m - 1;
    let y2 = y + mm.div_euclid(12);
    let m2 = mm.rem_euclid(12) + 1;
    let days = days_from_civil(y2, m2, 1) + d - 1;
    let serial = days - EPOCH_1899_12_30;
    // Before 1900-03-01 Excel counts one fewer (its fake 1900-02-29 is serial 60).
    let serial = if serial < 61 { serial - 1 } else { serial };
    if !(0..=2958465).contains(&serial) {
        return None;
    }
    Some(serial as f64)
}

/// (year, month, day) for a serial, Excel's way (serial 0 is 1900-01-00, 60 is
/// 1900-02-29).
pub fn ymd(serial: f64) -> Option<(i64, i64, i64)> {
    if !(0.0..2958466.0).contains(&serial) {
        return None;
    }
    let s = serial.floor() as i64;
    Some(match s {
        0 => (1900, 1, 0),
        60 => (1900, 2, 29),
        s if s < 60 => civil_from_days(EPOCH_1899_12_30 + s + 1),
        s => civil_from_days(EPOCH_1899_12_30 + s),
    })
}

/// Now as a serial with the time of day (UTC until time zones reach the engine).
pub fn now_serial() -> f64 {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    secs / 86400.0 - EPOCH_1899_12_30 as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serials() {
        assert_eq!(date_serial(1900.0, 1.0, 1.0), Some(1.0));
        assert_eq!(date_serial(1900.0, 2.0, 28.0), Some(59.0));
        assert_eq!(date_serial(1900.0, 3.0, 1.0), Some(61.0));
        assert_eq!(date_serial(2024.0, 2.0, 29.0), Some(45351.0));
        assert_eq!(date_serial(2023.0, 13.0, 1.0), Some(45292.0)); // 2024-01-01
        assert_eq!(date_serial(99.0, 1.0, 1.0), Some(36161.0)); // 1999-01-01
        assert_eq!(ymd(60.0), Some((1900, 2, 29)));
        assert_eq!(ymd(61.0), Some((1900, 3, 1)));
        assert_eq!(ymd(45351.0), Some((2024, 2, 29)));
        assert_eq!(ymd(1.0), Some((1900, 1, 1)));
        for s in 1..80000 {
            let (y, m, d) = ymd(s as f64).unwrap();
            if s != 60 {
                assert_eq!(date_serial(y as f64, m as f64, d as f64), Some(s as f64), "{s}");
            }
        }
    }
}
