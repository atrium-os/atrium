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
use pergola::theme::{font, palette, radius, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface, TextStyle};

fn apps_dir() -> PathBuf {
    std::env::var("FORUM_APPS_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(APPS_DIR))
}

const SCREEN_W: f32 = 1280.0;
const SCREEN_H: f32 = 720.0;
const TILE: f32 = 60.0;
const GAP: f32 = 16.0;
const PAD: f32 = 16.0;

/// The dock: the signature teal desktop with a rounded dock panel of app tiles
/// centred at the bottom. Each tile is a rounded square with the app's initial — an
/// icon placeholder until real icon assets ship. All from theme tokens.
struct DockView {
    apps: Vec<AppEntry>,
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
            // The app's initial, centred-ish in the tile.
            let initial = app.name.chars().next().unwrap_or('?').to_uppercase().to_string();
            let label_color = if i == 0 {
                pergola::color::Color::rgba(1.0, 1.0, 1.0, 1.0)
            } else {
                t.text_primary()
            };
            ctx.add(Node::Text {
                rect: Rect::new(tx + TILE * 0.5 - 8.0, ty + TILE * 0.5 - 14.0, 0.0, 0.0),
                content: initial,
                style: TextStyle {
                    family: font::SANS.into(),
                    size: type_size::XL,
                    weight: Weight::Semibold,
                    color: label_color,
                },
            });
        }

        ctx.pop();
    }
}

/// Sample apps when no real catalog is installed (so the dock renders standalone).
fn sample_apps() -> Vec<AppEntry> {
    ["Edit", "Terminal", "Files", "Settings", "Browser"]
        .iter()
        .map(|n| AppEntry { id: format!("org.atrium.{}", n.to_lowercase()), name: (*n).into(), description: None })
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
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let mut conn = Connection::connect(&sock)?;
    let win = conn.window_create(SCREEN_W as u32, SCREEN_H as u32, "forum-dock", WindowHints::default())?;
    let mut surface = FrescoSurface::new(conn, win);

    let mut app = App::new(DockView { apps }).with_theme(mode);
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
