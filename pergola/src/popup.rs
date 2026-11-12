//! Pop-up windows (docs/spec/fresco-popups.md §4): a menu or drop-down
//! list drawn in a window of its own, so it can reach past the app's
//! window.
//!
//! A view asks for one with [`crate::view::Ctx::popup`]: a free stack at
//! the top level, laid out where the overlay would have drawn it, with an
//! anchor and a side. The tree stays one tree (hit-testing, focus and
//! keys work as for an overlay); [`Popups`] keeps a frescod window per
//! pop-up root, sends the root's subtree there (the root at the window's
//! origin) instead of to the main window, and maps the pop-up's pointer
//! input back into tree coordinates. frescod places the window (flipping
//! and sliding it on the monitor) and dismisses it; a dismissal runs the
//! root's `on_dismiss`, which closes the menu as Escape does.
//!
//! Without a [`crate::window::Window`] (tests, the log surface) nothing
//! opens windows, and menus draw as overlays.

use std::collections::HashMap;
use std::io;

use crate::geom::Rect;
use crate::node::{Node, NodeDelta, NodeId, NodeTree};

/// Which side of its anchor a pop-up opens on (frescod flips it to the
/// other when there is no room).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Below,
    Above,
    Right,
    Left,
}

/// How a pop-up lines up along its anchor's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

/// What a pop-up root asks for: where it hangs from (window px) and how.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    pub anchor: Rect,
    pub side: Side,
    pub align: Align,
    /// Takes the keys and goes on a press outside (a menu); a tooltip
    /// would not.
    pub grab: bool,
}

impl Side {
    pub fn wire(self) -> u8 {
        use fresco_protocol::popup_side as s;
        match self {
            Side::Below => s::BELOW,
            Side::Above => s::ABOVE,
            Side::Right => s::RIGHT,
            Side::Left => s::LEFT,
        }
    }
}

impl Align {
    pub fn wire(self) -> u8 {
        use fresco_protocol::popup_align as a;
        match self {
            Align::Start => a::START,
            Align::Center => a::CENTER,
            Align::End => a::END,
        }
    }
}

/// What opens, moves and closes pop-up windows: frescod in a `Window`, a
/// fake in tests. `rect` is the root's laid-out rect (its size is the
/// window's), window px.
pub trait Host {
    fn create(&mut self, spec: &Spec, rect: Rect) -> io::Result<u32>;
    fn place(&mut self, window: u32, spec: &Spec, rect: Rect) -> io::Result<()>;
    fn destroy(&mut self, window: u32);
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Open {
    window: u32,
    spec: Spec,
    rect: Rect,
    /// frescod dismissed it and nothing has placed it again.
    dismissed: bool,
}

/// The window of nodes whose pop-up was destroyed.
const GONE: u32 = 0;

/// The pop-up windows of one `Window`, and which window each node is in.
#[derive(Debug, Default)]
pub struct Popups {
    open: HashMap<NodeId, Open>,
    /// The window each node was last sent to.
    owner: HashMap<NodeId, u32>,
}

impl Popups {
    /// Open, move or close windows to match the tree's pop-up roots.
    pub fn sync(&mut self, tree: &NodeTree, host: &mut dyn Host) {
        let want: HashMap<NodeId, (Spec, Rect)> = tree
            .popups()
            .filter_map(|(id, spec)| Some((id, (*spec, tree.get(id)?.rect()))))
            .collect();
        let gone: Vec<NodeId> = self.open.keys().filter(|id| !want.contains_key(id)).copied().collect();
        for id in gone {
            let o = self.open.remove(&id).unwrap();
            host.destroy(o.window);
            // Its nodes went with it: their removals go nowhere.
            for w in self.owner.values_mut().filter(|w| **w == o.window) {
                *w = GONE;
            }
        }
        for (id, (spec, rect)) in want {
            match self.open.get_mut(&id) {
                Some(o) if o.spec == spec && o.rect.size == rect.size && !o.dismissed => o.rect = rect,
                Some(o) => {
                    match host.place(o.window, &spec, rect) {
                        Ok(()) => *o = Open { spec, rect, dismissed: false, ..*o },
                        Err(e) => log::warn!("popup: place failed ({e})"),
                    }
                }
                None => match host.create(&spec, rect) {
                    Ok(window) => {
                        self.open.insert(id, Open { window, spec, rect, dismissed: false });
                    }
                    // Refused (the window is not active): the menu stays
                    // closed-looking but open; the next press closes it.
                    Err(e) => log::warn!("popup: create refused ({e})"),
                },
            }
        }
    }

