//! The receive counter window (WireGuard paper §5.4.6, RFC 6479's bitmap):
//! a counter is accepted once, and only if it is newer than the window's
//! bottom. Checked before decryption cheaply, marked after it succeeds.

use crate::limits::REJECT_AFTER_MESSAGES;

const BITS: u64 = 2048;
const WORDS: usize = (BITS / 64) as usize;
/// How many counters the window covers, ending at the newest: one
/// `WINDOW - 1` below it is still accepted once.
pub const WINDOW: u64 = BITS - 64;

#[derive(Debug, Clone)]
pub struct Window {
    /// The highest counter seen + 1 (0: none yet).
    next: u64,
    bitmap: [u64; WORDS],
}

impl Default for Window {
    fn default() -> Self {
        Window { next: 0, bitmap: [0; WORDS] }
    }
}

impl Window {
    /// Whether `c` could be new (no state change).
    pub fn check(&self, c: u64) -> bool {
        if c >= REJECT_AFTER_MESSAGES {
            return false;
        }
        if c >= self.next {
            return true;
        }
        if self.next - c > WINDOW {
            return false;
        }
        let bit = c % BITS;
        self.bitmap[(bit / 64) as usize] & (1 << (bit % 64)) == 0
    }

    /// Mark `c` seen; false if it was not new (then nothing changes).
    pub fn mark(&mut self, c: u64) -> bool {
        if !self.check(c) {
            return false;
        }
        if c >= self.next {
            // Clear the words the window moves past.
            let old_word = if self.next == 0 { None } else { Some((self.next - 1) / 64) };
            let new_word = c / 64;
            match old_word {
                None => self.bitmap = [0; WORDS],
                Some(o) => {
                    let steps = (new_word - o).min(WORDS as u64);
                    for k in 1..=steps {
                        self.bitmap[((o + k) % WORDS as u64) as usize] = 0;
                    }
                }
            }
            self.next = c + 1;
        }
        let bit = c % BITS;
        self.bitmap[(bit / 64) as usize] |= 1 << (bit % 64);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_each_in_any_order_within_the_window() {
        let mut w = Window::default();
        assert!(w.mark(0));
        assert!(!w.mark(0));
        assert!(w.mark(5));
        assert!(w.mark(3));
        assert!(!w.mark(3));
        assert!(w.mark(1));
        assert!(w.mark(10_000));
        assert!(!w.mark(5), "far behind now");
        assert!(w.mark(10_000 - WINDOW + 1), "the window's bottom");
        assert!(!w.mark(10_000 - WINDOW));
        assert!(!w.mark(10_000));
        assert!(w.mark(9_999));
        assert!(!w.mark(REJECT_AFTER_MESSAGES));
    }

    #[test]
    fn a_big_jump_forgets_the_old_bits() {
        let mut w = Window::default();
        for c in 0..3000 {
            assert!(w.mark(c));
        }
        assert!(w.mark(3000 + 5 * BITS));
        // A counter in the new window whose bit position was used before.
        assert!(w.mark(3000 + 5 * BITS - 100));
        assert!(!w.mark(3000 + 5 * BITS - 100));
    }

    #[test]
    fn matches_a_set_model() {
        // A small pseudo-random walk against the obvious model.
        let mut w = Window::default();
        let mut seen = std::collections::BTreeSet::new();
        let mut top = 0u64;
        let mut x = 12345u64;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let c = match x % 4 {
                0 => top + x % 70,
                _ => top.saturating_sub(x % 2200),
            };
            // New, and above the bottom: next (top + 1) - WINDOW.
            let model = !seen.contains(&c) && (seen.is_empty() || c > top || top - c < WINDOW);
            let got = w.mark(c);
            assert_eq!(got, model, "counter {c}, top {top}");
            if got {
                seen.insert(c);
                top = top.max(c);
            }
        }
    }
}
