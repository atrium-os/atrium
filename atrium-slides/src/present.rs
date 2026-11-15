//! Presenting (officina.md §9.1, §16.33 O6d): the slideshow full screen on
//! one monitor and the presenter view (current slide, next, notes, timer,
//! clock, a sorter to jump) on another.
//!
//! Every slide kept resident ([`Show::resident`]: the one shown, the next
//! and the previous) is a layer of its own (a Fresco scene group), and each
//! shape a build moves is a layer inside it. A transition or a build is a
//! set of declarative animation tracks ([`pergola::anim`]) on those layers,
//! run by frescod on its own clock: the app sets every layer to its end
//! state in the frame the tracks start with and the tracks cover the way
//! there, so a busy (or stopped) app never stalls a transition. A slide
//! going out holds at full opacity for the transition (a `STEP` track),
//! then shows its own state, hidden.
//!
//! Video plays in Limen's `media-player` (limen.md §2.2): once a slide with
//! a video has arrived, the player is placed over the video's box and
//! handed the file.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fresco_protocol::DisplayOutput;
use officina_slides::draw::{self, DrawItem, Notes, Part};
use officina_slides::model::{Build, Deck, Dir, Effect, MediaId, ShapeId, Transition, TransitionKind};
use officina_slides::show::{self, Go, Planned, Show};
use officina_text::layout::Fonts;
use pergola::anim::{curve, Offset, Track, Value};
use pergola::embed::{EmbedId, Embeds};
use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::{Node, NodeId};
use pergola::theme::{font, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Button;
use pergola::window::StepResult;
use pergola::{App, Color, SizeSpec, TextStyle, Window, WindowDesc};

use crate::render::{self, Place};

/// The most videos playing on one slide.
pub const MAX_VIDEOS: usize = 2;
/// Video slots in the slideshow window.
const VIDEO_SLOT: u64 = 100;
/// The longest a track runs (the protocol's cap).
const MAX_MS: u32 = fresco_protocol::ANIM_MAX_MS;

// ── Monitors and the stage ─────────────────────────────────────────────

/// Which monitors show the slideshow and the presenter view, as indices
/// into `outs`: the slideshow goes to an external monitor that is not the
/// first (a projector), the presenter view to the first. One monitor: the
/// slideshow only.
pub fn pick(outs: &[DisplayOutput]) -> (Option<usize>, Option<usize>) {
    match outs.len() {
        0 => (None, None),
        1 => (Some(0), None),
        _ => {
            let a = outs
                .iter()
                .position(|o| !o.internal && !o.primary)
                .or_else(|| outs.iter().position(|o| !o.primary))
                .unwrap_or(1);
            let p = outs.iter().enumerate().position(|(i, o)| i != a && o.primary).or_else(|| (0..outs.len()).find(|&i| i != a));
            (Some(a), p)
        }
    }
}

/// The slide fitted into `r`, centred: its box and its [`Place`] (points).
pub fn fit(size: (i64, i64), r: Rect) -> (Rect, Place) {
    let (sw, sh) = (size.0.max(1) as f64, size.1.max(1) as f64);
    let k = (r.w() as f64 / sw).min(r.h() as f64 / sh).max(1e-9);
    let (w, h) = (sw * k, sh * k);
    let x = r.x() as f64 + (r.w() as f64 - w) / 2.0;
    let y = r.y() as f64 + (r.h() as f64 - h) / 2.0;
    (Rect::new(x as f32, y as f32, w as f32, h as f32), Place { k, x, y })
}

// ── Transitions and builds as tracks ───────────────────────────────────

/// Whether the slide going out is drawn over the one coming in.
pub fn old_on_top(kind: TransitionKind) -> bool {
    matches!(kind, TransitionKind::Uncover(_) | TransitionKind::Split { out: false, .. } | TransitionKind::Zoom { out: true })
}

/// A unit step toward `d`.
fn toward(d: Dir) -> (f32, f32) {
    match d {
        Dir::Left => (-1.0, 0.0),
        Dir::Right => (1.0, 0.0),
        Dir::Up => (0.0, -1.0),
        Dir::Down => (0.0, 1.0),
    }
}

fn ms(v: u32) -> u32 {
    v.clamp(1, MAX_MS)
}

/// The tracks of a slide transition from layer `old` to layer `new` over a
/// stage `stage` (the layers' parent coordinates).
pub fn transition_tracks(kind: TransitionKind, dur: u32, old: Option<NodeId>, new: NodeId, stage: Rect) -> Vec<Track> {
    if matches!(kind, TransitionKind::Cut) || dur == 0 {
        return Vec::new();
    }
    let d = ms(dur);
    let mut t = Vec::new();
    // The old slide stays as it was until the new one is all there.
    if let Some(o) = old {
        t.push(Track::new(o, d, Value::Opacity { from: 1.0, to: 0.0 }).curve(curve::STEP));
    }
    let (w, h) = (stage.w(), stage.h());
    let (cx, cy) = (w / 2.0, h / 2.0);
    let by = |dir: Dir, s: f32| {
        let (x, y) = toward(dir);
        Offset::by(x * w * s, y * h * s)
    };
    match kind {
        TransitionKind::Cut => {}
        TransitionKind::Fade => t.push(Track::new(new, d, Value::Opacity { from: 0.0, to: 1.0 })),
        TransitionKind::Dissolve => t.push(Track::new(new, d, Value::Opacity { from: 0.0, to: 1.0 }).curve(curve::LINEAR)),
        TransitionKind::Push(dir) => {
            t.push(Track::new(new, d, Value::Offset { from: by(dir, -1.0), to: Offset::NONE }));
            if let Some(o) = old {
                t.push(Track::new(o, d, Value::Offset { from: Offset::NONE, to: by(dir, 1.0) }));
            }
        }
        TransitionKind::Cover(dir) => t.push(Track::new(new, d, Value::Offset { from: by(dir, -1.0), to: Offset::NONE })),
        TransitionKind::Uncover(dir) => {
            if let Some(o) = old {
                t.push(Track::new(o, d, Value::Offset { from: Offset::NONE, to: by(dir, 1.0) }));
            }
        }
        TransitionKind::Wipe(dir) => {
            let from = match dir {
                Dir::Right => Rect::new(stage.x(), stage.y(), 0.0, h),
                Dir::Left => Rect::new(stage.x() + w, stage.y(), 0.0, h),
                Dir::Down => Rect::new(stage.x(), stage.y(), w, 0.0),
                Dir::Up => Rect::new(stage.x(), stage.y() + h, w, 0.0),
            };
            t.push(Track::new(new, d, Value::Clip { from, to: stage }));
        }
        TransitionKind::Split { vertical, out } => {
            let line = if vertical { Rect::new(stage.x() + cx, stage.y(), 0.0, h) } else { Rect::new(stage.x(), stage.y() + cy, w, 0.0) };
            if out {
                t.push(Track::new(new, d, Value::Clip { from: line, to: stage }));
            } else if let Some(o) = old {
                // In: the old slide closes toward the middle, over the new.
                t.push(Track::new(o, d, Value::Clip { from: stage, to: line }));
            }
        }
        TransitionKind::Zoom { out: false } => {
            t.push(Track::new(new, d, Value::Offset { from: Offset::scaled_about(0.3, cx, cy), to: Offset::NONE }));
            t.push(Track::new(new, d, Value::Opacity { from: 0.0, to: 1.0 }));
        }
        TransitionKind::Zoom { out: true } => {
            if let Some(o) = old {
                // The old slide's hold is replaced by a fade as it grows.
                t.retain(|x| x.node != o);
                t.push(Track::new(o, d, Value::Offset { from: Offset::NONE, to: Offset::scaled_about(1.6, cx, cy) }));
                t.push(Track::new(o, d, Value::Opacity { from: 1.0, to: 0.0 }));
            }
        }
    }
    t
}

/// The tracks of one step's builds, `delay` ms in. `layer` names each moved
/// shape's layer; `bounds` its box on the stage (relative to the stage's
/// corner, points); `stage` is the stage's size.
pub fn build_tracks(builds: &[Build], step: &[Planned], delay: u32, layer: impl Fn(ShapeId) -> Option<NodeId>, bounds: impl Fn(ShapeId) -> Option<Rect>, stage: (f32, f32)) -> Vec<Track> {
    let mut t = Vec::new();
    for p in step {
        let b = &builds[p.build];
        let Some(node) = layer(b.shape) else { continue };
        let at = delay.saturating_add(p.start_ms).min(MAX_MS);
        let d = ms(b.ms);
        let r = bounds(b.shape).unwrap_or(Rect::new(0.0, 0.0, stage.0, stage.1));
        let mut add = |tr: Track| t.push(tr.delay(at));
        match b.effect {
            Effect::Appear => add(Track::new(node, 1, Value::Opacity { from: 0.0, to: 1.0 }).curve(curve::STEP)),
            Effect::Disappear => add(Track::new(node, 1, Value::Opacity { from: 1.0, to: 0.0 }).curve(curve::STEP)),
            Effect::Fade => add(Track::new(node, d, Value::Opacity { from: 0.0, to: 1.0 })),
            Effect::FadeOut => add(Track::new(node, d, Value::Opacity { from: 1.0, to: 0.0 })),
            Effect::FlyIn(from) => {
                // From just past the stage's edge on that side.
                let o = match from {
                    Dir::Left => Offset::by(-(r.x() + r.w()), 0.0),
                    Dir::Right => Offset::by(stage.0 - r.x(), 0.0),
                    Dir::Up => Offset::by(0.0, -(r.y() + r.h())),
                    Dir::Down => Offset::by(0.0, stage.1 - r.y()),
                };
                add(Track::new(node, d, Value::Offset { from: o, to: Offset::NONE }).curve(curve::EASE_OUT));
            }
            Effect::Wipe(from) => {
                let c = match from {
                    Dir::Left => Rect::new(r.x(), r.y(), 0.0, r.h()),
                    Dir::Right => Rect::new(r.x() + r.w(), r.y(), 0.0, r.h()),
                    Dir::Up => Rect::new(r.x(), r.y(), r.w(), 0.0),
                    Dir::Down => Rect::new(r.x(), r.y() + r.h(), r.w(), 0.0),
                };
                add(Track::new(node, d, Value::Clip { from: c, to: r }).curve(curve::LINEAR));
            }
            Effect::Zoom => {
                let (cx, cy) = (r.x() + r.w() / 2.0, r.y() + r.h() / 2.0);
                add(Track::new(node, d, Value::Offset { from: Offset::scaled_about(0.1, cx, cy), to: Offset::NONE }).curve(curve::EASE_OUT));
                add(Track::new(node, d, Value::Opacity { from: 0.0, to: 1.0 }));
            }
        }
    }
    t.truncate(fresco_protocol::ANIM_MAX_TRACKS);
    t
}

/// How long `tracks` run, ms.
pub fn tracks_ms(tracks: &[Track]) -> u32 {
    tracks.iter().map(|t| t.delay_ms.saturating_add(t.duration_ms)).max().unwrap_or(0)
}

// ── Keys ───────────────────────────────────────────────────────────────

/// What a key or button asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Next,
    Prev,
    /// A slide by deck index, hidden or not.
    GoTo(usize),
    First,
    Last,
    /// A digit typed (a slide number, ended by Enter).
    Digit(u8),
    Enter,
    /// The screen black (or white): toggles.
    Black,
    White,
    Exit,
}

