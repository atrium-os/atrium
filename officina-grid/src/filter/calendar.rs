//! Excel date groups and dynamic calendar periods.

use std::collections::HashSet;

use officina_core::wire::{Reader, WireError, Writer};

use crate::eval::{
    dates::{civil_from_days, days_from_civil, DateSystem},
    funcs::text_key,
};

pub const GROUP_FIELDS: [&str; 6] = ["year", "month", "day", "hour", "minute", "second"];
pub const MAX_SELECTIONS: usize = 1 << 20;

pub fn gregorian_calendar(name: &str) -> bool {
    matches!(
        name,
        "none"
            | "gregorian"
            | "gregorianUs"
            | "gregorianMeFrench"
            | "gregorianArabic"
            | "gregorianXlitEnglish"
            | "gregorianXlitFrench"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateGroup {
    parts: [u16; 6],
    len: u8,
}

impl DateGroup {
    pub fn new(parts: &[u16]) -> Option<Self> {
        let (&year, _) = parts.split_first()?;
        if parts.len() > 6 || !(1900..=9999).contains(&year) {
            return None;
        }
        let mut key = [0; 6];
        key[..parts.len()].copy_from_slice(parts);
        let [y, m, d, h, min, sec] = key.map(i64::from);
        if (parts.len() >= 2 && !(1..=12).contains(&m))
            || (parts.len() >= 3
                && (d == 0
                    || (civil_from_days(days_from_civil(y, m, d)) != (y, m, d)
                        && (y, m, d) != (1900, 2, 29))))
            || h > 23
            || min > 59
            || sec > 59
        {
            return None;
        }
        Some(Self {
            parts: key,
            len: parts.len() as u8,
        })
    }

    pub fn parts(&self) -> &[u16] {
        &self.parts[..usize::from(self.len)]
    }
}

/// Calendar groups share the value checklist's OR with its literal values and blanks.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarSelection {
    pub groups: Vec<DateGroup>,
    pub values: Vec<String>,
    pub calendar: Option<String>,
}

impl CalendarSelection {
    pub(super) fn encode(&self, w: &mut Writer) {
        w.str(self.calendar.as_deref().unwrap_or(""));
        w.var(self.values.len() as u64);
        for value in &self.values {
            w.str(value);
        }
        w.var(self.groups.len() as u64);
        for group in &self.groups {
            w.u8(group.len);
            for part in group.parts() {
                w.u16(*part);
            }
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let calendar = match r.str("filter calendar", 32)? {
            "" => None,
            name if gregorian_calendar(name) => Some(name.to_string()),
            _ => return Err(WireError::Invalid("filter calendar")),
        };
        let k = r.count("calendar values", MAX_SELECTIONS as u64, 1)?;
        let values = (0..k)
            .map(|_| {
                r.str("filter value", crate::strings::MAX_TEXT as u64)
                    .map(str::to_string)
            })
            .collect::<Result<_, _>>()?;
        let k = r.count("calendar groups", MAX_SELECTIONS as u64, 3)?;
        if k == 0 {
            return Err(WireError::Invalid("missing date groups"));
        }
        let mut groups = Vec::with_capacity(k);
        for _ in 0..k {
            let n = r.u8()? as usize;
            if !(1..=6).contains(&n) {
                return Err(WireError::Invalid("date group precision"));
            }
            let mut parts = [0; 6];
            for part in &mut parts[..n] {
                *part = r.u16()?;
            }
            groups.push(DateGroup::new(&parts[..n]).ok_or(WireError::Invalid("date group"))?);
        }
        Ok(Self {
            groups,
            values,
            calendar,
        })
    }

    pub(super) fn prepare(&self) -> CalendarMatcher {
        CalendarMatcher {
            groups: self.groups.iter().copied().collect(),
            values: self.values.iter().map(|v| text_key(v)).collect(),
        }
    }
}

pub(super) struct CalendarMatcher {
    groups: HashSet<DateGroup>,
    pub values: HashSet<String>,
}

impl CalendarMatcher {
    pub fn matches_date(&self, system: DateSystem, serial: f64) -> bool {
        let Some((year, month, day)) = system.ymd(serial) else {
            return false;
        };
        // Absorb floating-point error when a serial encodes an exact second.
        let epsilon = serial.abs() * f64::EPSILON * 86400.0;
        let second = (serial.fract() * 86400.0 + epsilon).floor().min(86399.0) as u16;
        let mut key = DateGroup {
            parts: [
                year as u16,
                month as u16,
                day as u16,
                second / 3600,
                second / 60 % 60,
                second % 60,
            ],
            len: 6,
        };
        loop {
            if self.groups.contains(&key) {
                return true;
            }
            if key.len == 1 {
                return false;
            }
            key.len -= 1;
            key.parts[key.len as usize] = 0;
        }
    }
}

const RELATIVE: [[&str; 3]; 5] = [
    ["yesterday", "today", "tomorrow"],
    ["lastWeek", "thisWeek", "nextWeek"],
    ["lastMonth", "thisMonth", "nextMonth"],
    ["lastQuarter", "thisQuarter", "nextQuarter"],
    ["lastYear", "thisYear", "nextYear"],
];
const MONTHS: [&str; 12] = [
    "M1", "M2", "M3", "M4", "M5", "M6", "M7", "M8", "M9", "M10", "M11", "M12",
];
const QUARTERS: [&str; 4] = ["Q1", "Q2", "Q3", "Q4"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Relative { period: usize, position: usize },
    YearToDate,
    Month(usize),
    Quarter(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DateBounds {
    start: f64,
    end: f64,
}

impl DateBounds {
    pub fn new(start: f64, end: f64) -> Option<Self> {
        (start.is_finite() && end.is_finite() && start < end).then_some(Self { start, end })
    }

    pub fn limits(self) -> (f64, f64) {
        (self.start, self.end)
    }
}

/// The relative rule and Excel's cached interval for the saved row visibility.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DynamicDate {
    mode: Mode,
    pub cached: Option<DateBounds>,
}

impl DynamicDate {
    pub fn parse(name: &str, cached: Option<DateBounds>) -> Option<Self> {
        let mode = if name == "yearToDate" {
            Mode::YearToDate
        } else if let Some(month) = MONTHS.iter().position(|&n| n == name) {
            Mode::Month(month)
        } else if let Some(quarter) = QUARTERS.iter().position(|&n| n == name) {
            Mode::Quarter(quarter)
        } else {
            let (period, position) = RELATIVE.iter().enumerate().find_map(|(period, names)| {
                names
                    .iter()
                    .position(|&n| n == name)
                    .map(|position| (period, position))
            })?;
            Mode::Relative { period, position }
        };
        let mut rule = Self { mode, cached };
        if !rule.uses_bounds() {
            rule.cached = None;
        }
        Some(rule)
    }

    pub fn name(self) -> &'static str {
        match self.mode {
            Mode::Relative { period, position } => RELATIVE[period][position],
            Mode::YearToDate => "yearToDate",
            Mode::Month(month) => MONTHS[month],
            Mode::Quarter(quarter) => QUARTERS[quarter],
        }
    }

    pub fn uses_bounds(self) -> bool {
        matches!(self.mode, Mode::Relative { .. } | Mode::YearToDate)
    }

    /// Compute once per Reapply. The upper edge is exclusive and includes the
    /// entirety of the last day, even for cells that also carry a time.
    pub fn bounds(self, system: DateSystem, today: f64) -> Option<DateBounds> {
        let today = today.floor();
        let (year, month, _) = system.ymd(today)?;
        let date = |month: i64| system.date(year as f64, month as f64, 1.0);
        let (start, end) = match self.mode {
            Mode::Relative {
                period: 0,
                position,
            } => {
                let day = today + position as f64 - 1.0;
                (day, day + 1.0)
            }
            Mode::Relative {
                period: 1,
                position,
            } => {
                let sunday =
                    today - system.weekday(today as i64) as f64 + (position as f64 - 1.0) * 7.0;
                (sunday, sunday + 7.0)
            }
            Mode::Relative { period, position } => {
                let width = match period {
                    2 => 1,
                    3 => 3,
                    _ => 12,
                };
                let first =
                    1 + (month - 1).div_euclid(width) * width + (position as i64 - 1) * width;
                (date(first)?, date(first + width)?)
            }
            Mode::YearToDate => (date(1)?, today + 1.0),
            _ => return None,
        };
        DateBounds::new(start, end)
    }

    pub(super) fn keeps(self, system: DateSystem, serial: f64) -> bool {
        if !system.contains(serial) {
            return false;
        }
        match self.mode {
            Mode::Month(month) => system
                .ymd(serial)
                .is_some_and(|(_, m, _)| m as usize == month + 1),
            Mode::Quarter(quarter) => system
                .ymd(serial)
                .is_some_and(|(_, m, _)| (m as usize - 1) / 3 == quarter),
            _ => self
                .cached
                .is_some_and(|b| b.start <= serial && serial < b.end),
        }
    }

    pub(super) fn encode(self, w: &mut Writer) {
        w.str(self.name());
        w.u8(u8::from(self.cached.is_some()));
        if let Some(bounds) = self.cached {
            w.f64(bounds.start);
            w.f64(bounds.end);
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let name = r.str("dynamic date filter", 16)?;
        let bounds = match r.u8()? {
            0 => None,
            1 => Some(
                DateBounds::new(r.f64()?, r.f64()?)
                    .ok_or(WireError::Invalid("dynamic date interval"))?,
            ),
            _ => return Err(WireError::Invalid("dynamic date interval flag")),
        };
        let rule = Self::parse(name, bounds).ok_or(WireError::Invalid("dynamic date mode"))?;
        if bounds.is_some() && !rule.uses_bounds() {
            return Err(WireError::Invalid("unexpected dynamic date interval"));
        }
        Ok(rule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_periods_roll_over_year_end_in_both_epochs() {
        for system in [DateSystem::Excel1900, DateSystem::Excel1904] {
            let serial = |s: &str| system.iso(s).unwrap().0;
            let today = serial("2022-12-31");
            for (name, lower, upper) in [
                ("yesterday", "2022-12-30", "2022-12-31"),
                ("today", "2022-12-31", "2023-01-01"),
                ("tomorrow", "2023-01-01", "2023-01-02"),
                ("lastWeek", "2022-12-18", "2022-12-25"),
                ("thisWeek", "2022-12-25", "2023-01-01"),
                ("nextWeek", "2023-01-01", "2023-01-08"),
                ("lastMonth", "2022-11-01", "2022-12-01"),
                ("thisMonth", "2022-12-01", "2023-01-01"),
                ("nextMonth", "2023-01-01", "2023-02-01"),
                ("lastQuarter", "2022-07-01", "2022-10-01"),
                ("thisQuarter", "2022-10-01", "2023-01-01"),
                ("nextQuarter", "2023-01-01", "2023-04-01"),
                ("lastYear", "2021-01-01", "2022-01-01"),
                ("thisYear", "2022-01-01", "2023-01-01"),
                ("nextYear", "2023-01-01", "2024-01-01"),
                ("yearToDate", "2022-01-01", "2023-01-01"),
            ] {
                let mut rule = DynamicDate::parse(name, None).unwrap();
                assert_eq!(rule.name(), name);
                rule.cached = rule.bounds(system, today + 0.5);
                let (lower, upper) = (serial(lower), serial(upper));
                assert_eq!(rule.cached.unwrap().limits(), (lower, upper), "{name}");
                assert!(!rule.keeps(system, lower - 0.5), "{name}");
                assert!(rule.keeps(system, lower), "{name}");
                assert!(rule.keeps(system, upper - 0.5), "{name}");
                assert!(!rule.keeps(system, upper), "{name}");
            }
            let leap = serial("2024-02-29");
            let feb = DynamicDate::parse("thisMonth", None).unwrap();
            assert_eq!(
                feb.bounds(system, leap).unwrap().limits(),
                (serial("2024-02-01"), serial("2024-03-01"))
            );
            let year = DynamicDate::parse("yearToDate", None).unwrap();
            assert_eq!(
                year.bounds(system, leap).unwrap().limits(),
                (serial("2024-01-01"), serial("2024-03-01"))
            );
        }
    }

    #[test]
    fn all_dates_in_period_ignore_year() {
        let system = DateSystem::Excel1904;
        let serial = |s: &str| system.iso(s).unwrap().0;
        for name in MONTHS.into_iter().chain(QUARTERS) {
            let mode = DynamicDate::parse(name, DateBounds::new(1.0, 2.0)).unwrap();
            assert_eq!(mode.name(), name);
            assert!(mode.cached.is_none());
            for month in 1..=12 {
                let expected = match &name[..1] {
                    "M" => name[1..].parse::<i32>().unwrap() == month,
                    _ => name[1..].parse::<i32>().unwrap() == (month - 1) / 3 + 1,
                };
                for year in [1950, 2026] {
                    assert_eq!(
                        mode.keeps(system, serial(&format!("{year}-{month:02}-07"))),
                        expected,
                        "{name}"
                    );
                }
            }
        }
    }

    #[test]
    fn calendar_precision_and_excel_leap_day() {
        let system = DateSystem::Excel1900;
        let parts = [2024, 2, 29, 0, 0, 15];
        for precision in 1..=6 {
            let selection = CalendarSelection {
                groups: vec![DateGroup::new(&parts[..precision]).unwrap()],
                values: Vec::new(),
                calendar: None,
            };
            let matcher = selection.prepare();
            let serial = system.iso("2024-02-29T00:00:15").unwrap().0;
            assert!(
                matcher.matches_date(system, serial),
                "precision {precision}"
            );
            let just_outside = [
                "2025-01-01",
                "2024-03-01",
                "2024-02-28",
                "2024-02-29T01:00:15",
                "2024-02-29T00:01:15",
                "2024-02-29T00:00:16",
            ][precision - 1];
            assert!(!matcher.matches_date(system, system.iso(just_outside).unwrap().0));
        }
        let leap = CalendarSelection {
            groups: vec![DateGroup::new(&[1900, 2, 29]).unwrap()],
            values: Vec::new(),
            calendar: None,
        };
        assert!(leap.prepare().matches_date(system, 60.5));
        assert!(DateGroup::new(&[2100, 2, 29]).is_none());
        assert!(DateGroup::new(&[2024, 2, 29, 24]).is_none());
    }
}
