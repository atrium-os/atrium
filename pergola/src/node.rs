//! The retained-mode scene-graph node tree that Pergola maintains.
//!
//! Apps don't construct `Node`s directly — they write `View`s. The
//! `View::render` pass produces this tree; commit diffs it against the
//! previous tree and emits the wire deltas (`SCENE_NODE_SET` /
//! `SCENE_NODE_CLEAR`) via `fresco-client`.
//!
//! Phase 0 ships only the primitive node kinds we need to demonstrate
//! the commit cycle. Layout primitives (`Stack`), `Text`, `Image`,
//! `Path` arrive in subsequent phases.

use crate::color::Color;
use crate::geom::{Axis, Rect, Size};
use crate::theme::tokens::Weight;

/// Stable identifier for a node in the tree. Allocated by the `Ctx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const ROOT: Self = Self(0);
}

/// A text-rendering style. Text nodes carry one of these alongside
/// their content. Real shaping happens via `fresco-text` once wire
/// emission lands (phase 4); for now `Text::measured_size` returns a
/// proportional-font estimate good enough to drive layout smoke tests.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// PostScript family name. Use `theme::font::SANS` / `MONO`.
    pub family: String,
    /// Logical pixels — pick from `theme::type_size`.
    pub size: f32,
    /// Weight from the theme's weight enum.
    pub weight: Weight,
    /// Fill color — use a `Semantic::text_*` token.
    pub color: Color,
}

/// Per-node payload. Keep this small; structural layout sits in
/// `NodeTree`'s parent/children indices, not duplicated per node.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Filled rectangle with optional rounded corners.
    Rect {
        rect: Rect,
        fill: Color,
        radius: f32,
    },
    /// Single-line text. `rect.size` is overwritten by the layout
    /// pass to match the shaped width × line-height; views that want
    /// hard wrapping or multi-line text should use a `Stack` of
    /// per-line `Text` nodes (this is fine — text is glyph-runs at
    /// the GPU layer, not a rich layout primitive).
    Text {
        rect: Rect,
        content: String,
        style: TextStyle,
    },
    /// A layout container that arranges children along an axis with
    /// uniform spacing. The layout pass walks Stacks and assigns
    /// child rects based on intrinsic sizes + spacing + axis.
    ///
    /// With `fill: Some(_)` the container also *paints* — one rounded
    /// rect behind its children (panels, chips, popover bodies). A
    /// filled Stack is a hit-target; a layout-only Stack is
    /// transparent to hits. Borders are composed by nesting: an outer
    /// filled Stack in the border color with 1px padding around an
    /// inner filled Stack.
    Stack {
        rect: Rect,
        axis: Axis,
        spacing: f32,
        fill: Option<Color>,
        radius: f32,
    },
    /// A single stroked line segment (a thick rotated quad at the GPU layer).
    /// Absolutely positioned — layout doesn't touch it. Vector icons are built from
    /// many of these (see `icon::parse_icon`).
    Path {
        p0: (f32, f32),
        p1: (f32, f32),
        width: f32,
        color: Color,
    },
}

impl Node {
    /// Layout-only stack (no paint).
    pub fn stack(axis: Axis, rect: Rect, spacing: f32) -> Self {
        Node::Stack { rect, axis, spacing, fill: None, radius: 0.0 }
    }

    pub fn vstack(rect: Rect, spacing: f32) -> Self {
        Self::stack(Axis::Vertical, rect, spacing)
    }

    pub fn hstack(rect: Rect, spacing: f32) -> Self {
        Self::stack(Axis::Horizontal, rect, spacing)
    }

    /// Stack that paints a rounded rect behind its children.
    pub fn stack_filled(axis: Axis, rect: Rect, spacing: f32, fill: Color, radius: f32) -> Self {
        Node::Stack { rect, axis, spacing, fill: Some(fill), radius }
    }

