//! forum-bar — Forum's top status bar, drawn as a real Pergola UI.
//!
//! An ordinary graphics-only app (no window-management cap): it asks the WM core
//! over forum-ctl for the session's surfaces (focused app + window count) and draws
//! a themed top bar through Pergola → frescod. It is the first visible piece of the
//! Atrium desktop shell. (docs/spec/forum.md §3; visual: atrium-visual-language.md)
//!
//! Status source: forum-ctl ListSurfaces if FORUM_CTL_SOCKET is reachable, else a
//! sample so the bar renders standalone. Draws to FRESCO_SOCKET.

use std::time::{SystemTime, UNIX_EPOCH};

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use forum_ctl::{Intent, Reply};
use pergola::geom::Rect;
use pergola::theme::{font, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface, TextStyle};

const W: f32 = 1280.0;
const BAR_H: f32 = 36.0;

/// What the bar shows. Tiny data model; the WM core (forum-ctl) feeds it.
struct BarView {
    focus_app: String,
    windows: usize,
    clock: String,
}

impl View for BarView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;

        // Bar surface — a chrome neutral spanning the screen width.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, W, BAR_H),
            fill: t.bg_surface(),
            radius: 0.0,
        });
        // A 1px bottom hairline (elevation by a line, not a shadow — the doc's rule).
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, BAR_H - 1.0, W, 1.0),
            fill: t.border_default(),
            radius: 0.0,
        });

        let ty = (BAR_H - type_size::MD) * 0.5; // vertical centring (approx)

        // Left: the Atrium wordmark.
        ctx.add(Node::Text {
            rect: Rect::new(16.0, ty, 0.0, 0.0),
            content: "Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::MD,
                weight: Weight::Semibold,
                color: t.text_primary(),
            },
        });
        // The focused app, next to the wordmark.
        ctx.add(Node::Text {
            rect: Rect::new(86.0, ty + 1.0, 0.0, 0.0),
            content: format!("· {}", self.focus_app),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_secondary(),
            },
        });

        // Right cluster: window count + clock + an accent "session active" dot.
        ctx.add(Node::Text {
            rect: Rect::new(W - 250.0, ty + 1.0, 0.0, 0.0),
            content: format!("{} window{}", self.windows, if self.windows == 1 { "" } else { "s" }),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_secondary(),
            },
        });
        ctx.add(Node::Text {
            rect: Rect::new(W - 96.0, ty, 0.0, 0.0),
            content: self.clock.clone(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::MD,
                weight: Weight::Medium,
                color: t.text_primary(),
            },
        });
        // Session-active indicator — the single accent, used for meaning.
        ctx.add(Node::Rect {
            rect: Rect::new(W - 26.0, BAR_H * 0.5 - 4.0, 8.0, 8.0),
            fill: t.accent_fg(),
            radius: 4.0,
        });

        ctx.pop();
    }
}

/// Live status from the WM core, or a sample if forum-ctl isn't reachable.
fn status() -> (String, usize) {
    let sock = std::env::var("FORUM_CTL_SOCKET").unwrap_or_default();
    if !sock.is_empty() {
        if let Ok(Reply::Surfaces { surfaces, focus }) = forum_ctl::request(&sock, &Intent::ListSurfaces) {
            let app = surfaces.iter().find(|s| s.surface_id == focus)
                .map(|s| s.owner_app.clone())
                .unwrap_or_else(|| "—".into());
            return (app, surfaces.len());
        }
    }
    ("atrium-edit".into(), 3) // sample for standalone render
}

fn clock_hhmm() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

fn main() -> std::io::Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("dark") => Semantic::DARK,
        _ => Semantic::LIGHT,
    };
    let (focus_app, windows) = status();
    let mut conn = Connection::connect_default()?;
    let win = conn.window_create(W as u32, BAR_H as u32, "forum-bar", WindowHints::default())?;
    let mut surface = FrescoSurface::new(conn, win);

    let view = BarView { focus_app, windows, clock: clock_hhmm() };
    let mut app = App::new(view).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-bar: drawing the bar ({} node deltas)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;

    std::thread::sleep(std::time::Duration::from_secs(30));
    Ok(())
}
