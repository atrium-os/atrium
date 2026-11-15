//! Declarative animation (pergola.md §4.2): a Stack's place, scale,
//! opacity or clip moved by frescod on its own clock, frame by frame, so
//! the app can be busy (or stopped) without the motion stalling. A slide
//! transition or a build (officina.md §9.1) is a few tracks.
//!
//! Values are in points, as layout is: a track's `Offset` moves the Stack
//! from where layout put it, and scales it about its own top-left corner;
//! a `Clip` is in the coordinates of the Stack's parent. Queue an
//! animation with [`crate::window::Window::animate`]: it goes out with the
//! next frame, so it starts with what that frame shows. Set the Stack to
//! its end state in that same frame (frescod shows the Stack's own values
//! again once a track ends). `Event::AnimationFinished` (raw
//! `fresco_client::Event`) says when it is over.

use crate::geom::Rect;
use crate::node::NodeId;

pub use fresco_protocol::anim_curve as curve;

/// Where a Stack is drawn relative to its laid-out place: moved by
/// `dx`, `dy` points and scaled by `scale` about its top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Offset {
    pub dx: f32,
    pub dy: f32,
    pub scale: f32,
}

impl Offset {
    /// Where layout put it.
    pub const NONE: Offset = Offset { dx: 0.0, dy: 0.0, scale: 1.0 };

    pub fn by(dx: f32, dy: f32) -> Offset {
        Offset { dx, dy, scale: 1.0 }
    }

    /// Scaled by `s` about the point (`cx`, `cy`) of the Stack's own box.
    pub fn scaled_about(s: f32, cx: f32, cy: f32) -> Offset {
        Offset { dx: cx * (1.0 - s), dy: cy * (1.0 - s), scale: s }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Offset { from: Offset, to: Offset },
    Opacity { from: f32, to: f32 },
    /// In the parent's coordinates, points.
    Clip { from: Rect, to: Rect },
}

/// One property of one Stack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Track {
    pub node: NodeId,
    pub delay_ms: u32,
    pub duration_ms: u32,
    /// [`curve`].
    pub curve: u8,
    /// The `from` value shows during the delay too.
    pub hold_before: bool,
    pub value: Value,
}

impl Track {
    pub fn new(node: NodeId, duration_ms: u32, value: Value) -> Track {
        Track { node, delay_ms: 0, duration_ms, curve: curve::EASE_IN_OUT, hold_before: true, value }
    }
    pub fn delay(mut self, ms: u32) -> Track {
        self.delay_ms = ms;
        self
    }
    pub fn curve(mut self, c: u8) -> Track {
        self.curve = c;
        self
    }
}

/// The wire tracks for `tracks` at scale `k` (device px per point), each
/// Stack's laid-out origin in device px from `origin` (`None`: not sent
/// yet; the track is left out).
pub fn wire(tracks: &[Track], k: f32, origin: impl Fn(u32) -> Option<(f32, f32)>) -> Vec<fresco_protocol::AnimTrack> {
    use fresco_protocol::{AnimTrack, AnimValue};
    let xf = |o: Offset, (x, y): (f32, f32)| [o.scale, 0.0, 0.0, o.scale, (x + o.dx * k).round(), (y + o.dy * k).round()];
    let clip = |r: Rect| [r.x() * k, r.y() * k, (r.x() + r.w()) * k, (r.y() + r.h()) * k];
    tracks
        .iter()
        .filter_map(|t| {
            let value = match t.value {
                Value::Offset { from, to } => {
                    let at = origin(t.node.0)?;
                    AnimValue::Transform { from: xf(from, at), to: xf(to, at) }
                }
                Value::Opacity { from, to } => AnimValue::Opacity { from: from.clamp(0.0, 1.0), to: to.clamp(0.0, 1.0) },
                Value::Clip { from, to } => AnimValue::Clip { from: clip(from), to: clip(to) },
            };
            Some(AnimTrack { node: t.node.0, delay_ms: t.delay_ms, duration_ms: t.duration_ms, curve: t.curve, hold_before: t.hold_before, value })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_become_device_pixels_from_the_stacks_place() {
        let t = [
            Track::new(NodeId(4), 300, Value::Offset { from: Offset::by(100.0, 0.0), to: Offset::NONE }),
            Track::new(NodeId(5), 300, Value::Opacity { from: 0.0, to: 2.0 }),
            Track::new(NodeId(6), 300, Value::Clip { from: Rect::new(0.0, 0.0, 0.0, 50.0), to: Rect::new(0.0, 0.0, 80.0, 50.0) }),
            Track::new(NodeId(7), 300, Value::Offset { from: Offset::NONE, to: Offset::by(1.0, 1.0) }),
        ];
        let w = wire(&t, 1.5, |id| (id == 4).then_some((30.0, 60.0)));
        assert_eq!(w.len(), 3, "a Stack not sent yet is left out");
        assert_eq!(w[0].value, fresco_protocol::AnimValue::Transform { from: [1.0, 0.0, 0.0, 1.0, 180.0, 60.0], to: [1.0, 0.0, 0.0, 1.0, 30.0, 60.0] });
        assert_eq!(w[1].value, fresco_protocol::AnimValue::Opacity { from: 0.0, to: 1.0 });
        assert_eq!(w[2].value, fresco_protocol::AnimValue::Clip { from: [0.0, 0.0, 0.0, 75.0], to: [0.0, 0.0, 120.0, 75.0] });
        let z = Offset::scaled_about(0.5, 100.0, 40.0);
        assert_eq!((z.dx, z.dy), (50.0, 20.0));
    }
}
