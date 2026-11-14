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

use std::sync::atomic::{AtomicBool, Ordering};
use std::rc::Rc;
use std::sync::Arc;

mod status_panel;

use forum_ctl::{Intent, PadInfo, Reply, StatusContent, StatusEntry, StatusItem};
use lyra_protocol::wire::{action, FromControl, FromMedia, MediaAction, NowPlaying, SessionState, ToControl, ToMedia, VolumeMode};
use fresco_protocol::{WindowHints, WmRole};
use pergola::geom::{Axis, Rect};
use pergola::reactive::Mutable;
use pergola::theme::{shell, type_size, Mode, Semantic, Weight};
use pergola::widgets::phosphor;
use pergola::widgets::popup::{MenuButton, MenuItem, MenuState, PanelButton, PANEL_PADDING};
use pergola::widgets::{RadioGroup, Slider, Toggle};
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
    /// The bar's width in points: the window's, which a live scale change
    /// (or a new mode) changes, so the main loop keeps it current.
    w: Rc<std::cell::Cell<f32>>,
    chips: Mutable<Vec<ChipData>>,
    workspace: Mutable<(String, String)>,
    time: Mutable<String>,
    date: Mutable<String>,
    /// Set by the light/dark switch; the main loop asks the WM to change
    /// the system appearance, and the bar follows it like every app.
    want_dark: Mutable<Option<bool>>,
    /// What is playing (choragusd's active session), if anything.
    now: Mutable<Option<NowShown>>,
    /// Set by the now-playing button; the main loop sends it to choragusd.
    want_media: Mutable<Option<MediaAction>>,
    /// The gamepads and who holds each (forum-ctl `ListPads`), for the
    /// controller menu.
    pads: Mutable<Vec<PadInfo>>,
    /// The controller menu (open, highlight).
    pads_open: Mutable<MenuState>,
    /// Set by a "Take back" click: the main loop asks for the pads again.
    pads_stale: Arc<AtomicBool>,
    /// The system volume, and what the volume drop-down asks for.
    audio: Mutable<AudioShown>,
    want_audio: Mutable<Option<AudioAction>>,
    /// The volume drop-down: open, and its controls' values (kept in step
    /// with `audio` by the main loop, so a drag is not undone mid-way).
    audio_open: Mutable<MenuState>,
    vol_level: Mutable<f64>,
    vol_muted: Mutable<bool>,
    vol_output: Mutable<usize>,
    /// The current output's "Fixed volume" switch.
    vol_fixed: Mutable<bool>,
    /// Apps' status items (forum-status-items.md): the bar draws each
    /// one's icon and menu; picks go back through forum-wm.
    status: Mutable<Vec<StatusShown>>,
    /// Set by a pick or a slide: the main loop asks for the items again.
    status_stale: Arc<AtomicBool>,
    /// A press on the icon of an app that draws its own panel: the main
    /// loop asks frescod for a grant and passes its token to the app.
    want_drawn: Mutable<Option<DrawnAsk>>,
    /// The USB sticks served for this session (dispensatord), their menu,
    /// the last eject's refusal, and the part an Eject pick asks for.
    sticks: Mutable<Vec<atrium_media::Volume>>,
    sticks_open: Mutable<MenuState>,
    stick_note: Mutable<Option<String>>,
    want_eject: Mutable<Option<String>>,
    /// The Lock button was pressed: the main loop asks ostiarius.
    want_lock: Mutable<bool>,
}

/// An app's own panel to open (forum-status-items.md §4): under the icon
/// at `rect` (layout px), `width` x `height` (layout px).
#[derive(Clone, Debug, PartialEq)]
struct DrawnAsk {
    app_id: String,
    rect: Rect,
    width: u16,
    height: u16,
}

/// One app's status item as the bar shows it, with its menu's state
/// (kept across polls, by app id).
#[derive(Clone)]
struct StatusShown {
    item: StatusItem,
    open: Mutable<MenuState>,
    /// A panel's controls' values (status_panel.rs).
    controls: status_panel::Controls,
    /// The app's vector icon, parsed, if it sent one fit to draw.
    vector: Option<Arc<pergola::icon::IconGeometry>>,
}

/// An app's vector icon, if it parses to something small and inside its
/// box (`IconGeometry::sane`); else the bar draws the app's glyph.
fn status_vector(svg: Option<&str>) -> Option<Arc<pergola::icon::IconGeometry>> {
    let g = pergola::icon::parse_icon(svg?);
    (!g.is_empty() && g.sane()).then(|| Arc::new(g))
}

/// An app's colour as its lit icon's fill: darkened as far as the icon
/// (`text_on_accent`) needs for 3:1 contrast (visual language, controls).
fn status_tint(t: &Semantic, tint: Option<[u8; 3]>) -> Option<pergola::Color> {
    let [r, g, b] = tint?;
    Some(pergola::Color::from_srgb8(r, g, b).fill_under(t.text_on_accent(), 3.0))
}

/// The heading over a status panel: the app's name, drawn by the bar.
const STATUS_PANEL_HEAD: f32 = 24.0;

/// An app's short name for the menu's heading: the registry id's last
/// part (the id comes from the launch registry, never from the app).
fn short_app_name(app_id: &str) -> &str {
    app_id.rsplit('.').next().unwrap_or(app_id)
}

/// A status item's menu as Pergola items: a heading (the app's name,
/// not pickable), then the app's entries, a separator marking the next
/// one. Also each Pergola item's index on the wire (None: the heading).
fn status_menu(item: &StatusItem) -> (Vec<MenuItem>, Vec<Option<u32>>) {
    let mut items = vec![MenuItem::new(short_app_name(&item.app_id)).enabled(false)];
    let mut wire = vec![None];
    let mut separate = true;
    let StatusContent::Menu(menu) = &item.content else { return (items, wire) };
    for (i, e) in menu.iter().enumerate() {
        let m = match e {
            StatusEntry::Separator => {
                separate = true;
                continue;
            }
            StatusEntry::Action { label, enabled } => MenuItem::new(label.clone()).enabled(*enabled),
            StatusEntry::Check { label, on, enabled } => MenuItem::new(label.clone()).checked(*on).enabled(*enabled),
            StatusEntry::Radio { label, on, enabled } => MenuItem::new(label.clone()).radio(*on).enabled(*enabled),
            StatusEntry::Slider { label, value } => MenuItem::slider(label.clone(), *value as f64),
        };
        items.push(if separate { m.separated() } else { m });
        wire.push(Some(i as u32));
        separate = false;
    }
    (items, wire)
}

/// The items from the WM, keeping each app's menu state. None when
/// nothing changed (same apps, same generations).
fn merge_status(old: &[StatusShown], new: Vec<StatusItem>, dirty: Option<&Arc<AtomicBool>>) -> Option<Vec<StatusShown>> {
    let same = old.len() == new.len()
        && old.iter().zip(&new).all(|(o, n)| o.item.app_id == n.app_id && o.item.generation == n.generation);
    if same {
        return None;
    }
    Some(new.into_iter().map(|item| {
        let (open, controls) = match old.iter().find(|o| o.item.app_id == item.app_id) {
            Some(o) => (o.open.clone(), o.controls.clone()),
            None => (
                match dirty {
                    Some(d) => Mutable::with_dirty(MenuState::default(), d.clone()),
                    None => Mutable::new(MenuState::default()),
                },
                status_panel::Controls::new(dirty.cloned()),
            ),
        };
        if let StatusContent::Panel(p) = &item.content {
            controls.sync(p);
        }
        let vector = status_vector(item.svg.as_deref());
        StatusShown { item, open, controls, vector }
    }).collect())
}