/// The action for a key press, by HID usage.
pub fn act_of_key(hid: u16) -> Option<Act> {
    Some(match hid {
        0x4F | 0x51 | 0x2C | 0x4E | 0x11 => Act::Next,
        0x50 | 0x52 | 0x4B | 0x13 | 0x2A => Act::Prev,
        0x28 | 0x58 => Act::Enter,
        0x29 => Act::Exit,
        0x05 | 0x37 => Act::Black,
        0x1A | 0x36 => Act::White,
        0x4A => Act::First,
        0x4D => Act::Last,
        0x1E..=0x26 => Act::Digit((hid - 0x1E + 1) as u8),
        0x27 => Act::Digit(0),
        0x59..=0x61 => Act::Digit((hid - 0x59 + 1) as u8),
        0x62 => Act::Digit(0),
        _ => return None,
    })
}

// ── The show's state, shared by both windows ───────────────────────────

/// One slide, ready to present.
pub struct Ready {
    pub parts: Vec<Part>,
    pub builds: Vec<Build>,
    pub steps: Vec<Vec<Planned>>,
    pub notes: String,
    pub hidden: bool,
    pub transition: Option<Transition>,
}

impl Ready {
    /// The items that show once `done` steps have played.
    fn items_at(&self, done: usize) -> Vec<DrawItem> {
        self.parts
            .iter()
            .filter(|p| p.shape.is_none_or(|s| show::shown(&self.builds, &self.steps, s, done)))
            .flat_map(|p| p.items.iter().cloned())
            .collect()
    }

