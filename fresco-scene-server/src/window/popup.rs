//! Pop-up windows (docs/spec/fresco-popups.md): placed, stacked and
//! dismissed by frescod, not the WM.

use super::{Compositor, DisplayEvent, Popup, PopupGrant, PopupRefused, WindowId};

/// The gap frescod leaves between the bar and a panel an app hangs from
/// it by a grant, where frescod's label naming the app goes; points, at
/// the bar's scale.
pub const HOSTED_LABEL_GAP: f32 = 24.0;

/// The most grants waiting at once (one a bar window; a few bars).
const GRANTS_MAX: usize = 8;
use fresco_protocol::{popup_align, popup_dismiss, popup_side, POPUP_MAX_DEPTH};

/// Top-left of a `w` x `h` pop-up on `side` of anchor `a`, lined up by
/// `align`.
fn at(a: [f32; 4], side: u8, align: u8, w: f32, h: f32) -> (f32, f32) {
    let [ax, ay, aw, ah] = a;
    let along = |start: f32, len: f32, size: f32| match align {
        popup_align::CENTER => start + (len - size) / 2.0,
        popup_align::END => start + len - size,
        _ => start,
    };
    match side {
        popup_side::ABOVE => (along(ax, aw, w), ay - h),
        popup_side::RIGHT => (ax + aw, along(ay, ah, h)),
        popup_side::LEFT => (ax - w, along(ay, ah, h)),
        _ => (along(ax, aw, w), ay + ah),
    }
}

/// Where a pop-up of `want` hanging from screen rect `a` on `side`, lined
/// up by `align`, goes inside `bounds` (`None`: anywhere): its screen rect
/// and the side after any flip. It flips when the other side has more
/// room and the asked one is too small; then it slides along to stay
/// inside, and is never larger than the bounds.
pub fn place(a: [f32; 4], side: u8, align: u8, want: (u32, u32), bounds: Option<[f32; 4]>) -> ([f32; 4], u8) {
    let [ax, ay, aw, ah] = a;
    let (mut w, mut h) = (want.0.max(1) as f32, want.1.max(1) as f32);
    let side = if side > popup_side::LEFT { popup_side::BELOW } else { side };
    let Some([bx, by, bw, bh]) = bounds else {
        let (x, y) = at(a, side, align, w, h);
        return ([x, y, w, h], side);
    };
    w = w.min(bw).floor();
    h = h.min(bh).floor();
    let (r, b) = (bx + bw, by + bh);
    let (below, above, right, left) = (b - (ay + ah), ay - by, r - (ax + aw), ax - bx);
    let side = match side {
        popup_side::ABOVE if above < h && below > above => popup_side::BELOW,
        popup_side::RIGHT if right < w && left > right => popup_side::LEFT,
        popup_side::LEFT if left < w && right > left => popup_side::RIGHT,
        _ => side,
    };
    let side = if side == popup_side::BELOW && below < h && above > below { popup_side::ABOVE } else { side };
    let (x, y) = at(a, side, align, w, h);
    let x = x.clamp(bx, r - w);
    let y = y.clamp(by, b - h);
    ([x, y, w, h], side)
}

impl Compositor {
    /// The first window up `id`'s chain of parents that is not a pop-up.
    pub fn popup_root(&self, mut id: WindowId) -> WindowId {
        for _ in 0..=POPUP_MAX_DEPTH {
            match self.windows.get(&id).and_then(|w| w.popup) {
                Some(p) => id = p.parent,
                None => return id,
            }
        }
        id
    }

    /// How many pop-ups deep `id` is (0: not a pop-up).
    fn popup_depth(&self, mut id: WindowId) -> usize {
        let mut n = 0;
        while let Some(p) = self.windows.get(&id).and_then(|w| w.popup) {
            n += 1;
            id = p.parent;
            if n > POPUP_MAX_DEPTH {
                break;
            }
        }
        n
    }

    /// Whether pop-up `id` is on screen.
    fn popup_open(&self, id: WindowId) -> bool {
        self.windows.get(&id).is_some_and(|w| w.popup.is_some() && !w.hidden)
    }

