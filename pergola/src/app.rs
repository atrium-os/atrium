//! `App` — the lifecycle host that ties views, signals, and the
//! commit cycle together.
//!
//! The contract:
//!
//! 1. App owns a root `View` and a "previous" `NodeTree` (initially
//!    empty).
//! 2. Mutables created via `app.mutable(value)` are wired to the
//!    app's dirty flag.
//! 3. `app.tick()` checks dirty; if set, re-renders the View into a
//!    fresh tree, diffs vs the previous tree, swaps in the new tree,
//!    and returns the delta list.
//! 4. The first tick always renders (treats the empty previous tree
//!    as the baseline) and returns "everything as Added."
//!
//! This is the bottom of the loop. A future phase wires `tick()` into
//! a kqueue-driven event loop that wakes on input or timer ticks; for
//! phase 1 we drive it manually from tests and the example.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::event::{hit_test, Event, Key, KeyEventKind};
use crate::interaction::Interactions;
use crate::layout;
use crate::node::{diff, NodeDelta, NodeId, NodeTree};
use crate::reactive::Mutable;
use crate::theme::Semantic;
use crate::view::{render_continuing, InteractionState, View};

pub struct App<V: View> {
    view: V,
    prev_tree: NodeTree,
    /// The most recent render's interaction table. Used by
    /// `handle_event` to dispatch input. Rebuilt every `tick`.
    interactions: Interactions,
    /// Pointer-down's hit target; armed at PointerDown, fired at
    /// PointerUp if the up event lands on the same node. Drag
    /// detection (when up lands elsewhere) lives in a future phase.
    pending_press: Option<NodeId>,
    /// The node currently receiving keyboard input, if any. Pointer-
    /// down on a focusable node sets this; Tab / Shift-Tab cycle it
    /// through focusables in id order.
    focused: Option<NodeId>,
    /// Whether focus came from the keyboard, so its ring shows (visual
    /// language rev. 2 §14.1: pointer focus shows no ring).
    focus_visible: bool,
    /// The node following the held press (`Ctx::on_drag`), if any.
    dragging: Option<NodeId>,
    /// Where focus was before an autofocus layer took it, to go back to.
    focus_return: Option<NodeId>,
    /// The autofocus layer that is up (a dialog, an open menu): Tab stays
    /// inside it, and keys no one in it took go to it.
    modal: Option<NodeId>,
    /// When the pointer came onto `hovered`.
    hover_since: f64,
    /// The node whose tooltip is up.
    tip: Option<NodeId>,
    /// Until when a tooltip that just closed lets the next open at once.
    tip_warm_until: f64,
    /// Current hover target — nearest interactive ancestor under the
    /// pointer. Drives widgets' hover looks via `InteractionState`.
    hovered: Option<NodeId>,
    /// Interactive node currently held down (visual pressed state;
    /// distinct from `pending_press`, which is click bookkeeping).
    pressed_vis: Option<NodeId>,
    dirty: Arc<AtomicBool>,
    /// The next tick sends every node, not just the changed ones (the
    /// surface's scale changed, so every node's wire form did).
    resend: bool,
    /// App time, ms (the `Window` sets it from its clock; tests set it).
    now_ms: f64,
    /// The pointer's last position, for wheel events (which carry none).
    pointer: Option<crate::geom::Point>,
    /// Modifiers of the last key event (Ctrl + wheel zooms).
    modifiers: crate::event::Modifiers,
    /// Some `on_frame` handler still needs frames.
    animating: bool,
    pub theme: Semantic,
    /// Embed slots as last placed, and what changed since the binder last
    /// asked ([`crate::slot`]).
    slots: std::collections::HashMap<crate::slot::SlotKey, crate::geom::Rect>,
    slot_changes: Vec<(crate::slot::SlotKey, Option<crate::geom::Rect>)>,
}

impl<V: View> App<V> {
    /// Construct an app around a root view. The first `tick()` will
    /// render and emit Added-deltas for the entire tree.
    pub fn new(view: V) -> Self {
        Self::new_with_flag(view, Arc::new(AtomicBool::new(true)))
    }