    /// The videos, in drawing order: (media, transform, w, h).
    fn videos(&self) -> Vec<(MediaId, draw::Xform, f64, f64)> {
        self.parts
            .iter()
            .flat_map(|p| p.items.iter())
            .filter_map(|it| match it {
                DrawItem::Video { xf, w, h, media, .. } => Some((*media, *xf, *w, *h)),
                _ => None,
            })
            .take(MAX_VIDEOS)
            .collect()
    }
}

pub struct Stage {
    pub deck: Deck,
    pub deck_key: u64,
    fonts: Rc<dyn Fonts>,
    pub show: Show,
    ready: HashMap<usize, Rc<Ready>>,
    /// The slides drawn (their layers kept), the last on top.
    pub resident: Vec<usize>,
    /// The slide drawn over the others while a transition runs.
    pub top: Option<usize>,
    /// The slide at full opacity (`None`: none, at the end screen).
    pub visible: Option<usize>,
    /// Steps drawn as played, by slide.
    pub done: HashMap<usize, usize>,
    /// Black or white over everything (B, W).
    pub blank: Option<Color>,
    pub ended: bool,
    /// The layers of the last slideshow paint: (slide, moved shape).
    pub ids: HashMap<(usize, Option<ShapeId>), NodeId>,
    /// The slide whose videos have players on them.
    pub playing: Option<usize>,
    /// The windows' sizes, points, and the slideshow's scale.
    pub audience: (f32, f32),
    pub presenter: (f32, f32),
    pub scale: f64,
    pub started: Instant,
    /// A slide number being typed.
    pub typed: String,
    pub acts: Arc<Mutex<Vec<Act>>>,
}

impl Stage {
    pub fn new(deck: Deck, deck_key: u64, fonts: Rc<dyn Fonts>) -> Stage {
        let show = Show::new(&deck);
        Stage {
            deck,
            deck_key,
            fonts,
            show,
            ready: HashMap::new(),
            resident: Vec::new(),
            top: None,
            visible: None,
            done: HashMap::new(),
            blank: None,
            ended: false,
            ids: HashMap::new(),
            playing: None,
            audience: (1280.0, 720.0),
            presenter: (0.0, 0.0),
            scale: 1.0,
            started: Instant::now(),
            typed: String::new(),
            acts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Slide `i` ready to draw (its display list cut into parts).
    pub fn ready(&mut self, i: usize) -> Rc<Ready> {
        if let Some(r) = self.ready.get(&i) {
            return r.clone();
        }
        let (builds, notes, hidden, transition) = match self.deck.slide(i) {
            Ok(s) => (s.builds.clone(), s.notes.as_ref().map(|n| n.text()).unwrap_or_default(), s.hidden, s.transition),
            Err(_) => (Vec::new(), String::new(), false, None),
        };
        let moved = show::moved(&builds);
        let parts = draw::draw_slide_parts(&mut self.deck, i, &moved, &*self.fonts, &mut Notes::default()).unwrap_or_default();
        let steps = show::steps(&builds);
        let r = Rc::new(Ready { parts, builds, steps, notes, hidden, transition });
        if self.ready.len() > 48 {
            let keep: Vec<usize> = self.wanted();
            self.ready.retain(|k, _| keep.contains(k));
        }
        self.ready.insert(i, r.clone());
        r
    }

    /// The steps slide `i` has.
    pub fn steps_of(&mut self, i: usize) -> usize {
        self.ready(i).steps.len()
    }

    /// The slides the sorter shows: a window of them round the current.
    pub fn sorter(&self) -> std::ops::Range<usize> {
        let n = self.deck.slide_count();
        let fit = (((self.presenter.0 - 32.0) / SORT_ROOM).floor() as usize).max(1);
        let first = self.show.slide.saturating_sub(fit / 2).min(n.saturating_sub(fit));
        first..(first + fit).min(n)
    }

    /// Every slide either window draws now.
    fn wanted(&self) -> Vec<usize> {
        let mut v = self.resident.clone();
        if self.presenter.0 > 0.0 {
            v.extend(self.sorter());
        }
        v
    }

    /// Readies every slide the windows draw.
    pub fn prepare(&mut self) {
        for i in self.wanted() {
            self.ready(i);
        }
        if let Some(n) = self.next_slide() {
            self.ready(n);
        }
    }

    /// What the next press shows: (slide, steps played then), or `None` at
    /// the end.
    pub fn next_view(&mut self) -> Option<(usize, usize)> {
        if self.ended {
            return None;
        }
        let cur = self.show.slide;
        if self.show.done < self.steps_of(cur) {
            return Some((cur, self.show.done + 1));
        }
        self.next_slide().map(|n| (n, 1))
    }

    fn next_slide(&mut self) -> Option<usize> {
        let n = self.deck.slide_count();
        ((self.show.slide + 1)..n).find(|&i| !self.deck.slide(i).map(|s| s.hidden).unwrap_or(false))
    }

    /// The stage on the slideshow window.
    pub fn stage(&self) -> (Rect, Place) {
        fit(self.deck.size, Rect::new(0.0, 0.0, self.audience.0, self.audience.1))
    }

    /// Shape `shape`'s box on slide `i`, relative to the stage's corner.
    fn shape_bounds(&mut self, i: usize, shape: ShapeId) -> Option<Rect> {
        let (stage, place) = self.stage();
        let slide = self.deck.slide(i).ok()?.clone();
        let (m, w, h) = draw::shape_box(&self.deck, officina_slides::resolve::Owner::Slide(slide.layout), &slide.shapes, shape)?;
        let r = render::bounds(&draw::mul(&place.xform(), &m), w, h);
        Some(Rect::new(r.x() - stage.x(), r.y() - stage.y(), r.w(), r.h()))
    }

    /// The tracks for step `step` of slide `i`, `delay` ms in.
    pub fn step_tracks(&mut self, i: usize, step: usize, delay: u32) -> Vec<Track> {
        let r = self.ready(i);
        let Some(planned) = r.steps.get(step) else { return Vec::new() };
        let mut boxes = HashMap::new();
        for p in planned {
            let s = r.builds[p.build].shape;
            if let std::collections::hash_map::Entry::Vacant(e) = boxes.entry(s) {
                e.insert(self.shape_bounds(i, s));
            }
        }
        let (stage, _) = self.stage();
        build_tracks(&r.builds, planned, delay, |s| self.ids.get(&(i, Some(s))).copied(), |s| boxes.get(&s).copied().flatten(), (stage.w(), stage.h()))
    }
}

fn text(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn free(ctx: &mut Ctx, id: NodeId) {
    ctx.set_flex(id, FlexStyle { free: true, ..FlexStyle::default() });
}

const BLACK: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

// ── The slideshow window ───────────────────────────────────────────────

pub struct Audience(pub Rc<RefCell<Stage>>);

impl View for Audience {
    fn render(&self, ctx: &mut Ctx) {
        let mut st = self.0.borrow_mut();
        let (w, h) = st.audience;
        let root = ctx.push(Node::vstack(Rect::new(0.0, 0.0, w, h), 0.0));
        free(ctx, root);
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: BLACK, radius: 0.0 });
        let (stage, place) = st.stage();
        let mut order = st.resident.clone();
        if let Some(t) = st.top.filter(|t| order.contains(t)) {
            order.retain(|&s| s != t);
            order.push(t);
        }
        st.ids.clear();
        for s in order {
            let Some(r) = st.ready.get(&s).cloned() else { continue };
            let on = st.visible == Some(s) && !st.ended;
            ctx.key(1 + s as u64);
            let layer = ctx.push(Node::layer(stage, if on { 1.0 } else { 0.0 }, true));
            free(ctx, layer);
            st.ids.insert((s, None), layer);
            let done = st.done.get(&s).copied().unwrap_or(0);
            for part in &r.parts {
                let mut out = Vec::new();
                render::nodes(&st.deck, st.deck_key, &part.items, place, st.scale, &mut out);
                match part.shape {
                    None => {
                        for n in out {
                            ctx.add(n);
                        }
                    }
                    Some(sh) => {
                        let shown = show::shown(&r.builds, &r.steps, sh, done);
                        ctx.key((1 << 40) + sh as u64);
                        let l = ctx.push(Node::layer(stage, if shown { 1.0 } else { 0.0 }, false));
                        free(ctx, l);
                        for n in out {
                            ctx.add(n);
                        }
                        ctx.pop();
                        st.ids.insert((s, Some(sh)), l);
                    }
                }
            }
            if st.playing == Some(s) {
                for (n, (_, xf, vw, vh)) in r.videos().into_iter().enumerate() {
                    let b = render::bounds(&draw::mul(&place.xform(), &xf), vw, vh);
                    ctx.key((2 << 40) + n as u64);
                    let id = ctx.add(Node::Rect { rect: b, fill: Color::TRANSPARENT, radius: 0.0 });
                    ctx.slot(id, VIDEO_SLOT + n as u64);
                }
            }
            ctx.pop();
        }
        if st.ended {
            ctx.key(3 << 40);
            ctx.add(Node::Text { rect: Rect::new(24.0, 24.0, (w - 48.0).max(0.0), 28.0), content: "End of slide show. Click or press a key to exit.".into(), style: text(18.0, Weight::Regular, Color::rgba(0.7, 0.7, 0.7, 1.0)) });
        }
        if let Some(c) = st.blank {
            ctx.key(4 << 40);
            ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: c, radius: 0.0 });
        }
        ctx.pop();
    }
}

