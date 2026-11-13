//! When a fingerprint may stand in for the passcode (sigillum.md §11.4).
//!
//! The rules are Apple's, which have held up: a fingerprint works only while
//! the user's keys are in memory (never at boot or after sign-out: the root
//! key needs the passcode), within 48 hours of the last unlock, not after 5
//! failed matches in a row, and not when the passcode hasn't been entered
//! for 6.5 days *and* no fingerprint unlock happened in the last 4 hours.

pub const HOUR_MS: u64 = 3600 * 1000;
pub const MAX_SINCE_UNLOCK_MS: u64 = 48 * HOUR_MS;
pub const PASSCODE_STALE_MS: u64 = 156 * HOUR_MS; // 6.5 days
pub const BIO_STALE_MS: u64 = 4 * HOUR_MS;
pub const MAX_FAILS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    /// Booted or signed out: the user's keys are not in memory.
    Cold,
    Unlocked,
    ScreenLocked,
}

#[derive(Debug, Clone)]
pub struct BioPolicy {
    store: Store,
    last_passcode: Option<u64>,
    last_bio: Option<u64>,
    last_unlock: Option<u64>,
    fails: u32,
}

impl Default for BioPolicy {
    fn default() -> BioPolicy {
        BioPolicy { store: Store::Cold, last_passcode: None, last_bio: None, last_unlock: None, fails: 0 }
    }
}

impl BioPolicy {
    pub fn allowed(&self, now: u64) -> bool {
        let since = |t: Option<u64>| t.map(|t| now.saturating_sub(t));
        let (Some(p), Some(u)) = (since(self.last_passcode), since(self.last_unlock)) else {
            return false;
        };
        let b = since(self.last_bio).unwrap_or(u64::MAX);
        self.store != Store::Cold && self.fails < MAX_FAILS && u <= MAX_SINCE_UNLOCK_MS && !(p > PASSCODE_STALE_MS && b > BIO_STALE_MS)
    }

    /// The passcode was entered (login, unlock, verify).
    pub fn passcode(&mut self, now: u64) {
        self.store = Store::Unlocked;
        self.last_passcode = Some(now);
        self.last_unlock = Some(now);
        self.fails = 0;
    }

    /// A fingerprint unlocked or verified; only counts if it was allowed.
    pub fn matched(&mut self, now: u64) -> bool {
        if !self.allowed(now) {
            return false;
        }
        self.store = Store::Unlocked;
        self.last_bio = Some(now);
        self.last_unlock = Some(now);
        self.fails = 0;
        true
    }

    pub fn failed(&mut self) {
        self.fails = self.fails.saturating_add(1);
    }

    pub fn screen_locked(&mut self) {
        if self.store == Store::Unlocked {
            self.store = Store::ScreenLocked;
        }
    }

    /// Reboot or sign-out: forget everything.
    pub fn cold(&mut self) {
        *self = BioPolicy::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000_000;

    #[test]
    fn never_cold() {
        let p = BioPolicy::default();
        assert!(!p.allowed(T0));
        let mut p = BioPolicy::default();
        p.passcode(T0);
        p.cold();
        assert!(!p.allowed(T0 + 1));
    }

    #[test]
    fn within_48_hours_of_an_unlock() {
        let mut p = BioPolicy::default();
        p.passcode(T0);
        p.screen_locked();
        assert!(p.allowed(T0 + 47 * HOUR_MS));
        assert!(!p.allowed(T0 + 49 * HOUR_MS));
        assert!(p.matched(T0 + 40 * HOUR_MS));
        assert!(p.allowed(T0 + 80 * HOUR_MS), "a fingerprint unlock restarts the 48 h");
    }

    #[test]
    fn passcode_goes_stale_after_six_and_a_half_days() {
        let mut p = BioPolicy::default();
        p.passcode(T0);
        // Fingerprint unlocks every 3 hours for a week keep working: each is
        // within 4 h of the last one.
        let mut t = T0;
        while t < T0 + 170 * HOUR_MS {
            t += 3 * HOUR_MS;
            assert!(p.matched(t), "at {} h", (t - T0) / HOUR_MS);
        }
        // Past 6.5 days since the passcode, a 5-hour gap needs the passcode.
        assert!(p.allowed(t + 3 * HOUR_MS));
        assert!(!p.allowed(t + 5 * HOUR_MS));
        // Before 6.5 days, a 40-hour gap is fine.
        let mut q = BioPolicy::default();
        q.passcode(T0);
        assert!(q.matched(T0 + 40 * HOUR_MS));
        assert!(q.allowed(T0 + 80 * HOUR_MS));
    }

    #[test]
    fn five_failures() {
        let mut p = BioPolicy::default();
        p.passcode(T0);
        for _ in 0..4 {
            p.failed();
        }
        assert!(p.allowed(T0));
        p.failed();
        assert!(!p.allowed(T0));
        assert!(!p.matched(T0));
        p.passcode(T0 + 1);
        assert!(p.allowed(T0 + 1));
    }
}
