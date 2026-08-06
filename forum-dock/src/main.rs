//! forum-dock — Forum's launcher, an ordinary graphics-only app.
//!
//! It holds `app-launch` — the grant to ASK portcullisd for the installed-app
//! catalog and to request a launch. That is all it is: the requester. portcullisd
//! (the TCB) still does the verify → allocate uid → register → jail dance, and the
//! user's grants still authorize it (docs/spec/forum.md §6). The capability exists
//! because a jailed dock can reach the daemon no other way, and because the catalog
//! coming over that socket means the launcher never needs the app tree mounted.
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
use fresco_protocol::{WindowHints, WmRole};
use pergola::geom::Rect;
use pergola::theme::{palette, radius, Semantic};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface};

fn apps_dir() -> PathBuf {
    std::env::var("FORUM_APPS_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(APPS_DIR))
}

/// The output geometry — ASKED, not assumed.
///
/// The dock fills the screen (it paints the wallpaper) and anchors its launcher
/// to the bottom edge, so a wrong screen size does not just look off, it puts
/// the launcher outside the display: hardcoded 1280x720 on a 640x480 scanout
/// placed the strip at y=600, below the bottom, and the desktop came up with no
/// visible dock at all.
///
/// FRESCO_SCREEN/FORUM_SCREEN still override for dev, but they cannot be the
/// mechanism: portcullisd launches an app through jail(8), which does not carry
/// the environment in, so a jailed dock can only be told by asking frescod.
fn screen_wh(conn: &mut Connection) -> (f32, f32) {
    if let Some((w, h)) = std::env::var("FORUM_SCREEN").ok().and_then(|s| {
        let (a, b) = s.split_once('x')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    }) {
        eprintln!("forum-dock: screen {w}x{h} (FORUM_SCREEN override)");
        return (w, h);
    }
    match conn.display_info() {
        Ok(d) if d.width > 0 && d.height > 0 => {
            eprintln!("forum-dock: screen {}x{} @ {} mHz (from frescod)",
                      d.width, d.height, d.refresh_mhz);
            (d.width as f32, d.height as f32)
        }
        Ok(_) => { eprintln!("forum-dock: frescod reports no mode; assuming 1280x720");
                   (1280.0, 720.0) }
        Err(e) => { eprintln!("forum-dock: display_info failed ({e}); assuming 1280x720");
                    (1280.0, 720.0) }
    }
}

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
    w: f32,
    h: f32,
}