    /// Split a frame's deltas by window: `main`'s, then each pop-up's
    /// (its root moved to the window's origin). A node that moved between
    /// windows is cleared from the one it left.
    pub fn route(&mut self, deltas: Vec<NodeDelta>, tree: &NodeTree, main: u32) -> Vec<(u32, Vec<NodeDelta>)> {
        let mut out: Vec<(u32, Vec<NodeDelta>)> = vec![(main, Vec::new())];
        let alive: HashMap<u32, ()> = self.open.values().map(|o| (o.window, ())).collect();
        let push = |w: u32, d: NodeDelta, out: &mut Vec<(u32, Vec<NodeDelta>)>| {
            match out.iter_mut().find(|(x, _)| *x == w) {
                Some((_, v)) => v.push(d),
                None => out.push((w, vec![d])),
            }
        };
        for d in deltas {
            match d {
                NodeDelta::Added { id, node, group, order } | NodeDelta::Changed { id, node, group, order } => {
                    let root = tree.root_of(id);
                    let w = self.open.get(&root).map_or(main, |o| o.window);
                    let prev = self.owner.insert(id, w);
                    if let Some(prev) = prev.filter(|&p| p != w && (p == main || alive.contains_key(&p))) {
                        push(prev, NodeDelta::Removed { id }, &mut out);
                    }
                    let mut node = node;
                    if root == id && w != main {
                        if let Node::Stack { rect, .. } = &mut node {
                            rect.origin = crate::geom::Point::new(0.0, 0.0);
                        }
                    }
                    // Whether it is new to this window decides nothing on
                    // the wire (set adds or replaces), so keep the kind.
                    push(w, NodeDelta::Changed { id, node, group, order }, &mut out);
                }
                NodeDelta::Removed { id } => {
                    let w = self.owner.remove(&id).unwrap_or(main);
                    if w == main || alive.contains_key(&w) {
                        push(w, NodeDelta::Removed { id }, &mut out);
                    }
                }
            }
        }
        out
    }

    /// Whether `window` is one of the pop-ups.
    pub fn is_popup(&self, window: u32) -> bool {
        self.open.values().any(|o| o.window == window)
    }

    /// frescod dismissed `window`: its root, to close.
    pub fn dismissed(&mut self, window: u32) -> Option<NodeId> {
        let (id, o) = self.open.iter_mut().find(|(_, o)| o.window == window)?;
        o.dismissed = true;
        Some(*id)
    }

    /// A point in pop-up `window`'s coordinates, in the tree's.
    pub fn to_tree(&self, window: u32, x: f32, y: f32) -> Option<(f32, f32)> {
        let o = self.open.values().find(|o| o.window == window)?;
        Some((x + o.rect.x(), y + o.rect.y()))
    }

    /// The pop-up windows open now.
    pub fn windows(&self) -> impl Iterator<Item = u32> + '_ {
        self.open.values().map(|o| o.window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        next: u32,
        log: Vec<String>,
    }
    impl Host for Fake {
        fn create(&mut self, s: &Spec, r: Rect) -> io::Result<u32> {
            self.next += 1;
            self.log.push(format!("create {} {:?} {}x{}", self.next + 100, s.side, r.w(), r.h()));
            Ok(self.next + 100)
        }
        fn place(&mut self, w: u32, _: &Spec, r: Rect) -> io::Result<()> {
            self.log.push(format!("place {w} {}x{}", r.w(), r.h()));
            Ok(())
        }
        fn destroy(&mut self, w: u32) {
            self.log.push(format!("destroy {w}"));
        }
    }

    fn spec() -> Spec {
        Spec { anchor: Rect::new(10.0, 10.0, 100.0, 30.0), side: Side::Below, align: Align::Start, grab: true }
    }

    #[test]
    fn a_popup_root_gets_a_window_its_subtree_and_back_on_close() {
        let mut tree = NodeTree::new();
        let body = tree.insert(None, Node::stack(crate::geom::Axis::Vertical, Rect::new(0.0, 0.0, 300.0, 200.0), 0.0));
        let pop = tree.insert(None, Node::stack(crate::geom::Axis::Vertical, Rect::new(10.0, 44.0, 160.0, 80.0), 0.0));
        let item = tree.insert(Some(pop), Node::Rect { rect: Rect::new(14.0, 48.0, 152.0, 28.0), fill: crate::Color::TRANSPARENT, radius: 0.0 });
        tree.set_popup(pop, spec());
        let mut host = Fake::default();
        let mut p = Popups::default();
        p.sync(&tree, &mut host);
        assert_eq!(host.log, ["create 101 Below 160x80"]);
        let deltas = crate::node::diff(&NodeTree::new(), &tree);
        let out = p.route(deltas, &tree, 7);
        let main: Vec<NodeId> = out[0].1.iter().map(delta_id).collect();
        assert_eq!(out[0].0, 7);
        assert_eq!(main, [body]);
        assert_eq!(out[1].0, 101);
        let popped: Vec<NodeId> = out[1].1.iter().map(delta_id).collect();
        assert_eq!(popped, [pop, item]);
        // The root at the pop-up's origin; children keep tree coordinates
        // (the wire makes them relative to their group).
        assert!(matches!(&out[1].1[0], NodeDelta::Changed { node: Node::Stack { rect, .. }, .. } if rect.x() == 0.0 && rect.y() == 0.0));
        // Pointer input maps back into the tree.
        assert_eq!(p.to_tree(101, 5.0, 6.0), Some((15.0, 50.0)));

        // Dismissed, then still rendered: placed again. Closed: destroyed,
        // and its nodes' removals go nowhere.
        assert_eq!(p.dismissed(101), Some(pop));
        p.sync(&tree, &mut host);
        assert_eq!(host.log.last().unwrap(), "place 101 160x80");
        let mut closed = NodeTree::new();
        closed.insert(None, Node::stack(crate::geom::Axis::Vertical, Rect::new(0.0, 0.0, 300.0, 200.0), 0.0));
        p.sync(&closed, &mut host);
        assert_eq!(host.log.last().unwrap(), "destroy 101");
        let out = p.route(crate::node::diff(&tree, &closed), &closed, 7);
        assert_eq!(out.len(), 1);
        assert!(out[0].1.is_empty(), "{:?}", out[0].1);
    }

    fn delta_id(d: &NodeDelta) -> NodeId {
        match d {
            NodeDelta::Added { id, .. } | NodeDelta::Changed { id, .. } | NodeDelta::Removed { id } => *id,
        }
    }
}
