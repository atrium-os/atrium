//! portcullisd library — currently exports the `jaild_client`
//! module so smoke binaries (and, later, the daemon's main.rs)
//! can drive jaild directly without each caller re-implementing
//! the framing + SCM_RIGHTS dance.
//!
//! The existing portcullisd `main.rs` predates the privsep
//! architecture and shells out to `jail(8)` itself; that path
//! gets migrated to `jaild_client` over the course of Phase 4.

#![deny(unsafe_code)]

pub mod jaild_client;
pub mod supervisor;
pub mod system_services;
