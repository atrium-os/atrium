//! Mail's message writer (docs/spec/mail.md §9).
//!
//! - [`write`]: an RFC 5322 message from a [`Draft`]: folded headers,
//!   RFC 2047 encoded words for names and subjects, plain text (and an
//!   HTML alternative), attachments, a fresh Message-ID. No `User-Agent`,
//!   no time zone (dates are UTC), no Bcc header: nothing the message
//!   doesn't need.
//! - [`reply`]: the recipients, subject, threading headers and quote of a
//!   reply, reply-all or forward, from what the parse jail checked.
//! - [`mailto`]: a `mailto:` link (RFC 6068) as fields to show, never to
//!   send on its own (§9.1).
//!
//! Everything a person types that ends up in a header is cleaned of line
//! breaks and control characters first, so no field can add a header.

pub mod mailto;
pub mod reply;
pub mod write;

pub use write::{write, Attachment, Draft, Error, Mailbox, Stamp, Written};
