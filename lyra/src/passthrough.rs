//! Passthrough / untouchable formats — DSD and encoded bitstream
//! (`docs/spec/atrium-lyra-architecture.md` §12 gap 1).
//!
//! Some formats cannot be mixed, resampled, or volume-scaled without defeating
//! their purpose, so they bypass the graph's DSP entirely and travel
//! **bit-exactly** from one source to one device:
//!
//! - **DSD** (SACD: 1-bit sigma-delta at 2.8224 MHz+) — mixing means PCM-
//!   converting and re-modulating, which no one wants.
//! - **Encoded bitstream** (AC3/DTS/Dolby over HDMI/SPDIF) — the receiver
//!   decodes it; the host must not touch it.
//!
//! A passthrough stream is therefore a **degenerate graph — source → sink,
//! exclusive, bit-perfect, no mix/DSP node** — which the §4.3 sole-ownership rule
//! already serves: lyrad grants the device to one passthrough client and denies
//! concurrent streams for the duration.
//!
//! **Transport.** DSD rides the *existing* bit-perfect PCM path via **DoP**
//! (DSD-over-PCM): 16 DSD bits packed into the low 16 bits of a 24-bit PCM
//! sample, with an 8-bit marker (0x05 / 0xFA, alternating per sample) the DAC
//! uses to recognise DoP. So DSD64 (2.8224 MHz) becomes ordinary 24-bit / 176.4
//! kHz PCM — no driver change. Encoded bitstream rides the same idea (IEC 61937
//! frames over PCM). Native DSD (no DoP wrapper) needs a driver format — a path-B
//! item (§4.4).

/// DSD bits carried per DoP PCM sample (the low 16 bits of a 24-bit word).
const DOP_DSD_BITS_PER_SAMPLE: u32 = 16;
/// DoP markers, alternating on consecutive samples.
const DOP_MARKER_A: u8 = 0x05;
const DOP_MARKER_B: u8 = 0xFA;

/// A stream's wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Ordinary PCM — mixable, resamplable; the normal graph.
    Pcm { bits: u8, rate_hz: u32 },
    /// DSD carried over PCM (DoP) — rides the bit-perfect PCM path untouched.
    DsdDop { dsd_rate_hz: u32 },
    /// Native DSD — needs a driver DSD format (path B), not the PCM transport.
    DsdNative { dsd_rate_hz: u32 },
    /// Encoded bitstream (AC3/DTS) in IEC 61937 frames over a PCM transport.
    Bitstream { transport_rate_hz: u32 },
}

impl Format {
    /// Untouchable: bypasses all mix/resample/volume.
    pub fn is_passthrough(&self) -> bool {
        !matches!(self, Format::Pcm { .. })
    }

    /// A passthrough stream must own the device exclusively (§4.3 / §12).
    pub fn requires_exclusive(&self) -> bool {
        self.is_passthrough()
    }

    /// Does this format travel over the ordinary bit-perfect PCM device path
    /// (so any existing snd_* driver carries it with no change)? True for PCM,
    /// DoP, and IEC-61937 bitstream; false for native DSD (needs a driver fmt).
    pub fn rides_bit_perfect_pcm(&self) -> bool {
        !matches!(self, Format::DsdNative { .. })
    }

    /// The PCM frame rate the device is clocked at to carry this format.
    pub fn transport_rate_hz(&self) -> u32 {
        match *self {
            Format::Pcm { rate_hz, .. } => rate_hz,
            // DoP packs DOP_DSD_BITS_PER_SAMPLE DSD bits per PCM sample.
            Format::DsdDop { dsd_rate_hz } => dsd_rate_hz / DOP_DSD_BITS_PER_SAMPLE,
            Format::DsdNative { dsd_rate_hz } => dsd_rate_hz,
            Format::Bitstream { transport_rate_hz } => transport_rate_hz,
        }
    }
}

