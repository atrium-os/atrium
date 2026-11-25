//! The inspector (dev-tools.md §3.6, D23–D25): an IDE that started a dev
//! build of an app reads its UI, read-only. It asks for the node tree
//! (each node's kind, box, colours and fonts, and the app's source line
//! that made it), outlines a node or every box, and picks the node under
//! the next press.
//!
//! The app's side is in dev builds only (`debug_assertions`): it finds the
//! IDE's socket at the descriptor `ATRIUM_INSPECT_FD` names, which
//! portcullisd puts there for a Run from Artifex and nothing else does.
//! The wire is shared: frames of a little-endian `u32` length then
//! postcard, each way, bounded by [`MAX_FRAME`].

use std::collections::HashSet;

use crate::geom::{Point, Rect};
use crate::node::{Node, NodeId, NodeTree};

pub use atrium_inspect::{encode, Ask, Decoder, Item, Site, Tell, MAX_BOUNDS, MAX_FRAME, MAX_ITEMS};
#[cfg(debug_assertions)]
pub use atrium_inspect::Link;

/// Longest text quoted in a node's detail, in characters.
const MAX_QUOTE: usize = 120;
/// Longest source path sent, in bytes.
const MAX_PATH: usize = 1024;

/// Whether `at` is in Pergola's own source: what a widget draws inside
/// itself belongs to the app's line that drew the widget.
///
/// This file is the exception: it makes nodes with a `Ctx` only in its
/// tests, which stand in for an app.
pub fn is_pergola(at: &std::panic::Location<'_>) -> bool {
    let here = file!();
    let dir = here.strip_suffix("inspect.rs").unwrap_or(here);
    at.file().starts_with(dir) && at.file() != here
}

fn hex(c: crate::Color) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    if b(c.a) == 255 {
        format!("#{:02x}{:02x}{:02x}", b(c.r), b(c.g), b(c.b))
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", b(c.r), b(c.g), b(c.b), b(c.a))
    }
}

fn quote(s: &str) -> String {
    let mut q: String = s.chars().take(MAX_QUOTE).collect();
    if s.chars().count() > MAX_QUOTE {
        q.push('…');
    }
    format!("{q:?}")
}

/// A node's kind and detail line.
pub fn describe(node: &Node) -> (&'static str, String) {
    match node {
        Node::Rect { fill, radius, .. } => ("Rect", format!("fill {} radius {radius}", hex(*fill))),
        Node::Text { content, style, .. } => ("Text", format!("{} {} {}pt weight {} {}",
            quote(content), style.family, style.size, style.weight as u16, hex(style.color))),
        Node::Stack { axis, spacing, fill, radius, opacity, clip, .. } => {
            let mut d = format!("{} spacing {spacing}", match axis { crate::geom::Axis::Horizontal => "row", crate::geom::Axis::Vertical => "column" });
            if let Some(f) = fill {
                d.push_str(&format!(" fill {} radius {radius}", hex(*f)));
            }
            if *opacity != 1.0 {
                d.push_str(&format!(" opacity {opacity}"));
            }
            if *clip {
                d.push_str(" clips");
            }
            ("Stack", d)
        }
        Node::Scroll { offset, offset_x, zoom, content_h, .. } =>
            ("Scroll", format!("at {offset_x}, {offset} of {content_h} high, zoom {zoom}")),
        Node::Path { width, color, .. } => ("Line", format!("{width}pt {}", hex(*color))),
        Node::Glyphs { size, color, glyphs, .. } => ("Glyphs", format!("{} glyphs {size}pt {}", glyphs.len(), hex(*color))),
        Node::Fill(_) => ("Shape", "a filled path".into()),
        Node::Image { image, .. } => { let d = image.data(); ("Image", format!("{}×{} px", d.width, d.height)) }
    }
}

fn site_of(tree: &NodeTree, id: NodeId) -> Option<Site> {
    tree.site(id).map(|l| Site {
        file: l.file().chars().take(MAX_PATH).collect(),
        line: l.line(),
        col: l.column(),
    })
}

fn rect4(r: Rect) -> [f32; 4] { [r.x(), r.y(), r.w(), r.h()] }

