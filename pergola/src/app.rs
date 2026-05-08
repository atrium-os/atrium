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

use crate::layout;
use crate::node::{diff, NodeDelta, NodeId, NodeTree};
use crate::reactive::Mutable;
use crate::theme::Semantic;
use crate::view::{render, View};

pub struct App<V: View> {
    view: V,
    prev_tree: NodeTree,
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
        Self { view, prev_tree: NodeTree::new(), dirty, theme: Semantic::LIGHT }
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
        let mut next = render(&self.view, self.theme);
        // Run layout from each root in the tree (a render pass may
        // produce multiple top-level nodes; in practice it's one per
        // window).
        let roots: Vec<NodeId> = next.roots().collect();
        for root in roots {
            layout::layout(&mut next, root);
        }
        let deltas = diff(&self.prev_tree, &next);
        self.prev_tree = next;
        deltas
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
        let mut app = App {
            view: Square { fill: fill.clone() },
            prev_tree: NodeTree::new(),
            dirty,
            theme: Semantic::LIGHT,
        };

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
