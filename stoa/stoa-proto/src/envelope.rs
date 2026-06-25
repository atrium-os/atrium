//! The Stoa datagram envelope (stoa.md §3.1).
//!
//! ```text
//! ┌──────┬──────┬──────┬──────────────┬──────────┐
//! │ ver  │ type │ seq  │   payload    │ MAC[16]  │
//! │ u8   │ u8   │ u32  │   variable   │  HMAC    │
//! └──────┴──────┴──────┴──────────────┴──────────┘
//! ```
//!
//! - `ver = 1` for v1.
//! - `MAC` is a truncated HMAC-SHA-256 over `ver ‖ type ‖ seq ‖ payload`,
//!   keyed by the per-session `K_sess` (stoa.md §2, derived from the SSH
//!   session id). Truncated to 16 bytes — the standard SSP envelope size.
//! - `seq` is big-endian; monotonic per direction. Drop/reorder detection
//!   and anti-replay live in [`crate::replay`].
//!
//! Decoding verifies the MAC in constant time before returning anything;
//! a bad MAC, short buffer, or unknown version is a hard error and the
//! caller drops the datagram silently (no oracle).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Protocol version carried in the `ver` byte.
pub const VERSION: u8 = 1;

/// Truncated-MAC length in bytes.
pub const MAC_LEN: usize = 16;

/// Fixed header bytes before the payload: ver(1) + type(1) + seq(4).
const HEADER_LEN: usize = 1 + 1 + 4;

/// Smallest possible valid datagram: header + empty payload + MAC.
const MIN_LEN: usize = HEADER_LEN + MAC_LEN;

/// Datagram disposition (stoa.md §3.1 `type`). The *interpretation* of
/// the payload bytes; the typed `Input`/`StateDiff` structures (§3.2/§3.3)
/// are layered on top in later slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    /// Client → server keystrokes / commands (the typed-disposition input).
    Input = 1,
    /// Server → client authoritative grid/cursor diff.
    StateDiff = 2,
    /// Acknowledgement (predictor convergence; §3.4).
    Ack = 3,
    /// Out-of-band control (resync, handshake, etc.).
    Control = 4,
    /// Liveness probe; carries no payload.
    Keepalive = 5,
}

impl MsgType {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => MsgType::Input,
            2 => MsgType::StateDiff,
            3 => MsgType::Ack,
            4 => MsgType::Control,
            5 => MsgType::Keepalive,
            _ => return None,
        })
    }
}

/// A decoded (or to-be-encoded) datagram. `payload` is opaque bytes at
/// this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub msg_type: MsgType,
    pub seq: u32,
    pub payload: Vec<u8>,
}

/// Why a datagram could not be decoded. All variants mean "drop it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    /// Buffer shorter than a minimal valid datagram.
    TooShort,
    /// `ver` byte did not match [`VERSION`].
    BadVersion(u8),
    /// `type` byte was not a known [`MsgType`].
    BadType(u8),
    /// MAC verification failed (tamper, wrong key, or truncation).
    BadMac,
}