/// Pack a mono DSD bit-stream (`dsd[i]` = 8 DSD bits, MSB first) into DoP PCM
/// samples (each a 24-bit value in the low bits of an `i32`): top byte = the
/// alternating marker, low 16 bits = the next 16 DSD bits. The result rides a
/// 24-bit PCM transport unchanged.
pub fn dop_pack(dsd: &[u8]) -> Vec<i32> {
    let mut out = Vec::with_capacity(dsd.len().div_ceil(2));
    for (i, chunk) in dsd.chunks(2).enumerate() {
        let marker = if i % 2 == 0 { DOP_MARKER_A } else { DOP_MARKER_B } as i32;
        let b0 = chunk[0] as i32;
        let b1 = *chunk.get(1).unwrap_or(&0) as i32;
        out.push((marker << 16) | (b0 << 8) | b1);
    }
    out
}

/// Recover the DSD bytes from DoP PCM (what the DAC does), verifying the marker
/// alternation. Returns `None` if a marker is wrong — i.e. this is ordinary PCM,
/// not DoP, so the device must *not* interpret it as DSD.
pub fn dop_unpack(dop: &[i32]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(dop.len() * 2);
    for (i, &s) in dop.iter().enumerate() {
        let marker = ((s >> 16) & 0xFF) as u8;
        let want = if i % 2 == 0 { DOP_MARKER_A } else { DOP_MARKER_B };
        if marker != want {
            return None; // not a DoP stream (or out of phase) — treat as PCM
        }
        out.push(((s >> 8) & 0xFF) as u8);
        out.push((s & 0xFF) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSD64: u32 = 2_822_400; // 44.1k × 64

    #[test]
    fn dsd64_dop_is_24bit_176k4_pcm() {
        let f = Format::DsdDop { dsd_rate_hz: DSD64 };
        assert_eq!(f.transport_rate_hz(), 176_400, "DSD64 over DoP = 176.4 kHz");
        assert!(f.is_passthrough() && f.requires_exclusive());
        assert!(f.rides_bit_perfect_pcm(), "DoP needs no driver change");
    }

    #[test]
    fn native_dsd_does_not_ride_pcm() {
        let f = Format::DsdNative { dsd_rate_hz: DSD64 };
        assert!(!f.rides_bit_perfect_pcm(), "native DSD needs a driver format");
        assert_eq!(f.transport_rate_hz(), DSD64);
    }

    #[test]
    fn pcm_is_neither_passthrough_nor_exclusive() {
        let f = Format::Pcm { bits: 16, rate_hz: 48_000 };
        assert!(!f.is_passthrough());
        assert!(!f.requires_exclusive());
        assert!(f.rides_bit_perfect_pcm());
    }

    #[test]
    fn bitstream_is_exclusive_passthrough_over_pcm() {
        let f = Format::Bitstream { transport_rate_hz: 48_000 };
        assert!(f.is_passthrough() && f.requires_exclusive());
        assert!(f.rides_bit_perfect_pcm(), "IEC 61937 rides the PCM transport");
    }

    #[test]
    fn dop_markers_alternate() {
        let dsd = vec![0xAA, 0xBB, 0xCC, 0xDD]; // 2 DoP samples
        let dop = dop_pack(&dsd);
        assert_eq!(dop.len(), 2);
        assert_eq!((dop[0] >> 16) & 0xFF, 0x05);
        assert_eq!((dop[1] >> 16) & 0xFF, 0xFA);
        assert_eq!(dop[0] & 0xFFFF, 0xAABB); // 16 DSD bits in the low word
    }

    #[test]
    fn dop_round_trips_bit_exact() {
        // the property a DAC depends on: DoP carries DSD untouched.
        let dsd: Vec<u8> = (0..256).map(|i| (i * 37 + 11) as u8).collect();
        let dop = dop_pack(&dsd);
        let back = dop_unpack(&dop).expect("valid DoP");
        assert_eq!(back, dsd, "DSD recovered bit-exactly through DoP");
    }

    #[test]
    fn ordinary_pcm_is_not_mistaken_for_dop() {
        // a PCM buffer without the marker pattern must not be read as DSD.
        let pcm = vec![0x1234, 0x5678, 0x0001];
        assert_eq!(dop_unpack(&pcm), None, "no DoP marker -> not DSD");
    }
}
