//! `vestibulum` — Atrium's login screen. The first real Pergola app.
//!
//! Opens a fresco-server connection, creates a window, builds a login
//! form (heading + username + password + Sign-in button), runs the
//! event loop. On submit, prints credentials + exits with status 0.
//! Real local-auth (pam_local equivalent) integrates at a later
//! milestone; for D2 the credential capture proves the toolkit
//! end-to-end on screen.
//!
//! Connection:
//!   FRESCO_SOCK env var, default `/tmp/frescod.sock`.
//!
//! Usage (in-VM):
//!   FRESCO_SOCK=/tmp/frescod.sock vestibulum
//!
//! Quit: Esc or close button.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use pergola::event::{Event, Key, KeyEventKind};
use pergola::geom::Rect;
use pergola::input::translate;
use pergola::node::Node;
use pergola::theme::{font, space, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::{
    commit, App, Button, FrescoSurface, Mutable, Surface, TextField, TextStyle,
};

/// Vestibulum runs *before* the window manager, so it owns the full
/// framebuffer. The smoke harness fixes 1280×720 (D2 lavapipe lane);
/// real frescod scanout will hand us the display dimensions through
/// the wire (WINDOW_CREATE response with 0×0 → fullscreen) — TODO.
const WIN_W: u32 = 1280;
const WIN_H: u32 = 720;

/// Form column — narrow, centered, no panel card. Text + inputs sit
/// directly on `bg_window` (Atrium teal); auto-contrast picks white
/// for the heading/subhead.
const FORM_W: f32 = 480.0;
const FORM_H: f32 = 360.0;
const FORM_X: f32 = (WIN_W as f32 - FORM_W) / 2.0;
const FORM_Y: f32 = (WIN_H as f32 - FORM_H) / 2.0;

#[derive(Clone)]
struct LoginView {
    username: Mutable<String>,
    password: Mutable<String>,
    status: Mutable<String>,
    /// Set true to break out of the event loop after a successful submit.
    done: Mutable<bool>,
}

impl View for LoginView {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;

        // Window background — Atrium signature deep teal, fullscreen.
        // No panel card: vestibulum is the only thing on screen until
        // login completes, so the form floats directly on the
        // chromatic background. Text + button colors are picked via
        // auto-contrast against `bg_window` so they remain readable
        // regardless of the background's luminance.
        ctx.tree.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, WIN_W as f32, WIN_H as f32),
            fill: theme.bg_window(),
            radius: 0.0,
        });
        let on_bg = theme.text_auto_on(theme.bg_window());

        // Heading.
        ctx.tree.insert(None, Node::Text {
            rect: Rect::new(FORM_X, FORM_Y, 0.0, 0.0),
            content: "Sign in to Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XL,
                weight: Weight::Semibold,
                color: on_bg,
            },
        });

        // Subhead / status line.
        ctx.tree.insert(None, Node::Text {
            rect: Rect::new(
                FORM_X,
                FORM_Y + type_size::XL + space::XS,
                0.0, 0.0,
            ),
            content: self.status.get_cloned(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: on_bg.with_alpha(0.85),
            },
        });

        // Username field.
        TextField::new(self.username.clone())
            .placeholder("username")
            .at(FORM_X, FORM_Y + 60.0)
            .width(FORM_W)
            .render(ctx);

        // Password field — masked.
        TextField::new(self.password.clone())
            .placeholder("password")
            .secret(true)
            .at(FORM_X, FORM_Y + 60.0 + 48.0)
            .width(FORM_W)
            .render(ctx);

        // Sign-in button.
        let username = self.username.clone();
        let password = self.password.clone();
        let status = self.status.clone();
        let done = self.done.clone();
        Button::primary("Sign in")
            .at(FORM_X, FORM_Y + 60.0 + 48.0 * 2.0 + space::MD)
            .width(FORM_W)
            .on_click(move || {
                let u = username.get_cloned();
                let p = password.get_cloned();
                if u.is_empty() || p.is_empty() {
                    status.set("enter both fields".into());
                    return;
                }
                // Hand the credential to ostiarius (the doorkeeper): it
                // authenticates and launches the human's jailed session
                // (Forum + chrome). On success vestibulum hands over the seat
                // and exits; on failure it shows the error and stays.
                match session_handoff(&u, &p) {
                    Ok(active) => {
                        status.set(format!("welcome, {active}"));
                        done.set(true);
                    }
                    Err(e) => status.set(e),
                }
            })
            .render(ctx);
    }
}

