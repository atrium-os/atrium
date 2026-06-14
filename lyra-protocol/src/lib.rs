//! Lyra audio control protocol — Aqueduct **class 5** (`audio-control`),
//! `docs/spec/atrium-lyra-architecture.md` §5.1.
//!
//! The wire dictionary **choragusd** (the policy layer) speaks to **lyrad** (the
//! RT engine): the mechanism-agnostic changes the policy resolves — a stream's
//! level, a stream's sink — that the engine then realises (a zipper-free gain
//! ramp; a glitch-free re-route). Deliberately tiny and fixed-width: this is a
//! control edge, not a data edge (audio samples ride the shmem rings, never
//! this socket).
//!
//! Both daemon crates depend on this one, so neither depends on the other — the
//! mechanism/policy split holds at the crate graph too.
//!
//! Frame: a fixed **12 bytes**, little-endian.
//! ```text
//!   byte 0      : tag (1 = SetGainDb, 2 = Reroute)
//!   bytes 1..4  : reserved (0)
//!   bytes 4..8  : stream id        (u32)
//!   bytes 8..12 : payload          (f32 dB bits for SetGainDb; u32 sink for Reroute)
//! ```

pub const FRAME_LEN: usize = 12;

const TAG_SET_GAIN: u8 = 1;
const TAG_REROUTE: u8 = 2;
const TAG_OPEN: u8 = 3;
const TAG_CLOSE: u8 = 4;

/// A control message from the session layer to the engine — a stream's lifecycle
/// (open / close) or a level / routing change.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ctl {
    /// Instantiate a mixer slot for `stream` (its audio source is its ring; the
    /// demo engine synthesises a tone keyed by id). Opens at unity gain.
    OpenStream { stream: u32 },
    /// Tear down `stream`'s mixer slot.
    CloseStream { stream: u32 },
    /// Ramp `stream`'s output gain to `db` (the zipper-free smoother applies it).
    SetGainDb { stream: u32, db: f32 },
    /// Move `stream` to `sink` (the glitch-free atomic-commit reconfig applies it).
    Reroute { stream: u32, sink: u32 },
}

impl Ctl {
    pub fn stream(&self) -> u32 {
        match *self {
            Ctl::OpenStream { stream }
            | Ctl::CloseStream { stream }
            | Ctl::SetGainDb { stream, .. }
            | Ctl::Reroute { stream, .. } => stream,
        }
    }

    /// Encode to the fixed 12-byte frame.
    pub fn encode(&self) -> [u8; FRAME_LEN] {
        let mut f = [0u8; FRAME_LEN];
        let (tag, stream, payload) = match *self {
            Ctl::OpenStream { stream } => (TAG_OPEN, stream, 0),
            Ctl::CloseStream { stream } => (TAG_CLOSE, stream, 0),
            Ctl::SetGainDb { stream, db } => (TAG_SET_GAIN, stream, db.to_bits()),
            Ctl::Reroute { stream, sink } => (TAG_REROUTE, stream, sink),
        };
        f[0] = tag;
        f[4..8].copy_from_slice(&stream.to_le_bytes());
        f[8..12].copy_from_slice(&payload.to_le_bytes());
        f
    }

    /// Decode one frame; `None` on a short buffer or an unknown tag.
    pub fn decode(b: &[u8]) -> Option<Ctl> {
        if b.len() < FRAME_LEN {
            return None;
        }
        let stream = u32::from_le_bytes(b[4..8].try_into().ok()?);
        let payload = u32::from_le_bytes(b[8..12].try_into().ok()?);
        match b[0] {
            TAG_OPEN => Some(Ctl::OpenStream { stream }),
            TAG_CLOSE => Some(Ctl::CloseStream { stream }),
            TAG_SET_GAIN => Some(Ctl::SetGainDb { stream, db: f32::from_bits(payload) }),
            TAG_REROUTE => Some(Ctl::Reroute { stream, sink: payload }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_gain_round_trips() {
        let c = Ctl::SetGainDb { stream: 7, db: -18.0 };
        assert_eq!(Ctl::decode(&c.encode()), Some(c));
    }

    #[test]
    fn reroute_round_trips() {
        let c = Ctl::Reroute { stream: 3, sink: 1 };
        assert_eq!(Ctl::decode(&c.encode()), Some(c));
    }

    #[test]
    fn frame_is_fixed_width() {
        assert_eq!(Ctl::SetGainDb { stream: 0, db: 0.0 }.encode().len(), FRAME_LEN);
    }

    #[test]
    fn short_or_bad_frames_reject() {
        assert_eq!(Ctl::decode(&[0u8; 4]), None, "short frame");
        let mut f = Ctl::Reroute { stream: 1, sink: 2 }.encode();
        f[0] = 99; // unknown tag
        assert_eq!(Ctl::decode(&f), None, "unknown tag");
    }

    #[test]
    fn negative_and_zero_gain_survive() {
        for db in [0.0f32, -6.0, -18.0, -60.0, 3.5] {
            let c = Ctl::SetGainDb { stream: 0, db };
            assert_eq!(Ctl::decode(&c.encode()), Some(c));
        }
    }

    #[test]
    fn open_and_close_round_trip() {
        for c in [Ctl::OpenStream { stream: 2 }, Ctl::CloseStream { stream: 5 }] {
            assert_eq!(Ctl::decode(&c.encode()), Some(c));
        }
    }
}