    /// Construct an app reusing a pre-existing dirty flag — useful
    /// when `Mutable<T>` cells need to be created *before* the View
    /// (so the View can hold them by value).
    ///
    /// ```ignore
    /// let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    /// let count = Mutable::with_dirty(0, Arc::clone(&dirty));
    /// let view  = Counter { count: count.clone() };
    /// let app   = App::new_with_flag(view, dirty);
    /// ```
    pub fn new_with_flag(view: V, dirty: Arc<AtomicBool>) -> Self {
        Self {
            view,
            prev_tree: NodeTree::new(),
            interactions: Interactions::new(),
            pending_press: None,
            focused: None,
            focus_visible: false,
            dragging: None,
            focus_return: None,
            modal: None,
            hover_since: 0.0,
            tip: None,
            tip_warm_until: f64::NEG_INFINITY,
            hovered: None,
            pressed_vis: None,
            dirty,
            resend: false,
            now_ms: 0.0,
            pointer: None,
            modifiers: Default::default(),
            animating: false,
            theme: Semantic::LIGHT,
            slots: Default::default(),
            slot_changes: Vec::new(),
        }
    }

    pub fn with_theme(mut self, theme: Semantic) -> Self {
        self.theme = theme;
        self
    }

    /// Switch theme at runtime; marks dirty so the next tick
    /// re-renders everything in the other mode.
    pub fn set_theme(&mut self, theme: Semantic) {
        if self.theme != theme {
            self.theme = theme;
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Re-render and send every node on the next tick, as if it were
    /// the first (`Window` calls this when the DPI scale changes).
    pub fn resend_all(&mut self) {
        self.resend = true;
        self.dirty.store(true, Ordering::Release);
    }

    /// Whether keyboard focus shows its ring: the window arrived by
    /// keyboard (true) or by a click (false).
    pub fn set_focus_visible(&mut self, on: bool) {
        if self.focus_visible != on {
            self.focus_visible = on;
            self.touch();
        }
    }

    /// Set the app clock (ms). Input and frames are stamped with it.
    pub fn set_time(&mut self, ms: f64) { self.now_ms = ms; }
    pub fn time(&self) -> f64 { self.now_ms }

    /// Whether something is moving (a fling, an eased jump, a zoom
    /// settling): the loop should call [`App::frame`] every vsync.
    pub fn wants_frame(&self) -> bool { self.animating }

    /// When the loop should wake with no input (ms on the app clock): a
    /// tooltip waiting out its delay.
    pub fn wake_at(&self) -> Option<f64> {
        // State changed outside any event (a render asked for another, a
        // background result): paint now.
        if self.is_dirty() {
            return Some(self.now_ms);
        }
        self.tip_due()
    }

    /// When the hovered node's tooltip opens, if it is waiting to.
    fn tip_due(&self) -> Option<f64> {
        let h = self.hovered?;
        if self.tip == Some(h) || !self.interactions.get(h).is_some_and(|h| h.tooltip) {
            return None;
        }
        let at = if self.now_ms <= self.tip_warm_until { self.now_ms } else { self.hover_since + TOOLTIP_DELAY_MS };
        at.is_finite().then_some(at)
    }

    /// Open the tooltip whose delay has run out.
    fn run_timers(&mut self) {
        if self.tip_due().is_some_and(|at| at <= self.now_ms) {
            self.tip = self.hovered;
            self.touch();
        }
    }

    /// Close the tooltip (the pointer left, a press, a key). After a press
    /// or a key it stays shut until the pointer moves to another node.
    fn close_tip(&mut self, warm: bool) {
        if !warm {
            self.hover_since = f64::INFINITY;
        }
        if self.tip.take().is_some() {
            self.tip_warm_until = if warm { self.now_ms + TOOLTIP_WARM_MS } else { f64::NEG_INFINITY };
            self.touch();
        }
    }

    /// Step every `on_frame` handler at the current time. Their state
    /// changes mark the app dirty; the next tick renders them.
    pub fn frame(&mut self) {
        if !self.animating {
            return;
        }
        let t = self.now_ms;
        let mut still = false;
        for (id, h) in &self.interactions.by_node {
            if let (Some(cb), Some(node)) = (&h.on_frame, self.prev_tree.get(*id)) {
                still |= cb(t, node);
            }
        }
        self.animating = still;
    }

    /// Deliver `kind` to the scroll view at or above `id`. Returns whether
    /// one took it.
    fn scroll_to(&mut self, id: Option<NodeId>, kind: crate::interaction::ScrollKind) -> bool {
        let Some(target) = id.and_then(|id| self.bubble_to(id, |h| h.on_scroll.is_some())) else {
            return false;
        };
        if let (Some(h), Some(node)) = (self.interactions.get(target), self.prev_tree.get(target)) {
            if let Some(cb) = &h.on_scroll {
                cb(&crate::interaction::ScrollInput { kind, t: self.now_ms, node });
                // Input may have started motion: step frames until the
                // view says it has stopped.
                self.animating = true;
                return true;
            }
        }
        false
    }

    /// Mark the UI for a re-render (the window was resized).
    pub fn invalidate(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// A handle to the dirty flag. Pass to `Mutable::with_dirty` if
    /// you need to construct cells before the app exists. Most code
    /// uses [`App::mutable`] instead.
    pub fn dirty_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.dirty)
    }

    /// Construct a `Mutable<T>` wired to this app's dirty flag. Calls
    /// to `.set()` on the returned cell will cause the next `tick()`
    /// to re-render.
    pub fn mutable<T: 'static>(&self, value: T) -> Mutable<T> {
        Mutable::with_dirty(value, Arc::clone(&self.dirty))
    }

