//! `CLASS_PRINT`: prelumd's Aqueduct surface (printing.md §6.4).
//!
//! | op | request | reply (`IS_RESPONSE`): `u8 status`, then |
//! |---|---|---|
//! | `PRINTERS` 0x00 | empty | postcard `Vec<Printer>` |
//! | `ADD` 0x01 | postcard [`Add`] | postcard [`Printer`] (asked, learned and saved), or a `String` |
//! | `REMOVE` 0x02 | postcard `u32` (printer) | postcard `String` |
//! | `RENAME` 0x03 | postcard [`Rename`] | postcard `String` |
//! | `REPIN` 0x04 | postcard `u32` | postcard [`Printer`] with its new identity, or a `String`: "Use the new identity" (§7.3) |
//! | `IDENTIFY` 0x05 | postcard `u32` | postcard `String` |
//! | `REFRESH` 0x06 | postcard `u32` | postcard [`Printer`]: asked again now |
//! | `NEARBY` 0x07 | postcard `u8`: seconds to listen (at most 10) | postcard `Vec<Nearby>`: printers found by mDNS and confirmed over IPP (§7.1) |
//! | `SHARE` 0x08 | postcard [`Share`] | postcard `String`: the printer is now shared, or its persona's again (§9) |
//! | `HISTORY` 0x09 | empty | postcard `Vec<Ended>`: the last 30 days' jobs, newest first (§6.5) |
//! | `CLEAR_HISTORY` 0x0A | empty | postcard `String` |
//! | `PRINT` 0x10 | postcard [`Print`], the PDF as the message's one descriptor | postcard `u64` (the job) or a `String` |
//! | `JOBS` 0x11 | empty | postcard `Vec<Job>` |
//! | `CANCEL` 0x12 | postcard `u64` | postcard `String` |
//! | `RELEASE` 0x13 | postcard `u64` | postcard `String`: a held job goes back in its queue |
//! | `PREVIEW` 0x14 | postcard [`PreviewReq`], the PDF as the one descriptor | postcard [`Previews`]: small sheet images drawn in a render jail (§5.2) |
//! | `WATCH` 0x16 | postcard `u64`: the last generation seen (0 at first) | postcard [`Watched`]: the jobs once they differ from that generation, or after 25 s unchanged |
//! | `SAVE_PDF` 0x15 | postcard [`Layout`], the PDF as the one descriptor | postcard `u64` (its size), the rewritten PDF as the reply's one descriptor (§5.2's Save as PDF) |
//! | `PRINT_PIN` 0x17 | postcard `(Print, [u8; 16])`, the Arca handle and a PDF descriptor | postcard `u64`: queued job id |
//! | `PIN_BINDING` 0x18 | postcard `u32`: saved printer ID | postcard [`PinBinding`], only for the acting print sheet |
//! | `FOR` 0x20 | postcard `u32`: the app's uid, as limend named the sheet's parent | postcard `String`: the app's persona ("" for Personal). Only from prelum-sheet's uid, once per connection: from then on the connection acts for that app's person (§5.2) |
//!
//! On anything but `OK` the payload after the status is a postcard
//! `String` to show. Every caller sees only its own printers and jobs: the
//! daemon keys both by the person, the peer's uid (`getpeereid`), or for
//! prelum-sheet after `FOR`, the person whose app asked to print.
//!
//! Two apps act for their person without `FOR`, named by the launch registry
//! (never by what they say): Curia's Printers page (`printers-configure`:
//! everything but printing) and prelum-status, the Forum status item and
//! Praeco notices (§6.6: `PRINTERS`, `JOBS`, `WATCH`, `CANCEL`, `RELEASE`).
//! Any other app is refused: apps print through the sheet.

use prelum_ipp_codec::ops::{ColorMode, Quality, Sides};
use prelum_ipp_codec::printer::{Marker, PrinterState, StateReason};
use serde::{Deserialize, Serialize};

