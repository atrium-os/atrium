//! atrium-lease: the jailed half of atrium-net that parses what the network
//! sends (docs/spec/atrium-net.md §3). This crate is the logic; the binary
//! talks to atrium-net over a socketpair.

pub mod client;
pub mod dhcp;
pub mod frame;
pub mod ra;

#[cfg(test)]
mod tests;
