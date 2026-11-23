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
//!
//! Sigillum's sheets (sigillum.md §7.2) are watched the same way: every
//! top-level window of a sheet uid ([`crate::activation::sheet_uids`]) must
//! be wholly on screen, nothing composed above it overlapping it, at least
//! the minimum size and where it was last frame, for an `IDENTITY` press.
//!
//! Embedded system sheets and every other top-level window are watched
//! the same way for presses that
//! need only the asking app's own window seen whole and still: Allow on
//! Settings' Apps page (`APPROVE`, curia.md §13). Those count for nothing
//! else ([`visible_for`] is for slots and sheets alone).

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
    /// An ordinary window: neither a credential slot nor a sheet.
    plain: bool,
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
    observe_frame_with(comp, layers, screen, now, crate::activation::sheet_uids())
}

/// A window's guard (`OP_WINDOW_GUARD`) is recorded under its window id
/// with this bit set (window ids are 16 bits).
pub const GUARD: u32 = 1 << 16;

/// [`observe_frame`] with the sheet uids given (tests).
pub fn observe_frame_with(comp: &Mutex<Compositor>, layers: &[(u32, [i32; 4])], screen: [i32; 4], now: u64, sheet_uids: &[u32]) -> Vec<[i32; 4]> {
    // (record key, window, the part of the window watched, fits, plain).
    let slots: Vec<(u32, u32, Option<[i32; 4]>, bool, bool)> = {
        let c = comp.lock().unwrap();
        let guards = c.windows.iter().filter_map(|(&id, w)| {
            let g = w.guard?;
            Some((id as u32 | GUARD, id as u32, Some(g), !w.hidden && g[2] as f32 >= MIN_W && g[3] as f32 >= MIN_H, false))
        });
        let slots = c.windows.iter().filter_map(|(&id, w)| {
            if w.slot.is_none() {
                let uid = crate::socket_server::client_uid(w.owner).unwrap_or(w.owner_uid);
                let (sw, sh) = w.size;
                let fits = !w.hidden && !w.embed_pending && sw >= MIN_W && sh >= MIN_H;
                return Some((id as u32, id as u32, None, fits, !sheet_uids.contains(&uid)));
            }
            let s = w.slot?;
            if s.kind == fresco_protocol::slot::kind::SHEET {
                let (sw, sh) = w.size;
                return Some((id as u32, id as u32, None, !w.hidden && sw >= MIN_W && sh >= MIN_H, true));
            }
            if s.kind != fresco_protocol::slot::kind::CREDENTIAL {
                return None;
            }
            let p = c.windows.get(&s.parent)?;
            let [x, y, rw, rh] = s.rect;
            let inside = x >= 0.0 && y >= 0.0 && x + rw <= p.size.0 && y + rh <= p.size.1;
            Some((id as u32, id as u32, None, inside && rw >= MIN_W && rh >= MIN_H, false))
        });
        slots.chain(guards).collect()
    };
    let mut g = RECS.lock().unwrap();
    let recs = g.get_or_insert_with(HashMap::new);
    recs.retain(|id, _| slots.iter().any(|(s, ..)| s == id));
    let mut shown = Vec::new();
    for (id, window, part, fits, plain) in slots {
        let at = layers.iter().position(|(l, _)| *l == window);
        // A guard is that part of its window's layer, and must lie in it.
        let r = at.map(|i| layers[i].1).and_then(|l| match part {
            None => Some(l),
            Some([x, y, w, h]) => {
                let r = [l[0].saturating_add(x), l[1].saturating_add(y), l[0].saturating_add(x).saturating_add(w), l[1].saturating_add(y).saturating_add(h)];
                (r[2] <= l[2] && r[3] <= l[3]).then_some(r)
            }
        });
        let visible = match (at, r) {
            (Some(i), Some(r)) => {
                fits
                    && r[0] >= screen[0] && r[1] >= screen[1] && r[2] <= screen[2] && r[3] <= screen[3]
                    && !layers[i + 1..].iter().any(|(_, o)| overlaps(*o, r))
            }
            _ => false,
        };
        let rec = recs.entry(id).or_default();
        if rec.plain != plain {
            // A window that changed kind starts over.
            *rec = Rec { plain, ..Rec::default() };
        }
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
        // Credential slots and sheets get frescod's frame; plain windows
        // and guards are their own.
        if visible && !plain && part.is_none() {
            shown.push(rec.rect);
        }
    }
    shown
}

