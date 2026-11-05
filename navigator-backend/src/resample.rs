//! Downsampling a decoded image to the size it is painted at (threat model
//! §2.4, "decode to the painted size").
//!
//! ★ DETERMINISTIC BY CONSTRUCTION. Two machines must produce the same
//! pixels from the same bytes (a scene is content-addressed and compared
//! across runs), so the arithmetic is integer: each output pixel's weights
//! are computed once, quantised to 1/2^14 and adjusted so they sum to
//! exactly 2^14, and the passes accumulate in `i64`. The kernels used in
//! production are polynomials (box, triangle, cubic) evaluated in `f64`,
//! which IEEE 754 fixes bit for bit; no transcendental is called, so a
//! platform's libm cannot move a pixel. Lanczos (which needs `sin`) is here
//! only so the filter choice could be measured against it.
//!
//! ★ PREMULTIPLIED WHILE FILTERING. The decoder's output is straight alpha
//! (image.rs); averaging straight colour across a transparent edge drags the
//! colour of invisible pixels into visible ones (the dark fringe around a
//! downscaled logo). So the channels are filtered as `colour × alpha` and
//! `alpha × 255` — one 16-bit scale for all four — and divided back at the
//! end. The output is straight alpha again, like every decode.
//!
//! ★ NEVER UPSCALES. `fit` returns the natural size whenever the painted
//! size is not smaller; a resample is a reduction or it does not happen.
//!
//! Memory: one horizontally-filtered row per source row inside the current
//! vertical window (a ring of at most `support` rows), plus the output. The
//! source frame is the decoder's and is dropped by the caller after this.

/// Resampling filters. `Default` is the one decodes use (see
/// `Filter::DEFAULT` for why).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Exact area average: each output pixel is the mean of the source area
    /// it covers, with fractional edge coverage. No ringing, no overshoot.
    Area,
    /// Triangle (tent) scaled to the reduction: bilinear generalised.
    Triangle,
    /// Cubic B=0, C=0.5: sharp, some overshoot.
    CatmullRom,
    /// Cubic B=C=1/3 (Mitchell–Netravali).
    Mitchell,
    /// Lanczos, a = 3. Calls `sin`: for measurement only, not deterministic
    /// across libm implementations.
    Lanczos3,
}

impl Filter {
    /// ★ The filter decodes use: the exact AREA average. Chosen by
    /// measurement (threat model §2.4, `examples/resample_compare.rs`): 444
    /// painted reductions from both corpora, each filter against headless
    /// Chrome drawing the same decoded frame at the same size (harness floor
    /// measured at 0.000). Area is closest to Chrome on 305 of 444 images,
    /// mean 2.46 levels against 2.88 (Catmull-Rom), 3.17 (Mitchell), 3.22
    /// (triangle) and 3.41 (Lanczos 3), and best below 4x; at 4x and over
    /// the five are within 0.1 except Lanczos. `sips` is sharper than both
    /// (Catmull-Rom is closest to it, and sips is 4.47 from Chrome), so the
    /// reference a reader compares against is the browser. Area also has
    /// no overshoot to clamp, and its weights are exact integers.
    pub const DEFAULT: Filter = Filter::Area;

    pub fn name(self) -> &'static str {
        match self {
            Filter::Area => "area", Filter::Triangle => "triangle", Filter::CatmullRom => "catmull-rom",
            Filter::Mitchell => "mitchell", Filter::Lanczos3 => "lanczos3",
        }
    }
    pub const ALL: [Filter; 5] = [Filter::Area, Filter::Triangle, Filter::CatmullRom, Filter::Mitchell, Filter::Lanczos3];

    fn support(self) -> f64 {
        match self { Filter::Area => 0.5, Filter::Triangle => 1.0, Filter::CatmullRom | Filter::Mitchell => 2.0, Filter::Lanczos3 => 3.0 }
    }

    fn eval(self, x: f64) -> f64 {
        let x = x.abs();
        let cubic = |b: f64, c: f64| -> f64 {
            if x < 1.0 {
                ((12.0 - 9.0 * b - 6.0 * c) * x * x * x + (-18.0 + 12.0 * b + 6.0 * c) * x * x + (6.0 - 2.0 * b)) / 6.0
            } else if x < 2.0 {
                ((-b - 6.0 * c) * x * x * x + (6.0 * b + 30.0 * c) * x * x + (-12.0 * b - 48.0 * c) * x + (8.0 * b + 24.0 * c)) / 6.0
            } else { 0.0 }
        };
        match self {
            Filter::Area => if x < 0.5 { 1.0 } else { 0.0 },
            Filter::Triangle => (1.0 - x).max(0.0),
            Filter::CatmullRom => cubic(0.0, 0.5),
            Filter::Mitchell => cubic(1.0 / 3.0, 1.0 / 3.0),
            Filter::Lanczos3 => {
                if x == 0.0 { 1.0 } else if x >= 3.0 { 0.0 } else {
                    let p = std::f64::consts::PI * x;
                    3.0 * p.sin() * (p / 3.0).sin() / (p * p)
                }
            }
        }
    }
}

