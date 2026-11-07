//! Vinculum's LE Security Manager (docs/spec/bluetooth.md §6.2, §7).
#![forbid(unsafe_code)]

pub mod crypto;
pub mod pairing;

/// Generated SMP packets (`pdl/smp.pdl`).
#[allow(warnings, clippy::all)]
pub mod pdu {
    include!(concat!(env!("OUT_DIR"), "/smp.rs"));
}
