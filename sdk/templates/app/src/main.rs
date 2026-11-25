//! {{title}}: a window with a counter (template: app).
//!
//! The process this file builds is the app's **core**: it holds the
//! capabilities in atrium.toml. Keep it to your own logic and data. When it
//! has to read something that came from outside (a file the person picked,
//! bytes from the network, another app's message), parse it in a worker or
//! use a system service instead (app-composition.md); `cargo atrium check`
//! tells you when a parser ends up here.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Button;
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

/// The window's size, in points.
const SIZE: (u32, u32) = (360, 160);

struct Main {
    count: Mutable<u32>,
}

impl View for Main {
    fn render(&self, ctx: &mut Ctx) {
        let n = self.count.get();
        // The window's own background: without it the window is see-through.
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, SIZE.0 as f32, SIZE.1 as f32), fill: ctx.theme.bg_canvas(), radius: 0.0 });
        ctx.add(Node::Text {
            rect: Rect::new(24.0, 24.0, 0.0, 0.0),
            content: format!("Pressed {n} time{}", if n == 1 { "" } else { "s" }),
            style: TextStyle { family: font::SANS.into(), size: type_size::XL, weight: Weight::Semibold, color: ctx.theme.text_primary() },
        });
        let count = self.count.clone();
        Button::primary("Press").at(24.0, 72.0).width(120.0).on_click(move || count.set(count.get() + 1)).render(ctx);
    }
}

fn main() {
    let mut win = match Window::open(&WindowDesc::new("{{title}}", SizeSpec::Fixed(SIZE.0, SIZE.1))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{{name}}: no display: {e}");
            std::process::exit(1);
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let count = Mutable::with_dirty(0u32, dirty.clone());
    let mut app = App::new_with_flag(Main { count }, dirty);
    loop {
        match win.step(&mut app, Some(Duration::from_millis(250))) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("{{name}}: {e}");
                std::process::exit(1);
            }
        }
    }
}
