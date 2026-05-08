//! The `View` trait — Pergola's compositional core.
//!
//! A `View` produces nodes during a render pass. Pergola owns the
//! resulting `NodeTree`, persists it across frames (retained mode), and
//! diffs it on `commit` to emit wire deltas to fresco-server.
//!
//! Phase 0 keeps the contract minimal:
//!
//! ```ignore
//! trait View {
//!     fn render(&self, ctx: &mut Ctx);
//! }
//! ```
//!
//! Phase 1 will add typed state (Xilem-style `View<State>`) once
//! `futures-signals` is wired in. The current shape is a faithful
//! subset that can be evolved without breaking existing code.

use crate::interaction::{ClickHandler, Interactions};
use crate::node::{Node, NodeId, NodeTree};
use crate::theme::Semantic;

/// Per-render context handed to `View::render`. Owns the in-construction
/// `NodeTree`, tracks the current parent for nested views, and
/// accumulates the `Interactions` table for this render pass.
pub struct Ctx<'a> {
    pub tree: &'a mut NodeTree,
    pub interactions: &'a mut Interactions,
    /// Current parent during traversal. `View::render` calls
    /// `push`/`pop` to descend into children; primitives call `add` to
    /// emit a leaf at the current parent.
    parent_stack: Vec<NodeId>,
    /// Theme tokens — light or dark.
    pub theme: Semantic,
}

impl<'a> Ctx<'a> {
    pub fn new(
        tree: &'a mut NodeTree,
        interactions: &'a mut Interactions,
        theme: Semantic,
    ) -> Self {
        Self { tree, interactions, parent_stack: Vec::new(), theme }
    }

    /// Add a leaf node under the current parent.
    pub fn add(&mut self, node: Node) -> NodeId {
        let parent = self.parent_stack.last().copied();
        self.tree.insert(parent, node)
    }

    /// Add a container node and push it as the parent for subsequent
    /// `add` calls. Caller must `pop` when finished.
    pub fn push(&mut self, container: Node) -> NodeId {
        let parent = self.parent_stack.last().copied();
        let id = self.tree.insert(parent, container);
        self.parent_stack.push(id);
        id
    }

    pub fn pop(&mut self) {
        self.parent_stack.pop();
    }

    /// Attach a click handler to a node previously emitted in this
    /// render pass. Replaces any existing click handler on that node.
    pub fn on_click<F: Fn() + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_click = Some(Box::new(handler) as ClickHandler);
    }

    /// Attach a key handler. Fires on each `Event::Key` while the
    /// node has focus. Replaces any existing key handler.
    pub fn on_key<F: Fn(&crate::event::Event) + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_key =
            Some(Box::new(handler) as crate::interaction::KeyHandler);
    }

    /// Mark a node as eligible for keyboard focus.
    pub fn focusable(&mut self, id: NodeId) {
        self.interactions.entry(id).focusable = true;
    }
}

/// Anything that can produce render output by emitting nodes into a `Ctx`.
pub trait View {
    fn render(&self, ctx: &mut Ctx);
}

/// `()` is the empty view — useful as a no-op default and for
/// optional-content patterns later.
impl View for () {
    fn render(&self, _ctx: &mut Ctx) {}
}

/// `Vec<V>` renders each element in order. Lets a View produce a
/// sequence of children without a custom container type.
impl<V: View> View for Vec<V> {
    fn render(&self, ctx: &mut Ctx) {
        for v in self {
            v.render(ctx);
        }
    }
}

/// Run one render pass and return both the resulting tree and the
/// interaction table built up during the pass. The diff vs the
/// previous tree (and the wire emission) lives in `surface::commit`.
pub fn render<V: View>(view: &V, theme: Semantic) -> (NodeTree, Interactions) {
    let mut tree = NodeTree::new();
    let mut interactions = Interactions::new();
    {
        let mut ctx = Ctx::new(&mut tree, &mut interactions, theme);
        view.render(&mut ctx);
    }
    (tree, interactions)
}