    /// Whether the app would re-render on the next tick.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Run one frame of the loop. If clean, returns an empty delta
    /// list and skips the render pass entirely. If dirty, renders,
    /// diffs, swaps trees, clears the flag, and returns the deltas.
    pub fn tick(&mut self) -> Vec<NodeDelta> {
        self.run_timers();
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        let state = InteractionState {
            hovered: self.hovered,
            pressed: self.pressed_vis,
            focused: self.focused,
            focus_visible: self.focus_visible,
            tip: self.tip,
        };
        let (mut next, interactions) = render_continuing(&self.view, self.theme, state, &mut self.prev_tree);
        // Run layout from each root in the tree (a render pass may
        // produce multiple top-level nodes; in practice it's one per
        // window).
        let roots: Vec<NodeId> = next.roots().collect();
        for root in roots {
            layout::layout(&mut next, root);
        }
        let deltas = if std::mem::take(&mut self.resend) {
            diff(&NodeTree::new(), &next)
        } else {
            diff(&self.prev_tree, &next)
        };
        self.follow_autofocus(&next, &interactions);
        self.prev_tree = next;
        self.interactions = interactions;
        self.place_slots();
        deltas
    }

    /// A layer that just appeared takes focus; when the focused node
    /// goes, focus goes back to where it was before the layer.
    fn follow_autofocus(&mut self, next: &NodeTree, interactions: &Interactions) {
        let fresh = interactions.by_node.iter()
            .filter(|(id, h)| h.autofocus && self.prev_tree.get(**id).is_none())
            .map(|(id, _)| *id)
            .max_by_key(|id| id.0);
        if let Some(id) = fresh {
            if self.focus_return.is_none() {
                self.focus_return = self.focused;
            }
            self.focused = Some(id);
            self.modal = Some(id);
            self.dirty.store(true, Ordering::Release);
        } else {
            if self.modal.is_some_and(|m| next.get(m).is_none()) {
                self.modal = None;
            }
            if let Some(f) = self.focused {
                if next.get(f).is_none() {
                    self.focused = self.focus_return.take().filter(|r| next.get(*r).is_some());
                    self.dirty.store(true, Ordering::Release);
                }
            }
        }
    }

