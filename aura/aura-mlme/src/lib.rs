//! aura-mlme's logic (docs/spec/wifi.md §3.4, §8.3): element parsing, the
//! management frames a station sends, and the station state machine. The
//! binary adds the I/O: `/dev/aura/ctlN`, the atrium-net socket and the
//! link to aura-keys.

#![forbid(unsafe_code)]

pub mod ie;
pub mod mgmt;
pub mod sta;

#[cfg(test)]
mod tests;
