//! The replay window against the obvious model: a counter is accepted once,
//! and only within WINDOW of the newest.
//!   cargo fuzz run replay
#![no_main]
use crypta_wg::replay::{Window, WINDOW};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

fuzz_target!(|data: &[u8]| {
    let mut w = Window::default();
    let mut seen = BTreeSet::new();
    let mut top: Option<u64> = None;
    for c in data.chunks_exact(3) {
        // Steps forward and back from the top, sometimes large.
        let d = u64::from(u16::from_le_bytes([c[1], c[2]]));
        let base = top.unwrap_or(0);
        let ctr = match c[0] % 4 {
            0 => base + d,
            1 => base.saturating_sub(d),
            2 => base + d * 977,
            _ => base.saturating_sub(d % 2100),
        };
        let model = !seen.contains(&ctr) && top.is_none_or(|t| ctr > t || t - ctr < WINDOW);
        assert_eq!(w.check(ctr), model);
        assert_eq!(w.mark(ctr), model);
        if model {
            seen.insert(ctr);
            top = Some(top.map_or(ctr, |t| t.max(ctr)));
        }
    }
});
