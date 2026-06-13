//! Channel layouts and channel-aware matrix conversion
//! (`docs/spec/atrium-lyra-architecture.md` §12 gap 2).
//!
//! Lyra subsumes the kernel's `feeder_matrix.c`: a stream declares *which*
//! channel is which (FL/FR/FC/LFE/…), and conversion between layouts is a matrix,
//! not a blind interleave. This is the foundation surround and (later) spatial
//! rendering build on — without it, "stereo float frames" cannot express 5.1, a
//! downmix folds the centre into silence, and an upmix smears mono across the
//! room.
//!
//! The conversion rule keeps it correct and predictable:
//! - a channel present in *both* layouts passes through at unity;
//! - a channel present in the **input but not the output** is *folded* into the
//!   appropriate output channel(s) at −3 dB (the ITU-R BS.775 surround downmix);
//! - a channel present in the **output but not the input** is **silent** — upmix
//!   routes, it never synthesises (no phantom centre, no fake surround).

/// Speaker positions, in the canonical interleave order layouts use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    FL,  // front left
    FR,  // front right
    FC,  // front centre (also carries mono)
    LFE, // low-frequency effects
    SL,  // surround left
    SR,  // surround right
    RL,  // rear left (7.1)
    RR,  // rear right (7.1)
}

/// An ordered channel layout. The order is the interleave order on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelMap(pub Vec<Channel>);

impl ChannelMap {
    pub fn mono() -> Self {
        ChannelMap(vec![Channel::FC])
    }
    pub fn stereo() -> Self {
        ChannelMap(vec![Channel::FL, Channel::FR])
    }
    pub fn surround_5_1() -> Self {
        use Channel::*;
        ChannelMap(vec![FL, FR, FC, LFE, SL, SR])
    }
    pub fn surround_7_1() -> Self {
        use Channel::*;
        ChannelMap(vec![FL, FR, FC, LFE, SL, SR, RL, RR])
    }
    pub fn count(&self) -> usize {
        self.0.len()
    }
    fn contains(&self, c: Channel) -> bool {
        self.0.contains(&c)
    }
}

const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2; // 0.7071 = −3 dB

/// How an input channel `inp` (absent from the output layout) folds into output
/// channel `out` — the BS.775 surround downmix. 0 if it does not fold there.
fn fold(out: Channel, inp: Channel) -> f32 {
    use Channel::*;
    match (out, inp) {
        // centre splits equally to front L/R (and front L/R fold to centre).
        (FL, FC) | (FR, FC) | (FC, FL) | (FC, FR) => M3DB,
        // surrounds and rears fold to the same-side front.
        (FL, SL) | (FL, RL) | (FR, SR) | (FR, RR) => M3DB,
        // LFE is dropped on downmix (no full-range destination).
        _ => 0.0,
    }
}

/// The conversion matrix `m[out][in]`: output channel `o` is
/// `Σ_i m[o][i] · input[i]`.
pub fn matrix(from: &ChannelMap, to: &ChannelMap) -> Vec<Vec<f32>> {
    let mut m = vec![vec![0.0f32; from.count()]; to.count()];
    for (oi, &out) in to.0.iter().enumerate() {
        for (ii, &inp) in from.0.iter().enumerate() {
            m[oi][ii] = if out == inp {
                1.0 // present in both — pass through
            } else if !to.contains(inp) {
                fold(out, inp) // input channel has no slot in output — fold it
            } else {
                0.0 // input has its own output slot; do not also fold here
            };
        }
    }
    m
}

/// Convert interleaved frames from `from` to `to` using [`matrix`].
pub fn convert(input: &[f32], from: &ChannelMap, to: &ChannelMap) -> Vec<f32> {
    let (ci, co) = (from.count(), to.count());
    let frames = input.len() / ci;
    let m = matrix(from, to);
    let mut out = vec![0.0f32; frames * co];
    for f in 0..frames {
        for o in 0..co {
            let mut acc = 0.0f32;
            for i in 0..ci {
                acc += m[o][i] * input[f * ci + i];
            }
            out[f * co + o] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passes_through() {
        let s = ChannelMap::stereo();
        let inb = [0.1, -0.2, 0.3, -0.4];
        assert_eq!(convert(&inb, &s, &s), inb);
    }

    #[test]
    fn five_one_downmix_to_stereo_folds_centre_and_surround() {
        // 5.1 frame [FL,FR,FC,LFE,SL,SR]; standard BS.775 downmix:
        //   Lo = FL + .707*FC + .707*SL ; Ro = FR + .707*FC + .707*SR ; LFE dropped.
        let f = ChannelMap::surround_5_1();
        let t = ChannelMap::stereo();
        let frame = [1.0, 2.0, 4.0, 9.0, 8.0, 16.0]; // FL FR FC LFE SL SR
        let out = convert(&frame, &f, &t);
        let lo = 1.0 + M3DB * 4.0 + M3DB * 8.0;
        let ro = 2.0 + M3DB * 4.0 + M3DB * 16.0;
        assert!((out[0] - lo).abs() < 1e-5, "Lo {} vs {lo}", out[0]);
        assert!((out[1] - ro).abs() < 1e-5, "Ro {} vs {ro}", out[1]);
    }

    #[test]
    fn stereo_upmix_to_5_1_routes_and_silences() {
        // upmix never synthesises: FL/FR pass; FC/LFE/SL/SR are silent.
        let f = ChannelMap::stereo();
        let t = ChannelMap::surround_5_1();
        let out = convert(&[3.0, 5.0], &f, &t); // FL FR
        assert_eq!(out, vec![3.0, 5.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn mono_spreads_to_both_fronts_at_minus_3db() {
        // mono is centre; centre folds to FL and FR equally.
        let out = convert(&[1.0], &ChannelMap::mono(), &ChannelMap::stereo());
        assert!((out[0] - M3DB).abs() < 1e-6);
        assert!((out[1] - M3DB).abs() < 1e-6);
    }

    #[test]
    fn stereo_downmix_to_mono_folds_both_to_centre() {
        let out = convert(&[1.0, 3.0], &ChannelMap::stereo(), &ChannelMap::mono());
        assert!((out[0] - (M3DB * 1.0 + M3DB * 3.0)).abs() < 1e-5);
    }

    #[test]
    fn seven_one_rears_fold_into_the_surrounds_then_fronts() {
        // 7.1 -> 5.1: RL/RR have no slot, fold to FL/FR (their same-side front).
        let f = ChannelMap::surround_7_1();
        let t = ChannelMap::surround_5_1();
        // FL FR FC LFE SL SR RL RR
        let frame = [1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 10.0, 20.0];
        let out = convert(&frame, &f, &t);
        // SL/SR present in both -> pass (0 here); RL folds to FL, RR to FR.
        assert!((out[0] - (1.0 + M3DB * 10.0)).abs() < 1e-5, "FL got RL fold");
        assert!((out[1] - (2.0 + M3DB * 20.0)).abs() < 1e-5, "FR got RR fold");
    }

    #[test]
    fn lfe_is_dropped_on_downmix() {
        let f = ChannelMap::surround_5_1();
        let t = ChannelMap::stereo();
        // only LFE non-zero -> output silent (LFE has no full-range destination).
        let frame = [0.0, 0.0, 0.0, 7.0, 0.0, 0.0];
        let out = convert(&frame, &f, &t);
        assert_eq!(out, vec![0.0, 0.0]);
    }
}