// ── The presenter view ─────────────────────────────────────────────────

/// The sorter's thumbnail width and the room each takes.
const SORT_W: f32 = 120.0;
const SORT_ROOM: f32 = 132.0;

pub struct Presenter(pub Rc<RefCell<Stage>>);

/// `h:mm:ss`.
pub fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The local time of day, `HH:MM`.
fn clock() -> String {
    // SAFETY: time and localtime_r only write the struct handed to them.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// Slide `i` with `done` steps played, fitted into `r` on black: a group
/// of its own, keyed by `role`, the slide and the steps, so a different
/// slide (or step) is a new group and the old one goes whole.
fn thumb(ctx: &mut Ctx, st: &Stage, role: u64, i: usize, done: usize, r: Rect) {
    ctx.key((role << 48) | ((i as u64) << 16) | (done as u64 & 0xFFFF));
    let g = ctx.push(Node::layer(r, 1.0, true));
    free(ctx, g);
    ctx.add(Node::Rect { rect: r, fill: BLACK, radius: 0.0 });
    if let Some(ready) = st.ready.get(&i) {
        let (_, place) = fit(st.deck.size, r);
        let mut out = Vec::new();
        render::nodes(&st.deck, st.deck_key, &ready.items_at(done), place, st.scale, &mut out);
        for n in out {
            ctx.add(n);
        }
    }
    ctx.pop();
}

impl View for Presenter {
    fn render(&self, ctx: &mut Ctx) {
        let mut st = self.0.borrow_mut();
        let (w, h) = st.presenter;
        let bg = Color::rgba(0.07, 0.07, 0.08, 1.0);
        let fg = Color::rgba(0.92, 0.92, 0.93, 1.0);
        let dim = Color::rgba(0.62, 0.63, 0.66, 1.0);
        let accent = Color::rgba(0.24, 0.52, 0.98, 1.0);
        let root = ctx.push(Node::vstack(Rect::new(0.0, 0.0, w, h), 0.0));
        free(ctx, root);
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: bg, radius: 0.0 });
        let pad = 16.0;
        let n = st.deck.slide_count();
        let cur = st.show.slide;
        // Top line: where, how long, what time.
        let at = if st.ended { "End of the show".to_string() } else { format!("Slide {} of {n}", cur + 1) };
        let at = if st.typed.is_empty() { at } else { format!("{at} · go to {}", st.typed) };
        ctx.add(Node::Text { rect: Rect::new(pad, 12.0, w * 0.4, 26.0), content: at, style: text(18.0, Weight::Semibold, fg) });
        ctx.add(Node::Text { rect: Rect::new(w * 0.42, 12.0, 160.0, 26.0), content: elapsed(st.started.elapsed()), style: text(18.0, Weight::Semibold, fg) });
        ctx.add(Node::Text { rect: Rect::new(w - pad - 80.0, 12.0, 80.0, 26.0), content: clock(), style: text(18.0, Weight::Regular, dim) });

        let top = 48.0;
        let sorter_h = SORT_W * 0.5625 + 34.0;
        let buttons_y = h - sorter_h - 52.0;
        let body_h = (buttons_y - top - pad).max(40.0);
        let cw = ((w - 3.0 * pad) * 0.62).max(40.0);
        // The slide as the audience sees it.
        let done = st.show.done;
        let (cur_box, _) = fit(st.deck.size, Rect::new(pad, top, cw, body_h));
        if st.ended {
            ctx.add(Node::Rect { rect: cur_box, fill: BLACK, radius: 0.0 });
        } else {
            thumb(ctx, &st, 1, cur, done, cur_box);
        }
        // Next.
        let rx = 2.0 * pad + cw;
        let rw = (w - rx - pad).max(40.0);
        ctx.add(Node::Text { rect: Rect::new(rx, top, rw, 20.0), content: "Next".into(), style: text(13.0, Weight::Semibold, dim) });
        let nh = rw * st.deck.size.1 as f32 / st.deck.size.0.max(1) as f32;
        let next_box = Rect::new(rx, top + 24.0, rw, nh.min(body_h * 0.5));
        match st.next_view() {
            Some((i, d)) => {
                let (b, _) = fit(st.deck.size, next_box);
                thumb(ctx, &st, 2, i, d, b);
            }
            None => {
                ctx.add(Node::Rect { rect: next_box, fill: BLACK, radius: 0.0 });
                ctx.add(Node::Text { rect: Rect::new(rx + 12.0, next_box.y() + 12.0, rw - 24.0, 20.0), content: "End of the show".into(), style: text(14.0, Weight::Regular, dim) });
            }
        }
        // Notes.
        let ny = next_box.y() + next_box.h() + pad;
        ctx.add(Node::Text { rect: Rect::new(rx, ny, rw, 20.0), content: "Notes".into(), style: text(13.0, Weight::Semibold, dim) });
        let notes = st.ready.get(&cur).map(|r| r.notes.clone()).unwrap_or_default();
        let notes = if notes.trim().is_empty() { "No notes.".to_string() } else { notes };
        let style = text(17.0, Weight::Regular, fg);
        let lh = pergola::text::line_height(&style);
        let bottom = top + body_h;
        let mut y = ny + 24.0;
        'notes: for para in notes.split('\n') {
            for line in pergola::text::lines(para, &style, rw) {
                if y + lh > bottom {
                    break 'notes;
                }
                ctx.add(Node::Text { rect: Rect::new(rx, y, rw, lh), content: line, style: style.clone() });
                y += lh;
            }
            y += 6.0;
        }

        // Buttons.
        let acts = st.acts.clone();
        let mut x = pad;
        for (label, a, bw) in [("Previous", Act::Prev, 104.0), ("Next", Act::Next, 88.0), ("Black Screen", Act::Black, 124.0), ("End Show", Act::Exit, 104.0)] {
            let acts = acts.clone();
            Button::secondary(label).at(x, buttons_y).width(bw).on_click(move || acts.lock().unwrap().push(a)).render(ctx);
            x += bw + 8.0;
        }

        // The sorter: click a slide to show it.
        let sy = h - sorter_h;
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy - 8.0, w, 1.0), fill: Color::rgba(1.0, 1.0, 1.0, 0.12), radius: 0.0 });
        let th = SORT_W * st.deck.size.1 as f32 / st.deck.size.0.max(1) as f32;
        let range = st.sorter();
        for (k, i) in range.enumerate() {
            let x = pad + k as f32 * SORT_ROOM;
            let r = Rect::new(x, sy + 4.0, SORT_W, th);
            ctx.key(1000 + i as u64);
            let border = if i == cur { accent } else { Color::rgba(1.0, 1.0, 1.0, 0.18) };
            let bw = if i == cur { 3.0 } else { 1.0 };
            let hit = ctx.add(Node::Rect { rect: Rect::new(r.x() - bw, r.y() - bw, r.w() + 2.0 * bw, r.h() + 2.0 * bw), fill: border, radius: 2.0 });
            let acts = acts.clone();
            ctx.on_click(hit, move || acts.lock().unwrap().push(Act::GoTo(i)));
            let steps = st.ready.get(&i).map_or(1, |r| r.steps.len());
            thumb(ctx, &st, 3, i, steps, r);
            let hidden = st.ready.get(&i).is_some_and(|r| r.hidden);
            if hidden {
                ctx.add(Node::Rect { rect: r, fill: Color::rgba(0.0, 0.0, 0.0, 0.55), radius: 0.0 });
            }
            let label = if hidden { format!("{} (hidden)", i + 1) } else { (i + 1).to_string() };
            ctx.add(Node::Text { rect: Rect::new(x, sy + th + 8.0, SORT_W, 16.0), content: label, style: text(12.0, Weight::Regular, dim) });
        }
        ctx.pop();
    }
}

