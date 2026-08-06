//! forum-demo — the §12 reference render from `docs/design/atrium-visual-language.md`.
//!
//! "Before any widget code, Pergola produces one screen showing the language
//! standing alone." This is that screen, built entirely from theme tokens
//! (rev. 1): the shell wallpaper + hairline grid, one elevated panel, the type
//! scale doing the work, the three button variants, and an input carrying the
//! focus ring. If this screen reads wrong, fix the tokens — not the widgets.
//!
//! Run against frescod-vulkan-smoke (renders each frame to a PNG via lavapipe):
//!   FRESCO_SOCKET=/tmp/frescod-smoke.sock forum-demo [light|dark]

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use pergola::geom::Rect;
use pergola::theme::{font, radius, shell, size, space, stroke, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface, TextStyle};

const W: f32 = 1280.0;
const H: f32 = 720.0;
const PANEL_W: f32 = 460.0;
const PANEL_H: f32 = 272.0;

struct ReferenceScreen;

/// A filled rect with a 1px border, faked as two fills until an outline
/// pass exists (M2). Good enough for the token gate.
fn bordered(ctx: &mut Ctx, r: Rect, border: pergola::color::Color, fill: pergola::color::Color, rad: f32) {
    ctx.add(Node::Rect { rect: r, fill: border, radius: rad });
    ctx.add(Node::Rect {
        rect: Rect::new(
            r.origin.x + stroke::DEFAULT,
            r.origin.y + stroke::DEFAULT,
            r.size.w - 2.0 * stroke::DEFAULT,
            r.size.h - 2.0 * stroke::DEFAULT,
        ),
        fill,
        radius: (rad - stroke::DEFAULT).max(0.0),
    });
}

impl View for ReferenceScreen {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;

        // Shell wallpaper (flat mid-stop until the gradient op lands) + the
        // 64px hairline grid.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, W, H),
            fill: shell::wallpaper_flat(t.mode),
            radius: 0.0,
        });
        let grid = shell::grid_line(t.mode);
        let mut x = shell::GRID_STEP;
        while x < W {
            ctx.add(Node::Rect { rect: Rect::new(x, 0.0, 1.0, H), fill: grid, radius: 0.0 });
            x += shell::GRID_STEP;
        }
        let mut y = shell::GRID_STEP;
        while y < H {
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, W, 1.0), fill: grid, radius: 0.0 });
            y += shell::GRID_STEP;
        }

        // The elevated panel — radius-md per §12, no shadow.
        let px = (W - PANEL_W) * 0.5;
        let py = (H - PANEL_H) * 0.5;
        bordered(
            ctx,
            Rect::new(px, py, PANEL_W, PANEL_H),
            t.border_default(),
            t.bg_elevated(),
            radius::MD,
        );

        let pad = space::LG;
        let ix = px + pad;
        let iw = PANEL_W - 2.0 * pad;
        let mut cy = py + pad;

        // Heading — 2xl semibold, text-primary. Type does the work.
        ctx.add(Node::Text {
            rect: Rect::new(ix, cy, 0.0, 0.0),
            content: "The language, standing alone".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XXL,
                weight: Weight::Semibold,
                color: t.text_primary(),
            },
        });
        cy += type_size::XXL * 1.25 + space::SM;

        // Body — sm regular, primary then secondary.
        ctx.add(Node::Text {
            rect: Rect::new(ix, cy, 0.0, 0.0),
            content: "Calm, confident, slightly sharp. Neutrals carry the surface;".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_primary(),
            },
        });
        cy += type_size::SM * 1.45;
        ctx.add(Node::Text {
            rect: Rect::new(ix, cy, 0.0, 0.0),
            content: "the single amber accent carries focus and nothing else.".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_secondary(),
            },
        });
        cy += type_size::SM * 1.45 + space::LG;

        // Text input with placeholder, carrying the focus ring:
        // 2px accent-bg halo, 1px focus-ring stroke, canvas fill.
        let input_h = size::INPUT_HEIGHT_DEFAULT;
        let halo = 2.0;
        ctx.add(Node::Rect {
            rect: Rect::new(ix - halo - stroke::DEFAULT, cy - halo - stroke::DEFAULT,
                            iw + 2.0 * (halo + stroke::DEFAULT), input_h + 2.0 * (halo + stroke::DEFAULT)),
            fill: t.accent_bg(),
            radius: radius::XS + halo,
        });
        bordered(
            ctx,
            Rect::new(ix - stroke::DEFAULT, cy - stroke::DEFAULT,
                      iw + 2.0 * stroke::DEFAULT, input_h + 2.0 * stroke::DEFAULT),
            t.focus_ring(),
            t.bg_canvas(),
            radius::XS + stroke::DEFAULT,
        );
        ctx.add(Node::Text {
            rect: Rect::new(ix + space::SM, cy + (input_h - type_size::MD) * 0.5, 0.0, 0.0),
            content: "focused input — placeholder".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::MD,
                weight: Weight::Regular,
                color: t.text_tertiary(),
            },
        });
        cy += input_h + space::LG;

        // The three button variants: primary / secondary / ghost.
        let bh = size::BUTTON_HEIGHT_DEFAULT;
        let bw = (iw - 2.0 * space::SM) / 3.0;
        // Primary — accent fill, text-on-accent label.
        ctx.add(Node::Rect {
            rect: Rect::new(ix, cy, bw, bh),
            fill: t.accent_fg(),
            radius: radius::SM,
        });
        ctx.add(Node::Text {
            rect: Rect::new(ix + bw * 0.5 - 24.0, cy + (bh - type_size::SM) * 0.5, 0.0, 0.0),
            content: "Primary".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Medium,
                color: t.text_on_accent(),
            },
        });
        // Secondary — border-strong outline, text-primary label.
        let sx = ix + bw + space::SM;
        bordered(ctx, Rect::new(sx, cy, bw, bh), t.border_strong(), t.bg_elevated(), radius::SM);
        ctx.add(Node::Text {
            rect: Rect::new(sx + bw * 0.5 - 32.0, cy + (bh - type_size::SM) * 0.5, 0.0, 0.0),
            content: "Secondary".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Medium,
                color: t.text_primary(),
            },
        });
        // Ghost — text only.
        let gx = sx + bw + space::SM;
        ctx.add(Node::Text {
            rect: Rect::new(gx + bw * 0.5 - 20.0, cy + (bh - type_size::SM) * 0.5, 0.0, 0.0),
            content: "Ghost".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Medium,
                color: t.text_secondary(),
            },
        });
        cy += bh + space::LG;

        // Machine-text line — Mono, dense-shell tier, tertiary. The voice of
        // everything machine-true in the shell.
        ctx.add(Node::Text {
            rect: Rect::new(ix, cy, 0.0, 0.0),
            content: "seat0 · lucius · forum-wm · #a3f8".into(),
            style: TextStyle {
                family: font::MONO.into(),
                size: type_size::XS,
                weight: Weight::Regular,
                color: t.text_tertiary(),
            },
        });

        ctx.pop();
    }
}

fn main() -> std::io::Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("dark") => Semantic::DARK,
        _ => Semantic::LIGHT,
    };
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    eprintln!("forum-demo: connecting to {sock} ({:?})", mode);
    let mut conn = Connection::connect(&sock)?;
    let win = conn.window_create(W as u32, H as u32, "Atrium", WindowHints::default())?;

    let mut surface = FrescoSurface::new(conn, win);
    let mut app = App::new(ReferenceScreen).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-demo: committing {} node delta(s)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;
    eprintln!("forum-demo: presented.");

    std::thread::sleep(std::time::Duration::from_secs(30));
    Ok(())
}
