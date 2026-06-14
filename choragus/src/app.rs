//! choragus's app-layer glue: the [`Role`] ⟷ wire-byte mapping.
//!
//! The registration **wire** itself (`AppMsg`, the capability bits, the hello)
//! lives in `lyra_protocol::app`, so an audio app needs only that crate — not
//! this policy one. Here we add the policy meaning: a wire role byte ⟷ the
//! [`Role`] enum the session layer reasons about.

use crate::policy::Role;

pub use lyra_protocol::app::{
    cap_names, read_hello, write_hello, AppMsg, APP_FRAME_LEN, CAP_AUDIO, CAP_MICROPHONE,
    CAP_MONITOR, DENIED,
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