/// Whether slot `window` was visible and still from `STILL_NS` before a
/// press minted at `minted` up to that press, as of `now`.
pub fn visible_for(window: u32, minted: u64, now: u64) -> bool {
    seen_whole(window, minted, now, false)
}

/// [`visible_for`] for any top-level window (an `APPROVE` press in
/// Settings' own window), slots and sheets too.
pub fn window_visible_for(window: u32, minted: u64, now: u64) -> bool {
    seen_whole(window, minted, now, true)
}

/// Which check a press for `purpose` takes.
pub fn visible_for_purpose(purpose: u8, window: u32, minted: u64, now: u64) -> bool {
    if purpose == fresco_protocol::activation::purpose::LINK {
        // Asked of the guard: none, nothing more to see; else as `PROMPT`.
        !guarded(window) || visible_for(window, minted, now)
    } else if purpose == fresco_protocol::activation::purpose::APPROVE {
        window_visible_for(window, minted, now)
    } else {
        visible_for(window, minted, now)
    }
}

/// Whether a guard is recorded under `key` (`window | GUARD`).
pub fn guarded(key: u32) -> bool {
    key & GUARD != 0 && RECS.lock().unwrap().as_ref().is_some_and(|r| r.contains_key(&key))
}

fn seen_whole(window: u32, minted: u64, now: u64, any: bool) -> bool {
    let g = RECS.lock().unwrap();
    let Some(rec) = g.as_ref().and_then(|r| r.get(&window)) else { return false };
    if rec.plain && !any {
        return false;
    }
    let from = minted.saturating_sub(STILL_NS);
    let covers = |s: u64, e: u64| s <= from && e >= minted;
    rec.since.is_some_and(|s| covers(s, now)) || rec.last.is_some_and(|(s, e)| covers(s, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The record is one static: these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn a_slot_counts_only_after_half_a_second_clear_and_still() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn a_sheet_window_is_watched_like_a_slot() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (app, sheet) = {
            let mut c = comp.lock().unwrap();
            let a = c.create(1, (800, 600));
            let s = c.create(2, (420, 220));
            c.windows.get_mut(&a).unwrap().owner_uid = 1001;
            c.windows.get_mut(&s).unwrap().owner_uid = 65030;
            (a as u32, s as u32)
        };
        let screen = [0, 24, 1280, 720];
        let ms = 1_000_000u64;
        let t0 = 5000 * ms;
        let frame = |above: bool, t: u64| {
            let mut l = vec![(app, [0, 30, 800, 630]), (sheet, [430, 200, 850, 420])];
            if above {
                l.push((app, [0, 30, 800, 630]));
            }
            observe_frame_with(&comp, &l, screen, t, &[65030]);
        };
        frame(false, t0);
        frame(false, t0 + 600 * ms);
        assert!(visible_for(sheet, t0 + 600 * ms, t0 + 600 * ms));
        assert!(!visible_for(app, t0 + 600 * ms, t0 + 600 * ms), "an app window is not a sheet");
        // It is watched as a window, for Allow in Settings (APPROVE): the
        // sheet over it covers it.
        assert!(!window_visible_for(app, t0 + 600 * ms, t0 + 600 * ms), "the sheet covers it");
        assert!(window_visible_for(sheet, t0 + 600 * ms, t0 + 600 * ms));
        assert!(visible_for_purpose(fresco_protocol::activation::purpose::IDENTITY, sheet, t0 + 600 * ms, t0 + 600 * ms));
        frame(true, t0 + 700 * ms);
        assert!(!visible_for(sheet, t0 + 800 * ms, t0 + 800 * ms), "covered");
        // Not a sheet uid: never watched.
        observe_frame_with(&comp, &[(sheet, [430, 200, 850, 420])], screen, t0 + 2000 * ms, &[1]);
        observe_frame_with(&comp, &[(sheet, [430, 200, 850, 420])], screen, t0 + 3000 * ms, &[1]);
        assert!(!visible_for(sheet, t0 + 3000 * ms, t0 + 3000 * ms));
        // ... except as an ordinary window, which counts for APPROVE alone.
        assert!(window_visible_for(sheet, t0 + 3000 * ms, t0 + 3000 * ms));
        assert!(!visible_for_purpose(fresco_protocol::activation::purpose::IDENTITY, sheet, t0 + 3000 * ms, t0 + 3000 * ms));
        assert!(visible_for_purpose(fresco_protocol::activation::purpose::APPROVE, sheet, t0 + 3000 * ms, t0 + 3000 * ms));
    }

    #[test]
    fn a_windows_guard_is_watched_like_a_slot() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (ui, other) = {
            let mut c = comp.lock().unwrap();
            let ui = c.create(1, (1000, 700));
            let other = c.create(2, (300, 200));
            (ui, other)
        };
        // The row: 1000 x 40 at y 80 of the window.
        assert!(!comp.lock().unwrap().set_guard(ui, 2, Some([0, 80, 1000, 40])), "another client's window");
        assert!(comp.lock().unwrap().set_guard(ui, 1, Some([0, 80, 1000, 40])));
        let (ui, other) = (ui as u32, other as u32);
        let g = ui | GUARD;
        let screen = [0, 24, 1280, 800];
        let ms = 1_000_000u64;
        let t0 = 9000 * ms;
        let win = (ui, [0, 62, 1000, 762]);
        observe_frame(&comp, &[win], screen, t0);
        observe_frame(&comp, &[win], screen, t0 + 600 * ms);
        assert!(visible_for(g, t0 + 600 * ms, t0 + 600 * ms), "the row, clear for half a second");
        // Another program's window over the page, not the row: still clear.
        observe_frame(&comp, &[win, (other, [100, 300, 400, 500])], screen, t0 + 700 * ms);
        assert!(visible_for(g, t0 + 800 * ms, t0 + 800 * ms));
        // Over the row (y 142..182 on screen): not, and for half a second after.
        observe_frame(&comp, &[win, (other, [100, 150, 400, 350])], screen, t0 + 900 * ms);
        assert!(!visible_for(g, t0 + 1000 * ms, t0 + 1000 * ms), "covered");
        // A link pressed there: its guard is asked; another window has none.
        let link = fresco_protocol::activation::purpose::LINK;
        assert!(!visible_for_purpose(link, g, t0 + 1000 * ms, t0 + 1000 * ms), "a link from a covered row");
        assert!(visible_for_purpose(link, other | GUARD, t0 + 1000 * ms, t0 + 1000 * ms), "no guard, no look");
        observe_frame(&comp, &[win, (other, [100, 300, 400, 500])], screen, t0 + 1100 * ms);
        assert!(!visible_for(g, t0 + 1300 * ms, t0 + 1300 * ms), "just uncovered");
        assert!(visible_for(g, t0 + 1700 * ms, t0 + 1700 * ms));
        // Its window moved: a new run.
        observe_frame(&comp, &[(ui, [0, 70, 1000, 770])], screen, t0 + 1800 * ms);
        assert!(!visible_for(g, t0 + 1900 * ms, t0 + 1900 * ms), "moved");
        // Past the window's edge, or too small, or dropped: never.
        assert!(comp.lock().unwrap().set_guard(ui as u16, 1, Some([0, 690, 1000, 40])));
        observe_frame(&comp, &[(ui, [0, 70, 1000, 770])], screen, t0 + 3000 * ms);
        observe_frame(&comp, &[(ui, [0, 70, 1000, 770])], screen, t0 + 4000 * ms);
        assert!(!visible_for(g, t0 + 4000 * ms, t0 + 4000 * ms), "past the window");
        assert!(comp.lock().unwrap().set_guard(ui as u16, 1, Some([0, 80, 100, 10])));
        observe_frame(&comp, &[(ui, [0, 70, 1000, 770])], screen, t0 + 5000 * ms);
        observe_frame(&comp, &[(ui, [0, 70, 1000, 770])], screen, t0 + 6000 * ms);
        assert!(!visible_for(g, t0 + 6000 * ms, t0 + 6000 * ms), "too small");
        assert!(comp.lock().unwrap().set_guard(ui as u16, 1, None));
        observe_frame(&comp, &[(ui, [0, 70, 1000, 770])], screen, t0 + 7000 * ms);
        assert!(!visible_for(g, t0 + 7000 * ms, t0 + 7000 * ms), "no guard");
        assert!(!guarded(g), "its record went with it");
        assert!(!comp.lock().unwrap().set_guard(ui as u16, 1, Some([-1, 0, 100, 40])), "outside the window");
    }
}
