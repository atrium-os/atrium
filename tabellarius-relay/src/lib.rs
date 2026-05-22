//! `tabellarius-relay` — the push relay's library
//! surface: the wire protocol (`proto`) and the
//! transport-independent routing core (`relay`).
//!
//! The daemon binary (`src/main.rs`) wraps
//! [`relay::Relay`] with a TCP accept loop. The
//! `tabellarius-macos` daemon depends on this crate as
//! a path dep purely for the `proto` types, so the
//! device side and the relay side can never drift.
//!
//! See `docs/spec/tabellarius.md` §3 for the design.

#![forbid(unsafe_code)]

pub mod proto;
pub mod relay;
pub mod tls;

pub use proto::{ClientMsg, PushKey, RelayMsg, read_msg, write_msg};
pub use relay::{ConnId, Relay};
pub use tls::Identity;
