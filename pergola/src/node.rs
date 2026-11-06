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

use std::collections::HashMap;

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
    /// A scrolling viewport (`ScrollView`, pergola.md §10): on the wire a
    /// `SCROLL` group clipped to `rect`, an optional opaque `backdrop`
    /// under everything, and one content group holding the children. The
    /// children are laid out once, in a column `rect.w` wide starting at
    /// `rect.origin`, as if not scrolled; a scroll changes only the
    /// content group's transform.
    ///
    /// - `offset`: scroll position, content px at zoom 1.
    /// - `zoom`: the settled zoom the children are sent at (their
    ///   payloads are multiplied by it).
    /// - `stretch`: the gesture's zoom over `zoom` while a pinch runs (the
    ///   content group's scale: existing content stretched until it settles).
    /// - `content_h`: the children's height at zoom 1, written by layout.
    Scroll {
        rect: Rect,
        offset: f32,
        zoom: f32,
        stretch: f32,
        backdrop: Option<Color>,
        content_h: f32,
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
            Node::Rect { rect, .. } | Node::Text { rect, .. } | Node::Stack { rect, .. } | Node::Scroll { rect, .. } => *rect,
            Node::Path { p0, p1, .. } => {
                let (x, y) = (p0.0.min(p1.0), p0.1.min(p1.1));
                Rect::new(x, y, (p0.0 - p1.0).abs(), (p0.1 - p1.1).abs())
            }
        }
    }

    pub fn set_rect(&mut self, r: Rect) {
        match self {
            Node::Rect { rect, .. } | Node::Text { rect, .. } | Node::Stack { rect, .. } | Node::Scroll { rect, .. } => *rect = r,
            Node::Path { .. } => {} // absolutely positioned — layout doesn't move it
        }
    }

    /// The node in a group's coordinates: moved by `-origin`, then
    /// multiplied by `scale` (a scroll view's zoom; 1 elsewhere).
    pub fn to_group(mut self, origin: crate::geom::Point, scale: f32) -> Self {
        let map = |x: f32, y: f32| ((x - origin.x) * scale, (y - origin.y) * scale);
        match &mut self {
            Node::Rect { rect, radius, .. } => {
                *rect = scale_rect(*rect, origin, scale);
                *radius *= scale;
            }
            Node::Text { rect, style, .. } => {
                *rect = scale_rect(*rect, origin, scale);
                style.size *= scale;
            }
            Node::Stack { rect, radius, spacing, .. } => {
                *rect = scale_rect(*rect, origin, scale);
                *radius *= scale;
                *spacing *= scale;
            }
            Node::Scroll { rect, .. } => *rect = scale_rect(*rect, origin, scale),
            Node::Path { p0, p1, width, .. } => {
                *p0 = map(p0.0, p0.1);
                *p1 = map(p1.0, p1.1);
                *width *= scale;
            }
        }
        self
    }

    /// Intrinsic size — what this node "wants" to be when given no
    /// outside constraint. Used by `layout::layout_stack` to
    /// distribute children along the axis. Stack containers report
    /// their declared size; primitives report their `rect.size`.
    pub fn intrinsic_size(&self) -> Size {
        match self {
            Node::Rect { rect, .. } | Node::Stack { rect, .. } | Node::Scroll { rect, .. } => rect.size,
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

fn scale_rect(r: Rect, origin: crate::geom::Point, scale: f32) -> Rect {
    Rect::new((r.origin.x - origin.x) * scale, (r.origin.y - origin.y) * scale, r.size.w * scale, r.size.h * scale)
}

/// What makes a node "the same node" from one render to the next: its
/// parent's id plus either an explicit key (`Ctx::key`) or its position
/// among its parent's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Ordinal(u32),
    Explicit(u64),
}

/// Hands out stable node ids. Persists across renders (the `App` moves it
/// from one tree to the next), so a node keeps its id while its key path
/// stays the same, however the nodes around it change. Ids are below
/// 2^30: the top two bits name the wire nodes a node derives
/// ([`DERIVED_BIT`], [`CONTENT_BIT`]).
#[derive(Debug, Default)]
pub struct KeyAlloc {
    ids: HashMap<(u32, Key), u32>,
    /// The ids in `ids`, so a counter that wraps never reuses a live one.
    live: std::collections::HashSet<u32>,
    seen: std::collections::HashSet<(u32, Key)>,
    next: u32,
}

/// Parent id of the top-level nodes in the key map.
const NO_PARENT: u32 = u32::MAX;
/// A wire id with this bit is a node's background rect (a filled
/// Stack's fill, a scroll view's backdrop).
pub const DERIVED_BIT: u32 = 1 << 31;
/// A wire id with this bit is a scroll view's content group.
pub const CONTENT_BIT: u32 = 1 << 30;
/// Node ids are below this.
pub const ID_LIMIT: u32 = 1 << 30;

impl KeyAlloc {
    fn id(&mut self, parent: Option<NodeId>, key: Key) -> NodeId {
        let k = (parent.map(|p| p.0).unwrap_or(NO_PARENT), key);
        self.seen.insert(k);
        if let Some(&id) = self.ids.get(&k) {
            return NodeId(id);
        }
        let mut id = self.next;
        while self.live.contains(&id) {
            id = (id + 1) % ID_LIMIT;
        }
        self.next = (id + 1) % ID_LIMIT;
        self.ids.insert(k, id);
        self.live.insert(id);
        NodeId(id)
    }

    /// Forget keys not used since the last sweep (after each render).
    pub fn sweep(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        let live = &mut self.live;
        self.ids.retain(|k, id| {
            let keep = seen.contains(k);
            if !keep { live.remove(id); }
            keep
        });
    }
}

/// The node tree of one render. Ids are stable across renders
/// ([`KeyAlloc`]); structure lives in `parent`/`children`, not pointers.
#[derive(Debug, Default)]
pub struct NodeTree {
    nodes: Vec<Node>,
    ids: Vec<NodeId>,
    index: HashMap<NodeId, usize>,
    parent: Vec<Option<NodeId>>,
    children: Vec<Vec<NodeId>>,
    /// Flex parameters per node (side-table — layout concerns stay
    /// out of the wire-facing `Node` payloads). Sparse: most nodes
    /// use defaults.
    styles: Vec<Option<crate::layout::FlexStyle>>,
    keys: KeyAlloc,
    /// Set by `Ctx::key`: the key of the next node inserted.
    next_key: Option<u64>,
    /// Top-level nodes so far (their position key).
    root_count: u32,
}

impl NodeTree {
    pub fn new() -> Self { Self::default() }

    /// A tree that continues `prev`'s ids: same key path, same id.
    pub fn continuing(prev: &mut NodeTree) -> Self {
        Self { keys: std::mem::take(&mut prev.keys), ..Self::default() }
    }

    /// Forget the keys this render did not use.
    pub fn sweep_keys(&mut self) {
        self.keys.sweep();
    }

    /// The next `insert` uses `key` instead of its position among its
    /// siblings, so it keeps its id when siblings come and go.
    pub fn key_next(&mut self, key: u64) {
        self.next_key = Some(key);
    }

    /// Insert a node under `parent`. Returns its (stable) id.
    pub fn insert(&mut self, parent: Option<NodeId>, node: Node) -> NodeId {
        let key = match self.next_key.take() {
            Some(k) => Key::Explicit(k),
            None => {
                let n = match parent {
                    Some(p) => self.index.get(&p).map(|&i| self.children[i].len()).unwrap_or(0),
                    None => self.root_count as usize,
                };
                Key::Ordinal(n as u32)
            }
        };
        let mut id = self.keys.id(parent, key);
        // Two explicit keys alike under one parent: keep the tree sound
        // with a fresh id (the second node just does not diff stably).
        while self.index.contains_key(&id) {
            id = self.keys.id(parent, Key::Ordinal(u32::MAX - self.nodes.len() as u32));
        }
        let i = self.nodes.len();
        self.nodes.push(node);
        self.ids.push(id);
        self.index.insert(id, i);
        self.parent.push(parent);
        self.children.push(Vec::new());
        self.styles.push(None);
        match parent {
            Some(p) => {
                if let Some(&pi) = self.index.get(&p) {
                    self.children[pi].push(id);
                }
            }
            None => self.root_count += 1,
        }
        id
    }

    /// Attach flex parameters to a node (grow, alignment, container
    /// padding/justify for Stacks). See `layout::FlexStyle`.
    pub fn set_style(&mut self, id: NodeId, style: crate::layout::FlexStyle) {
        if let Some(&i) = self.index.get(&id) {
            self.styles[i] = Some(style);
        }
    }

    pub fn style(&self, id: NodeId) -> Option<&crate::layout::FlexStyle> {
        self.index.get(&id).and_then(|&i| self.styles[i].as_ref())
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.index.get(&id).map(|&i| &self.nodes[i])
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let i = *self.index.get(&id)?;
        Some(&mut self.nodes[i])
    }

    pub fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        self.index.get(&id).and_then(|&i| self.parent[i])
    }

    pub fn children_of(&self, id: NodeId) -> &[NodeId] {
        self.index.get(&id).map(|&i| self.children[i].as_slice()).unwrap_or(&[])
    }

    /// Every node, in insertion order (parents before children).
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.ids.iter().copied().zip(self.nodes.iter())
    }

    /// The top-level nodes. Layout is run once per root.
    pub fn roots(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.ids.iter().zip(&self.parent).filter(|(_, p)| p.is_none()).map(|(id, _)| *id)
    }

    pub fn len(&self) -> usize { self.nodes.len() }
    pub fn is_empty(&self) -> bool { self.nodes.is_empty() }

    /// The nearest Stack or Scroll strictly above `id`: the node whose
    /// group it is drawn in.
    pub fn group_of(&self, id: NodeId) -> Option<NodeId> {
        let mut p = self.parent_of(id);
        while let Some(x) = p {
            if matches!(self.get(x), Some(Node::Stack { .. } | Node::Scroll { .. })) {
                return Some(x);
            }
            p = self.parent_of(x);
        }
        None
    }

    /// Where each node sits on the wire: its group (a Stack's own id, a
    /// Scroll's content group), its paint order within that group
    /// (preorder among the group's members), and its payload in the
    /// group's coordinates, times the zoom of the scroll views it is in.
    fn placements(&self) -> HashMap<NodeId, Placement> {
        #[derive(Clone, Copy)]
        struct Frame {
            wire: Option<NodeId>,
            origin: crate::geom::Point,
            scale: f32,
        }
        let mut out = HashMap::with_capacity(self.nodes.len());
        let mut next_order: HashMap<Option<NodeId>, u32> = HashMap::new();
        let top = Frame { wire: None, origin: crate::geom::Point::new(0.0, 0.0), scale: 1.0 };
        // Preorder: roots in order, then children depth first.
        let mut stack: Vec<(NodeId, Frame)> = self.roots().collect::<Vec<_>>().into_iter().rev().map(|r| (r, top)).collect();
        while let Some((id, f)) = stack.pop() {
            let Some(node) = self.get(id) else { continue };
            // Order 0 is a group's own background, drawn first.
            let o = next_order.entry(f.wire).or_insert(1);
            let order = *o;
            *o += 1;
            out.insert(id, Placement { group: f.wire, order, node: node.clone().to_group(f.origin, f.scale) });
            let inner = match node {
                Node::Stack { rect, .. } => Frame { wire: Some(id), origin: rect.origin, scale: f.scale },
                Node::Scroll { rect, zoom, .. } => Frame {
                    wire: Some(NodeId(id.0 | CONTENT_BIT)),
                    origin: rect.origin,
                    scale: f.scale * zoom,
                },
                _ => f,
            };
            for &c in self.children_of(id).iter().rev() {
                stack.push((c, inner));
            }
        }
        out
    }

    /// Ids in preorder (parents before children, siblings in order).
    fn preorder(&self) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<NodeId> = self.roots().collect::<Vec<_>>().into_iter().rev().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            for &c in self.children_of(id).iter().rev() {
                stack.push(c);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Placement {
    group: Option<NodeId>,
    order: u32,
    node: Node,
}

/// A single change between two `NodeTree`s, as it goes on the wire: the
/// node's payload is in its group's coordinates (`group` = the nearest
/// Stack above it, `None` = the window), so moving a Stack changes only
/// the Stack. A Stack is itself a group on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeDelta {
    /// Node id was not present in the previous tree.
    Added { id: NodeId, node: Node, group: Option<NodeId>, order: u32 },
    /// Its payload, group or order changed.
    Changed { id: NodeId, node: Node, group: Option<NodeId>, order: u32 },
    /// Node id was present in the previous tree but not in the new one.
    Removed { id: NodeId },
}

