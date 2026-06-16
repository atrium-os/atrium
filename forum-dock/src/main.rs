//! forum-dock — Forum's launcher, an ordinary graphics-only app.
//!
//! It holds no special capability. It lists the installed apps and, on activate,
//! REQUESTS a launch from portcullisd (the TCB), which does the verify → allocate
//! uid → register → jail dance. The dock never launches anything itself; launching
//! is a request authorized by the user's grants (docs/spec/forum.md §6).
//!
//! Usage:
//!   forum-dock              # list installed apps
//!   forum-dock launch <id>  # ask the TCB to launch app <id>
//!
//! Apps dir: FORUM_APPS_DIR (default /var/lib/atrium/apps).

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use forum_dock::{catalog, AppEntry, APPS_DIR};
use portcullis_ipc::{
    read_response, round_trip, send_fds, write_request, Request, Response, PROTO_VERSION,
    SOCKET_PATH,
};

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use pergola::geom::Rect;
use pergola::theme::{palette, radius, Semantic};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface};

fn apps_dir() -> PathBuf {
    std::env::var("FORUM_APPS_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(APPS_DIR))
}

const SCREEN_W: f32 = 1280.0;
const SCREEN_H: f32 = 720.0;
const TILE: f32 = 60.0;
const GAP: f32 = 16.0;
const PAD: f32 = 16.0;

// Lucide vector icons (ISC), embedded + parsed once into polylines. Drawn as stroked
// segments — resolution-independent, like everything in the vector scene graph.
const SVG_EDITOR: &str = include_str!("../../assets/icons/lucide/editor.svg");
const SVG_TERMINAL: &str = include_str!("../../assets/icons/lucide/terminal.svg");
const SVG_FILES: &str = include_str!("../../assets/icons/lucide/files.svg");
const SVG_SETTINGS: &str = include_str!("../../assets/icons/lucide/settings.svg");
const SVG_BROWSER: &str = include_str!("../../assets/icons/lucide/browser.svg");

struct Icons {
    editor: pergola::icon::IconGeometry,
    terminal: pergola::icon::IconGeometry,
    files: pergola::icon::IconGeometry,
    settings: pergola::icon::IconGeometry,
    browser: pergola::icon::IconGeometry,
}

impl Icons {
    fn load() -> Self {
        use pergola::icon::parse_icon;
        Icons {
            editor: parse_icon(SVG_EDITOR),
            terminal: parse_icon(SVG_TERMINAL),
            files: parse_icon(SVG_FILES),
            settings: parse_icon(SVG_SETTINGS),
            browser: parse_icon(SVG_BROWSER),
        }
    }
    /// Pick the app's icon: the manifest's declared `[app] icon` if present, else a
    /// keyword match on the id/name as a fallback. (A full system would resolve the
    /// declared name against the whole icon set / the app's bundled SVG; for now it
    /// maps onto the embedded dock set.)
    fn for_app(&self, app: &AppEntry) -> &pergola::icon::IconGeometry {
        let s = app.icon.clone()
            .unwrap_or_else(|| format!("{} {}", app.id, app.name))
            .to_lowercase();
        if s.contains("term") { &self.terminal }
        else if s.contains("file") || s.contains("folder") { &self.files }
        else if s.contains("setting") || s.contains("config") || s.contains("pref") { &self.settings }
        else if s.contains("brows") || s.contains("web") || s.contains("net") { &self.browser }
        else { &self.editor }
    }
}

/// The dock: the signature teal desktop with a rounded dock panel of app tiles
/// centred at the bottom. Each tile carries the app's Lucide vector icon. All from
/// theme tokens.
struct DockView {
    apps: Vec<AppEntry>,
    icons: Icons,
}

impl View for DockView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        // Desktop wallpaper.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, SCREEN_W, SCREEN_H),
            fill: palette::deep_teal(),
            radius: 0.0,
        });

        let n = self.apps.len().max(1) as f32;
        let dock_w = PAD * 2.0 + n * TILE + (n - 1.0) * GAP;
        let dock_h = PAD * 2.0 + TILE;
        let dock_x = (SCREEN_W - dock_w) * 0.5;
        let dock_y = SCREEN_H - dock_h - 28.0;

        // Dock panel — elevated, rounded.
        ctx.add(Node::Rect {
            rect: Rect::new(dock_x, dock_y, dock_w, dock_h),
            fill: t.bg_elevated(),
            radius: radius::LG,
        });

        for (i, app) in self.apps.iter().enumerate() {
            let tx = dock_x + PAD + i as f32 * (TILE + GAP);
            let ty = dock_y + PAD;
            // Tile — the first app uses the accent (as if focused/running); the rest a
            // recessed surface tone.
            let fill = if i == 0 { t.accent_fg() } else { t.bg_surface() };
            ctx.add(Node::Rect {
                rect: Rect::new(tx, ty, TILE, TILE),
                fill,
                radius: radius::MD,
            });
            // The app's vector icon, centred in the tile. White on the accent tile,
            // primary-ink on the neutral tiles.
            let icon_color = if i == 0 {
                pergola::color::Color::rgba(1.0, 1.0, 1.0, 1.0)
            } else {
                t.text_primary()
            };
            let icon_size = 30.0;
            let inset = (TILE - icon_size) * 0.5;
            pergola::icon::draw_icon(
                ctx,
                self.icons.for_app(app),
                tx + inset, ty + inset, icon_size,
                2.0,
                icon_color,
            );
        }

        ctx.pop();
    }
}

