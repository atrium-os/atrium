//! forum-overview — Forum's window switcher, drawn as a real Pergola UI.
//!
//! An ordinary graphics-only app (no window-management cap): it asks the WM core over
//! forum-ctl to list the session's surfaces, draws them as a grid of window cards
//! (with each app's vector icon), and on a pick asks the core to focus one. The core
//! holds the cap; the chrome is unprivileged (docs/spec/forum.md §3).
//!
//! Usage:
//!   forum-overview            # draw the switcher
//!   forum-overview list       # print the surfaces
//!   forum-overview focus <id> # ask the core to focus surface <id>

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use forum_ctl::{Intent, Reply};
use pergola::geom::Rect;
use pergola::theme::{font, palette, radius, space, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface, TextStyle};

const W: f32 = 1280.0;
const H: f32 = 720.0;

fn socket() -> String {
    std::env::var("FORUM_CTL_SOCKET").unwrap_or_else(|_| "/tmp/forum-ctl.sock".into())
}

// Lucide icons, shared with the dock (ISC). Embedded + parsed once.
const SVG_EDITOR: &str = include_str!("../../assets/icons/lucide/editor.svg");
const SVG_TERMINAL: &str = include_str!("../../assets/icons/lucide/terminal.svg");
const SVG_FILES: &str = include_str!("../../assets/icons/lucide/files.svg");
const SVG_SETTINGS: &str = include_str!("../../assets/icons/lucide/settings.svg");
const SVG_BROWSER: &str = include_str!("../../assets/icons/lucide/browser.svg");

struct Icons([Vec<pergola::icon::Polyline>; 5]);
impl Icons {
    fn load() -> Self {
        use pergola::icon::parse_icon;
        Icons([
            parse_icon(SVG_EDITOR), parse_icon(SVG_TERMINAL), parse_icon(SVG_FILES),
            parse_icon(SVG_SETTINGS), parse_icon(SVG_BROWSER),
        ])
    }
    fn for_app(&self, key: &str) -> &[pergola::icon::Polyline] {
        let s = key.to_lowercase();
        if s.contains("term") { &self.0[1] }
        else if s.contains("file") || s.contains("folder") { &self.0[2] }
        else if s.contains("setting") || s.contains("pref") { &self.0[3] }
        else if s.contains("brows") || s.contains("web") { &self.0[4] }
        else { &self.0[0] }
    }
}

/// One window in the switcher.
struct Win { title: String, key: String, focused: bool }

struct OverviewView { windows: Vec<Win>, icons: Icons }

impl View for OverviewView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        // Wallpaper.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, W, H),
            fill: palette::deep_teal(),
            radius: 0.0,
        });
        // Title — light ink on the teal.
        ctx.add(Node::Text {
            rect: Rect::new(W * 0.5 - 90.0, 96.0, 0.0, 0.0),
            content: "Open Windows".into(),
            style: TextStyle {
                family: font::SANS.into(), size: type_size::XXL, weight: Weight::Semibold,
                color: pergola::color::Color::rgba(1.0, 1.0, 1.0, 0.95),
            },
        });

        let n = self.windows.len().max(1) as f32;
        let (cw, ch, gap) = (300.0f32, 200.0f32, space::XL);
        let total = n * cw + (n - 1.0) * gap;
        let x0 = (W - total) * 0.5;
        let y0 = (H - ch) * 0.5 + 20.0;

        for (i, win) in self.windows.iter().enumerate() {
            let cx = x0 + i as f32 * (cw + gap);
            // Focus ring — an accent rect 4px proud behind the focused card.
            if win.focused {
                ctx.add(Node::Rect {
                    rect: Rect::new(cx - 4.0, y0 - 4.0, cw + 8.0, ch + 8.0),
                    fill: t.accent_fg(),
                    radius: radius::XL,
                });
            }
            // Card.
            ctx.add(Node::Rect {
                rect: Rect::new(cx, y0, cw, ch),
                fill: t.bg_elevated(),
                radius: radius::LG,
            });
            // App icon, centred-upper.
            let isz = 56.0;
            pergola::icon::draw_icon(
                ctx, self.icons.for_app(&win.key),
                cx + cw * 0.5 - isz * 0.5, y0 + 36.0, isz, 2.5, t.text_primary(),
            );
            // Title under the icon.
            ctx.add(Node::Text {
                rect: Rect::new(cx + 20.0, y0 + ch - 44.0, 0.0, 0.0),
                content: win.title.clone(),
                style: TextStyle {
                    family: font::SANS.into(), size: type_size::MD, weight: Weight::Medium,
                    color: t.text_primary(),
                },
            });
        }
        ctx.pop();
    }
}

/// Live windows from the WM core, or a sample so it renders standalone.
fn windows() -> Vec<Win> {
    if let Ok(Reply::Surfaces { surfaces, focus }) = forum_ctl::request(&socket(), &Intent::ListSurfaces) {
        if !surfaces.is_empty() {
            return surfaces.into_iter().map(|s| Win {
                title: s.owner_app.clone(), key: s.owner_app, focused: s.surface_id == focus,
            }).collect();
        }
    }
    [("Atrium Edit", "org.atrium.edit", true), ("Terminal", "org.atrium.terminal", false),
     ("Files", "org.atrium.files", false)]
        .iter().map(|(t, k, f)| Win { title: (*t).into(), key: (*k).into(), focused: *f }).collect()
}

fn render_overview() -> std::io::Result<()> {
    let mode = match std::env::var("FORUM_THEME").as_deref() {
        Ok("dark") => Semantic::DARK, _ => Semantic::LIGHT,
    };
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let mut conn = Connection::connect(&sock)?;
    let win = conn.window_create(W as u32, H as u32, "forum-overview", WindowHints::default())?;
    let mut surface = FrescoSurface::new(conn, win);

    let mut app = App::new(OverviewView { windows: windows(), icons: Icons::load() }).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-overview: drawing the switcher ({} node deltas)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;
    std::thread::sleep(std::time::Duration::from_secs(30));
    Ok(())
}

fn print_or_focus(args: &[String]) -> std::io::Result<()> {
    let intent = match args.first().map(String::as_str) {
        Some("focus") => {
            let id: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                eprintln!("usage: forum-overview focus <surface_id>"); std::process::exit(2);
            });
            Intent::Focus { surface_id: id }
        }
        _ => Intent::ListSurfaces,
    };
    match forum_ctl::request(&socket(), &intent)? {
        Reply::Surfaces { surfaces, focus } => {
            println!("forum-overview: {} surface(s) (focus={})", surfaces.len(), focus);
            for s in surfaces {
                let star = if s.surface_id == focus { '*' } else { ' ' };
                println!("  {star} #{:<3} {:<24} {:?}", s.surface_id, s.owner_app, s.role);
            }
        }
        Reply::Ack => println!("forum-overview: focus applied"),
        Reply::Err { message } => { eprintln!("forum-overview: core refused: {message}"); std::process::exit(1); }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => render_overview(),
        _ => print_or_focus(&args),
    }
}
