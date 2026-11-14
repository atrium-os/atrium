//! Choice controls (visual language rev. 2 §14.2): `Checkbox`, `RadioGroup`,
//! `Toggle`, `Segmented` and `Tabs`.
//!
//! Each holds its value in a `Mutable` the app owns. A click or Space
//! changes it; a group (radios, segments, tabs) is one Tab stop and its
//! arrows move the choice (§14.3). Every control draws the same nodes in
//! every state, so ids stay stable, and shows the §14.1 focus ring only for
//! keyboard focus. Placement is absolute (`.at`), like `Button`.

use std::rc::Rc;

use crate::event::{Event, Key, KeyEventKind};
use crate::geom::Rect;
use crate::node::{Node, NodeId, TextStyle};
use crate::reactive::Mutable;
use crate::theme::{font, radius, size, space, stroke, type_size, Weight};
use crate::view::{Ctx, View};
use crate::Color;

/// Box and circle side (rev. 2 §14.2).
pub const BOX: f32 = 16.0;
/// Gap from a box to its label.
pub const LABEL_GAP: f32 = space::XS;
/// Toggle track.
pub const TOGGLE_W: f32 = 32.0;
pub const TOGGLE_H: f32 = 18.0;
pub const TOGGLE_THUMB: f32 = 14.0;
/// The radio's dot.
pub const RADIO_DOT: f32 = 6.0;
/// Tabs: gap between labels, and the selected tab's bar.
pub const TAB_GAP: f32 = space::MD;
pub const TAB_BAR: f32 = 2.0;
/// Space between radio rows.
pub const RADIO_ROW_GAP: f32 = space::XS;

pub type Changed<T> = Rc<dyn Fn(T)>;

/// A size above at the system text size: a box grows with its label
/// (resolution-independence.md §6.2).
fn em(pt: f32) -> f32 { size::em(pt) }

fn inflate(r: Rect, d: f32) -> Rect {
    Rect::new(r.x() - d, r.y() - d, r.w() + 2.0 * d, r.h() + 2.0 * d)
}

fn label_style(color: Color, sz: f32) -> TextStyle {
    TextStyle { family: font::SANS.into(), size: sz, weight: Weight::Medium, color }
}

/// The focus halo and ring around `r` (§14.1): emitted always, coloured
/// only while `on`. Returns their ids so a caller can colour them later.
fn focus_ring(ctx: &mut Ctx, r: Rect, rad: f32) -> (NodeId, NodeId) {
    let halo = ctx.add(Node::Rect { rect: inflate(r, 3.0), fill: Color::TRANSPARENT, radius: rad + 3.0 });
    let ring = ctx.add(Node::Rect { rect: inflate(r, 1.0), fill: Color::TRANSPARENT, radius: rad + 1.0 });
    (halo, ring)
}

fn light_ring(ctx: &mut Ctx, (halo, ring): (NodeId, NodeId)) {
    let t = ctx.theme;
    if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(halo) {
        *fill = t.accent_bg();
    }
    if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(ring) {
        *fill = t.focus_ring();
    }
}

fn set_fill(ctx: &mut Ctx, id: NodeId, c: Color) {
    if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
        *fill = c;
    }
}

fn is_space(ev: &Event) -> bool {
    matches!(ev, Event::Key { kind: KeyEventKind::Down, key: Key::Char, chars, modifiers } if chars == " " && !modifiers.ctrl && !modifiers.meta)
}

/// Arrow keys as a step through a group: -1, +1, or none.
fn arrow(ev: &Event) -> Option<isize> {
    match ev {
        Event::Key { kind: KeyEventKind::Down | KeyEventKind::Repeat, key: Key::Left | Key::Up, .. } => Some(-1),
        Event::Key { kind: KeyEventKind::Down | KeyEventKind::Repeat, key: Key::Right | Key::Down, .. } => Some(1),
        _ => None,
    }
}

// ── Checkbox ────────────────────────────────────────────────────────────

/// A checkbox's value: `Mixed` shows a bar, for a parent of mixed children;
/// a click on it checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Off,
    On,
    Mixed,
}

pub struct Checkbox {
    pub value: Mutable<Check>,
    pub label: String,
    pub enabled: bool,
    pub x: f32,
    pub y: f32,
    on_change: Option<Changed<Check>>,
}

