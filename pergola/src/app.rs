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

use crate::event::{hit_test, Event};
use crate::interaction::Interactions;
use crate::layout;
use crate::node::{diff, NodeDelta, NodeId, NodeTree};
use crate::reactive::Mutable;
use crate::theme::Semantic;
use crate::view::{render, View};

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
    /// The node currently receiving keyboard input, if any.
    /// Pointer-down on a focusable node sets this; nothing else
    /// changes it yet (Tab navigation in a later phase).
    focused: Option<NodeId>,
    dirty: Arc<AtomicBool>,
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
            dirty,
            theme: Semantic::LIGHT,
        }
    }

    pub fn with_theme(mut self, theme: Semantic) -> Self {
        self.theme = theme;
        self
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
        let (mut next, interactions) = render(&self.view, self.theme);
        // Run layout from each root in the tree (a render pass may
        // produce multiple top-level nodes; in practice it's one per
        // window).
        let roots: Vec<NodeId> = next.roots().collect();
        for root in roots {
            layout::layout(&mut next, root);
        }
        let deltas = diff(&self.prev_tree, &next);
        self.prev_tree = next;
        self.interactions = interactions;
        deltas
    }

    /// The node currently receiving keyboard input, if any.
    pub fn focused(&self) -> Option<NodeId> { self.focused }

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

    pub fn handle_event(&mut self, event: Event) {
        match &event {
            Event::PointerDown { at } => {
                let hit = hit_test(&self.prev_tree, *at);
                // Bubble from the hit node up to the nearest interactive ancestor:
                // a click landing on a child (e.g. a button's centered label, or a
                // field's text) must still target the interactive parent that holds
                // the handler. Press-tracking uses the nearest on_click ancestor;
                // focus uses the nearest focusable ancestor.
                self.pending_press = hit.and_then(|id| self.bubble_to(id, |h| h.on_click.is_some()));
                self.focused = hit.and_then(|id| self.bubble_to(id, |h| h.focusable));
            }
            Event::PointerUp { at } => {
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
            }
            Event::PointerMove { .. } => {
                // Hover/drag: future phase
            }
            Event::Key { .. } => {
                if let Some(id) = self.focused {
                    if let Some(h) = self.interactions.get(id) {
                        if let Some(cb) = &h.on_key {
                            cb(&event);
                        }
                    }
                }
            }
        }
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
