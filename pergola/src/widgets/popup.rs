//! Floating surfaces drawn in the window's overlay layer
//! ([`Ctx::overlay`]): `Menu`, `Dropdown` and `Dialog` (visual language
//! rev. 2 §14.2).
//!
//! A floating surface is `bg-elevated` with a 1px `border-default` edge
//! (the §5 shadow waits for a Fresco op). It takes focus when it opens and
//! gives it back when it closes; Escape closes the topmost one, and a
//! press outside a menu or on a dialog's scrim closes it too.
//!
//! A menu given an anchor ([`Menu::anchored`]; a `Dropdown`'s list always
//! is) opens as a pop-up window when the app runs in a `Window`
//! ([`crate::popup`]): frescod places it, so it reaches past the window's
//! edge, flips above when there is no room below, and takes it down on a
//! press outside (which goes no further) or a change of focus. Without a
//! `Window` (tests) it draws in the overlay layer, clipped by the window.

use std::rc::Rc;
use std::sync::Arc;

use crate::event::{Event, Key, KeyEventKind};
use crate::geom::{Axis, Rect};
use crate::layout::{Align, FlexStyle, Justify};
use crate::node::{Node, NodeId, TextStyle};
use crate::reactive::Mutable;
use crate::theme::{font, radius, size, space, stroke, type_size, Weight};
use crate::view::{Ctx, View};
use crate::widgets::button::Button;
use crate::Color;

/// Inset of a menu's items from its edge.
pub const MENU_INSET: f32 = 4.0;
/// Gap between a dropdown and the menu it opens.
pub const MENU_GAP: f32 = 4.0;
/// The accent edge of a menu hanging from a lit (attached) menu button.
pub const ATTACHED_EDGE: f32 = 2.0;
/// A press-catcher bigger than any window: frescod clips it.
const EVERYWHERE: Rect = Rect::new(-100_000.0, -100_000.0, 200_000.0, 200_000.0);

fn inflate(r: Rect, d: f32) -> Rect {
    Rect::new(r.x() - d, r.y() - d, r.w() + 2.0 * d, r.h() + 2.0 * d)
}

fn text(color: Color, sz: f32, family: &str, weight: Weight) -> TextStyle {
    TextStyle { family: family.into(), size: sz, weight, color }
}

/// A floating surface's body: the edge, then the fill. Returns the fill.
fn surface(ctx: &mut Ctx, r: Rect, rad: f32) -> NodeId {
    let t = ctx.theme;
    let e = ctx.add(Node::Rect { rect: r, fill: t.border_default(), radius: rad });
    ctx.tree.insert(Some(e), Node::Rect { rect: inflate(r, -stroke::DEFAULT), fill: t.bg_elevated(), radius: rad - stroke::DEFAULT })
}

// ── Menu ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub enabled: bool,
    /// A separator above this item.
    pub separated: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), shortcut: None, enabled: true, separated: false }
    }
    pub fn shortcut(mut self, s: impl Into<String>) -> Self {
        self.shortcut = Some(s.into());
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn separated(mut self) -> Self {
        self.separated = true;
        self
    }
}

/// Whether a menu is open and which item the arrows are on. The app keeps
/// it in a `Mutable` so it survives renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuState {
    pub open: bool,
    pub highlight: Option<usize>,
}

impl MenuState {
    /// Open with `highlight` on an item (keyboard opening: the first or the
    /// current one), or none (pointer opening).
    pub fn opened(highlight: Option<usize>) -> Self { Self { open: true, highlight } }
}

pub struct Menu {
    pub state: Mutable<MenuState>,
    pub items: Vec<MenuItem>,
    /// Top left of the menu, window px.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    /// What it hangs from, for a pop-up window: the rect (window px), the
    /// side it opens on and how it lines up.
    pub anchor: Option<(Rect, crate::popup::Side, crate::popup::Align)>,
    /// Hangs from a lit menu button: an accent edge (see [`trigger`]).
    pub attached: bool,
    on_pick: Rc<dyn Fn(usize)>,
}

impl Menu {
    pub fn new(state: Mutable<MenuState>, items: Vec<MenuItem>, on_pick: impl Fn(usize) + 'static) -> Self {
        Self { state, items, x: 0.0, y: 0.0, width: 220.0, anchor: None, attached: false, on_pick: Rc::new(on_pick) }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.x = x;
        self.y = y;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }
    /// Open as a pop-up window hanging from `rect` when the app's window
    /// allows (else it draws at `at` inside the window, as before).
    pub fn anchored(mut self, rect: Rect, side: crate::popup::Side, align: crate::popup::Align) -> Self {
        self.anchor = Some((rect, side, align));
        self
    }

