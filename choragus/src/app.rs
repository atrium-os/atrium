//! Capability bits and the wire role ⟷ policy role mapping.

use crate::policy::Role;
use lyra_protocol::wire;

/// What a connection may do (lyra-v1.md §4): its user's grant, capped by the
/// socket it reached ([`crate::identity`]).
pub const CAP_AUDIO: u8 = 1;
pub const CAP_MICROPHONE: u8 = 2;
pub const CAP_MONITOR: u8 = 4;

/// Human-readable names of the set capability bits, for logs.
pub fn cap_names(bits: u8) -> Vec<&'static str> {
    let mut v = Vec::new();
    if bits & CAP_AUDIO != 0 { v.push("audio"); }
    if bits & CAP_MICROPHONE != 0 { v.push("microphone"); }
    if bits & CAP_MONITOR != 0 { v.push("audio_monitor"); }
    v
}

pub fn role_from_wire(r: wire::Role) -> Role {
    match r {
        wire::Role::Media => Role::Media,
        wire::Role::Communication => Role::Communication,
        wire::Role::Notification => Role::Notification,
        wire::Role::Game => Role::Game,
        wire::Role::Pro => Role::Pro,
    }
}
