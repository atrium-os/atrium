//! The lock screen (docs/spec/lock-screen.md §3).
//!
//! While locked, only windows whose owner uid is in the lock's list are on
//! screen: the lock screen's and Sigillum's sheets. Every other window is
//! taken out of the z-order and marked hidden, so it is not drawn, hit,
//! focused, raised or laid out (every path that shows a window acts only on
//! windows in the z-order, or refuses hidden ones), and its owner cannot
//! bring it back. An unresolved owner (uid 0) is never allowed. The check
//! runs after every change to the z-order, so a window created or shown
//! while locked never reaches the screen; unlocking puts the hidden ones
//! back in the order they had.

use super::{Compositor, DisplayEvent, WindowId};

impl Compositor {
    /// Lock to the windows of `uids` (`Some`), or unlock (`None`). Called
    /// by frescod as the seat's lock file says; repeating it is harmless.
    pub fn set_lock(&mut self, uids: Option<Vec<u32>>) {
        match uids {
            Some(u) => {
                self.lock = Some(u);
                self.apply_lock();
            }
            None => {
                if self.lock.take().is_none() {
                    return;
                }
                for id in std::mem::take(&mut self.lock_hidden) {
                    let Some(w) = self.windows.get_mut(&id) else { continue };
                    if !w.hidden {
                        continue;
                    }
                    w.hidden = false;
                    // A slot goes back above its parent (place_slots).
                    if w.slot.is_none() && !self.z_order.contains(&id) {
                        self.z_order.push(id);
                    }
                }
                self.layer_notifications();
                self.place_slots();
            }
        }
    }

    /// Whether the screen is locked.
    pub fn locked(&self) -> bool {
        self.lock.is_some()
    }

    /// Whether window `id` is kept off screen by the lock. The screen
    /// (window 0) never is; frescod draws it blank while locked.
    pub fn locked_out(&self, id: WindowId) -> bool {
        let Some(uids) = &self.lock else { return false };
        if id == 0 {
            return false;
        }
        !self.windows.get(&id).is_some_and(|w| w.owner_uid != 0 && uids.contains(&w.owner_uid))
    }

    /// Take every locked-out window off screen, and bring back those the
    /// lock hid that are allowed now (a window whose owner uid was not yet
    /// known when it was created).
    pub(super) fn apply_lock(&mut self) {
        if self.lock.is_none() {
            return;
        }
        let out: Vec<WindowId> = self.z_order.iter().copied().filter(|&id| self.locked_out(id)).collect();
        for id in out {
            self.z_order.retain(|&z| z != id);
            let w = self.windows.get_mut(&id).expect("in z_order");
            w.hidden = true;
            if w.focus {
                w.focus = false;
            }
            if !self.lock_hidden.contains(&id) {
                self.lock_hidden.push(id);
            }
            if self.focus == Some(id) {
                self.focus = None;
                self.emit(DisplayEvent::WindowFocusChanged { window_id: id as u32, gained: false, cause: fresco_protocol::focus_cause::UNKNOWN });
            }
        }
        let back: Vec<WindowId> = self.lock_hidden.iter().copied().filter(|&id| self.windows.contains_key(&id) && !self.locked_out(id)).collect();
        for id in back {
            self.lock_hidden.retain(|&h| h != id);
            let w = self.windows.get_mut(&id).expect("checked");
            if w.hidden && w.slot.is_none() {
                w.hidden = false;
                self.z_order.push(id);
            }
        }
        self.lock_hidden.retain(|id| self.windows.contains_key(id));
    }

    /// The keyboard while locked: the focused window if the lock allows
    /// it, else the topmost allowed one, else none (0, which frescod drops
    /// while locked rather than give the WM).
    pub(super) fn lock_keyboard_window(&self) -> u32 {
        if let Some(f) = self.focus.filter(|&f| f != 0 && self.z_order.contains(&f) && !self.locked_out(f)) {
            return f as u32;
        }
        self.z_order
            .iter()
            .rev()
            .find(|&&id| id != 0 && !self.locked_out(id) && self.windows.get(&id).is_some_and(|w| w.popup.is_none()))
            .copied()
            .unwrap_or(0) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows a (uid 1001, the user's app), b (1001), l (500, the lock
    /// screen); a has focus.
    fn setup() -> (Compositor, WindowId, WindowId, WindowId) {
        let mut c = Compositor::new();
        let a = c.create(1, (100, 100));
        let b = c.create(1, (100, 100));
        let l = c.create(2, (100, 100));
        c.windows.get_mut(&a).unwrap().owner_uid = 1001;
        c.windows.get_mut(&b).unwrap().owner_uid = 1001;
        c.windows.get_mut(&l).unwrap().owner_uid = 500;
        c.raise(a);
        (c, a, b, l)
    }

    #[test]
    fn locking_hides_the_session_and_unlocking_restores_its_order() {
        let (mut c, a, b, l) = setup();
        let before = c.z_order.clone();
        c.set_lock(Some(vec![500]));
        assert_eq!(c.z_order, vec![0, l]);
        assert!(c.windows[&a].hidden && c.windows[&b].hidden);
        assert_eq!(c.keyboard_window(), l as u32, "keys go to the lock screen");
        assert_eq!(c.hit_test(10.0, 10.0), Some(l));
        c.set_lock(None);
        assert_eq!(c.z_order.iter().filter(|&&id| id != l).copied().collect::<Vec<_>>(), before.into_iter().filter(|&id| id != l).collect::<Vec<_>>());
        assert!(!c.windows[&a].hidden && !c.windows[&b].hidden);
    }

    #[test]
    fn nothing_brings_a_session_window_back_while_locked() {
        let (mut c, a, b, l) = setup();
        c.set_lock(Some(vec![500]));
        assert!(c.raise(a).is_none());
        c.set_focus_no_raise(Some(b));
        assert_ne!(c.focus, Some(b));
        c.place_slots();
        assert!(!c.z_order.contains(&a) && !c.z_order.contains(&b));
        // A window created while locked, unknown owner then the user's: never shown.
        let n = c.create(1, (50, 50));
        assert!(!c.z_order.contains(&n));
        c.windows.get_mut(&n).unwrap().owner_uid = 1001;
        c.place_slots();
        assert!(!c.z_order.contains(&n));
        // A lock-screen window whose uid is learned after create is shown.
        let m = c.create(2, (50, 50));
        assert!(!c.z_order.contains(&m), "owner unknown yet");
        c.windows.get_mut(&m).unwrap().owner_uid = 500;
        c.set_lock(Some(vec![500]));
        assert!(c.z_order.contains(&m) && c.z_order.contains(&l));
        // No allowed window: keys go nowhere.
        let mut d = Compositor::new();
        let x = d.create(1, (10, 10));
        d.windows.get_mut(&x).unwrap().owner_uid = 1001;
        d.set_lock(Some(vec![500]));
        assert_eq!(d.keyboard_window(), 0);
        c.set_lock(None);
        assert!(c.z_order.contains(&n) && c.z_order.contains(&a));
    }

    #[test]
    fn a_window_closed_while_locked_is_forgotten() {
        let (mut c, a, _b, _l) = setup();
        c.set_lock(Some(vec![500]));
        c.destroy(a, 1);
        c.set_lock(None);
        assert!(!c.windows.contains_key(&a) && !c.z_order.contains(&a));
    }
}