    /// The menu's height for its items.
    pub fn height(&self) -> f32 {
        let seps = self.items.iter().filter(|i| i.separated).count() as f32;
        MENU_INSET * 2.0 + self.items.len() as f32 * size::MENU_ITEM + seps * (stroke::DEFAULT + 2.0 * MENU_INSET)
    }

    /// The next enabled item from `from` in direction `step`, wrapping.
    fn step(items: &[MenuItem], from: Option<usize>, step: isize) -> Option<usize> {
        let n = items.len() as isize;
        if n == 0 || !items.iter().any(|i| i.enabled) {
            return None;
        }
        let mut i = from.map_or(if step > 0 { -1 } else { n }, |f| f as isize);
        loop {
            i = (i + step).rem_euclid(n);
            if items[i as usize].enabled {
                return Some(i as usize);
            }
        }
    }
}

impl View for Menu {
    fn render(&self, ctx: &mut Ctx) {
        let st = self.state.get();
        if !st.open {
            return;
        }
        let r = Rect::new(self.x, self.y, self.width, self.height());
        if let Some((anchor, side, align)) = self.anchor.filter(|_| ctx.popups_enabled()) {
            // Its own window: frescod places it and takes it down on a
            // press outside (which goes no further) or a change of focus.
            let spec = crate::popup::Spec { anchor, side, align, grab: true };
            let s = self.state.clone();
            ctx.popup(r, spec, move || s.set(MenuState::default()), |ctx| self.body(ctx, st, r));
            return;
        }
        // A press anywhere outside closes it, and goes no further.
        let catcher = ctx.add(Node::Rect { rect: EVERYWHERE, fill: Color::TRANSPARENT, radius: 0.0 });
        let s = self.state.clone();
        ctx.on_click(catcher, move || s.set(MenuState::default()));
        self.body(ctx, st, r);
    }
}

impl Menu {
    /// The menu's surface and items in `r`.
    fn body(&self, ctx: &mut Ctx, st: MenuState, r: Rect) {
        let t = ctx.theme;
        let body = if self.attached {
            // Joined to the lit button: its accent as a 2px edge.
            let e = ctx.add(Node::Rect { rect: r, fill: t.accent_fill(), radius: radius::MD });
            ctx.tree.insert(Some(e), Node::Rect { rect: inflate(r, -ATTACHED_EDGE), fill: t.bg_elevated(), radius: radius::MD - ATTACHED_EDGE })
        } else {
            surface(ctx, r, radius::MD)
        };
        ctx.autofocus(body);
        let mut y = r.y() + MENU_INSET;
        for (i, item) in self.items.iter().enumerate() {
            if item.separated {
                y += MENU_INSET;
                ctx.tree.insert(Some(body), Node::Rect { rect: Rect::new(r.x() + MENU_INSET, y, r.w() - 2.0 * MENU_INSET, stroke::DEFAULT), fill: t.border_default(), radius: 0.0 });
                y += stroke::DEFAULT + MENU_INSET;
            }
            let row = Rect::new(r.x() + MENU_INSET, y, r.w() - 2.0 * MENU_INSET, size::MENU_ITEM);
            let hit = ctx.tree.insert(Some(body), Node::Rect { rect: row, fill: Color::TRANSPARENT, radius: radius::XS });
            let lit = item.enabled && (st.highlight == Some(i) || ctx.is_hovered(hit));
            if lit {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(hit) {
                    *fill = t.selection_bg();
                }
            }
            let color = if item.enabled { t.text_primary() } else { t.text_disabled() };
            let ls = Node::measure_text(&item.label, &text(color, type_size::MD, font::SANS, Weight::Regular));
            ctx.tree.insert(Some(hit), Node::Text {
                rect: Rect::new(row.x() + space::SM, row.y() + (row.h() - ls.h) / 2.0, ls.w, ls.h),
                content: item.label.clone(),
                style: text(color, type_size::MD, font::SANS, Weight::Regular),
            });
            if let Some(k) = &item.shortcut {
                let kc = if item.enabled { t.text_tertiary() } else { t.text_disabled() };
                let ks = Node::measure_text(k, &text(kc, type_size::SM, font::MONO, Weight::Regular));
                ctx.tree.insert(Some(hit), Node::Text {
                    rect: Rect::new(row.x() + row.w() - space::SM - ks.w, row.y() + (row.h() - ks.h) / 2.0, ks.w, ks.h),
                    content: k.clone(),
                    style: text(kc, type_size::SM, font::MONO, Weight::Regular),
                });
            }
            if item.enabled {
                let (s, pick) = (self.state.clone(), self.on_pick.clone());
                ctx.on_click(hit, move || {
                    s.set(MenuState::default());
                    pick(i);
                });
            }
            y += size::MENU_ITEM;
        }
        // Presses on the body between items close nothing.
        ctx.on_click(body, || {});
        let (s, pick, items) = (self.state.clone(), self.on_pick.clone(), self.items.clone());
        ctx.on_key(body, move |ev| {
            let Event::Key { kind, key, chars, .. } = ev else { return };
            if !kind.pressed() {
                return;
            }
            // Held arrows walk the menu; a held Enter or Space picks once.
            if *kind == KeyEventKind::Repeat && !matches!(key, Key::Down | Key::Up) {
                return;
            }
            let mut st = s.get();
            match key {
                Key::Down => st.highlight = Menu::step(&items, st.highlight, 1),
                Key::Up => st.highlight = Menu::step(&items, st.highlight, -1),
                Key::Escape => st = MenuState::default(),
                Key::Enter => {
                    if let Some(i) = st.highlight.filter(|&i| items[i].enabled) {
                        s.set(MenuState::default());
                        pick(i);
                    }
                    return;
                }
                Key::Char if chars == " " => {
                    if let Some(i) = st.highlight.filter(|&i| items[i].enabled) {
                        s.set(MenuState::default());
                        pick(i);
                    }
                    return;
                }
                _ => return,
            }
            s.set(st);
        });
    }
}

