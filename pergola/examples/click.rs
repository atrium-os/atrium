//! `click` — phase 3 smoke harness for input dispatch.
//!
//! A single-button view: pressing it increments a `Mutable<i32>`,
//! which changes the button's color (intensity rises with the count).
//! Drives synthetic `PointerDown`/`PointerUp` events to verify the
//! full event → hit-test → handler → mutate → re-render → wire-delta
//! cycle.
//!
//! Run: `cargo run --example click -p pergola`

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pergola::geom::{Point, Rect};
use pergola::node::Node;
use pergola::theme::{palette, radius};
use pergola::view::{Ctx, View};
use pergola::{commit, App, Event, LogSurface, Mutable, Surface};

struct ClickyButton {
    count: Mutable<i32>,
}

impl View for ClickyButton {
    fn render(&self, ctx: &mut Ctx) {
        let n = self.count.get();

        // The visible rect.
        let id = ctx.tree.insert(None, Node::Rect {
            rect: Rect::new(40.0, 40.0, 160.0, 48.0),
            // Color steps with the count: 0 = neutral, then deeper accent.
            fill: match n {
                0 => palette::neutral_300(),
                1 => palette::accent_200(),
                2 => palette::accent_300(),
                _ => palette::accent_400(),
            },
            radius: radius::SM,
        });

        // Attach the click handler. Each render rebuilds it; the
        // captured `Mutable` clone keeps the closure tied to the
        // same cell across re-renders.
        let count = self.count.clone();
        ctx.on_click(id, move || {
            count.set(count.get() + 1);
        });
    }
}

fn main() {
    env_logger::init();

    let dirty = Arc::new(AtomicBool::new(true));
    let count = Mutable::with_dirty(0i32, Arc::clone(&dirty));
    let mut app = App::new_with_flag(ClickyButton { count: count.clone() }, dirty);
    let mut surface = LogSurface::default();

    // Initial render — paint the button.
    let deltas = app.tick();
    commit(&mut surface, &deltas).unwrap();
    surface.present().unwrap();

    println!(">>> Sending click 1 (down at 100,60 → up at 100,60)");
    app.handle_event(Event::PointerDown { at: Point::new(100.0, 60.0) });
    app.handle_event(Event::PointerUp { at: Point::new(100.0, 60.0) });
    let deltas = app.tick();
    commit(&mut surface, &deltas).unwrap();

    println!(">>> Sending click 2 (down at 50,50 → up at 60,60 — same node)");
    app.handle_event(Event::PointerDown { at: Point::new(50.0, 50.0) });
    app.handle_event(Event::PointerUp { at: Point::new(60.0, 60.0) });
    let deltas = app.tick();
    commit(&mut surface, &deltas).unwrap();

    println!(">>> Sending miss (down at 300,300 — outside button)");
    app.handle_event(Event::PointerDown { at: Point::new(300.0, 300.0) });
    app.handle_event(Event::PointerUp { at: Point::new(300.0, 300.0) });
    let deltas = app.tick();
    if deltas.is_empty() {
        println!("    (no deltas — no handler fired, no state change)");
    } else {
        commit(&mut surface, &deltas).unwrap();
    }

    println!(">>> Sending press-then-drag-out (down at 100,60 → up at 300,300)");
    app.handle_event(Event::PointerDown { at: Point::new(100.0, 60.0) });
    app.handle_event(Event::PointerUp { at: Point::new(300.0, 300.0) });
    let deltas = app.tick();
    if deltas.is_empty() {
        println!("    (no deltas — press-then-release-elsewhere doesn't fire click)");
    } else {
        commit(&mut surface, &deltas).unwrap();
    }

    println!("\nfinal count: {}", count.get());
}
