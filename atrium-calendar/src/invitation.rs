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

impl Proposal {
    pub fn validate(&self, source: &Source) -> Result<(), String> {
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
                .map(|_| "weekly".into())
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
        self
    }
}