    /// The root `id` sits under.
    fn root_of(&self, mut id: NodeId) -> NodeId {
        while let Some(p) = self.prev_tree.parent_of(id) {
            id = p;
        }
        id
    }

    /// Whether `id` belongs to the modal layer: everything emitted from
    /// the layer's root on (a layer is drawn after the content under it).
    fn in_modal(&self, id: NodeId) -> bool {
        let Some(m) = self.modal else { return true };
        let roots: Vec<NodeId> = self.prev_tree.roots().collect();
        let at = |r: NodeId| roots.iter().position(|&x| x == r);
        match (at(self.root_of(m)), at(self.root_of(id))) {
            (Some(layer), Some(node)) => node >= layer,
            _ => true,
        }
    }

    /// Diff the slots' window rects against the last placement.
    fn place_slots(&mut self) {
        let mut now = std::collections::HashMap::new();
        for (id, h) in &self.interactions.by_node {
            if let Some(key) = h.slot {
                if let Some(r) = crate::slot::window_rect(&self.prev_tree, *id) {
                    now.insert(key, r);
                }
            }
        }
        for (k, r) in &now {
            if self.slots.get(k) != Some(r) {
                self.slot_changes.push((*k, Some(*r)));
            }
        }
        for k in self.slots.keys() {
            if !now.contains_key(k) {
                self.slot_changes.push((*k, None));
            }
        }
        self.slots = now;
    }

    /// Slot placements since the last call: `Some(rect)` in window
    /// coordinates (logical px) to place or move, `None` to release. The
    /// `Window` hands these to its slot binder.
    pub fn take_slot_changes(&mut self) -> Vec<(crate::slot::SlotKey, Option<crate::geom::Rect>)> {
        std::mem::take(&mut self.slot_changes)
    }

    /// The node currently receiving keyboard input, if any.
    pub fn focused(&self) -> Option<NodeId> { self.focused }

    /// The tree as last rendered and laid out.
    pub fn tree(&self) -> &NodeTree { &self.prev_tree }

    /// Dispatch an input event. Pointer events are hit-tested against
    /// the current tree; matching handlers fire on `PointerUp` when
    /// the up matches the same node as the prior `PointerDown`. Key
    /// events go to the currently-focused node's `on_key` handler.
    /// Walk from `id` up the parent chain (including `id` itself) to the nearest
    /// node whose `Handlers` satisfy `pred`. This is event bubbling: a pointer hit
    /// on a non-interactive child resolves to the interactive ancestor that owns
    /// the handler (a button's label → the button; a field's text → the field).
    fn bubble_to<P: Fn(&crate::interaction::Handlers) -> bool>(
        &self, mut id: NodeId, pred: P,
    ) -> Option<NodeId> {
        loop {
            if let Some(h) = self.interactions.get(id) {
                if pred(h) { return Some(id); }
            }
            id = self.prev_tree.parent_of(id)?;
        }
    }

    /// Mark the UI dirty (state-dependent looks need a re-render).
    fn touch(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Focusable node ids in id order — the Tab traversal sequence.
    fn focusables(&self) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = self
            .interactions
            .by_node
            .iter()
            .filter(|(id, h)| h.focusable && self.in_modal(**id))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }

    /// Advance focus to the next/previous focusable (Tab / Shift-Tab),
    /// wrapping. No-op when nothing is focusable.
    fn cycle_focus(&mut self, backwards: bool) {
        let ids = self.focusables();
        if ids.is_empty() {
            return;
        }
        let next = match self.focused.and_then(|f| ids.iter().position(|&i| i == f)) {
            Some(pos) => {
                let n = ids.len() as isize;
                let step = if backwards { -1 } else { 1 };
                ids[((pos as isize + step).rem_euclid(n)) as usize]
            }
            None => {
                if backwards { *ids.last().unwrap() } else { ids[0] }
            }
        };
        if self.focused != Some(next) || !self.focus_visible {
            self.focused = Some(next);
            self.focus_visible = true;
            self.touch();
        }
    }

