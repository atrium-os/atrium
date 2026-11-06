//! Floating surfaces drawn in the window's overlay layer
//! ([`Ctx::overlay`]): `Menu`, `Dropdown` and `Dialog` (visual language
//! rev. 2 §14.2).
//!
//! A floating surface is `bg-elevated` with a 1px `border-default` edge
//! (the §5 shadow waits for a Fresco op). It takes focus when it opens and
//! gives it back when it closes; Escape closes the topmost one, and a
//! press outside a menu or on a dialog's scrim closes it too.
//!
//! These live inside the window, so a menu near the window's edge is
//! clipped by it. Menus that reach past the window need a pop-up window
//! kind from frescod; until then, callers place them inside.

use std::rc::Rc;
use std::sync::Arc;

use crate::event::{Event, Key, KeyEventKind};
use crate::geom::Rect;
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
    on_pick: Rc<dyn Fn(usize)>,
}

impl Menu {
    pub fn new(state: Mutable<MenuState>, items: Vec<MenuItem>, on_pick: impl Fn(usize) + 'static) -> Self {
        Self { state, items, x: 0.0, y: 0.0, width: 220.0, on_pick: Rc::new(on_pick) }
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
        let t = ctx.theme;
        // A press anywhere outside closes it, and goes no further.
        let catcher = ctx.add(Node::Rect { rect: EVERYWHERE, fill: Color::TRANSPARENT, radius: 0.0 });
        let s = self.state.clone();
        ctx.on_click(catcher, move || s.set(MenuState::default()));

        let r = Rect::new(self.x, self.y, self.width, self.height());
        let body = surface(ctx, r, radius::MD);
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
            let Event::Key { kind: KeyEventKind::Down, key, chars, .. } = ev else { return };
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
    on_change: Option<Rc<dyn Fn(usize)>>,
}

impl Dropdown {
    pub fn new(value: Mutable<usize>, menu: Mutable<MenuState>, options: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            value,
            options: options.into_iter().map(Into::into).collect(),
            menu,
            rect: Rect::new(0.0, 0.0, 180.0, size::BUTTON_HEIGHT_DEFAULT),
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
    pub fn on_change(mut self, f: impl Fn(usize) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl View for Dropdown {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let r = self.rect;
        let cur = self.value.get().min(self.options.len().saturating_sub(1));
        let halo = ctx.add(Node::Rect { rect: inflate(r, 3.0), fill: Color::TRANSPARENT, radius: radius::SM + 3.0 });
        let ring = ctx.add(Node::Rect { rect: inflate(r, 1.0), fill: Color::TRANSPARENT, radius: radius::SM + 1.0 });
        let edge = ctx.add(Node::Rect { rect: r, fill: t.border_strong(), radius: radius::SM });
        let base = t.bg_elevated();
        let body = if ctx.is_pressed(edge) {
            t.state_pressed().over(base)
        } else if ctx.is_hovered(edge) {
            t.state_hover().over(base)
        } else {
            base
        };
        ctx.tree.insert(Some(edge), Node::Rect { rect: inflate(r, -stroke::DEFAULT), fill: body, radius: radius::SM - stroke::DEFAULT });
        let label = self.options.get(cur).cloned().unwrap_or_default();
        let ls = Node::measure_text(&label, &text(t.text_primary(), type_size::MD, font::SANS, Weight::Medium));
        ctx.tree.insert(Some(edge), Node::Text {
            rect: Rect::new(r.x() + space::SM, r.y() + (r.h() - ls.h) / 2.0, ls.w, ls.h),
            content: label,
            style: text(t.text_primary(), type_size::MD, font::SANS, Weight::Medium),
        });
        // The caret: a chevron of two strokes, icon-sm wide.
        let (cx, cy) = (r.x() + r.w() - space::SM - 8.0, r.y() + r.h() / 2.0);
        let c = t.text_secondary();
        ctx.tree.insert(Some(edge), Node::Path { p0: (cx - 4.0, cy - 2.0), p1: (cx, cy + 2.0), width: 1.5, color: c });
        ctx.tree.insert(Some(edge), Node::Path { p0: (cx, cy + 2.0), p1: (cx + 4.0, cy - 2.0), width: 1.5, color: c });
        if ctx.is_focus_visible(edge) {
            for (id, col) in [(halo, t.accent_bg()), (ring, t.focus_ring())] {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                    *fill = col;
                }
            }
        }
        let menu = self.menu.clone();
        ctx.on_click(edge, move || {
            let open = menu.get().open;
            menu.set(if open { MenuState::default() } else { MenuState::opened(None) });
        });
        let menu = self.menu.clone();
        ctx.on_key(edge, move |ev| {
            if let Event::Key { kind: KeyEventKind::Down, key, chars, .. } = ev {
                if matches!(key, Key::Down | Key::Up | Key::Enter) || (*key == Key::Char && chars == " ") {
                    menu.set(MenuState::opened(Some(cur)));
                }
            }
        });
        ctx.focusable(edge);
        if self.menu.get().open {
            let items: Vec<MenuItem> = self.options.iter().map(MenuItem::new).collect();
            let (value, cb) = (self.value.clone(), self.on_change.clone());
            let m = Menu::new(self.menu.clone(), items, move |i| {
                value.set(i);
                if let Some(f) = &cb {
                    f(i);
                }
            })
            .at(r.x(), r.y() + r.h() + MENU_GAP)
            .width(r.w().max(160.0));
            ctx.overlay(move |ctx| m.render(ctx));
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

        let pad = space::LG;
        let w = self.width.min(self.within.w() - 2.0 * space::LG);
        let title_s = text(t.text_primary(), type_size::LG, font::SANS, Weight::Semibold);
        let body_s = text(t.text_secondary(), type_size::SM, font::SANS, Weight::Regular);
        let th = Node::measure_text(&self.title, &title_s).h;
        let bh = Node::measure_text(&self.body, &body_s).h;
        let btn_h = size::BUTTON_HEIGHT_DEFAULT;
        let h = pad + th + space::SM + bh + space::LG + btn_h + pad;
        let r = Rect::new(self.within.x() + (self.within.w() - w) / 2.0, self.within.y() + (self.within.h() - h) / 2.0, w, h);
        let body = surface(ctx, r, radius::LG);
        ctx.autofocus(body);
        ctx.on_click(body, || {});
        let o = self.open.clone();
        ctx.on_key(body, move |ev| {
            if matches!(ev, Event::Key { kind: KeyEventKind::Down, key: Key::Escape, .. }) {
                o.set(false);
            }
        });
        ctx.tree.insert(Some(body), Node::Text { rect: Rect::new(r.x() + pad, r.y() + pad, w - 2.0 * pad, th), content: self.title.clone(), style: title_s });
        ctx.tree.insert(Some(body), Node::Text { rect: Rect::new(r.x() + pad, r.y() + pad + th + space::SM, w - 2.0 * pad, bh), content: self.body.clone(), style: body_s });

        // Buttons bottom right, primary last.
        let by = r.y() + h - pad - btn_h;
        let mut x = r.x() + w - pad;
        let last = self.buttons.len().saturating_sub(1);
        let mut placed = Vec::new();
        for (i, b) in self.buttons.iter().enumerate().rev() {
            let bw = Node::measure_text(&b.label, &text(t.text_primary(), type_size::MD, font::SANS, Weight::Medium)).w + 2.0 * space::SM;
            x -= bw;
            placed.push((i, x, bw));
            x -= space::XS;
        }
        placed.reverse();
        for (i, bx, bw) in placed {
            let b = &self.buttons[i];
            let (o, act) = (self.open.clone(), b.action.clone());
            let btn = if i == last && b.destructive {
                Button::destructive(b.label.clone())
            } else if i == last {
                Button::primary(b.label.clone())
            } else {
                Button::secondary(b.label.clone())
            };
            btn.at(bx, by).width(bw).on_click(move || {
                o.set(false);
                act();
            })
            .render(ctx);
        }
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
