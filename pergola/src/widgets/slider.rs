//! `Slider` (visual language rev. 2 §14.2): a 4px pill track in
//! `border-strong`, its filled part `accent-mark`, a 16px `bg-elevated`
//! thumb with a `border-control` edge. Press anywhere on it and drag; the
//! thumb follows the pointer even off the track. Arrows step, Page Up and
//! Down take ten steps, Home and End go to the ends (§14.3).

use std::rc::Rc;

use crate::event::{Event, Key};
use crate::geom::{Point, Rect};
use crate::interaction::DragPhase;
use crate::node::Node;
use crate::reactive::Mutable;
use crate::theme::stroke;
use crate::view::{Ctx, View};
use crate::Color;

pub const TRACK_H: f32 = 4.0;
pub const THUMB: f32 = 16.0;

/// A size above at the system text size (resolution-independence.md §6.2).
fn em(pt: f32) -> f32 { crate::theme::size::em(pt) }

pub struct Slider {
    /// The value, in `min..=max`.
    pub value: Mutable<f64>,
    pub min: f64,
    pub max: f64,
    /// Snap to multiples of this from `min`; 0 is continuous.
    pub step: f64,
    pub rect: Rect,
    pub enabled: bool,
    on_change: Option<Rc<dyn Fn(f64)>>,
}

impl Slider {
    pub fn new(value: Mutable<f64>, min: f64, max: f64) -> Self {
        Self { value, min, max, step: 0.0, rect: Rect::new(0.0, 0.0, 180.0, em(THUMB)), enabled: true, on_change: None }
    }
    pub fn step(mut self, s: f64) -> Self {
        self.step = s.max(0.0);
        self
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
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn on_change(mut self, f: impl Fn(f64) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

/// Clamp and snap `v` to the slider's range and step.
pub fn snap(v: f64, min: f64, max: f64, step: f64) -> f64 {
    let (lo, hi) = (min.min(max), min.max(max));
    let v = v.clamp(lo, hi);
    if step > 0.0 {
        (lo + ((v - lo) / step).round() * step).clamp(lo, hi)
    } else {
        v
    }
}

impl View for Slider {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let r = Rect::new(self.rect.x(), self.rect.y(), self.rect.w(), em(THUMB));
        let span = (self.max - self.min).abs().max(f64::EPSILON);
        let frac = ((self.value.get() - self.min.min(self.max)) / span).clamp(0.0, 1.0) as f32;
        // The thumb's centre travels THUMB/2 in from each end.
        let travel = (r.w() - em(THUMB)).max(0.0);
        let cx = r.x() + em(THUMB) / 2.0 + frac * travel;
        let ty = r.y() + (em(THUMB) - em(TRACK_H)) / 2.0;

        let thumb = Rect::new(cx - em(THUMB) / 2.0, r.y(), em(THUMB), em(THUMB));
        // Focus ring around the thumb (§14.1), under everything.
        let halo = ctx.add(Node::Rect { rect: grow(thumb, 3.0), fill: Color::TRANSPARENT, radius: em(THUMB) / 2.0 + 3.0 });
        let ring = ctx.add(Node::Rect { rect: grow(thumb, 1.0), fill: Color::TRANSPARENT, radius: em(THUMB) / 2.0 + 1.0 });
        let target = ctx.add(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 0.0 });
        let (track, filled, edge) = if self.enabled {
            (t.border_strong(), t.accent_mark(), t.border_control())
        } else {
            (t.border_default(), t.border_strong(), t.border_default())
        };
        ctx.tree.insert(Some(target), Node::Rect { rect: Rect::new(r.x(), ty, r.w(), em(TRACK_H)), fill: track, radius: em(TRACK_H) / 2.0 });
        ctx.tree.insert(Some(target), Node::Rect { rect: Rect::new(r.x(), ty, cx - r.x(), em(TRACK_H)), fill: filled, radius: em(TRACK_H) / 2.0 });
        let e = ctx.tree.insert(Some(target), Node::Rect { rect: thumb, fill: edge, radius: em(THUMB) / 2.0 });
        let body = if ctx.is_pressed(target) && self.enabled {
            t.state_pressed().over(t.bg_elevated())
        } else if ctx.is_hovered(target) && self.enabled {
            t.state_hover().over(t.bg_elevated())
        } else {
            t.bg_elevated()
        };
        ctx.tree.insert(Some(e), Node::Rect { rect: grow(thumb, -stroke::DEFAULT), fill: body, radius: em(THUMB) / 2.0 - stroke::DEFAULT });
        if ctx.is_focus_visible(target) {
            for (id, c) in [(halo, t.accent_bg()), (ring, t.focus_ring())] {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                    *fill = c;
                }
            }
        }
        if !self.enabled {
            return;
        }
        let (min, max, step) = (self.min, self.max, self.step);
        let set = {
            let value = self.value.clone();
            let cb = self.on_change.clone();
            move |v: f64| {
                let v = snap(v, min, max, step);
                if v != value.get() {
                    value.set(v);
                    if let Some(f) = &cb {
                        f(v);
                    }
                }
            }
        };
        let s = set.clone();
        let (x0, w) = (r.x() + em(THUMB) / 2.0, travel);
        ctx.on_drag(target, move |_: DragPhase, p: Point| {
            let f = if w > 0.0 { ((p.x - x0) / w).clamp(0.0, 1.0) } else { 0.0 } as f64;
            s(min + f * (max - min));
        });
        // A press has to take focus too (focus follows the nearest
        // focusable), and the click handler marks the press for its look.
        ctx.on_click(target, || {});
        ctx.focusable(target);
        let value = self.value.clone();
        let unit = if step > 0.0 { step } else { span / 100.0 };
        ctx.on_key(target, move |ev| {
            let Event::Key { kind, key, .. } = ev else { return };
            if !kind.pressed() {
                return;
            }
            let v = value.get();
            match key {
                Key::Left | Key::Down => set(v - unit),
                Key::Right | Key::Up => set(v + unit),
                Key::PageDown => set(v - unit * 10.0),
                Key::PageUp => set(v + unit * 10.0),
                Key::Home => set(min),
                Key::End => set(max),
                _ => {}
            }
        });
    }
}

