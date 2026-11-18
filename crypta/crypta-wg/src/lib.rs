//! Crypta's WireGuard engine core (docs/spec/vpn.md §4.2).
//!
//! Written from the WireGuard paper and the protocol page: the
//! Noise_IKpsk2 handshake, the cookie reply, the transport and its replay
//! window, and the timers. Sans-IO: the engine feeds it datagrams from the
//! network and packets from `tun`, and the time; it answers with what to
//! send and what to deliver.
//!
//! The static private key is never here. Every Diffie-Hellman step with it
//! goes through [`StaticKey`], which the engine answers with Arca's
//! `DH_X25519` (§4.3); [`LocalKey`] holds one in memory for tests and
//! host tools.

pub mod crypto;
pub mod device;
pub mod key;
pub mod messages;
pub mod noise;
pub mod replay;
pub mod tunnel;

pub use device::Device;
pub use key::{KeyError, LocalKey, StaticKey};
pub use tunnel::{Action, Peer, Tunnel};

/// The timers and limits of the WireGuard paper, §6.
pub mod limits {
    use std::time::Duration;
    pub const REKEY_AFTER_MESSAGES: u64 = 1 << 60;
    pub const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 13);
    pub const REKEY_AFTER_TIME: Duration = Duration::from_secs(120);
    pub const REJECT_AFTER_TIME: Duration = Duration::from_secs(180);
    pub const REKEY_ATTEMPT_TIME: Duration = Duration::from_secs(90);
    pub const REKEY_TIMEOUT: Duration = Duration::from_secs(5);
    pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
    /// Initiations from one peer closer together than this are dropped
    /// (the protocol's flood limit).
    pub const MIN_INITIATION_INTERVAL: Duration = Duration::from_millis(50);
    /// How long a cookie we were given stays good, and how often the
    /// secret behind the cookies we hand out changes.
    pub const COOKIE_REFRESH_TIME: Duration = Duration::from_secs(120);
    /// Packets kept while a handshake is under way.
    pub const MAX_QUEUED: usize = 128;
}
