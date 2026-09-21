//! portcullisd library — currently exports the `jaild_client`
//! module so smoke binaries (and, later, the daemon's main.rs)
//! can drive jaild directly without each caller re-implementing
//! the framing + SCM_RIGHTS dance.
//!
//! The existing portcullisd `main.rs` predates the privsep
//! architecture and shells out to `jail(8)` itself; that path
//! gets migrated to `jaild_client` over the course of Phase 4.

#![deny(unsafe_code)]

pub mod host_mount;
pub mod init_phase;
pub mod jaild_client;
/// The manifest trust gate, re-exported so existing call sites keep working.
/// It lives in `portcullis-trust` so the CLI and `atrium-launch` share it
/// rather than each carrying their own answer (they did).
pub use portcullis_trust as manifest_trust;
/// Rate and concurrency limits for the one-shot jail path — the first daemon
/// verb a program calls in a loop rather than a person clicking something.
pub mod ratelimit;
pub mod supervisor;
pub mod system_services;
pub mod volumes_client;
