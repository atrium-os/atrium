//! Organizer identity and the original occurrence stay independent of its proposed time.
use crate::{Entry, EventEdit, Kind};
use chrono::NaiveDate;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
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
    pub limit: Option<crate::WeeklyLimit>,
    #[serde(default)]
    pub exceptions: Vec<IncludedMove>,
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

pub const MAX_MOVES: usize = 8;

impl Proposal {
    pub fn validate(&self, source: &Source) -> Result<(), String> {
        if self.exceptions.len() > MAX_MOVES {
            return Err("Too many included moves".into());
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
        if self.limit.is_some() || !self.exceptions.is_empty() {
            let base = self
                .row
                .as_ref()
                .filter(|r| r.kind == Kind::Weekly)
                .ok_or("Missing weekly master")?;
            let last = self.limit.map(|rule| rule.last(base)).transpose()?;
            let mut dates = std::collections::HashSet::new();
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
        Ok(())
    }
    pub fn scope(&self) -> Option<String> {
        if self.weekly_cancellation {
            return Some("weekly".into());
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
pub struct Cancellation {
    pub key: MeetingKey,
    pub original: Option<NaiveDate>,
    pub sequence: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    #[serde(default)]
    pub before_moves: Vec<Option<Entry>>,
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
