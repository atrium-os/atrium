//! `counter` — phase 1 smoke harness for the reactive cycle.
//!
//! Builds an app with a `Mutable<i32>` count. Drives 5 ticks; mutates
//! the count between ticks; prints the delta list emitted at each
//! tick. Demonstrates:
//!
//!   - Sync read (`count.get()`) inside `View::render`
//!   - Sync write (`count.set()`) anywhere
//!   - Dirty flag tripping the next tick
//!   - Diff producing minimal deltas (Added once, Changed per mutation)
//!
//! Run: `cargo run --example counter -p pergola`

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pergola::geom::{Axis, Rect};
use pergola::node::Node;
use pergola::theme::{palette, radius, space};
use pergola::view::Ctx;
use pergola::{App, Mutable, NodeDelta, View};

struct Counter {
    count: Mutable<i32>,
}

impl View for Counter {
    fn render(&self, ctx: &mut Ctx) {
        let n = self.count.get();

        ctx.push(Node::Stack {
            rect: Rect::new(0.0, 0.0, 240.0, 96.0),
            axis: Axis::Vertical,
            spacing: space::SM,
        });

        // The count is illustrated by the width of a bar — toy demo
        // for phase 1, before Text widgets exist. Wider = bigger n.
        // Color comes from the palette via theme tokens (here we read
        // the palette directly for emphasis).
        let bar_width = (n as f32).abs() * 16.0;
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, bar_width, 24.0),
            fill: ctx.theme.accent_fg(),
            radius: radius::SM,
        });

        // A second bar showing absolute count (always positive). This
        // exercises the diff: only the bar tied to `n` changes; the
        // static one shouldn't appear in any post-init delta.
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 64.0, 24.0),
            fill: palette::neutral_300(),
            radius: radius::SM,
        });

        ctx.pop();
    }
}

fn main() {
    env_logger::init();

    // Build the dirty flag first, then the cell wired to it, then the
    // view that holds the cell, then the app reusing the flag.
    // Chicken-and-egg solved without RefCells or factories.
    let dirty = Arc::new(AtomicBool::new(true));
    let count = Mutable::with_dirty(0i32, Arc::clone(&dirty));
    let mut app = App::new_with_flag(Counter { count: count.clone() }, dirty);

    for frame in 0..6 {
        let deltas = app.tick();
        println!(
            "frame {frame}: dirty_before={} count={} deltas={}",
            !deltas.is_empty(), count.get(), deltas.len(),
        );
        for d in &deltas {
            print_delta(d);
        }

        // Mutate before next tick to drive the demo. Real apps would
        // do this from event handlers, async tasks, or animation frames.
        match frame {
            0 => count.set(1),    // first real change
            1 => count.set(3),    // jump
            2 => {}               // no change → no delta
            3 => count.set(3),    // same value → still a "change" event
                                  //   from futures-signals, so dirty trips
                                  //   but diff yields no delta
            4 => count.set(10),   // big jump
            _ => {}
        }
    }
}

fn print_delta(d: &NodeDelta) {
    match d {
        NodeDelta::Added { id, parent, .. } => println!("  + Added id={} parent={:?}", id.0, parent.map(|p| p.0)),
        NodeDelta::Changed { id, .. } => println!("  ~ Changed id={}", id.0),
        NodeDelta::Removed { id } => println!("  - Removed id={}", id.0),
    }
}
