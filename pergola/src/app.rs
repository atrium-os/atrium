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
            hovered: None,
            pressed_vis: None,
            dirty,
            resend: false,
            now_ms: 0.0,
            pointer: None,
            modifiers: Default::default(),
            animating: false,
            theme: Semantic::LIGHT,
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

    /// Set the app clock (ms). Input and frames are stamped with it.
    pub fn set_time(&mut self, ms: f64) { self.now_ms = ms; }
    pub fn time(&self) -> f64 { self.now_ms }

    /// Whether something is moving (a fling, an eased jump, a zoom
    /// settling): the loop should call [`App::frame`] every vsync.
    pub fn wants_frame(&self) -> bool { self.animating }

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
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        let state = InteractionState {
            hovered: self.hovered,
            pressed: self.pressed_vis,
            focused: self.focused,
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
        self.prev_tree = next;
        self.interactions = interactions;
        deltas
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
            .filter(|(_, h)| h.focusable)
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
        if self.focused != Some(next) {
            self.focused = Some(next);
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
                let hit = hit_test(&self.prev_tree, *at);
                // A press inside a scroll view catches its fling.
                self.scroll_to(hit, crate::interaction::ScrollKind::Press);
                // Bubble from the hit node up to the nearest interactive ancestor:
                // a click landing on a child (e.g. a button's centered label, or a
                // field's text) must still target the interactive parent that holds
                // the handler. Press-tracking uses the nearest on_click ancestor;
                // focus uses the nearest focusable ancestor.
                self.pending_press = hit.and_then(|id| self.bubble_to(id, |h| h.on_click.is_some()));
                let new_focus = hit.and_then(|id| self.bubble_to(id, |h| h.focusable));
                if self.focused != new_focus {
                    self.focused = new_focus;
                    self.touch();
                }
                if self.pressed_vis != self.pending_press {
                    self.pressed_vis = self.pending_press;
                    self.touch();
                }
            }
            Event::PointerUp { at } => {
                self.pointer = Some(*at);
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
                // Hover target = nearest interactive ancestor (clickable
                // or focusable) under the pointer.
                let hover = hit_test(&self.prev_tree, *at)
                    .and_then(|id| self.bubble_to(id, |h| h.on_click.is_some() || h.focusable));
                if self.hovered != hover {
                    self.hovered = hover;
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
            Event::Key { kind, key, modifiers, chars } => {
                self.modifiers = *modifiers;
                // Tab is focus traversal, owned by the App; focused
                // widgets never see it.
                if *kind == KeyEventKind::Down && *key == Key::Tab {
                    self.cycle_focus(modifiers.shift);
                    return;
                }
                let mut taken = false;
                if let Some(id) = self.focused {
                    if let Some(h) = self.interactions.get(id) {
                        if let Some(cb) = &h.on_key {
                            cb(&event);
                            taken = true;
                        }
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
}