/// The size to decode an image to so it is painted at `target` (device
/// pixels) without a loss: ONE scale for both axes, the larger of the two
/// the box asks for, so neither axis is under-sampled whatever `object-fit`
/// or `background-size` does with the aspect. Never above the natural size;
/// never below one pixel. `target` of 0 on an axis asks nothing of it.
pub fn fit(natural: (u32, u32), target: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = (natural.0 as u64, natural.1 as u64);
    let (tw, th) = (target.0 as u64, target.1 as u64);
    if nw == 0 || nh == 0 { return natural }
    // Compare tw/nw with th/nh without division: the axis whose ratio is
    // larger sets the scale.
    let (num, den) = if tw * nh >= th * nw { (tw, nw) } else { (th, nh) };
    if num >= den { return natural } // not a reduction
    // Each axis scaled by num/den, rounded UP so the box is covered.
    let w = (nw * num).div_ceil(den).clamp(1, nw);
    let h = (nh * num).div_ceil(den).clamp(1, nh);
    (w as u32, h as u32)
}

const Q: i64 = 1 << 14;

/// One output sample's taps: the first source index and integer weights
/// summing to exactly `Q`.
struct Taps { start: usize, w: Vec<i32> }

fn taps(n_in: usize, n_out: usize, f: Filter) -> Vec<Taps> {
    if f == Filter::Area { return area_taps(n_in, n_out) }
    let scale = n_in as f64 / n_out as f64;
    let fs = scale.max(1.0);
    let support = f.support() * fs;
    (0..n_out).map(|o| {
        let centre = (o as f64 + 0.5) * scale;
        let lo = ((centre - support).floor().max(0.0)) as usize;
        let hi = ((centre + support).ceil() as usize).min(n_in);
        let raw: Vec<f64> = (lo..hi).map(|i| f.eval((i as f64 + 0.5 - centre) / fs)).collect();
        quantise(lo, &raw)
    }).collect()
}

/// ★ Exact area coverage in integers: output pixel `o` covers source span
/// `[o·n_in, (o+1)·n_in)` in units of 1/n_out of a source pixel, and source
/// pixel `i` covers `[i·n_out, (i+1)·n_out)`.
fn area_taps(n_in: usize, n_out: usize) -> Vec<Taps> {
    let (ni, no) = (n_in as u64, n_out as u64);
    (0..no).map(|o| {
        let (a, b) = (o * ni, (o + 1) * ni);
        let lo = (a / no) as usize;
        let hi = (b.div_ceil(no) as usize).min(n_in);
        let raw: Vec<f64> = (lo..hi).map(|i| {
            let (s, e) = (i as u64 * no, (i as u64 + 1) * no);
            (e.min(b).saturating_sub(s.max(a))) as f64
        }).collect();
        quantise(lo, &raw)
    }).collect()
}

