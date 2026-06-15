//! forum-demo — pipe-clean the full UI render path with a real Pergola app.
//!
//! This is what a Forum chrome app *is*: a program that links Pergola (the UI
//! library), describes its UI, and lets Pergola translate that into the fresco
//! retained scene graph and drive the connection. We send a small rect composition
//! (a mock dock panel — text is a follow-up, glyph runs are still TODO in Pergola's
//! wire path) and frescod renders it. No Vulkan/Tier-2/Tier-3 in this app — it's all
//! scenegraph, exactly per the UI render model.
//!
//! Run against frescod-vulkan-smoke (which renders each frame to a PNG via the
//! lavapipe software Vulkan ICD): FRESCO_SOCKET=/tmp/frescod-smoke.sock forum-demo

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use pergola::color::Color;
use pergola::geom::Rect;
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface};

const W: f32 = 480.0;
const H: f32 = 320.0;

/// A mock dock panel: a dark window, a teal title strip, three rounded "app icon"
/// squares, and an accent strip — all solid-fill rects, so it exercises the
/// Pergola → scene_node_rect → frescod render path end to end.
struct DockPanel;

impl View for DockPanel {
    fn render(&self, ctx: &mut Ctx) {
        // Window background (dark), with the rest as its children.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, W, H),
            fill: Color::rgba(0.10, 0.11, 0.13, 1.0),
            radius: 0.0,
        });
        // Title strip (teal).
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, W, 44.0),
            fill: Color::rgba(0.12, 0.45, 0.50, 1.0),
            radius: 0.0,
        });
        // Three app-icon squares.
        ctx.add(Node::Rect {
            rect: Rect::new(24.0, 88.0, 96.0, 96.0),
            fill: Color::rgba(0.90, 0.30, 0.35, 1.0),
            radius: 16.0,
        });
        ctx.add(Node::Rect {
            rect: Rect::new(192.0, 88.0, 96.0, 96.0),
            fill: Color::rgba(0.35, 0.75, 0.45, 1.0),
            radius: 16.0,
        });
        ctx.add(Node::Rect {
            rect: Rect::new(360.0, 88.0, 96.0, 96.0),
            fill: Color::rgba(0.30, 0.55, 0.95, 1.0),
            radius: 16.0,
        });
        // Accent strip near the bottom.
        ctx.add(Node::Rect {
            rect: Rect::new(24.0, 240.0, W - 48.0, 32.0),
            fill: ctx.theme.bg_elevated(),
            radius: 8.0,
        });
        ctx.pop();
    }
}

fn main() -> std::io::Result<()> {
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    eprintln!("forum-demo: connecting to {sock}");
    let mut conn = Connection::connect(&sock)?;
    let win = conn.window_create(W as u32, H as u32, "forum-demo", WindowHints::default())?;

    let mut surface = FrescoSurface::new(conn, win);
    let mut app = App::new(DockPanel);
    let deltas = app.tick();
    eprintln!("forum-demo: committing {} node delta(s) to window {win}", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;
    eprintln!("forum-demo: presented — holding the connection so the server can render.");

    // Keep the connection open: frescod drops a client's surface on disconnect.
    std::thread::sleep(std::time::Duration::from_secs(30));
    Ok(())
}
