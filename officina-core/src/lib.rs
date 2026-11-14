//! Officina's document core (docs/spec/officina.md §6, §5.1).
//!
//! - [`wire`]: a small, bounded binary encoding. Every length a reader takes from the
//!   bytes is checked against a limit before anything is allocated, so a hostile file
//!   costs at most the limits, never a panic.
//! - [`container`]: the chunked native file (`OFFICINA` magic, typed chunks with CRC-32,
//!   an index at the end so a reader can map chunks lazily).
//! - [`history`]: undo and redo over any op type whose `apply` returns its inverse.
//! - [`journal`]: the autosave journal, ops appended as framed records and replayed
//!   after a crash (§4.4).
//!
//! The engines (`officina-grid`, later `officina-text`) define their own ops and chunk
//! kinds on top of these.

pub mod container;
pub mod crc32;
pub mod history;
pub mod journal;
pub mod wire;
