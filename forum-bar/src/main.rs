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
use fresco_protocol::{WindowHints, WmRole};
use forum_ctl::{Intent, Reply};
use pergola::geom::Rect;
use pergola::theme::{font, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface, TextStyle};

/// The bar spans the FULL width of the display, so it has to know what that is.
/// It used to be a constant 1280, which on a 1920-wide screen left the last 640
/// px bare — the bar simply stopped two-thirds of the way across. Asked from
/// frescod now (FORUM_SCREEN overrides for dev; see the dock for why an env var
/// cannot be the mechanism for a jailed app).
const BAR_H: f32 = 36.0;

fn screen_w(conn: &mut Connection) -> f32 {
    if let Some(w) = std::env::var("FORUM_SCREEN").ok()
        .and_then(|s| s.split_once('x').and_then(|(a, _)| a.trim().parse().ok()))
    {
        eprintln!("forum-bar: width {w} (FORUM_SCREEN override)");
        return w;
    }
    match conn.display_info() {
        Ok(d) if d.width > 0 => {
            eprintln!("forum-bar: width {} (from frescod)", d.width);
            d.width as f32
        }
        Ok(_)  => { eprintln!("forum-bar: frescod reports no mode; assuming 1280"); 1280.0 }
        Err(e) => { eprintln!("forum-bar: display_info failed ({e}); assuming 1280"); 1280.0 }
    }
}

/// What the bar shows. Tiny data model; the WM core (forum-ctl) feeds it.
/// Live session state, as signals rather than plain values: the bar is session
/// chrome that stays up for the whole session, so every field it shows has to be
/// updatable without rebuilding the view. Setting a `Mutable` marks the app
/// dirty, which is what makes the next `tick()` re-render.
struct BarView {
    w:         f32,
    focus_app: pergola::reactive::Mutable<String>,
    windows:   pergola::reactive::Mutable<usize>,
    clock:     pergola::reactive::Mutable<String>,
}

impl View for BarView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;

        // Bar surface — a chrome neutral spanning the screen width.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, self.w, BAR_H),
            fill: t.bg_surface(),
            radius: 0.0,
        });
        // A 1px bottom hairline (elevation by a line, not a shadow — the doc's rule).
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, BAR_H - 1.0, self.w, 1.0),
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
            content: format!("· {}", self.focus_app.get_cloned()),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_secondary(),
            },
        });

        // Right cluster: window count + clock + an accent "session active" dot.
        ctx.add(Node::Text {
            rect: Rect::new(self.w - 250.0, ty + 1.0, 0.0, 0.0),
            content: { let n = self.windows.get(); format!("{n} window{}", if n == 1 { "" } else { "s" }) },
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_secondary(),
            },
        });
        ctx.add(Node::Text {
            rect: Rect::new(self.w - 96.0, ty, 0.0, 0.0),
            content: self.clock.get_cloned(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::MD,
                weight: Weight::Medium,
                color: t.text_primary(),
            },
        });
        // Session-active indicator — the single accent, used for meaning.
        ctx.add(Node::Rect {
            rect: Rect::new(self.w - 26.0, BAR_H * 0.5 - 4.0, 8.0, 8.0),
            fill: t.accent_fg(),
            radius: 4.0,
        });

        ctx.pop();
    }
}

/// Live status from the WM core, or a sample if forum-ctl isn't reachable.
fn status() -> (String, usize) {
    let sock = forum_ctl::default_socket_path();
    if let Ok(Reply::Surfaces { surfaces, focus }) = forum_ctl::request(&sock, &Intent::ListSurfaces) {
        let app = surfaces.iter().find(|s| s.surface_id == focus)
            .map(|s| s.owner_app.clone())
            .unwrap_or_else(|| "—".into());
        return (app, surfaces.len());
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
    // Declare the chrome role so the WM reserves the top edge for the bar
    // (rather than treating it as an ordinary document).
    let hints = WindowHints { role: Some(WmRole::Chrome), ..Default::default() };
    let w = screen_w(&mut conn);
    let win = conn.window_create(w as u32, BAR_H as u32, "forum-bar", hints)?;
    let mut surface = FrescoSurface::new(conn, win);

    // One dirty flag shared by the app and its signals, so `set()` on any of
    // them is what schedules the next repaint.
    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    use pergola::reactive::Mutable;
    let focus_sig  = Mutable::with_dirty(focus_app, dirty.clone());
    let window_sig = Mutable::with_dirty(windows,   dirty.clone());
    let clock_sig  = Mutable::with_dirty(clock_hhmm(), dirty.clone());
    let view = BarView {
        w,
        focus_app: focus_sig.clone(),
        windows:   window_sig.clone(),
        clock:     clock_sig.clone(),
    };
    let mut app = App::new_with_flag(view, dirty).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-bar: drawing the bar ({} node deltas)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;

    // ★ Stay up. The bar is session chrome, not a one-shot: it used to draw
    // once, sleep 30s and exit, so the desktop evaporated half a minute after
    // login. Poll once a second, but only COMMIT when something actually
    // changed — tick() returns no deltas while the app is clean, so an idle bar
    // does no compositing and lets the GPU stay gated (forum.md §2.5).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let now = clock_hhmm();
        if now != clock_sig.get_cloned() { clock_sig.set(now); }
        let (f, w) = status();
        if f != focus_sig.get_cloned()  { focus_sig.set(f); }
        if w != window_sig.get() { window_sig.set(w); }

        let deltas = app.tick();
        if deltas.is_empty() { continue; }
        commit(&mut surface, &deltas)?;
        surface.present()?;
    }
}