/// The tree as drawn, leaving out `skip` (the inspector's own outlines).
pub fn snapshot(tree: &NodeTree, skip: &HashSet<NodeId>) -> Vec<Item> {
    let mut out = Vec::new();
    // A node a widget put in the tree itself, not through `Ctx`, is its
    // parent's.
    fn walk(t: &NodeTree, id: NodeId, parent: Option<u32>, depth: u16, inherited: Option<&Site>, skip: &HashSet<NodeId>, out: &mut Vec<Item>) {
        if out.len() >= MAX_ITEMS || skip.contains(&id) {
            return;
        }
        let Some(node) = t.get(id) else { return };
        let (kind, detail) = describe(node);
        let site = site_of(t, id).or_else(|| inherited.cloned());
        out.push(Item {
            id: id.0, parent, depth, kind: kind.into(),
            rect: crate::slot::window_rect(t, id).map(rect4),
            detail, site: site.clone(),
        });
        for c in t.children_of(id) {
            walk(t, *c, Some(id.0), depth.saturating_add(1), site.as_ref(), skip, out);
        }
    }
    for r in tree.roots().collect::<Vec<_>>() {
        walk(tree, r, None, 0, None, skip, &mut out);
    }
    out
}

/// The node a press at `p` (window points) is on: the deepest, last drawn
/// node whose box holds it, whatever its kind.
pub fn pick(tree: &NodeTree, p: Point, skip: &HashSet<NodeId>) -> Option<NodeId> {
    fn walk(t: &NodeTree, id: NodeId, p: Point, skip: &HashSet<NodeId>) -> Option<NodeId> {
        if skip.contains(&id) {
            return None;
        }
        for c in t.children_of(id).iter().rev() {
            if let Some(hit) = walk(t, *c, p, skip) {
                return Some(hit);
            }
        }
        crate::slot::window_rect(t, id).filter(|r| r.contains(p)).map(|_| id)
    }
    tree.roots().collect::<Vec<_>>().into_iter().rev().find_map(|r| walk(tree, r, p, skip))
}

/// What the inspector outlines in the app's window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Marks {
    pub highlight: Option<NodeId>,
    pub bounds: bool,
}

impl Marks {
    pub fn any(&self) -> bool { self.highlight.is_some() || self.bounds }
}

