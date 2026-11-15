//! Declarative animation (pergola.md §4.2): a client names groups of its
//! window and how one property of each moves (transform, opacity, clip),
//! and frescod moves them on its own clock, frame by frame, so a busy or
//! stopped client does not stall the motion (officina.md §9.3: slide
//! transitions and builds while the app decodes the slide after next).
//!
//! The state lives in the window's [`WindowSceneState`]; [`tick`] runs
//! before each frame is packed and leaves the groups' values for that
//! frame in `anim_now`, which the packer reads in place of the groups' own
//! params. A track that has ended leaves no trace: its group's own params
//! (which the client set to the end state when it started the animation)
//! apply again.
//!
//! [`tick`]: WindowSceneState::tick_animations

use std::collections::HashMap;

use fresco_protocol::{anim_ease, AnimTrack, AnimValue, AnimationFinishedEvent, GroupParams, ANIM_MAX_LIVE, ANIM_MAX_TRACKS};

use super::envelope_frontend::WindowSceneState;

/// One running animation.
#[derive(Debug, Clone)]
pub struct Anim {
    pub handle: u32,
    /// The client that started it.
    pub writer: u8,
    pub tracks: Vec<AnimTrack>,
    /// Monotonic ns of the first frame it was drawn in.
    pub start_ns: Option<u64>,
    pub frames: u32,
    pub last_ns: Option<u64>,
    pub worst_gap_ns: u64,
    /// A track was taken from it (a group gone, or another animation took
    /// the property over).
    pub lost: bool,
}

/// What a track moves, for "one track per group and property".
fn kind(v: &AnimValue) -> u8 {
    match v {
        AnimValue::Transform { .. } => 0,
        AnimValue::Opacity { .. } => 1,
        AnimValue::Clip { .. } => 2,
    }
}

fn lerp(a: f32, b: f32, t: f64) -> f32 {
    (a as f64 + (b as f64 - a as f64) * t) as f32
}

/// Track `tr`'s value at `t` (eased progress 0..=1) applied to `g`.
fn apply(g: &mut GroupParams, v: &AnimValue, t: f64) {
    match *v {
        AnimValue::Transform { from, to } => {
            for i in 0..6 {
                g.transform[i] = lerp(from[i], to[i], t);
            }
        }
        AnimValue::Opacity { from, to } => g.opacity = lerp(from, to, t).clamp(0.0, 1.0),
        AnimValue::Clip { from, to } => {
            let mut c = [0.0; 4];
            for i in 0..4 {
                c[i] = lerp(from[i], to[i], t);
            }
            g.clip = Some(c);
        }
    }
}

fn finished(window_id: u32, a: &Anim, cancelled: bool) -> AnimationFinishedEvent {
    AnimationFinishedEvent {
        window_id,
        handle: a.handle,
        cancelled,
        refused: false,
        frames: a.frames,
        worst_gap_us: (a.worst_gap_ns / 1000).min(u32::MAX as u64) as u32,
    }
}

impl WindowSceneState {
    /// Start animation `handle` of `writer` on window `window_id`. A
    /// refusal is reported as an already-finished animation in
    /// `anim_done`. An animation already running under the same handle is
    /// cancelled first.
    pub fn start_animation(&mut self, window_id: u32, writer: u8, handle: u32, tracks: Vec<AnimTrack>) {
        self.cancel_animation(window_id, writer, handle);
        let refuse = |s: &mut Self, why: &str| {
            log::debug!("window {window_id}: animation {handle} refused: {why}");
            s.anim_done.push(AnimationFinishedEvent { window_id, handle, cancelled: false, refused: true, frames: 0, worst_gap_us: 0 });
        };
        if tracks.is_empty() || tracks.len() > ANIM_MAX_TRACKS {
            return refuse(self, "no tracks, or too many");
        }
        if self.anims.len() >= ANIM_MAX_LIVE {
            return refuse(self, "too many running");
        }
        for t in &tracks {
            if let Err(e) = t.validate() {
                return refuse(self, e);
            }
            if !self.group_nodes.contains_key(&t.node) || self.node_writers.get(&t.node) != Some(&writer) {
                return refuse(self, "a track's node is not a group of this client");
            }
        }
        // A property already moving passes to the new animation.
        for t in &tracks {
            for a in self.anims.iter_mut() {
                let before = a.tracks.len();
                a.tracks.retain(|o| !(o.node == t.node && kind(&o.value) == kind(&t.value)));
                a.lost |= a.tracks.len() != before;
            }
        }
        let mut gone = Vec::new();
        self.anims.retain(|a| {
            if a.tracks.is_empty() {
                gone.push(finished(window_id, a, true));
                false
            } else {
                true
            }
        });
        self.anim_done.extend(gone);
        self.anims.push(Anim { handle, writer, tracks, start_ns: None, frames: 0, last_ns: None, worst_gap_ns: 0, lost: false });
    }