    /// Logical bounds in this node's own (parent-relative) coordinate
    /// space. Layout uses this as the *current* rect; for some node
    /// kinds (Text) it is mutated in place by the layout pass.
    pub fn rect(&self) -> Rect {
        match self {
            Node::Rect { rect, .. } | Node::Text { rect, .. } | Node::Stack { rect, .. } => *rect,
            Node::Path { p0, p1, .. } => {
                let (x, y) = (p0.0.min(p1.0), p0.1.min(p1.1));
                Rect::new(x, y, (p0.0 - p1.0).abs(), (p0.1 - p1.1).abs())
            }
        }
    }

    pub fn set_rect(&mut self, r: Rect) {
        match self {
            Node::Rect { rect, .. } | Node::Text { rect, .. } | Node::Stack { rect, .. } => *rect = r,
            Node::Path { .. } => {} // absolutely positioned — layout doesn't move it
        }
    }

    /// Intrinsic size — what this node "wants" to be when given no
    /// outside constraint. Used by `layout::layout_stack` to
    /// distribute children along the axis. Stack containers report
    /// their declared size; primitives report their `rect.size`.
    pub fn intrinsic_size(&self) -> Size {
        match self {
            Node::Rect { rect, .. } | Node::Stack { rect, .. } => rect.size,
            Node::Text { content, style, .. } => Self::measure_text(content, style),
            Node::Path { .. } => self.rect().size,
        }
    }

    /// Shaped text size via the installed measurer (`crate::text`);
    /// falls back to the historical 0.55·size estimate when headless.
    pub fn measure_text(content: &str, style: &TextStyle) -> Size {
        crate::text::measure(content, style)
    }
}

/// A flat arena of nodes. Tree structure lives in the
/// `parent`/`children` Vecs, not in pointers — this lets us diff old
/// vs new trees by iterating numeric ids.
#[derive(Debug, Default)]
pub struct NodeTree {
    nodes: Vec<Option<Node>>,
    parent: Vec<Option<NodeId>>,
    children: Vec<Vec<NodeId>>,
    /// Flex parameters per node (side-table — layout concerns stay
    /// out of the wire-facing `Node` payloads). Sparse: most nodes
    /// use defaults.
    styles: Vec<Option<crate::layout::FlexStyle>>,
}

impl NodeTree {
    pub fn new() -> Self { Self::default() }

    /// Allocate a new node in the tree under `parent`. Returns its id.
    pub fn insert(&mut self, parent: Option<NodeId>, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Some(node));
        self.parent.push(parent);
        self.children.push(Vec::new());
        self.styles.push(None);
        if let Some(p) = parent {
            self.children[p.0 as usize].push(id);
        }
        id
    }

    /// Attach flex parameters to a node (grow, alignment, container
    /// padding/justify for Stacks). See `layout::FlexStyle`.
    pub fn set_style(&mut self, id: NodeId, style: crate::layout::FlexStyle) {
        if let Some(slot) = self.styles.get_mut(id.0 as usize) {
            *slot = Some(style);
        }
    }

    pub fn style(&self, id: NodeId) -> Option<&crate::layout::FlexStyle> {
        self.styles.get(id.0 as usize).and_then(|s| s.as_ref())
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize).and_then(|n| n.as_ref())
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.0 as usize).and_then(|n| n.as_mut())
    }

    pub fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        self.parent.get(id.0 as usize).copied().flatten()
    }

    pub fn children_of(&self, id: NodeId) -> &[NodeId] {
        self.children.get(id.0 as usize).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter().enumerate().filter_map(|(i, n)| {
            n.as_ref().map(|node| (NodeId(i as u32), node))
        })
    }

    /// Iterate the node ids that have no parent (top-level nodes).
    /// Layout is run once per root.
    pub fn roots(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.parent.iter().enumerate().filter_map(|(i, p)| {
            if p.is_none() && self.nodes.get(i).and_then(|n| n.as_ref()).is_some() {
                Some(NodeId(i as u32))
            } else {
                None
            }
        })
    }

    pub fn len(&self) -> usize { self.nodes.iter().filter(|n| n.is_some()).count() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

/// A single change between two `NodeTree`s. The diff pass produces a
/// stream of these; on phase-4 wire integration these will translate
/// into `SCENE_NODE_SET` / `SCENE_NODE_CLEAR` envelopes via fresco-client.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeDelta {
    /// Node id was not present in the previous tree.
    Added { id: NodeId, node: Node, parent: Option<NodeId> },
    /// Node id existed but its payload changed.
    Changed { id: NodeId, node: Node },
    /// Node id was present in the previous tree but not in the new one.
    Removed { id: NodeId },
}

