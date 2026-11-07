//! Key repeat (docs/spec/atrium-input.md §5.1): synthesised here, the same
//! for every source, from a delay and a rate. Only the last key pressed
//! repeats, as on every desktop; modifiers and lock keys never do.

use std::time::{Duration, Instant};

use crate::hid::{Usage, PAGE_KEYBOARD};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub delay: Duration,
    pub interval: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { delay: Duration::from_millis(500), interval: Duration::from_millis(33) }
    }
}

impl Settings {
    /// `TACTUS_REPEAT=<delay ms>,<per second>` (0 per second: off).
    pub fn from_env() -> Settings {
        let mut s = Settings::default();
        if let Ok(v) = std::env::var("TACTUS_REPEAT") {
            let mut it = v.split(',').map(|p| p.trim().parse::<u64>().ok());
            if let (Some(Some(d)), Some(Some(r))) = (it.next(), it.next()) {
                s.delay = Duration::from_millis(d);
                s.interval = if r == 0 { Duration::ZERO } else { Duration::from_micros(1_000_000 / r) };
            }
        }
        s
    }
}

pub fn repeats(u: Usage) -> bool {
    u >> 16 == u32::from(PAGE_KEYBOARD) && !matches!(u & 0xffff, 0xe0..=0xe7 | 0x39 | 0x47 | 0x53 | 0x00..=0x03)
}

#[derive(Debug, Default)]
pub struct Repeat {
    settings: Settings,
    /// The repeating key: device, usage, when it next repeats.
    armed: Option<(u32, Usage, Instant)>,
}

impl Repeat {
    pub fn new(settings: Settings) -> Repeat {
        Repeat { settings, armed: None }
    }

    /// A key changed on device `dev` at `now`.
    pub fn key(&mut self, dev: u32, u: Usage, down: bool, now: Instant) {
        if down && repeats(u) && !self.settings.interval.is_zero() {
            self.armed = Some((dev, u, now + self.settings.delay));
        } else if !down && matches!(self.armed, Some((d, a, _)) if d == dev && a == u) {
            self.armed = None;
        }
    }

    /// The device went away.
    pub fn gone(&mut self, dev: u32) {
        if matches!(self.armed, Some((d, ..)) if d == dev) {
            self.armed = None;
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.armed.map(|(.., t)| t)
    }

    /// The repeat due at `now`, if any (one per call; a late caller
    /// does not get a burst).
    pub fn due(&mut self, now: Instant) -> Option<(u32, Usage)> {
        let (dev, u, t) = self.armed?;
        if now < t {
            return None;
        }
        self.armed = Some((dev, u, (t + self.settings.interval).max(now)));
        Some((dev, u))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::usage;

    #[test]
    fn the_last_key_repeats_after_the_delay() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut r = Repeat::new(Settings::default());
        r.key(1, usage(7, 0xe1), true, t0);
        assert_eq!(r.deadline(), None, "shift does not repeat");
        r.key(1, usage(7, 4), true, t0);
        assert_eq!(r.due(ms(499)), None);
        assert_eq!(r.due(ms(500)), Some((1, usage(7, 4))));
        assert_eq!(r.due(ms(520)), None);
        assert_eq!(r.due(ms(533)), Some((1, usage(7, 4))));
        r.key(1, usage(7, 5), true, ms(540));
        r.key(1, usage(7, 4), false, ms(541));
        assert!(r.deadline().is_some(), "releasing an older key keeps the newer one");
        r.key(1, usage(7, 5), false, ms(542));
        assert_eq!(r.deadline(), None);
        r.key(2, usage(7, 6), true, ms(600));
        r.gone(2);
        assert_eq!(r.deadline(), None);
        assert!(!repeats(usage(0x0c, 0xcd)), "media keys do not repeat");
    }
}
