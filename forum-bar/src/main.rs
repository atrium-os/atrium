//! forum-bar — Forum's top bar, to the shell design handoff (§1):
//! brand block, session line, surface chips with engine-state dots,
//! the centered workspace chip, the right icon row, and the clock.
//!
//! An ordinary graphics-only app (no window-management cap): surface
//! status comes over forum-ctl (`ListSurfaces`; a chip click sends
//! `Focus`), drawing goes through Pergola → frescod. Falls back to the
//! handoff's sample session when forum-ctl isn't reachable, so the bar
//! renders standalone (docs/spec/forum.md §3).

use std::time::Duration;

use forum_ctl::{Intent, Reply};
use fresco_protocol::{WindowHints, WmRole};
use pergola::geom::{Axis, Rect};
use pergola::reactive::Mutable;
use pergola::theme::{shell, type_size, Mode, Semantic, Weight};
use pergola::widgets::phosphor;
use pergola::{
    App, Chip, Color, Ctx, Divider, Dot, Draw, FlexStyle, Glyph, Label, Node, SizeSpec, StepResult,
    View, Window, WindowDesc,
};

/// One surface chip. Identity tint is app metadata (per the
/// visual-language rev. 1 note) — a small local map until the app
/// catalog carries it.
#[derive(Clone, PartialEq)]
struct ChipData {
    surface_id: u32,
    glyph: char,
    tint: Color,
    /// Engine-state dot: accent = deadline lane, info = best-effort,
    /// tertiary = gated/stashed.
    state: StateKind,
    focused: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum StateKind {
    Deadline,
    BestEffort,
}

fn app_identity(owner: &str) -> (char, &'static str) {
    if owner.contains("edit") {
        (phosphor::CODE, "#47617A")
    } else if owner.contains("stoa") || owner.contains("term") {
        (phosphor::TERMINAL_WINDOW, "#3E6B54")
    } else if owner.contains("file") {
        (phosphor::FILES, "#8A6A33")
    } else if owner.contains("nav") {
        (phosphor::COMPASS, "#64517A")
    } else {
        (phosphor::SQUARES_FOUR, "#5A636C")
    }
}

struct BarView {
    w: f32,
    chips: Mutable<Vec<ChipData>>,
    workspace: Mutable<(String, String)>,
    time: Mutable<String>,
    date: Mutable<String>,
    /// Set by the theme-toggle click; the main loop applies it to the App.
    want_dark: Mutable<bool>,
}

/// A right-row icon: Phosphor glyph, hover → accent. A no-op click
/// handler keeps un-wired icons hover-reactive like the design.
fn icon_glyph(ctx: &mut Ctx, glyph: char, on_click: Option<Box<dyn Fn() + 'static>>) {
    let t = ctx.theme;
    let id = Glyph::new(glyph).size(15.0).color(t.text_secondary()).emit(ctx);
    if ctx.is_hovered(id) {
        if let Some(Node::Text { style, .. }) = ctx.tree.get_mut(id) {
            style.color = t.accent_fg();
        }
    }
    match on_click {
        Some(f) => ctx.on_click(id, move || f()),
        None => ctx.on_click(id, || {}),
    }
}

impl View for BarView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;

        // The bar body: full-width horizontal flex, 16px gaps, 14px
        // side padding, children vertically centered. The 1px bottom
        // hairline is a separate absolute rect below the fill.
        let root = ctx.push(Node::stack_filled(
            Axis::Horizontal,
            Rect::new(0.0, 0.0, self.w, shell::BAR_H - 1.0),
            16.0,
            t.bg_surface(),
            0.0,
        ));
        ctx.set_flex(root, FlexStyle {
            padding_x: Some(14.0),
            align: pergola::Align::Center,
            ..FlexStyle::default()
        });