    /// Stop `handle` where it is; its groups show their own params.
    pub fn cancel_animation(&mut self, window_id: u32, writer: u8, handle: u32) {
        if let Some(i) = self.anims.iter().position(|a| a.handle == handle && a.writer == writer) {
            let a = self.anims.remove(i);
            self.anim_done.push(finished(window_id, &a, true));
            self.anim_now.clear();
        }
    }

    /// Whether any animation runs: the frame loop draws again next frame.
    pub fn animating(&self) -> bool {
        !self.anims.is_empty()
    }

    /// The values of the animated groups at `now_ns` (monotonic), into
    /// `anim_now`; animations that ended go to `anim_done`. Returns
    /// whether any still runs.
    pub fn tick_animations(&mut self, window_id: u32, now_ns: u64) -> bool {
        self.anim_now.clear();
        if self.anims.is_empty() {
            return false;
        }
        let mut now: HashMap<u32, GroupParams> = HashMap::new();
        let groups = &self.group_nodes;
        let mut done = Vec::new();
        self.anims.retain_mut(|a| {
            let start = *a.start_ns.get_or_insert(now_ns);
            if let Some(last) = a.last_ns {
                a.worst_gap_ns = a.worst_gap_ns.max(now_ns.saturating_sub(last));
            }
            a.last_ns = Some(now_ns);
            a.frames += 1;
            let elapsed_ms = now_ns.saturating_sub(start) as f64 / 1e6;
            a.tracks.retain(|t| {
                let Some(own) = groups.get(&t.node) else {
                    a.lost = true;
                    return false; // the group went
                };
                let local = elapsed_ms - t.delay_ms as f64;
                if local >= t.duration_ms as f64 {
                    return false;
                }
                if local < 0.0 && !t.hold_before {
                    return true;
                }
                let p = if local < 0.0 { 0.0 } else { anim_ease(t.curve, local / t.duration_ms.max(1) as f64) };
                let g = now.entry(t.node).or_insert_with(|| own.clone());
                apply(g, &t.value, p);
                true
            });
            if a.tracks.is_empty() {
                done.push(finished(window_id, a, a.lost));
                false
            } else {
                true
            }
        });
        self.anim_done.extend(done);
        self.anim_now = now;
        !self.anims.is_empty()
    }