    /// The checks every pop-up request passes (fresco-popups.md §2):
    /// `parent` is `owner`'s, on screen, and its chain's root is focused
    /// or just pressed; the anchor lies inside it; the side and size are
    /// sane and the chain not too deep.
    fn popup_ok(&self, owner: u32, parent: WindowId, anchor: [f32; 4], side: u8, align: u8, size: (u32, u32), extra_depth: usize) -> Result<(), PopupRefused> {
        let p = self.windows.get(&parent).filter(|w| parent != 0 && w.owner == owner).ok_or(PopupRefused::UnknownWindow)?;
        if p.hidden || !self.z_order.contains(&parent) {
            return Err(PopupRefused::NotActive);
        }
        let root = self.popup_root(parent);
        if self.popups_upstream {
            // frescod decides the rest.
        } else if self.focus != Some(root) && self.last_press != Some(root) && !self.popup_open(parent) {
            return Err(PopupRefused::NotActive);
        }
        if !self.popups_upstream && p.popup.is_some() && !self.popup_open(parent) {
            return Err(PopupRefused::NotActive);
        }
        // While a menu is open, only its chain opens more (§2): an app
        // under the bar's menu, still focused, cannot cover it.
        if !self.popups_upstream && self.windows.values().any(|w| {
            w.popup.is_some_and(|q| q.grab) && !w.hidden && self.popup_root(w.id) != root
        }) {
            return Err(PopupRefused::NotActive);
        }
        let [x, y, w, h] = anchor;
        let inside = anchor.iter().all(|v| v.is_finite())
            && w >= 0.0 && h >= 0.0 && x >= 0.0 && y >= 0.0
            && x + w <= p.size.0 + 0.5 && y + h <= p.size.1 + 0.5;
        let sane = side <= popup_side::LEFT && align <= popup_align::END
            && (1..=fresco_protocol::SPAN_MAX_DIM).contains(&size.0)
            && (1..=fresco_protocol::SPAN_MAX_DIM).contains(&size.1);
        let ok = inside && sane
            && self.popup_depth(parent) + extra_depth <= POPUP_MAX_DEPTH
            && p.slot.is_none() && !p.embed_pending
            && p.role != WmRole::Notification;
        if ok { Ok(()) } else { Err(PopupRefused::Invalid) }
    }

    /// `OP_POPUP_CREATE` from client `owner`: a new window, placed.
    #[allow(clippy::too_many_arguments)]
    pub fn open_popup(&mut self, owner: u32, parent: WindowId, anchor: [f32; 4], side: u8, align: u8, size: (u32, u32), grab: bool) -> Result<WindowId, PopupRefused> {
        self.popup_ok(owner, parent, anchor, side, align, size, 1)?;
        let id = self.create(owner, size);
        let w = self.windows.get_mut(&id).expect("just made");
        w.title = String::new();
        w.popup = Some(Popup { parent, anchor, side, align, want: size, grab, told: None, shown: 1, hosted: None });
        self.place_slots();
        Ok(id)
    }

    /// A split renderer's window `id` (made hidden, as a pop-up to be) is
    /// a pop-up of `parent`: checked as `OP_POPUP_CREATE` is, then shown
    /// and placed.
    #[allow(clippy::too_many_arguments)]
    pub fn attach_popup(&mut self, owner: u32, id: WindowId, parent: WindowId, anchor: [f32; 4], side: u8, align: u8, size: (u32, u32), grab: bool) -> Result<(), PopupRefused> {
        let w = self.windows.get(&id).filter(|w| w.owner == owner && id != parent).ok_or(PopupRefused::UnknownWindow)?;
        if let Some(p) = w.popup {
            if p.parent != parent || p.grab != grab || p.hosted.is_some() {
                return Err(PopupRefused::Invalid);
            }
            return self.replace_popup(owner, id, anchor, side, align, size);
        }
        if w.slot.is_some() || self.windows.values().any(|x| x.popup.is_some_and(|p| p.parent == id) || x.slot.is_some_and(|s| s.parent == id)) {
            return Err(PopupRefused::Invalid);
        }
        self.popup_ok(owner, parent, anchor, side, align, size, 1)?;
        let w = self.windows.get_mut(&id).expect("checked");
        w.popup = Some(Popup { parent, anchor, side, align, want: size, grab, told: None, shown: 1, hosted: None });
        w.hidden = false;
        w.embed_pending = false;
        if !self.z_order.contains(&id) {
            self.z_order.push(id);
        }
        self.place_slots();
        Ok(())
    }

    /// `OP_POPUP_PLACE`: place pop-up `id` again, showing it if it was
    /// dismissed.
    pub fn replace_popup(&mut self, owner: u32, id: WindowId, anchor: [f32; 4], side: u8, align: u8, size: (u32, u32)) -> Result<(), PopupRefused> {
        let p = self.windows.get(&id).filter(|w| w.owner == owner).and_then(|w| w.popup).ok_or(PopupRefused::UnknownWindow)?;
        if p.hosted.is_some() {
            return Err(PopupRefused::Invalid);
        }
        self.popup_ok(owner, p.parent, anchor, side, align, size, 0)?;
        let w = self.windows.get_mut(&id).expect("checked");
        w.popup = Some(Popup { anchor, side, align, want: size, shown: p.shown.wrapping_add(w.hidden as u32), ..p });
        if w.hidden {
            // Shown again (in a split renderer: asked to be; frescod says).
            w.hidden = false;
            self.z_order.push(id);
        }
        self.place_slots();
        Ok(())
    }

