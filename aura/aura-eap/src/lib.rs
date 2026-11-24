//! Aura's EAP peer for WPA2/WPA3-Enterprise (docs/spec/wifi.md §7.4).

pub mod eap;
pub mod helper;
pub mod profile;

#[cfg(feature = "peer")]
pub mod mschapv2;
#[cfg(feature = "peer")]
pub mod peap;
#[cfg(feature = "peer")]
pub mod peer;
#[cfg(feature = "peer")]
pub mod tls;
#[cfg(feature = "peer")]
pub mod ttls;

#[cfg(feature = "peer")]
pub use peer::{Config, Out, Peer, Why};
pub use profile::Method;
#[cfg(feature = "peer")]
pub use tls::{Seen, Trust};