// ── Dropdown ────────────────────────────────────────────────────────────

/// A select: the secondary button's body with the chosen label and a
/// caret; it opens a menu of the options under itself.
pub struct Dropdown {
    pub value: Mutable<usize>,
    pub options: Vec<String>,
    pub menu: Mutable<MenuState>,
    pub rect: Rect,
    /// The trusted-surface look (see [`trigger`]).
    pub attached: bool,
    on_change: Option<Rc<dyn Fn(usize)>>,
}

impl Dropdown {
    pub fn new(value: Mutable<usize>, menu: Mutable<MenuState>, options: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            value,
            options: options.into_iter().map(Into::into).collect(),
            menu,
            rect: Rect::new(0.0, 0.0, 180.0, size::BUTTON_HEIGHT_DEFAULT),
            attached: false,
            on_change: None,
        }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.rect.size.w = w;
        self
    }
    /// The trusted-surface look: lit while open, the list joined to it.
    pub fn attached(mut self) -> Self {
        self.attached = true;
        self
    }
    pub fn on_change(mut self, f: impl Fn(usize) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl View for Dropdown {
    fn render(&self, ctx: &mut Ctx) {
        let r = self.rect;
        let cur = self.value.get().min(self.options.len().saturating_sub(1));
        let label = self.options.get(cur).cloned().unwrap_or_default();
        trigger(ctx, r, &label, &self.menu, Some(cur), self.attached);
        if self.menu.get().open {
            let items: Vec<MenuItem> = self.options.iter().map(MenuItem::new).collect();
            let (value, cb) = (self.value.clone(), self.on_change.clone());
            let m = Menu::new(self.menu.clone(), items, move |i| {
                value.set(i);
                if let Some(f) = &cb {
                    f(i);
                }
            });
            open_under(ctx, m, r, self.attached, crate::popup::Align::Start);
        }
    }
}

/// The menu under a trigger at `r`: a pop-up hanging from it when the
/// window allows, at least as wide. Attached: no gap, joined to it.
fn open_under(ctx: &mut Ctx, m: Menu, r: Rect, attached: bool, align: crate::popup::Align) {
    let gap = if attached { 0.0 } else { MENU_GAP };
    let w = r.w().max(160.0);
    let x = if align == crate::popup::Align::End { r.x() + r.w() - w } else { r.x() };
    let mut m = m
        .at(x, r.y() + r.h() + gap)
        .width(w)
        // The gap is part of the anchor, so a flipped list keeps it.
        .anchored(Rect::new(r.x(), r.y() - gap, r.w(), r.h() + 2.0 * gap), crate::popup::Side::Below, align);
    m.attached = attached;
    ctx.overlay(move |ctx| m.render(ctx));
}

/// A button that opens a menu: a secondary button's body with `label` and
/// a caret. `cur`: the item the keyboard opens on (a select's value).
///
/// `attached` is the trusted-surface look (the Forum bar, visual language
/// §14.2 "Menu button, attached"): while open the button is filled with
/// the accent, its label and caret on it, the caret turned up, and the
/// menu hangs from it with no gap inside an accent edge. An app cannot
/// draw in the bar, so a fake menu under it has no lit button above it.
fn trigger(ctx: &mut Ctx, r: Rect, label: &str, menu: &Mutable<MenuState>, cur: Option<usize>, attached: bool) -> NodeId {
    trigger_with(ctx, r, label, None, menu, cur, attached)
}

/// An icon trigger's glyph: Phosphor char, size, colour (default
/// `text-secondary`).
#[derive(Clone, Copy)]
pub struct TriggerIcon {
    pub glyph: char,
    pub size: f32,
    pub color: Option<Color>,
}

fn trigger_with(ctx: &mut Ctx, r: Rect, label: &str, icon: Option<TriggerIcon>, menu: &Mutable<MenuState>, cur: Option<usize>, attached: bool) -> NodeId {
    let t = ctx.theme;
    let open = menu.get().open;
    let lit = attached && open;
    if let Some(ic) = icon {
        return icon_trigger(ctx, r, ic, menu, lit);
    }
    let halo = ctx.add(Node::Rect { rect: inflate(r, 3.0), fill: Color::TRANSPARENT, radius: radius::SM + 3.0 });
    let ring = ctx.add(Node::Rect { rect: inflate(r, 1.0), fill: Color::TRANSPARENT, radius: radius::SM + 1.0 });
    let edge = ctx.add(Node::Rect { rect: r, fill: if lit { t.accent_fill() } else { t.border_strong() }, radius: radius::SM });
    let body = if lit {
        if ctx.is_hovered(edge) || ctx.is_pressed(edge) { t.accent_fill_hover() } else { t.accent_fill() }
    } else {
        let base = t.bg_elevated();
        if ctx.is_pressed(edge) || open {
            t.state_pressed().over(base)
        } else if ctx.is_hovered(edge) {
            t.state_hover().over(base)
        } else {
            base
        }
    };
    ctx.tree.insert(Some(edge), Node::Rect { rect: inflate(r, -stroke::DEFAULT), fill: body, radius: radius::SM - stroke::DEFAULT });
    let fg = if lit { t.text_on_accent() } else { t.text_primary() };
    let ls = Node::measure_text(label, &text(fg, type_size::MD, font::SANS, Weight::Medium));
    ctx.tree.insert(Some(edge), Node::Text {
        rect: Rect::new(r.x() + space::SM, r.y() + (r.h() - ls.h) / 2.0, ls.w, ls.h),
        content: label.to_string(),
        style: text(fg, type_size::MD, font::SANS, Weight::Medium),
    });
    // The caret: a chevron of two strokes, icon-sm wide; up while lit.
    let (cx, cy) = (r.x() + r.w() - space::SM - 8.0, r.y() + r.h() / 2.0);
    let (c, d) = if lit { (t.text_on_accent(), -2.0) } else { (t.text_secondary(), 2.0) };
    ctx.tree.insert(Some(edge), Node::Path { p0: (cx - 4.0, cy - d), p1: (cx, cy + d), width: 1.5, color: c });
    ctx.tree.insert(Some(edge), Node::Path { p0: (cx, cy + d), p1: (cx + 4.0, cy - d), width: 1.5, color: c });
    if ctx.is_focus_visible(edge) {
        for (id, col) in [(halo, t.accent_bg()), (ring, t.focus_ring())] {
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                *fill = col;
            }
        }
    }
    let m = menu.clone();
    ctx.on_click(edge, move || {
        let open = m.get().open;
        m.set(if open { MenuState::default() } else { MenuState::opened(None) });
    });
    let m = menu.clone();
    ctx.on_key(edge, move |ev| {
        if let Event::Key { kind: KeyEventKind::Down, key, chars, .. } = ev {
            if matches!(key, Key::Down | Key::Up | Key::Enter) || (*key == Key::Char && chars == " ") {
                m.set(MenuState::opened(Some(cur.unwrap_or(0))));
            }
        }
    });
    ctx.focusable(edge);
    edge
}

