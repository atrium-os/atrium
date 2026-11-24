//! Persona-local planning with floating, all-day and source-zoned commitments.
pub mod import;
pub mod invitation;
mod invitation_limit;
pub use invitation_limit::WeeklyLimit;
pub mod zone;
pub use zone::{Fold, SourceSlot, Zone};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_hash: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation: Option<invitation::Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<Zone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceSlot>,
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

/// Issued only with the reply to a committed local schedule change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoReceipt {
    pub revision: u64,
    pub token: Uuid,
}

/// Editable local fields. Fasti retains ownership of identity and recurrence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<Zone>,
    pub date: NaiveDate,
    pub title: String,
    pub span: Span,
}

impl EventEdit {
    pub fn agenda_date(&self, viewing: chrono_tz::Tz) -> Result<NaiveDate, &'static str> {
        match self.zone {
            Some(zone) => Ok(zone
                .interval(self.date, self.span, false)?
                .0
                .with_timezone(&viewing)
                .date_naive()),
            None => Ok(self.date),
        }
    }
}

impl Entry {
    pub fn fields_on(&self, date: NaiveDate) -> EventEdit {
        EventEdit {
            date: self.source.map_or(date, |s| s.date),
            title: self.title.clone(),
            span: self.source.map_or(self.span, |s| s.span),
            zone: self.zone,
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
        days == 0
            || (self.kind == Kind::Weekly
                && days > 0
                && days % 7 == 0
                && self
                    .invitation
                    .as_ref()
                    .and_then(|o| o.last)
                    .is_none_or(|last| date <= last))
    }

    pub(crate) fn remains_cancelled(&self, date: NaiveDate) -> bool {
        self.kind == Kind::Weekly
            && self
                .invitation
                .as_ref()
                .is_some_and(|o| o.cancelled_intervals.iter().any(|gap| gap.contains(date)))
    }

    pub fn appears(&self, date: NaiveDate) -> bool {
        self.scheduled_on(date)
            && !self.remains_cancelled(date)
            && self.excluded.binary_search(&date).is_err()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(zone) = self.zone {
            if matches!(self.kind, Kind::Hold | Kind::AllDay) {
                return Err("Only timed meetings have a source zone");
            }
            if let Some(source) = self.source {
                if source.parts[0].is_none()
                    || source.parts.into_iter().flatten().any(|p| {
                        !p.span.valid()
                            || p.span.start < self.span.start
                            || p.span.end > self.span.end
                            || chrono::FixedOffset::east_opt(p.offset).is_none()
                    })
                {
                    return Err("Invalid projected interval");
                }
            } else {
                zone.interval(self.date, self.span, false)?;
                return validate_identity_title(self.id, &self.title);
            }
        } else if self.source.is_some() {
            return Err("Missing source zone");
        }
        validate_event_fields(self.id, &self.title, self.span, self.kind)
    }
}

fn validate_event_fields(
    id: Uuid,
    title: &str,
    span: Span,
    kind: Kind,
) -> Result<(), &'static str> {
    validate_identity_title(id, title)?;
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

fn validate_identity_title(id: Uuid, title: &str) -> Result<(), &'static str> {
    if id.is_nil()
        || title.trim().is_empty()
        || title.len() > 160
        || title.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return Err("Enter a title of up to 160 bytes without control characters");
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Day {
    #[serde(default)]
    pub clock_gap: Option<Span>,
    pub revision: u64,
    pub entries: Vec<Entry>,
}

impl Day {
    /// One result per qualifying gap. Adjacent meetings share a boundary.
    pub fn openings(&self, window: Span, minutes: u16) -> Result<Vec<Span>, &'static str> {
        if !window.valid() || minutes == 0 {
            return Err("Invalid search range or duration");
        }
        let mut busy: Vec<_> = self.entries.iter().flat_map(Entry::busy_spans).collect();
        busy.extend(self.clock_gap);
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