// ── Running a show ─────────────────────────────────────────────────────

/// The player on one video.
struct Playing {
    embed: EmbedId,
    _file: File,
}

/// A deck's media as an unlinked read-only temp file, for the player.
fn media_file(deck: &Deck, id: MediaId) -> io::Result<File> {
    let bytes = deck.media_bytes(id).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))?;
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = std::env::temp_dir().join(format!(".atrium-slides-{}-{id}-{n}", std::process::id()));
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?
    };
    let _ = std::fs::remove_file(&path);
    f.write_all(&bytes)?;
    f.seek(SeekFrom::Start(0))?;
    Ok(f)
}

/// The window description for a monitor (`None`: full screen wherever).
fn desc_on(title: &str, o: Option<&DisplayOutput>) -> WindowDesc {
    let size = match o {
        Some(o) => {
            let s = (o.scale_milli.max(250) as f32) / 1000.0;
            SizeSpec::Fixed(((o.rect.w as f32 / s).round() as u32).max(64), ((o.rect.h as f32 / s).round() as u32).max(64))
        }
        None => SizeSpec::FullScreen,
    };
    let mut d = WindowDesc::new(title, size);
    d.hints.full_screen_on = o.map(|o| o.identity);
    d.hints.server_decorations = Some(false);
    d
}

/// One window of the show and its app.
struct Pane<V: View> {
    win: Window,
    app: App<V>,
}

impl<V: View> Pane<V> {
    /// Every event waiting, handled by the window; `None` when it closed.
    fn drain(&mut self) -> io::Result<Option<Vec<fresco_client::Event>>> {
        let mut evs = Vec::new();
        loop {
            match self.win.step(&mut self.app, Some(Duration::ZERO))? {
                StepResult::Closed => return Ok(None),
                StepResult::Timeout => return Ok(Some(evs)),
                StepResult::Event(e) => evs.push(e),
            }
        }
    }
    fn paint(&mut self) -> io::Result<()> {
        self.app.invalidate();
        self.win.paint(&mut self.app)
    }
}