/// Hand the authenticated credential to ostiarius over its control socket
/// (`$OSTIARIUS_SOCK`, default `/atrium/sockets/ostiarius.sock`) — one
/// newline-delimited JSON `login` request, one reply. ostiarius authenticates +
/// launches the human's jailed session (Forum + chrome). Returns the active
/// human on success, or a human-readable error (which the form shows).
/// The default lives under /atrium/sockets so a jailed vestibulum (real root,
/// not path=/) reaches it via the same ro service-socket mount as frescod —
/// jaild scrubs env, so the in-jail process relies on this default.
fn session_handoff(user: &str, password: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let sock = std::env::var("OSTIARIUS_SOCK")
        .unwrap_or_else(|_| "/atrium/sockets/ostiarius.sock".to_string());
    let stream = UnixStream::connect(&sock)
        .map_err(|e| format!("session manager unavailable: {e}"))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;

    let req = serde_json::json!({
        "op": "login", "user": user, "password": password, "frontend": "gui",
    });
    let mut line = req.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp).map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(resp.trim()).map_err(|e| format!("bad reply: {e}"))?;
    match v.get("status").and_then(|s| s.as_str()) {
        Some("ok") => Ok(v
            .get("active")
            .and_then(|a| a.as_str())
            .unwrap_or(user)
            .to_string()),
        _ => Err(v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("login failed")
            .to_string()),
    }
}

fn main() -> std::io::Result<()> {
    env_logger::init();

    // $FRESCO_SOCK overrides (dev/harness); otherwise resolve like every other
    // client — connect_default() finds the canonical in-jail socket
    // (/atrium/sockets/fresco/fresco.sock) the graphics-cap mount provides, then
    // the dev fallback. (jaild doesn't pass FRESCO_SOCK into the jail, so the
    // booted-jailed vestibulum relies on this path.)
    // Connect to frescod. ostiarius gates our launch on frescod being ready
    // (its connect-probe), so the first attempt normally succeeds; the couple of
    // retries here only absorb a brief jitter window. We deliberately FAIL FAST
    // (3 tries, then die) rather than spin forever — vestibulum is a supervised
    // service (restart=on-crash), so a persistent failure is the supervisor's to
    // recover, not ours to mask.
    let connect = || match std::env::var("FRESCO_SOCK") {
        Ok(s) => Connection::connect(&s),
        Err(_) => Connection::connect_default(),
    };
    const TRIES: u32 = 3;
    let mut conn = {
        let mut attempt = 1;
        loop {
            match connect() {
                Ok(c) => break c,
                Err(_) if attempt < TRIES => {
                    log::info!("vestibulum: frescod not ready (try {attempt}/{TRIES}), retrying…");
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                Err(e) => {
                    log::error!("vestibulum: frescod unreachable after {TRIES} tries: {e}");
                    return Err(e);
                }
            }
        }
    };

    // Create a window.
    let window_id = conn.window_create(WIN_W, WIN_H, "Atrium", WindowHints::default())?;
    log::info!("vestibulum: window_id={window_id}");

    // Build app + view.
    let dirty = Arc::new(AtomicBool::new(true));
    let username = Mutable::with_dirty(String::new(), Arc::clone(&dirty));
    let password = Mutable::with_dirty(String::new(), Arc::clone(&dirty));
    let status   = Mutable::with_dirty("Use your local account password.".to_string(), Arc::clone(&dirty));
    let done     = Mutable::with_dirty(false, Arc::clone(&dirty));

    let view = LoginView {
        username: username.clone(),
        password: password.clone(),
        status,
        done: done.clone(),
    };

    let mut app = App::new_with_flag(view, dirty);
    let mut surface = FrescoSurface::new(conn, window_id);

    // Initial paint.
    let deltas = app.tick();
    commit(&mut surface, &deltas)?;
    surface.present()?;

    // Event loop. wait_event blocks; pass None for "no timeout."
    while !done.get() {
        let ev = match surface.connection().wait_event(None)? {
            Some(ev) => ev,
            None => continue,
        };

        // Handle window-lifecycle events at the loop level.
        match &ev {
            fresco_client::Event::CloseRequested { .. } => {
                log::info!("vestibulum: close requested, exiting");
                break;
            }
            fresco_client::Event::Key { hid_usage: 0x29, pressed: true, .. } => {
                // Esc — quit.
                log::info!("vestibulum: Esc pressed, exiting");
                break;
            }
            _ => {}
        }

        // Translate + dispatch into Pergola.
        if let Some(pergola_ev) = translate(&ev) {
            // Special case Tab to move focus between username/password.
            // (Phase-3.5+ Tab navigation lands toolkit-side; here we're
            // pre-empting it locally for the MVP.)
            if let Event::Key { kind: KeyEventKind::Down, key: Key::Tab, .. } = &pergola_ev {
                // No-op for now — focus management isn't yet
                // sequenced; the user clicks the desired field.
            }
            app.handle_event(pergola_ev);
        }

        // Re-render if anything changed.
        let deltas = app.tick();
        if !deltas.is_empty() {
            commit(&mut surface, &deltas)?;
            surface.present()?;
        }
    }

    log::info!("vestibulum: shutdown");
    Ok(())
}
