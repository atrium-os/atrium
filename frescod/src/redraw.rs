//! Damage-driven redraw signal (task #25).
//!
//! frescod's display loop used to render + page-flip at a fixed 30 fps
//! regardless of whether the composited image changed — continuous CPU +
//! GPU work on a static idle screen. The display engine scans out the
//! shared VRAM BO independently (the decoupled display architecture), so
//! frescod only needs to recompose + flip when the output actually
//! changes.
//!
//! This is a monotonic generation counter guarded by a mutex + condvar.
//! Every damage source (client scene/window ops via the socket server;
//! input + compositor events via the fan-out thread) calls `wake()`,
//! which bumps the counter and notifies. The frame loop renders only when
//! the counter advanced past what it last drew, and otherwise blocks in
//! `wait_past()` until woken (or a conservative heartbeat elapses as a
//! safety net against any unwired damage source).

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub struct RedrawSignal {
    /// Bumped on every damage event. Starts at 1 so a frame loop tracking
    /// `last = 0` always renders the first frame.
    gen: Mutex<u64>,
    cv: Condvar,
}

pub type Redraw = Arc<RedrawSignal>;

impl RedrawSignal {
    pub fn new() -> Redraw {
        Arc::new(Self { gen: Mutex::new(1), cv: Condvar::new() })
    }

    /// Signal that render-relevant state changed. Cheap; safe to over-call
    /// (bumps coalesce — the frame loop renders once per observed value).
    pub fn wake(&self) {
        let mut g = self.gen.lock().unwrap();
        *g = g.wrapping_add(1);
        self.cv.notify_all();
    }

    /// Current generation (what the next render would be drawing).
    pub fn current(&self) -> u64 {
        *self.gen.lock().unwrap()
    }

    /// Block until the generation differs from `last`, or `timeout` elapses.
    /// Returns the generation observed on wake — pass it back as the next
    /// `last`. The timeout is the idle heartbeat: even with no damage the
    /// loop re-checks (and re-renders) at most once per `timeout`.
    pub fn wait_past(&self, last: u64, timeout: Duration) -> u64 {
        let g = self.gen.lock().unwrap();
        if *g != last {
            return *g;
        }
        let (g, _) = self.cv.wait_timeout(g, timeout).unwrap();
        *g
    }
}