/// Next free after `CLASS_NETCONF` (18) and vpn.md's `CLASS_VPN` (19).
pub use aqueduct::classes::CLASS_PRINT as CLASS;

pub mod op {
    pub const PRINTERS: u16 = 0x00;
    pub const ADD: u16 = 0x01;
    pub const REMOVE: u16 = 0x02;
    pub const RENAME: u16 = 0x03;
    pub const REPIN: u16 = 0x04;
    pub const IDENTIFY: u16 = 0x05;
    pub const REFRESH: u16 = 0x06;
    pub const NEARBY: u16 = 0x07;
    pub const SHARE: u16 = 0x08;
    pub const HISTORY: u16 = 0x09;
    pub const CLEAR_HISTORY: u16 = 0x0A;
    pub const PRINT: u16 = 0x10;
    pub const JOBS: u16 = 0x11;
    pub const CANCEL: u16 = 0x12;
    pub const RELEASE: u16 = 0x13;
    pub const PREVIEW: u16 = 0x14;
    pub const SAVE_PDF: u16 = 0x15;
    pub const WATCH: u16 = 0x16;
    pub const PRINT_PIN: u16 = 0x17;
    pub const PIN_BINDING: u16 = 0x18;
    pub const FOR: u16 = 0x20;
}

pub mod status {
    pub const OK: u8 = 0;
    pub const MALFORMED: u8 = 1;
    pub const NOT_ALLOWED: u8 = 2;
    pub const NOT_FOUND: u8 = 3;
    /// The printer could not be reached.
    pub const OFFLINE: u8 = 4;
    /// The printer is not the one that was saved (§7.3).
    pub const IDENTITY_CHANGED: u8 = 5;
    pub const FAILED: u8 = 6;
}

/// Add a printer by address: `ipps://host[:port]/path`, `ipp://…`, or a
/// bare host (IPPS tried first, then IPP, on 631, `/ipp/print`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Add {
    pub uri: String,
    /// What to call it; empty: the printer's own `printer-info` or model.
    pub name: String,
}

/// Make a printer shared by every persona, or its own persona's again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub printer: u32,
    pub shared: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rename {
    pub printer: u32,
    pub name: String,
}

/// The authenticated person and pinned destination of a desktop PIN job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinBinding {
    pub user: String,
    pub persona: String,
    pub key: [u8; 32],
    pub uuid: String,
}

/// A saved printer as clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Printer {
    pub id: u32,
    pub name: String,
    pub uri: String,
    /// IPPS; a plain IPP printer is shown as unencrypted.
    pub tls: bool,
    /// The pinned key's fingerprint (`SHA256:` base64), IPPS only.
    pub fingerprint: Option<String>,
    pub uuid: Option<String>,
    pub make_and_model: String,
    pub location: String,
    /// The network it was added on (§7.3).
    pub network: String,
    /// Not on the network it was added on: it is not contacted.
    pub elsewhere: bool,
    pub state: Option<PrinterState>,
    pub reasons: Vec<StateReason>,
    pub markers: Vec<Marker>,
    pub media_supported: Vec<String>,
    pub media_default: Option<String>,
    pub duplex: bool,
    pub color: bool,
    /// No job has gone to it yet (§7.2: the sheet says so).
    pub new: bool,
    /// Its identity changed since it was saved (§7.3): what it shows now.
    pub changed: Option<Changed>,
    /// The persona it was added from ("" for Personal), §9.
    pub persona: String,
    /// Every persona's (§9).
    pub shared: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changed {
    pub fingerprint: Option<String>,
    pub uuid: Option<String>,
}

/// A printer found on the LAN (§7.1), as the printer itself described it
/// when asked over IPP; multicast only said where to ask.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nearby {
    /// What to `ADD`: `ipps://address:port/path` (or `ipp://`).
    pub uri: String,
    /// The printer's `printer-info`, or its model, or the mDNS name.
    pub name: String,
    pub make_and_model: String,
    pub location: String,
    /// The name it announced over mDNS (cut and cleaned).
    pub announced: String,
    pub tls: bool,
    /// Its TLS key's fingerprint, IPPS only.
    pub fingerprint: Option<String>,
    pub uuid: Option<String>,
    pub color: bool,
    pub duplex: bool,
    /// One of this person's saved printers, with the same identity.
    pub saved: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scale {
    Fit,
    ShrinkToFit,
    Percent(u16),
}

