//! Object-based spatial audio — amplitude panning to a speaker layout
//! (`docs/spec/atrium-lyra-architecture.md` §12 gap 2, the spatial extension).
//!
//! The object-based (Atmos-shaped) model: instead of authoring fixed channels, a
//! source is an **object at a position**, and Lyra *renders* it to whatever
//! [`ChannelMap`](crate::channels::ChannelMap) the device has — stereo, 5.1, 7.1,
//! or (later) binaural. This is the spatialiser node kind §12 calls for, built on
//! the channel map: each speaker has an azimuth, an object is panned across the
//! two bracketing speakers with **constant-power** gains (the tangent/`cos·sin`
//! law, so total energy is preserved as the object moves and a centred object is
//! −3 dB in each of its two speakers, not doubled).
//!
//! Pure rendering math, host-tested; the daemon wires it as a node whose input is
//! a mono object stream + a position and whose output is the device layout.

use crate::channels::{Channel, ChannelMap};

/// Speaker azimuth in degrees (0 = front, + = right, − = left, ±180 = back).
/// `None` for LFE — objects are not panned to the LFE (bass management is
/// separate).
fn azimuth(c: Channel) -> Option<f32> {
    use Channel::*;
    Some(match c {
        FC => 0.0,
        FL => -30.0,
        FR => 30.0,
        SL => -110.0,
        SR => 110.0,
        RL => -150.0,
        RR => 150.0,
        LFE => return None,
    })
}

fn wrap180(mut a: f32) -> f32 {
    while a > 180.0 {
        a -= 360.0;
    }
    while a <= -180.0 {
        a += 360.0;
    }
    a
}

/// Per-speaker gains for a mono object at azimuth `az` (degrees), rendered to
/// `layout`. Constant-power panned across the two speakers bracketing `az`;
/// every other speaker (and LFE) is zero. `Σ gain² = 1`.
pub fn pan(az: f32, layout: &ChannelMap) -> Vec<f32> {
    let az = wrap180(az);
    // (output index, azimuth) for the real speakers, sorted around the circle.
    let mut spk: Vec<(usize, f32)> = layout
        .0
        .iter()
        .enumerate()
        .filter_map(|(i, &c)| azimuth(c).map(|a| (i, a)))
        .collect();
    spk.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let mut g = vec![0.0f32; layout.count()];
    if spk.is_empty() {
        return g;
    }
    if spk.len() == 1 {
        g[spk[0].0] = 1.0;
        return g;
    }

    // find the arc [a_lo, a_hi] containing az; the last→first arc wraps +360.
    let n = spk.len();
    let (mut lo, mut hi) = (n - 1, 0usize); // default: the wrap arc
    for i in 0..n - 1 {
        if az >= spk[i].1 && az <= spk[i + 1].1 {
            lo = i;
            hi = i + 1;
            break;
        }
    }
    let (i_lo, a_lo) = spk[lo];
    let (i_hi, mut a_hi) = spk[hi];
    let mut azp = az;
    if lo == n - 1 && hi == 0 {
        // wrap arc through the back: unwrap so a_lo < azp < a_hi.
        a_hi += 360.0;
        if azp < a_lo {
            azp += 360.0;
        }
    }
    // pan position in [0,1] across the arc, then constant-power gains.
    let span = a_hi - a_lo;
    let p = if span.abs() < 1e-6 { 0.0 } else { (azp - a_lo) / span };
    let p = p.clamp(0.0, 1.0);
    let theta = p * std::f32::consts::FRAC_PI_2;
    g[i_lo] = theta.cos();
    g[i_hi] = theta.sin();
    g
}

/// Render a mono object buffer to the layout: each output frame is the object
/// sample scaled by the per-speaker pan gains.
pub fn render(object: &[f32], az: f32, layout: &ChannelMap) -> Vec<f32> {
    let g = pan(az, layout);
    let co = layout.count();
    let mut out = vec![0.0f32; object.len() * co];
    for (f, &s) in object.iter().enumerate() {
        for o in 0..co {
            out[f * co + o] = s * g[o];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn power(g: &[f32]) -> f32 {
        g.iter().map(|x| x * x).sum()
    }

    #[test]
    fn object_at_a_speaker_goes_fully_to_it() {
        // stereo: FL=-30, FR=+30. Object at -30 -> all FL.
        let g = pan(-30.0, &ChannelMap::stereo());
        assert!((g[0] - 1.0).abs() < 1e-5 && g[1].abs() < 1e-5, "{g:?}");
    }

    #[test]
    fn centred_object_is_minus_3db_in_both() {
        // object at 0 between FL(-30) and FR(+30): equal constant-power gains.
        let g = pan(0.0, &ChannelMap::stereo());
        let k = std::f32::consts::FRAC_1_SQRT_2;
        assert!((g[0] - k).abs() < 1e-5 && (g[1] - k).abs() < 1e-5, "{g:?}");
    }

    #[test]
    fn panning_is_constant_power_everywhere() {
        let s = ChannelMap::surround_5_1();
        for deg in (-180..180).step_by(7) {
            let p = power(&pan(deg as f32, &s));
            assert!((p - 1.0).abs() < 1e-4, "az {deg}: power {p}");
        }
    }

    #[test]
    fn five_one_renders_centre_object_to_fc_only() {
        // 5.1 has a real centre speaker at 0 -> an object at 0 is entirely FC.
        let s = ChannelMap::surround_5_1();
        let g = pan(0.0, &s);
        // map order FL,FR,FC,LFE,SL,SR -> FC is index 2.
        assert!((g[2] - 1.0).abs() < 1e-5, "centre to FC: {g:?}");
        assert!(g.iter().enumerate().all(|(i, &x)| i == 2 || x.abs() < 1e-5));
        assert!(g[3].abs() < 1e-9, "never the LFE");
    }

    #[test]
    fn object_sweeps_smoothly_between_two_speakers() {
        // between FL(-30) and FC(0) the FL gain falls and FC rises monotonically.
        let s = ChannelMap::surround_5_1();
        let (mut prev_fl, mut prev_fc) = (2.0f32, -1.0f32);
        for deg in (-30..=0).step_by(5) {
            let g = pan(deg as f32, &s);
            assert!(g[0] <= prev_fl + 1e-6, "FL monotone down");
            assert!(g[2] >= prev_fc - 1e-6, "FC monotone up");
            prev_fl = g[0];
            prev_fc = g[2];
        }
    }

    #[test]
    fn render_scales_the_object_by_the_gains() {
        let s = ChannelMap::stereo();
        let out = render(&[1.0, 1.0], 0.0, &s);
        let k = std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - k).abs() < 1e-5 && (out[1] - k).abs() < 1e-5);
        assert_eq!(out.len(), 4); // 2 frames × stereo
    }

    #[test]
    fn rear_object_pans_across_the_wrap_arc() {
        // object directly behind (180) with 7.1: between RL(-150) and RR(150),
        // the wrap arc through the back. Constant power, both rears active.
        let g = pan(180.0, &ChannelMap::surround_7_1());
        assert!((power(&g) - 1.0).abs() < 1e-4);
        // RL idx 6, RR idx 7 in FL,FR,FC,LFE,SL,SR,RL,RR.
        assert!(g[6] > 0.1 && g[7] > 0.1, "both rears carry it: {g:?}");
    }
}