/// An icon trigger (a ghost button: no edge, a hover fill; the bar's
/// icons): lit, it is filled with the accent and the glyph is
/// `text-on-accent`.
fn icon_trigger(ctx: &mut Ctx, r: Rect, ic: TriggerIcon, menu: &Mutable<MenuState>, lit: bool) -> NodeId {
    let t = ctx.theme;
    let halo = ctx.add(Node::Rect { rect: inflate(r, 3.0), fill: Color::TRANSPARENT, radius: radius::SM + 3.0 });
    let ring = ctx.add(Node::Rect { rect: inflate(r, 1.0), fill: Color::TRANSPARENT, radius: radius::SM + 1.0 });
    let edge = ctx.add(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: radius::SM });
    let fill = if lit {
        if ctx.is_hovered(edge) || ctx.is_pressed(edge) { t.accent_fill_hover() } else { t.accent_fill() }
    } else if ctx.is_pressed(edge) || menu.get().open {
        t.state_pressed()
    } else if ctx.is_hovered(edge) {
        t.state_hover()
    } else {
        Color::TRANSPARENT
    };
    if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(edge) {
        *f = fill;
    }
    let color = if lit { t.text_on_accent() } else { ic.color.unwrap_or_else(|| t.text_secondary()) };
    let gs = Node::measure_text(&ic.glyph.to_string(), &text(color, ic.size, font::ICONS, Weight::Regular));
    ctx.tree.insert(Some(edge), Node::Text {
        rect: Rect::new(r.x() + (r.w() - gs.w) / 2.0, r.y() + (r.h() - gs.h) / 2.0, gs.w, gs.h),
        content: ic.glyph.to_string(),
        style: text(color, ic.size, font::ICONS, Weight::Regular),
    });
    if ctx.is_focus_visible(edge) {
        for (id, col) in [(halo, t.accent_bg()), (ring, t.focus_ring())] {
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                *fill = col;
            }
        }
    }
    let m = menu.clone();
    ctx.on_click(edge, move || {
        let open = m.get().open;
        m.set(if open { MenuState::default() } else { MenuState::opened(None) });
    });
    let m = menu.clone();
    ctx.on_key(edge, move |ev| {
        if let Event::Key { kind: KeyEventKind::Down, key, chars, .. } = ev {
            if matches!(key, Key::Down | Key::Up | Key::Enter) || (*key == Key::Char && chars == " ") {
                m.set(MenuState::opened(Some(0)));
            }
        }
    });
    ctx.focusable(edge);
    edge
}