/// What the sheet sends with the document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Print {
    pub printer: u32,
    /// The document's title; shown in history and sent as `job-name`.
    pub title: String,
    pub copies: u32,
    pub sides: Sides,
    pub color: ColorMode,
    /// A PWG media keyword; `None`: the printer's default.
    pub media: Option<String>,
    pub quality: Option<Quality>,
    /// `1-3,5`; `None`: all.
    pub pages: Option<String>,
    /// Pages per sheet: 1, 2 or 4.
    pub per_sheet: u8,
    pub scale: Scale,
}

impl Print {
    pub fn new(printer: u32, title: &str) -> Print {
        Print {
            printer,
            title: title.into(),
            copies: 1,
            sides: Sides::OneSided,
            color: ColorMode::Color,
            media: None,
            quality: None,
            pages: None,
            per_sheet: 1,
            scale: Scale::ShrinkToFit,
        }
    }
}

/// How pages go on paper, for a preview or Save as PDF: what [`Print`]
/// says of the same things.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// A PWG media keyword (the paper).
    pub media: String,
    /// `1-3,5`; `None`: all.
    pub pages: Option<String>,
    /// 1, 2 or 4.
    pub per_sheet: u8,
    pub scale: Scale,
}

impl Layout {
    pub fn new(media: &str) -> Layout {
        Layout { media: media.into(), pages: None, per_sheet: 1, scale: Scale::ShrinkToFit }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewReq {
    pub layout: Layout,
    /// From 1.
    pub first: u32,
    /// At most [`MAX_PREVIEWS`].
    pub count: u8,
    /// The longer side, pixels, at most [`MAX_PREVIEW`].
    pub longest: u16,
}

pub const MAX_PREVIEWS: u8 = 8;
pub const MAX_PREVIEW: u16 = 400;

/// One sheet's preview: 8-bit sRGB, `w` x `h`, rows top down.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheetImage {
    pub sheet: u32,
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Previews {
    /// How many sheets the whole document makes with this layout.
    pub sheets: u32,
    pub images: Vec<SheetImage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Pending,
    /// Rendering (and sending as it goes).
    Preparing,
    Sending,
    /// The printer has it and reports it processing.
    Printing,
    Done,
    Cancelled,
    Failed,
    /// Needs the person (§6.4): the printer's identity changed.
    Held,
}

impl Phase {
    pub fn ended(&self) -> bool {
        matches!(self, Phase::Done | Phase::Cancelled | Phase::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub printer: u32,
    pub title: String,
    pub phase: Phase,
    /// Sheets drawn so far, and of how many (0 until known).
    pub sheets: u32,
    pub of: u32,
    /// Why it waits, failed or is held (plain text).
    pub message: String,
    /// Seconds since the epoch.
    pub submitted: u64,
    /// What the person may have to do something about (§6.6), and since when
    /// (seconds since the epoch).
    pub alert: Option<Alert>,
    pub alert_since: u64,
}

/// Why a job needs the person, or may (§6.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Alert {
    OutOfPaper,
    Jam,
    /// A door or cover is open, a tray is out, the output tray is full.
    Open,
    /// Not answering: Praeco is told after 2 minutes.
    Offline,
    /// Held: the printer is not the one that was saved (§7.3).
    IdentityChanged,
    /// Stopped for another reason.
    Stopped,
}

impl Alert {
    /// What to tell the person, about the printer named `printer`.
    pub fn words(self, printer: &str) -> (String, String) {
        match self {
            Alert::OutOfPaper => (format!("{printer} is out of paper"), "Add paper and the job carries on.".into()),
            Alert::Jam => (format!("Paper is jammed in {printer}"), "Clear the jam and the job carries on.".into()),
            Alert::Open => (format!("{printer} needs attention"), "A door or tray is open, or the output tray is full.".into()),
            Alert::Offline => (format!("{printer} is offline"), "The job waits until the printer answers again.".into()),
            Alert::IdentityChanged => (format!("{printer} is not the printer you saved"), "The job is held. Check the printer in Settings, Printers.".into()),
            Alert::Stopped => (format!("{printer} has stopped"), "The job waits until the printer is ready.".into()),
        }
    }
}

/// What `WATCH` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    pub generation: u64,
    pub jobs: Vec<Job>,
}

/// A job that ended, as history keeps it (§6.5): no document content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub job: u64,
    /// "Document" when the printer has titles off.
    pub title: String,
    /// The printer's name when the job ended.
    pub printer: String,
    pub sheets: u32,
    /// Seconds since the epoch.
    pub at: u64,
    /// Done, Cancelled or Failed.
    pub result: Phase,
    pub message: String,
    /// "" for Personal.
    pub persona: String,
}

/// How long history keeps a job.
pub const HISTORY_DAYS: u64 = 30;

pub mod client {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    use aqueduct::{flag, Connection};
    use serde::de::DeserializeOwned;
    use serde::Serialize;

