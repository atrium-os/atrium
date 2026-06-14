//! The app-facing registration protocol — what an audio app says to choragusd.
//!
//! An app does not talk to the RT engine to *start*; it registers with the
//! session layer, declaring its **role** (from its manifest). choragusd assigns a
//! stream id, applies policy (routing + ducking of others), and commands lyrad to
//! open the mixer slot. This is the control handshake; the audio data path (the
//! app's samples → lyrad's ring) is a separate edge (today the demo engine
//! synthesises the tone).
//!
//! Frame: a fixed **8 bytes**, little-endian.
//! ```text
//!   byte 0     : tag (1 = Register, 2 = Close)
//!   byte 1     : role (Register only; see role_to_u8)
//!   bytes 2..4 : reserved
//!   bytes 4..8 : stream id (Close only)
//! ```
//! The reply to a `Register` is the 4-byte assigned stream id.

use crate::policy::Role;

pub const APP_FRAME_LEN: usize = 8;

const TAG_REGISTER: u8 = 1;
const TAG_CLOSE: u8 = 2;

/// Requested-capability bits an app sends with `Register` (§9). choragusd checks
/// these against the app's Portcullis grant and refuses the ones not held.
pub const CAP_AUDIO: u8 = 1;
pub const CAP_MICROPHONE: u8 = 2;
pub const CAP_MONITOR: u8 = 4;

/// Human-readable names of the set bits, for logging a denial.
pub fn cap_names(bits: u8) -> Vec<&'static str> {
    let mut v = Vec::new();
    if bits & CAP_AUDIO != 0 { v.push("audio"); }
    if bits & CAP_MICROPHONE != 0 { v.push("microphone"); }
    if bits & CAP_MONITOR != 0 { v.push("audio_monitor"); }
    v
}

/// Sentinel stream id in the registration reply meaning "denied".
pub const DENIED: u32 = u32::MAX;

pub fn role_to_u8(r: Role) -> u8 {
    match r {
        Role::Media => 0,
        Role::Communication => 1,
        Role::Notification => 2,
        Role::Game => 3,
        Role::Pro => 4,
    }
}

pub fn role_from_u8(b: u8) -> Option<Role> {
    Some(match b {
        0 => Role::Media,
        1 => Role::Communication,
        2 => Role::Notification,
        3 => Role::Game,
        4 => Role::Pro,
        _ => return None,
    })
}

/// Parse a role from a CLI word.
pub fn role_from_str(s: &str) -> Option<Role> {
    Some(match s.to_ascii_lowercase().as_str() {
        "media" => Role::Media,
        "comms" | "communication" => Role::Communication,
        "notification" | "notify" => Role::Notification,
        "game" => Role::Game,
        "pro" => Role::Pro,
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AppMsg {
    /// "I want to play, as this role, requesting these capability bits."
    /// choragusd replies with the assigned id, or [`DENIED`].
    Register { role: Role, caps: u8 },
    /// "I am done with this stream."
    Close { stream: u32 },
}

impl AppMsg {
    pub fn encode(&self) -> [u8; APP_FRAME_LEN] {
        let mut f = [0u8; APP_FRAME_LEN];
        match *self {
            AppMsg::Register { role, caps } => {
                f[0] = TAG_REGISTER;
                f[1] = role_to_u8(role);
                f[2] = caps;
            }
            AppMsg::Close { stream } => {
                f[0] = TAG_CLOSE;
                f[4..8].copy_from_slice(&stream.to_le_bytes());
            }
        }
        f
    }

    pub fn decode(b: &[u8]) -> Option<AppMsg> {
        if b.len() < APP_FRAME_LEN {
            return None;
        }
        match b[0] {
            TAG_REGISTER => Some(AppMsg::Register { role: role_from_u8(b[1])?, caps: b[2] }),
            TAG_CLOSE => {
                Some(AppMsg::Close { stream: u32::from_le_bytes(b[4..8].try_into().ok()?) })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_close_round_trip() {
        for m in [
            AppMsg::Register { role: Role::Communication, caps: CAP_AUDIO },
            AppMsg::Register { role: Role::Media, caps: CAP_AUDIO | CAP_MONITOR },
            AppMsg::Close { stream: 9 },
        ] {
            assert_eq!(AppMsg::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn cap_names_lists_set_bits() {
        assert_eq!(cap_names(CAP_AUDIO | CAP_MONITOR), vec!["audio", "audio_monitor"]);
    }

    #[test]
    fn all_roles_survive_the_byte() {
        for r in [Role::Media, Role::Communication, Role::Notification, Role::Game, Role::Pro] {
            assert_eq!(role_from_u8(role_to_u8(r)), Some(r));
        }
    }

    #[test]
    fn cli_words_parse() {
        assert_eq!(role_from_str("comms"), Some(Role::Communication));
        assert_eq!(role_from_str("Media"), Some(Role::Media));
        assert_eq!(role_from_str("nonsense"), None);
    }
}