/// How a show went, for tests and the log.
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// The slide on screen when it ended (deck index).
    pub last: usize,
    /// Animations finished: (frames, worst gap µs, cancelled).
    pub animations: Vec<(u32, u32, bool)>,
}

/// Presents `deck` from slide `start` until the presenter ends the show.
/// `idle` runs between events (the editor's window keeps up meanwhile);
/// `press_every` presses Next by itself at that interval (tests, kiosks).
pub fn present(deck: Deck, deck_key: u64, fonts: Rc<dyn Fonts>, start: usize, embeds: Option<Rc<RefCell<Embeds>>>, press_every: Option<Duration>, mut idle: impl FnMut()) -> io::Result<Report> {
    let mut conn = fresco_client::Connection::connect_default()?;
    let outs = conn.display_outputs().unwrap_or_default();
    let (a, p) = pick(&outs);
    let win = Window::open_on(conn, &desc_on("Slide Show", a.map(|i| &outs[i])))?;
    let presenter = match p {
        Some(i) => match Window::open(&desc_on("Presenter View", Some(&outs[i]))) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("atrium-slides: no presenter view: {e}");
                None
            }
        },
        None => None,
    };
    let stage = Rc::new(RefCell::new(Stage::new(deck, deck_key, fonts)));
    let mut aud = Pane { win, app: App::new(Audience(stage.clone())) };
    if let Some(e) = &embeds {
        aud.win.set_slot_binder(Embeds::binder(e));
    }
    let mut pres = presenter.map(|win| Pane { win, app: App::new(Presenter(stage.clone())) });
    {
        let mut st = stage.borrow_mut();
        st.audience = (aud.win.width, aud.win.height);
        st.scale = aud.win.scale() as f64;
        st.presenter = pres.as_ref().map_or((0.0, 0.0), |p| (p.win.width, p.win.height));
    }
    let mut run = Running { stage: stage.clone(), handle: 0, live: None, report: Report::default(), auto: None, settle: None, players: Vec::new(), embeds, log: std::env::var_os("ATRIUM_SLIDES_ANIM_LOG").is_some() };
    let go = {
        let mut st = stage.borrow_mut();
        let Stage { show, deck, .. } = &mut *st;
        show.start(deck, start)
    };
    run.apply(go, &mut aud, pres.as_mut())?;
    let mut press_at = press_every.map(|d| Instant::now() + d);
    let mut second = Instant::now();
    'show: loop {
        let mut acts = Vec::new();
        let Some(evs) = aud.drain()? else { break };
        for e in evs {
            match e {
                fresco_client::Event::Key { hid_usage, pressed: true, .. } => acts.extend(act_of_key(hid_usage)),
                fresco_client::Event::PointerButton { pressed: true, .. } => acts.push(Act::Next),
                fresco_client::Event::AnimationFinished(f) => run.finished(&f),
                _ => {}
            }
        }
        if let Some(p) = pres.as_mut() {
            match p.drain()? {
                None => break,
                Some(evs) => {
                    for e in evs {
                        if let fresco_client::Event::Key { hid_usage, pressed: true, .. } = e {
                            acts.extend(act_of_key(hid_usage));
                        }
                    }
                }
            }
        }
        acts.append(&mut stage.borrow().acts.lock().unwrap());
        let now = Instant::now();
        if press_at.is_some_and(|t| now >= t) {
            acts.push(Act::Next);
            press_at = press_every.map(|d| now + d);
        }
        if run.auto.is_some_and(|t| now >= t) {
            run.auto = None;
            acts.push(Act::Next);
        }
        for a in acts {
            let ended = stage.borrow().ended;
            if a == Act::Exit || (ended && matches!(a, Act::Next | Act::Enter)) {
                break 'show;
            }
            let go = run.act(a);
            run.apply(go, &mut aud, pres.as_mut())?;
        }
        if run.settle.is_some_and(|t| now >= t) {
            run.settle = None;
            run.start_videos(&mut aud)?;
        }
        if let Some(e) = &run.embeds {
            while let Ok(Some(_)) = e.borrow_mut().poll(Duration::ZERO) {}
        }
        if second.elapsed() >= Duration::from_secs(1) {
            second = Instant::now();
            if let Some(p) = pres.as_mut() {
                p.paint()?;
            }
        }
        idle();
        // Pictures decoded on the workers (a slide drawn from previews
        // sharpens): painted again between animations.
        if run.live.is_none() && crate::render::pump(crate::decode::take_decoded()) {
            aud.paint()?;
            if let Some(p) = pres.as_mut() {
                p.paint()?;
            }
        }
        // Wait for either window, a timer or the next second.
        let mut wait = Duration::from_millis(if !run.players.is_empty() || crate::render::decoding() > 0 { 50 } else { 250 });
        for t in [run.auto, run.settle, press_at].into_iter().flatten() {
            wait = wait.min(t.saturating_duration_since(Instant::now()));
        }
        let mut fds = vec![libc::pollfd { fd: aud.win.connection().as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if let Some(p) = pres.as_mut() {
            fds.push(libc::pollfd { fd: p.win.connection().as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        // SAFETY: fds is a live array of pollfd of the length given.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait.as_millis() as i32) };
    }
    run.stop_videos();
    let mut report = run.report.clone();
    report.last = stage.borrow().show.slide;
    Ok(report)
}

struct Running {
    stage: Rc<RefCell<Stage>>,
    handle: u32,
    /// The animation running, by handle.
    live: Option<u32>,
    report: Report,
    /// When the slide moves on by itself (its transition's `after_ms`).
    auto: Option<Instant>,
    /// When the slide shown has arrived (its videos start).
    settle: Option<Instant>,
    players: Vec<Playing>,
    embeds: Option<Rc<RefCell<Embeds>>>,
    log: bool,
}

impl Running {
    fn finished(&mut self, f: &fresco_protocol::AnimationFinishedEvent) {
        if self.live == Some(f.handle) {
            self.live = None;
        }
        self.report.animations.push((f.frames, f.worst_gap_us, f.cancelled));
        if self.log {
            eprintln!("atrium-slides: animation {} {} frames, worst gap {} us{}{}", f.handle, f.frames, f.worst_gap_us, if f.cancelled { ", cancelled" } else { "" }, if f.refused { ", refused" } else { "" });
        }
    }

    /// What an action does to the show.
    fn act(&mut self, a: Act) -> Go {
        let mut st = self.stage.borrow_mut();
        if st.blank.is_some() && !matches!(a, Act::Black | Act::White | Act::Digit(_)) {
            st.blank = None;
            return Go::Stay;
        }
        let Stage { show, deck, typed, blank, .. } = &mut *st;
        match a {
            Act::Next => show.next(deck),
            Act::Prev => show.prev(deck),
            Act::GoTo(i) => show.go_to(deck, i),
            Act::First => show.go_to(deck, 0),
            Act::Last => show.go_to(deck, deck.slide_count().saturating_sub(1)),
            Act::Digit(d) => {
                if typed.len() < 6 {
                    typed.push((b'0' + d) as char);
                }
                Go::Stay
            }
            Act::Enter => match std::mem::take(typed).parse::<usize>() {
                Ok(n) if n >= 1 => show.go_to(deck, n - 1),
                _ => show.next(deck),
            },
            Act::Black => {
                *blank = if *blank == Some(BLACK) { None } else { Some(BLACK) };
                Go::Stay
            }
            Act::White => {
                let white = Color::rgba(1.0, 1.0, 1.0, 1.0);
                *blank = if *blank == Some(white) { None } else { Some(white) };
                Go::Stay
            }
            Act::Exit => Go::Stay,
        }
    }

    fn cancel(&mut self, aud: &mut Pane<Audience>) {
        if let Some(h) = self.live.take() {
            let id = aud.win.id();
            let _ = aud.win.connection().animation_cancel(id, h);
        }
    }

    fn animate(&mut self, aud: &mut Pane<Audience>, tracks: Vec<Track>) -> u32 {
        if tracks.is_empty() {
            return 0;
        }
        let run = tracks_ms(&tracks);
        self.handle = self.handle.wrapping_add(1).max(1);
        self.live = Some(self.handle);
        aud.win.animate(self.handle, tracks);
        run
    }

    /// Shows what `go` says, animated where it moves on.
    fn apply(&mut self, go: Go, aud: &mut Pane<Audience>, pres: Option<&mut Pane<Presenter>>) -> io::Result<()> {
        let leaving = matches!(go, Go::Slide { .. } | Go::Back { .. } | Go::End);
        if leaving {
            self.stop_videos();
        }
        let mut ran = 0;
        match go {
            Go::Stay => {}
            Go::End => {
                self.cancel(aud);
                let mut st = self.stage.borrow_mut();
                st.ended = true;
                self.auto = None;
            }
            Go::Step { slide, step } => {
                self.cancel(aud);
                let tracks = {
                    let mut st = self.stage.borrow_mut();
                    st.done.insert(slide, step + 1);
                    st.step_tracks(slide, step, 0)
                };
                ran = self.animate(aud, tracks);
            }
            Go::Back { slide, done } => {
                self.cancel(aud);
                let mut st = self.stage.borrow_mut();
                st.visible = Some(slide);
                st.top = Some(slide);
                st.ended = false;
                st.done.insert(slide, done);
                st.resident = { let Stage { show, deck, .. } = &mut *st; show.resident(deck) };
                st.prepare();
                self.settle = Some(Instant::now());
            }
            Go::Slide { from, to, transition } => {
                self.cancel(aud);
                // First the new slide's layers, unseen, as before it arrives.
                {
                    let mut st = self.stage.borrow_mut();
                    st.ended = false;
                    st.ready(to);
                    st.done.insert(to, 0);
                    if !st.resident.contains(&to) {
                        st.resident.push(to);
                    }
                    let over_old = transition.is_some_and(|t| old_on_top(t.kind));
                    st.top = if over_old { from } else { Some(to) };
                }
                aud.paint()?;
                // Then its end state, with the tracks that lead there.
                let tracks = {
                    let mut st = self.stage.borrow_mut();
                    let (stage, _) = st.stage();
                    let new = st.ids.get(&(to, None)).copied();
                    let old = from.and_then(|f| st.ids.get(&(f, None)).copied());
                    let (tr_ms, mut tracks) = match (transition, new) {
                        (Some(t), Some(n)) if from.is_some() => (t.ms.min(MAX_MS), transition_tracks(t.kind, t.ms, old, n, stage)),
                        _ => (0, Vec::new()),
                    };
                    let tr_ms = if tracks.is_empty() { 0 } else { tr_ms };
                    st.visible = Some(to);
                    st.done.insert(to, 1);
                    tracks.extend(st.step_tracks(to, 0, tr_ms));
                    tracks.truncate(fresco_protocol::ANIM_MAX_TRACKS);
                    st.resident = { let Stage { show, deck, .. } = &mut *st; show.resident(deck) };
                    st.prepare();
                    tracks
                };
                ran = self.animate(aud, tracks);
                self.settle = Some(Instant::now() + Duration::from_millis(ran as u64));
            }
        }
        // A slide whose transition says so moves on by itself.
        if matches!(go, Go::Slide { .. } | Go::Step { .. }) {
            let mut st = self.stage.borrow_mut();
            let cur = st.show.slide;
            let after = st.ready(cur).transition.and_then(|t| t.after_ms);
            self.auto = after.map(|a| Instant::now() + Duration::from_millis(ran as u64 + a as u64));
        } else if matches!(go, Go::Back { .. }) {
            self.auto = None;
        }
        self.stage.borrow_mut().prepare();
        aud.paint()?;
        if let Some(p) = pres {
            p.paint()?;
        }
        Ok(())
    }

    /// Puts players on the shown slide's videos.
    fn start_videos(&mut self, aud: &mut Pane<Audience>) -> io::Result<()> {
        let Some(e) = self.embeds.clone() else { return Ok(()) };
        let Some(token) = aud.win.embed_token() else { return Ok(()) };
        let (slide, videos, files) = {
            let mut st = self.stage.borrow_mut();
            if st.ended {
                return Ok(());
            }
            let slide = st.show.slide;
            let videos = st.ready(slide).videos();
            let files: Vec<_> = videos.iter().map(|(m, ..)| (media_file(&st.deck, *m), st.deck.media(*m).map(|x| x.mime.to_string()).unwrap_or_default())).collect();
            (slide, videos, files)
        };
        if videos.is_empty() {
            return Ok(());
        }
        for (n, (file, mime)) in files.into_iter().enumerate() {
            let Ok(file) = file else { continue };
            let asked = e.borrow_mut().request("media-player", token, VIDEO_SLOT + n as u64);
            let Ok(embed) = asked else { continue };
            let load = limen::media_player::ToChild::Load { source: limen::media_player::Source::Fd, mime, growing: false }.encode();
            let sent = e.borrow_mut().send_fd(embed, load, file.as_fd());
            if sent.is_err() {
                let _ = e.borrow_mut().detach_embed(embed);
                continue;
            }
            let _ = e.borrow_mut().send(embed, limen::media_player::ToChild::Play.encode());
            self.players.push(Playing { embed, _file: file });
        }
        self.stage.borrow_mut().playing = Some(slide);
        aud.paint()
    }

    fn stop_videos(&mut self) {
        if let Some(e) = &self.embeds {
            for p in self.players.drain(..) {
                let _ = e.borrow_mut().detach_embed(p.embed);
            }
        }
        self.players.clear();
        self.stage.borrow_mut().playing = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresco_protocol::WmRect;
    use officina_slides::model::Trigger;

    fn out(id: u64, x: i32, internal: bool, primary: bool) -> DisplayOutput {
        DisplayOutput { identity: id, rect: WmRect { x, y: 0, w: 1920, h: 1080 }, scale_milli: 1000, refresh_mhz: 60_000, internal, primary }
    }

    #[test]
    fn the_show_goes_to_the_projector_and_the_presenter_view_stays() {
        assert_eq!(pick(&[]), (None, None));
        assert_eq!(pick(&[out(1, 0, true, true)]), (Some(0), None));
        // A laptop (its panel first) and a projector.
        assert_eq!(pick(&[out(1, 0, true, true), out(2, 1920, false, false)]), (Some(1), Some(0)));
        // Two external monitors.
        assert_eq!(pick(&[out(1, 0, false, true), out(2, 1920, false, false)]), (Some(1), Some(0)));
        // A projector listed first but not primary.
        assert_eq!(pick(&[out(2, 1920, false, false), out(1, 0, true, true)]), (Some(0), Some(1)));
    }

    #[test]
    fn a_slide_fits_the_screen_letterboxed() {
        let (r, p) = fit((12_192_000, 6_858_000), Rect::new(0.0, 0.0, 1600.0, 1200.0));
        assert_eq!((r.x(), r.w()), (0.0, 1600.0));
        assert!((r.y() - 150.0).abs() < 0.01 && (r.h() - 900.0).abs() < 0.01);
        assert!((p.k * 12_192_000.0 - 1600.0).abs() < 1e-6);
    }

    #[test]
    fn transitions_become_tracks_on_the_two_slides() {
        let stage = Rect::new(0.0, 60.0, 1280.0, 720.0);
        let (o, n) = (NodeId(5), NodeId(9));
        assert!(transition_tracks(TransitionKind::Cut, 500, Some(o), n, stage).is_empty());
        let push = transition_tracks(TransitionKind::Push(Dir::Left), 700, Some(o), n, stage);
        // The old slide holds, the new comes from the right, the old leaves to the left.
        assert_eq!(push.len(), 3);
        assert_eq!(push[0], Track::new(o, 700, Value::Opacity { from: 1.0, to: 0.0 }).curve(curve::STEP));
        assert_eq!(push[1].value, Value::Offset { from: Offset::by(1280.0, 0.0), to: Offset::NONE });
        assert_eq!(push[2].value, Value::Offset { from: Offset::NONE, to: Offset::by(-1280.0, 0.0) });
        let wipe = transition_tracks(TransitionKind::Wipe(Dir::Down), 400, Some(o), n, stage);
        assert_eq!(wipe[1].value, Value::Clip { from: Rect::new(0.0, 60.0, 1280.0, 0.0), to: stage });
        let split = transition_tracks(TransitionKind::Split { vertical: true, out: false }, 400, Some(o), n, stage);
        assert_eq!(split[1].node, o, "split in closes the old slide");
        assert!(old_on_top(TransitionKind::Split { vertical: true, out: false }));
        let zoom = transition_tracks(TransitionKind::Zoom { out: true }, 400, Some(o), n, stage);
        assert_eq!(zoom.len(), 2);
        assert!(zoom.iter().all(|t| t.node == o && t.curve != curve::STEP));
        // The first slide has no old one.
        assert_eq!(transition_tracks(TransitionKind::Fade, 400, None, n, stage).len(), 1);
    }

    #[test]
    fn builds_become_tracks_at_their_start_times() {
        let b = |shape, effect, trigger, ms, delay_ms| Build { shape, effect, trigger, ms, delay_ms, by_paragraph: false };
        let builds = [
            b(1, Effect::FlyIn(Dir::Left), Trigger::OnClick, 500, 0),
            b(2, Effect::Wipe(Dir::Up), Trigger::WithPrevious, 300, 100),
            b(3, Effect::Appear, Trigger::AfterPrevious, 0, 0),
            b(4, Effect::FadeOut, Trigger::AfterPrevious, 200, 0),
        ];
        let steps = show::steps(&builds);
        let layer = |s: ShapeId| Some(NodeId(100 + s));
        let bounds = |_s: ShapeId| Some(Rect::new(100.0, 50.0, 200.0, 80.0));
        let t = build_tracks(&builds, &steps[1], 250, layer, bounds, (1280.0, 720.0));
        assert_eq!(t.len(), 4);
        assert_eq!((t[0].node, t[0].delay_ms), (NodeId(101), 250));
        assert_eq!(t[0].value, Value::Offset { from: Offset::by(-300.0, 0.0), to: Offset::NONE });
        assert_eq!(t[1].delay_ms, 350);
        assert_eq!(t[1].value, Value::Clip { from: Rect::new(100.0, 50.0, 200.0, 0.0), to: Rect::new(100.0, 50.0, 200.0, 80.0) });
        assert_eq!((t[2].delay_ms, t[2].curve), (250 + 400, curve::STEP));
        assert_eq!(t[3].value, Value::Opacity { from: 1.0, to: 0.0 });
        assert_eq!(tracks_ms(&t), 650 + 200);
    }

    #[test]
    fn keys_drive_the_show() {
        assert_eq!(act_of_key(0x4F), Some(Act::Next));
        assert_eq!(act_of_key(0x2C), Some(Act::Next));
        assert_eq!(act_of_key(0x4B), Some(Act::Prev));
        assert_eq!(act_of_key(0x29), Some(Act::Exit));
        assert_eq!(act_of_key(0x05), Some(Act::Black));
        assert_eq!(act_of_key(0x20), Some(Act::Digit(3)));
        assert_eq!(act_of_key(0x27), Some(Act::Digit(0)));
        assert_eq!(act_of_key(0x04), None);
        assert_eq!(elapsed(Duration::from_secs(3723)), "1:02:03");
    }
}