    /// `handle_event` for a press frescod minted `activation` for: the
    /// handlers it reaches can spend it on a copy or paste
    /// ([`crate::clipboard`]).
    pub fn handle_event_activated(&mut self, event: Event, activation: Option<crate::clipboard::Token>) {
        crate::clipboard::with_activation(activation, || self.handle_event(event));
    }

    pub fn handle_event(&mut self, event: Event) {
        match &event {
            Event::PointerDown { at } => {
                self.pointer = Some(*at);
                self.close_tip(false);
                let hit = hit_test(&self.prev_tree, *at);
                // A press inside a scroll view catches its fling.
                self.scroll_to(hit, crate::interaction::ScrollKind::Press);
                // Bubble from the hit node up to the nearest interactive ancestor:
                // a click landing on a child (e.g. a button's centered label, or a
                // field's text) must still target the interactive parent that holds
                // the handler. Press-tracking uses the nearest on_click ancestor;
                // focus uses the nearest focusable ancestor.
                self.pending_press = hit.and_then(|id| self.bubble_to(id, |h| h.on_click.is_some()));
                self.dragging = hit.and_then(|id| self.bubble_to(id, |h| h.on_drag.is_some()));
                if let Some(cb) = self.dragging.and_then(|d| self.interactions.get(d)).and_then(|h| h.on_drag.as_ref()) {
                    cb(crate::interaction::DragPhase::Start, *at);
                }
                let new_focus = hit.and_then(|id| self.bubble_to(id, |h| h.focusable));
                if self.focused != new_focus || self.focus_visible {
                    self.focused = new_focus;
                    self.focus_visible = false;
                    self.touch();
                }
                if self.pressed_vis != self.pending_press {
                    self.pressed_vis = self.pending_press;
                    self.touch();
                }
            }
            Event::PointerUp { at } => {
                self.pointer = Some(*at);
                if let Some(d) = self.dragging.take() {
                    if let Some(cb) = self.interactions.get(d).and_then(|h| h.on_drag.as_ref()) {
                        cb(crate::interaction::DragPhase::End, *at);
                    }
                }
                let target = hit_test(&self.prev_tree, *at)
                    .and_then(|id| self.bubble_to(id, |h| h.on_click.is_some()));
                if let (Some(pressed), Some(up)) = (self.pending_press, target) {
                    if pressed == up {
                        if let Some(h) = self.interactions.get(pressed) {
                            if let Some(cb) = &h.on_click {
                                cb();
                            }
                        }
                    }
                }
                self.pending_press = None;
                if self.pressed_vis.take().is_some() {
                    self.touch();
                }
            }
            Event::PointerMove { at } => {
                self.pointer = Some(*at);
                if let Some(cb) = self.dragging.and_then(|d| self.interactions.get(d)).and_then(|h| h.on_drag.as_ref()) {
                    cb(crate::interaction::DragPhase::Move, *at);
                }
                // Hover target = nearest interactive ancestor (clickable
                // or focusable) under the pointer.
                let hover = hit_test(&self.prev_tree, *at)
                    .and_then(|id| self.bubble_to(id, |h| h.on_click.is_some() || h.focusable || h.tooltip));
                if self.hovered != hover {
                    self.hovered = hover;
                    self.hover_since = self.now_ms;
                    self.close_tip(true);
                    self.touch();
                }
            }
            Event::Scroll { at, dx, dy, modifiers } => {
                let at = at.or(self.pointer);
                let mods = if *modifiers == Default::default() { self.modifiers } else { *modifiers };
                let hit = at.and_then(|p| hit_test(&self.prev_tree, p));
                let kind = crate::interaction::ScrollKind::Wheel { at, dx: *dx, dy: *dy, modifiers: mods };
                self.scroll_to(hit, kind);
            }
            Event::Drop { at } => {
                self.pointer = Some(*at);
                let hit = hit_test(&self.prev_tree, *at);
                if let Some(id) = hit.and_then(|id| self.bubble_to(id, |h| h.on_drop.is_some())) {
                    if let Some(cb) = self.interactions.get(id).and_then(|h| h.on_drop.as_ref()) {
                        cb();
                    }
                }
            }
            Event::Key { kind, key, modifiers, chars } => {
                self.modifiers = *modifiers;
                if *kind == KeyEventKind::Down {
                    self.close_tip(false);
                }
                // Tab is focus traversal, owned by the App; focused
                // widgets never see it.
                if *kind == KeyEventKind::Down && *key == Key::Tab {
                    self.cycle_focus(modifiers.shift);
                    return;
                }
                let mut taken = false;
                if let Some(id) = self.focused {
                    let h = self.interactions.get(id);
                    let press = *kind == KeyEventKind::Down
                        && (*key == Key::Enter || (*key == Key::Char && chars == " "))
                        && !modifiers.ctrl && !modifiers.meta;
                    // Space or Enter press a focused control that has no key
                    // handler of its own (a button; §14.3).
                    if let Some(cb) = h.filter(|h| press && h.on_key.is_none()).and_then(|h| h.on_click.as_ref()) {
                        cb();
                        taken = true;
                    } else if let Some(t) = self.bubble_to(id, |h| h.on_key.is_some()) {
                        // Otherwise the key goes to the nearest handler up the
                        // tree: a dialog hears Escape from its focused button.
                        if let Some(cb) = self.interactions.get(t).and_then(|h| h.on_key.as_ref()) {
                            cb(&event);
                            taken = true;
                        }
                    }
                }
                // A key no one in the layer took goes to the layer itself: a
                // dialog hears Escape from the button that has focus.
                if !taken {
                    if let Some(cb) = self.modal.filter(|m| Some(*m) != self.focused).and_then(|m| self.interactions.get(m)).and_then(|h| h.on_key.as_ref()) {
                        cb(&event);
                        taken = true;
                    }
                }
                // Navigation keys no focused widget took scroll the view
                // that has focus inside it, else the one under the pointer.
                if !taken && *kind == KeyEventKind::Down && is_scroll_key(*key, chars, modifiers) {
                    let k = crate::interaction::ScrollKind::Key {
                        key: *key,
                        chars: chars.chars().next().unwrap_or('\0'),
                        modifiers: *modifiers,
                    };
                    let under = self.pointer.and_then(|p| hit_test(&self.prev_tree, p));
                    if !self.scroll_to(self.focused, k) {
                        self.scroll_to(under, k);
                    }
                }
            }
        }
    }
}

