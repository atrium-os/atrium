//! Pixel comparison helpers for differential testing.
//!
//! Shaders produce float-valued RGBA outputs. Pixel-exact
//! equality across backends is not always achievable due
//! to legal IEEE-754 reordering by codegen; we provide a
//! tolerance-aware comparator and an "exact" mode for
//! tests that need it.

/// Per-channel tolerance for pixel comparison.
///
/// `Exact` is the strictest mode — every channel must match
/// bit-exactly. Backends that take legal IEEE-754
/// rearrangement liberties (e.g. fused multiply-add vs
/// separate mul+add) may need [`AbsEpsilon`] instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorTolerance {
    /// Bit-exact comparison; channels must agree to the
    /// last bit. The strictest mode; use when the shader
    /// is known to be deterministic across backends.
    Exact,
    /// Per-channel absolute epsilon. Any channel may
    /// differ by up to `eps` without failing.
    AbsEpsilon {
        /// Maximum allowed per-channel absolute diff.
        eps: f32,
    },
}

/// A single RGBA pixel value as four f32 channels.
pub type RgbaF32 = [f32; 4];

/// Compare two pixel buffers under a given tolerance.
///
/// Returns `Ok(())` if every pixel is within tolerance,
/// otherwise `Err(PixelMismatch)` describing the first
/// failure.
pub fn compare_buffers(
    a: &[RgbaF32],
    b: &[RgbaF32],
    tolerance: ColorTolerance,
) -> Result<(), PixelMismatch> {
    if a.len() != b.len() {
        return Err(PixelMismatch::LengthDiffers {
            a_len: a.len(),
            b_len: b.len(),
        });
    }
    for (i, (pa, pb)) in a.iter().zip(b.iter()).enumerate() {
        if !pixel_eq(pa, pb, tolerance) {
            return Err(PixelMismatch::Differs {
                index: i,
                a: *pa,
                b: *pb,
                tolerance,
            });
        }
    }
    Ok(())
}

fn pixel_eq(a: &RgbaF32, b: &RgbaF32, tolerance: ColorTolerance) -> bool {
    match tolerance {
        ColorTolerance::Exact => a == b,
        ColorTolerance::AbsEpsilon { eps } => {
            (0..4).all(|c| (a[c] - b[c]).abs() <= eps)
        }
    }
}

/// A pixel-buffer comparison failure.
#[derive(Debug, Clone)]
pub enum PixelMismatch {
    /// Buffers have different lengths.
    LengthDiffers {
        /// Length of the first buffer.
        a_len: usize,
        /// Length of the second buffer.
        b_len: usize,
    },
    /// A pixel differs beyond tolerance.
    Differs {
        /// Linear pixel index where the disagreement was
        /// first observed.
        index: usize,
        /// First buffer's pixel value.
        a: RgbaF32,
        /// Second buffer's pixel value.
        b: RgbaF32,
        /// Tolerance under which the comparison ran.
        tolerance: ColorTolerance,
    },
}

impl std::fmt::Display for PixelMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PixelMismatch::LengthDiffers { a_len, b_len } => write!(
                f,
                "pixel buffers have different lengths: {a_len} vs {b_len}",
            ),
            PixelMismatch::Differs { index, a, b, tolerance } => write!(
                f,
                "pixel {index} differs under {tolerance:?}: {a:?} vs {b:?}",
            ),
        }
    }
}

impl std::error::Error for PixelMismatch {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_equal() {
        let a = [[1.0, 0.0, 0.0, 1.0]];
        let b = [[1.0, 0.0, 0.0, 1.0]];
        compare_buffers(&a, &b, ColorTolerance::Exact).unwrap();
    }

    #[test]
    fn exact_differs() {
        let a = [[1.0, 0.0, 0.0, 1.0]];
        let b = [[1.0, 0.0001, 0.0, 1.0]];
        assert!(compare_buffers(&a, &b, ColorTolerance::Exact).is_err());
    }

    #[test]
    fn epsilon_within_bound() {
        let a = [[1.0, 0.0, 0.0, 1.0]];
        let b = [[1.0, 0.0001, 0.0, 1.0]];
        compare_buffers(&a, &b, ColorTolerance::AbsEpsilon { eps: 0.001 }).unwrap();
    }

    #[test]
    fn epsilon_outside_bound() {
        let a = [[1.0, 0.0, 0.0, 1.0]];
        let b = [[1.0, 0.01, 0.0, 1.0]];
        assert!(compare_buffers(
            &a, &b,
            ColorTolerance::AbsEpsilon { eps: 0.001 },
        ).is_err());
    }

    #[test]
    fn length_mismatch() {
        let a: &[RgbaF32] = &[[1.0; 4]];
        let b: &[RgbaF32] = &[[1.0; 4], [1.0; 4]];
        let err = compare_buffers(a, b, ColorTolerance::Exact).unwrap_err();
        match err {
            PixelMismatch::LengthDiffers { a_len, b_len } => {
                assert_eq!((a_len, b_len), (1, 2));
            }
            other => panic!("expected LengthDiffers, got {other:?}"),
        }
    }
}
