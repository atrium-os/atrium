//! Mail's protocol codecs (docs/spec/mail.md §6.1), sans-I/O.
//!
//! [`imap`] decodes server responses (with literals) into typed data and
//! encodes the commands sync needs; [`smtp`] runs a submission after the
//! courier has logged in. Neither has a way to log in or to start TLS:
//! the Arca mail courier does both and refuses them from us (§5.2).
//!
//! [`jmap`] builds JMAP requests and reads their answers (RFC 8620, 8621);
//! the HTTP and its credential are the courier's.

pub mod imap;
pub mod jmap;
pub mod smtp;
pub mod utf7;
