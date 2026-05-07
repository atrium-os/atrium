//! atrium-volumes — Atrium volume-allocation broker.
//!
//! Companion to jaild. jaild owns mount operations (nmount on
//! a jail's chroot path, lo0 alias setup, etc.); atrium-volumes
//! owns *what gets mounted* — creating per-jail
//! datasets/directories, setting ownership/quotas, returning
//! host paths.
//!
//! Spec: `docs/spec/atrium-volumes.md`. Architectural context:
//! `docs/spec/storage.md`, `docs/spec/service-management.md` §5
//! (the rule that put this in its own daemon).
//!
//! ## Discipline (per `LANGUAGE-POLICY.md` smallest-TCB carve-out)
//!
//! - `#![deny(unsafe_code)]` at the crate root. The only
//!   `unsafe` lives in `mod ffi`, which is opt-in to that lint
//!   and wraps the libc + filesystem calls we need.
//! - No async runtime. Single-threaded blocking accept loop.
//! - Aqueduct-shaped Unix-socket protocol; portcullisd is the
//!   sole client (`getpeereid`-checked).
//! - Plugin trait per backend kind. tessera, tmpfs, plain in
//!   V0; zfs in a follow-up commit; future btrfs/etc. trivial.

#![deny(unsafe_code)]

pub mod error;
pub mod ffi;
pub mod plugin;
pub mod policy;
pub mod protocol;
pub mod server;
pub mod state;

pub use error::VolumesError;
pub use protocol::{ProvisionRequest, Request, Response, VolumeKind, VolumeSpec};