/// The deltas from `prev` to `next`, matched by id (ids are stable across
/// renders, [`KeyAlloc`]). Removals first, then additions and changes in
/// preorder, so a group always exists before anything is placed in it.
pub fn diff(prev: &NodeTree, next: &NodeTree) -> Vec<NodeDelta> {
    let before = prev.placements();
    let after = next.placements();
    let mut deltas = Vec::new();
    for id in prev.preorder() {
        if after.contains_key(&id) {
            continue;
        }
        // Removing a group removes everything in it, so a node whose
        // group also goes needs no message of its own.
        if prev.group_of(id).is_some_and(|g| !after.contains_key(&g)) {
            continue;
        }
        deltas.push(NodeDelta::Removed { id });
    }
    for id in next.preorder() {
        let Some(a) = after.get(&id) else { continue };
        match before.get(&id) {
            None => deltas.push(NodeDelta::Added { id, node: a.node.clone(), group: a.group, order: a.order }),
            Some(b) if b != a => deltas.push(NodeDelta::Changed { id, node: a.node.clone(), group: a.group, order: a.order }),
            _ => {}
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

    fn rect(x: f32, y: f32) -> Node {
        Node::Rect { rect: Rect::new(x, y, 4.0, 4.0), fill: Color::rgba(1.0, 0.0, 0.0, 1.0), radius: 0.0 }
    }

    fn stack(x: f32, y: f32) -> Node {
        Node::Stack {
            rect: Rect::new(x, y, 40.0, 40.0),
            axis: crate::geom::Axis::Vertical,
            spacing: 0.0,
            fill: None,
            radius: 0.0,
        }
    }

    /// A root Stack holding a keyed child per entry of `keys`.
    fn keyed(prev: &mut NodeTree, keys: &[u64]) -> (NodeTree, Vec<NodeId>) {
        let mut t = NodeTree::continuing(prev);
        let root = t.insert(None, stack(0.0, 0.0));
        let ids = keys.iter().map(|&k| {
            t.key_next(k);
            t.insert(Some(root), rect(k as f32, 0.0))
        }).collect();
        t.sweep_keys();
        (t, ids)
    }

    #[test]
    fn the_first_render_numbers_nodes_in_order() {
        let mut t = NodeTree::new();
        let a = t.insert(None, stack(0.0, 0.0));
        let b = t.insert(Some(a), rect(0.0, 0.0));
        let c = t.insert(Some(a), rect(0.0, 0.0));
        assert_eq!((a.0, b.0, c.0), (0, 1, 2));
    }

    #[test]
    fn a_keyed_node_keeps_its_id_when_a_sibling_is_inserted_before_it() {
        let mut empty = NodeTree::new();
        let (mut first, ids1) = keyed(&mut empty, &[10, 20]);
        let (second, ids2) = keyed(&mut first, &[5, 10, 20]);
        assert_eq!(&ids2[1..], &ids1[..]);
        assert!(!ids1.contains(&ids2[0]));
        // The new node is added; the others only move down one place.
        let d = diff(&first, &second);
        assert!(d.iter().any(|d| matches!(d, NodeDelta::Added { id, .. } if *id == ids2[0])));
        assert!(!d.iter().any(|d| matches!(d, NodeDelta::Removed { .. })));
    }

    #[test]
    fn a_node_not_rendered_loses_its_key() {
        let mut empty = NodeTree::new();
        let (mut first, ids1) = keyed(&mut empty, &[1, 2]);
        let (mut second, _) = keyed(&mut first, &[2]);
        let (_, ids3) = keyed(&mut second, &[1, 2]);
        assert_ne!(ids3[0], ids1[0], "key 1 was swept, so it gets a new id");
        assert_eq!(ids3[1], ids1[1]);
    }

    #[test]
    fn moving_a_stack_changes_only_the_stack() {
        let build = |prev: &mut NodeTree, x: f32| {
            let mut t = NodeTree::continuing(prev);
            let root = t.insert(None, stack(0.0, 0.0));
            let inner = t.insert(Some(root), stack(x, 10.0));
            t.insert(Some(inner), rect(x + 2.0, 12.0));
            t.sweep_keys();
            (t, inner)
        };
        let (mut a, inner) = build(&mut NodeTree::new(), 10.0);
        let (b, _) = build(&mut a, 50.0);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1, "{d:?}");
        match &d[0] {
            NodeDelta::Changed { id, group, .. } => {
                assert_eq!(*id, inner);
                assert!(group.is_some());
            }
            other => panic!("expected the Stack to change, got {other:?}"),
        }
    }

    #[test]
    fn a_leaf_is_placed_in_its_stack_coordinates() {
        let mut t = NodeTree::new();
        let root = t.insert(None, stack(0.0, 0.0));
        let inner = t.insert(Some(root), stack(10.0, 20.0));
        let leaf = t.insert(Some(inner), rect(13.0, 25.0));
        let d = diff(&NodeTree::new(), &t);
        let placed = d.iter().find_map(|d| match d {
            NodeDelta::Added { id, node, group, order } if *id == leaf => Some((node.clone(), *group, *order)),
            _ => None,
        }).unwrap();
        assert_eq!(placed.1, Some(inner));
        assert_eq!(placed.2, 1, "order 0 is the Stack's background");
        assert_eq!(placed.0.rect().origin, crate::geom::Point::new(3.0, 5.0));
    }

    #[test]
    fn removing_a_stack_sends_one_removal() {
        let mut a = NodeTree::new();
        let root = a.insert(None, stack(0.0, 0.0));
        let inner = a.insert(Some(root), stack(0.0, 0.0));
        a.insert(Some(inner), rect(0.0, 0.0));
        a.insert(Some(inner), rect(0.0, 0.0));
        let mut b = NodeTree::continuing(&mut a);
        b.insert(None, stack(0.0, 0.0));
        b.sweep_keys();
        assert_eq!(diff(&a, &b), vec![NodeDelta::Removed { id: inner }]);
    }
}
