//! The pointer stage (docs/spec/atrium-input.md §5.2): acceleration for
//! relative devices. The gain rises with speed between a low and a high
//! threshold, so slow moves stay precise and fast ones cross the screen.
//! Raw deltas are always kept beside the accelerated ones; pointer lock
//! uses raw.

use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Curve {
    /// Below this speed (counts per ms) the gain is 1.
    pub slow: f32,
    /// At and above this speed the gain is `max_gain`.
    pub fast: f32,
    pub max_gain: f32,
}

impl Curve {
    /// No acceleration.
    pub const FLAT: Curve = Curve { slow: f32::MAX, fast: f32::MAX, max_gain: 1.0 };

    pub fn gain(&self, speed: f32) -> f32 {
        if speed <= self.slow || self.max_gain <= 1.0 {
            return 1.0;
        }
        let t = ((speed - self.slow) / (self.fast - self.slow).max(1e-3)).min(1.0);
        1.0 + t * (self.max_gain - 1.0)
    }

    /// `TACTUS_POINTER=flat` or `<slow>,<fast>,<max gain>`.
    pub fn from_env() -> Curve {
        match std::env::var("TACTUS_POINTER").as_deref() {
            Ok("flat") => Curve::FLAT,
            Ok(v) => {
                let p: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                match p[..] {
                    [slow, fast, max_gain] if fast > slow && max_gain >= 1.0 => Curve { slow, fast, max_gain },
                    _ => Curve::default(),
                }
            }
            Err(_) => Curve::default(),
        }
    }
}

impl Default for Curve {
    fn default() -> Self {
        Curve { slow: 0.4, fast: 4.0, max_gain: 3.0 }
    }
}

/// One relative device's acceleration state.
#[derive(Debug)]
pub struct Stage {
    curve: Curve,
    last: Option<Instant>,
    /// Smoothed speed, counts per ms.
    speed: f32,
    /// Sub-pixel remainder carried between reports.
    rest: (f32, f32),
}

impl Stage {
    pub fn new(curve: Curve) -> Stage {
        Stage { curve, last: None, speed: 0.0, rest: (0.0, 0.0) }
    }

    /// Accelerated `(dx, dy)` for raw counts at `now`.
    pub fn motion(&mut self, dx: i32, dy: i32, now: Instant) -> (f32, f32) {
        let dist = ((dx as f32).powi(2) + (dy as f32).powi(2)).sqrt();
        let ms = self.last.map(|t| now.saturating_duration_since(t).as_secs_f32() * 1000.0).unwrap_or(8.0);
        self.last = Some(now);
        // A pause resets the speed; otherwise smooth over a few reports.
        let inst = dist / ms.clamp(1.0, 50.0);
        self.speed = if ms > 100.0 { inst } else { 0.6 * self.speed + 0.4 * inst };
        let g = self.curve.gain(self.speed);
        let (x, y) = (dx as f32 * g + self.rest.0, dy as f32 * g + self.rest.1);
        self.rest = (x - x.trunc(), y - y.trunc());
        (x.trunc(), y.trunc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn slow_is_exact_fast_is_faster() {
        let t0 = Instant::now();
        let mut s = Stage::new(Curve::default());
        let mut sum = 0.0;
        for i in 1..=100 {
            sum += s.motion(1, 0, t0 + Duration::from_millis(8 * i)).0;
        }
        assert_eq!(sum, 100.0, "1 count per 8 ms moves exactly");
        let mut f = Stage::new(Curve::default());
        let mut sum = 0.0;
        for i in 1..=100 {
            sum += f.motion(40, 0, t0 + Duration::from_millis(8 * i)).0;
        }
        assert!(sum > 4000.0 * 2.5, "fast moves gain: {sum}");
        let mut flat = Stage::new(Curve::FLAT);
        assert_eq!(flat.motion(40, -3, t0), (40.0, -3.0));
    }
}
