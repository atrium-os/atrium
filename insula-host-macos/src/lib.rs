//! Insula host adapter — macOS.
//!
//! Translates an [`insula_manifest::Manifest`] into the
//! macOS security descriptors the platform consumes:
//!
//! - **SBPL** — Apple's Sandbox Profile Language. A
//!   `.sb` file describing what the sandboxed process
//!   can and cannot do. See [`sbpl`].
//! - **Entitlements plist** — XML key/value pairs
//!   embedded in the code signature, gating capability-
//!   tier APIs (network, camera, microphone, files,
//!   …). See [`entitlements`].
//!
//! # Status (v0)
//!
//! Pure-functional generation only. No process launching
//! yet; that's a subsequent commit using `posix_spawn` +
//! `sandbox_init_with_parameters`. Generation is
//! platform-neutral — these are byte-output functions
//! over an in-memory manifest — so the tests run on any
//! host.
//!
//! Reference: `docs/spec/insula-host-macos.md` §2.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod entitlements;
mod error;
pub mod install;
pub mod launch;
pub mod sbpl;

pub use error::Error;
pub use install::{
    install, launch_installed, launch_installed_all, launch_installed_full,
    launch_installed_v2, launch_installed_v3, launch_installed_v4,
    launch_installed_with_log, InstalledApp,
};
pub use launch::{launch, LaunchOptions, SandboxedChild};
