//! Organizer identity and the original occurrence stay independent of its proposed time.
use crate::{Entry, EventEdit, Kind};
use chrono::{NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use portcullis_ipc::calendar::{address, Source};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeetingKey {
    pub account: u32,
    pub attendee: String,
    pub organizer: String,
    pub uid: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct OccurrenceId {
    pub date: NaiveDate,
    pub minute: u16,
    pub zone: Option<Tz>,
}
impl OccurrenceId {
    pub fn in_series(self, base: &Entry) -> Result<Self, &'static str> {
        let mismatch = "The skipped time does not match the weekly occurrence.";
        if base.kind != Kind::Weekly || self.minute >= 1440 {
            return Err(mismatch);
        }
        let source = base.zone.map(|z| z.name);
        let mut result = self;
        let mut supplied_instant = None;
        if self.zone != source {
            let (Some(from), Some(to)) = (self.zone, source) else {
                return Err(mismatch);
            };
            let wall = self
                .date
                .and_hms_opt(u32::from(self.minute / 60), u32::from(self.minute % 60), 0)
                .ok_or(mismatch)?;
            let instant = from
                .from_local_datetime(&wall)
                .single()
                .ok_or(mismatch)?
                .to_utc();
            let local = instant.with_timezone(&to);
            result = Self {
                date: local.date_naive(),
                minute: (local.hour() * 60 + local.minute()) as u16,
                zone: source,
            };
            supplied_instant = Some(instant);
        }
        if result.minute != base.span.start || !base.scheduled_on(result.date) {
            return Err(mismatch);
        }
        if let Some(zone) = base.zone {
            let (start, _) = zone.interval(result.date, base.span, true)?;
            if supplied_instant.is_some_and(|i| i != start) {
                return Err(mismatch);
            }
        }
        Ok(result)
    }

    pub fn property(self) -> String {
        let stamp = format!(
            "{}T{:02}{:02}00",
            self.date.format("%Y%m%d"),
            self.minute / 60,
            self.minute % 60
        );
        match self.zone {
            Some(chrono_tz::UTC) => format!("RECURRENCE-ID:{stamp}Z"),
            Some(zone) => format!("RECURRENCE-ID;TZID={zone}:{stamp}"),
            None => format!("RECURRENCE-ID:{stamp}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeRule {
    pub anchor: NaiveDate,
    pub sequence: u32,
    pub schedule: EventEdit,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelledInterval {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u32>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub copies: std::collections::BTreeMap<NaiveDate, uuid::Uuid>,
    pub first: NaiveDate,
    pub restart: NaiveDate,
}

impl CancelledInterval {
    pub fn contains(&self, original: NaiveDate) -> bool {
        self.first <= original && original < self.restart
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancelled_intervals: Vec<CancelledInterval>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<RangeRule>,
    /// The range update that generated this detached occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_from: Option<NaiveDate>,
    /// Highest accepted range revision on the weekly master.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_sequence: Option<u32>,
    /// Organizer's most recently accepted EXDATE set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skips: Vec<OccurrenceId>,
    /// Exclusions that can be lifted when the organizer reinstates a date.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_skips: Vec<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<crate::WeeklyLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included: Vec<OccurrenceId>,
    #[serde(flatten)]
    pub key: MeetingKey,
    pub sequence: u32,
    pub location: String,
    pub accepted: EventEdit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<OccurrenceId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default)]
    pub this_and_future: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skips: Vec<OccurrenceId>,
    #[serde(default)]
    pub limit: Option<crate::WeeklyLimit>,
    #[serde(default)]
    pub exceptions: Vec<IncludedMove>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancelled_dates: Vec<CancelledDate>,
    pub uid: String,
    pub sequence: u32,
    pub organizer: String,
    pub attendees: Vec<String>,
    pub location: String,
    pub original: Option<OccurrenceId>,
    pub weekly_cancellation: bool,
    /// A cancellation has no replacement schedule.
    pub row: Option<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncludedMove {
    pub original: OccurrenceId,
    pub row: Entry,
    pub location: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CancelledDate {
    pub original: OccurrenceId,
    pub sequence: u32,
}
pub const MAX_MOVES: usize = 8;
pub const MAX_CANCELLED_DATES: usize = 8;

impl Proposal {
    // Return source-clock occurrences only after comparing Mail's independently
    // read wire scope. Different representations can designate the same instant.
    pub(crate) fn validate_delivery(&self, source: &Source) -> Result<Vec<OccurrenceId>, String> {
        if self.exceptions.len().saturating_add(self.skips.len()) > MAX_MOVES {
            return Err("Too many included moves or skipped dates".into());
        }
        if !self.cancelled_dates.is_empty() {
            let mut dates = std::collections::HashSet::new();
            if !(2..=MAX_CANCELLED_DATES).contains(&self.cancelled_dates.len())
                || self.cancelled_dates[0].sequence != self.sequence
                || self.row.is_some()
                || self.original.is_some()
                || self.weekly_cancellation
                || self.limit.is_some()
                || !self.exceptions.is_empty()
                || self
                    .cancelled_dates
                    .iter()
                    .any(|o| o.original.minute >= 1440 || !dates.insert(o.original.date))
            {
                return Err("Invalid group of cancelled occurrences".into());
            }
        }
        if self.this_and_future
            && (self.original.is_none()
                || self
                    .row
                    .as_ref()
                    .is_some_and(|r| r.kind != Kind::Appointment)
                || self.limit.is_some()
                || !self.exceptions.is_empty()
                || !self.skips.is_empty()
                || !self.cancelled_dates.is_empty()
                || self.weekly_cancellation)
        {
            return Err("Unsupported range invitation".into());
        }
        let scope = self.scope();
        let bound = source
            .delivery
            .as_ref()
            .map_or(scope.is_none() && self.row.is_some(), |d| {
                d.uid == self.uid
                    && d.sequence == self.sequence
                    && d.organizer.eq_ignore_ascii_case(&self.organizer)
                    && d.cancelled == self.row.is_none()
                    && d.scope == scope
            });
        if !bound {
            return Err(
                "Mail and Calendar disagree on the meeting or its scope. Reopen it in Mail.".into(),
            );
        }
        if !source.valid()
            || !address(&self.organizer)
            || self.uid.is_empty()
            || self.uid.len() > 200
            || !self
                .uid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
            || self.location.len() > 160
            || self.location.chars().any(char::is_control)
            || self.attendees.len() > 256
            || self.attendees.iter().any(|a| !address(a))
            || (self.weekly_cancellation && (self.row.is_some() || self.original.is_some()))
            || self.original.is_some_and(|o| o.minute >= 1440)
        {
            return Err("Unsupported invitation details".into());
        }
        self.validate_schedule()
    }

    /// Validate the schedule and return unique skipped slots in the series clock.
    pub fn validate_schedule(&self) -> Result<Vec<OccurrenceId>, String> {
        if let Some(row) = &self.row {
            if !matches!(row.kind, Kind::Appointment | Kind::AllDay | Kind::Weekly)
                || (self.original.is_some() && row.kind != Kind::Appointment)
                || row.source.is_some()
                || row.invitation.is_some()
                || row.import_hash.is_some()
                || !row.excluded.is_empty()
                || !row.span.valid()
                || row.validate().is_err()
            {
                return Err("Unsupported invitation schedule".into());
            }
        }
        let mut normalized = Vec::new();
        if self.limit.is_some() || !self.exceptions.is_empty() || !self.skips.is_empty() {
            let base = self
                .row
                .as_ref()
                .filter(|r| r.kind == Kind::Weekly)
                .ok_or("Missing weekly master")?;
            let last = self.limit.map(|rule| rule.last(base)).transpose()?;
            let mut dates = std::collections::HashSet::new();
            for skip in &self.skips {
                let original = skip.in_series(base)?;
                if last.is_some_and(|end| original.date > end) {
                    return Err("The skipped date is outside the series.".into());
                }
                if dates.insert(original.date) {
                    normalized.push(original);
                }
            }
            normalized.sort_by_key(|o| o.date);
            for m in &self.exceptions {
                if !dates.insert(m.original.date)
                    || !base.scheduled_on(m.original.date)
                    || last.is_some_and(|end| m.original.date > end)
                    || m.original.minute >= 1440
                    || m.location.len() > 160
                    || m.location.chars().any(char::is_control)
                    || m.row.kind != Kind::Appointment
                    || m.row.invitation.is_some()
                    || m.row.source.is_some()
                    || m.row.import_hash.is_some()
                    || !m.row.excluded.is_empty()
                    || !m.row.span.valid()
                    || m.row.validate().is_err()
                {
                    return Err("Invalid or duplicate occurrence in this series".into());
                }
            }
        }
        Ok(normalized)
    }
    pub fn validate(&self, source: &Source) -> Result<(), String> {
        self.validate_delivery(source).map(drop)
    }
    pub fn scope(&self) -> Option<String> {
        if !self.cancelled_dates.is_empty() {
            let mut scope = String::from("cancel-dates");
            for date in &self.cancelled_dates {
                scope.push_str(&format!("|{}/{}", date.sequence, date.original.property()));
            }
            return Some(scope);
        }
        if self.weekly_cancellation {
            return Some("weekly".into());
        }
        if self.this_and_future {
            return self.original.map(|o| {
                o.property()
                    .replacen("RECURRENCE-ID", "RECURRENCE-ID;RANGE=THISANDFUTURE", 1)
            });
        }
        self.original.map(OccurrenceId::property).or_else(|| {
            self.row
                .as_ref()
                .filter(|r| r.kind == Kind::Weekly)
                .map(|_| {
                    let mut scope = String::from("weekly");
                    if let Some(rule) = self.limit {
                        scope.push(';');
                        scope.push_str(&rule.property());
                    }
                    for skip in &self.skips {
                        scope.push('|');
                        scope.push_str(&skip.property().replacen("RECURRENCE-ID", "EXDATE", 1));
                    }
                    for m in &self.exceptions {
                        scope.push('|');
                        scope.push_str(&m.original.property());
                    }
                    scope
                })
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelledTail {
    pub master: Entry,
    pub exceptions: Vec<Entry>,
    pub copies: Vec<(NaiveDate, uuid::Uuid)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cancellation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<CancelledTail>,
    #[serde(default)]
    pub this_and_future: bool,
    pub key: MeetingKey,
    pub original: Option<NaiveDate>,
    pub sequence: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipEffect {
    Cancel,
    KeepEarlier,
    Skip,
    AlreadyAbsent,
    ChooseLocalMove,
    KeepLocalMove,
    Restore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkipReview {
    pub original: OccurrenceId,
    pub current: Option<Entry>,
    pub effect: SkipEffect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipDecision {
    pub original: NaiveDate,
    pub keep: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RangeDate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_copy: Option<EventEdit>,
    pub original: NaiveDate,
    pub before: Option<EventEdit>,
    pub after: Option<EventEdit>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    #[serde(default)]
    pub reinstating: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_before_restart: Option<CancelledInterval>,
    #[serde(default)]
    pub range_summary: Option<String>,
    #[serde(default)]
    pub range_dates: Vec<RangeDate>,
    #[serde(default)]
    pub skips: Vec<SkipReview>,
    #[serde(default)]
    #[serde(rename = "before_moves")]
    pub previous_dates: Vec<Option<Entry>>,
    pub previous: Option<Entry>,
    pub attendee: String,
    pub weekly: bool,
    pub can_save: bool,
    pub token: Option<uuid::Uuid>,
    pub notice: String,
}

impl Review {
    pub(super) fn blocked(mut self, reason: &str) -> Self {
        self.notice = reason.into();
        self.token = None;
        self.can_save = false;
        self
    }
}