impl View for DockView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        // Desktop wallpaper.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, self.w, self.h),
            fill: palette::deep_teal(),
            radius: 0.0,
        });

        let n = self.apps.len().max(1) as f32;
        let dock_w = PAD * 2.0 + n * TILE + (n - 1.0) * GAP;
        let dock_h = PAD * 2.0 + TILE;
        let dock_x = (self.w - dock_w) * 0.5;
        let dock_y = self.h - dock_h - 28.0;

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
    // ★ Ask the daemon first, filesystem second. A jailed dock has no app tree
    // (deliberately), so the old filesystem-only scan came back empty and the
    // fallback below quietly drew FAKE apps — a launcher that looks like it is
    // working. Say what happened instead of papering over it.
    let (mut apps, src) = forum_dock::catalog_resolved(&apps_dir());
    match (&src, apps.is_empty()) {
        (forum_dock::CatalogSource::Unavailable, _) => eprintln!(
            "forum-dock: NO CATALOG — portcullisd is not reachable and {} is not \
             readable. Drawing placeholders; nothing here can launch. (A jailed \
             dock needs the `app-launch` capability.)",
            apps_dir().display()
        ),
        (_, true) => eprintln!("forum-dock: catalog from {src:?} is empty — no apps installed"),
        (_, false) => eprintln!("forum-dock: {} app(s) from {src:?}", apps.len()),
    }
    if apps.is_empty() {
        apps = sample_apps();
    }
    let mut conn = Connection::connect_default()?;
    // The dock draws the full-screen wallpaper + the dock panel, so it's the
    // back/background surface — the WM places it behind everything; the dock
    // panel it paints in the bottom strip stays visible under documents.
    let hints = WindowHints { role: Some(WmRole::Background), ..Default::default() };
    let (sw, sh) = screen_wh(&mut conn);
    let win = conn.window_create(sw as u32, sh as u32, "forum-dock", hints)?;
    let mut surface = FrescoSurface::new(conn, win);

    let mut app = App::new(DockView { apps, icons: Icons::load(), w: sw, h: sh }).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-dock: drawing the dock ({} node deltas)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;

    // ★ Stay up. The dock paints the wallpaper and the launcher strip — it IS
    // the desktop background, so exiting takes the desktop with it. It used to
    // draw once, sleep 30s and quit. Its content is static (the catalog only
    // changes on install/uninstall), so unlike the bar it has nothing to
    // repaint: park instead of polling, and do no compositing at all while
    // idle. A redraw comes from the WM re-declaring the layout, not from here.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => render_dock(),
        Some("list") => {
            let (apps, src) = forum_dock::catalog_resolved(&apps_dir());
            if apps.is_empty() {
                println!("forum-dock: no apps ({src:?}) — daemon unreachable or nothing installed");
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
    // Resolve the daemon's socket rather than hardcoding the flat path: inside a
    // jail only the per-service directory is mounted (by `app-launch`), and the
    // flat /atrium/sockets/portcullis.sock does not exist there at all.
    let Some(path) = portcullis_ipc::resolve_socket_path() else {
        eprintln!("forum-dock: portcullisd not reachable (looked for {} and {}). \
                   A jailed dock needs the `app-launch` capability.",
                  portcullis_ipc::SERVICE_SOCKET_PATH, SOCKET_PATH);
        std::process::exit(1);
    };
    let mut s = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("forum-dock: portcullisd not reachable at {path}: {e}");
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
        /* The daemon answers this BEFORE ReadyForFds — it decides "already
         * running" up front, without mounting anything — so the second click on
         * a dock icon lands here, not on the post-launch path. Raise the window
         * that exists instead of reporting a failure. */
        Response::AlreadyRunning { jid, uid, .. } => {
            println!("forum-dock: '{app_id}' is already running (jid {jid})");
            match focus_running(uid) {
                // Ack means the WM ACCEPTED the intent, not that focus moved:
                // roles that never take focus (a panel, the background) are
                // declined by the layout policy, which is correct. Say what we
                // actually know.
                Ok(Some(sid)) => println!("forum-dock: asked the WM to focus its surface {sid}"),
                Ok(None)      => println!("forum-dock: it has no surface to focus"),
                Err(e)        => eprintln!("forum-dock: could not focus it: {e}"),
            }
            return Ok(());
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

/// Ask forum-wm to focus the surface owned by `uid` — the running instance of
/// an app we were asked to launch a second time.
///
/// Matching on `owner_uid` is what makes this work from inside a jail: the dock
/// cannot read the launch registry, but every surface already carries the uid
/// that created it, and portcullisd told us which uid to look for. Returns the
/// focused surface id, or `None` if the app has not created a window (yet).
fn focus_running(uid: u32) -> io::Result<Option<u32>> {
    use std::os::unix::net::UnixStream;
    let path = forum_ctl::default_socket_path();
    let mut s = UnixStream::connect(&path)?;
    let req = forum_ctl::encode(&forum_ctl::Intent::ListSurfaces)
        .map_err(|e| io::Error::other(format!("encode: {e}")))?;
    forum_ctl::write_frame(&mut s, &req)?;
    let reply: forum_ctl::Reply = forum_ctl::decode(&forum_ctl::read_frame(&mut s)?)
        .map_err(|e| io::Error::other(format!("decode: {e}")))?;
    let surfaces = match reply {
        forum_ctl::Reply::Surfaces { surfaces, .. } => surfaces,
        forum_ctl::Reply::Err { message } => return Err(io::Error::other(message)),
        other => return Err(io::Error::other(format!("unexpected reply: {other:?}"))),
    };
    let Some(target) = surfaces.iter().find(|s| s.owner_uid == uid) else {
        return Ok(None);
    };
    /* One connection per intent — the control protocol is request/response. */
    let mut s = UnixStream::connect(&path)?;
    let req = forum_ctl::encode(&forum_ctl::Intent::Focus { surface_id: target.surface_id })
        .map_err(|e| io::Error::other(format!("encode: {e}")))?;
    forum_ctl::write_frame(&mut s, &req)?;
    match forum_ctl::decode::<forum_ctl::Reply>(&forum_ctl::read_frame(&mut s)?) {
        Ok(forum_ctl::Reply::Ack)              => Ok(Some(target.surface_id)),
        Ok(forum_ctl::Reply::Err { message })  => Err(io::Error::other(message)),
        Ok(other)                              => Err(io::Error::other(format!("{other:?}"))),
        Err(e)                                 => Err(io::Error::other(format!("decode: {e}"))),
    }
}
