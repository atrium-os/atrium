//! `hello` — phase 0 smoke harness for the View → NodeTree commit cycle.
//!
//! Builds a tiny scene-graph using only the foundations shipped in
//! phase 0 (geometry, theme tokens, `View` trait, `Node`/`NodeTree`)
//! and dumps the resulting tree to stdout. No fresco-server required;
//! wire emission lands in phase 4.
//!
//! What we're proving here:
//!   - The View trait composes cleanly
//!   - Theme tokens flow through the render pass
//!   - The NodeTree captures structure correctly (parent → children)
//!   - The whole crate compiles + runs
//!
//! Run: `cargo run --example hello -p pergola`

use pergola::geom::{Axis, Rect};
use pergola::node::Node;
use pergola::theme::{radius, space, Semantic};
use pergola::view::{Ctx, View};
use pergola::{render, NodeId};

/// A minimal hand-written View: panel containing two stacked rectangles.
struct LoginPlaceholder;

impl View for LoginPlaceholder {
    fn render(&self, ctx: &mut Ctx) {
        // Outer panel (raised surface, rounded).
        let panel_bg = ctx.theme.bg_elevated();
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, 480.0, 320.0),
            fill: panel_bg,
            radius: radius::XL,
        });

        // Stack container for content.
        ctx.push(Node::Stack {
            rect: Rect::new(space::LG, space::LG, 480.0 - 2.0 * space::LG, 320.0 - 2.0 * space::LG),
            axis: Axis::Vertical,
            spacing: space::MD,
        });

        // Two placeholder children — text + button slots, before
        // those primitives exist. The accent rectangle stands in
        // for the primary "Sign in" button to show accent flow.
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, 32.0),
            fill: ctx.theme.bg_surface(),
            radius: radius::SM,
        });
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, 32.0),
            fill: ctx.theme.accent_fg(),
            radius: radius::SM,
        });

        ctx.pop(); // stack
        ctx.pop(); // panel
    }
}

fn main() {
    env_logger::init();

    let tree = render(&LoginPlaceholder, Semantic::LIGHT);

    println!("Pergola phase 0 — render pass produced {} nodes:\n", tree.len());
    print_subtree(&tree, NodeId::ROOT, 0);
}

fn print_subtree(tree: &pergola::NodeTree, id: NodeId, depth: usize) {
    let indent = "  ".repeat(depth);
    if let Some(node) = tree.get(id) {
        match node {
            Node::Rect { rect, fill, radius } => println!(
                "{indent}[{:>2}] Rect rect=({:.0},{:.0} {:.0}×{:.0}) radius={:.0}px fill=rgba({:.2},{:.2},{:.2},{:.2})",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), radius, fill.r, fill.g, fill.b, fill.a,
            ),
            Node::Stack { rect, axis, spacing } => println!(
                "{indent}[{:>2}] Stack rect=({:.0},{:.0} {:.0}×{:.0}) axis={:?} spacing={:.0}px",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), axis, spacing,
            ),
        }
    }
    for child in tree.children_of(id) {
        print_subtree(tree, *child, depth + 1);
    }
}
