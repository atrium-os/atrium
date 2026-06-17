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
use pergola::theme::{font, radius, space, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{render, NodeId, TextStyle};

/// A minimal hand-written login-form sketch using the visual language
/// tokens. Layout pass turns the stack's children into placed rects.
struct LoginPlaceholder;

impl View for LoginPlaceholder {
    fn render(&self, ctx: &mut Ctx) {
        // Outer panel (raised surface, rounded).
        let panel_bg = ctx.theme.bg_elevated();
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, 480.0, 360.0),
            fill: panel_bg,
            radius: radius::XL,
        });

        // Stack container for content. Inset by space::LG.
        ctx.push(Node::Stack {
            rect: Rect::new(space::LG, space::LG, 480.0 - 2.0 * space::LG, 360.0 - 2.0 * space::LG),
            axis: Axis::Vertical,
            spacing: space::MD,
        });

        // Heading.
        ctx.add(Node::Text {
            rect: Rect::new(0.0, 0.0, 0.0, 0.0),  // size derived by layout
            content: "Sign in to Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XL,
                weight: Weight::Semibold,
                color: ctx.theme.text_primary(),
            },
        });

        // Subhead.
        ctx.add(Node::Text {
            rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            content: "Use your local account password.".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: ctx.theme.text_secondary(),
            },
        });

        // Two input slot placeholders + a primary action.
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, 32.0),
            fill: ctx.theme.bg_surface(),
            radius: radius::SM,
        });
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

    let (mut tree, _interactions) = render(&LoginPlaceholder, Semantic::LIGHT);
    // Run layout from each root.
    let roots: Vec<_> = tree.roots().collect();
    for root in roots {
        pergola::layout::layout(&mut tree, root);
    }

    println!("Pergola phase 2 — render + layout produced {} nodes:\n", tree.len());
    print_subtree(&tree, NodeId::ROOT, 0);
}

fn print_subtree(tree: &pergola::NodeTree, id: NodeId, depth: usize) {
    let indent = "  ".repeat(depth);
    if let Some(node) = tree.get(id) {
        match node {
            Node::Rect { rect, fill, radius } => println!(
                "{indent}[{:>2}] Rect ({:>3.0},{:>3.0} {:>3.0}×{:>3.0}) r={:.0} fill=rgba({:.2},{:.2},{:.2},{:.2})",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), radius, fill.r, fill.g, fill.b, fill.a,
            ),
            Node::Text { rect, content, style } => println!(
                "{indent}[{:>2}] Text ({:>3.0},{:>3.0} {:>3.0}×{:>3.0}) {:?}px {:?}  {:?}",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), style.size, style.weight, content,
            ),
            Node::Stack { rect, axis, spacing } => println!(
                "{indent}[{:>2}] Stack ({:>3.0},{:>3.0} {:>3.0}×{:>3.0}) axis={:?} sp={:.0}",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), axis, spacing,
            ),
            Node::Path { p0, p1, width, .. } => println!(
                "{indent}[{:>2}] Path ({:>3.0},{:>3.0})->({:>3.0},{:>3.0}) w={:.0}",
                id.0, p0.0, p0.1, p1.0, p1.1, width,
            ),
        }
    }
    for child in tree.children_of(id) {
        print_subtree(tree, *child, depth + 1);
    }
}
