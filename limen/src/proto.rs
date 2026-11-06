//! limend's wire: CLASS_LIMEN, postcard payloads (limen.md §11).
//!
//! A parent's requests get a [`Reply`] (IS_RESPONSE, same op). Events go
//! out as ASYNC_EVENT: [`ParentEvent`] to a parent, [`ChildEvent`] to a
//! child or a system provider. A provider serves many instances on one
//! connection, so every child op names its embed.

use serde::{Deserialize, Serialize};

pub use crate::catalogue::SlotKind;

pub const CLASS: u8 = aqueduct::classes::CLASS_LIMEN;

pub mod op {
    // parent
    pub const REQUEST: u16 = 0x01;
    pub const PLACE: u16 = 0x02;
    pub const SEND: u16 = 0x03;
    pub const DETACH: u16 = 0x04;
    pub const SET_VISIBLE: u16 = 0x05;
    /// [`Send`](super::Send) with one descriptor in the same frame, for a
    /// role that takes one (catalogue `takes_fd`).
    pub const SEND_FD: u16 = 0x06;
    // child or system provider
    pub const PROVIDE: u16 = 0x10;
    pub const READY: u16 = 0x11;
    pub const EMIT: u16 = 0x12;
    pub const FOCUS_OUT: u16 = 0x13;
    /// [`Pip`](super::Pip): the child's window into or out of
    /// picture-in-picture.
    pub const PIP: u16 = 0x14;
    // events
    pub const EV_PARENT: u16 = 0x80;
    pub const EV_CHILD: u16 = 0x81;
}

pub mod refusal {
    pub const UNKNOWN_ROLE: u8 = 1;
    pub const NO_PROVIDER: u8 = 2;
    pub const NOT_YOUR_WINDOW: u8 = 3;
    pub const TOO_MANY: u8 = 4;
    pub const NOT_YOURS: u8 = 5;
    pub const TOO_BIG: u8 = 6;
    pub const MALFORMED: u8 = 7;
    pub const NOT_PROVIDER: u8 = 8;
    pub const LAUNCH_FAILED: u8 = 9;
    pub const NOT_ATTACHED: u8 = 10;
    pub const NOT_AN_APP: u8 = 11;
    pub const RATE: u8 = 12;
    /// The descriptor isn't a read-only regular file or pipe.
    pub const BAD_FD: u8 = 13;
    /// PiP: not this role, no recent press, not shown, or Forum said no.
    pub const NO_PIP: u8 = 14;
}

pub type EmbedId = u64;

/// frescod's embed token for a window (EV_WINDOW_EMBED_TOKEN): 16 random
/// bytes pushed only to the window's owner, so holding one proves the
/// window. App window ids are not global in split mode; tokens are.
pub type WindowToken = [u8; 16];

/// Embed a `role` child into the caller's window named by `window`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub role: String,
    pub window: WindowToken,
    /// Stacking among the parent window's slots: higher above lower (a
    /// document's video above the doc-viewer). Credential slots are above
    /// every plain one whatever this says.
    pub z: u8,
}

/// The slot's rect in the parent window's surface pixels; `None` hides it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place { pub embed: EmbedId, pub rect: Option<[f32; 4]> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Send { pub embed: EmbedId, pub bytes: Vec<u8> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detach { pub embed: EmbedId }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetVisible { pub embed: EmbedId, pub visible: bool }

/// A system service offers `role` (it must be the catalogue's provider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provide { pub role: String }

/// The child's window for `embed` is open; `window` is its embed token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready { pub embed: EmbedId, pub window: WindowToken }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emit { pub embed: EmbedId, pub bytes: Vec<u8> }

/// Tab left the child's focus order: the parent focuses its next widget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusOut { pub embed: EmbedId, pub forward: bool }

/// The child asks for picture-in-picture (`on`), or to go back into its
/// slot. `aspect` is its video's width:height, for Forum's sizing; going
/// in needs a press on the child's or the parent's window within
/// `catalogue::PIP_GESTURE_MS` (frescod routed it, so no one can fake it).
/// One embed is in PiP at a time: a new one ends the last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pip { pub embed: EmbedId, pub on: bool, pub aspect: [u32; 2] }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Embed { embed: EmbedId },
    Refused { reason: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParentEvent {
    /// The child's window is bound into the slot.
    Attached { embed: EmbedId },
    Message { embed: EmbedId, bytes: Vec<u8> },
    Suspended { embed: EmbedId },
    Resumed { embed: EmbedId },
    /// The child crashed or exited; the slot is gone.
    ChildLost { embed: EmbedId },
    Detached { embed: EmbedId },
    FocusOut { embed: EmbedId, forward: bool },
    /// The child went over the message rate; told once.
    Dropped { embed: EmbedId },
    /// The child's window went into picture-in-picture (`on`: draw a
    /// placeholder in the slot) or came back. From limend, so the parent
    /// can trust it. No `Suspended` comes while in PiP.
    Pip { embed: EmbedId, on: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildEvent {
    /// A new instance. `parent_app` is the parent's registry app id and
    /// `parent_uid` the uid limend read off the parent's connection
    /// (getpeereid; frescod's bind confirms it owns the slot's window), so
    /// a provider such as Arca can look the publisher up in the launch
    /// registry itself. Data, never authority.
    Attach { embed: EmbedId, role: String, parent_app: String, parent_uid: u32 },
    Message { embed: EmbedId, bytes: Vec<u8> },
    /// A message with one descriptor, read-only and narrowed by limend:
    /// take it with [`Child::take_fd`](crate::client::Child::take_fd).
    MessageFd { embed: EmbedId, bytes: Vec<u8> },
    Suspended { embed: EmbedId },
    Resumed { embed: EmbedId },
    /// The parent is gone or detached it: stop, the jail ends shortly.
    Detaching { embed: EmbedId },
    /// Right after `Attach`, for a role that plays sound (`media-player`):
    /// choragusd's attribution ticket. The child puts it in its Lyra `Hello`
    /// and its streams count as the parent's; there is none when the parent
    /// doesn't hold `audio` (lyra-v1.md §4). It dies with the embed.
    AudioTicket { embed: EmbedId, ticket: [u8; 16] },
    /// PiP began or ended (also when another embed's PiP replaced this one).
    Pip { embed: EmbedId, on: bool },
}