/// Keys a scroll view acts on: arrows, Page Up/Down, Home/End, Space,
/// and Ctrl with `+`, `=`, `-`, `0`.
/// Hover this long before a tooltip opens (visual language §14.2).
pub const TOOLTIP_DELAY_MS: f64 = 500.0;
/// After one closes, the next opens at once for this long.
pub const TOOLTIP_WARM_MS: f64 = 300.0;

fn is_scroll_key(key: Key, chars: &str, m: &crate::event::Modifiers) -> bool {
    match key {
        Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End => true,
        Key::Char if chars == " " => true,
        Key::Char if m.ctrl => matches!(chars, "+" | "=" | "-" | "0"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::geom::Rect;
    use crate::node::Node;
    use crate::view::Ctx;

    struct Square { fill: Mutable<Color> }

    impl View for Square {
        fn render(&self, ctx: &mut Ctx) {
            ctx.add(Node::Rect {
                rect: Rect::new(0.0, 0.0, 16.0, 16.0),
                fill: self.fill.get_cloned(),
                radius: 0.0,
            });
        }
    }

    /// A clickable parent whose interactive handler sits on the outer node, with
    /// a non-interactive child covering the center — the button/label shape.
    struct ClickParent { fired: std::sync::Arc<AtomicBool> }
    impl View for ClickParent {
        fn render(&self, ctx: &mut Ctx) {
            let bg = ctx.tree.insert(None, Node::Rect {
                rect: Rect::new(0.0, 0.0, 100.0, 40.0),
                fill: Color::rgba(0.0, 0.0, 0.0, 1.0), radius: 0.0,
            });
            // Child label covering the center — a naive hit-test returns THIS.
            ctx.tree.insert(Some(bg), Node::Rect {
                rect: Rect::new(30.0, 12.0, 40.0, 16.0),
                fill: Color::rgba(1.0, 1.0, 1.0, 1.0), radius: 0.0,
            });
            let fired = self.fired.clone();
            ctx.on_click(bg, move || fired.store(true, Ordering::SeqCst));
        }
    }

    #[test]
    fn click_on_child_bubbles_to_parent_on_click() {
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let mut app = App::new(ClickParent { fired: fired.clone() });
        app.tick(); // render → prev_tree + interactions
        // (50,20) is inside the child rect (30..70, 12..28) — which has no handler;
        // the press must bubble to the parent's on_click. Regression: previously
        // hit_test returned the child and the click was silently dropped.
        app.handle_event(Event::PointerDown { at: crate::geom::Point::new(50.0, 20.0) });
        app.handle_event(Event::PointerUp { at: crate::geom::Point::new(50.0, 20.0) });
        assert!(fired.load(Ordering::SeqCst), "on_click must fire via bubbling from a child hit");
    }

    /// Two focusable rects side by side — the Tab-cycling shape.
    struct TwoFields;
    impl View for TwoFields {
        fn render(&self, ctx: &mut Ctx) {
            for i in 0..2 {
                let id = ctx.tree.insert(None, Node::Rect {
                    rect: Rect::new(i as f32 * 60.0, 0.0, 50.0, 20.0),
                    fill: Color::rgba(0.5, 0.5, 0.5, 1.0), radius: 0.0,
                });
                ctx.focusable(id);
            }
        }
    }

    #[test]
    fn hover_change_marks_dirty_and_reaches_render_state() {
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let mut app = App::new(ClickParent { fired });
        app.tick();
        assert!(!app.is_dirty());
        // Move onto the interactive parent: dirty, hovered set.
        app.handle_event(Event::PointerMove { at: crate::geom::Point::new(50.0, 20.0) });
        assert!(app.is_dirty());
        app.tick();
        // Move within the same target: no spurious dirty.
        app.handle_event(Event::PointerMove { at: crate::geom::Point::new(60.0, 20.0) });
        assert!(!app.is_dirty());
        // Move off: dirty again, hover cleared.
        app.handle_event(Event::PointerMove { at: crate::geom::Point::new(500.0, 500.0) });
        assert!(app.is_dirty());
    }

    #[test]
    fn tab_cycles_focus_and_shift_tab_reverses() {
        let mut app = App::new(TwoFields);
        app.tick();
        let tab = |shift| Event::Key {
            kind: crate::event::KeyEventKind::Down,
            key: crate::event::Key::Tab,
            modifiers: crate::event::Modifiers { shift, ..Default::default() },
            chars: String::new(),
        };
        assert_eq!(app.focused(), None);
        app.handle_event(tab(false));
        let first = app.focused().expect("tab focuses first focusable");
        app.handle_event(tab(false));
        let second = app.focused().unwrap();
        assert_ne!(first, second);
        app.handle_event(tab(false));
        assert_eq!(app.focused(), Some(first), "wraps around");
        app.handle_event(tab(true));
        assert_eq!(app.focused(), Some(second), "shift-tab reverses");
    }

    #[test]
    fn only_keyboard_focus_shows_the_ring() {
        let mut app = App::new(TwoFields);
        app.tick();
        app.handle_event(Event::PointerDown { at: crate::geom::Point::new(10.0, 10.0) });
        assert!(app.focused().is_some());
        assert!(!app.focus_visible, "a click focuses without a ring");
        app.handle_event(Event::Key {
            kind: crate::event::KeyEventKind::Down,
            key: crate::event::Key::Tab,
            modifiers: Default::default(),
            chars: String::new(),
        });
        assert!(app.focus_visible, "Tab shows it");
        app.handle_event(Event::PointerDown { at: crate::geom::Point::new(10.0, 10.0) });
        assert!(!app.focus_visible, "and the pointer hides it again");
    }

    #[test]
    fn first_tick_renders_everything() {
        let mut app = App::new(Square { fill: Mutable::new(Color::rgba(1.0, 0.0, 0.0, 1.0)) });
        let d = app.tick();
        assert_eq!(d.len(), 1);
        assert!(matches!(d[0], NodeDelta::Added { .. }));
    }

    #[test]
    fn second_tick_with_no_changes_emits_nothing() {
        let red = Color::rgba(1.0, 0.0, 0.0, 1.0);
        let mut app = App::new(Square { fill: Mutable::new(red) });
        let _ = app.tick();
        let d = app.tick();
        // No signal was set; dirty flag is clear; no work done.
        assert!(d.is_empty());
    }

    #[test]
    fn signal_change_marks_dirty_and_emits_changed() {
        let red = Color::rgba(1.0, 0.0, 0.0, 1.0);
        let blue = Color::rgba(0.0, 0.0, 1.0, 1.0);

        // Pre-build the cell with the app's dirty flag.
        let dirty = Arc::new(AtomicBool::new(true));
        let fill = Mutable::with_dirty(red, Arc::clone(&dirty));
        let mut app = App::new_with_flag(Square { fill: fill.clone() }, dirty);

        // First tick: Added.
        let d = app.tick();
        assert_eq!(d.len(), 1);
        assert!(matches!(d[0], NodeDelta::Added { .. }));

        // No change: clean.
        assert!(!app.is_dirty());

        // Mutate signal: should trip dirty.
        fill.set(blue);
        assert!(app.is_dirty());

        // Tick: Changed.
        let d = app.tick();
        assert_eq!(d.len(), 1);
        assert!(matches!(d[0], NodeDelta::Changed { .. }));

        // Now clean again.
        assert!(!app.is_dirty());
    }

    struct Tipped;
    impl View for Tipped {
        fn render(&self, ctx: &mut Ctx) {
            crate::widgets::Button::ghost("A").at(0.0, 0.0).width(40.0).tooltip("First").on_click(|| {}).render(ctx);
            crate::widgets::Button::ghost("B").at(100.0, 0.0).width(40.0).tooltip("Second").on_click(|| {}).render(ctx);
        }
    }

    fn tip_text(app: &App<Tipped>) -> Option<String> {
        app.tree().iter().find_map(|(_, n)| match n {
            Node::Text { content, .. } if content == "First" || content == "Second" => Some(content.clone()),
            _ => None,
        })
    }

    #[test]
    fn a_tooltip_waits_for_the_hover_delay_and_moves_on_at_once() {
        let mut app = App::new(Tipped);
        app.tick();
        app.set_time(1000.0);
        app.handle_event(Event::PointerMove { at: crate::geom::Point::new(20.0, 10.0) });
        app.tick();
        assert_eq!(tip_text(&app), None, "not yet");
        assert_eq!(app.wake_at(), Some(1500.0), "the loop wakes for it");
        app.set_time(1499.0);
        app.tick();
        assert_eq!(tip_text(&app), None);
        app.set_time(1500.0);
        app.tick();
        assert_eq!(tip_text(&app).as_deref(), Some("First"));
        // Straight to the next one: no second wait.
        app.set_time(1600.0);
        app.handle_event(Event::PointerMove { at: crate::geom::Point::new(120.0, 10.0) });
        app.tick();
        assert_eq!(tip_text(&app).as_deref(), Some("Second"));
        // A press closes it, and it stays shut while the pointer stays.
        app.handle_event(Event::PointerDown { at: crate::geom::Point::new(120.0, 10.0) });
        app.handle_event(Event::PointerUp { at: crate::geom::Point::new(120.0, 10.0) });
        app.set_time(5000.0);
        app.tick();
        assert_eq!(tip_text(&app), None);
        assert_eq!(app.wake_at(), None);
    }
}
