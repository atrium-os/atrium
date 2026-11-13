//! Capsa's FUSE server (docs/spec/device-assignment.md §4.6).
//!
//! It is handed two open file descriptors and nothing else: the partition
//! and `/dev/fuse` (already mounted by jaild, outside the jail). It speaks
//! the FUSE kernel protocol directly, so the kernel side it talks to is
//! FreeBSD's `fusefs(5)` (or Linux's, for host tests) and the parsing of
//! untrusted media happens here, in a jail, in Rust.
//!
//! Requests from the kernel are trusted only as far as their framing: every
//! length is checked before use, and a malformed request gets `EINVAL`.

pub mod backend;
pub mod proto;
pub mod server;

pub use backend::{Attr, Backend};
pub use server::{serve, Options};
