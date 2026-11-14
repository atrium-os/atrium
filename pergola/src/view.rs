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

/// Interaction state carried into a render pass so widgets can draw
/// their hover / pressed / focused looks. Node ids are stable across
/// renders while the view structure is stable, so last frame's state
/// keys match this frame's nodes.
#[derive(Debug, Clone, Copy, Default)]
pub struct InteractionState {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub focused: Option<NodeId>,
    /// Focus came from the keyboard: draw the focus ring.
    pub focus_visible: bool,
    /// The node whose tooltip is showing.
    pub tip: Option<NodeId>,
    /// Menus may open as pop-up windows (a `Window` hosts the app).
    pub popups: bool,
}

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
    /// Hover/press/focus from the app, for state-dependent looks.
    pub state: InteractionState,
    /// Layers drawn after the whole view, at the top level: menus,
    /// dropdowns, dialogs ([`Ctx::overlay`]).
    overlays: Vec<Overlay>,
    /// Widgets placed in a layout ([`Ctx::place`]), drawn once layout has
    /// given their slot its rect.
    placements: Vec<(NodeId, Placement)>,
}

/// A placed widget waiting for its laid-out rect.
pub type Placement = Box<dyn for<'b> FnOnce(&mut Ctx<'b>, crate::geom::Rect)>;

/// A widget that can be placed in a layout: it says how big it wants to
/// be, and draws itself in whatever rect layout gives it.
pub trait Widget: View + 'static {
    /// The size it wants (px). A 0 width or height means "as the layout
    /// gives" (it stretches on that axis).
    fn natural_size(&self) -> crate::geom::Size;
    /// The widget, moved and sized to `r`.
    fn framed(self, r: crate::geom::Rect) -> Self
    where
        Self: Sized;
    /// Whether it takes all the width its parent gives (a field, a tab
    /// row, a slider with no width set); `natural_size().w` is then its
    /// least width.
    fn fills(&self) -> bool {
        false
    }
    /// Its label's baseline, px below its top, for baseline alignment.
    fn baseline(&self, _size: crate::geom::Size) -> Option<f32> {
        None
    }
}

/// A deferred top-level layer.
pub type Overlay = Box<dyn for<'b> FnOnce(&mut Ctx<'b>)>;

impl<'a> Ctx<'a> {
    pub fn new(
        tree: &'a mut NodeTree,
        interactions: &'a mut Interactions,
        theme: Semantic,
    ) -> Self {
        Self::with_state(tree, interactions, theme, InteractionState::default())
    }

    pub fn with_state(
        tree: &'a mut NodeTree,
        interactions: &'a mut Interactions,
        theme: Semantic,
        state: InteractionState,
    ) -> Self {
        Self { tree, interactions, parent_stack: Vec::new(), theme, state, overlays: Vec::new(), placements: Vec::new() }
    }

    /// Whether `id` is the current hover target. Valid for nodes whose
    /// id is stable across renders (stable view structure).
    pub fn is_hovered(&self, id: NodeId) -> bool { self.state.hovered == Some(id) }
    pub fn is_pressed(&self, id: NodeId) -> bool { self.state.pressed == Some(id) }
    pub fn is_focused(&self, id: NodeId) -> bool { self.state.focused == Some(id) }
    /// Focused from the keyboard: the one case the focus ring shows.
    pub fn is_focus_visible(&self, id: NodeId) -> bool { self.state.focus_visible && self.is_focused(id) }

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

    /// Mark `id` as an embed slot: its laid-out window rect goes to the
    /// slot binder whenever it changes (see [`crate::slot`]).
    pub fn slot(&mut self, id: NodeId, key: crate::slot::SlotKey) {
        self.interactions.entry(id).slot = Some(key);
    }

