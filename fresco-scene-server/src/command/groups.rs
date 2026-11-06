//! Scene groups (docs/spec/fresco-scene-groups.md §2): a window's nodes as
//! a tree. A group has a parent, a paint order, a transform, a clip, an
//! opacity and flags; any node (leaf or group) can be placed in a group.
//! A window that never sends a group or a placement keeps its flat scene,
//! drawn as before.
//!
//! Invariants kept here, so the packer can trust them:
//! - a parent is a group of the same window, written by the same client;
//! - no node is its own ancestor;
//! - depth ≤ `GROUP_MAX_DEPTH`, groups ≤ `GROUP_MAX_COUNT`;
//! - a cleared or replaced group takes its whole subtree with it, so no
//!   node ever names a parent that is gone.

use std::collections::HashMap;

use fresco_protocol::{GroupParams, PlaceParams, GROUP_MAX_COUNT, GROUP_MAX_DEPTH};

use super::envelope_frontend::WindowSceneState;

/// Why a group or placement was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupError {
    Invalid(&'static str),
    /// The parent is not a group of this window.
    NoParent,
    /// The parent was written by another client.
    ForeignParent,
    /// The node would become its own ancestor.
    Cycle,
    TooDeep,
    TooMany,
    /// PLACE named a node that does not exist.
    NoNode,
}

impl WindowSceneState {
    /// Whether this window uses groups at all. A window that does not is
    /// packed exactly as before groups existed.
    pub fn has_groups(&self) -> bool {
        !self.group_nodes.is_empty() || !self.placements.is_empty()
    }

    /// The parent of `id`, if it is placed (a group's lives in its params).
    pub fn parent_of(&self, id: u32) -> Option<u32> {
        match self.group_nodes.get(&id) {
            Some(g) => g.parent,
            None => self.placements.get(&id).and_then(|p| p.parent),
        }
    }

    fn leaf_exists(&self, id: u32) -> bool {
        self.rect_nodes.contains_key(&id)
            || self.texture_nodes.contains_key(&id)
            || self.path_nodes.contains_key(&id)
            || self.glyph_run_nodes.contains_key(&id)
            || self.fill_nodes.contains_key(&id)
    }

    /// Depth of group `g` (a root group is 1).
    fn depth_of(&self, mut g: u32) -> usize {
        let mut d = 1;
        while let Some(p) = self.group_nodes.get(&g).and_then(|x| x.parent) {
            d += 1;
            g = p;
            if d > GROUP_MAX_DEPTH + 1 {
                break; // cannot happen (no cycles); bounded anyway
            }
        }
        d
    }