/// Weights to integers summing to exactly `Q`; the rounding residue goes to
/// the largest tap, so a flat field stays flat.
fn quantise(start: usize, raw: &[f64]) -> Taps {
    let sum: f64 = raw.iter().sum();
    let mut w: Vec<i32> = raw.iter().map(|v| (v / sum * Q as f64).round() as i32).collect();
    let got: i64 = w.iter().map(|&v| v as i64).sum();
    if let Some((i, _)) = w.iter().enumerate().max_by_key(|(_, v)| **v) {
        w[i] += (Q - got) as i32;
    }
    // Trim zero taps at the ends so a window is no wider than it must be.
    let first = w.iter().position(|&v| v != 0).unwrap_or(0);
    let last = w.iter().rposition(|&v| v != 0).map(|i| i + 1).unwrap_or(w.len());
    Taps { start: start + first, w: w[first..last].to_vec() }
}

/// Premultiplied sample scale: every channel is in 0..=255·255.
const FULL: i64 = 255 * 255;

/// Resample straight-alpha RGBA8 `src` (`w × h`) to `ow × oh`. See the
/// module comment for the arithmetic.
pub fn resample(src: &[u8], w: u32, h: u32, ow: u32, oh: u32, f: Filter) -> Vec<u8> {
    let (w, h, ow, oh) = (w as usize, h as usize, ow as usize, oh as usize);
    assert_eq!(src.len(), w * h * 4, "source is w × h RGBA8");
    assert!(ow >= 1 && oh >= 1 && ow <= w && oh <= h, "a resample is a reduction");
    if (ow, oh) == (w, h) { return src.to_vec() }
    let hx = taps(w, ow, f);
    let vy = taps(h, oh, f);
    // One source row, premultiplied and filtered horizontally: 4 × ow
    // samples on the 255·255 scale.
    let hrow = |y: usize| -> Vec<i32> {
        let row = &src[y * w * 4..(y + 1) * w * 4];
        let mut out = vec![0i32; ow * 4];
        for (o, t) in hx.iter().enumerate() {
            let mut acc = [0i64; 4];
            for (k, &wt) in t.w.iter().enumerate() {
                let p = &row[(t.start + k) * 4..(t.start + k) * 4 + 4];
                let a = p[3] as i64;
                acc[0] += wt as i64 * p[0] as i64 * a;
                acc[1] += wt as i64 * p[1] as i64 * a;
                acc[2] += wt as i64 * p[2] as i64 * a;
                acc[3] += wt as i64 * a * 255;
            }
            for c in 0..4 { out[o * 4 + c] = round_q(acc[c]).clamp(0, FULL) as i32 }
        }
        out
    };
    let mut out = vec![0u8; ow * oh * 4];
    // A ring of filtered rows: the vertical windows only move forward.
    let mut ring: std::collections::VecDeque<(usize, Vec<i32>)> = Default::default();
    for (oy, t) in vy.iter().enumerate() {
        while ring.front().is_some_and(|(y, _)| *y < t.start) { ring.pop_front(); }
        let next = ring.back().map(|(y, _)| y + 1).unwrap_or(t.start);
        for y in next..t.start + t.w.len() { ring.push_back((y, hrow(y))) }
        let base = ring.front().map(|(y, _)| *y).expect("window is not empty");
        for ox in 0..ow {
            let mut acc = [0i64; 4];
            for (k, &wt) in t.w.iter().enumerate() {
                let r = &ring[t.start + k - base].1[ox * 4..ox * 4 + 4];
                for c in 0..4 { acc[c] += wt as i64 * r[c] as i64 }
            }
            let a = round_q(acc[3]).clamp(0, FULL);
            let px = &mut out[(oy * ow + ox) * 4..(oy * ow + ox) * 4 + 4];
            if a == 0 { px.copy_from_slice(&[0, 0, 0, 0]); continue }
            for c in 0..3 {
                // colour = (colour·alpha) / alpha, with both on the 255·255
                // scale; a premultiplied colour above its alpha (cubic
                // overshoot) is clamped to it.
                let p = round_q(acc[c]).clamp(0, a);
                px[c] = ((p * 255 + a / 2) / a) as u8;
            }
            px[3] = ((a + 127) / 255) as u8;
        }
    }
    out
}

/// Round a sum of `Q`-scaled terms to the nearest integer, halves away
/// from zero.
fn round_q(v: i64) -> i64 {
    if v >= 0 { (v + Q / 2) / Q } else { -((-v + Q / 2) / Q) }
}
