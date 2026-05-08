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