    use super::*;

    pub struct Client {
        c: Connection,
    }

    /// A refusal: the status and the daemon's message.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Refused {
        pub status: u8,
        pub message: String,
    }

    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    pub type Res<T> = Result<T, Refused>;

    fn io_refused(e: io::Error) -> Refused {
        Refused { status: status::FAILED, message: format!("prelumd: {e}") }
    }

    impl Client {
        pub fn connect(sock: &Path) -> io::Result<Client> {
            Ok(Client { c: Connection::connect(sock)? })
        }

        fn call<T: DeserializeOwned>(&mut self, o: u16, payload: &[u8], fd: Option<&dyn AsRawFd>) -> Res<T> {
            self.call_fd(o, payload, fd).map(|(v, _)| v)
        }

        /// A call whose reply may carry a descriptor.
        fn call_fd<T: DeserializeOwned>(&mut self, o: u16, payload: &[u8], fd: Option<&dyn AsRawFd>) -> Res<(T, Option<std::os::fd::OwnedFd>)> {
            match fd {
                Some(f) => self.c.send_message_with_fds(CLASS, o, flag::RESPONSE_EXPECTED, payload, &[f.as_raw_fd()]),
                None => self.c.send_message(CLASS, o, flag::RESPONSE_EXPECTED, payload),
            }
            .map_err(io_refused)?;
            let p = loop {
                let m = self.c.recv_message().map_err(io_refused)?;
                if m.opcode_class == CLASS && m.op == o && m.flags & flag::IS_RESPONSE != 0 {
                    break m.payload;
                }
            };
            let bad = || Refused { status: status::MALFORMED, message: "prelumd sent a malformed reply".into() };
            match p.split_first() {
                Some((&status::OK, rest)) => {
                    let fd = if o == op::SAVE_PDF { self.c.take_fds(1).and_then(|mut v| v.pop()) } else { None };
                    postcard::from_bytes(rest).map(|v| (v, fd)).map_err(|_| bad())
                }
                Some((&st, rest)) => Err(Refused { status: st, message: postcard::from_bytes(rest).unwrap_or_default() }),
                None => Err(bad()),
            }
        }

        fn enc<T: Serialize>(v: &T) -> Vec<u8> {
            postcard::to_stdvec(v).unwrap_or_default()
        }

