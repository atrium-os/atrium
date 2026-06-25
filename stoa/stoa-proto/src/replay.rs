//! Anti-replay sliding window (stoa.md §3.1).
//!
//! > Anti-replay: receiver tracks (last_seq, sliding window of N=128
//! > past seqs). Out-of-window or duplicate-in-window → drop silently.
//!
//! Standard IPsec/SSP-style window: a `highest` seq seen plus a 128-bit
//! bitmap where bit `i` records that `highest - i` was already accepted
//! (bit 0 = `highest` itself). A datagram is accepted exactly once:
//!
//! - `seq > highest`  — fresh; slide the window forward.
//! - `seq == highest` — duplicate of the newest; reject.
//! - `highest - seq < 128` and its bit is clear — fresh in-window; mark.
//! - `highest - seq < 128` and its bit is set — duplicate; reject.
//! - `highest - seq >= 128` — too old (fell off the window); reject.
//!
//! Reattach uses a *fresh* MAC-key/window (stoa.md §2), so a new
//! [`ReplayWindow`] is the right thing to construct on every attach;
//! sequence-space wraparound within one session is out of scope for v1
//! (a u32 at terminal rates is decades).

/// The window width, N.
pub const WINDOW: u32 = 128;

/// Per-direction replay state. Construct one per attach.
#[derive(Debug, Default, Clone)]
pub struct ReplayWindow {
    /// Highest seq accepted so far; `None` until the first datagram.
    highest: Option<u32>,
    /// Bit `i` set ⇒ `highest - i` has been accepted. Bit 0 ⇒ `highest`.
    bits: u128,
}

impl ReplayWindow {
    pub fn new() -> Self {
        ReplayWindow::default()
    }

    /// Check `seq` against the window and, if fresh, record it. Returns
    /// `true` iff the datagram should be accepted (fresh, in policy).
    /// Idempotent on rejection: a rejected seq does not mutate state.
    pub fn accept(&mut self, seq: u32) -> bool {
        let highest = match self.highest {
            None => {
                // First datagram of the session direction.
                self.highest = Some(seq);
                self.bits = 1; // bit 0 = this seq, now seen
                return true;
            }
            Some(h) => h,
        };

        if seq > highest {
            // Slide forward by the gap; the old window shifts down, and
            // the new highest takes bit 0.
            let shift = seq - highest;
            self.bits = if shift >= 128 { 0 } else { self.bits << shift };
            self.bits |= 1;
            self.highest = Some(seq);
            true
        } else {
            let diff = highest - seq;
            if diff >= WINDOW {
                return false; // too old
            }
            let mask = 1u128 << diff;
            if self.bits & mask != 0 {
                false // duplicate
            } else {
                self.bits |= mask;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_is_accepted() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1000));
    }

    #[test]
    fn exact_duplicate_of_newest_rejected() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(5));
        assert!(!w.accept(5));
    }

    #[test]
    fn monotonic_sequence_all_accepted() {
        let mut w = ReplayWindow::new();
        for s in 1..=300 {
            assert!(w.accept(s), "seq {s} should be fresh");
        }
    }

    #[test]
    fn in_window_reorder_accepted_once() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(10));
        assert!(w.accept(12)); // skip 11 (it's still in-window)
        assert!(w.accept(11)); // late but in-window → accept
        assert!(!w.accept(11)); // now a duplicate → reject
        assert!(!w.accept(12)); // duplicate → reject
        assert!(!w.accept(10)); // duplicate → reject
    }

    #[test]
    fn too_old_rejected() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(200));
        // 200 - 71 = 129 ≥ 128 → outside the window.
        assert!(!w.accept(71));
        // 200 - 73 = 127 < 128 → inside, fresh → accept.
        assert!(w.accept(73));
    }

    #[test]
    fn big_forward_jump_resets_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1));
        assert!(w.accept(1_000_000)); // shift ≥ 128 → window cleared
        // The old seqs are now "too old", not "duplicates" — either way reject.
        assert!(!w.accept(1));
        assert!(!w.accept(1_000_000)); // newest dup
        assert!(w.accept(999_999)); // in-window behind the new highest
    }

    #[test]
    fn boundary_127_in_128_out() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(128));
        assert!(w.accept(1)); // diff 127 → in
        let mut w2 = ReplayWindow::new();
        assert!(w2.accept(129));
        assert!(!w2.accept(1)); // diff 128 → out
    }
}