    /// `OP_POPUP_GRANT` from client `owner`: its chrome window `parent`
    /// lets the holder of `token` open one pop-up of at most `max` from
    /// `anchor`, checked now as `OP_POPUP_CREATE` is (the bar was just
    /// pressed). One grant a parent; it lasts `POPUP_GRANT_TTL_MS`. In a
    /// split renderer it is only recorded, for frescod.
    #[allow(clippy::too_many_arguments)]
    pub fn grant_popup(&mut self, owner: u32, parent: WindowId, anchor: [f32; 4], side: u8, align: u8, max: (u32, u32), token: [u8; 16]) -> Result<(), PopupRefused> {
        let p = self.windows.get(&parent).filter(|w| parent != 0 && w.owner == owner).ok_or(PopupRefused::UnknownWindow)?;
        // A split renderer does not know the roles frescod gave.
        if p.popup.is_some() || (!self.popups_upstream && p.role != WmRole::Chrome) {
            return Err(PopupRefused::Invalid);
        }
        self.popup_ok(owner, parent, anchor, side, align, max, 1)?;
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_millis(fresco_protocol::POPUP_GRANT_TTL_MS);
        self.popup_grants.retain(|g| g.parent != parent && now.duration_since(g.at) < ttl);
        if self.popup_grants.len() >= GRANTS_MAX {
            self.popup_grants.remove(0);
        }
        self.popup_grants.push(PopupGrant { token, owner, parent, anchor, side, align, max, at: now });
        Ok(())
    }

    /// A split renderer: the grants to send to frescod.
    pub fn take_popup_grants(&mut self) -> Vec<PopupGrant> {
        std::mem::take(&mut self.popup_grants)
    }

    /// The live grant `token` names, used up.
    fn take_grant(&mut self, token: &[u8; 16]) -> Result<PopupGrant, PopupRefused> {
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_millis(fresco_protocol::POPUP_GRANT_TTL_MS);
        self.popup_grants.retain(|g| now.duration_since(g.at) < ttl);
        let i = self.popup_grants.iter().position(|g| g.token == *token).ok_or(PopupRefused::UnknownWindow)?;
        Ok(self.popup_grants.remove(i))
    }

    /// `OP_POPUP_CLAIM` from client `owner`: a new window, the grabbing
    /// pop-up `token` grants, `size` (points, at the scale of the window
    /// the grant hangs from) cut to the grant's. The grant is
    /// checked again (its parent still on screen, still pressed or with
    /// its chain open). In a split renderer it is only recorded: frescod
    /// checks the token.
    pub fn claim_popup(&mut self, owner: u32, token: [u8; 16], size: (u32, u32)) -> Result<WindowId, PopupRefused> {
        if !(1..=fresco_protocol::SPAN_MAX_DIM).contains(&size.0) || !(1..=fresco_protocol::SPAN_MAX_DIM).contains(&size.1) {
            return Err(PopupRefused::Invalid);
        }
        if self.popups_upstream {
            let id = self.create(owner, size);
            let w = self.windows.get_mut(&id).expect("just made");
            w.title = String::new();
            w.popup = Some(Popup { parent: 0, anchor: [0.0; 4], side: 0, align: 0, want: size, grab: true, told: None, shown: 1, hosted: Some(token) });
            return Ok(id);
        }
        let g = self.take_grant(&token)?;
        let scale = self.windows.get(&g.parent).map_or(1000, |w| w.scale_milli);
        let size = claimed_size(size, scale, g.max);
        self.popup_ok(g.owner, g.parent, g.anchor, g.side, g.align, size, 1)?;
        let id = self.create(owner, size);
        let w = self.windows.get_mut(&id).expect("just made");
        w.scale_milli = scale;
        w.title = String::new();
        w.popup = Some(Popup { parent: g.parent, anchor: g.anchor, side: g.side, align: g.align, want: size, grab: true, told: None, shown: 1, hosted: Some(token) });
        self.place_slots();
        self.emit(DisplayEvent::PopupHosted { window_id: g.parent as u32, token, open: true });
        Ok(id)
    }

    /// frescod, for a split renderer: its window `id` (made hidden, as a
    /// pop-up to be) is the pop-up `token` grants: checked as
    /// `OP_POPUP_CLAIM` is (`size` in points), then shown and placed.
    pub fn attach_hosted(&mut self, owner: u32, id: WindowId, token: [u8; 16], size: (u32, u32)) -> Result<(), PopupRefused> {
        if !self.windows.get(&id).is_some_and(|w| w.owner == owner && w.popup.is_none() && w.slot.is_none()) {
            return Err(PopupRefused::UnknownWindow);
        }
        if self.windows.values().any(|x| x.popup.is_some_and(|p| p.parent == id) || x.slot.is_some_and(|s| s.parent == id)) {
            return Err(PopupRefused::Invalid);
        }
        let g = self.take_grant(&token)?;
        let scale = self.windows.get(&g.parent).map_or(1000, |w| w.scale_milli);
        let size = claimed_size(size, scale, g.max);
        self.popup_ok(g.owner, g.parent, g.anchor, g.side, g.align, size, 1)?;
        if self.windows[&id].scale_milli != scale {
            self.set_window_dpi(id, scale as f32 / 1000.0);
        }
        if self.windows[&id].size != (size.0 as f32, size.1 as f32) {
            self.resize_window(id, size.0, size.1);
        }
        let w = self.windows.get_mut(&id).expect("checked");
        w.popup = Some(Popup { parent: g.parent, anchor: g.anchor, side: g.side, align: g.align, want: size, grab: true, told: None, shown: 1, hosted: Some(token) });
        w.hidden = false;
        w.embed_pending = false;
        if !self.z_order.contains(&id) {
            self.z_order.push(id);
        }
        self.place_slots();
        self.emit(DisplayEvent::PopupHosted { window_id: g.parent as u32, token, open: true });
        Ok(())
    }