    /// Children of every group, built once.
    fn children_index(&self) -> HashMap<u32, Vec<u32>> {
        let mut c: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&id, g) in &self.group_nodes {
            if let Some(p) = g.parent {
                c.entry(p).or_default().push(id);
            }
        }
        for (&id, pl) in &self.placements {
            if let Some(p) = pl.parent {
                c.entry(p).or_default().push(id);
            }
        }
        c
    }

    /// Height of the subtree under group `g` in groups (g alone is 1).
    fn group_height(&self, g: u32, kids: &HashMap<u32, Vec<u32>>) -> usize {
        let mut best = 1;
        let mut stack = vec![(g, 1usize)];
        while let Some((n, h)) = stack.pop() {
            best = best.max(h);
            for &k in kids.get(&n).map(|v| v.as_slice()).unwrap_or(&[]) {
                if self.group_nodes.contains_key(&k) && h < GROUP_MAX_DEPTH + 1 {
                    stack.push((k, h + 1));
                }
            }
        }
        best
    }

    /// Check that `id` may sit under `parent`, written by `writer`.
    fn check_parent(&self, id: u32, parent: Option<u32>, writer: u8) -> Result<(), GroupError> {
        let Some(p) = parent else { return Ok(()) };
        if !self.group_nodes.contains_key(&p) {
            return Err(GroupError::NoParent);
        }
        if self.node_writers.get(&p) != Some(&writer) {
            return Err(GroupError::ForeignParent);
        }
        // `id` must not be `p` or an ancestor of `p`.
        let mut a = Some(p);
        while let Some(x) = a {
            if x == id {
                return Err(GroupError::Cycle);
            }
            a = self.group_nodes.get(&x).and_then(|g| g.parent);
        }
        if self.group_nodes.contains_key(&id) {
            let kids = self.children_index();
            if self.depth_of(p) + self.group_height(id, &kids) > GROUP_MAX_DEPTH {
                return Err(GroupError::TooDeep);
            }
        } else if self.depth_of(p) >= GROUP_MAX_DEPTH + 1 {
            return Err(GroupError::TooDeep);
        }
        Ok(())
    }

    /// `ATRIUM_CORE_GROUP` at `id`. A leaf at `id` is replaced (node ids
    /// name one node); an existing group keeps its children.
    pub fn set_group(&mut self, id: u32, g: GroupParams, writer: u8) -> Result<(), GroupError> {
        g.validate().map_err(GroupError::Invalid)?;
        if let Some(old) = self.group_nodes.get(&id) {
            if self.node_writers.get(&id) != Some(&writer) {
                return Err(GroupError::ForeignParent);
            }
            let _ = old;
        } else if self.group_nodes.len() >= GROUP_MAX_COUNT {
            return Err(GroupError::TooMany);
        }
        if !self.group_nodes.contains_key(&id) {
            // A fresh group at `id` (no children yet): only its own depth counts.
            if let Some(p) = g.parent {
                if !self.group_nodes.contains_key(&p) {
                    return Err(GroupError::NoParent);
                }
                if self.node_writers.get(&p) != Some(&writer) {
                    return Err(GroupError::ForeignParent);
                }
                if p == id {
                    return Err(GroupError::Cycle);
                }
                if self.depth_of(p) + 1 > GROUP_MAX_DEPTH {
                    return Err(GroupError::TooDeep);
                }
            }
        } else {
            self.check_parent(id, g.parent, writer)?;
        }
        // Replacing a leaf: drop it from the leaf maps and its placement.
        self.rect_nodes.remove(&id);
        self.texture_nodes.remove(&id);
        self.path_nodes.remove(&id);
        self.glyph_run_nodes.remove(&id);
        self.fill_nodes.remove(&id);
        self.placements.remove(&id);
        self.group_nodes.insert(id, g);
        self.node_writers.insert(id, writer);
        Ok(())
    }

    /// `ATRIUM_CORE_PLACE` for the existing node `id`.
    pub fn place(&mut self, id: u32, p: PlaceParams, writer: u8) -> Result<(), GroupError> {
        let is_group = self.group_nodes.contains_key(&id);
        if !is_group && !self.leaf_exists(id) {
            return Err(GroupError::NoNode);
        }
        if self.node_writers.get(&id) != Some(&writer) {
            return Err(GroupError::ForeignParent);
        }
        self.check_parent(id, p.parent, writer)?;
        if let Some(g) = self.group_nodes.get_mut(&id) {
            g.parent = p.parent;
            g.order = p.order;
        } else {
            self.placements.insert(id, p);
        }
        Ok(())
    }

    /// A leaf is about to replace whatever is at `id`. If that was a group,
    /// its subtree goes (its children would otherwise name a missing
    /// parent). The leaf keeps the group's place in the tree.
    pub fn leaf_replaces(&mut self, id: u32) {
        if let Some(g) = self.group_nodes.get(&id).copied() {
            self.clear_subtree_below(id);
            self.group_nodes.remove(&id);
            if g.parent.is_some() || g.order != 0 {
                self.placements.insert(id, PlaceParams { parent: g.parent, order: g.order });
            }
        }
    }

    /// Remove `id` and, if it is a group, everything under it. Returns the
    /// ids removed (for the writer bookkeeping and tests).
    pub fn remove_node(&mut self, id: u32) -> Vec<u32> {
        let mut gone = vec![id];
        if self.group_nodes.contains_key(&id) {
            gone.extend(self.clear_subtree_below(id));
        }
        self.drop_one(id);
        gone
    }

    /// Remove every descendant of group `g` (not `g` itself).
    fn clear_subtree_below(&mut self, g: u32) -> Vec<u32> {
        let kids = self.children_index();
        let mut gone = Vec::new();
        let mut stack: Vec<u32> = kids.get(&g).cloned().unwrap_or_default();
        while let Some(n) = stack.pop() {
            if let Some(k) = kids.get(&n) {
                stack.extend(k.iter().copied());
            }
            gone.push(n);
        }
        for &n in &gone {
            self.drop_one(n);
        }
        gone
    }

    fn drop_one(&mut self, id: u32) {
        self.rect_nodes.remove(&id);
        self.texture_nodes.remove(&id);
        self.path_nodes.remove(&id);
        self.glyph_run_nodes.remove(&id);
        self.fill_nodes.remove(&id);
        self.group_nodes.remove(&id);
        self.placements.remove(&id);
        self.node_writers.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresco_protocol::{group_flags, RectParams};

    fn g(parent: Option<u32>) -> GroupParams {
        GroupParams { parent, order: 0, transform: GroupParams::IDENTITY, clip: None, opacity: 1.0, flags: 0 }
    }
    fn rect(s: &mut WindowSceneState, id: u32, w: u8) {
        s.leaf_replaces(id);
        s.rect_nodes.insert(id, RectParams { x: 0.0, y: 0.0, w: 1.0, h: 1.0, r: 1.0, g: 1.0, b: 1.0, a: 1.0, radius: 0.0 });
        s.node_writers.insert(id, w);
    }

    #[test]
    fn legacy_window_has_no_groups() {
        let mut s = WindowSceneState::new();
        rect(&mut s, 1, 7);
        assert!(!s.has_groups());
    }

    #[test]
    fn place_and_clear_subtree() {
        let mut s = WindowSceneState::new();
        s.set_group(10, g(None), 1).unwrap();
        s.set_group(11, g(Some(10)), 1).unwrap();
        rect(&mut s, 1, 1);
        rect(&mut s, 2, 1);
        s.place(1, PlaceParams { parent: Some(11), order: 5 }, 1).unwrap();
        s.place(2, PlaceParams { parent: Some(10), order: 1 }, 1).unwrap();
        assert_eq!(s.parent_of(1), Some(11));
        let mut gone = s.remove_node(10);
        gone.sort();
        assert_eq!(gone, vec![1, 2, 10, 11]);
        assert!(s.group_nodes.is_empty() && s.rect_nodes.is_empty() && s.placements.is_empty() && s.node_writers.is_empty());
    }

    #[test]
    fn refusals() {
        let mut s = WindowSceneState::new();
        s.set_group(10, g(None), 1).unwrap();
        s.set_group(11, g(Some(10)), 1).unwrap();
        // No such parent; another client's parent; a cycle.
        assert_eq!(s.set_group(12, g(Some(99)), 1), Err(GroupError::NoParent));
        assert_eq!(s.set_group(12, g(Some(10)), 2), Err(GroupError::ForeignParent));
        assert_eq!(s.place(10, PlaceParams { parent: Some(11), order: 0 }, 1), Err(GroupError::Cycle));
        assert_eq!(s.set_group(10, g(Some(10)), 1), Err(GroupError::Cycle));
        // PLACE of a node that does not exist; of another client's node.
        assert_eq!(s.place(5, PlaceParams { parent: None, order: 0 }, 1), Err(GroupError::NoNode));
        rect(&mut s, 5, 2);
        assert_eq!(s.place(5, PlaceParams { parent: Some(10), order: 0 }, 2), Err(GroupError::ForeignParent));
        // Rotation, NaN, unknown flag.
        let mut bad = g(None);
        bad.transform = [0.0, 1.0, -1.0, 0.0, 0.0, 0.0];
        assert!(matches!(s.set_group(20, bad, 1), Err(GroupError::Invalid(_))));
        bad.transform = GroupParams::IDENTITY;
        bad.opacity = f32::NAN;
        assert!(matches!(s.set_group(20, bad, 1), Err(GroupError::Invalid(_))));
        bad.opacity = 1.0;
        bad.flags = 1 << 7;
        assert!(matches!(s.set_group(20, bad, 1), Err(GroupError::Invalid(_))));
        bad.flags = group_flags::SCROLL;
        s.set_group(20, bad, 1).unwrap();
    }

    #[test]
    fn depth_and_count_limits() {
        let mut s = WindowSceneState::new();
        s.set_group(1, g(None), 1).unwrap();
        for d in 2..=GROUP_MAX_DEPTH as u32 {
            s.set_group(d, g(Some(d - 1)), 1).unwrap();
        }
        let last = GROUP_MAX_DEPTH as u32;
        assert_eq!(s.set_group(last + 1, g(Some(last)), 1), Err(GroupError::TooDeep));
        // A leaf may sit in the deepest group.
        rect(&mut s, 5000, 1);
        s.place(5000, PlaceParams { parent: Some(last), order: 0 }, 1).unwrap();
        // Moving a two-deep subtree under the deepest-but-one group is too deep.
        s.set_group(6000, g(None), 1).unwrap();
        s.set_group(6001, g(Some(6000)), 1).unwrap();
        assert_eq!(s.place(6000, PlaceParams { parent: Some(last - 1), order: 0 }, 1), Err(GroupError::TooDeep));
        let mut s = WindowSceneState::new();
        for i in 0..GROUP_MAX_COUNT as u32 {
            s.set_group(i, g(None), 1).unwrap();
        }
        assert_eq!(s.set_group(GROUP_MAX_COUNT as u32, g(None), 1), Err(GroupError::TooMany));
        // Updating an existing group is not a new one.
        s.set_group(0, g(None), 1).unwrap();
    }

    #[test]
    fn leaf_over_group_takes_its_place() {
        let mut s = WindowSceneState::new();
        s.set_group(10, g(None), 1).unwrap();
        let mut inner = g(Some(10));
        inner.order = 3;
        s.set_group(11, inner, 1).unwrap();
        rect(&mut s, 1, 1);
        s.place(1, PlaceParams { parent: Some(11), order: 0 }, 1).unwrap();
        rect(&mut s, 11, 1);
        assert!(!s.rect_nodes.contains_key(&1), "the replaced group's children go");
        assert_eq!(s.placements.get(&11), Some(&PlaceParams { parent: Some(10), order: 3 }));
    }
}
