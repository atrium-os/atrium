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
use pergola::geom::{Axis, Rect};
use pergola::input::translate;
use pergola::node::Node;
use pergola::theme::{font, radius, space, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::{
    commit, App, Button, FrescoSurface, Mutable, Surface, TextField, TextStyle,
};

const WIN_W: u32 = 640;
const WIN_H: u32 = 520;

const PANEL_W: f32 = 480.0;
const PANEL_H: f32 = 360.0;
const PANEL_X: f32 = (WIN_W as f32 - PANEL_W) / 2.0;
const PANEL_Y: f32 = (WIN_H as f32 - PANEL_H) / 2.0;

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

        // Canvas background — neutral_50 covers the rest of the window.
        ctx.tree.insert(None, Node::Rect {
            rect: Rect::new(0.0, 0.0, WIN_W as f32, WIN_H as f32),
            fill: theme.bg_canvas(),
            radius: 0.0,
        });

        // Panel.
        let panel = ctx.tree.insert(None, Node::Rect {
            rect: Rect::new(PANEL_X, PANEL_Y, PANEL_W, PANEL_H),
            fill: theme.bg_elevated(),
            radius: radius::XL,
        });

        // Inner stack (heading + status + fields placed by hand below).
        let _ = ctx.tree.insert(Some(panel), Node::Stack {
            rect: Rect::new(
                PANEL_X + space::LG, PANEL_Y + space::LG,
                PANEL_W - 2.0 * space::LG, 60.0,
            ),
            axis: Axis::Vertical,
            spacing: space::XS,
        });

        // Heading.
        ctx.tree.insert(Some(panel), Node::Text {
            rect: Rect::new(PANEL_X + space::LG, PANEL_Y + space::LG, 0.0, 0.0),
            content: "Sign in to Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XL,
                weight: Weight::Semibold,
                color: theme.text_primary(),
            },
        });

        // Subhead / status line.
        ctx.tree.insert(Some(panel), Node::Text {
            rect: Rect::new(
                PANEL_X + space::LG,
                PANEL_Y + space::LG + type_size::XL + space::XS,
                0.0, 0.0,
            ),
            content: self.status.get_cloned(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: theme.text_secondary(),
            },
        });

        // Username field.
        TextField::new(self.username.clone())
            .placeholder("username")
            .at(PANEL_X + space::LG, PANEL_Y + space::LG + 60.0)
            .width(PANEL_W - 2.0 * space::LG)
            .render(ctx);

        // Password field.
        TextField::new(self.password.clone())
            .placeholder("password")
            .at(PANEL_X + space::LG, PANEL_Y + space::LG + 60.0 + 48.0)
            .width(PANEL_W - 2.0 * space::LG)
            .render(ctx);

        // Sign-in button.
        let username = self.username.clone();
        let password = self.password.clone();
        let status = self.status.clone();
        let done = self.done.clone();
        Button::primary("Sign in")
            .at(PANEL_X + space::LG, PANEL_Y + space::LG + 60.0 + 48.0 * 2.0 + space::MD)
            .width(PANEL_W - 2.0 * space::LG)
            .on_click(move || {
                let u = username.get_cloned();
                let p = password.get_cloned();
                if u.is_empty() || p.is_empty() {
                    status.set("enter both fields".into());
                    return;
                }
                println!("vestibulum: submit user={u:?} (password elided)");
                let _ = p; // would be passed to pam_local
                status.set(format!("welcome, {u}"));
                done.set(true);
            })
            .render(ctx);
    }
}

fn main() -> std::io::Result<()> {
    env_logger::init();

    let socket_path = std::env::var("FRESCO_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod.sock".to_string());
    log::info!("vestibulum: connecting to {socket_path}");
    let mut conn = Connection::connect(&socket_path)?;

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