    /// The client whose grant opened pop-up `id`'s chain, and the window
    /// it hangs from, if a grant did (frescod labels it with the client).
    pub fn hosted_by(&self, mut id: WindowId) -> Option<WindowId> {
        for _ in 0..=POPUP_MAX_DEPTH {
            let p = self.windows.get(&id).and_then(|w| w.popup)?;
            if p.hosted.is_some() {
                return Some(id);
            }
            id = p.parent;
        }
        None
    }

    /// A split renderer: frescod dismissed pop-up `id`. It stays off
    /// screen here until the client places it again.
    pub fn popup_dismissed_upstream(&mut self, id: WindowId) {
        if let Some(w) = self.windows.get_mut(&id).filter(|w| w.popup.is_some()) {
            w.hidden = true;
            self.z_order.retain(|&z| z != id);
        }
    }

    /// Whether window `id` is the shell's own layer: the bar or dock
    /// (Chrome), a pop-up hanging from one, or a notification. These stack
    /// above every app (fresco-popups.md §2), so frescod's focus ring does
    /// not cross them: an open bar menu hides the ring under it.
    pub fn shell_layer(&self, id: WindowId) -> bool {
        self.windows.get(&id).is_some_and(|w| matches!(w.role, WmRole::Chrome | WmRole::Notification))
            || (self.windows.get(&id).is_some_and(|w| w.popup.is_some())
                && self.windows.get(&self.popup_root(id)).is_some_and(|w| w.role == WmRole::Chrome))
    }

    /// Whether pop-up `id`'s chain hangs from the shell's chrome (the
    /// Forum bar): it stacks above every app's pop-up and may reach the
    /// top of the screen. A chain an app opened with the bar's grant is
    /// not: it stays below the strip and the bar's own menus.
    fn popup_trusted(&self, id: WindowId) -> bool {
        self.hosted_by(id).is_none() && self.windows.get(&self.popup_root(id)).is_some_and(|w| w.role == WmRole::Chrome)
    }

    /// The monitor a screen point is on, from [`Compositor::popup_bounds`]
    /// (the first one if it is on none; `None` if none are known). An
    /// app's pop-up (`trusted` false) is kept below the trusted strip.
    fn bounds_at(&self, x: f32, y: f32, trusted: bool) -> Option<[f32; 4]> {
        let b = self.popup_bounds
            .iter()
            .find(|b| x >= b[0] && x < b[0] + b[2] && y >= b[1] && y < b[1] + b[3])
            .or(self.popup_bounds.first())
            .copied()?;
        if trusted || b[1] >= self.popup_top || b[3] <= self.popup_top - b[1] {
            return Some(b);
        }
        let cut = self.popup_top - b[1];
        Some([b[0], b[1] + cut, b[2], b[3] - cut])
    }

    /// Put every open pop-up where it hangs, above everything but the
    /// notifications, chains in order; dismiss those whose parent is off
    /// screen or gone. Tells each owner where its pop-up landed when that
    /// changed. Run by [`Compositor::place_slots`].
    pub(super) fn place_popups(&mut self) {
        if self.popups_upstream {
            return;
        }
        let mut open: Vec<(usize, WindowId)> = self
            .windows
            .iter()
            .filter(|(_, w)| w.popup.is_some() && !w.hidden)
            .map(|(&id, _)| (self.popup_depth(id), id))
            .collect();
        if open.is_empty() {
            return;
        }
        open.sort();
        let mut stacked = Vec::new();
        for (_, id) in open {
            let p = self.windows[&id].popup.expect("a pop-up");
            // Its parent is on screen: a window in the z-order, or a pop-up
            // placed just before it (taken out of the order meanwhile).
            let parent = self.windows.get(&p.parent).filter(|w| {
                !w.hidden && if w.popup.is_some() { stacked.contains(&p.parent) } else { self.z_order.contains(&p.parent) }
            });
            let Some(parent) = parent else {
                self.dismiss_popups(|_, w| w == id, popup_dismiss::PARENT);
                continue;
            };
            let ppos = parent.pos;
            // The label's gap is in points, at the parent's scale.
            let label_gap = HOSTED_LABEL_GAP * parent.scale_milli.max(1) as f32 / 1000.0;
            let mut a = [ppos.0 + p.anchor[0], ppos.1 + p.anchor[1], p.anchor[2], p.anchor[3]];
            if p.hosted.is_some() {
                // Room between the bar and the app's panel for frescod's
                // label naming the app (forum-status-items.md §4).
                a[1] -= label_gap;
                a[3] += 2.0 * label_gap;
            }
            let ([x, y, w, h], side) = place(a, p.side, p.align, p.want, self.bounds_at(a[0] + a[2] / 2.0, a[1] + a[3] / 2.0, self.popup_trusted(id)));
            let size = (w as u32, h as u32);
            if self.windows[&id].size != (w, h) {
                self.resize_window(id, size.0, size.1);
            }
            let told = (x - ppos.0, y - ppos.1, size.0, size.1, side);
            let win = self.windows.get_mut(&id).expect("open");
            win.pos = (x, y);
            let changed = win.popup.as_ref().and_then(|p| p.told) != Some(told);
            if let Some(pp) = win.popup.as_mut() {
                pp.told = Some(told);
            }
            if changed {
                self.emit(DisplayEvent::PopupPlaced { window_id: id as u32, x: told.0, y: told.1, width: told.2, height: told.3, side });
            }
            self.z_order.retain(|&z| z != id);
            stacked.push(id);
        }
        // Apps' windows, their pop-ups, those apps hang from the bar by
        // its grant, the shell's chrome (the bar) and its pop-ups, then
        // notifications: no app pixel covers the bar or
        // its menus (fresco-popups.md §2).
        let chrome = |c: &Self, id: WindowId| c.windows.get(&id).is_some_and(|w| w.role == WmRole::Chrome);
        let (bars, mut z): (Vec<WindowId>, Vec<WindowId>) = self.z_order.iter().partition(|&&id| chrome(self, id));
        let (trusted, apps): (Vec<WindowId>, Vec<WindowId>) = stacked.into_iter().partition(|&id| self.popup_trusted(id));
        let (hosted, apps): (Vec<WindowId>, Vec<WindowId>) = apps.into_iter().partition(|&id| self.hosted_by(id).is_some());
        z.extend(apps);
        z.extend(hosted);
        z.extend(bars);
        z.extend(trusted);
        self.z_order = z;
        self.layer_notifications();
        for (i, &id) in self.z_order.iter().enumerate() {
            if let Some(w) = self.windows.get_mut(&id) {
                w.z = i as u32;
            }
        }
    }

