//! Only normalized, bounded schedule data crosses from the format worker.
use crate::Entry;
use atrium_message::Message;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const FILE_LIMIT: usize = 4 << 20;
pub const ROW_LIMIT: usize = 256;
pub const FRAME_LIMIT: usize = 256 << 10;

#[derive(Message)]
#[message(version = 1)]
pub struct Parse {}
#[derive(Message)]
#[message(version = 1)]
pub struct Parsed {
    #[message(max_len = 262144)]
    pub schedule: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidates {
    pub rows: Vec<Entry>,
    pub omitted_details: bool,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum Prepared {
    New(Uuid),
    Existing(NaiveDate),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Outcome {
    Ready(Candidates),
    Blocked(Issue),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Issue {
    Format,
    Limit,
    Component,
    Recurrence,
    Zone,
    Time,
    Scheduling,
    Title,
}
impl Issue {
    pub fn message(self) -> &'static str {
        match self {
            Self::Format => "The file has an incomplete or invalid calendar record.",
            Self::Limit => "The file exceeds a calendar import limit.",
            Self::Component => "This file contains an unsupported calendar component.",
            Self::Recurrence => {
                "Only simple weekly timed series and their matching exceptions can be copied."
            }
            Self::Zone => "Custom zones, unknown zones and ambiguous clock times need review.",
            Self::Time => "This date or time span cannot be represented in Calendar.",
            Self::Scheduling => {
                "This file contains a scheduling instruction, cancellation or free-time event."
            }
            Self::Title => "An event title is missing or cannot be displayed safely.",
        }
    }
}
