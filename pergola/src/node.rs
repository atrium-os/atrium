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
use crate::geom::{Axis, Rect};

/// Stable identifier for a node in the tree. Allocated by the `Ctx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const ROOT: Self = Self(0);
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
    /// A layout container that arranges children along an axis with
    /// uniform spacing. Real flex/grid lands in phase 2 via `taffy`;
    /// this is the placeholder for the commit-cycle smoke test.
    Stack {
        rect: Rect,
        axis: Axis,
        spacing: f32,
    },
}

/// A flat arena of nodes. Tree structure lives in the
/// `parent`/`children` Vecs, not in pointers — this lets us diff old
/// vs new trees by iterating numeric ids.
#[derive(Debug, Default)]
pub struct NodeTree {
    nodes: Vec<Option<Node>>,
    parent: Vec<Option<NodeId>>,
    children: Vec<Vec<NodeId>>,
}

impl NodeTree {
    pub fn new() -> Self { Self::default() }

    /// Allocate a new node in the tree under `parent`. Returns its id.
    pub fn insert(&mut self, parent: Option<NodeId>, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Some(node));
        self.parent.push(parent);
        self.children.push(Vec::new());
        if let Some(p) = parent {
            self.children[p.0 as usize].push(id);
        }
        id
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize).and_then(|n| n.as_ref())
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

    pub fn len(&self) -> usize { self.nodes.iter().filter(|n| n.is_some()).count() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}