// ── MenuButton ──────────────────────────────────────────────────────────

/// A button that opens a menu of actions (the Forum bar's controller
/// menu): the label stays, an item runs `on_pick`. `attached()` gives the
/// trusted-surface look (see [`trigger`]).
pub struct MenuButton {
    pub label: String,
    pub items: Vec<MenuItem>,
    pub menu: Mutable<MenuState>,
    pub rect: Rect,
    pub attached: bool,
    /// An icon instead of the label and caret (the label stays its
    /// accessible name).
    pub icon: Option<TriggerIcon>,
    on_pick: Rc<dyn Fn(usize)>,
}

impl MenuButton {
    pub fn new(label: impl Into<String>, menu: Mutable<MenuState>, items: Vec<MenuItem>, on_pick: impl Fn(usize) + 'static) -> Self {
        Self {
            label: label.into(),
            items,
            menu,
            rect: Rect::new(0.0, 0.0, 160.0, size::BUTTON_HEIGHT_DEFAULT),
            attached: false,
            icon: None,
            on_pick: Rc::new(on_pick),
        }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.rect.size.w = w;
        self
    }
    pub fn height(mut self, h: f32) -> Self {
        self.rect.size.h = h;
        self
    }
    /// The trusted-surface look: lit while open, the menu joined to it.
    pub fn attached(mut self) -> Self {
        self.attached = true;
        self
    }
    /// Show a Phosphor glyph (`size` px, `color` or `text-secondary`)
    /// instead of the label: a square ghost button `size` + 12 wide, as
    /// tall as its rect. In a row it takes its natural size.
    pub fn icon(mut self, glyph: char, size: f32, color: Option<Color>) -> Self {
        self.icon = Some(TriggerIcon { glyph, size, color });
        self.rect.size.w = (size + 12.0).round();
        self.rect.size.h = self.rect.size.h.min(size + 12.0).max(size + 8.0);
        self
    }
}

impl View for MenuButton {
    fn render(&self, ctx: &mut Ctx) {
        let r = self.rect;
        trigger_with(ctx, r, &self.label, self.icon, &self.menu, None, self.attached);
        if self.menu.get().open {
            let pick = self.on_pick.clone();
            let m = Menu::new(self.menu.clone(), self.items.clone(), move |i| pick(i));
            // An icon button is small and sits at the end of a row (the
            // bar's right side): its menu lines up with its right edge.
            let align = if self.icon.is_some() { crate::popup::Align::End } else { crate::popup::Align::Start };
            open_under(ctx, m, r, self.attached, align);
        }
    }
}

// ── Dialog ──────────────────────────────────────────────────────────────

/// One of a dialog's buttons; the last is the primary one (§14.2).
pub struct DialogButton {
    pub label: String,
    pub destructive: bool,
    pub action: Arc<dyn Fn() + Send + Sync>,
}