    /// Group `id`'s params as this frame draws them: an animation's
    /// values while one moves it, else its own.
    pub fn group_now(&self, id: u32) -> Option<&GroupParams> {
        self.anim_now.get(&id).or_else(|| self.group_nodes.get(&id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresco_protocol::anim_curve;

    const ID: [f32; 6] = GroupParams::IDENTITY;

    fn state() -> WindowSceneState {
        let mut s = WindowSceneState::default();
        for id in [10, 11] {
            s.set_group(id, GroupParams { parent: None, order: id, transform: ID, clip: None, opacity: 1.0, flags: 0 }, 1).unwrap();
        }
        s
    }

    fn opacity(node: u32, from: f32, to: f32, delay_ms: u32, duration_ms: u32) -> AnimTrack {
        AnimTrack { node, delay_ms, duration_ms, curve: anim_curve::LINEAR, hold_before: true, value: AnimValue::Opacity { from, to } }
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn a_fade_runs_on_the_frame_clock_and_ends_on_the_groups_own() {
        let mut s = state();
        s.start_animation(5, 1, 7, vec![opacity(10, 0.0, 1.0, 0, 100)]);
        assert!(s.tick_animations(5, 1000 * MS), "starts on its first frame");
        assert_eq!(s.group_now(10).unwrap().opacity, 0.0);
        assert!(s.tick_animations(5, 1050 * MS));
        assert!((s.group_now(10).unwrap().opacity - 0.5).abs() < 1e-6);
        assert_eq!(s.group_now(11).unwrap().opacity, 1.0, "another group untouched");
        assert!(!s.tick_animations(5, 1100 * MS), "over at its duration");
        assert_eq!(s.group_now(10).unwrap().opacity, 1.0, "the group's own again");
        let ev = s.anim_done.pop().unwrap();
        assert_eq!((ev.handle, ev.cancelled, ev.refused, ev.frames), (7, false, false, 3));
        assert_eq!(ev.worst_gap_us, 50_000);
    }

    #[test]
    fn delays_hold_or_show_the_groups_own() {
        let mut s = state();
        let mut held = opacity(10, 0.0, 1.0, 50, 100);
        let mut free = opacity(11, 0.25, 1.0, 50, 100);
        free.hold_before = false;
        held.curve = anim_curve::EASE_IN_OUT;
        s.start_animation(5, 1, 1, vec![held, free]);
        s.tick_animations(5, 0);
        assert_eq!(s.group_now(10).unwrap().opacity, 0.0);
        assert_eq!(s.group_now(11).unwrap().opacity, 1.0);
        s.tick_animations(5, 100 * MS);
        assert!((s.group_now(10).unwrap().opacity - 0.5).abs() < 1e-6, "eased midpoint");
        assert!((s.group_now(11).unwrap().opacity - 0.625).abs() < 1e-6);
    }

    #[test]
    fn transforms_and_clips_interpolate() {
        let mut s = state();
        let tr = AnimTrack { node: 10, delay_ms: 0, duration_ms: 200, curve: anim_curve::LINEAR, hold_before: true, value: AnimValue::Transform { from: [1.0, 0.0, 0.0, 1.0, 1920.0, 0.0], to: ID } };
        let cl = AnimTrack { node: 11, delay_ms: 0, duration_ms: 200, curve: anim_curve::LINEAR, hold_before: true, value: AnimValue::Clip { from: [0.0, 0.0, 0.0, 1080.0], to: [0.0, 0.0, 1920.0, 1080.0] } };
        s.start_animation(5, 1, 1, vec![tr, cl]);
        s.tick_animations(5, 0);
        s.tick_animations(5, 50 * MS);
        assert_eq!(s.group_now(10).unwrap().transform[4], 1440.0);
        assert_eq!(s.group_now(11).unwrap().clip, Some([0.0, 0.0, 480.0, 1080.0]));
    }

    #[test]
    fn refusals_replacements_and_cancels() {
        let mut s = state();
        // Not a group; another client's group; a bad value.
        s.start_animation(5, 1, 1, vec![opacity(99, 0.0, 1.0, 0, 10)]);
        s.start_animation(5, 2, 2, vec![opacity(10, 0.0, 1.0, 0, 10)]);
        s.start_animation(5, 1, 3, vec![opacity(10, 0.0, 2.0, 0, 10)]);
        s.start_animation(5, 1, 4, vec![]);
        assert!(s.anim_done.iter().all(|e| e.refused) && s.anim_done.len() == 4);
        assert!(!s.animating());
        s.anim_done.clear();
        // A second animation of the same property takes it over.
        s.start_animation(5, 1, 5, vec![opacity(10, 0.0, 1.0, 0, 100)]);
        s.start_animation(5, 1, 6, vec![opacity(10, 1.0, 0.0, 0, 100)]);
        assert_eq!(s.anim_done.len(), 1);
        assert!(s.anim_done[0].cancelled && s.anim_done[0].handle == 5);
        s.tick_animations(5, 0);
        assert_eq!(s.group_now(10).unwrap().opacity, 1.0);
        s.cancel_animation(5, 1, 6);
        assert!(!s.animating());
        assert_eq!(s.group_now(10).unwrap().opacity, 1.0);
        assert!(s.anim_done.last().unwrap().cancelled);
        // A group cleared mid-way ends its animation, cancelled.
        s.anim_done.clear();
        s.start_animation(5, 1, 8, vec![opacity(11, 0.0, 1.0, 0, 100)]);
        s.tick_animations(5, 0);
        s.group_nodes.remove(&11);
        assert!(!s.tick_animations(5, 10 * MS));
        assert!(s.anim_done[0].cancelled);
    }
}
