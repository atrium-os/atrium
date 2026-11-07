//! HCI packets for Vinculum (docs/spec/bluetooth.md §5.1, §6.1).
//!
//! - [`hci`]: every HCI command, event, ACL, SCO and ISO packet, generated at
//!   build time from `pdl/hci_packets.pdl` (Android's grammar) by
//!   pdl-compiler. Nothing here slices radio bytes by hand.
//! - [`h4`]: the H4 frame (type byte + HCI packet) that `vinculum-hci` and
//!   `vinculumd` exchange, and that Rootcanal speaks over TCP. Checks each
//!   packet's length field against what arrived; looks no deeper.
//! - [`gap`]: advertising data (AD structures), from `pdl/gap.pdl`.
//! - [`btsnoop`]: btsnoop trace files (the format Wireshark reads), for the
//!   recorded-trace tests and the dev-only HCI log, which never carries keys.
#![forbid(unsafe_code)]

pub mod btsnoop;
pub mod gap;
pub mod h4;

/// Generated HCI packets. Lints are off for generated code only.
#[allow(warnings, clippy::all)]
pub mod hci {
    include!(concat!(env!("OUT_DIR"), "/hci_packets.rs"));
}

pub use pdl_runtime::{DecodeError, EncodeError, Packet};
