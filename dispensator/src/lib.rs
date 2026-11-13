//! Dispensator: one owner per device (docs/spec/device-assignment.md).
//!
//! - [`rules`]: the `devices.d/*.toml` format, merged and checked.
//! - [`plan`]: devd lines (and `devinfo -v` at start) to [`plan::Op`]s.
//! - [`jail`]: a driver rule's jail, ready for jaild.
//!
//! The daemon (`atrium-dispensatord`) carries the ops out as root on
//! FreeBSD; everything here is pure and runs in host tests.

pub mod jail;
pub mod plan;
pub mod rules;
