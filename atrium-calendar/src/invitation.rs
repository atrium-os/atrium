//! A proposal belongs to one Mail account and attendee in this Fasti instance.
use crate::{Entry, EventEdit, Kind};
use portcullis_ipc::calendar::{address, Source};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub account: u32,
    pub attendee: String,
    pub organizer: String,
    pub uid: String,
    pub sequence: u32,
    pub location: String,
    pub accepted: EventEdit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub uid: String,
    pub sequence: u32,
    pub organizer: String,
    pub attendees: Vec<String>,
    pub location: String,
    pub row: Entry,
}

impl Proposal {
    pub fn origin(&self, source: &Source) -> Result<Origin, String> {
        let row = &self.row;
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
            || !matches!(row.kind, Kind::Appointment | Kind::AllDay)
            || row.source.is_some()
            || row.invitation.is_some()
            || row.import_hash.is_some()
            || !row.excluded.is_empty()
            || !row.span.valid()
            || row.validate().is_err()
        {
            return Err("Unsupported invitation details".into());
        }
        let mut mine = source
            .identities
            .iter()
            .filter(|id| self.attendees.iter().any(|a| a.eq_ignore_ascii_case(id)));
        let attendee = mine
            .next()
            .ok_or("This invitation does not name an identity on this Mail account")?;
        if mine.next().is_some() {
            return Err("This invitation names multiple identities on this account".into());
        }
        Ok(Origin {
            account: source.account,
            attendee: attendee.to_ascii_lowercase(),
            organizer: self.organizer.to_ascii_lowercase(),
            uid: self.uid.clone(),
            sequence: self.sequence,
            location: self.location.clone(),
            accepted: row.fields_on(row.date),
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    pub previous: Option<Entry>,
    pub can_save: bool,
    pub token: Option<uuid::Uuid>,
    pub notice: String,
}
