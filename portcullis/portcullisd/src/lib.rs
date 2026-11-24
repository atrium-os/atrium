//! portcullisd library — currently exports the `jaild_client`
//! module so smoke binaries (and, later, the daemon's main.rs)
//! can drive jaild directly without each caller re-implementing
//! the framing + SCM_RIGHTS dance.
//!
//! The existing portcullisd `main.rs` predates the privsep
//! architecture and shells out to `jail(8)` itself; that path
//! gets migrated to `jaild_client` over the course of Phase 4.

#![deny(unsafe_code)]

/// Boot order: `after` edges, classes, held dependents (fast-boot.md §6).
pub mod boot_graph;
pub mod cdm;
pub mod home;
pub mod host_mount;
pub mod init_phase;
pub mod jaild_client;
pub mod jail_request;
/// Settings' Apps page (curia.md §13).
pub mod access;
pub mod crossing;
/// Opening files in apps (opening-files.md §6.2).
pub mod links;
pub mod listeners;
pub mod opens;
pub mod persona_colours;
pub mod persona_last;
pub mod portal;
/// The manifest trust gate, re-exported so existing call sites keep working.
/// It lives in `portcullis-trust` so the CLI and `atrium-launch` share it
/// rather than each carrying their own answer (they did).
pub use portcullis_trust as manifest_trust;
/// Rate and concurrency limits for the one-shot jail path — the first daemon
/// verb a program calls in a loop rather than a person clicking something.
pub mod ratelimit;
/// A persona's route domain (vpn.md §5.4).
pub mod route_domain;
pub mod supervisor;
/// What a system worker's bundle may be, and its per-caller bounds.
pub mod system_worker;
pub mod system_services;
pub mod volumes_client;

pub mod workbook_versions;