fn grow(r: Rect, d: f32) -> Rect {
    Rect::new(r.x() - d, r.y() - d, r.w() + 2.0 * d, r.h() + 2.0 * d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::Modifiers;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    struct One(Slider);
    impl View for One {
        fn render(&self, ctx: &mut Ctx) { self.0.render(ctx) }
    }

    #[test]
    fn snapping_clamps_and_rounds_to_the_step() {
        assert_eq!(snap(0.37, 0.0, 1.0, 0.25), 0.25);
        assert_eq!(snap(0.38, 0.0, 1.0, 0.25), 0.5);
        assert_eq!(snap(7.0, 0.0, 1.0, 0.0), 1.0);
        assert_eq!(snap(-3.0, 0.0, 10.0, 2.0), 0.0);
    }

    #[test]
    fn a_drag_follows_the_pointer_even_off_the_track() {
        let d = Arc::new(AtomicBool::new(true));
        let v = Mutable::with_dirty(0.0f64, d.clone());
        // 0..100 over a 216px slider: 200px of travel from x = 8.
        let mut app = App::new_with_flag(One(Slider::new(v.clone(), 0.0, 100.0).width(216.0)), d);
        app.tick();
        app.handle_event(Event::PointerDown { at: Point::new(108.0, 8.0) });
        assert!((v.get() - 50.0).abs() < 1e-6, "{}", v.get());
        app.tick();
        app.handle_event(Event::PointerMove { at: Point::new(158.0, 80.0) }); // below the track
        assert!((v.get() - 75.0).abs() < 1e-6);
        app.handle_event(Event::PointerMove { at: Point::new(900.0, 8.0) });
        assert_eq!(v.get(), 100.0, "clamped at the end");
        app.handle_event(Event::PointerUp { at: Point::new(900.0, 8.0) });
        app.handle_event(Event::PointerMove { at: Point::new(8.0, 8.0) });
        assert_eq!(v.get(), 100.0, "released: moves no longer drag");
    }

    #[test]
    fn keys_step_and_jump() {
        let d = Arc::new(AtomicBool::new(true));
        let v = Mutable::with_dirty(50.0f64, d.clone());
        let mut app = App::new_with_flag(One(Slider::new(v.clone(), 0.0, 100.0).step(5.0)), d);
        app.tick();
        let key = |app: &mut App<One>, k| {
            app.handle_event(Event::Key { kind: crate::event::KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: String::new() });
            app.tick();
        };
        key(&mut app, Key::Tab);
        key(&mut app, Key::Right);
        assert_eq!(v.get(), 55.0);
        key(&mut app, Key::PageDown);
        assert_eq!(v.get(), 5.0);
        key(&mut app, Key::End);
        assert_eq!(v.get(), 100.0);
        key(&mut app, Key::Home);
        assert_eq!(v.get(), 0.0);
    }
}