        // Brand: 10px amber square + `atrium` in Mono 600.
        let brand = ctx.push(Node::hstack(Rect::ZERO_SIZED, 8.0));
        ctx.set_flex(brand, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            fill: t.accent_fg(),
            radius: 2.0,
        });
        Label::new("atrium")
            .mono()
            .size(type_size::SM)
            .weight(Weight::Semibold)
            .emit(ctx);
        ctx.pop();

        // Session line.
        Label::new("seat0 · lucius · forum-wm")
            .mono()
            .size(type_size::XS)
            .color(t.text_tertiary())
            .emit(ctx);

        Divider::vertical(16.0).emit(ctx);

        // Surface strip: one chip per live surface.
        let strip = ctx.push(Node::hstack(Rect::ZERO_SIZED, 4.0));
        ctx.set_flex(strip, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
        for chip in self.chips.get_cloned() {
            let glyph = chip.glyph;
            let tint = chip.tint;
            let dot = match chip.state {
                StateKind::Deadline => t.accent_fg(),
                StateKind::BestEffort => pergola::theme::palette::info_500(),
            };
            let sid = chip.surface_id;
            Chip::new(
                shell::SURFACE_CHIP_H,
                Draw(move |ctx: &mut Ctx| {
                    Glyph::new(glyph).size(14.0).color(tint).emit(ctx);
                    Dot::new(5.0, dot).emit(ctx);
                }),
            )
            .active(chip.focused)
            .on_click(move || {
                let sock = forum_ctl::default_socket_path();
                let _ = forum_ctl::request(&sock, &Intent::Focus { surface_id: sid });
            })
            .render(ctx);
        }
        ctx.pop();

        // Center spacer | workspace chip | spacer.
        let sp = ctx.add(Node::hstack(Rect::ZERO_SIZED, 0.0));
        ctx.set_flex(sp, FlexStyle::grow(1.0));

        let (ws_name, ws_hash) = self.workspace.get_cloned();
        Chip::new(
            shell::WORKSPACE_CHIP_H,
            Draw(move |ctx: &mut Ctx| {
                let t = ctx.theme;
                Glyph::new(phosphor::STACK_SIMPLE).size(13.0).color(t.accent_fg()).emit(ctx);
                Label::new(ws_name.clone()).mono().size(type_size::XS).emit(ctx);
                Label::new(format!("#{ws_hash}"))
                    .mono()
                    .size(type_size::XS)
                    .color(t.text_tertiary())
                    .emit(ctx);
            }),
        )
        .active(true)
        .gap(7.0)
        .pad(10.0)
        .on_click(|| {})
        .render(ctx);

        let sp = ctx.add(Node::hstack(Rect::ZERO_SIZED, 0.0));
        ctx.set_flex(sp, FlexStyle::grow(1.0));

        // Right icon row.
        let row = ctx.push(Node::hstack(Rect::ZERO_SIZED, 14.0));
        ctx.set_flex(row, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
        icon_glyph(ctx, phosphor::BROADCAST, None);
        icon_glyph(ctx, phosphor::SPEAKER_SIMPLE_HIGH, None);
        icon_glyph(ctx, phosphor::BATTERY_CHARGING, None);
        icon_glyph(ctx, phosphor::PULSE, None);
        let theme_glyph = if t.mode == Mode::Dark { phosphor::SUN } else { phosphor::MOON };
        let dark = self.want_dark.clone();
        let now_dark = t.mode == Mode::Dark;
        icon_glyph(ctx, theme_glyph, Some(Box::new(move || dark.set(!now_dark))));
        // Bell with the amber badge dot pinned to its top-right corner.
        let bell = ctx.push(Node::hstack(Rect::ZERO_SIZED, 0.0));
        ctx.set_flex(bell, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
        icon_glyph(ctx, phosphor::BELL, None);
        let badge = Dot::new(6.0, t.accent_fg()).emit(ctx);
        ctx.set_flex(badge, FlexStyle {
            absolute_inset: Some([None, Some(-1.0), Some(-2.0), None]),
            ..FlexStyle::default()
        });
        ctx.pop();
        icon_glyph(ctx, phosphor::LOCK_SIMPLE, None);
        ctx.pop();

        // Clock block, right-aligned text.
        let clock = ctx.push(Node::vstack(Rect::ZERO_SIZED, 1.0));
        ctx.set_flex(clock, FlexStyle { align: pergola::Align::End, ..FlexStyle::default() });
        Label::new(self.time.get_cloned())
            .size(type_size::SM)
            .weight(Weight::Medium)
            .emit(ctx);
        Label::new(self.date.get_cloned())
            .mono()
            .size(type_size::XXS)
            .color(t.text_tertiary())
            .emit(ctx);
        ctx.pop();

        ctx.pop(); // root

        // Bottom hairline.
        ctx.add(Node::Rect {
            rect: Rect::new(0.0, shell::BAR_H - 1.0, self.w, 1.0),
            fill: t.border_default(),
            radius: 0.0,
        });
    }
}

/// Live chips from the WM core; the handoff's sample session standalone.
fn poll_status() -> (Vec<ChipData>, (String, String)) {
    let sock = forum_ctl::default_socket_path();
    if let Ok(Reply::Surfaces { surfaces, focus }) = forum_ctl::request(&sock, &Intent::ListSurfaces)
    {
        let chips = surfaces
            .iter()
            .filter(|s| matches!(s.role, WmRole::Document | WmRole::Panel))
            .map(|s| {
                let (glyph, tint) = app_identity(&s.owner_app);
                ChipData {
                    surface_id: s.surface_id,
                    glyph,
                    tint: Color::from_hex(tint),
                    state: if s.surface_id == focus {
                        StateKind::Deadline
                    } else {
                        StateKind::BestEffort
                    },
                    focused: s.surface_id == focus,
                }
            })
            .collect();
        return (chips, ("atelier".into(), "a3f8".into()));
    }
    // Standalone: the handoff's reference session.
    let sample = [("atrium-edit", true), ("stoa", false), ("atrium-files", false)];
    let chips = sample
        .iter()
        .enumerate()
        .map(|(i, (app, focused))| {
            let (glyph, tint) = app_identity(app);
            ChipData {
                surface_id: i as u32,
                glyph,
                tint: Color::from_hex(tint),
                state: if *focused { StateKind::Deadline } else { StateKind::BestEffort },
                focused: *focused,
            }
        })
        .collect();
    (chips, ("atelier".into(), "a3f8".into()))
}

/// hh:mm + "Fri 7 Aug" from the epoch (civil-from-days, UTC — the VM
/// runs UTC).
fn clock_now() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let time = format!("{:02}:{:02}", rem / 3600, (rem / 60) % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days.rem_euclid(7)) as usize];
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        [(m - 1) as usize];
    let _ = y;
    (time, format!("{weekday} {d} {month}"))
}

