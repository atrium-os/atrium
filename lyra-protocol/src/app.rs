//! The app→choragusd registration wire (the app-facing protocol).
//!
//! An audio app does not talk to the RT engine to start; it registers with the
//! session layer (choragusd), declaring its **role** and the capabilities it
//! wants. This is the wire form only — the `Role` *enum* and its policy meaning
//! live in `choragus::policy`; here a role is an opaque byte, so an app can speak
//! it without depending on the policy crate.
//!
//! Registration frame: a fixed **8 bytes**, little-endian.
//! ```text
//!   byte 0     : tag (1 = Register, 2 = Close)
//!   byte 1     : role byte (Register only)
//!   byte 2     : requested capability bits (Register only)
//!   byte 3     : reserved
//!   bytes 4..8 : stream id (Close only)
//! ```
//! The reply to a `Register` is the 4-byte assigned stream id, or [`DENIED`].

pub const APP_FRAME_LEN: usize = 8;

const TAG_REGISTER: u8 = 1;
const TAG_CLOSE: u8 = 2;

/// Requested-capability bits an app sends with `Register` (§9). choragusd checks
/// these against the app's grant and refuses the ones not held.
pub const CAP_AUDIO: u8 = 1;
pub const CAP_MICROPHONE: u8 = 2;
pub const CAP_MONITOR: u8 = 4;

/// Sentinel stream id in the registration reply meaning "denied".
pub const DENIED: u32 = u32::MAX;

/// The role byte values (the wire convention; `choragus::policy::Role` is the
/// typed view). Apps that don't link the policy crate use these.
pub const ROLE_MEDIA: u8 = 0;
pub const ROLE_COMMUNICATION: u8 = 1;
pub const ROLE_NOTIFICATION: u8 = 2;
pub const ROLE_GAME: u8 = 3;
pub const ROLE_PRO: u8 = 4;

/// Parse a role name to its wire byte.
pub fn role_byte(name: &str) -> Option<u8> {
    Some(match name.to_ascii_lowercase().as_str() {
        "media" => ROLE_MEDIA,
        "comms" | "communication" => ROLE_COMMUNICATION,
        "notification" | "notify" => ROLE_NOTIFICATION,
        "game" => ROLE_GAME,
        "pro" => ROLE_PRO,
        _ => return None,
    })
}

/// Human-readable names of the set capability bits, for logging a denial.
pub fn cap_names(bits: u8) -> Vec<&'static str> {
    let mut v = Vec::new();
    if bits & CAP_AUDIO != 0 { v.push("audio"); }
    if bits & CAP_MICROPHONE != 0 { v.push("microphone"); }
    if bits & CAP_MONITOR != 0 { v.push("audio_monitor"); }
    v
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AppMsg {
    /// "I want to play, as this role byte, requesting these capability bits."
    Register { role: u8, caps: u8 },
    /// "I am done with this stream."
    Close { stream: u32 },
}

impl AppMsg {
    pub fn encode(&self) -> [u8; APP_FRAME_LEN] {
        let mut f = [0u8; APP_FRAME_LEN];
        match *self {
            AppMsg::Register { role, caps } => {
                f[0] = TAG_REGISTER;
                f[1] = role;
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
            TAG_REGISTER => Some(AppMsg::Register { role: b[1], caps: b[2] }),
            TAG_CLOSE => Some(AppMsg::Close { stream: u32::from_le_bytes(b[4..8].try_into().ok()?) }),
            _ => None,
        }
    }
}

/// At connect, an app announces its identity (its manifest app-id) once: a
/// `u16` length-prefixed UTF-8 string. choragusd uses it to look up the grant.
pub fn write_hello<W: std::io::Write>(w: &mut W, app_id: &str) -> std::io::Result<()> {
    let b = app_id.as_bytes();
    let len = b.len().min(255) as u16;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&b[..len as usize])
}

pub fn read_hello<R: std::io::Read>(r: &mut R) -> std::io::Result<String> {
    let mut l = [0u8; 2];
    r.read_exact(&mut l)?;
    let len = u16::from_le_bytes(l) as usize;
    let mut buf = vec![0u8; len.min(256)];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_close_round_trip() {
        for m in [
            AppMsg::Register { role: 1, caps: CAP_AUDIO },
            AppMsg::Register { role: 0, caps: CAP_AUDIO | CAP_MONITOR },
            AppMsg::Close { stream: 9 },
        ] {
            assert_eq!(AppMsg::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn cap_names_lists_set_bits() {
        assert_eq!(cap_names(CAP_AUDIO | CAP_MONITOR), vec!["audio", "audio_monitor"]);
    }
}
