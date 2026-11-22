//! Excel date serials, including the 1900 system's fictitious leap day.

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
    if ![y, m, d].into_iter().all(|n| n.is_finite() && n.abs() < i32::MAX as f64) {
        return None;
    }
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

/// The calendar used by a workbook's numeric date values.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DateSystem {
    #[default]
    Excel1900,
    Excel1904,
}

impl DateSystem {
    const OFFSET_1904: f64 = 1462.0;

    pub fn from_1900(self, serial: f64) -> Option<f64> {
        let serial = match self {
            Self::Excel1900 => serial,
            Self::Excel1904 => serial - Self::OFFSET_1904,
        };
        self.contains(serial).then_some(serial)
    }

    /// A calendar date or timestamp in the ISO form used by spreadsheet cells.
    /// Offsets, when present, are normalized to UTC, since serials carry no zone.
    pub fn iso(self, text: &str) -> Option<(f64, &'static str)> {
        let text = text.trim();
        let date = text.get(..10)?;
        if date.as_bytes()[4] != b'-' || date.as_bytes()[7] != b'-' {
            return None;
        }
        let digits = |s: &str| -> Option<u32> {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
        };
        let year = digits(date.get(..4)?)?;
        let month = digits(date.get(5..7)?)?;
        let day = digits(date.get(8..10)?)?;
        if year < 1900 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let day_serial = date_serial(year.into(), month.into(), day.into())?;
        if ymd(day_serial) != Some((year.into(), month.into(), day.into())) {
            return None;
        }
        let tail = text.get(10..)?;
        if tail.is_empty() {
            return Some((self.from_1900(day_serial)?, "yyyy-mm-dd"));
        }
        let time = tail.strip_prefix('T')?;
        let (time, offset) = if let Some(t) = time.strip_suffix('Z') {
            (t, 0)
        } else if let Some(i) = time.find(['+', '-']) {
            let zone = time.get(i..)?;
            if zone.len() != 6 || zone.as_bytes()[3] != b':' {
                return None;
            }
            let hour = digits(zone.get(1..3)?)?;
            let minute = digits(zone.get(4..6)?)?;
            if hour > 23 || minute > 59 {
                return None;
            }
            let seconds = (hour * 3600 + minute * 60) as i32;
            (&time[..i], if zone.starts_with('-') { -seconds } else { seconds })
        } else {
            (time, 0)
        };
        let mut fields = time.split(':');
        let two = |s: &str| (s.len() == 2).then(|| digits(s)).flatten();
        let hour = two(fields.next()?)?;
        let minute = two(fields.next()?)?;
        let second_field = fields.next();
        if hour > 23 || minute > 59 || fields.next().is_some() {
            return None;
        }
        let (seconds, fractional) = match second_field {
            Some(s) => {
                let (whole, frac) = s.split_once('.').map_or((s, None), |(w, f)| (w, Some(f)));
                let whole = two(whole)?;
                if whole > 59 {
                    return None;
                }
                let frac = match frac {
                    Some(f) if f.len() <= 9 => f64::from(digits(f)?) / 10f64.powi(f.len() as i32),
                    Some(_) => return None,
                    None => 0.0,
                };
                (f64::from(whole) + frac, frac != 0.0)
            }
            None => (0.0, false),
        };
        let time = f64::from(hour * 3600 + minute * 60) + seconds - f64::from(offset);
        let day_delta = (time / 86400.0).floor() as i64;
        let utc_day = if day_delta == 0 {
            day_serial
        } else {
            let (y, m, d) = civil_from_days(days_from_civil(year.into(), month.into(), day.into()) + day_delta);
            if y < 1900 {
                return None;
            }
            date_serial(y as f64, m as f64, d as f64)?
        };
        let serial = self.from_1900(utc_day)? + time.rem_euclid(86400.0) / 86400.0;
        if !self.contains(serial) {
            return None;
        }
        let format = if fractional {
            "yyyy-mm-dd hh:mm:ss.000"
        } else if second_field.is_some() {
            "yyyy-mm-dd hh:mm:ss"
        } else {
            "yyyy-mm-dd hh:mm"
        };
        Some((serial, format))
    }

    pub fn contains(self, serial: f64) -> bool {
        let end = match self {
            Self::Excel1900 => 2958466.0,
            Self::Excel1904 => 2958466.0 - Self::OFFSET_1904,
        };
        (0.0..end).contains(&serial)
    }

    pub fn date(self, year: f64, month: f64, day: f64) -> Option<f64> {
        self.from_1900(date_serial(year, month, day)?)
    }

    pub fn ymd(self, serial: f64) -> Option<(i64, i64, i64)> {
        if !self.contains(serial) {
            return None;
        }
        ymd(match self {
            Self::Excel1900 => serial,
            Self::Excel1904 => serial.floor() + Self::OFFSET_1904,
        })
    }

    /// Sunday is zero. The 1904 epoch began on Friday.
    pub fn weekday(self, day: i64) -> i64 {
        let offset = match self {
            Self::Excel1900 => -1,
            Self::Excel1904 => 5,
        };
        (day + offset).rem_euclid(7)
    }

    pub fn now(self) -> f64 {
        let serial = now_serial();
        match self {
            Self::Excel1900 => serial,
            Self::Excel1904 => serial - Self::OFFSET_1904,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates_keep_the_calendar_and_reject_invalid_input() {
        let old = DateSystem::Excel1900;
        let mac = DateSystem::Excel1904;
        assert_eq!(mac.iso("1904-01-01"), Some((0.0, "yyyy-mm-dd")));
        assert_eq!(mac.iso("1904-02-29"), Some((59.0, "yyyy-mm-dd")));
        assert_eq!(old.iso("1904-02-29"), Some((1521.0, "yyyy-mm-dd")));
        assert_eq!(mac.iso("1904-01-02T00:30:00+01:00"), mac.iso("1904-01-01T23:30:00Z"));
        assert_eq!(old.iso("1900-02-28T23:30:00-01:00"), old.iso("1900-03-01T00:30:00Z"));
        assert!(old.iso("1900-01-01T00:30:00+01:00").is_none());
        for bad in ["1904-02-30", "1904-1-01", "1904-01-01T25:00", "1904-01-01T00:00:01.foo", "1903-12-31"] {
            assert!(mac.iso(bad).is_none(), "{bad}");
        }
    }

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
