//! ★ WHERE NORMALIZE'S TIME GOES (legacy-web §5.4.5.7, cut B). Observation
//! only: a lap timer per thread, read by the driver's display timing. Nothing
//! here is read by the normalizer itself, so its output cannot depend on it.
//!
//! `lap(name)` charges the time since the previous lap (or `start`) to
//! `name`; `take` returns the totals since `start` and clears them.

use std::cell::RefCell;
use std::time::{Duration, Instant};

thread_local! {
    static T: RefCell<(Option<Instant>, Vec<(&'static str, Duration)>)> = const { RefCell::new((None, Vec::new())) };
}

/// Start (or restart) the laps.
pub fn start() { T.with(|t| *t.borrow_mut() = (Some(Instant::now()), Vec::new())) }

/// Charge the time since the last lap to `name`. Does nothing unless started.
pub fn lap(name: &'static str) {
    T.with(|t| {
        let mut t = t.borrow_mut();
        let Some(last) = t.0 else { return };
        let now = Instant::now();
        match t.1.iter_mut().find(|(n, _)| *n == name) {
            Some(e) => e.1 += now - last,
            None => t.1.push((name, now - last)),
        }
        t.0 = Some(now);
    })
}

/// The laps since `start`, in first-seen order; stops the timer.
pub fn take() -> Vec<(&'static str, Duration)> {
    T.with(|t| { let mut t = t.borrow_mut(); t.0 = None; std::mem::take(&mut t.1) })
}
