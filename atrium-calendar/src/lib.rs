//! Manual local planning. Times are floating civil minutes within the selected date.
mod period;
pub use period::{CalendarPage, CalendarView};
pub mod store;
pub use store::search::{SearchHit, SearchPage, SearchQuery, SEARCH_PAGE_SIZE};
pub mod wire;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: u16,
    pub end: u16,
}

impl Span {
    pub fn valid(self) -> bool {
        self.start < self.end && self.end <= 1440
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: Uuid,
    pub date: NaiveDate,
    pub title: String,
    pub span: Span,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<NaiveDate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Appointment,
    Weekly,
    Occurrence { series: Uuid, original: NaiveDate },
    AllDay,
    Hold,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EditScope {
    ThisEvent,
    WholeSeries,
}

/// Editable local fields. Fasti retains ownership of identity and recurrence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEdit {
    pub date: NaiveDate,
    pub title: String,
    pub span: Span,
}

impl Entry {
    pub fn fields_on(&self, date: NaiveDate) -> EventEdit {
        EventEdit {
            date,
            title: self.title.clone(),
            span: self.span,
        }
    }

    pub fn series_id(&self) -> Option<Uuid> {
        match self.kind {
            Kind::Weekly => Some(self.id),
            Kind::Occurrence { series, .. } => Some(series),
            _ => None,
        }
    }

    pub fn scheduled_on(&self, date: NaiveDate) -> bool {
        let days = (date - self.date).num_days();
        days == 0 || (self.kind == Kind::Weekly && days > 0 && days % 7 == 0)
    }

    pub fn appears(&self, date: NaiveDate) -> bool {
        self.scheduled_on(date) && self.excluded.binary_search(&date).is_err()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        validate_event_fields(self.id, &self.title, self.span, self.kind)
    }
}

fn validate_event_fields(
    id: Uuid,
    title: &str,
    span: Span,
    kind: Kind,
) -> Result<(), &'static str> {
    if id.is_nil()
        || title.trim().is_empty()
        || title.len() > 160
        || title.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return Err("Enter a title of up to 160 bytes without control characters");
    }
    if !span.valid()
        || (kind == Kind::AllDay
            && span
                != (Span {
                    start: 0,
                    end: 1440,
                }))
    {
        return Err("Invalid event time range");
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Day {
    pub revision: u64,
    pub entries: Vec<Entry>,
}

impl Day {
    /// One result per qualifying gap. Adjacent meetings share a boundary.
    pub fn openings(&self, window: Span, minutes: u16) -> Result<Vec<Span>, &'static str> {
        if !window.valid() || minutes == 0 {
            return Err("Invalid search range or duration");
        }
        let mut busy: Vec<_> = self.entries.iter().map(|e| e.span).collect();
        busy.sort_by_key(|s| s.start);
        let mut cursor = window.start;
        let mut gaps = Vec::new();
        for span in busy {
            let start = span.start.min(window.end);
            if start.saturating_sub(cursor) >= minutes {
                gaps.push(Span {
                    start: cursor,
                    end: start,
                });
            }
            cursor = cursor.max(span.end);
            if cursor >= window.end {
                return Ok(gaps);
            }
        }
        if window.end - cursor >= minutes {
            gaps.push(Span {
                start: cursor,
                end: window.end,
            });
        }
        Ok(gaps)
    }
}
