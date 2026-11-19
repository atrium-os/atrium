//! Crypta's AnyConnect engine core (docs/spec/vpn.md §9): sans-IO pieces
//! the engine process (`atrium-crypta-ac`) drives over its TLS stream.
//!
//! - [`xml`]: the strict, bounded XML subset gateways speak;
//! - [`http`]: request heads we write, response heads and bodies we read;
//! - [`auth`]: the `config-auth` sign-in exchange as typed prompts
//!   (forms, SSO, posture), our replies in zeroizing memory;
//! - [`cstp`]: the `CONNECT`, the pushed settings, CSTP frames.
//!
//! What the gateway learns (§9.5): the OS class `Linux` (decision 14), the
//! client as Atrium, truthfully, and the hostname the profile gives.

pub mod auth;
pub mod cstp;
pub mod http;
pub mod xml;

/// Gateways recognise an AnyConnect-protocol client by this word in the
/// agent string; the rest says what it is.
pub const USER_AGENT: &str = "AnyConnect-compatible Atrium VPN Agent v0.1";
/// `version who="vpn"`.
pub const VERSION: &str = "Atrium 0.1";
/// The OS class (decision 14): what OpenConnect sends from every Unix.
pub const DEVICE_ID: &str = "linux-64";
