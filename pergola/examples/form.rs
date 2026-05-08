//! `form` — phase 5 smoke harness for Button + TextField with focus
//! and key dispatch.
//!
//! Two TextFields (username, password) and a primary Button. Drives
//! synthetic events:
//!   - click each TextField in turn → focus moves
//!   - send characters → fields fill in
//!   - click the Button → fires its handler
//!
//! Run: `cargo run --example form -p pergola`

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pergola::event::{Key, KeyEventKind, Modifiers};
use pergola::geom::{Axis, Point, Rect};
use pergola::node::Node;
use pergola::theme::{font, radius, space, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::{
    commit, App, Button, Event, LogSurface, Mutable, Surface, TextField, TextStyle,
};

#[derive(Clone)]
struct LoginView {
    username: Mutable<String>,
    password: Mutable<String>,
    submitted: Mutable<bool>,
}

impl View for LoginView {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;

        // Outer panel.
        let panel = ctx.tree.insert(None, Node::Rect {
            rect: Rect::new(80.0, 80.0, 480.0, 360.0),
            fill: theme.bg_elevated(),
            radius: radius::XL,
        });
        // Inner stack.
        let stack = ctx.tree.insert(Some(panel), Node::Stack {
            rect: Rect::new(
                80.0 + space::LG,
                80.0 + space::LG,
                480.0 - 2.0 * space::LG,
                360.0 - 2.0 * space::LG,
            ),
            axis: Axis::Vertical,
            spacing: space::MD,
        });
        let _ = stack;
        // Heading.
        ctx.tree.insert(Some(stack), Node::Text {
            rect: Rect::ZERO_SIZED,
            content: "Sign in to Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XL,
                weight: Weight::Semibold,
                color: theme.text_primary(),
            },
        });

        // Render the two TextFields and the Button as composed
        // sub-views. We construct them here per-frame; their state
        // (Mutables) is held by us across renders.
        let inner_x = 80.0 + space::LG;
        let inner_w = 480.0 - 2.0 * space::LG;

        TextField::new(self.username.clone())
            .placeholder("username")
            .at(inner_x, 80.0 + space::LG + 60.0)
            .width(inner_w)
            .render(ctx);

        TextField::new(self.password.clone())
            .placeholder("password")
            .at(inner_x, 80.0 + space::LG + 60.0 + 48.0)
            .width(inner_w)
            .render(ctx);

        // Submit button.
        let submitted = self.submitted.clone();
        Button::primary("Sign in")
            .at(inner_x, 80.0 + space::LG + 60.0 + 48.0 * 2.0 + 16.0)
            .width(inner_w)
            .on_click(move || submitted.set(true))
            .render(ctx);
    }
}

fn modifiers() -> Modifiers { Modifiers::default() }

fn type_chars(app: &mut App<LoginView>, s: &str) {
    for c in s.chars() {
        app.handle_event(Event::Key {
            kind: KeyEventKind::Down,
            key: Key::Char,
            modifiers: modifiers(),
            chars: c.to_string(),
        });
    }
}

fn main() {
    env_logger::init();

    let dirty = Arc::new(AtomicBool::new(true));
    let username = Mutable::with_dirty(String::new(), Arc::clone(&dirty));
    let password = Mutable::with_dirty(String::new(), Arc::clone(&dirty));
    let submitted = Mutable::with_dirty(false, Arc::clone(&dirty));

    let view = LoginView {
        username: username.clone(),
        password: password.clone(),
        submitted: submitted.clone(),
    };

    let mut app = App::new_with_flag(view, dirty);
    let mut surface = LogSurface::default();

    // Initial paint.
    let deltas = app.tick();
    println!("--- initial paint ({} nodes) ---", deltas.len());
    commit(&mut surface, &deltas).unwrap();
    surface.present().unwrap();

    // The two TextField backgrounds are at roughly:
    //   username y = 80+24+60 = 164  (height 32) → click at (200, 180)
    //   password y = 80+24+60+48 = 212 (height 32) → click at (200, 228)
    println!(">>> Click username field");
    app.handle_event(Event::PointerDown { at: Point::new(200.0, 180.0) });
    app.handle_event(Event::PointerUp { at: Point::new(200.0, 180.0) });
    println!("    focused = {:?}", app.focused());

    println!(">>> Type \"alice\" into username");
    type_chars(&mut app, "alice");
    let _ = app.tick();   // re-render after each set is fine; here we batch
    println!("    username = {:?}", username.get_cloned());

    println!(">>> Click password field");
    app.handle_event(Event::PointerDown { at: Point::new(200.0, 228.0) });
    app.handle_event(Event::PointerUp { at: Point::new(200.0, 228.0) });
    println!("    focused = {:?}", app.focused());

    println!(">>> Type \"hunter2\" into password");
    type_chars(&mut app, "hunter2");
    let _ = app.tick();
    println!("    password = {:?}", password.get_cloned());

    println!(">>> Backspace once");
    app.handle_event(Event::Key {
        kind: KeyEventKind::Down, key: Key::Backspace,
        modifiers: modifiers(), chars: String::new(),
    });
    let _ = app.tick();
    println!("    password = {:?}", password.get_cloned());

    println!(">>> Click Sign in (primary button at y≈276)");
    app.handle_event(Event::PointerDown { at: Point::new(200.0, 280.0) });
    app.handle_event(Event::PointerUp { at: Point::new(200.0, 280.0) });
    let _ = app.tick();
    println!("    submitted = {}", submitted.get());
    println!("    final state: u={:?} p={:?}",
             username.get_cloned(), password.get_cloned());
}
