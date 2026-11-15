//! A window to launch from the dock in checks (scripts/vm-apps-grid-jail-check.sh):
//! an ordinary Pergola app that draws its argv[1] (or its binary's name) and
//! stays up until frescod closes it.

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::{SizeSpec, Window, WindowDesc};
use pergola::{App, TextStyle};

struct Sample(String);

impl View for Sample {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, 4000.0, 4000.0), fill: t.bg_canvas(), radius: 0.0 });
        let style = TextStyle { family: font::SANS.into(), size: type_size::XL, weight: Weight::Semibold, color: t.text_primary() };
        ctx.add(Node::Text { rect: Rect::new(40.0, 40.0, 0.0, 40.0), content: self.0.clone(), style });
        ctx.pop();
    }
}

fn main() -> std::io::Result<()> {
    // Named after its binary (bin/<name> in each test bundle).
    let name = std::env::args().nth(1).or_else(|| {
        std::env::current_exe().ok()?.file_stem().map(|s| s.to_string_lossy().into_owned())
    }).unwrap_or_else(|| "Sample app".into());
    let win = Window::open(&WindowDesc::new(name.clone(), SizeSpec::Fixed(640, 400)))?;
    eprintln!("sample_app: '{name}' up (pid {})", std::process::id());
    let mut app = App::new(Sample(name));
    win.run(&mut app, |_, _, _| {})
}