/// Apps' status items from the WM; none when it isn't reachable.
fn poll_status_items() -> Vec<StatusItem> {
    match forum_ctl::request(forum_ctl::default_socket_path(), &Intent::ListStatusItems) {
        Ok(Reply::StatusItems { items }) => items,
        _ => Vec::new(),
    }
}

/// The controller menu's line for one pad: its name, and the app holding
/// it or that it follows focus.
fn pad_line(p: &PadInfo) -> String {
    let mut name = p.pad.name.clone();
    if name.is_empty() {
        name = format!("Controller {}", p.pad.device_id);
    }
    if name.chars().count() > 24 {
        name = name.chars().take(23).collect::<String>() + "…";
    }
    if p.pad.held_by == 0 {
        format!("{name} · follows focus")
    } else {
        // The app's short name: the registry id's last part.
        let app = p.held_by_app.rsplit('.').next().unwrap_or(&p.held_by_app);
        format!("{name} · held by {app}")
    }
}

/// The bar's view of choragusd's `NowPlaying`.
#[derive(Clone, PartialEq, Debug)]
struct NowShown {
    /// "title · app", cut to fit.
    line: String,
    playing: bool,
    can_toggle: bool,
}

/// Longest now-playing line, in characters.
const NOW_MAX: usize = 36;

