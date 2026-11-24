//! Limits for a timed weekly Mail invitation, evaluated on its source clock.
use crate::Entry;
use chrono::{Days, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WeeklyLimit {
    Count(u16),
    Until { wall: NaiveDateTime, utc: bool },
}

impl WeeklyLimit {
    pub fn parse(rule: &str) -> Option<Option<Self>> {
        let mut weekly = false;
        let mut limit = None;
        for item in rule.to_ascii_uppercase().split(';') {
            let (name, value) = item.split_once('=')?;
            match name {
                "FREQ" if value == "WEEKLY" && !weekly => weekly = true,
                "COUNT" if limit.is_none() && value.bytes().all(|c| c.is_ascii_digit()) => {
                    let count = value.parse().ok()?;
                    if !(1..=520).contains(&count) {
                        return None;
                    }
                    limit = Some(Self::Count(count));
                }
                "UNTIL" if limit.is_none() => {
                    let stamp = value.strip_suffix('Z').unwrap_or(value);
                    if stamp.len() != 15 {
                        return None;
                    }
                    let wall = NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S").ok()?;
                    limit = Some(Self::Until {
                        wall,
                        utc: stamp != value,
                    });
                }
                _ => return None,
            }
        }
        weekly.then_some(limit)
    }

    pub fn property(self) -> String {
        match self {
            Self::Count(n) => format!("COUNT={n}"),
            Self::Until { wall, utc } => format!(
                "UNTIL={}{}",
                wall.format("%Y%m%dT%H%M%S"),
                if utc { "Z" } else { "" }
            ),
        }
    }

    pub fn last(self, row: &Entry) -> Result<NaiveDate, &'static str> {
        if row.kind != crate::Kind::Weekly {
            return Err("Expected a weekly series");
        }
        match self {
            Self::Count(n) if !(1..=520).contains(&n) => return Err("Unsupported weekly count"),
            Self::Until { utc, .. } if utc != row.zone.is_some() => {
                return Err("End time and series time zones do not match")
            }
            _ => (),
        }
        let mut date = row.date;
        let mut last = None;
        let mut count = 0;
        // Includes headroom for skipped DST gaps. Never expand an arbitrary rule.
        for _ in 0..=540 {
            let start = if let Some(zone) = row.zone {
                zone.interval(date, row.span, true)
                    .ok()
                    .map(|v| v.0.naive_utc())
            } else {
                date.and_hms_opt(
                    u32::from(row.span.start / 60),
                    u32::from(row.span.start % 60),
                    0,
                )
            };
            if let Some(start) = start {
                if matches!(self, Self::Until {wall,..} if start > wall) {
                    return last.ok_or("Series ends before its first meeting");
                }
                count += 1;
                if count > 520 {
                    return Err("Series exceeds 520 meetings");
                }
                last = Some(date);
                if self == Self::Count(count) {
                    return Ok(date);
                }
            }
            date = date
                .checked_add_days(Days::new(7))
                .ok_or("Series date outside calendar")?;
        }
        Err("Series exceeds supported dates")
    }
}
