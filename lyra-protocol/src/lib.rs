//! Lyra audio IPC (lyra-v1.md §5). Shared by lyrad (engine), choragusd
//! (policy) and audio apps, so none of them depends on another.
//!
//! - [`wire`]   — the messages: app ⟷ choragusd and choragusd ⟷ lyrad, as
//!   length-prefixed postcard frames.
//! - [`ring`]   — the SPSC shared-memory sample ring (the data plane).
//! - [`fdpass`] — `SCM_RIGHTS` fd passing.
//! - [`peer`]   — the kernel's peer uid for a Unix socket.
//! - [`client`] — the app API: connect, open streams, write samples, events.

pub mod client;
pub mod fdpass;
pub mod peer;
pub mod ring;
pub mod wire;