/// Compute the minimal delta list between `prev` and `next`. The
/// algorithm is positional: a NodeId in the new tree is "the same"
/// node as that NodeId in the previous tree. View code that allocates
/// stable ids therefore gets stable diffs.
///
/// This is the simple textbook diff — O(n) — appropriate while node
/// counts are in the thousands. Keyed reconciliation (React-style) can
/// layer on top later if collection-of-children patterns demand it.
pub fn diff(prev: &NodeTree, next: &NodeTree) -> Vec<NodeDelta> {
    let mut deltas = Vec::new();
    let max = prev.nodes.len().max(next.nodes.len());

    for i in 0..max {
        let p = prev.nodes.get(i).and_then(|n| n.as_ref());
        let n = next.nodes.get(i).and_then(|n| n.as_ref());
        let id = NodeId(i as u32);

        match (p, n) {
            (None, Some(node)) => {
                let parent = next.parent.get(i).copied().flatten();
                deltas.push(NodeDelta::Added { id, node: node.clone(), parent });
            }
            (Some(_), None) => {
                deltas.push(NodeDelta::Removed { id });
            }
            (Some(a), Some(b)) if a != b => {
                deltas.push(NodeDelta::Changed { id, node: b.clone() });
            }
            _ => {} // unchanged or both empty
        }
    }

    deltas
}

#[cfg(test)]
mod diff_tests {
    use super::*;
    use crate::color::Color;
    use crate::geom::Rect;

    #[test]
    fn empty_to_empty_is_noop() {
        let prev = NodeTree::new();
        let next = NodeTree::new();
        assert!(diff(&prev, &next).is_empty());
    }

    #[test]
    fn add_one_node() {
        let prev = NodeTree::new();
        let mut next = NodeTree::new();
        let id = next.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            fill: Color::rgba(1.0, 0.0, 0.0, 1.0),
            radius: 0.0,
        });
        let d = diff(&prev, &next);
        assert_eq!(d.len(), 1);
        match &d[0] {
            NodeDelta::Added { id: did, .. } => assert_eq!(*did, id),
            _ => panic!("expected Added"),
        }
    }

    #[test]
    fn change_one_node() {
        let mut prev = NodeTree::new();
        let id = prev.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            fill: Color::rgba(1.0, 0.0, 0.0, 1.0),
            radius: 0.0,
        });
        let mut next = NodeTree::new();
        next.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            fill: Color::rgba(0.0, 1.0, 0.0, 1.0),  // green now
            radius: 0.0,
        });
        let d = diff(&prev, &next);
        assert_eq!(d.len(), 1);
        match &d[0] {
            NodeDelta::Changed { id: did, .. } => assert_eq!(*did, id),
            _ => panic!("expected Changed"),
        }
    }

    #[test]
    fn unchanged_is_no_delta() {
        let mut a = NodeTree::new();
        a.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            fill: Color::rgba(1.0, 0.0, 0.0, 1.0),
            radius: 0.0,
        });
        let mut b = NodeTree::new();
        b.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            fill: Color::rgba(1.0, 0.0, 0.0, 1.0),
            radius: 0.0,
        });
        assert!(diff(&a, &b).is_empty());
    }
}