impl core::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProtoError::TooShort => write!(f, "datagram too short"),
            ProtoError::BadVersion(v) => write!(f, "bad version {v}"),
            ProtoError::BadType(t) => write!(f, "bad message type {t}"),
            ProtoError::BadMac => write!(f, "MAC verification failed"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl Envelope {
    pub fn new(msg_type: MsgType, seq: u32, payload: Vec<u8>) -> Self {
        Envelope { msg_type, seq, payload }
    }

    /// Serialize + append the MAC, keyed by `key` (the session `K_sess`).
    pub fn encode(&self, key: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.payload.len() + MAC_LEN);
        buf.push(VERSION);
        buf.push(self.msg_type as u8);
        buf.extend_from_slice(&self.seq.to_be_bytes());
        buf.extend_from_slice(&self.payload);

        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
        mac.update(&buf);
        let tag = mac.finalize().into_bytes();
        buf.extend_from_slice(&tag[..MAC_LEN]);
        buf
    }

    /// Verify the MAC and parse. Verification is constant-time (the `hmac`
    /// crate's `verify_truncated_left`); a failure reveals nothing about
    /// where it diverged.
    pub fn decode(key: &[u8], bytes: &[u8]) -> Result<Envelope, ProtoError> {
        if bytes.len() < MIN_LEN {
            return Err(ProtoError::TooShort);
        }
        let (signed, tag) = bytes.split_at(bytes.len() - MAC_LEN);

        if signed[0] != VERSION {
            return Err(ProtoError::BadVersion(signed[0]));
        }
        let msg_type = MsgType::from_u8(signed[1]).ok_or(ProtoError::BadType(signed[1]))?;

        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
        mac.update(signed);
        mac.verify_truncated_left(tag).map_err(|_| ProtoError::BadMac)?;

        let seq = u32::from_be_bytes([signed[2], signed[3], signed[4], signed[5]]);
        let payload = signed[HEADER_LEN..].to_vec();
        Ok(Envelope { msg_type, seq, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-session-key-K_sess";

    #[test]
    fn round_trips() {
        let e = Envelope::new(MsgType::Input, 42, b"hello pty".to_vec());
        let wire = e.encode(KEY);
        let back = Envelope::decode(KEY, &wire).expect("decode");
        assert_eq!(e, back);
    }

    #[test]
    fn empty_payload_round_trips() {
        let e = Envelope::new(MsgType::Keepalive, 0, vec![]);
        let wire = e.encode(KEY);
        assert_eq!(wire.len(), HEADER_LEN + MAC_LEN);
        assert_eq!(Envelope::decode(KEY, &wire).unwrap(), e);
    }

    #[test]
    fn tampered_payload_fails_mac() {
        let e = Envelope::new(MsgType::Input, 7, b"abc".to_vec());
        let mut wire = e.encode(KEY);
        wire[HEADER_LEN] ^= 0x01; // flip a payload bit
        assert_eq!(Envelope::decode(KEY, &wire), Err(ProtoError::BadMac));
    }

    #[test]
    fn tampered_seq_fails_mac() {
        let e = Envelope::new(MsgType::Input, 7, b"abc".to_vec());
        let mut wire = e.encode(KEY);
        wire[2] ^= 0x80; // flip a seq bit (header is MAC'd too)
        assert_eq!(Envelope::decode(KEY, &wire), Err(ProtoError::BadMac));
    }

    #[test]
    fn wrong_key_fails_mac() {
        let e = Envelope::new(MsgType::Input, 7, b"abc".to_vec());
        let wire = e.encode(KEY);
        assert_eq!(Envelope::decode(b"other-key", &wire), Err(ProtoError::BadMac));
    }

    #[test]
    fn short_buffer_rejected() {
        assert_eq!(Envelope::decode(KEY, &[1, 2, 3]), Err(ProtoError::TooShort));
    }

    #[test]
    fn bad_version_rejected() {
        let e = Envelope::new(MsgType::Input, 1, b"x".to_vec());
        let mut wire = e.encode(KEY);
        wire[0] = 9;
        // version is checked before the MAC, so we see BadVersion not BadMac.
        assert_eq!(Envelope::decode(KEY, &wire), Err(ProtoError::BadVersion(9)));
    }

    #[test]
    fn bad_type_rejected() {
        // Hand-build a datagram with type=0 (invalid) and a valid MAC.
        let mut signed = vec![VERSION, 0u8, 0, 0, 0, 1];
        signed.extend_from_slice(b"payload");
        let mut mac = HmacSha256::new_from_slice(KEY).unwrap();
        mac.update(&signed);
        let tag = mac.finalize().into_bytes();
        signed.extend_from_slice(&tag[..MAC_LEN]);
        assert_eq!(Envelope::decode(KEY, &signed), Err(ProtoError::BadType(0)));
    }
}