/// Sample apps when no real catalog is installed (so the dock renders standalone).
fn sample_apps() -> Vec<AppEntry> {
    [("Edit", "editor"), ("Terminal", "terminal"), ("Files", "files"),
     ("Settings", "settings"), ("Browser", "browser")]
        .iter()
        .map(|(n, icon)| AppEntry {
            id: format!("org.atrium.{}", n.to_lowercase()),
            name: (*n).into(),
            description: None,
            icon: Some((*icon).into()), // as if declared in the manifest
        })
        .collect()
}

fn render_dock() -> io::Result<()> {
    let mode = match std::env::var("FORUM_THEME").as_deref() {
        Ok("dark") => Semantic::DARK,
        _ => Semantic::LIGHT,
    };
    let mut apps = catalog(&apps_dir());
    if apps.is_empty() {
        apps = sample_apps();
    }
    let mut conn = Connection::connect_default()?;
    let win = conn.window_create(SCREEN_W as u32, SCREEN_H as u32, "forum-dock", WindowHints::default())?;
    let mut surface = FrescoSurface::new(conn, win);

    let mut app = App::new(DockView { apps, icons: Icons::load() }).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-dock: drawing the dock ({} node deltas)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;
    std::thread::sleep(std::time::Duration::from_secs(30));
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => render_dock(),
        Some("list") => {
            let apps = catalog(&apps_dir());
            if apps.is_empty() {
                println!("forum-dock: no apps installed under {}", apps_dir().display());
            }
            for a in apps {
                let desc = a.description.unwrap_or_default();
                println!("  {:<28} {:<20} {}", a.id, a.name, desc);
            }
            Ok(())
        }
        Some("launch") => {
            let id = args.get(1).cloned().unwrap_or_else(|| {
                eprintln!("usage: forum-dock launch <app-id>");
                std::process::exit(2);
            });
            launch(&id)
        }
        Some(other) => {
            eprintln!("forum-dock: unknown command '{other}' (try: list | launch <id>)");
            std::process::exit(2);
        }
    }
}

/// Request a launch from portcullisd. The dock is just the requester — the daemon
/// does the policy check, the per-app uid, the jail, the teardown. Mirrors the
/// portcullis CLI's wire dance: Hello → Launch → ReadyForFds → hand over stdio
/// (SCM_RIGHTS) → LaunchExit.
fn launch(app_id: &str) -> io::Result<()> {
    let mut s = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("forum-dock: portcullisd not reachable at {SOCKET_PATH}: {e}");
            std::process::exit(1);
        }
    };

    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION })? {
        Response::Hello { version } if version == PROTO_VERSION => {}
        Response::ProtoMismatch { server_version } => {
            eprintln!("forum-dock: portcullisd speaks proto v{server_version}, dock speaks v{PROTO_VERSION}");
            std::process::exit(1);
        }
        other => {
            eprintln!("forum-dock: unexpected handshake reply: {other:?}");
            std::process::exit(1);
        }
    }

    write_request(&mut s, &Request::Launch { app_id: app_id.into(), bypass_policy: false })?;
    match read_response(&mut s)? {
        Response::ReadyForFds => {}
        Response::LaunchNeedsApproval { delta } => {
            eprintln!("forum-dock: '{app_id}' needs the user's approval:");
            for line in delta { eprintln!("  - {line}"); }
            std::process::exit(1);
        }
        Response::LaunchFailed { stage, message } => {
            eprintln!("forum-dock: launch of '{app_id}' failed at {stage}: {message}");
            std::process::exit(1);
        }
        other => {
            eprintln!("forum-dock: unexpected pre-launch reply: {other:?}");
            std::process::exit(1);
        }
    }

    // Hand over our stdio to the launched app (the daemon is blocked in recv_fds).
    let stdio = [
        io::stdin().as_raw_fd(),
        io::stdout().as_raw_fd(),
        io::stderr().as_raw_fd(),
    ];
    send_fds(&s, &stdio)?;

    match read_response(&mut s)? {
        Response::LaunchExit { code } => {
            println!("forum-dock: '{app_id}' exited (code {code:?})");
            Ok(())
        }
        Response::LaunchFailed { stage, message } => {
            eprintln!("forum-dock: '{app_id}' failed at {stage}: {message}");
            std::process::exit(1);
        }
        other => {
            eprintln!("forum-dock: unexpected post-launch reply: {other:?}");
            std::process::exit(1);
        }
    }
}