impl DialogButton {
    pub fn new(label: impl Into<String>, action: impl Fn() + Send + Sync + 'static) -> Self {
        Self { label: label.into(), destructive: false, action: Arc::new(action) }
    }
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

/// A modal dialog over a scrim, centred in `within` (the window's rect).
/// Escape and a press on the scrim cancel it (they close it and call
/// nothing); a button closes it and runs its action.
pub struct Dialog {
    pub open: Mutable<bool>,
    pub title: String,
    pub body: String,
    pub buttons: Vec<DialogButton>,
    pub within: Rect,
    pub width: f32,
}

impl Dialog {
    pub fn new(open: Mutable<bool>, within: Rect, title: impl Into<String>, body: impl Into<String>, buttons: Vec<DialogButton>) -> Self {
        Self { open, title: title.into(), body: body.into(), buttons, within, width: 400.0 }
    }
}

impl View for Dialog {
    fn render(&self, ctx: &mut Ctx) {
        if !self.open.get() {
            return;
        }
        let t = ctx.theme;
        let scrim = ctx.add(Node::Rect { rect: EVERYWHERE, fill: t.scrim(), radius: 0.0 });
        let o = self.open.clone();
        ctx.on_click(scrim, move || o.set(false));

        // Laid out: a column the size of `within` centres the body; the
        // body is a padded column (title, text, then the buttons' row).
        let w = self.width.min(self.within.w() - 2.0 * space::LG);
        let o = self.open.clone();
        let centre = ctx.push(Node::stack(Axis::Vertical, self.within, 0.0));
        ctx.set_flex(centre, FlexStyle { justify: Justify::Center, align: Align::Center, ..FlexStyle::default() });
        ctx.on_click(centre, move || o.set(false));
        let body = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, w, 0.0), space::LG));
        ctx.set_flex(body, FlexStyle { padding: space::LG, ..FlexStyle::default() });
        let inset = |d: f32| FlexStyle { absolute_inset: Some([Some(d), Some(d), Some(d), Some(d)]), ..FlexStyle::default() };
        let edge = ctx.add(Node::Rect { rect: Rect::ZERO_SIZED, fill: t.border_default(), radius: radius::LG });
        ctx.set_flex(edge, inset(0.0));
        let fill = ctx.add(Node::Rect { rect: Rect::ZERO_SIZED, fill: t.bg_elevated(), radius: radius::LG - stroke::DEFAULT });
        ctx.set_flex(fill, inset(stroke::DEFAULT));
        ctx.autofocus(body);
        ctx.on_click(body, || {});
        let o = self.open.clone();
        ctx.on_key(body, move |ev| {
            if matches!(ev, Event::Key { kind: KeyEventKind::Down, key: Key::Escape, .. }) {
                o.set(false);
            }
        });
        ctx.push(Node::stack(Axis::Vertical, Rect::ZERO_SIZED, space::SM));
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: self.title.clone(), style: text(t.text_primary(), type_size::LG, font::SANS, Weight::Semibold) });
        ctx.paragraph(self.body.clone(), text(t.text_secondary(), type_size::SM, font::SANS, Weight::Regular));
        ctx.pop();

        // Buttons bottom right, primary last.
        let row = ctx.push(Node::stack(Axis::Horizontal, Rect::ZERO_SIZED, space::XS));
        ctx.set_flex(row, FlexStyle { justify: Justify::End, align: Align::Baseline, ..FlexStyle::default() });
        let last = self.buttons.len().saturating_sub(1);
        for (i, b) in self.buttons.iter().enumerate() {
            let (o, act) = (self.open.clone(), b.action.clone());
            let btn = if i == last && b.destructive {
                Button::destructive(b.label.clone())
            } else if i == last {
                Button::primary(b.label.clone())
            } else {
                Button::secondary(b.label.clone())
            };
            ctx.place(btn.on_click(move || {
                o.set(false);
                act();
            }));
        }
        ctx.pop();
        ctx.pop();
        ctx.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::Modifiers;
    use crate::geom::Point;
    use std::sync::atomic::AtomicBool;

    fn key(app: &mut App<impl View>, k: Key) {
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: String::new() });
        app.tick();
    }

    fn click(app: &mut App<impl View>, x: f32, y: f32) {
        app.handle_event(Event::PointerDown { at: Point::new(x, y) });
        app.handle_event(Event::PointerUp { at: Point::new(x, y) });
        app.tick();
    }

    struct Form {
        value: Mutable<usize>,
        menu: Mutable<MenuState>,
    }
    impl View for Form {
        fn render(&self, ctx: &mut Ctx) {
            Dropdown::new(self.value.clone(), self.menu.clone(), ["Daily", "Weekly", "Never"]).at(10.0, 10.0).render(ctx);
        }
    }

    fn form() -> (App<Form>, Mutable<usize>, Mutable<MenuState>) {
        let d = Arc::new(AtomicBool::new(true));
        let value = Mutable::with_dirty(0usize, d.clone());
        let menu = Mutable::with_dirty(MenuState::default(), d.clone());
        let mut app = App::new_with_flag(Form { value: value.clone(), menu: menu.clone() }, d);
        app.tick();
        (app, value, menu)
    }

    #[test]
    fn a_dropdown_opens_picks_and_gives_focus_back() {
        let (mut app, value, menu) = form();
        key(&mut app, Key::Tab);
        let button = app.focused();
        key(&mut app, Key::Down);
        assert!(menu.get().open);
        assert_ne!(app.focused(), button, "the menu took focus");
        key(&mut app, Key::Down);
        key(&mut app, Key::Down);
        key(&mut app, Key::Enter);
        assert_eq!(value.get(), 2);
        assert!(!menu.get().open);
        app.tick();
        assert_eq!(app.focused(), button, "focus is back on the dropdown");
    }

    #[test]
    fn a_click_on_an_item_picks_it_and_a_click_outside_closes() {
        let (mut app, value, menu) = form();
        click(&mut app, 20.0, 20.0);
        assert!(menu.get().open);
        // Second item: below the button (32) + gap (4) + inset (4) + one row.
        let y = 10.0 + size::BUTTON_HEIGHT_DEFAULT + MENU_GAP + MENU_INSET + size::MENU_ITEM * 1.5;
        click(&mut app, 30.0, y);
        assert_eq!(value.get(), 1);
        click(&mut app, 20.0, 20.0);
        assert!(menu.get().open);
        click(&mut app, 600.0, 400.0);
        assert!(!menu.get().open, "outside closes");
        assert_eq!(value.get(), 1, "and picks nothing");
    }

    #[test]
    fn in_a_window_the_list_is_a_pop_up_that_frescod_takes_down() {
        let (mut app, value, menu) = form();
        app.set_popups(true);
        app.tick();
        click(&mut app, 20.0, 20.0);
        assert!(menu.get().open);
        let pops: Vec<_> = app.tree().popups().map(|(id, s)| (id, *s)).collect();
        assert_eq!(pops.len(), 1);
        let (root, spec) = pops[0];
        // Hanging from the button, the gap inside the anchor.
        let h = size::BUTTON_HEIGHT_DEFAULT;
        assert_eq!(spec.anchor, Rect::new(10.0, 10.0 - MENU_GAP, 180.0, h + 2.0 * MENU_GAP));
        assert_eq!((spec.side, spec.align, spec.grab), (crate::popup::Side::Below, crate::popup::Align::Start, true));
        let r = app.tree().get(root).unwrap().rect();
        assert_eq!((r.x(), r.y()), (10.0, 10.0 + h + MENU_GAP));
        // No press-catcher: frescod keeps the outside press.
        assert_eq!(app.tree().roots().count(), 4, "the dropdown's halo, ring and edge, and the pop-up");
        // Items work as before (input arrives in tree coordinates).
        let y = r.y() + MENU_INSET + size::MENU_ITEM * 1.5;
        click(&mut app, 30.0, y);
        assert_eq!(value.get(), 1);
        assert_eq!(app.tree().popups().count(), 0);
        // Dismissed by frescod: closed, nothing picked.
        click(&mut app, 20.0, 20.0);
        let root = app.tree().popups().next().unwrap().0;
        app.popup_dismissed(root);
        app.tick();
        assert!(!menu.get().open);
        assert_eq!(value.get(), 1);
    }

    struct Bar {
        menu: Mutable<MenuState>,
        picked: Rc<std::cell::Cell<Option<usize>>>,
    }
    impl View for Bar {
        fn render(&self, ctx: &mut Ctx) {
            let p = self.picked.clone();
            MenuButton::new("Controller", self.menu.clone(), vec![MenuItem::new("Pair"), MenuItem::new("Forget")], move |i| p.set(Some(i)))
                .at(10.0, 0.0)
                .attached()
                .render(ctx);
        }
    }

    #[test]
    fn an_attached_menu_button_lights_up_and_its_menu_joins_it() {
        let d = Arc::new(AtomicBool::new(true));
        let menu = Mutable::with_dirty(MenuState::default(), d.clone());
        let picked = Rc::new(std::cell::Cell::new(None));
        let mut app = App::new_with_flag(Bar { menu: menu.clone(), picked: picked.clone() }, d);
        app.set_popups(true);
        app.tick();
        let t = app.theme;
        let lit = |app: &App<Bar>| app.tree().iter().any(|(_, n)| matches!(n, Node::Rect { fill, .. } if *fill == t.accent_fill()));
        assert!(!lit(&app), "closed: an ordinary button");
        click(&mut app, 20.0, 10.0);
        assert!(menu.get().open);
        assert!(lit(&app), "open: filled with the accent");
        let (root, spec) = app.tree().popups().next().map(|(id, s)| (id, *s)).unwrap();
        let h = size::BUTTON_HEIGHT_DEFAULT;
        assert_eq!(spec.anchor, Rect::new(10.0, 0.0, 160.0, h), "no gap");
        assert_eq!(app.tree().get(root).unwrap().rect().y(), h, "it hangs from the button");
        click(&mut app, 30.0, h + MENU_INSET + size::MENU_ITEM * 1.5);
        assert_eq!(picked.get(), Some(1));
        assert!(!lit(&app), "closed again");
    }

    struct IconBar {
        menu: Mutable<MenuState>,
    }
    impl View for IconBar {
        fn render(&self, ctx: &mut Ctx) {
            let row = ctx.push(Node::stack(crate::geom::Axis::Horizontal, Rect::new(0.0, 0.0, 400.0, 32.0), 8.0));
            ctx.set_flex(row, FlexStyle { justify: Justify::End, align: Align::Center, ..FlexStyle::default() });
            let b = MenuButton::new("Controllers", self.menu.clone(), vec![MenuItem::new("Fake Pad")], |_| {})
                .icon(crate::widgets::phosphor::GAME_CONTROLLER, 15.0, None)
                .attached();
            ctx.place(b);
            ctx.pop();
        }
    }

    #[test]
    fn an_icon_menu_button_in_a_laid_out_row_hangs_its_menu_from_where_layout_put_it() {
        let d = Arc::new(AtomicBool::new(true));
        let menu = Mutable::with_dirty(MenuState::default(), d.clone());
        let mut app = App::new_with_flag(IconBar { menu: menu.clone() }, d);
        app.set_popups(true);
        app.tick();
        // 27 square, at the row's end, centred in its 32.
        click(&mut app, 386.0, 16.0);
        assert!(menu.get().open);
        let (root, spec) = app.tree().popups().next().map(|(id, s)| (id, *s)).unwrap();
        assert_eq!(spec.anchor, Rect::new(373.0, 3.0, 27.0, 27.0));
        assert_eq!(spec.align, crate::popup::Align::End);
        let r = app.tree().get(root).unwrap().rect();
        assert_eq!((r.x() + r.w(), r.y()), (400.0, 30.0), "right edges line up, no gap");
    }

    #[test]
    fn arrows_skip_disabled_items_and_escape_closes() {
        let items = vec![MenuItem::new("New"), MenuItem::new("Save").enabled(false), MenuItem::new("Close").separated()];
        assert_eq!(Menu::step(&items, Some(0), 1), Some(2));
        assert_eq!(Menu::step(&items, Some(2), 1), Some(0));
        assert_eq!(Menu::step(&items, None, -1), Some(2));
        let (mut app, _, menu) = form();
        click(&mut app, 20.0, 20.0);
        key(&mut app, Key::Escape);
        assert!(!menu.get().open);
    }

    struct Confirm {
        open: Mutable<bool>,
        done: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl View for Confirm {
        fn render(&self, ctx: &mut Ctx) {
            let d = self.done.clone();
            Dialog::new(
                self.open.clone(),
                Rect::new(0.0, 0.0, 800.0, 600.0),
                "Delete 3 messages?",
                "They move to Trash.",
                vec![DialogButton::new("Cancel", || {}), DialogButton::new("Delete", move || { d.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }).destructive()],
            )
            .render(ctx);
        }
    }

    #[test]
    fn a_dialog_cancels_on_escape_and_on_the_scrim_and_runs_its_button() {
        let d = Arc::new(AtomicBool::new(true));
        let open = Mutable::with_dirty(true, d.clone());
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut app = App::new_with_flag(Confirm { open: open.clone(), done: done.clone() }, d);
        app.tick();
        app.tick();
        assert!(app.focused().is_some(), "the dialog took focus");
        // Tab to a button: Escape still reaches the dialog.
        key(&mut app, Key::Tab);
        key(&mut app, Key::Escape);
        assert!(!open.get());
        open.set(true);
        app.tick();
        click(&mut app, 5.0, 5.0);
        assert!(!open.get(), "the scrim cancels");
        assert_eq!(done.load(std::sync::atomic::Ordering::SeqCst), 0);
        open.set(true);
        app.tick();
        app.tick();
        // Tab to the last (primary) button and press Enter.
        key(&mut app, Key::Tab);
        key(&mut app, Key::Tab);
        key(&mut app, Key::Enter);
        assert_eq!(done.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!open.get());
    }
}
