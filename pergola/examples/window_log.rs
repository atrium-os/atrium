//! `window_log` — phase 4 smoke harness: full render → layout → diff →
//! wire-emit cycle, with the LogSurface dumping what would have gone
//! to fresco-server.
//!
//! Drives 3 frames of the login-form view used in `examples/hello.rs`,
//! flipping a Mutable to demonstrate that only changed nodes appear
//! on the wire after the first frame.
//!
//! Run: `cargo run --example window_log -p pergola`

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pergola::geom::{Axis, Rect};
use pergola::node::Node;
use pergola::theme::{font, radius, space, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, LogSurface, Mutable, Surface, TextStyle};

struct LoginForm {
    /// Drives the primary action's fill: amber-bronze when "ready,"
    /// muted when "submitting." Toy demonstration of reactivity.
    submitting: Mutable<bool>,
}

impl View for LoginForm {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;

        // Outer panel.
        ctx.push(Node::Rect {
            rect: Rect::new(80.0, 80.0, 480.0, 360.0),
            fill: theme.bg_elevated(),
            radius: radius::XL,
        });

        // Inner stack.
        ctx.push(Node::Stack {
            rect: Rect::new(80.0 + space::LG, 80.0 + space::LG, 480.0 - 2.0 * space::LG, 360.0 - 2.0 * space::LG),
            axis: Axis::Vertical,
            spacing: space::MD,
        });

        ctx.add(Node::Text {
            rect: Rect::ZERO_SIZED,
            content: "Sign in to Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XL,
                weight: Weight::Semibold,
                color: theme.text_primary(),
            },
        });
        ctx.add(Node::Text {
            rect: Rect::ZERO_SIZED,
            content: "Use your local account password.".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: theme.text_secondary(),
            },
        });
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, 32.0),
            fill: theme.bg_surface(),
            radius: radius::SM,
        });
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, 32.0),
            fill: theme.bg_surface(),
            radius: radius::SM,
        });

        // Primary action: fill flips between accent and muted as the
        // submitting Mutable changes.
        let action_fill = if self.submitting.get() {
            theme.text_disabled()
        } else {
            theme.accent_fg()
        };
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, 32.0),
            fill: action_fill,
            radius: radius::SM,
        });

        ctx.pop();
        ctx.pop();
    }
}

fn main() {
    env_logger::init();

    let dirty = Arc::new(AtomicBool::new(true));
    let submitting = Mutable::with_dirty(false, Arc::clone(&dirty));
    let mut app = App::new_with_flag(LoginForm { submitting: submitting.clone() }, dirty);
    let mut surface = LogSurface::default();

    for frame in 0..3 {
        let deltas = app.tick();
        if !deltas.is_empty() {
            commit(&mut surface, &deltas).expect("LogSurface never fails");
            surface.present().unwrap();
        } else {
            println!("--- frame {frame}: clean, no wire traffic ---\n");
        }

        match frame {
            0 => submitting.set(true),    // press "Sign in"
            1 => submitting.set(false),   // failure → re-enable
            _ => {}
        }
    }
}