fn main() -> std::io::Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("dark") => Semantic::DARK,
        _ => Semantic::LIGHT,
    };

    let hints = WindowHints { role: Some(WmRole::Chrome), ..Default::default() };
    let mut desc = WindowDesc::new("forum-bar", SizeSpec::FullScreen);
    desc.hints = hints;
    let mut window = Window::open(&desc)?;
    // Chrome claims the top strip only.
    let w = window.width;

    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (chips0, ws0) = poll_status();
    let (time0, date0) = clock_now();
    let chips = Mutable::with_dirty(chips0, dirty.clone());
    let workspace = Mutable::with_dirty(ws0, dirty.clone());
    let time = Mutable::with_dirty(time0, dirty.clone());
    let date = Mutable::with_dirty(date0, dirty.clone());
    let want_dark = Mutable::with_dirty(mode == Semantic::DARK, dirty.clone());

    let view = BarView {
        w,
        chips: chips.clone(),
        workspace: workspace.clone(),
        time: time.clone(),
        date: date.clone(),
        want_dark: want_dark.clone(),
    };
    let mut app = App::new_with_flag(view, dirty).with_theme(mode);

    // Session chrome: stays up. Step with a 1s timeout — events repaint
    // immediately; the timeout refreshes clock + surface status.
    loop {
        match window.step(&mut app, Some(Duration::from_secs(1)))? {
            StepResult::Closed => return Ok(()),
            StepResult::Event(_) => {}
            StepResult::Timeout => {
                let (t_now, d_now) = clock_now();
                if t_now != time.get_cloned() {
                    time.set(t_now);
                }
                if d_now != date.get_cloned() {
                    date.set(d_now);
                }
                let (c_now, w_now) = poll_status();
                if c_now != chips.get_cloned() {
                    chips.set(c_now);
                }
                if w_now != workspace.get_cloned() {
                    workspace.set(w_now);
                }
            }
        }
        // Theme toggle requested by the moon/sun click.
        let want = if want_dark.get() { Semantic::DARK } else { Semantic::LIGHT };
        if want != app.theme {
            app.set_theme(want);
            window.paint(&mut app)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::view::render;

    fn sample_view() -> BarView {
        let (chips, ws) = poll_status();
        let (time, date) = clock_now();
        BarView {
            w: 1280.0,
            chips: Mutable::new(chips),
            workspace: Mutable::new(ws),
            time: Mutable::new(time),
            date: Mutable::new(date),
            want_dark: Mutable::new(false),
        }
    }

    /// The idle bar must go quiet: after one tick, a second tick with
    /// no input and no state change must produce ZERO deltas (the
    /// damage-driven-present doctrine — an idle bar presents nothing
    /// and the GPU stays gated). Catches anything that dirties on
    /// every render pass.
    #[test]
    fn idle_bar_produces_no_deltas() {
        pergola::text::clear_measurer();
        let view = sample_view();
        let mut app = App::new_with_flag(
            view,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        );
        let first = app.tick();
        assert!(!first.is_empty(), "first tick renders");
        let second = app.tick();
        assert!(
            second.is_empty(),
            "idle bar re-rendered: {} deltas, first: {:?}",
            second.len(),
            second.first(),
        );
    }

    #[test]
    fn bar_renders_three_surface_chips_with_glyphs_and_dots() {
        pergola::text::clear_measurer();
        let view = sample_view();
        let (mut tree, _) = render(&view, Semantic::LIGHT);
        let roots: Vec<_> = tree.roots().collect();
        for r in roots {
            pergola::layout::layout(&mut tree, r);
        }
        // Count icon-font text nodes at 14px (chip glyphs).
        let chip_glyphs = tree
            .iter()
            .filter(|(_, n)| matches!(n, Node::Text { style, .. }
                if style.family == "system-icons" && style.size == 14.0))
            .count();
        assert_eq!(chip_glyphs, 3, "three surface chips expected");
        // Every chip glyph must have landed inside the bar strip
        // (x > 0 — an unplaced node still sits at the origin).
        for (id, n) in tree.iter() {
            if let Node::Text { style, rect, .. } = n {
                if style.family == "system-icons" && style.size == 14.0 {
                    assert!(rect.origin.x > 100.0,
                        "chip glyph {id:?} unplaced at {:?}", rect.origin);
                    assert!(rect.origin.y > 0.0 && rect.origin.y < 38.0,
                        "chip glyph {id:?} outside bar at {:?}", rect.origin);
                }
            }
        }
    }
}
