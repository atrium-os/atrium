//! prelumd ⇄ `prelum-ipp` (printing.md §6.3). One worker talks to one
//! printer, at the address prelumd resolved before starting it. The first
//! frame is a [`Start`]; then requests one at a time, each answered by one
//! or more [`Ev`]s ending with a final one ([`Ev::is_final`]).
//!
//! A `Submit` or `SubmitPin` is followed by `Data` frames and an `End` (or the socket
//! closing, which abandons the job) and is answered by `JobCreated`, then
//! `Sent` once the printer has the whole document.

use std::net::SocketAddr;

use prelum_ipp_codec::ops::{Format, JobIdentity, Ticket};
use prelum_ipp_codec::printer::PrinterInfo;
use serde::{Deserialize, Serialize};

/// Who the printer must be (§7.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// The printer's URI as the printer names itself (`ipps://host:631/ipp/print`):
    /// what goes in `printer-uri` and the HTTP request line.
    pub uri: String,
    /// Where to connect: resolved once by prelumd, never by the worker.
    pub addr: SocketAddr,
    /// IPPS. Plain IPP printers have no key to pin.
    pub tls: bool,
    /// SHA-256 of the printer's TLS public key (SPKI). `None` with `tls`:
    /// learn it (adding a printer); the worker then refuses `Submit`.
    pub key: Option<[u8; 32]>,
    /// `printer-uuid`, lower case, without `urn:uuid:`. `None`: learn it.
    pub uuid: Option<String>,
    pub login: Option<LoginGrant>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginGrant(pub [u8; 16]);

impl std::fmt::Debug for LoginGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginGrant { .. }")
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinGrant {
    pub handle: [u8; 16],
    pub user: String,
    pub persona: String,
}

impl std::fmt::Debug for PinGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinGrant { .. }")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Req {
    /// `Get-Printer-Attributes`.
    Query,
    /// `Identify-Printer` with these actions (from `identify-actions-supported`).
    Identify { actions: Vec<String> },
    /// Start a job; `Data` and `End` follow.
    Submit { identity: JobIdentity, format: Format, ticket: Ticket },
    Data(Vec<u8>),
    End,
    /// `Get-Job-Attributes`.
    Job { id: i32, user: String },
    /// `Cancel-Job`.
    Cancel { id: i32, user: String },
    SubmitPin { identity: JobIdentity, format: Format, ticket: Ticket, grant: PinGrant },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Pending,
    Held,
    Processing,
    Stopped,
    Canceled,
    Aborted,
    Completed,
}

impl JobState {
    pub fn from_ipp(v: i32) -> Option<JobState> {
        Some(match v {
            3 => JobState::Pending,
            4 => JobState::Held,
            5 => JobState::Processing,
            6 => JobState::Stopped,
            7 => JobState::Canceled,
            8 => JobState::Aborted,
            9 => JobState::Completed,
            _ => return None,
        })
    }

    pub fn ended(self) -> bool {
        matches!(self, JobState::Canceled | JobState::Aborted | JobState::Completed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ev {
    /// The printer answered `Get-Printer-Attributes`; `key` is what its TLS
    /// showed (IPPS only).
    Printer { info: Box<PrinterInfo>, key: Option<[u8; 32]> },
    /// Not the pinned printer: a different TLS key or `printer-uuid`. Nothing
    /// of the job was sent.
    IdentityChanged { key: Option<[u8; 32]>, uuid: Option<String> },
    /// No connection (refused, timed out, no route): the printer is offline
    /// or not here.
    Unreachable(String),
    /// The printer refused the request: IPP status and its message (cut and
    /// checked, plain text).
    Refused { status: u16, message: String },
    /// Something else went wrong talking to it (bad HTTP, bad IPP).
    Failed(String),
    JobCreated { id: i32 },
    /// The printer has the whole document.
    Sent,
    Job { state: JobState, reasons: Vec<String>, impressions: Option<i32> },
    /// The request is done (`Identify`, `Cancel`).
    Done,
}

impl Ev {
    /// The last event for the current request (`JobCreated` is not: `Sent`
    /// or an error follows).
    pub fn is_final(&self) -> bool {
        !matches!(self, Ev::JobCreated { .. })
    }
}
