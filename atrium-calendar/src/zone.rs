//! Resolve source wall times with the same embedded IANA rules in both jails.
use crate::{Entry, Kind, Span};
use chrono::{DateTime, LocalResult, NaiveDate, Offset, TimeDelta, TimeZone, Utc};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClockPart {
    pub span: Span,
    pub offset: i32,
}

/// Only returned on a resolved agenda row. The persisted entry is in its source zone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSlot {
    pub date: NaiveDate,
    pub span: Span,
    pub first: NaiveDate,
    pub parts: [Option<ClockPart>; 2],
}

impl SourceSlot {
    pub fn clock_label(self) -> String {
        self.parts
            .into_iter()
            .flatten()
            .map(|p| {
                let clock = |m: u16| format!("{:02}:{:02}", m / 60, m % 60);
                let offset = chrono::FixedOffset::east_opt(p.offset)
                    .map(|o| o.to_string())
                    .unwrap_or_default();
                format!("{}–{} {offset}", clock(p.span.start), clock(p.span.end))
            })
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

pub fn parse_name(input: &str) -> Option<Tz> {
    let name = input.trim();
    name.parse().ok().or_else(|| {
        chrono_tz::TZ_VARIANTS
            .iter()
            .copied()
            .find(|z| z.name().eq_ignore_ascii_case(name))
    })
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

// Two IANA zones can put an instant on civil dates two days apart.
pub(crate) fn nearby(date: NaiveDate, delta: i64) -> NaiveDate {
    date.checked_add_signed(TimeDelta::days(delta))
        .unwrap_or(if delta < 0 {
            NaiveDate::MIN
        } else {
            NaiveDate::MAX
        })
}

// Each interval examined here is at most three days. Successive transitions
// in the embedded IANA rules are farther apart.
fn clock_parts(a: i64, b: i64, shown: NaiveDate, view: Tz) -> [Option<ClockPart>; 2] {
    let midnight = shown.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
    let offset = |t: i64| {
        view.offset_from_utc_datetime(&DateTime::from_timestamp(t, 0).unwrap().naive_utc())
            .fix()
            .local_minus_utc()
    };
    let (first, last) = (offset(a), offset(b - 1));
    let mut cut = b;
    if first != last {
        let mut lo = a;
        let mut hi = b - 1;
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if offset(mid) == first {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        cut = hi;
    }
    let mut parts = [None; 2];
    let mut count = 0;
    for (lo, hi, offset) in [(a, cut, first), (cut, b, last)] {
        let clip = |at: i64| (at + i64::from(offset) - midnight).clamp(0, 86400);
        let (lo, hi) = (clip(lo), clip(hi));
        if lo < hi {
            parts[count] = Some(ClockPart {
                span: Span {
                    start: (lo / 60) as u16,
                    end: ((hi + 59) / 60) as u16,
                },
                offset,
            });
            count += 1;
        }
    }
    parts
}

pub(crate) fn day_parts(date: NaiveDate, view: Tz) -> [Option<ClockPart>; 2] {
    let midnight = date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
    clock_parts(
        (midnight - 86400).max(DateTime::<Utc>::MIN_UTC.timestamp()),
        (midnight + 172800).min(DateTime::<Utc>::MAX_UTC.timestamp() + 1),
        date,
        view,
    )
}

pub(crate) fn missing_minutes(date: NaiveDate, view: Tz) -> Option<Span> {
    let parts = day_parts(date, view);
    let mut spans = parts
        .into_iter()
        .flatten()
        .map(|p| p.span)
        .collect::<Vec<_>>();
    spans.sort_by_key(|s| s.start);
    let mut end = 0;
    for span in spans {
        if span.start > end {
            return Some(Span {
                start: end,
                end: span.start,
            });
        }
        end = end.max(span.end);
    }
    (end < 1440).then_some(Span {
        start: end,
        end: 1440,
    })
}

// Chronological order within a displayed date, including the second copy of an hour.
pub(crate) fn sort_time(source: Option<SourceSlot>, span: Span, date: NaiveDate, view: Tz) -> i64 {
    if let Some(p) = source.and_then(|s| s.parts[0]) {
        return i64::from(p.span.start) * 60 - i64::from(p.offset);
    }
    let offset = alternatives(view, date, span.start)
        .earliest()
        .map_or(0, |at| at.offset().fix().local_minus_utc());
    i64::from(span.start) * 60 - i64::from(offset)
}

impl Entry {
    /// Split at an offset transition before clipping to the displayed civil day.
    /// A meeting in a backwards hour can have two disjoint wall-clock spans.
    pub(crate) fn project(&self, source: NaiveDate, shown: NaiveDate, view: Tz) -> Option<Self> {
        if !self.appears(source) {
            return None;
        }
        let (start, end) = self
            .zone?
            .interval(
                source,
                self.span,
                self.kind == Kind::Weekly && source != self.date,
            )
            .ok()?;
        let parts = clock_parts(start.timestamp(), end.timestamp(), shown, view);
        let mut span = parts[0]?.span;
        if let Some(second) = parts[1] {
            span.start = span.start.min(second.span.start);
            span.end = span.end.max(second.span.end);
        }
        Some(Self {
            date: shown,
            span,
            source: Some(SourceSlot {
                date: source,
                span: self.span,
                first: self.date,
                parts,
            }),
            excluded: Vec::new(),
            ..self.clone()
        })
    }

    pub(crate) fn on_day(&self, shown: NaiveDate, view: Tz) -> Option<Self> {
        if self.source.is_some() {
            return (shown == self.date).then(|| self.clone());
        }
        if self.zone.is_none() {
            return self.appears(shown).then(|| self.clone());
        }
        (-2..=2).find_map(|delta| self.project(nearby(shown, delta), shown, view))
    }
    pub fn busy_spans(&self) -> impl Iterator<Item = Span> {
        let parts = self
            .source
            .map(|s| s.parts.map(|p| p.map(|part| part.span)))
            .unwrap_or([Some(self.span), None]);
        parts.into_iter().flatten()
    }
}