impl Checkbox {
    pub fn new(value: Mutable<Check>, label: impl Into<String>) -> Self {
        Self { value, label: label.into(), enabled: true, x: 0.0, y: 0.0, on_change: None }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.x = x;
        self.y = y;
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn on_change(mut self, f: impl Fn(Check) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

/// Box plus label, one hit target (§14.2: the label is part of it).
fn box_and_label(ctx: &mut Ctx, x: f32, y: f32, label: &str, round: bool, enabled: bool) -> (NodeId, Rect, (NodeId, NodeId), NodeId) {
    let t = ctx.theme;
    let color = if enabled { t.text_primary() } else { t.text_disabled() };
    let ls = Node::measure_text(label, &label_style(color, type_size::MD));
    let h = em(BOX).max(ls.h);
    let hit = Rect::new(x, y, em(BOX) + LABEL_GAP + ls.w, h);
    let bx = Rect::new(x, y + (h - em(BOX)) / 2.0, em(BOX), em(BOX));
    let rad = if round { em(BOX) / 2.0 } else { radius::XS };
    let ring = focus_ring(ctx, bx, rad);
    // The hit target: transparent, the whole box and label.
    let target = ctx.add(Node::Rect { rect: hit, fill: Color::TRANSPARENT, radius: 0.0 });
    let edge = ctx.tree.insert(Some(target), Node::Rect { rect: bx, fill: t.border_control(), radius: rad });
    ctx.tree.insert(Some(target), Node::Text {
        rect: Rect::new(x + em(BOX) + LABEL_GAP, y + (h - ls.h) / 2.0, ls.w, ls.h),
        content: label.into(),
        style: label_style(color, type_size::MD),
    });
    (target, bx, ring, edge)
}

impl View for Checkbox {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let v = self.value.get();
        let (target, bx, ring, edge) = box_and_label(ctx, self.x, self.y, &self.label, false, self.enabled);
        let on = v != Check::Off;
        let hovered = ctx.is_hovered(target) && self.enabled;
        let pressed = ctx.is_pressed(target) && self.enabled;
        // The box: edge (or the fill, when on), then the inside.
        let fill = if !self.enabled {
            t.bg_surface()
        } else if on {
            if pressed { t.accent_fill_pressed() } else if hovered { t.accent_fill_hover() } else { t.accent_fill() }
        } else {
            let base = t.bg_canvas();
            if pressed { t.state_pressed().over(base) } else if hovered { t.state_hover().over(base) } else { base }
        };
        set_fill(ctx, edge, if on && self.enabled { fill } else if self.enabled { t.border_control() } else { t.border_default() });
        ctx.tree.insert(Some(edge), Node::Rect { rect: inflate(bx, -stroke::DEFAULT), fill, radius: radius::XS - stroke::DEFAULT });
        // The mark: a check (two strokes) or the mixed bar, clear when off.
        let mark = if on { t.text_on_accent() } else { Color::TRANSPARENT };
        let (cx, cy) = (bx.x(), bx.y());
        let (check, bar) = match v {
            Check::On => (mark, Color::TRANSPARENT),
            Check::Mixed => (Color::TRANSPARENT, mark),
            Check::Off => (Color::TRANSPARENT, Color::TRANSPARENT),
        };
        let u = em(1.0);
        ctx.tree.insert(Some(edge), Node::Path { p0: (cx + 4.0 * u, cy + 8.3 * u), p1: (cx + 6.7 * u, cy + 11.0 * u), width: 1.6 * u, color: check });
        ctx.tree.insert(Some(edge), Node::Path { p0: (cx + 6.7 * u, cy + 11.0 * u), p1: (cx + 12.0 * u, cy + 5.0 * u), width: 1.6 * u, color: check });
        ctx.tree.insert(Some(edge), Node::Rect { rect: Rect::new(cx + 4.0 * u, cy + 7.0 * u, 8.0 * u, 2.0 * u), fill: bar, radius: u });
        if ctx.is_focus_visible(target) {
            light_ring(ctx, ring);
        }
        if !self.enabled {
            return;
        }
        let flip = {
            let value = self.value.clone();
            let cb = self.on_change.clone();
            move || {
                let next = if value.get() == Check::On { Check::Off } else { Check::On };
                value.set(next);
                if let Some(f) = &cb {
                    f(next);
                }
            }
        };
        let f = flip.clone();
        ctx.on_click(target, move || f());
        ctx.focusable(target);
        ctx.on_key(target, move |ev| {
            if is_space(ev) {
                flip();
            }
        });
    }
}

// ── Toggle ──────────────────────────────────────────────────────────────

pub struct Toggle {
    pub value: Mutable<bool>,
    pub label: String,
    pub enabled: bool,
    pub x: f32,
    pub y: f32,
    on_change: Option<Changed<bool>>,
}

impl Toggle {
    pub fn new(value: Mutable<bool>, label: impl Into<String>) -> Self {
        Self { value, label: label.into(), enabled: true, x: 0.0, y: 0.0, on_change: None }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.x = x;
        self.y = y;
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn on_change(mut self, f: impl Fn(bool) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl View for Toggle {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let on = self.value.get();
        let color = if self.enabled { t.text_primary() } else { t.text_disabled() };
        let ls = Node::measure_text(&self.label, &label_style(color, type_size::MD));
        let h = em(TOGGLE_H).max(ls.h);
        let track = Rect::new(self.x, self.y + (h - em(TOGGLE_H)) / 2.0, em(TOGGLE_W), em(TOGGLE_H));
        let ring = focus_ring(ctx, track, em(TOGGLE_H) / 2.0);
        let target = ctx.add(Node::Rect {
            rect: Rect::new(self.x, self.y, em(TOGGLE_W) + LABEL_GAP + ls.w, h),
            fill: Color::TRANSPARENT,
            radius: 0.0,
        });
        let hovered = ctx.is_hovered(target) && self.enabled;
        let (edge, inside, thumb) = match (on, self.enabled) {
            (_, false) => (t.border_default(), t.bg_surface(), t.border_default()),
            (true, true) => {
                let f = if hovered { t.accent_fill_hover() } else { t.accent_fill() };
                (f, f, t.text_on_accent())
            }
            (false, true) => {
                let base = t.bg_canvas();
                (t.border_control(), if hovered { t.state_hover().over(base) } else { base }, t.border_control())
            }
        };
        let e = ctx.tree.insert(Some(target), Node::Rect { rect: track, fill: edge, radius: em(TOGGLE_H) / 2.0 });
        ctx.tree.insert(Some(e), Node::Rect { rect: inflate(track, -stroke::DEFAULT), fill: inside, radius: em(TOGGLE_H) / 2.0 - stroke::DEFAULT });
        let inset = (em(TOGGLE_H) - em(TOGGLE_THUMB)) / 2.0;
        let tx = if on { track.x() + em(TOGGLE_W) - inset - em(TOGGLE_THUMB) } else { track.x() + inset };
        ctx.tree.insert(Some(e), Node::Rect { rect: Rect::new(tx, track.y() + inset, em(TOGGLE_THUMB), em(TOGGLE_THUMB)), fill: thumb, radius: em(TOGGLE_THUMB) / 2.0 });
        ctx.tree.insert(Some(target), Node::Text {
            rect: Rect::new(self.x + em(TOGGLE_W) + LABEL_GAP, self.y + (h - ls.h) / 2.0, ls.w, ls.h),
            content: self.label.clone(),
            style: label_style(color, type_size::MD),
        });
        if ctx.is_focus_visible(target) {
            light_ring(ctx, ring);
        }
        if !self.enabled {
            return;
        }
        let flip = {
            let value = self.value.clone();
            let cb = self.on_change.clone();
            move || {
                let next = !value.get();
                value.set(next);
                if let Some(f) = &cb {
                    f(next);
                }
            }
        };
        let f = flip.clone();
        ctx.on_click(target, move || f());
        ctx.focusable(target);
        ctx.on_key(target, move |ev| {
            if is_space(ev) {
                flip();
            }
        });
    }
}

// ── Groups: radios, segments, tabs ──────────────────────────────────────

/// Arrows and Space on a group that is one focus stop.
fn group_keys(ctx: &mut Ctx, id: NodeId, value: Mutable<usize>, n: usize, cb: Option<Changed<usize>>) {
    if n == 0 {
        return;
    }
    ctx.focusable(id);
    ctx.on_key(id, move |ev| {
        if let Some(step) = arrow(ev) {
            let next = (value.get() as isize + step).rem_euclid(n as isize) as usize;
            value.set(next);
            if let Some(f) = &cb {
                f(next);
            }
        }
    });
}

fn pick(value: &Mutable<usize>, i: usize, cb: &Option<Changed<usize>>) -> impl Fn() + 'static {
    let value = value.clone();
    let cb = cb.clone();
    move || {
        value.set(i);
        if let Some(f) = &cb {
            f(i);
        }
    }
}

/// Radios in a column; `value` is the chosen index.
pub struct RadioGroup {
    pub value: Mutable<usize>,
    pub labels: Vec<String>,
    pub x: f32,
    pub y: f32,
    on_change: Option<Changed<usize>>,
}

impl RadioGroup {
    pub fn new(value: Mutable<usize>, labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { value, labels: labels.into_iter().map(Into::into).collect(), x: 0.0, y: 0.0, on_change: None }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.x = x;
        self.y = y;
        self
    }
    pub fn on_change(mut self, f: impl Fn(usize) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl View for RadioGroup {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let chosen = self.value.get();
        let row_h = em(BOX).max(Node::measure_text("Mg", &label_style(t.text_primary(), type_size::MD)).h);
        let total = Rect::new(self.x, self.y, 0.0, self.labels.len() as f32 * (row_h + RADIO_ROW_GAP));
        let group = ctx.add(Node::Rect { rect: total, fill: Color::TRANSPARENT, radius: 0.0 });
        let visible = ctx.is_focus_visible(group);
        for (i, label) in self.labels.iter().enumerate() {
            let y = self.y + i as f32 * (row_h + RADIO_ROW_GAP);
            let (target, bx, ring, edge) = box_and_label(ctx, self.x, y, label, true, true);
            let on = i == chosen;
            let hovered = ctx.is_hovered(target);
            let base = t.bg_canvas();
            let fill = if on { t.accent_fill() } else if hovered { t.state_hover().over(base) } else { base };
            if on {
                set_fill(ctx, edge, fill);
            }
            ctx.tree.insert(Some(edge), Node::Rect { rect: inflate(bx, -stroke::DEFAULT), fill, radius: em(BOX) / 2.0 - stroke::DEFAULT });
            let c = (em(BOX) - em(RADIO_DOT)) / 2.0;
            ctx.tree.insert(Some(edge), Node::Rect {
                rect: Rect::new(bx.x() + c, bx.y() + c, em(RADIO_DOT), em(RADIO_DOT)),
                fill: if on { t.text_on_accent() } else { Color::TRANSPARENT },
                radius: em(RADIO_DOT) / 2.0,
            });
            // The ring sits on the chosen radio while the group has it.
            if visible && on {
                light_ring(ctx, ring);
            }
            ctx.on_click(target, pick(&self.value, i, &self.on_change));
        }
        group_keys(ctx, group, self.value.clone(), self.labels.len(), self.on_change.clone());
    }
}

/// A segmented control: equal segments on a `bg-surface` track; the chosen
/// one is a raised pill.
pub struct Segmented {
    pub value: Mutable<usize>,
    pub labels: Vec<String>,
    pub rect: Rect,
    on_change: Option<Changed<usize>>,
}

impl Segmented {
    pub fn new(value: Mutable<usize>, labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let labels: Vec<String> = labels.into_iter().map(Into::into).collect();
        Self { value, labels, rect: Rect::new(0.0, 0.0, 0.0, size::button_height()), on_change: None }
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

impl View for Segmented {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let n = self.labels.len().max(1);
        let chosen = self.value.get();
        let inset = 2.0;
        let mut r = self.rect;
        r.size.w = self.natural_width();
        let ring = focus_ring(ctx, r, radius::SM);
        // The track: a `border-default` edge round a `bg-surface` fill, so
        // it shows on a `bg-surface` page too.
        let track = ctx.add(Node::Rect { rect: r, fill: t.border_default(), radius: radius::SM });
        ctx.tree.insert(Some(track), Node::Rect { rect: inflate(r, -stroke::DEFAULT), fill: t.bg_surface(), radius: radius::SM - stroke::DEFAULT });
        let seg_w = (r.w() - inset * 2.0 - inset * (n as f32 - 1.0)) / n as f32;
        for (i, label) in self.labels.iter().enumerate() {
            let s = Rect::new(r.x() + inset + i as f32 * (seg_w + inset), r.y() + inset, seg_w, r.h() - inset * 2.0);
            let on = i == chosen;
            let edge = ctx.tree.insert(Some(track), Node::Rect { rect: s, fill: if on { t.border_default() } else { Color::TRANSPARENT }, radius: radius::SM - 1.0 });
            let hovered = ctx.is_hovered(edge) && !on;
            let body = if on { t.bg_elevated() } else if hovered { t.state_hover() } else { Color::TRANSPARENT };
            ctx.tree.insert(Some(edge), Node::Rect { rect: inflate(s, -stroke::DEFAULT), fill: body, radius: radius::SM - 2.0 });
            let color = if on { t.text_primary() } else { t.text_secondary() };
            let ls = Node::measure_text(label, &label_style(color, type_size::SM));
            ctx.tree.insert(Some(edge), Node::Text {
                rect: Rect::new(s.x() + (s.w() - ls.w) / 2.0, s.y() + (s.h() - ls.h) / 2.0, ls.w, ls.h),
                content: label.clone(),
                style: label_style(color, type_size::SM),
            });
            ctx.on_click(edge, pick(&self.value, i, &self.on_change));
        }
        if ctx.is_focus_visible(track) {
            light_ring(ctx, ring);
        }
        group_keys(ctx, track, self.value.clone(), self.labels.len(), self.on_change.clone());
    }
}

/// Tabs in a panel: labels `TAB_GAP` apart, a `TAB_BAR` accent bar under
/// the chosen one, and a hairline under the row.
pub struct Tabs {
    pub value: Mutable<usize>,
    pub labels: Vec<String>,
    pub rect: Rect,
    on_change: Option<Changed<usize>>,
}

impl Tabs {
    pub fn new(value: Mutable<usize>, labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let labels: Vec<String> = labels.into_iter().map(Into::into).collect();
        Self { value, labels, rect: Rect::new(0.0, 0.0, 0.0, size::button_height()), on_change: None }
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

impl View for Tabs {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let r = self.rect;
        let chosen = self.value.get();
        // Rings first, so the tabs are above them for hits.
        let widths: Vec<f32> = self.labels.iter().map(|l| Node::measure_text(l, &label_style(t.text_primary(), type_size::MD)).w).collect();
        let mut rings = Vec::new();
        let mut x = r.x();
        for w in &widths {
            rings.push(focus_ring(ctx, inflate(Rect::new(x, r.y(), *w, r.h()), 2.0), radius::XS));
            x += w + TAB_GAP;
        }
        let row = ctx.add(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 0.0 });
        // The hairline under the row.
        ctx.tree.insert(Some(row), Node::Rect { rect: Rect::new(r.x(), r.y() + r.h() - stroke::DEFAULT, r.w(), stroke::DEFAULT), fill: t.border_default(), radius: 0.0 });
        let visible = ctx.is_focus_visible(row);
        let mut x = r.x();
        for (i, label) in self.labels.iter().enumerate() {
            let on = i == chosen;
            let color = if on { t.text_primary() } else { t.text_secondary() };
            let ls = Node::measure_text(label, &label_style(color, type_size::MD));
            let tab = Rect::new(x, r.y(), ls.w, r.h());
            let ring = rings[i];
            let hit = ctx.tree.insert(Some(row), Node::Rect { rect: tab, fill: Color::TRANSPARENT, radius: 0.0 });
            let hovered = ctx.is_hovered(hit) && !on;
            ctx.tree.insert(Some(hit), Node::Text {
                rect: Rect::new(x, r.y() + (r.h() - ls.h) / 2.0, ls.w, ls.h),
                content: label.clone(),
                style: label_style(if hovered { t.text_primary() } else { color }, type_size::MD),
            });
            ctx.tree.insert(Some(hit), Node::Rect {
                rect: Rect::new(x, r.y() + r.h() - TAB_BAR, ls.w, TAB_BAR),
                fill: if on { t.accent_mark() } else { Color::TRANSPARENT },
                radius: TAB_BAR / 2.0,
            });
            if visible && on {
                light_ring(ctx, ring);
            }
            ctx.on_click(hit, pick(&self.value, i, &self.on_change));
            x += ls.w + TAB_GAP;
        }
        group_keys(ctx, row, self.value.clone(), self.labels.len(), self.on_change.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::Modifiers;
    use crate::geom::Point;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn dirty() -> Arc<AtomicBool> { Arc::new(AtomicBool::new(true)) }

    fn click(app: &mut App<impl View>, x: f32, y: f32) {
        app.handle_event(Event::PointerDown { at: Point::new(x, y) });
        app.handle_event(Event::PointerUp { at: Point::new(x, y) });
        app.tick();
    }

    fn key(app: &mut App<impl View>, k: Key, chars: &str) {
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: chars.into() });
        app.tick();
    }

    struct One<V: View>(V);
    impl<V: View> View for One<V> {
        fn render(&self, ctx: &mut Ctx) { self.0.render(ctx) }
    }

    #[test]
    fn a_checkbox_flips_on_a_click_on_its_label_and_on_space() {
        let d = dirty();
        let v = Mutable::with_dirty(Check::Off, d.clone());
        let mut app = App::new_with_flag(One(Checkbox::new(v.clone(), "Remember me").at(10.0, 10.0)), d);
        app.tick();
        click(&mut app, 40.0, 18.0); // on the label, not the box
        assert_eq!(v.get(), Check::On);
        key(&mut app, Key::Char, " ");
        assert_eq!(v.get(), Check::Off);
        v.set(Check::Mixed);
        app.tick();
        click(&mut app, 14.0, 18.0);
        assert_eq!(v.get(), Check::On, "a click on mixed checks it");
    }

    #[test]
    fn a_disabled_checkbox_does_nothing() {
        let d = dirty();
        let v = Mutable::with_dirty(Check::Off, d.clone());
        let mut app = App::new_with_flag(One(Checkbox::new(v.clone(), "Off").at(10.0, 10.0).enabled(false)), d);
        app.tick();
        click(&mut app, 14.0, 18.0);
        assert_eq!(v.get(), Check::Off);
        assert_eq!(app.focused(), None, "and takes no focus");
    }

    #[test]
    fn a_toggle_flips_and_reports() {
        let d = dirty();
        let v = Mutable::with_dirty(false, d.clone());
        let seen = Rc::new(std::cell::Cell::new(None));
        let s = seen.clone();
        let mut app = App::new_with_flag(One(Toggle::new(v.clone(), "Wi-Fi").at(0.0, 0.0).on_change(move |b| s.set(Some(b)))), d);
        app.tick();
        click(&mut app, 10.0, 9.0);
        assert!(v.get());
        assert_eq!(seen.get(), Some(true));
    }

    #[test]
    fn radios_pick_by_click_and_arrows_wrap() {
        let d = dirty();
        let v = Mutable::with_dirty(0usize, d.clone());
        let mut app = App::new_with_flag(One(RadioGroup::new(v.clone(), ["Daily", "Weekly", "Never"]).at(0.0, 0.0)), d);
        app.tick();
        let row = em(BOX).max(Node::measure_text("Mg", &label_style(Color::TRANSPARENT, type_size::MD)).h) + RADIO_ROW_GAP;
        click(&mut app, 8.0, row * 2.0 + 8.0);
        assert_eq!(v.get(), 2);
        // The group is one Tab stop; arrows move the choice and wrap.
        key(&mut app, Key::Tab, "");
        key(&mut app, Key::Down, "");
        assert_eq!(v.get(), 0);
        key(&mut app, Key::Up, "");
        assert_eq!(v.get(), 2);
    }

    #[test]
    fn segments_and_tabs_pick_by_click_and_arrow() {
        let d = dirty();
        let v = Mutable::with_dirty(0usize, d.clone());
        let mut app = App::new_with_flag(One(Segmented::new(v.clone(), ["Day", "Week", "Month"]).at(0.0, 0.0).width(300.0)), d);
        app.tick();
        click(&mut app, 250.0, 16.0);
        assert_eq!(v.get(), 2);
        key(&mut app, Key::Tab, "");
        key(&mut app, Key::Right, "");
        assert_eq!(v.get(), 0);

        let d = dirty();
        let v = Mutable::with_dirty(0usize, d.clone());
        let mut app = App::new_with_flag(One(Tabs::new(v.clone(), ["General", "Privacy"]).at(0.0, 0.0).width(400.0)), d);
        app.tick();
        let w = Node::measure_text("General", &label_style(Color::TRANSPARENT, type_size::MD)).w;
        click(&mut app, w + TAB_GAP + 4.0, 16.0);
        assert_eq!(v.get(), 1);
    }

    #[test]
    fn a_state_change_keeps_the_node_count() {
        let d = dirty();
        let v = Mutable::with_dirty(Check::Off, d.clone());
        let mut app = App::new_with_flag(One(Checkbox::new(v.clone(), "x").at(0.0, 0.0)), d);
        let before = app.tick().len();
        v.set(Check::On);
        let deltas = app.tick();
        assert!(deltas.iter().all(|d| !matches!(d, crate::node::NodeDelta::Added { .. } | crate::node::NodeDelta::Removed { .. })), "only payloads change");
        assert!(before > 0);
    }
}