    /// Take off screen every open pop-up for which `pick(root, id)` holds,
    /// with all the pop-ups hanging from it, telling owners deepest first.
    pub(super) fn dismiss_popups(&mut self, pick: impl Fn(WindowId, WindowId) -> bool, reason: u8) {
        if self.popups_upstream {
            return;
        }
        let mut gone: Vec<(usize, WindowId)> = Vec::new();
        let open: Vec<WindowId> = self.windows.iter().filter(|(_, w)| w.popup.is_some() && !w.hidden).map(|(&id, _)| id).collect();
        for &id in &open {
            // It goes if it, or a pop-up it hangs from, was picked.
            let mut at = id;
            let mut hit = false;
            for _ in 0..=POPUP_MAX_DEPTH {
                if pick(self.popup_root(at), at) {
                    hit = true;
                    break;
                }
                match self.windows.get(&at).and_then(|w| w.popup) {
                    Some(p) => at = p.parent,
                    None => break,
                }
            }
            if hit {
                gone.push((self.popup_depth(id), id));
            }
        }
        gone.sort_by(|a, b| b.cmp(a));
        for (_, id) in gone {
            // Off screen, as `hide` does (not through it: it dismisses).
            let mut hosted = None;
            if let Some(w) = self.windows.get_mut(&id) {
                w.hidden = true;
                if let Some(p) = w.popup.as_mut() {
                    p.told = None;
                    hosted = p.hosted.map(|t| (p.parent, t));
                }
            }
            self.z_order.retain(|&z| z != id);
            if self.focus == Some(id) {
                self.set_focus_no_raise(None);
            }
            self.emit(DisplayEvent::PopupDismissed { window_id: id as u32, reason });
            if let Some((parent, token)) = hosted {
                self.emit(DisplayEvent::PopupHosted { window_id: parent as u32, token, open: false });
            }
        }
    }

    /// A press is going to window `target` (0: the background): every
    /// open grabbing pop-up it is not in goes, with what hangs from it.
    /// `true`: one went, so the press is not delivered (fresco-popups.md
    /// §1). Pop-ups that do not grab (tooltips) stay.
    pub fn press_dismisses_popups(&mut self, target: WindowId) -> bool {
        if self.popups_upstream {
            return false;
        }
        // The chain the press is in, if it is on one of its pop-ups.
        let inside = (target != 0 && self.popup_open(target)).then(|| self.popup_root(target));
        let hit: Vec<WindowId> = self.windows.values()
            .filter(|w| w.popup.is_some_and(|p| p.grab) && !w.hidden && Some(self.popup_root(w.id)) != inside)
            .map(|w| w.id).collect();
        if hit.is_empty() {
            return false;
        }
        self.dismiss_popups(|_, id| hit.contains(&id), popup_dismiss::PRESS_OUTSIDE);
        true
    }

    /// Focus moved to `to`: pop-ups of other chains go.
    pub(super) fn focus_dismisses_popups(&mut self, to: Option<WindowId>) {
        let root = to.map(|t| self.popup_root(t));
        self.dismiss_popups(|r, _| Some(r) != root, popup_dismiss::FOCUS);
    }

