//! What Crypta's processes say to each other (docs/spec/vpn.md §3), and the
//! one file format it reads from outside: WireGuard's `.conf`.
//!
//! - [`conf`]: the importer. A `.conf` comes from a VPN provider or a
//!   server's admin, through the picker; it is hostile input, parsed with
//!   fixed limits and fuzzed (`fuzz/`).
//! - [`engine`]: the messages between cryptad and a jailed engine, both
//!   ways, as length-prefixed postcard frames on their socketpair. The
//!   engine is the part that faces the internet, so cryptad reads what it
//!   says as hostile too: bounded frames, fixed types.
//! - [`net`]: small address helpers both sides use.

pub mod b64;
pub mod conf;
pub mod engine;
pub mod net;