    /// Accept drops from other windows. The handler runs with the drop's
    /// token current and takes the content with `clipboard::drop_text`.
    pub fn on_drop<F: Fn() + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_drop = Some(Box::new(handler));
    }

    /// Follow a press from this node until its release: `Start` at the
    /// press, `Move` for each motion (wherever the pointer is), `End`.
    pub fn on_drag<F: Fn(crate::interaction::DragPhase, crate::geom::Point) + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_drag = Some(Box::new(handler));
    }

    /// Receive wheel, navigation-key and press input for a scroll view.
    pub fn on_scroll<F: Fn(&crate::interaction::ScrollInput) + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_scroll = Some(Box::new(handler));
    }

    /// Run `handler` every frame while the app animates (it returns
    /// whether it still needs frames). Frames start when input reaches an
    /// `on_scroll` handler.
    pub fn on_frame<F: Fn(f64, &Node) -> bool + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_frame = Some(Box::new(handler));
    }

    /// The window's minimum size is `id`'s natural minimum: the narrowest
    /// it lays out in and its height at the window's width
    /// (resolution-independence.md §6.3). `id` is a top-level flex stack
    /// that fills the window; a layout placed by hand (right-aligned at the
    /// window's width) should not say this, as its minimum would be its
    /// current size. Without it the window gives the WM no minimum.
    pub fn min_size_from(&mut self, id: NodeId) {
        self.tree.set_min_root(id);
    }

    /// Keep a scroll view's place across a reflow (resolution-independence.md
    /// §6.3): when its content lays out again with its position unchanged
    /// (a text size or width change), `handler` gets the position that
    /// keeps the first visible text where it was, and the node; it
    /// returns nothing of use (the bool is ignored).
    pub fn on_reflow<F: Fn(f64, &Node) -> bool + 'static>(&mut self, id: NodeId, handler: F) {
        self.interactions.entry(id).on_reflow = Some(Box::new(handler));
    }

    /// Whether [`Ctx::popup`] opens a window (the app runs in a `Window`);
    /// otherwise a menu draws as an overlay.
    pub fn popups_enabled(&self) -> bool { self.state.popups }

    /// A pop-up window (docs/spec/fresco-popups.md): call from an overlay.
    /// `rect` is where it would draw in this window (its size is the
    /// pop-up's; frescod decides where it goes, from `spec`). `f` draws its
    /// content in window coordinates inside `rect`; `on_dismiss` runs when
    /// frescod takes it down (close the menu there).
    pub fn popup(&mut self, rect: crate::geom::Rect, spec: crate::popup::Spec, on_dismiss: impl Fn() + 'static, f: impl FnOnce(&mut Ctx)) -> NodeId {
        let saved = std::mem::take(&mut self.parent_stack);
        let id = self.push(Node::stack(crate::geom::Axis::Vertical, rect, 0.0));
        self.set_flex(id, crate::layout::FlexStyle { free: true, ..Default::default() });
        self.tree.set_popup(id, spec);
        self.interactions.entry(id).on_dismiss = Some(Box::new(on_dismiss));
        f(self);
        self.parent_stack = saved;
        id
    }

    /// Draw `f` above everything the view draws, at the window's top
    /// level, after the view has rendered: a menu or dialog escapes its
    /// parent's layout and clip, and is hit before what it covers.
    pub fn overlay(&mut self, f: impl for<'b> FnOnce(&mut Ctx<'b>) + 'static) {
        self.overlays.push(Box::new(f));
    }

    /// Put `w` in the current stack as a laid-out child: a slot its
    /// natural size takes part in layout (grow, align, baseline, grid
    /// cell) and the widget draws in the rect layout gives the slot.
    /// Returns the slot.
    pub fn place<W: Widget>(&mut self, w: W) -> NodeId {
        self.place_with(w, crate::layout::FlexStyle::default())
    }

    /// [`Ctx::place`] with the slot's flex parameters (grow, align_self,
    /// min/max). A grown or stretched widget draws at its grown size.
    pub fn place_with<W: Widget>(&mut self, w: W, style: crate::layout::FlexStyle) -> NodeId {
        let s = w.natural_size();
        let fills = w.fills();
        let in_row = self.parent_stack.last().and_then(|p| self.tree.get(*p)).is_some_and(|n| matches!(n, Node::Stack { axis: crate::geom::Axis::Horizontal, .. }))
            && !self.parent_stack.last().is_some_and(|p| self.tree.grid(*p).is_some());
        let width = if fills { 0.0 } else { s.w };
        let id = self.push(Node::stack(crate::geom::Axis::Vertical, crate::geom::Rect::new(0.0, 0.0, width, s.h), 0.0));
        self.pop();
        let baseline = w.baseline(s).or(style.baseline);
        let mut style = crate::layout::FlexStyle { free: true, baseline, ..style };
        if fills {
            // Stretched across a column; in a row, it takes what is left.
            style.min_width = style.min_width.or(Some(s.w));
            if in_row {
                style.grow = style.grow.max(1.0);
            }
        }
        self.set_flex(id, style);
        self.placements.push((id, Box::new(move |ctx: &mut Ctx, r| w.framed(r).render(ctx))));
        id
    }

    /// Draw the placed widgets in their slots' rects, laying out what
    /// they draw, until none is waiting.
    fn run_placements(&mut self) {
        while !self.placements.is_empty() {
            for (slot, draw) in std::mem::take(&mut self.placements) {
                let Some(r) = self.tree.get(slot).map(|n| n.rect()) else { continue };
                let saved = std::mem::replace(&mut self.parent_stack, vec![slot]);
                draw(self, r);
                self.parent_stack = saved;
                for c in self.tree.children_of(slot).to_vec() {
                    crate::layout::layout(self.tree, c);
                }
            }
        }
    }

    /// Lay out, draw the placed widgets, then the overlays queued so far
    /// (and any they queue) at top level, each round laid out in turn.
    pub fn flush_overlays(&mut self) {
        let mut laid = 0;
        loop {
            let roots: Vec<NodeId> = self.tree.roots().skip(laid).collect();
            laid += roots.len();
            for r in roots {
                crate::layout::layout(self.tree, r);
            }
            self.run_placements();
            if self.overlays.is_empty() {
                break;
            }
            let saved = std::mem::take(&mut self.parent_stack);
            for f in std::mem::take(&mut self.overlays) {
                f(self);
            }
            self.parent_stack = saved;
        }
    }

    /// Give `id` a tooltip (visual language §14.2): `bg-inverse`, `sm`
    /// `text-inverse`, 8px under the node, after 500 ms of hover, at once
    /// when moving from one tooltip to the next. Drawn as an overlay.
    pub fn tooltip(&mut self, id: NodeId, text: impl Into<String>) {
        self.interactions.entry(id).tooltip = true;
        if self.state.tip != Some(id) {
            return;
        }
        let text = text.into();
        self.overlay(move |ctx| {
            // Read after layout: where the node ended up.
            let Some(anchor) = ctx.tree.get(id).map(|n| n.rect()) else { return };
            use crate::theme::{font, radius, space, type_size, Weight};
            let t = ctx.theme;
            let style = crate::node::TextStyle { family: font::SANS.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_inverse() };
            let m = Node::measure_text(&text, &style);
            let (px, py) = (space::XS, space::XXS);
            let (w, h) = (m.w + 2.0 * px, m.h + 2.0 * py);
            let r = crate::geom::Rect::new(anchor.x() + (anchor.w() - w) / 2.0, anchor.y() + anchor.h() + space::XS, w, h);
            let bg = ctx.tree.insert(None, Node::Rect { rect: r, fill: t.bg_inverse(), radius: radius::XS });
            ctx.tree.insert(Some(bg), Node::Text { rect: crate::geom::Rect::new(r.x() + px, r.y() + py, m.w, m.h), content: text, style });
        });
    }

    /// Give `id` keyboard focus when it first appears (an opened menu or
    /// dialog); focus goes back where it was when it goes.
    pub fn autofocus(&mut self, id: NodeId) {
        let h = self.interactions.entry(id);
        h.focusable = true;
        h.autofocus = true;
    }

    /// Mark a node as eligible for keyboard focus.
    pub fn focusable(&mut self, id: NodeId) {
        self.interactions.entry(id).focusable = true;
    }

    /// Give the next node emitted a stable key among its siblings, so
    /// it keeps its id (and its state on the screen) when siblings
    /// before it are inserted or removed. Keys are per parent.
    pub fn key(&mut self, key: u64) {
        self.tree.key_next(key);
    }

    /// Add text that breaks into lines at the width layout gives it (a
    /// stack's stretched child, a grid cell, or `max_width`); it is as tall
    /// as its lines. Lines break at spaces and at `\n`.
    pub fn paragraph(&mut self, content: impl Into<String>, style: crate::node::TextStyle) -> NodeId {
        let id = self.add(Node::Text { rect: crate::geom::Rect::ZERO_SIZED, content: content.into(), style });
        self.set_flex(id, crate::layout::FlexStyle { wrap: true, shrink: 1.0, ..crate::layout::FlexStyle::default() });
        id
    }

    /// [`Ctx::paragraph`] with hyphenation and a line limit ("…" past it;
    /// `max_lines: 1` is a truncated label).
    pub fn paragraph_with(&mut self, content: impl Into<String>, style: crate::node::TextStyle, wrap: crate::wrap::Wrap) -> NodeId {
        let id = self.paragraph(content, style);
        let fs = self.tree.style(id).copied().unwrap_or_default();
        self.set_flex(id, crate::layout::FlexStyle { hyphens: wrap.hyphens, max_lines: wrap.max_lines, ..fs });
        id
    }

    /// Lay the stack `id` out as a grid ([`crate::layout::Grid`]).
    pub fn grid(&mut self, id: NodeId, grid: crate::layout::Grid) {
        self.tree.set_grid(id, grid);
    }

    /// Attach flex parameters (grow, alignment, container padding /
    /// justify) to a node emitted in this render pass.
    pub fn set_flex(&mut self, id: NodeId, style: crate::layout::FlexStyle) {
        self.tree.set_style(id, style);
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

/// Closure-as-View adapter — one-off content (chip innards, panel
/// bodies) without a named struct per composition.
pub struct Draw<F: Fn(&mut Ctx)>(pub F);

impl<F: Fn(&mut Ctx)> View for Draw<F> {
    fn render(&self, ctx: &mut Ctx) {
        (self.0)(ctx)
    }
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
    render_with_state(view, theme, InteractionState::default())
}

/// `render` with hover/press/focus state, so widgets can draw their
/// state-dependent looks. `App::tick` uses this.
pub fn render_with_state<V: View>(
    view: &V,
    theme: Semantic,
    state: InteractionState,
) -> (NodeTree, Interactions) {
    render_continuing(view, theme, state, &mut NodeTree::new())
}

/// `render_with_state` keeping `prev`'s node ids: a node at the same
/// key path gets the same id, so the diff sees a change, not a
/// remove and an add. `App::tick` uses this.
pub fn render_continuing<V: View>(
    view: &V,
    theme: Semantic,
    state: InteractionState,
    prev: &mut NodeTree,
) -> (NodeTree, Interactions) {
    let mut tree = NodeTree::continuing(prev);
    let mut interactions = Interactions::new();
    {
        let mut ctx = Ctx::with_state(&mut tree, &mut interactions, theme, state);
        view.render(&mut ctx);
        ctx.flush_overlays();
    }
    tree.sweep_keys();
    (tree, interactions)
}