    /// The topmost open pop-up that takes the keys (grabs), if any.
    pub fn key_popup(&self) -> Option<WindowId> {
        self.z_order.iter().rev().copied().find(|id| self.windows.get(id).is_some_and(|w| !w.hidden && w.popup.is_some_and(|p| p.grab)))
    }
}

use fresco_protocol::WmRole;

/// A claimed pop-up's size: `size` points at `scale_milli`, in device
/// pixels, cut to the grant's `max` (device pixels).
fn claimed_size(size: (u32, u32), scale_milli: u32, max: (u32, u32)) -> (u32, u32) {
    let d = |v: u32| fresco_protocol::pt_to_device(v as f32, scale_milli);
    (d(size.0).min(max.0), d(size.1).min(max.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const B: Option<[f32; 4]> = Some([0.0, 0.0, 1000.0, 800.0]);

    #[test]
    fn places_flips_and_slides() {
        // Below a button, as asked.
        assert_eq!(place([100.0, 10.0, 50.0, 20.0], popup_side::BELOW, 0, (200, 300), B), ([100.0, 30.0, 200.0, 300.0], popup_side::BELOW));
        // No room below, more above: flips up.
        assert_eq!(place([100.0, 700.0, 50.0, 20.0], popup_side::BELOW, 0, (200, 300), B), ([100.0, 400.0, 200.0, 300.0], popup_side::ABOVE));
        // Near the right edge: slides left.
        assert_eq!(place([950.0, 10.0, 50.0, 20.0], popup_side::BELOW, 0, (200, 300), B).0, [800.0, 30.0, 200.0, 300.0]);
        // A submenu at the right edge opens left.
        assert_eq!(place([800.0, 100.0, 180.0, 20.0], popup_side::RIGHT, 0, (200, 300), B), ([600.0, 100.0, 200.0, 300.0], popup_side::LEFT));
        // Larger than the monitor: shrunk to it.
        assert_eq!(place([0.0, 0.0, 10.0, 10.0], popup_side::BELOW, 0, (2000, 2000), B).0, [0.0, 0.0, 1000.0, 800.0]);
        // A bad side is below.
        assert_eq!(place([0.0, 0.0, 10.0, 10.0], 9, 0, (10, 10), B).1, popup_side::BELOW);
        // Centred and end-aligned under a button.
        assert_eq!(place([100.0, 10.0, 50.0, 20.0], popup_side::BELOW, popup_align::CENTER, (150, 10), B).0[0], 50.0);
        assert_eq!(place([300.0, 10.0, 50.0, 20.0], popup_side::BELOW, popup_align::END, (150, 10), B).0[0], 200.0);
        // A context menu at the pointer: a zero-size anchor.
        assert_eq!(place([500.0, 400.0, 0.0, 0.0], popup_side::BELOW, 0, (100, 100), B).0, [500.0, 400.0, 100.0, 100.0]);
    }

    fn comp() -> (Compositor, mpsc::Receiver<DisplayEvent>) {
        let mut c = Compositor::new();
        let (tx, rx) = mpsc::channel();
        c.set_event_sink(tx);
        c.popup_bounds = vec![[0.0, 0.0, 1000.0, 800.0]];
        (c, rx)
    }

    fn popup_events(rx: &mpsc::Receiver<DisplayEvent>) -> Vec<DisplayEvent> {
        rx.try_iter().filter(|e| matches!(e, DisplayEvent::PopupPlaced { .. } | DisplayEvent::PopupDismissed { .. })).collect()
    }

    #[test]
    fn opens_from_the_focused_window_only_and_inside_it() {
        let (mut c, rx) = comp();
        let a = c.create(3, (400, 300));
        let other = c.create(4, (400, 300));
        c.windows.get_mut(&a).unwrap().pos = (100.0, 100.0);
        c.set_focus_no_raise(Some(other));
        let ask = |c: &mut Compositor, owner, parent| c.open_popup(owner, parent, [10.0, 10.0, 80.0, 20.0], popup_side::BELOW, 0, (150, 200), true);
        assert_eq!(ask(&mut c, 3, a), Err(PopupRefused::NotActive), "not focused, not pressed");
        assert_eq!(ask(&mut c, 3, other), Err(PopupRefused::UnknownWindow), "not its window");
        c.last_press = Some(a);
        let p = ask(&mut c, 3, a).expect("just pressed");
        assert_eq!(c.windows[&p].pos, (110.0, 130.0));
        assert_eq!(*c.z_order.last().unwrap(), p, "on top");
        let ev = popup_events(&rx);
        assert!(matches!(ev[..], [DisplayEvent::PopupPlaced { x: 10.0, y: 30.0, width: 150, height: 200, side: 0, .. }]), "{ev:?}");
        // An anchor outside the parent, or too deep a chain, is refused.
        assert_eq!(c.open_popup(3, a, [390.0, 10.0, 80.0, 20.0], popup_side::BELOW, 0, (10, 10), true), Err(PopupRefused::Invalid));
        let mut at = p;
        for _ in 1..POPUP_MAX_DEPTH {
            at = c.open_popup(3, at, [0.0, 0.0, 10.0, 10.0], popup_side::RIGHT, 0, (50, 50), true).expect("a submenu");
        }
        assert_eq!(c.open_popup(3, at, [0.0, 0.0, 10.0, 10.0], popup_side::RIGHT, 0, (50, 50), true), Err(PopupRefused::Invalid));
        // The WM does not see them.
        assert!(!c.windows.values().filter(|w| w.popup.is_none()).any(|w| w.id == p));
    }

    #[test]
    fn a_press_outside_dismisses_the_chain_and_is_swallowed() {
        let (mut c, rx) = comp();
        let a = c.create(3, (400, 300));
        c.set_focus_no_raise(Some(a));
        let menu = c.open_popup(3, a, [0.0, 0.0, 50.0, 20.0], popup_side::BELOW, 0, (100, 100), true).unwrap();
        let sub = c.open_popup(3, menu, [0.0, 10.0, 100.0, 20.0], popup_side::RIGHT, 0, (100, 100), true).unwrap();
        let tip = c.create(3, (1, 1)); // stands in for another window
        let _ = popup_events(&rx);
        assert_eq!(c.key_popup(), Some(sub), "keys to the submenu");
        assert_eq!(c.keyboard_window(), sub as u32);
        // A press inside the chain stays.
        assert!(!c.press_dismisses_popups(menu));
        assert!(popup_events(&rx).is_empty());
        // Outside (the parent itself): both go, the submenu first; swallowed.
        assert!(c.press_dismisses_popups(a));
        let ev = popup_events(&rx);
        assert_eq!(ev.len(), 2);
        assert!(matches!(ev[0], DisplayEvent::PopupDismissed { window_id, reason: popup_dismiss::PRESS_OUTSIDE } if window_id == sub as u32));
        assert!(matches!(ev[1], DisplayEvent::PopupDismissed { window_id, .. } if window_id == menu as u32));
        assert!(c.windows[&menu].hidden && !c.z_order.contains(&menu));
        assert_eq!(c.keyboard_window(), a as u32);
        // Nothing open: a press goes through.
        assert!(!c.press_dismisses_popups(tip));
        // Shown again where it was.
        c.replace_popup(3, menu, [0.0, 0.0, 50.0, 20.0], popup_side::BELOW, 0, (100, 100)).unwrap();
        assert!(!c.windows[&menu].hidden && matches!(popup_events(&rx)[..], [DisplayEvent::PopupPlaced { .. }]));
    }

    #[test]
    fn focus_moving_or_the_parent_going_dismisses_and_it_follows_its_parent() {
        let (mut c, rx) = comp();
        let a = c.create(3, (400, 300));
        let b = c.create(4, (400, 300));
        c.set_focus_no_raise(Some(a));
        let menu = c.open_popup(3, a, [0.0, 0.0, 50.0, 20.0], popup_side::BELOW, 0, (100, 100), true).unwrap();
        // The parent moves (the WM lays out again): the pop-up follows.
        c.windows.get_mut(&a).unwrap().pos = (200.0, 50.0);
        c.place_slots();
        assert_eq!(c.windows[&menu].pos, (200.0, 70.0));
        let _ = popup_events(&rx);
        // Focus to another app's window: it goes.
        c.set_focus_no_raise(Some(b));
        assert!(matches!(popup_events(&rx)[..], [DisplayEvent::PopupDismissed { reason: popup_dismiss::FOCUS, .. }]));
        // Open again from a; then a goes away: it goes.
        c.set_focus_no_raise(Some(a));
        c.replace_popup(3, menu, [0.0, 0.0, 50.0, 20.0], popup_side::BELOW, 0, (100, 100)).unwrap();
        let _ = popup_events(&rx);
        c.hide(a);
        c.place_slots();
        assert!(matches!(popup_events(&rx)[..], [DisplayEvent::PopupDismissed { reason: popup_dismiss::PARENT, .. }]));
        assert!(c.windows[&menu].hidden);
    }

    #[test]
    fn the_bars_menu_stays_above_apps_and_apps_stay_below_the_strip() {
        let (mut c, rx) = comp();
        c.popup_top = 24.0;
        let app = c.create(3, (1000, 776));
        c.windows.get_mut(&app).unwrap().pos = (0.0, 24.0);
        let bar = c.create(9, (1000, 30));
        c.windows.get_mut(&bar).unwrap().role = WmRole::Chrome;
        c.windows.get_mut(&bar).unwrap().pos = (0.0, 24.0);
        c.set_focus_no_raise(Some(app));
        // An app's menu that would open above its anchor near the top
        // flips below, never into the strip.
        let m = c.open_popup(3, app, [10.0, 0.0, 50.0, 20.0], popup_side::ABOVE, 0, (100, 100), true).unwrap();
        let y = c.windows[&m].pos.1;
        assert!(y >= 24.0, "app pop-up at {y}, inside the strip");
        // Its menu stacks below the bar.
        let zpos = |c: &Compositor, id| c.z_order.iter().position(|&z| z == id).unwrap();
        assert!(zpos(&c, m) < zpos(&c, bar));
        assert!(c.press_dismisses_popups(0));
        // The bar (pressed, never focused) opens its menu; the app, still
        // focused, cannot open one over it.
        c.last_press = Some(bar);
        let bm = c.open_popup(9, bar, [900.0, 0.0, 40.0, 30.0], popup_side::BELOW, 2, (200, 150), true).unwrap();
        assert_eq!(c.focus, Some(app));
        assert_eq!(c.open_popup(3, app, [10.0, 100.0, 50.0, 20.0], popup_side::BELOW, 0, (100, 100), true), Err(PopupRefused::NotActive));
        assert_eq!(*c.z_order.last().unwrap(), bm, "the bar's menu is on top");
        assert_eq!(c.keyboard_window(), bm as u32, "and takes the keys");
        let _ = popup_events(&rx);
    }

    #[test]
    fn an_app_hangs_its_panel_from_the_bars_grant_once() {
        let (mut c, rx) = comp();
        c.popup_top = 24.0;
        let app = c.create(3, (1000, 700));
        c.windows.get_mut(&app).unwrap().pos = (0.0, 62.0);
        let bar = c.create(9, (1000, 38));
        c.windows.get_mut(&bar).unwrap().pos = (0.0, 24.0);
        c.set_focus_no_raise(Some(app));
        let anchor = [900.0, 0.0, 30.0, 38.0];
        // Only the shell's chrome grants, and only when just pressed.
        assert_eq!(c.grant_popup(9, bar, anchor, popup_side::BELOW, popup_align::END, (300, 400), [1; 16]), Err(PopupRefused::Invalid));
        c.windows.get_mut(&bar).unwrap().role = WmRole::Chrome;
        assert_eq!(c.grant_popup(9, bar, anchor, popup_side::BELOW, popup_align::END, (300, 400), [1; 16]), Err(PopupRefused::NotActive));
        assert_eq!(c.grant_popup(3, bar, anchor, popup_side::BELOW, 0, (300, 400), [1; 16]), Err(PopupRefused::UnknownWindow));
        c.last_press = Some(bar);
        c.grant_popup(9, bar, anchor, popup_side::BELOW, popup_align::END, (300, 400), [1; 16]).unwrap();
        // A wrong token opens nothing; the right one, once, cut to the grant.
        assert_eq!(c.claim_popup(5, [2; 16], (300, 400)), Err(PopupRefused::UnknownWindow));
        let p = c.claim_popup(5, [1; 16], (500, 380)).unwrap();
        assert_eq!(c.claim_popup(5, [1; 16], (300, 400)), Err(PopupRefused::UnknownWindow), "used up");
        assert_eq!(c.windows[&p].owner, 5);
        assert_eq!(c.windows[&p].size, (300.0, 380.0));
        assert_eq!(c.windows[&p].pos, (630.0, 62.0 + HOSTED_LABEL_GAP), "under the icon, end-aligned, room for the label");
        let ev: Vec<DisplayEvent> = rx.try_iter().filter(|e| matches!(e, DisplayEvent::PopupHosted { .. })).collect();
        assert!(matches!(ev[..], [DisplayEvent::PopupHosted { window_id, token: [1, ..], open: true }] if window_id == bar as u32), "{ev:?}");
        // Above the app, below the bar; it takes the keys; not the shell's.
        let zpos = |c: &Compositor, id| c.z_order.iter().position(|&z| z == id).unwrap();
        assert!(zpos(&c, app) < zpos(&c, p) && zpos(&c, p) < zpos(&c, bar));
        assert_eq!(c.keyboard_window(), p as u32);
        assert!(!c.popup_trusted(p));
        assert_eq!(c.hosted_by(p), Some(p));
        // Its own submenu is the app's too.
        let sub = c.open_popup(5, p, [0.0, 10.0, 300.0, 20.0], popup_side::LEFT, 0, (100, 100), true).unwrap();
        assert_eq!(c.hosted_by(sub), Some(p));
        assert!(!c.popup_trusted(sub));
        // Never placed again.
        assert_eq!(c.replace_popup(5, p, anchor, popup_side::BELOW, 0, (300, 300)), Err(PopupRefused::Invalid));
        // A press outside closes it; the bar hears.
        let _ = rx.try_iter().count();
        assert!(c.press_dismisses_popups(app));
        let ev: Vec<DisplayEvent> = rx.try_iter().filter(|e| matches!(e, DisplayEvent::PopupHosted { .. })).collect();
        assert!(matches!(ev[..], [DisplayEvent::PopupHosted { token: [1, ..], open: false, .. }]), "{ev:?}");
        // A stale grant is refused.
        c.grant_popup(9, bar, anchor, popup_side::BELOW, 0, (300, 400), [3; 16]).unwrap();
        c.popup_grants[0].at -= std::time::Duration::from_millis(fresco_protocol::POPUP_GRANT_TTL_MS + 1);
        assert_eq!(c.claim_popup(5, [3; 16], (300, 400)), Err(PopupRefused::UnknownWindow));
    }
}