        pub fn printers(&mut self) -> Res<Vec<Printer>> {
            self.call(op::PRINTERS, &[], None)
        }
        pub fn add(&mut self, a: &Add) -> Res<Printer> {
            self.call(op::ADD, &Self::enc(a), None)
        }
        pub fn remove(&mut self, id: u32) -> Res<String> {
            self.call(op::REMOVE, &Self::enc(&id), None)
        }
        pub fn rename(&mut self, r: &Rename) -> Res<String> {
            self.call(op::RENAME, &Self::enc(r), None)
        }
        pub fn repin(&mut self, id: u32) -> Res<Printer> {
            self.call(op::REPIN, &Self::enc(&id), None)
        }
        pub fn identify(&mut self, id: u32) -> Res<String> {
            self.call(op::IDENTIFY, &Self::enc(&id), None)
        }
        pub fn refresh(&mut self, id: u32) -> Res<Printer> {
            self.call(op::REFRESH, &Self::enc(&id), None)
        }
        pub fn nearby(&mut self, seconds: u8) -> Res<Vec<Nearby>> {
            self.call(op::NEARBY, &Self::enc(&seconds), None)
        }
        pub fn share(&mut self, s: &Share) -> Res<String> {
            self.call(op::SHARE, &Self::enc(s), None)
        }
        pub fn history(&mut self) -> Res<Vec<Ended>> {
            self.call(op::HISTORY, &[], None)
        }
        pub fn clear_history(&mut self) -> Res<String> {
            self.call(op::CLEAR_HISTORY, &[], None)
        }
        /// The jobs once they differ from generation `after`, or after 25 s.
        pub fn watch(&mut self, after: u64) -> Res<Watched> {
            self.call(op::WATCH, &Self::enc(&after), None)
        }
        pub fn print(&mut self, p: &Print, pdf: &dyn AsRawFd) -> Res<u64> {
            self.call(op::PRINT, &Self::enc(p), Some(pdf))
        }
        pub fn pin_binding(&mut self, printer: u32) -> Res<PinBinding> {
            self.call(op::PIN_BINDING, &Self::enc(&printer), None)
        }
        /// Submit a PDF with the opaque, one-use handle from Arca's credential UI.
        pub fn print_pin(&mut self, p: &Print, handle: [u8; 16], pdf: &dyn AsRawFd) -> Res<u64> {
            self.call(op::PRINT_PIN, &Self::enc(&(p, handle)), Some(pdf))
        }
        pub fn jobs(&mut self) -> Res<Vec<Job>> {
            self.call(op::JOBS, &[], None)
        }
        pub fn cancel(&mut self, job: u64) -> Res<String> {
            self.call(op::CANCEL, &Self::enc(&job), None)
        }
        pub fn release(&mut self, job: u64) -> Res<String> {
            self.call(op::RELEASE, &Self::enc(&job), None)
        }
        pub fn preview(&mut self, r: &PreviewReq, pdf: &dyn AsRawFd) -> Res<Previews> {
            self.call(op::PREVIEW, &Self::enc(r), Some(pdf))
        }
        /// The document rewritten with `l` (§6.2), as an unnamed file.
        pub fn save_pdf(&mut self, l: &Layout, pdf: &dyn AsRawFd) -> Res<(u64, std::fs::File)> {
            match self.call_fd::<u64>(op::SAVE_PDF, &Self::enc(l), Some(pdf))? {
                (n, Some(fd)) => Ok((n, std::fs::File::from(fd))),
                (_, None) => Err(Refused { status: status::MALFORMED, message: "prelumd sent no document".into() }),
            }
        }
        /// Act for the person whose app runs as `app_uid` (prelum-sheet only).
        pub fn act_for(&mut self, app_uid: u32) -> Res<String> {
            self.call(op::FOR, &Self::enc(&app_uid), None)
        }
    }
}

/// `OK` and a value, or a status and a message: a reply's payload.
pub fn reply<T: Serialize>(r: Result<T, (u8, String)>) -> Vec<u8> {
    match r {
        Ok(v) => {
            let mut p = vec![status::OK];
            p.extend(postcard::to_stdvec(&v).unwrap_or_default());
            p
        }
        Err((st, msg)) => {
            let mut p = vec![st];
            p.extend(postcard::to_stdvec(&msg).unwrap_or_default());
            p
        }
    }
}
