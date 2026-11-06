//! Credential slots (docs/spec/arca-autofill.md §4): what frescod saw.
//!
//! Each composed frame, every credential slot is checked: its rect wholly
//! inside its parent and the parent's slot on screen (below frescod's
//! strip), nothing composed above it overlapping it, at least
//! `credential::MIN_W` x `MIN_H`, and where it was last frame. While that
//! holds it is a visible run; anything else ends the run. A `FILL` press
//! counts only if one run covered the `STILL_NS` before the press up to
//! the press. A slot is a window of its own above its parent, so the
//! parent has no way to fade, blur, scale or transform it.

use std::collections::HashMap;
use std::sync::Mutex;

use fresco_protocol::credential::{MIN_H, MIN_W, STILL_NS};
use fresco_scene_server::window::Compositor;

#[derive(Debug, Default)]
struct Rec {
    /// Its screen rect last frame.
    rect: [i32; 4],
    /// The current visible run's start.
    since: Option<u64>,
    /// The last run that ended: (start, end).
    last: Option<(u64, u64)>,
}

static RECS: Mutex<Option<HashMap<u32, Rec>>> = Mutex::new(None);

fn overlaps(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// Record a composed frame: `layers` are each window's screen rect
/// (`[x0, y0, x1, y1]`) bottom to top, as composed, without frescod's own
/// chrome; the screen is `screen` (`[x0, y0, x1, y1]`, the strip left out).
/// Returns the slots visible now, for the chrome to frame.
pub fn observe_frame(comp: &Mutex<Compositor>, layers: &[(u32, [i32; 4])], screen: [i32; 4], now: u64) -> Vec<[i32; 4]> {
    let slots: Vec<(u32, bool)> = {
        let c = comp.lock().unwrap();
        c.windows.iter().filter_map(|(&id, w)| {
            let s = w.slot.filter(|s| s.kind == fresco_protocol::slot::kind::CREDENTIAL)?;
            let p = c.windows.get(&s.parent)?;
            let [x, y, rw, rh] = s.rect;
            let inside = x >= 0.0 && y >= 0.0 && x + rw <= p.size.0 && y + rh <= p.size.1;
            Some((id as u32, inside && rw >= MIN_W && rh >= MIN_H))
        }).collect()
    };
    let mut g = RECS.lock().unwrap();
    let recs = g.get_or_insert_with(HashMap::new);
    recs.retain(|id, _| slots.iter().any(|(s, _)| s == id));
    let mut shown = Vec::new();
    for (id, fits) in slots {
        let at = layers.iter().position(|(l, _)| *l == id);
        let r = at.map(|i| layers[i].1);
        let visible = match (at, r) {
            (Some(i), Some(r)) => {
                fits
                    && r[0] >= screen[0] && r[1] >= screen[1] && r[2] <= screen[2] && r[3] <= screen[3]
                    && !layers[i + 1..].iter().any(|(_, o)| overlaps(*o, r))
            }
            _ => false,
        };
        let rec = recs.entry(id).or_default();
        let moved = r != Some(rec.rect);
        if let Some(r) = r {
            rec.rect = r;
        }
        match (rec.since, visible && !moved) {
            (Some(s), false) => {
                rec.last = Some((s, now));
                rec.since = None;
            }
            (None, false) => {}
            (Some(_), true) => {}
            (None, true) => rec.since = Some(now),
        }
        // A slot that just moved but is otherwise clear starts a new run.
        if visible && rec.since.is_none() {
            rec.since = Some(now);
        }
        if visible {
            shown.push(rec.rect);
        }
    }
    shown
}

/// Whether slot `window` was visible and still from `STILL_NS` before a
/// press minted at `minted` up to that press, as of `now`.
pub fn visible_for(window: u32, minted: u64, now: u64) -> bool {
    let g = RECS.lock().unwrap();
    let Some(rec) = g.as_ref().and_then(|r| r.get(&window)) else { return false };
    let from = minted.saturating_sub(STILL_NS);
    let covers = |s: u64, e: u64| s <= from && e >= minted;
    rec.since.is_some_and(|s| covers(s, now)) || rec.last.is_some_and(|(s, e)| covers(s, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_slot_counts_only_after_half_a_second_clear_and_still() {
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (p, s) = {
            let mut c = comp.lock().unwrap();
            let p = c.create(1, (400, 300));
            let s = c.create(2, (10, 10));
            assert!(c.bind_credential(s, p, Some([20.0, 20.0, 200.0, 40.0])));
            (p as u32, s as u32)
        };
        let screen = [0, 24, 1280, 720];
        let at = |y: i32| vec![(p, [0, 30, 400, 330]), (s, [20, y, 220, y + 40])];
        let ms = 1_000_000u64;
        let t0 = 1000 * ms;
        observe_frame(&comp, &at(50), screen, t0);
        observe_frame(&comp, &at(50), screen, t0 + 100 * ms);
        assert!(!visible_for(s, t0 + 300 * ms, t0 + 300 * ms), "too soon after it appeared");
        assert!(visible_for(s, t0 + 600 * ms, t0 + 600 * ms));
        // Moved: a new run starts.
        observe_frame(&comp, &at(60), screen, t0 + 700 * ms);
        assert!(!visible_for(s, t0 + 800 * ms, t0 + 800 * ms), "it just moved");
        assert!(visible_for(s, t0 + 1300 * ms, t0 + 1300 * ms));
        // Covered by a window above it.
        let mut over = at(60);
        over.push((99, [100, 70, 300, 120]));
        observe_frame(&comp, &over, screen, t0 + 1400 * ms);
        assert!(!visible_for(s, t0 + 1500 * ms, t0 + 1500 * ms));
        assert!(visible_for(s, t0 + 1350 * ms, t0 + 1500 * ms), "a press before the cover still counts");
        observe_frame(&comp, &at(60), screen, t0 + 1600 * ms);
        assert!(!visible_for(s, t0 + 1900 * ms, t0 + 1900 * ms), "uncovered: the half second starts again");
        // Shrunk below the minimum.
        assert!(comp.lock().unwrap().bind_credential(s as u16, p as u16, Some([20.0, 20.0, 100.0, 20.0])));
        observe_frame(&comp, &at(60), screen, t0 + 2000 * ms);
        observe_frame(&comp, &at(60), screen, t0 + 3000 * ms);
        assert!(!visible_for(s, t0 + 3000 * ms, t0 + 3000 * ms), "too small");
        // Under the strip.
        assert!(comp.lock().unwrap().bind_credential(s as u16, p as u16, Some([20.0, 20.0, 200.0, 40.0])));
        observe_frame(&comp, &at(10), screen, t0 + 4000 * ms);
        observe_frame(&comp, &at(10), screen, t0 + 5000 * ms);
        assert!(!visible_for(s, t0 + 5000 * ms, t0 + 5000 * ms), "partly off the screen");
    }
}
