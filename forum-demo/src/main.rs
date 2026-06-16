//! forum-demo — render a genuinely Atrium-styled surface, to validate the locked
//! visual language (docs/design/atrium-visual-language.md) as actual pixels.
//!
//! This is the doc's hero example — a "Sign in to Atrium" card — built entirely from
//! Pergola theme TOKENS (cool-neutral ramp, the single amber-bronze accent, moderate
//! radii, the IBM Plex type scale), not hardcoded colors. It's what a Forum chrome
//! app is: a Pergola app; Pergola translates it to the fresco scene graph.
//!
//! Run against frescod-vulkan-smoke (renders each frame to a PNG via lavapipe):
//!   FRESCO_SOCKET=/tmp/frescod-smoke.sock forum-demo [light|dark]

use fresco_client::Connection;
use fresco_protocol::WindowHints;
use pergola::geom::Rect;
use pergola::theme::{font, radius, space, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::{commit, App, FrescoSurface, Node, Surface, TextStyle};

const W: f32 = 440.0;
const H: f32 = 380.0;

/// The login card, all from theme tokens — so it reads as Atrium, and flips cleanly
/// between light and dark via the same token names.
struct LoginCard;

impl View for LoginCard {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        // Page background (the window fill) — recessed canvas neutral.
        ctx.push(Node::Rect {
            rect: Rect::new(0.0, 0.0, W, H),
            fill: t.bg_canvas(),
            radius: 0.0,
        });

        // The card — an elevated surface (lighter on light, darker on dark; no shadow
        // by the doc's policy), radius-lg.
        let (cx, cy, cw, ch) = (space::XL, space::XL, W - 2.0 * space::XL, H - 2.0 * space::XL);
        ctx.add(Node::Rect {
            rect: Rect::new(cx, cy, cw, ch),
            fill: t.bg_elevated(),
            radius: radius::LG,
        });

        let pad = space::LG;
        let ix = cx + pad; // inner x
        let iw = cw - 2.0 * pad; // inner width

        // Heading (3xl-ish, semibold) — type does the work.
        ctx.add(Node::Text {
            rect: Rect::new(ix, cy + pad, 0.0, 0.0),
            content: "Sign in to Atrium".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::XXL,
                weight: Weight::Semibold,
                color: t.text_primary(),
            },
        });
        // Subhead (sm, secondary).
        ctx.add(Node::Text {
            rect: Rect::new(ix, cy + pad + 40.0, 0.0, 0.0),
            content: "Use your local account password.".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::SM,
                weight: Weight::Regular,
                color: t.text_secondary(),
            },
        });

        // Two input fields — recessed surface tone, radius-sm, with placeholder text.
        let field_y0 = cy + pad + 78.0;
        for (i, ph) in ["username", "password"].iter().enumerate() {
            let fy = field_y0 + i as f32 * (36.0 + space::SM);
            ctx.add(Node::Rect {
                rect: Rect::new(ix, fy, iw, 36.0),
                fill: t.bg_surface(),
                radius: radius::SM,
            });
            ctx.add(Node::Text {
                rect: Rect::new(ix + space::SM, fy + 9.0, 0.0, 0.0),
                content: (*ph).into(),
                style: TextStyle {
                    family: font::SANS.into(),
                    size: type_size::MD,
                    weight: Weight::Regular,
                    color: t.text_tertiary(),
                },
            });
        }

        // Primary action — the single amber-bronze accent, radius-sm, white label.
        let by = field_y0 + 2.0 * (36.0 + space::SM) + space::XS;
        ctx.add(Node::Rect {
            rect: Rect::new(ix, by, iw, 40.0),
            fill: t.accent_fg(),
            radius: radius::SM,
        });
        ctx.add(Node::Text {
            rect: Rect::new(ix + iw * 0.5 - 28.0, by + 11.0, 0.0, 0.0),
            content: "Sign in".into(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::MD,
                weight: Weight::Medium,
                color: pergola::color::Color::rgba(1.0, 1.0, 1.0, 1.0),
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
    let mut app = App::new(LoginCard).with_theme(mode);
    let deltas = app.tick();
    eprintln!("forum-demo: committing {} node delta(s)", deltas.len());
    commit(&mut surface, &deltas)?;
    surface.present()?;
    eprintln!("forum-demo: presented.");

    std::thread::sleep(std::time::Duration::from_secs(30));
    Ok(())
}