/// Draw `marks` over everything in `tree` (after layout), returning the
/// nodes added, which [`snapshot`] and [`pick`] then leave out.
pub fn draw_marks(tree: &mut NodeTree, marks: &Marks) -> HashSet<NodeId> {
    let mut boxes: Vec<(Rect, crate::Color, f32)> = Vec::new();
    if marks.bounds {
        let ids: Vec<NodeId> = tree.iter().map(|(id, _)| id).collect();
        for id in ids {
            if boxes.len() >= MAX_BOUNDS {
                break;
            }
            if let Some(r) = crate::slot::window_rect(tree, id) {
                boxes.push((r, crate::Color::rgba(0.15, 0.45, 0.95, 0.55), 1.0));
            }
        }
    }
    if let Some(r) = marks.highlight.and_then(|id| crate::slot::window_rect(tree, id)) {
        boxes.push((r, crate::Color::rgba(0.88, 0.16, 0.55, 1.0), 2.0));
    }
    let mut added = HashSet::new();
    for (r, color, width) in boxes {
        let (x0, y0, x1, y1) = (r.x(), r.y(), r.x() + r.w(), r.y() + r.h());
        for (p0, p1) in [((x0, y0), (x1, y0)), ((x1, y0), (x1, y1)), ((x1, y1), (x0, y1)), ((x0, y1), (x0, y0))] {
            added.insert(tree.insert(None, Node::Path { p0, p1, width, color }));
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{Ctx, View};

    struct Ui;
    impl View for Ui {
        fn render(&self, ctx: &mut Ctx) {
            ctx.push(Node::vstack(Rect::new(0.0, 0.0, 100.0, 40.0), 4.0));
            ctx.add(Node::Rect { rect: Rect::new(10.0, 10.0, 50.0, 20.0), fill: crate::Color::rgba(1.0, 0.0, 0.0, 1.0), radius: 2.0 });
            ctx.pop();
            crate::widgets::Button::primary("Go").at(120.0, 10.0).render(ctx);
        }
    }

    fn tree() -> NodeTree {
        let mut prev = NodeTree::new();
        crate::view::render_continuing(&Ui, crate::theme::Semantic::LIGHT, Default::default(), &mut prev).0
    }

    #[test]
    fn the_link_hears_asks_and_drops_a_peer_that_breaks_the_wire() {
        use std::io::{Read, Write};
        let (app_end, mut ide) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut l = Link::new(app_end).unwrap();
        assert_eq!(l.poll().unwrap(), vec![], "nothing yet, and no wait");
        ide.write_all(&encode(&Ask::Pick(true)).unwrap()).unwrap();
        ide.write_all(&encode(&Ask::Tree).unwrap()).unwrap();
        assert_eq!(l.poll().unwrap(), vec![Ask::Pick(true), Ask::Tree]);
        l.send(&Tell::Picked(9));
        let mut got = [0u8; 64];
        let n = ide.read(&mut got).unwrap();
        let mut d = Decoder::default();
        d.push(&got[..n]);
        assert_eq!(d.next::<Tell>().unwrap(), Some(Tell::Picked(9)));
        ide.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert!(l.poll().is_err(), "an oversized frame drops it");
        let (app_end, ide) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut l = Link::new(app_end).unwrap();
        drop(ide);
        assert!(l.poll().is_err(), "the IDE went");
    }

    #[test]
    fn frames_round_trip_and_bad_ones_are_refused() {
        let mut d = Decoder::default();
        let f = encode(&Ask::Highlight(Some(7))).unwrap();
        d.push(&f[..3]);
        assert_eq!(d.next::<Ask>().unwrap(), None, "half a frame waits");
        d.push(&f[3..]);
        d.push(&encode(&Ask::Bounds(true)).unwrap());
        assert_eq!(d.next::<Ask>().unwrap(), Some(Ask::Highlight(Some(7))));
        assert_eq!(d.next::<Ask>().unwrap(), Some(Ask::Bounds(true)));
        let mut big = Decoder::default();
        big.push(&((MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(big.next::<Ask>().is_err());
        let mut junk = Decoder::default();
        junk.push(&[1, 0, 0, 0, 0xff]);
        assert!(junk.next::<Ask>().is_err());
    }

    #[test]
    fn each_node_is_put_down_to_the_apps_own_line() {
        let t = tree();
        let items = snapshot(&t, &HashSet::new());
        let rect = items.iter().find(|i| i.kind == "Rect" && i.detail.starts_with("fill #ff0000")).expect("the rect");
        let site = rect.site.as_ref().expect("a dev build records where");
        assert!(site.file.ends_with("inspect.rs"), "{site:?}");
        // The rect is made two lines below the stack, in `Ui::render`.
        let stack = items.iter().find(|i| i.kind == "Stack" && i.parent.is_none()).unwrap();
        assert_eq!(site.line, stack.site.as_ref().unwrap().line + 1);
        // What the Button draws is the line that drew the Button, not
        // Pergola's own.
        let label = items.iter().find(|i| i.kind == "Text" && i.detail.starts_with("\"Go\"")).expect("its label");
        assert_eq!(label.site.as_ref().unwrap().line, site.line + 2, "{label:?}");
        assert!(rect.rect.is_some_and(|r| r[2] > 0.0 && r[3] > 0.0), "{rect:?}");
        assert!(rect.parent == Some(stack.id) && rect.depth == 1);
    }

    #[test]
    fn a_pick_finds_the_deepest_box_and_marks_are_left_out() {
        let mut t = tree();
        let items = snapshot(&t, &HashSet::new());
        let rect = items.iter().find(|i| i.kind == "Rect" && i.detail.starts_with("fill #ff0000")).unwrap();
        let [x, y, w, h] = rect.rect.unwrap();
        let at = Point::new(x + w / 2.0, y + h / 2.0);
        assert_eq!(pick(&t, at, &HashSet::new()), Some(NodeId(rect.id)));
        let n = t.len();
        let marks = draw_marks(&mut t, &Marks { highlight: Some(NodeId(rect.id)), bounds: false });
        assert_eq!(marks.len(), 4, "four sides");
        assert_eq!(t.len(), n + 4);
        assert_eq!(pick(&t, at, &marks), Some(NodeId(rect.id)), "not the outline over it");
        assert_eq!(snapshot(&t, &marks).len(), items.len());
        let all = draw_marks(&mut tree(), &Marks { highlight: None, bounds: true });
        assert!(all.len() >= 4 * 3, "{}", all.len());
    }
}
