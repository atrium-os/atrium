//! Resolve source wall times with the same embedded IANA rules in both jails.
use crate::{Entry, Kind, Span};
use chrono::{DateTime, Days, LocalResult, NaiveDate, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Fold {
    Earlier,
    Later,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    pub name: Tz,
    pub start_fold: Option<Fold>,
    pub end_fold: Option<Fold>,
}

/// Only returned on a resolved UTC agenda row. The persisted entry is in its source zone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSlot {
    pub date: NaiveDate,
    pub span: Span,
    pub first: NaiveDate,
}

pub fn alternatives(zone: Tz, date: NaiveDate, minute: u16) -> LocalResult<DateTime<Tz>> {
    if minute > 1440 {
        return LocalResult::None;
    }
    let midnight = date.and_hms_opt(0, 0, 0).unwrap();
    midnight
        .checked_add_signed(TimeDelta::minutes(i64::from(minute)))
        .map_or(LocalResult::None, |wall| zone.from_local_datetime(&wall))
}

impl Zone {
    /// Both boundaries are resolved independently. A recurrence that first lands
    /// in a fold uses the earlier instant unless the series has a saved choice.
    pub fn interval(
        self,
        date: NaiveDate,
        span: Span,
        recurring: bool,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), &'static str> {
        if span.start >= 1440 || span.end > 1440 {
            return Err("Invalid source time");
        }
        let resolve = |minute, choice| match alternatives(self.name, date, minute) {
            LocalResult::Single(at) => Ok(at.to_utc()),
            LocalResult::None => Err("This wall time does not exist. Choose another time."),
            LocalResult::Ambiguous(first, second) => match choice {
                Some(Fold::Earlier) => Ok(first.to_utc()),
                Some(Fold::Later) => Ok(second.to_utc()),
                None if recurring => Ok(first.to_utc()),
                None => Err("This time repeats. Choose its UTC offset."),
            },
        };
        let start = resolve(span.start, self.start_fold)?;
        let end = resolve(span.end, self.end_fold)?;
        if end <= start {
            return Err("The end must be after the start in actual time.");
        }
        Ok((start, end))
    }
}

impl Entry {
    /// Project one source occurrence onto a displayed UTC day, clipping at midnight.
    pub(crate) fn segment(&self, source: NaiveDate, shown: NaiveDate) -> Option<Span> {
        if !self.appears(source) {
            return None;
        }
        let Some(zone) = self.zone else {
            return (source == shown).then_some(self.span);
        };
        let (start, end) = zone
            .interval(
                source,
                self.span,
                self.kind == Kind::Weekly && source != self.date,
            )
            .ok()?;
        let midnight = shown.and_hms_opt(0, 0, 0)?.and_utc();
        let lo = (start - midnight).num_seconds().clamp(0, 86400);
        let hi = (end - midnight).num_seconds().clamp(0, 86400);
        (lo < hi).then_some(Span {
            start: (lo / 60) as u16,
            end: ((hi + 59) / 60) as u16,
        })
    }

    pub(crate) fn on_day(&self, shown: NaiveDate) -> Option<Self> {
        if self.source.is_some() {
            return (shown == self.date).then(|| self.clone());
        }
        if self.zone.is_none() {
            return self.appears(shown).then(|| self.clone());
        }
        // A civil day in any IANA zone can touch the preceding or following UTC date.
        for source in [
            shown.checked_sub_days(Days::new(1)),
            Some(shown),
            shown.succ_opt(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(span) = self.segment(source, shown) {
                return Some(Self {
                    date: shown,
                    span,
                    source: Some(SourceSlot {
                        date: source,
                        span: self.span,
                        first: self.date,
                    }),
                    excluded: Vec::new(),
                    ..self.clone()
                });
            }
        }
        None
    }
}