fn now_shown(np: &NowPlaying) -> Option<NowShown> {
    if np.state == SessionState::None {
        return None;
    }
    // The app's short name: the registry id's last part.
    let app = np.app_id.rsplit('.').next().unwrap_or(&np.app_id);
    let mut line = if np.title.is_empty() { app.to_string() } else { format!("{} · {app}", np.title) };
    if line.chars().count() > NOW_MAX {
        line = line.chars().take(NOW_MAX - 1).collect::<String>() + "…";
    }
    Some(NowShown {
        line,
        playing: np.state == SessionState::Playing,
        can_toggle: np.actions & (action::PLAY | action::PAUSE) != 0,
    })
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
            Rect::new(0.0, 0.0, self.w.get(), shell::bar_h() - 1.0),
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

        // Now playing: a note, what it is, and play/pause.
        if let Some(n) = self.now.get_cloned() {
            let np = ctx.push(Node::hstack(Rect::ZERO_SIZED, 6.0));
            ctx.set_flex(np, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
            Glyph::new(phosphor::MUSIC_NOTE_SIMPLE).size(13.0).color(t.accent_fg()).emit(ctx);
            Label::new(n.line.clone()).size(type_size::XS).color(t.text_secondary()).emit(ctx);
            if n.can_toggle {
                let want = self.want_media.clone();
                let g = if n.playing { phosphor::PAUSE } else { phosphor::PLAY };
                icon_glyph(ctx, g, Some(Box::new(move || want.set(Some(MediaAction::PlayPause)))));
            }
            ctx.pop();
        }

        let pads = self.pads.get_cloned();

        // Apps' status items (forum-status-items.md §1): their own group,
        // left of the system icons and set apart from them by a rule, so
        // a glyph that looks like a system icon still sits among apps.
        // The bar draws the icon and the menu, headed by the app's name.
        let status = self.status.get_cloned();
        if !status.is_empty() {
            let group = ctx.push(Node::hstack(Rect::ZERO_SIZED, 10.0));
            ctx.set_flex(group, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
            for s in &status {
                let label = if s.item.tooltip.is_empty() { short_app_name(&s.item.app_id).to_string() } else { s.item.tooltip.clone() };
                // A panel the app draws itself (§4): the press asks for a
                // grant; the icon is lit while frescod says it is open.
                if let StatusContent::Drawn { width, height } = s.item.content {
                    let (want, app_id) = (self.want_drawn.clone(), s.item.app_id.clone());
                    let button = PanelButton::hosted(label, s.open.clone(), move |rect| {
                        want.set(Some(DrawnAsk { app_id: app_id.clone(), rect, width, height }));
                    });
                    let button = match &s.vector {
                        Some(v) => button.vector_icon(v.clone(), s.item.glyph, 15.0, Some(t.text_secondary())),
                        None => button.icon(s.item.glyph, 15.0, Some(t.text_secondary())),
                    };
                    let button = match status_tint(&t, s.item.tint) {
                        Some(c) => button.tint(c),
                        None => button,
                    };
                    ctx.place(button);
                    continue;
                }
                // A panel the app described: laid out and drawn by the bar,
                // under the app's name.
                if let StatusContent::Panel(p) = &s.item.content {
                    let w = p.width as f32;
                    let (_, h) = status_panel::laid_out(p, 0.0, 0.0, w - 2.0 * PANEL_PADDING);
                    let size = pergola::geom::Size::new(w, 2.0 * PANEL_PADDING + STATUS_PANEL_HEAD + h);
                    let (panel, controls) = (p.clone(), s.controls.clone());
                    let sink = status_panel::Sink { app_id: s.item.app_id.clone(), stale: self.status_stale.clone() };
                    let name = short_app_name(&s.item.app_id).to_string();
                    let button = PanelButton::new(label, s.open.clone(), size, move |ctx: &mut Ctx, inner: Rect| {
                        let t = ctx.theme;
                        status_panel::text_at(ctx, &name, inner.x(), inner.y(), inner.w(), type_size::XS, Weight::Semibold, false, t.text_tertiary());
                        let (pieces, _) = status_panel::laid_out(&panel, inner.x(), inner.y() + STATUS_PANEL_HEAD, inner.w());
                        status_panel::draw(ctx, &pieces, &controls, &sink);
                    })
                    .attached();
                    let button = match &s.vector {
                        Some(v) => button.vector_icon(v.clone(), s.item.glyph, 15.0, Some(t.text_secondary())),
                        None => button.icon(s.item.glyph, 15.0, Some(t.text_secondary())),
                    };
                    let button = match status_tint(&t, s.item.tint) {
                        Some(c) => button.tint(c),
                        None => button,
                    };
                    ctx.place(button);
                    continue;
                }
                let (items, wire) = status_menu(&s.item);
                let wire = Rc::new(wire);
                let (w1, w2) = (wire.clone(), wire);
                let (a1, a2) = (s.item.app_id.clone(), s.item.app_id.clone());
                let (st1, st2) = (self.status_stale.clone(), self.status_stale.clone());
                let button = MenuButton::new(label, s.open.clone(), items, move |i| {
                    let Some(&Some(index)) = w1.get(i) else { return };
                    let r = forum_ctl::request(forum_ctl::default_socket_path(), &Intent::StatusPick { app_id: a1.clone(), index });
                    if !matches!(r, Ok(Reply::Ack)) {
                        eprintln!("forum-bar: status pick {a1} {index}: {r:?}");
                    }
                    st1.store(true, Ordering::Relaxed);
                })
                .on_slide(move |i, value| {
                    let Some(&Some(index)) = w2.get(i) else { return };
                    let r = forum_ctl::request(forum_ctl::default_socket_path(), &Intent::StatusSlide { app_id: a2.clone(), index, value: value as f32 });
                    if !matches!(r, Ok(Reply::Ack)) {
                        eprintln!("forum-bar: status slide {a2} {index}: {r:?}");
                    }
                    st2.store(true, Ordering::Relaxed);
                })
                .attached();
                let button = match &s.vector {
                    Some(v) => button.vector_icon(v.clone(), s.item.glyph, 15.0, Some(t.text_secondary())),
                    None => button.icon(s.item.glyph, 15.0, Some(t.text_secondary())),
                };
                let button = match status_tint(&t, s.item.tint) {
                    Some(c) => button.tint(c),
                    None => button,
                };
                ctx.place(button);
            }
            ctx.pop();
            let gap = ctx.push(Node::hstack(Rect::ZERO_SIZED, 0.0));
            ctx.set_flex(gap, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
            Divider::vertical(16.0).emit(ctx);
            ctx.pop();
        }

        // Right icon row.
        let row = ctx.push(Node::hstack(Rect::ZERO_SIZED, 14.0));
        ctx.set_flex(row, FlexStyle { align: pergola::Align::Center, ..FlexStyle::default() });
        // USB sticks (device-assignment.md §6.6): shown while one is
        // served for this session; its menu ejects one. The bar draws it,
        // like the other system icons; dispensatord decides who may eject.
        let sticks = self.sticks.get_cloned();
        if !sticks.is_empty() {
            let (items, parts) = stick_menu(&sticks, self.stick_note.get_cloned().as_deref());
            let want = self.want_eject.clone();
            let button = MenuButton::new("USB sticks", self.sticks_open.clone(), items, move |i| {
                if let Some(Some(p)) = parts.get(i) {
                    want.set(Some(p.clone()));
                }
            })
            .icon(phosphor::USB, 15.0, Some(t.text_secondary()))
            .attached();
            ctx.place(button);
        }
        // The controller menu (fresco-capabilities.md §4): a drop-down from
        // its button, each pad and who holds it; picking a held one takes it
        // back. Attached (atrium-visual-language.md §14.2): the button is
        // lit while the menu is open, which no app can draw in the bar
        // (fresco-popups.md §2). Accent while an app holds a pad.
        if !pads.is_empty() {
            let held = pads.iter().any(|p| p.pad.held_by != 0);
            let items = pads.iter().map(|p| {
                let line = if p.pad.held_by != 0 { format!("{} — Take back", pad_line(p)) } else { pad_line(p) };
                MenuItem::new(line).enabled(p.pad.held_by != 0)
            }).collect();
            let devices: Vec<(u32, bool)> = pads.iter().map(|p| (p.pad.device_id, p.pad.held_by != 0)).collect();
            let stale = self.pads_stale.clone();
            let colour = if held { t.accent_fg() } else { t.text_secondary() };
            let button = MenuButton::new("Controllers", self.pads_open.clone(), items, move |i| {
                let Some(&(device_id, true)) = devices.get(i) else { return };
                let sock = forum_ctl::default_socket_path();
                match forum_ctl::request(&sock, &Intent::TakeBackPad { device_id }) {
                    Ok(Reply::Ack) => {}
                    Ok(r) => eprintln!("forum-bar: take back pad {device_id}: {r:?}"),
                    Err(e) => eprintln!("forum-bar: take back pad {device_id}: {e}"),
                }
                stale.store(true, Ordering::Relaxed);
            })
            .icon(phosphor::GAME_CONTROLLER, 15.0, Some(colour))
            .attached();
            ctx.place(button);
        }
        icon_glyph(ctx, phosphor::BROADCAST, None);
        // The system volume (lyra-v1.md §7): an attached drop-down from the
        // speaker, drawn by the bar (fresco-popups.md §2): mute, the master
        // level, and the output in use. Dimmed while choragusd is away.
        {
            let a = self.audio.get_cloned();
            let glyph = if !a.available || a.muted || a.master <= 0.0 {
                phosphor::SPEAKER_SIMPLE_X
            } else {
                phosphor::SPEAKER_SIMPLE_HIGH
            };
            let names: Vec<String> = a.outputs.iter().map(|(_, n)| n.clone()).collect();
            let rows = names.len() as f32;
            // A Fixed output (line out to an amplifier) keeps the mix at 100%:
            // the slider is off and says where the volume is.
            let fixed_now = a.fixed();
            // Em-based, so it grows with the text size (resolution-independence.md §7.2).
            let em = pergola::theme::size::em;
            let size = pergola::geom::Size::new(em(260.0), 2.0 * PANEL_PADDING + em(28.0 + 36.0 + 36.0) + if fixed_now { em(20.0) } else { 0.0 } + if rows > 0.0 { em(24.0 + rows * 28.0) } else { 0.0 });
            let (level, muted, output, fixed) = (self.vol_level.clone(), self.vol_muted.clone(), self.vol_output.clone(), self.vol_fixed.clone());
            let current = a.current;
            let want = self.want_audio.clone();
            let ids: Vec<u32> = a.outputs.iter().map(|(id, _)| *id).collect();
            let enabled = a.available;
            let panel = PanelButton::new("Volume", self.audio_open.clone(), size, move |ctx: &mut Ctx, inner: Rect| {
                let t = ctx.theme;
                let (w1, w2, w3, w4) = (want.clone(), want.clone(), want.clone(), want.clone());
                Toggle::new(muted.clone(), "Mute")
                    .at(inner.x(), inner.y())
                    .on_change(move |m| w1.set(Some(AudioAction::Mute(m))))
                    .render(ctx);
                Slider::new(level.clone(), 0.0, 1.0)
                    .at(inner.x(), inner.y() + em(36.0))
                    .width(inner.w())
                    .enabled(enabled && !fixed_now)
                    .on_change(move |v| w2.set(Some(AudioAction::Master(v as f32))))
                    .render(ctx);
                let mut y = inner.y() + em(36.0 + 36.0);
                if fixed_now {
                    Label::new("Fixed at 100%: use your amplifier").size(type_size::XS).color(t.text_tertiary()).at(inner.x(), y - em(8.0)).emit(ctx);
                    y += em(20.0);
                }
                Toggle::new(fixed.clone(), "Fixed volume (line out)")
                    .at(inner.x(), y)
                    .on_change(move |on| w4.set(Some(AudioAction::Fixed(current, on))))
                    .render(ctx);
                y += em(36.0);
                if !names.is_empty() {
                    Label::new("Output").size(type_size::XS).color(t.text_tertiary()).at(inner.x(), y).emit(ctx);
                    let ids = ids.clone();
                    RadioGroup::new(output.clone(), names.clone())
                        .at(inner.x(), y + em(24.0))
                        .on_change(move |i| {
                            if let Some(&id) = ids.get(i) {
                                w3.set(Some(AudioAction::Output(id)));
                            }
                        })
                        .render(ctx);
                }
            })
            .icon(glyph, 15.0, Some(if enabled { t.text_secondary() } else { t.text_tertiary() }))
            .attached();
            ctx.place(panel);
        }
        icon_glyph(ctx, phosphor::BATTERY_CHARGING, None);
        icon_glyph(ctx, phosphor::PULSE, None);
        let theme_glyph = if t.mode == Mode::Dark { phosphor::SUN } else { phosphor::MOON };
        let dark = self.want_dark.clone();
        let now_dark = t.mode == Mode::Dark;
        icon_glyph(ctx, theme_glyph, Some(Box::new(move || dark.set(Some(!now_dark)))));
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
        // Lock the screen (lock-screen.md §2): ostiarius locks, frescod
        // hides the session, the lock screen comes up.
        let lock = self.want_lock.clone();
        icon_glyph(ctx, phosphor::LOCK_SIMPLE, Some(Box::new(move || lock.set(true))));
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
            rect: Rect::new(0.0, shell::bar_h() - 1.0, self.w.get(), 1.0),
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

/// The gamepads from the WM core; none when it isn't reachable.
fn poll_pads() -> Vec<PadInfo> {
    match forum_ctl::request(forum_ctl::default_socket_path(), &Intent::ListPads) {
        Ok(Reply::Pads { pads }) => pads,
        _ => Vec::new(),
    }
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

/// choragusd's media-control socket, in the bar's jail (`media-control` grant).
const MEDIA_SOCKET: &str = "/atrium/sockets/media-control/media-control.sock";

/// Follow choragusd's now-playing on a thread of its own (it pushes every
/// change), reconnecting when it restarts; `None` while nothing plays or
/// choragusd is away.
fn follow_now_playing() -> std::sync::mpsc::Receiver<Option<NowPlaying>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || loop {
        if let Ok(mut m) = lyra_protocol::client::Media::connect(MEDIA_SOCKET, false) {
            if m.send(&ToMedia::NowPlaying).is_ok() {
                while let Ok(ev) = m.recv() {
                    if let FromMedia::NowPlaying(np) = ev {
                        if tx.send(np).is_err() {
                            return;
                        }
                    }
                }
            }
            if tx.send(None).is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    });
    rx
}

/// choragusd's volume-control socket, in the bar's jail (`audio-control`).
const AUDIO_CONTROL_SOCKET: &str = "/atrium/sockets/audio-control/audio-control.sock";

/// The system volume as the bar shows it (lyra-v1.md §7).
#[derive(Clone, PartialEq, Debug, Default)]
struct AudioShown {
    /// choragusd answers (the user's session is the active one).
    available: bool,
    master: f32,
    muted: bool,
    /// The outputs (id, name) and the one in use.
    outputs: Vec<(u32, String)>,
    current: u32,
    /// Where the current output's volume is applied (lyra-hires.md §5).
    volume_mode: Option<VolumeMode>,
}

impl AudioShown {
    /// The current output's volume is fixed at 100% (line out to an amplifier).
    fn fixed(&self) -> bool {
        self.volume_mode == Some(VolumeMode::Fixed)
    }
}

impl AudioShown {
    /// Take in an answer or change event from choragusd.
    fn apply(&mut self, m: FromControl) {
        match m {
            FromControl::Apps { master, master_muted, .. } => {
                self.available = true;
                self.master = master.clamp(0.0, 1.0);
                self.muted = master_muted;
            }
            FromControl::Outputs { current, outputs } => {
                self.volume_mode = outputs.iter().find(|o| o.id == current).map(|o| o.volume);
                self.outputs = outputs.into_iter().map(|o| (o.id, o.name)).collect();
                self.current = current;
            }
            FromControl::Welcome | FromControl::Denied { .. } => {}
        }
    }
}

/// What the volume drop-down asks choragusd for.
#[derive(Clone, Copy, PartialEq, Debug)]
enum AudioAction {
    Master(f32),
    Mute(bool),
    Output(u32),
    /// The output's volume fixed at 100% (`true`), or automatic again.
    Fixed(u32, bool),
}

impl AudioAction {
    fn wire(self) -> ToControl {
        match self {
            AudioAction::Master(v) => ToControl::SetMaster { volume: v.clamp(0.0, 1.0) },
            AudioAction::Mute(m) => ToControl::SetMasterMute { muted: m },
            AudioAction::Output(o) => ToControl::SetOutput { output: o },
            AudioAction::Fixed(o, on) => ToControl::SetVolumeMode { output: o, mode: on.then_some(VolumeMode::Fixed) },
        }
    }
}

/// Follow choragusd's volume state on a thread of its own, reconnecting
/// when it restarts (`None`: it went away). The connection's other handle
/// goes into `send` for the bar's own requests, whose answers come back here.
fn follow_audio(send: Arc<std::sync::Mutex<Option<lyra_protocol::client::Control>>>) -> std::sync::mpsc::Receiver<Option<FromControl>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || loop {
        if let Ok(mut c) = lyra_protocol::client::Control::connect(AUDIO_CONTROL_SOCKET) {
            *send.lock().unwrap() = c.try_clone().ok();
            if c.send(&ToControl::List).is_ok() && c.send(&ToControl::ListOutputs).is_ok() {
                while let Ok(m) = c.recv() {
                    if tx.send(Some(m)).is_err() {
                        return;
                    }
                }
            }
            *send.lock().unwrap() = None;
            if tx.send(None).is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    });
    rx
}

/// dispensatord's media socket, in the bar's jail (`removable-media` capability).
const STICKS_SOCKET: &str = "/atrium/sockets/removable-media/removable-media.sock";

/// What the sticks thread tells the main loop.
#[derive(Debug)]
enum StickMsg {
    /// (Re)subscribed: the list starts empty, `Added` events follow.
    Reset,
    Event(atrium_media::Event),
    /// An eject's answer.
    Ejected(Result<String, (u8, String)>),
}

/// Follow dispensatord's sticks on a thread of its own. The stream ends
/// when the session changes (or dispensatord restarts): subscribe again,
/// starting from an empty list; refused (not this session), try later.
fn follow_sticks() -> (std::sync::mpsc::Receiver<StickMsg>, std::sync::mpsc::Sender<StickMsg>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = tx.clone();
    std::thread::spawn(move || loop {
        if let Ok(mut ev) = atrium_media::client::Events::open(std::path::Path::new(STICKS_SOCKET)) {
            if t.send(StickMsg::Reset).is_err() {
                return;
            }
            while let Ok(e) = ev.next() {
                if t.send(StickMsg::Event(e)).is_err() {
                    return;
                }
            }
            if t.send(StickMsg::Reset).is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });
    (rx, tx)
}

/// Apply one event to the list the bar shows.
fn apply_stick(list: &mut Vec<atrium_media::Volume>, e: atrium_media::Event) {
    match e {
        atrium_media::Event::Added(v) => {
            list.retain(|o| o.part != v.part);
            list.push(v);
            list.sort_by(|a, b| a.part.cmp(&b.part));
        }
        atrium_media::Event::Removed { part, .. } => list.retain(|o| o.part != part),
    }
}

/// "80 MB", "1.9 GB" (decimal, as sticks are sold).
fn human_size(b: u64) -> String {
    let (v, unit) = match b {
        b if b >= 1_000_000_000_000 => (b as f64 / 1e12, "TB"),
        b if b >= 1_000_000_000 => (b as f64 / 1e9, "GB"),
        b if b >= 1_000_000 => (b as f64 / 1e6, "MB"),
        b => (b as f64 / 1e3, "KB"),
    };
    if v < 10.0 { format!("{v:.1} {unit}") } else { format!("{v:.0} {unit}") }
}

/// The sticks menu: the last refusal (if any), then per stick its name
/// and space (not pickable) and "Eject". Also the part each item ejects.
fn stick_menu(sticks: &[atrium_media::Volume], note: Option<&str>) -> (Vec<MenuItem>, Vec<Option<String>>) {
    let mut items = Vec::new();
    let mut parts = Vec::new();
    if let Some(n) = note {
        items.push(MenuItem::new(n.to_string()).enabled(false));
        parts.push(None);
    }
    for (i, v) in sticks.iter().enumerate() {
        let ro = if v.read_only { ", read-only" } else { "" };
        let head = MenuItem::new(format!("{} · {}, {} free{ro}", v.part, human_size(v.size_bytes), human_size(v.free_bytes))).enabled(false);
        items.push(if i > 0 || note.is_some() { head.separated() } else { head });
        parts.push(None);
        items.push(MenuItem::new(format!("Eject {}", v.part)));
        parts.push(Some(v.part.clone()));
    }
    (items, parts)
}

/// Ask ostiarius to lock the screen, over the socket `session-lock` mounts
/// (docs/spec/lock-screen.md §2). It answers once the lock screen is up.
fn lock_screen() {
    use std::io::{BufRead, BufReader, Write};
    let path = std::env::var("FORUM_BAR_LOCK_SOCK").unwrap_or_else(|_| "/atrium/sockets/ostiarius-lock/lock.sock".into());
    let r = std::os::unix::net::UnixStream::connect(&path).and_then(|s| {
        (&s).write_all(b"{\"op\":\"lock\"}\n")?;
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line)?;
        Ok(line)
    });
    match r {
        Ok(l) if l.contains("\"ok\"") => {}
        Ok(l) => eprintln!("forum-bar: lock refused: {}", l.trim()),
        Err(e) => eprintln!("forum-bar: lock: {path}: {e}"),
    }
}

fn main() -> std::io::Result<()> {
    // The system appearance decides (the Window follows it); `dark` only
    // seeds the first frame when frescod has none to give.
    let mode = match std::env::args().nth(1).as_deref() {
        Some("dark") => Semantic::DARK,
        _ => Semantic::LIGHT,
    };

    let hints = WindowHints { role: Some(WmRole::Chrome), ..Default::default() };
    let mut desc = WindowDesc::new("forum-bar", SizeSpec::FullScreen);
    desc.hints = hints;
    let mut window = Window::open(&desc)?;
    // Chrome claims the top strip only.
    let w = Rc::new(std::cell::Cell::new(window.width));

    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (chips0, ws0) = poll_status();
    let (time0, date0) = clock_now();
    let chips = Mutable::with_dirty(chips0, dirty.clone());
    let workspace = Mutable::with_dirty(ws0, dirty.clone());
    let time = Mutable::with_dirty(time0, dirty.clone());
    let date = Mutable::with_dirty(date0, dirty.clone());
    let want_dark = Mutable::with_dirty(None, dirty.clone());
    let now = Mutable::with_dirty(None, dirty.clone());
    let want_media = Mutable::with_dirty(None, dirty.clone());
    let pads = Mutable::with_dirty(poll_pads(), dirty.clone());
    let pads_open = Mutable::with_dirty(MenuState::default(), dirty.clone());
    let pads_stale = Arc::new(AtomicBool::new(false));
    let playing = follow_now_playing();
    let audio = Mutable::with_dirty(AudioShown::default(), dirty.clone());
    let want_audio = Mutable::with_dirty(None, dirty.clone());
    let audio_send = Arc::new(std::sync::Mutex::new(None));
    let audio_events = follow_audio(audio_send.clone());
    let audio_open = Mutable::with_dirty(MenuState::default(), dirty.clone());
    let vol_level = Mutable::with_dirty(0.0f64, dirty.clone());
    let vol_muted = Mutable::with_dirty(false, dirty.clone());
    let vol_output = Mutable::with_dirty(0usize, dirty.clone());
    let vol_fixed = Mutable::with_dirty(false, dirty.clone());
    let mut control: Option<lyra_protocol::client::Media> = None;
    let status = Mutable::with_dirty(
        merge_status(&[], poll_status_items(), Some(&dirty)).unwrap_or_default(),
        dirty.clone(),
    );
    let status_stale = Arc::new(AtomicBool::new(false));
    let want_drawn = Mutable::with_dirty(None, dirty.clone());
    let sticks = Mutable::with_dirty(Vec::new(), dirty.clone());
    let sticks_open = Mutable::with_dirty(MenuState::default(), dirty.clone());
    let stick_note = Mutable::with_dirty(None, dirty.clone());
    let want_eject = Mutable::with_dirty(None, dirty.clone());
    let want_lock = Mutable::with_dirty(false, dirty.clone());
    let (stick_events, stick_tx) = follow_sticks();
    // The grants handed to apps for their own panels: token, app.
    let mut drawn: Vec<([u8; 16], String)> = Vec::new();
    let dirty_flag = dirty.clone();

    let view = BarView {
        now: now.clone(),
        want_media: want_media.clone(),
        w: w.clone(),
        chips: chips.clone(),
        workspace: workspace.clone(),
        time: time.clone(),
        date: date.clone(),
        want_dark: want_dark.clone(),
        pads: pads.clone(),
        pads_open: pads_open.clone(),
        pads_stale: pads_stale.clone(),
        audio: audio.clone(),
        want_audio: want_audio.clone(),
        audio_open: audio_open.clone(),
        vol_level: vol_level.clone(),
        vol_muted: vol_muted.clone(),
        vol_output: vol_output.clone(),
        vol_fixed: vol_fixed.clone(),
        status: status.clone(),
        status_stale: status_stale.clone(),
        want_drawn: want_drawn.clone(),
        sticks: sticks.clone(),
        sticks_open: sticks_open.clone(),
        stick_note: stick_note.clone(),
        want_eject: want_eject.clone(),
        want_lock: want_lock.clone(),
    };
    let mut app = App::new_with_flag(view, dirty).with_theme(mode);

    // Session chrome: stays up. Step with a 1s timeout — events repaint
    // immediately; the timeout refreshes clock + surface status.
    let mut bar_sent = 0.0f32;
    loop {
        let stepped = window.step(&mut app, Some(Duration::from_secs(1)))?;
        // A new width in points (the monitor's scale changed live, or its
        // mode): lay the bar out across it now, not one frame late. Without
        // this the bar kept its first width, and at 1.5 on a 1920 px screen
        // its right half (status icons, clock) fell outside the window.
        if window.width != w.get() {
            w.set(window.width);
            app.invalidate();
            window.paint(&mut app)?;
        }
        // The bar's height is em-based: tell forum-wm when the text size
        // changes it, and once at start (resolution-independence.md §7.2).
        let bh = shell::bar_h();
        if bh != bar_sent {
            bar_sent = bh;
            if let Err(e) = forum_ctl::request(forum_ctl::default_socket_path(), &forum_ctl::Intent::ReserveTop { pt: bh as u32 }) {
                eprintln!("forum-bar: could not tell forum-wm the bar's height: {e}");
            }
            app.invalidate();
        }
        match stepped {
            StepResult::Closed => return Ok(()),
            // An app's own panel opened or went (frescod's word): its icon
            // is lit while it is open.
            StepResult::Event(fresco_client::Event::PopupHosted { token, open, .. }) => {
                if let Some(i) = drawn.iter().position(|(t, _)| *t == token) {
                    if let Some(s) = status.get_cloned().iter().find(|s| s.item.app_id == drawn[i].1) {
                        s.open.set(if open { MenuState::opened(None) } else { MenuState::default() });
                    }
                    if !open {
                        drawn.remove(i);
                    }
                }
            }
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
                pads_stale.store(true, Ordering::Relaxed);
                status_stale.store(true, Ordering::Relaxed);
            }
        }
        // Apps' status items: every second, and at once after a pick (the
        // app answers with its next `Set`). A menu whose app went closes.
        if status_stale.swap(false, Ordering::Relaxed) {
            let old = status.get_cloned();
            if let Some(new) = merge_status(&old, poll_status_items(), Some(&dirty_flag)) {
                for o in &old {
                    if o.open.get().open && !new.iter().any(|n| n.item.app_id == o.item.app_id) {
                        o.open.set(MenuState::default());
                    }
                }
                status.set(new);
            }
        }
        // The pads: every second, and at once after a "Take back".
        if pads_stale.swap(false, Ordering::Relaxed) {
            let p_now = poll_pads();
            if p_now.is_empty() && pads_open.get().open {
                pads_open.set(MenuState::default());
            }
            if p_now != pads.get_cloned() {
                pads.set(p_now);
            }
        }
        // What is playing changed (choragusd pushes it).
        while let Ok(np) = playing.try_recv() {
            let shown = np.as_ref().and_then(now_shown);
            if shown != now.get_cloned() {
                now.set(shown);
            }
        }
        // The system volume (choragusd pushes changes).
        while let Ok(m) = audio_events.try_recv() {
            let mut a = audio.get_cloned();
            match m {
                Some(m) => a.apply(m),
                None => a = AudioShown::default(),
            }
            if a != audio.get_cloned() {
                // The controls follow choragusd (a drag's own changes come
                // back as the same values).
                if (vol_level.get() - a.master as f64).abs() > 1e-3 {
                    vol_level.set(a.master as f64);
                }
                if vol_muted.get() != a.muted {
                    vol_muted.set(a.muted);
                }
                if vol_fixed.get() != a.fixed() {
                    vol_fixed.set(a.fixed());
                }
                let i = a.outputs.iter().position(|(id, _)| *id == a.current).unwrap_or(0);
                if vol_output.get() != i {
                    vol_output.set(i);
                }
                if !a.available && audio_open.get().open {
                    audio_open.set(MenuState::default());
                }
                audio.set(a);
            }
        }
        // A press on the icon of an app that draws its own panel: a grant
        // from frescod (it checks the bar was just pressed), its token to
        // the app through forum-wm; the app claims the pop-up with it.
        if let Some(ask) = want_drawn.get_cloned() {
            want_drawn.set(None);
            // Points, as the bar lays out (resolution-independence.md §3).
            let anchor = [ask.rect.x(), 0.0, ask.rect.w(), shell::bar_h()];
            let (w, h) = (u32::from(ask.width), u32::from(ask.height));
            let id = window.id();
            match window.connection().popup_grant(id, anchor, fresco_protocol::popup_side::BELOW, fresco_protocol::popup_align::END, w, h) {
                Ok(token) => {
                    drawn.retain(|(_, a)| *a != ask.app_id);
                    let r = forum_ctl::request(forum_ctl::default_socket_path(), &Intent::StatusOpenPanel { app_id: ask.app_id.clone(), token });
                    if matches!(r, Ok(Reply::Ack)) {
                        drawn.push((token, ask.app_id));
                    } else {
                        eprintln!("forum-bar: open {}'s panel: {r:?}", ask.app_id);
                    }
                }
                Err(e) => eprintln!("forum-bar: grant for {}'s panel: {e}", ask.app_id),
            }
        }
        // USB sticks: dispensatord's events and eject answers.
        while let Ok(m) = stick_events.try_recv() {
            let mut l = sticks.get_cloned();
            match m {
                StickMsg::Reset => l.clear(),
                StickMsg::Event(e) => apply_stick(&mut l, e),
                StickMsg::Ejected(Ok(_)) => stick_note.set(None),
                StickMsg::Ejected(Err((_, msg))) => stick_note.set(Some(msg)),
            }
            if l.is_empty() {
                if sticks_open.get().open {
                    sticks_open.set(MenuState::default());
                }
                if stick_note.get_cloned().is_some() {
                    stick_note.set(None);
                }
            }
            if l != sticks.get_cloned() {
                sticks.set(l);
            }
        }
        // The Lock button: ostiarius's lock-only socket, on a thread.
        if want_lock.get() {
            want_lock.set(false);
            std::thread::spawn(lock_screen);
        }
        // An Eject pick: on a thread, as an unmount can take a while.
        if let Some(part) = want_eject.get_cloned() {
            want_eject.set(None);
            let tx = stick_tx.clone();
            std::thread::spawn(move || {
                let r = atrium_media::client::eject(std::path::Path::new(STICKS_SOCKET), &part);
                let _ = tx.send(StickMsg::Ejected(r));
            });
        }
        // The volume drop-down: a slider drag, mute, an output.
        if let Some(a) = want_audio.get() {
            want_audio.set(None);
            let sent = audio_send.lock().unwrap().as_mut().map(|c| c.send(&a.wire()));
            if !matches!(sent, Some(Ok(()))) {
                eprintln!("forum-bar: choragusd audio-control is not reachable");
            }
        }
        // The play/pause click.
        if let Some(a) = want_media.get() {
            want_media.set(None);
            if control.is_none() {
                control = lyra_protocol::client::Media::connect(MEDIA_SOCKET, false).ok();
            }
            if control.as_mut().map(|c| c.control(a)).map_or(true, |r| r.is_err()) {
                eprintln!("forum-bar: choragusd media-control is not reachable");
                control = None;
            }
        }
        // The moon/sun click: ask the WM to switch the whole system. The
        // change comes back as an appearance event, which repaints the bar.
        if let Some(dark) = want_dark.get() {
            want_dark.set(None);
            match forum_ctl::request(forum_ctl::default_socket_path(), &forum_ctl::Intent::SetAppearance { dark }) {
                Ok(forum_ctl::Reply::Ack) => {}
                Ok(r) => eprintln!("forum-bar: appearance refused: {r:?}"),
                Err(e) => eprintln!("forum-bar: appearance: {e}"),
            }
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
            w: Rc::new(std::cell::Cell::new(1280.0)),
            chips: Mutable::new(chips),
            workspace: Mutable::new(ws),
            time: Mutable::new(time),
            date: Mutable::new(date),
            want_dark: Mutable::new(None),
            now: Mutable::new(None),
            want_media: Mutable::new(None),
            pads: Mutable::new(Vec::new()),
            pads_open: Mutable::new(MenuState::default()),
            pads_stale: Arc::new(AtomicBool::new(false)),
            audio: Mutable::new(AudioShown::default()),
            want_audio: Mutable::new(None),
            audio_open: Mutable::new(MenuState::default()),
            vol_level: Mutable::new(0.0),
            vol_muted: Mutable::new(false),
            vol_output: Mutable::new(0),
            vol_fixed: Mutable::new(false),
            status: Mutable::new(Vec::new()),
            status_stale: Arc::new(AtomicBool::new(false)),
            want_drawn: Mutable::new(None),
            sticks: Mutable::new(Vec::new()),
            sticks_open: Mutable::new(MenuState::default()),
            stick_note: Mutable::new(None),
            want_eject: Mutable::new(None),
            want_lock: Mutable::new(false),
        }
    }

    fn stick(p: &str) -> atrium_media::Volume {
        atrium_media::Volume { part: p.into(), label: String::new(), mountpoint: format!("/media/{p}"), size_bytes: 80_000_000, free_bytes: 1_900_000_000 / 100, read_only: false }
    }

    #[test]
    fn sticks_follow_events_and_eject() {
        let mut l = Vec::new();
        apply_stick(&mut l, atrium_media::Event::Added(stick("da1s1")));
        apply_stick(&mut l, atrium_media::Event::Added(stick("da0p1")));
        assert_eq!(l.iter().map(|v| v.part.as_str()).collect::<Vec<_>>(), ["da0p1", "da1s1"]);
        let (items, parts) = stick_menu(&l, None);
        assert_eq!(items.len(), 4);
        assert_eq!(parts, [None, Some("da0p1".to_string()), None, Some("da1s1".to_string())]);
        let (items, parts) = stick_menu(&l[..1], Some("busy"));
        assert_eq!((items.len(), parts[0].clone()), (3, None));
        assert_eq!(human_size(80_000_000), "80 MB");
        assert_eq!(human_size(1_900_000_000), "1.9 GB");
        apply_stick(&mut l, atrium_media::Event::Removed { part: "da0p1".into(), ejected: true });
        assert_eq!(l, [stick("da1s1")]);
    }

    #[test]
    fn the_stick_icon_shows_only_with_a_stick() {
        pergola::text::clear_measurer();
        let v = sample_view();
        let usb = |tree: &pergola::NodeTree| tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content.contains(phosphor::USB)));
        assert!(!usb(&render(&v, Semantic::LIGHT).0));
        v.sticks.set(vec![stick("da0p1")]);
        assert!(usb(&render(&v, Semantic::LIGHT).0));
        // Opened: the stick and its Eject.
        v.sticks_open.set(MenuState::opened(None));
        let (tree, _) = render(&v, Semantic::LIGHT);
        assert!(tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content == "Eject da0p1")));
    }

    fn sample_item(generation: u64) -> StatusItem {
        StatusItem {
            app_id: "org.test.vpn".into(),
            glyph: phosphor::LOCK_SIMPLE,
            svg: None,
            tint: None,
            tooltip: "VPN".into(),
            content: StatusContent::Menu(vec![
                StatusEntry::Check { label: "Connected".into(), on: true, enabled: true },
                StatusEntry::Separator,
                StatusEntry::Radio { label: "Home".into(), on: true, enabled: true },
                StatusEntry::Radio { label: "Office".into(), on: false, enabled: false },
                StatusEntry::Slider { label: "Level".into(), value: 0.25 },
                StatusEntry::Action { label: "Quit".into(), enabled: true },
            ]),
            generation,
        }
    }

    #[test]
    fn a_status_menu_is_headed_by_the_app_and_maps_back_to_its_entries() {
        let (items, wire) = status_menu(&sample_item(1));
        let labels: Vec<_> = items.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["vpn", "Connected", "Home", "Office", "Level", "Quit"]);
        assert!(!items[0].enabled, "the heading is not pickable");
        assert_eq!(wire, [None, Some(0), Some(2), Some(3), Some(4), Some(5)]);
        assert!(items[1].separated && items[2].separated && !items[3].separated);
        assert_eq!(items[1].mark, pergola::widgets::popup::Mark::Check(true));
        assert_eq!(items[2].mark, pergola::widgets::popup::Mark::Radio(true));
        assert!(!items[3].enabled);
        assert_eq!(items[4].slider, Some(0.25));
    }

    #[test]
    fn a_status_item_draws_its_vector_icon_or_falls_back_to_its_glyph() {
        let ok = status_vector(Some(r#"<svg viewBox="0 0 24 24"><path d="M5 12h14M12 5v14"/></svg>"#));
        assert!(ok.is_some());
        assert!(status_vector(Some(r#"<path d="M5 12L500 12"/>"#)).is_none(), "outside its box");
        assert!(status_vector(Some("not an icon")).is_none());
        assert!(status_vector(None).is_none());
        // Drawn as strokes (Path nodes), not the glyph.
        pergola::text::clear_measurer();
        let view = sample_view();
        let mut item = sample_item(1);
        item.svg = Some(r#"<path d="M5 12h14M12 5v14"/>"#.into());
        view.status.set(merge_status(&[], vec![item], None).unwrap());
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(tree.iter().any(|(_, x)| matches!(x, Node::Path { .. })));
    }

    #[test]
    fn an_open_status_item_is_lit_in_its_apps_colour() {
        pergola::text::clear_measurer();
        let view = sample_view();
        let mut item = sample_item(1);
        item.tint = Some([0xff, 0xe0, 0x40]); // too light under a white icon
        let shown = merge_status(&[], vec![item], None).unwrap();
        shown[0].open.set(MenuState::opened(None));
        view.status.set(shown);
        let t = Semantic::LIGHT;
        let lit = status_tint(&t, Some([0xff, 0xe0, 0x40])).unwrap();
        assert!(t.text_on_accent().contrast(lit) >= 3.0, "darkened to read");
        let (tree, _) = render(&view, t);
        assert!(tree.iter().any(|(_, x)| matches!(x, Node::Rect { fill, .. } if *fill == lit)), "the icon's fill");
        assert!(status_tint(&t, None).is_none());
    }

    #[test]
    fn status_items_keep_their_menu_state_and_redraw_only_on_change() {
        let first = merge_status(&[], vec![sample_item(1)], None).unwrap();
        first[0].open.set(MenuState::opened(None));
        assert!(merge_status(&first, vec![sample_item(1)], None).is_none());
        let second = merge_status(&first, vec![sample_item(2)], None).unwrap();
        assert!(second[0].open.get().open, "an open menu stays open across a change");
        assert!(merge_status(&second, Vec::new(), None).unwrap().is_empty());
    }

    #[test]
    fn status_items_sit_in_the_bar_and_open_their_menu() {
        pergola::text::clear_measurer();
        let view = sample_view();
        let has_text = |tree: &pergola::NodeTree, s: &str| tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content == s));
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(!has_text(&tree, "Connected"));
        view.status.set(merge_status(&[], vec![sample_item(1)], None).unwrap());
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(!has_text(&tree, "Connected"));
        view.status.get_cloned()[0].open.set(MenuState::opened(None));
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, "vpn") && has_text(&tree, "Connected") && has_text(&tree, "Quit"));
    }

    #[test]
    fn a_status_panel_opens_under_the_apps_name_with_its_content() {
        use forum_ctl::{StatusNode, StatusPanel, StatusText};
        pergola::text::clear_measurer();
        let view = sample_view();
        let has_text = |tree: &pergola::NodeTree, s: &str| tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content == s));
        let item = StatusItem {
            app_id: "org.test.clip".into(),
            glyph: phosphor::CLIPBOARD,
            svg: None,
            tint: None,
            tooltip: "Clipboard".into(),
            content: StatusContent::Panel(StatusPanel {
                width: 280,
                nodes: vec![
                    StatusNode::Heading { text: "History".into() },
                    StatusNode::Text { text: "Twelve items kept".into(), style: StatusText::Secondary },
                    StatusNode::Group { title: "Keep".into(), children: vec![StatusNode::Toggle { id: 1, label: "Paused".into(), on: true, enabled: true }] },
                    StatusNode::Row { children: vec![StatusNode::Button { id: 2, label: "Clear".into(), primary: false, enabled: true }] },
                ],
            }),
            generation: 1,
        };
        view.status.set(merge_status(&[], vec![item], None).unwrap());
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, &phosphor::CLIPBOARD.to_string()) && !has_text(&tree, "History"));
        view.status.get_cloned()[0].open.set(MenuState::opened(None));
        let (tree, _) = render(&view, Semantic::LIGHT);
        for s in ["clip", "History", "Twelve items kept", "Keep", "Paused", "Clear"] {
            assert!(has_text(&tree, s), "{s}");
        }
    }

    #[test]
    fn the_speaker_opens_the_volume_drop_down() {
        pergola::text::clear_measurer();
        let view = sample_view();
        let has_text = |tree: &pergola::NodeTree, s: &str| tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content == s));
        view.audio.set(AudioShown {
            available: true, master: 0.5, muted: false, current: 2,
            outputs: vec![(1, "Speakers".into()), (2, "Headphones".into())],
            volume_mode: Some(VolumeMode::Digital),
        });
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, &phosphor::SPEAKER_SIMPLE_HIGH.to_string()) && !has_text(&tree, "Headphones"));
        view.audio_open.set(MenuState::opened(None));
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, "Mute") && has_text(&tree, "Output") && has_text(&tree, "Speakers") && has_text(&tree, "Headphones"));
        assert!(has_text(&tree, "Fixed volume (line out)") && !has_text(&tree, "Fixed at 100%: use your amplifier"));
        // A Fixed output says where its volume is.
        let mut a = view.audio.get_cloned();
        a.volume_mode = Some(VolumeMode::Fixed);
        view.audio.set(a);
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, "Fixed at 100%: use your amplifier"));
    }

    #[test]
    fn the_volume_follows_choragusd() {
        let mut a = AudioShown::default();
        a.apply(FromControl::Apps { master: 1.4, master_muted: true, apps: Vec::new() });
        assert!(a.available && a.master == 1.0 && a.muted);
        a.apply(FromControl::Outputs {
            current: 2,
            outputs: vec![
                lyra_protocol::wire::Output { id: 1, name: "Speakers".into(), volume: VolumeMode::Digital, volume_setting: None, hw_volume: false, balance: 0.0, channels: 2, spatial: false },
                lyra_protocol::wire::Output { id: 2, name: "Headphones".into(), volume: VolumeMode::Fixed, volume_setting: Some(VolumeMode::Fixed), hw_volume: false, balance: 0.0, channels: 2, spatial: true },
            ],
        });
        assert_eq!(a.outputs, vec![(1, "Speakers".into()), (2, "Headphones".into())]);
        assert_eq!(a.current, 2);
        assert!(a.fixed(), "the current output's mode");
        assert_eq!(AudioAction::Fixed(2, true).wire(), ToControl::SetVolumeMode { output: 2, mode: Some(VolumeMode::Fixed) });
        assert_eq!(AudioAction::Fixed(2, false).wire(), ToControl::SetVolumeMode { output: 2, mode: None });
        assert_eq!(AudioAction::Master(-0.5).wire(), ToControl::SetMaster { volume: 0.0 });
        assert_eq!(AudioAction::Output(1).wire(), ToControl::SetOutput { output: 1 });
    }

    #[test]
    fn controller_menu_shows_each_pad_and_a_take_back_for_held_ones() {
        let pad = |id, name: &str, held_by, app: &str| PadInfo {
            pad: forum_ctl::WmPad { device_id: id, name: name.into(), vendor: 1, product: 2, window_id: held_by, held_by },
            held_by_app: app.into(),
        };
        let held = pad(1, "Fake Pad", 5, "org.test.game");
        let free = pad(2, "", 0, "");
        assert_eq!(pad_line(&held), "Fake Pad · held by game");
        assert_eq!(pad_line(&free), "Controller 2 · follows focus");

        pergola::text::clear_measurer();
        let view = sample_view();
        let has_text = |tree: &pergola::NodeTree, s: &str| tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content == s));
        let controller = phosphor::GAME_CONTROLLER.to_string();
        // No pads: no controller button.
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(!has_text(&tree, &controller));
        // Pads: the button; open, a line per pad and one "Take back".
        view.pads.set(vec![held, free]);
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, &controller) && !has_text(&tree, "Fake Pad · held by game — Take back"));
        view.pads_open.set(MenuState::opened(None));
        let (tree, _) = render(&view, Semantic::LIGHT);
        assert!(has_text(&tree, "Fake Pad · held by game — Take back") && has_text(&tree, "Controller 2 · follows focus"));
    }

    #[test]
    fn now_playing_shows_title_and_app_and_a_play_pause_button() {
        let np = NowPlaying {
            app_id: "org.atrium.navigator".into(), title: "A very long song title that goes on and on".into(),
            artist: String::new(), state: SessionState::Playing, actions: action::PLAY | action::PAUSE,
            duration: None, position: None,
        };
        let n = now_shown(&np).unwrap();
        assert_eq!(n.line.chars().count(), NOW_MAX);
        assert!(n.line.ends_with('…') && n.playing && n.can_toggle);
        let short = NowPlaying { title: "Song".into(), ..np.clone() };
        assert_eq!(now_shown(&short).unwrap().line, "Song · navigator");
        assert_eq!(now_shown(&NowPlaying { state: SessionState::None, ..np }), None);

        pergola::text::clear_measurer();
        let view = sample_view();
        view.now.set(Some(n));
        let (tree, _) = render(&view, Semantic::LIGHT);
        let has = |c: char| tree.iter().any(|(_, x)| matches!(x, Node::Text { content, .. } if content.starts_with(c)));
        assert!(has(phosphor::PAUSE) && has(phosphor::MUSIC_NOTE_SIMPLE));
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
