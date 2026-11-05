//! The SDR desktop on an HDR monitor (display ABI 2.3).
//!
//! The renderers draw sRGB, 8 bits a channel. With `FRESCOD_HDR=1` and a
//! monitor that takes HDR10 (PQ in BT.2020), frescod switches the stream to
//! HDR10 and re-encodes each frame on its way to scanout: sRGB decoded to
//! linear light, BT.709 primaries converted to BT.2020, SDR reference white
//! placed at 203 cd/m² (ITU-R BT.2408), then PQ-encoded (SMPTE ST 2084) into
//! a 10-bit XRGB2101010 word. The desktop looks the same as in SDR, at a
//! defined brightness, and HDR content can later be composed above white.
//!
//! The work per pixel is two table lookups a channel and a 3x3 integer
//! matrix. The gpu renderer draws straight into scanout, so it stays SDR.

/// SDR reference white, cd/m² (BT.2408).
pub const SDR_WHITE_NITS: f64 = 203.0;

/// Linear light carried in 16 bits (0..=65535 = 0..1 of SDR white).
const LIN_ONE: u32 = 65535;
/// Entries of the PQ encode table over 0..=SDR white.
const PQ_STEPS: usize = 4096;

/// BT.709 -> BT.2020 primaries (ITU-R BT.2087), Q14.
const M709_2020: [[i32; 3]; 3] = [
    [10279, 5395, 710],
    [1132, 15065, 187],
    [269, 1442, 14673],
];

pub struct Encoder {
    srgb_to_lin: [u16; 256],
    lin_to_pq: Vec<u16>,
}

impl Encoder {
    pub fn new() -> Self {
        let mut srgb_to_lin = [0u16; 256];
        for (i, v) in srgb_to_lin.iter_mut().enumerate() {
            *v = (srgb_decode(i as f64 / 255.0) * f64::from(LIN_ONE)).round() as u16;
        }
        let lin_to_pq = (0..=PQ_STEPS)
            .map(|k| {
                let nits = k as f64 / PQ_STEPS as f64 * SDR_WHITE_NITS;
                (pq_encode(nits) * 1023.0).round() as u16
            })
            .collect();
        Encoder { srgb_to_lin, lin_to_pq }
    }

    /// One B,G,R,X sRGB pixel as an XRGB2101010 HDR10 word.
    pub fn pixel(&self, b: u8, g: u8, r: u8) -> u32 {
        let lin = [self.srgb_to_lin[r as usize], self.srgb_to_lin[g as usize], self.srgb_to_lin[b as usize]];
        let mut out = [0u32; 3];
        for (o, row) in out.iter_mut().zip(&M709_2020) {
            let v: i64 = row.iter().zip(&lin).map(|(&m, &l)| i64::from(m) * i64::from(l)).sum();
            let v = ((v + (1 << 13)) >> 14).clamp(0, i64::from(LIN_ONE)) as u32;
            // Interpolate the PQ table: v in 0..=65535 over 4096 steps.
            let pos = v * PQ_STEPS as u32;
            let (i, f) = ((pos / LIN_ONE) as usize, pos % LIN_ONE);
            let a = u32::from(self.lin_to_pq[i]);
            let b = u32::from(self.lin_to_pq[(i + 1).min(PQ_STEPS)]);
            *o = (a * (LIN_ONE - f) + b * f + LIN_ONE / 2) / LIN_ONE;
        }
        0xc000_0000 | out[0] << 20 | out[1] << 10 | out[2]
    }

    /// Re-encode a B,G,R,X sRGB frame in place as XRGB2101010 HDR10.
    pub fn encode(&self, frame: &mut [u8]) {
        for px in frame.chunks_exact_mut(4) {
            let w = self.pixel(px[0], px[1], px[2]);
            px.copy_from_slice(&w.to_le_bytes());
        }
    }
}

fn srgb_decode(v: f64) -> f64 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// SMPTE ST 2084 inverse EOTF: absolute luminance (cd/m²) to a 0..1 signal.
pub fn pq_encode(nits: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(w: u32) -> (u32, u32, u32) { ((w >> 20) & 0x3ff, (w >> 10) & 0x3ff, w & 0x3ff) }

    #[test]
    fn pq_matches_the_standard_reference_points() {
        assert!((pq_encode(10000.0) - 1.0).abs() < 1e-9);
        assert!(pq_encode(0.0) < 1e-5);
        // 100 cd/m² is code ~0.508, 203 ~0.58 (BT.2408 table).
        assert!((pq_encode(100.0) - 0.5081).abs() < 0.001, "{}", pq_encode(100.0));
        assert!((pq_encode(203.0) - 0.5807).abs() < 0.001, "{}", pq_encode(203.0));
    }

    #[test]
    fn sdr_white_lands_at_203_nits_and_black_at_zero() {
        let e = Encoder::new();
        let white = (pq_encode(SDR_WHITE_NITS) * 1023.0).round() as u32;
        assert_eq!(channels(e.pixel(255, 255, 255)), (white, white, white));
        assert_eq!(channels(e.pixel(0, 0, 0)).0, (pq_encode(0.0) * 1023.0).round() as u32);
    }

    #[test]
    fn grey_stays_grey_and_primaries_move_inside_bt2020() {
        let e = Encoder::new();
        let (r, g, b) = channels(e.pixel(128, 128, 128));
        assert!(r == g && g == b, "{r} {g} {b}");
        // sRGB red is inside BT.2020: mostly R, a little G and B.
        let (r, g, b) = channels(e.pixel(0, 0, 255));
        assert!(r > g && g > b && b > 0, "{r} {g} {b}");
    }

    #[test]
    fn encode_writes_words_in_place() {
        let e = Encoder::new();
        let mut f = vec![255u8, 255, 255, 255, 0, 0, 0, 255];
        e.encode(&mut f);
        assert_eq!(u32::from_le_bytes([f[0], f[1], f[2], f[3]]), e.pixel(255, 255, 255));
        assert_eq!(u32::from_le_bytes([f[4], f[5], f[6], f[7]]), e.pixel(0, 0, 0));
    }
}
