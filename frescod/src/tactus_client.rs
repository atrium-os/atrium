//! Input from tactusd (docs/spec/atrium-input.md §2): Tactus reads the
//! devices and sends canonical events; frescod integrates the cursor,
//! hit-tests, routes by focus and mints activation tokens, as its own
//! hidraw readers did.
//!
//! This is frescod's only device input: it parses no device reports and
//! its jail has no input nodes. The socket is `FRESCOD_TACTUS_SOCKET`, or
//! /atrium/sockets/tactus/tactus.sock; while tactusd is down frescod
//! retries every second and has no input. (Tests also have the injector
//! socket, which mints no activation tokens.)
//!
//! Mapping, as frescod's old hidraw readers did it:
//! - keyboard page keys go to `keyboard_window()` as `InputKey` with the
//!   usage's low 16 bits; modifier keys (0xE0..0xE7) update the modifier
//!   bitmap and are not sent themselves;
//! - consumer-page keys go to the WM only (window 0, `0x0C00 | usage`, no
//!   token: atrium-player-pip-keys.md §4.3);
//! - key repeats go to the window the press went to, flagged `repeat`
//!   and without a token, and stop if that window loses the keyboard;
//! - pointer buttons, motion (relative or absolute) and wheels move the
//!   cursor and go to the window under it; a touchpad's two-finger scroll
//!   goes, with its phases, to the window it began over, content following
//!   the fingers;
//! - a pinch goes to the window it began over as `InputPointerMagnify`
//!   (scale against the start, about where it began); three- and
//!   four-finger swipes go to the WM only (window 0) as `InputSwipe`,
//!   with the total movement since they began.
//! Every press from tactusd mints an activation token: tactusd is the
//! only peer on that socket (its directory is _tactus:_frescod 0750).

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fresco_scene_server::window::{Compositor, DisplayEvent, MINT_HERE};
use fresco_protocol::{control, scroll_phase};
use tactus::wire::{self, Axis, GestureKind, Message, Phase};

pub const DEFAULT_SOCKET: &str = "/atrium/sockets/tactus/tactus.sock";

/// tactusd's socket.
pub fn socket() -> String {
    std::env::var("FRESCOD_TACTUS_SOCKET").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_SOCKET.to_string())
}

/// `fallback`: the screen size while no mode is set (headless).
pub fn spawn(sink: Sender<DisplayEvent>, comp: Arc<Mutex<Compositor>>, path: String, fallback: (u32, u32)) {
    let _ = FALLBACK.set(fallback);
    std::thread::Builder::new()
        .name("frescod-tactus".into())
        .spawn(move || {
            let mut said = false;
            loop {
                match UnixStream::connect(&path) {
                    Ok(s) => {
                        eprintln!("frescod: input from tactusd ({path})");
                        said = false;
                        let mut seat = Seat::default();
                        let e = seat.run(s, &sink, &comp);
                        eprintln!("frescod: tactusd connection ended: {e}");
                        // Whatever was held is released: tactusd is gone.
                        seat.release(&sink, &comp);
                    }
                    Err(e) if !said => {
                        eprintln!("frescod: tactusd not reachable at {path} ({e}); retrying");
                        said = true;
                    }
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("spawn tactus client");
}

#[derive(Default)]
struct Seat {
    /// HID modifier byte (LCtrl=1 .. RGui=128), across keyboards.
    mods: u8,
    /// Keys sent as pressed, with the window they went to.
    keys: Vec<(u32, u16)>,
    buttons: Vec<u8>,
    /// The window a touchpad scroll is going to, between Begin and End.
    scrolling: Option<u32>,
    /// A pinch going on: its window, where it began (window-local) and
    /// the last scale.
    magnify: Option<(u32, f32, f32, f32)>,
    /// A swipe going on: fingers and total movement so far.
    swipe: Option<(u8, f32, f32)>,
    /// Gamepads and joysticks, by tactus device id.
    pads: std::collections::BTreeMap<u32, Pad>,
}

/// A gamepad (spec §4.1): it belongs to the focused window, which got
/// `CONNECT` and every event since; when focus moves the old window gets
/// releases and `DISCONNECT`, the new one `CONNECT` and where the axes are.
#[derive(Default)]
struct Pad {
    name: String,
    vendor: u16,
    product: u16,
    buttons: u16,
    axes: u16,
    /// The window it is connected to.
    owner: Option<u32>,
    /// Buttons held that `owner` saw pressed.
    held: Vec<u32>,
    /// Every axis's last value.
    state: Vec<(u32, i16)>,
}

fn wire_phase(p: Phase) -> u8 {
    match p {
        Phase::Begin => scroll_phase::BEGIN,
        Phase::Update => scroll_phase::UPDATE,
        Phase::End => scroll_phase::END,
    }
}

/// The HID modifier byte to the wire's bits (shift 1, ctrl 2, alt 4).
fn wire_mods(hid: u8) -> u8 {
    let mut out = 0u8;
    if hid & 0b0010_0010 != 0 {
        out |= 0x01;
    }
    if hid & 0b0001_0001 != 0 {
        out |= 0x02;
    }
    if hid & 0b0100_0100 != 0 {
        out |= 0x04;
    }
    out
}

impl Seat {
    fn run(&mut self, mut s: UnixStream, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) -> std::io::Error {
        match wire::read(&mut s) {
            Ok(Message::Hello { version }) if version == wire::VERSION => {}
            Ok(m) => return std::io::Error::other(format!("unexpected hello {m:?}")),
            Err(e) => return e,
        }
        {
            let (w, h) = screen();
            comp.lock().unwrap().cursor = (w / 2.0, h / 2.0);
        }
        loop {
            let m = match wire::read(&mut s) {
                Ok(m) => m,
                Err(e) => return e,
            };
            let pad = matches!(m, Message::PadButton { .. } | Message::PadAxis { .. } | Message::PadLayout { .. });
            self.handle(m, sink, comp);
            if !pad {
                self.pads_follow(sink, comp);
            }
        }
    }

    fn handle(&mut self, m: Message, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) {
        match m {
            Message::Device { id, bus, classes, name, vendor, product } => {
                eprintln!("frescod: input device {id} {name:?} ({bus:?}, {classes:?})");
                if classes.iter().any(|c| matches!(c, tactus::wire::Class::Gamepad | tactus::wire::Class::Joystick)) {
                    self.pads.insert(id, Pad { name, vendor, product, ..Pad::default() });
                }
            }
            Message::PadLayout { id, buttons, axes } => {
                let target = comp.lock().unwrap().keyboard_window();
                if let Some(p) = self.pads.get_mut(&id) {
                    p.buttons = u16::try_from(buttons.len()).unwrap_or(u16::MAX);
                    p.axes = u16::try_from(axes.len()).unwrap_or(u16::MAX);
                    // Plugged in: the focused window has it from now.
                    pad_follow(sink, id, p, target);
                }
            }
            Message::PadButton { id, usage, down, .. } => {
                let target = comp.lock().unwrap().keyboard_window();
                let Some(p) = self.pads.get_mut(&id) else { return };
                pad_follow(sink, id, p, target);
                let Some(window_id) = p.owner else { return };
                if down {
                    if p.held.contains(&usage) {
                        return;
                    }
                    p.held.push(usage);
                } else {
                    // A release the window never saw pressed (it pressed
                    // before the pad became this window's) is not sent.
                    let Some(i) = p.held.iter().position(|&u| u == usage) else { return };
                    p.held.remove(i);
                }
                let _ = sink.send(DisplayEvent::Gamepad { window_id, kind: control::EV_GAMEPAD_BUTTON, device_id: id, usage, value: i32::from(down) });
            }
            Message::PadAxis { id, usage, value, .. } => {
                let target = comp.lock().unwrap().keyboard_window();
                let Some(p) = self.pads.get_mut(&id) else { return };
                match p.state.iter_mut().find(|(u, _)| *u == usage) {
                    Some((_, v)) => *v = value,
                    None => p.state.push((usage, value)),
                }
                // A new owner gets this value in its snapshot.
                if pad_follow(sink, id, p, target) {
                    return;
                }
                if let Some(window_id) = p.owner {
                    let _ = sink.send(DisplayEvent::Gamepad { window_id, kind: control::EV_GAMEPAD_AXIS, device_id: id, usage, value: value.into() });
                }
            }
            Message::Key { usage, down, repeat, .. } => {
                let (page, id) = ((usage >> 16) as u16, (usage & 0xffff) as u16);
                if repeat {
                    if page == 0x07 {
                        self.repeat(sink, comp, id);
                    }
                    return;
                }
                match page {
                    0x07 if (0xe0..=0xe7).contains(&id) => {
                        let bit = 1u8 << (id - 0xe0);
                        if down {
                            self.mods |= bit;
                        } else {
                            self.mods &= !bit;
                        }
                    }
                    0x07 => self.key(sink, comp, id, down),
                    0x0c if id < 0x400 => {
                        let _ = sink.send(DisplayEvent::InputKey {
                            window_id: 0,
                            hid_usage: 0x0c00 | id,
                            pressed: down,
                            modifiers: 0,
                            activation: [0; 16],
                            repeat: false,
                        });
                    }
                    _ => {}
                }
            }
            Message::Button { usage, down, .. } => {
                if usage >> 16 == 0x09 && (1..=32).contains(&(usage & 0xffff)) {
                    let b = (usage & 0xffff) as u8;
                    self.buttons.retain(|&x| x != b);
                    if down {
                        self.buttons.push(b);
                    }
                    self.button(sink, comp, b, down);
                }
            }
            Message::Motion { dx, dy, .. } => {
                let (w, h) = screen();
                let mut g = comp.lock().unwrap();
                let (x, y) = g.cursor;
                self.moved(sink, &mut g, (x + dx).clamp(0.0, w - 1.0), (y + dy).clamp(0.0, h - 1.0));
            }
            Message::Absolute { x, y, .. } => {
                let (w, h) = screen();
                let mut g = comp.lock().unwrap();
                self.moved(sink, &mut g, x * (w - 1.0), y * (h - 1.0));
            }
            Message::Scroll { axis, value_120, .. } => {
                let (target, _) = {
                    let g = comp.lock().unwrap();
                    let (x, y) = g.cursor;
                    g.pointer_target(x, y)
                };
                // HID: positive is away from the user; the wire's positive
                // dy scrolls content down. 8 px per detent, as before.
                let v = value_120 as f32 / 120.0 * 8.0;
                let (dx, dy) = match axis {
                    Axis::Vertical => (0.0, -v),
                    Axis::Horizontal => (v, 0.0),
                };
                let _ = sink.send(DisplayEvent::InputPointerScroll { window_id: target as u32, dx, dy, phase: scroll_phase::NONE });
            }
            Message::ScrollPixels { dx, dy, phase, .. } => {
                // Content follows the fingers ("natural" scrolling): fingers
                // up scroll the content up, i.e. the view down (+dy). The
                // whole scroll goes to the window it began over.
                let window_id = match (phase, self.scrolling) {
                    (Phase::Begin, _) | (_, None) => {
                        let g = comp.lock().unwrap();
                        let (x, y) = g.cursor;
                        g.pointer_target(x, y).0 as u32
                    }
                    (_, Some(w)) => w,
                };
                self.scrolling = (phase != Phase::End).then_some(window_id);
                let phase = wire_phase(phase);
                let _ = sink.send(DisplayEvent::InputPointerScroll { window_id, dx: -dx, dy: -dy, phase });
            }
            // A pinch goes, whole, to the window it began over, about the
            // pointer where it began; tactusd's END carries no scale, so the
            // last one is repeated.
            Message::Gesture { kind: GestureKind::Pinch, phase, scale, .. } => {
                let (window_id, x, y) = match (phase, self.magnify) {
                    (Phase::Begin, _) | (_, None) => {
                        let g = comp.lock().unwrap();
                        let (cx, cy) = g.cursor;
                        let (w, (x, y)) = g.pointer_target(cx, cy);
                        (w as u32, x, y)
                    }
                    (_, Some((w, x, y, _))) => (w, x, y),
                };
                let scale = match phase {
                    Phase::End => self.magnify.map_or(1.0, |m| m.3),
                    _ if scale.is_finite() && scale > 0.0 => scale,
                    _ => return,
                };
                self.magnify = (phase != Phase::End).then_some((window_id, x, y, scale));
                let phase = wire_phase(phase);
                let _ = sink.send(DisplayEvent::InputPointerMagnify { window_id, x, y, scale, phase });
            }
            // Three- and four-finger swipes are system gestures: to the WM
            // (window 0) only, as the total since the swipe began.
            Message::Gesture { kind: GestureKind::Swipe, fingers, phase, dx, dy, .. } => {
                let (tx, ty) = match (phase, self.swipe) {
                    (Phase::Begin, _) | (_, None) => (0.0, 0.0),
                    (_, Some((_, x, y))) => (x, y),
                };
                let (tx, ty) = if phase == Phase::End { (tx, ty) } else { (tx + dx, ty + dy) };
                if !(tx.is_finite() && ty.is_finite()) {
                    return;
                }
                self.swipe = (phase != Phase::End).then_some((fingers, tx, ty));
                let _ = sink.send(DisplayEvent::InputSwipe { window_id: 0, fingers, dx: tx, dy: ty, phase: wire_phase(phase) });
            }
            Message::Gone { id } => {
                eprintln!("frescod: input device {id} gone");
                if let Some(mut p) = self.pads.remove(&id) {
                    pad_follow(sink, id, &mut p, 0);
                }
            }
            Message::Hello { .. } => {}
        }
    }

    /// Hand every gamepad to the focused window, if focus moved (a click
    /// or a key usually moves it, so this runs after each).
    fn pads_follow(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) {
        if self.pads.values().all(|p| p.owner.is_none()) {
            return;
        }
        let target = comp.lock().unwrap().keyboard_window();
        for (&id, p) in self.pads.iter_mut() {
            if p.owner.is_some() {
                pad_follow(sink, id, p, target);
            }
        }
    }

    fn key(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, id: u16, down: bool) {
        // A release goes where the press went, even if focus moved.
        let window_id = if down {
            let w = comp.lock().unwrap().keyboard_window();
            self.keys.push((w, id));
            w
        } else {
            match self.keys.iter().position(|&(_, k)| k == id) {
                Some(i) => self.keys.remove(i).0,
                None => return,
            }
        };
        let _ = sink.send(DisplayEvent::InputKey {
            window_id,
            hid_usage: id,
            pressed: down,
            modifiers: wire_mods(self.mods),
            activation: if down { MINT_HERE } else { [0; 16] },
            repeat: false,
        });
    }

    /// A repeat of held key `id`, to the window its press went to while
    /// that window still has the keyboard.
    fn repeat(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, id: u16) {
        let Some(&(window_id, _)) = self.keys.iter().find(|&&(_, k)| k == id) else { return };
        if comp.lock().unwrap().keyboard_window() != window_id {
            return;
        }
        let _ = sink.send(DisplayEvent::InputKey {
            window_id,
            hid_usage: id,
            pressed: true,
            modifiers: wire_mods(self.mods),
            activation: [0; 16],
            repeat: true,
        });
    }

    fn button(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, button: u8, down: bool) {
        let (target, (x, y)) = {
            let g = comp.lock().unwrap();
            let (cx, cy) = g.cursor;
            g.pointer_target(cx, cy)
        };
        let _ = sink.send(DisplayEvent::InputPointerButton {
            window_id: target as u32,
            x,
            y,
            button,
            pressed: down,
            modifiers: wire_mods(self.mods),
            activation: if down { MINT_HERE } else { [0; 16] },
        });
    }

    fn moved(&mut self, sink: &Sender<DisplayEvent>, g: &mut Compositor, x: f32, y: f32) {
        if g.cursor == (x, y) {
            return;
        }
        g.cursor = (x, y);
        let (target, (lx, ly)) = g.pointer_target(x, y);
        let _ = sink.send(DisplayEvent::InputPointerMotion { window_id: target as u32, x: lx, y: ly });
    }

    /// tactusd went away: release what it held and end what was going on.
    fn release(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) {
        if let Some(window_id) = self.scrolling.take() {
            let _ = sink.send(DisplayEvent::InputPointerScroll { window_id, dx: 0.0, dy: 0.0, phase: scroll_phase::END });
        }
        if let Some((window_id, x, y, scale)) = self.magnify.take() {
            let _ = sink.send(DisplayEvent::InputPointerMagnify { window_id, x, y, scale, phase: scroll_phase::END });
        }
        if let Some((fingers, dx, dy)) = self.swipe.take() {
            let _ = sink.send(DisplayEvent::InputSwipe { window_id: 0, fingers, dx, dy, phase: scroll_phase::END });
        }
        for (_, k) in self.keys.clone() {
            self.key(sink, comp, k, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.button(sink, comp, b, false);
        }
        for (id, mut p) in std::mem::take(&mut self.pads) {
            pad_follow(sink, id, &mut p, 0);
        }
        self.mods = 0;
    }
}

/// Give pad `id` to window `target` (0: no one) if it is not already
/// its: releases and `DISCONNECT` to the old owner, `CONNECT` and every
/// axis to the new. True if it changed hands.
fn pad_follow(sink: &Sender<DisplayEvent>, id: u32, p: &mut Pad, target: u32) -> bool {
    let target = (target != 0).then_some(target);
    if p.owner == target {
        return false;
    }
    if let Some(window_id) = p.owner.take() {
        for usage in std::mem::take(&mut p.held) {
            let _ = sink.send(DisplayEvent::Gamepad { window_id, kind: control::EV_GAMEPAD_BUTTON, device_id: id, usage, value: 0 });
        }
        let _ = sink.send(DisplayEvent::Gamepad { window_id, kind: control::EV_GAMEPAD_DISCONNECT, device_id: id, usage: 0, value: 0 });
    }
    if let Some(window_id) = target {
        let _ = sink.send(DisplayEvent::GamepadConnect {
            window_id,
            device_id: id,
            vendor: p.vendor,
            product: p.product,
            buttons: p.buttons,
            axes: p.axes,
            name: p.name.clone(),
        });
        for &(usage, value) in &p.state {
            let _ = sink.send(DisplayEvent::Gamepad { window_id, kind: control::EV_GAMEPAD_AXIS, device_id: id, usage, value: value.into() });
        }
        p.owner = Some(window_id);
    }
    true
}

static FALLBACK: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

fn screen() -> (f32, f32) {
    let (mut w, mut h, _) = crate::screen::get();
    if w == 0 || h == 0 {
        (w, h) = FALLBACK.get().copied().unwrap_or((1, 1));
    }
    (w.max(1) as f32, h.max(1) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use tactus::wire::GestureKind::{Pinch, Swipe};

    fn gesture(kind: GestureKind, fingers: u8, phase: Phase, dx: f32, dy: f32, scale: f32) -> Message {
        Message::Gesture { id: 1, ts_ns: 0, kind, fingers, phase, dx, dy, scale }
    }

    /// A gamepad goes to the focused window: CONNECT on plug-in, its
    /// events, and on a focus change releases + DISCONNECT to the old
    /// window, CONNECT + the axes to the new; unplugged: DISCONNECT.
    #[test]
    fn gamepads_follow_focus() {
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (a, b) = {
            let mut g = comp.lock().unwrap();
            let a = g.create(1, (100, 100));
            let b = g.create(2, (100, 100));
            g.set_focus_no_raise(Some(a));
            (u32::from(a), u32::from(b))
        };
        let (tx, rx) = channel();
        let mut seat = Seat::default();
        let ev = |rx: &std::sync::mpsc::Receiver<DisplayEvent>| -> Vec<(u32, u16, u32, i32)> {
            rx.try_iter()
                .map(|e| match e {
                    DisplayEvent::GamepadConnect { window_id, device_id, buttons, axes, .. } => {
                        (window_id, control::EV_GAMEPAD_CONNECT, device_id, i32::from(buttons) << 8 | i32::from(axes))
                    }
                    DisplayEvent::Gamepad { window_id, kind, usage, value, .. } => (window_id, kind, usage, value),
                    e => panic!("{e:?}"),
                })
                .collect()
        };
        let dev = Message::Device { id: 7, bus: tactus::wire::Bus::Usb, classes: vec![tactus::wire::Class::Gamepad], vendor: 1, product: 2, name: "pad".into() };
        seat.handle(dev, &tx, &comp);
        seat.handle(Message::PadLayout { id: 7, buttons: vec![0x0009_0001, 0x0009_0002], axes: vec![0x0001_0030] }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_CONNECT, 7, 2 << 8 | 1)]);
        seat.handle(Message::PadAxis { id: 7, ts_ns: 0, usage: 0x0001_0030, value: -5 }, &tx, &comp);
        seat.handle(Message::PadButton { id: 7, ts_ns: 0, usage: 0x0009_0001, down: true }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_AXIS, 0x0001_0030, -5), (a, control::EV_GAMEPAD_BUTTON, 0x0009_0001, 1)]);
        // Focus moves to b (as after a click): the held button is released
        // to a, which is disconnected; b is connected with the axis.
        comp.lock().unwrap().set_focus_no_raise(Some(b as u16));
        seat.pads_follow(&tx, &comp);
        assert_eq!(
            ev(&rx),
            vec![
                (a, control::EV_GAMEPAD_BUTTON, 0x0009_0001, 0),
                (a, control::EV_GAMEPAD_DISCONNECT, 0, 0),
                (b, control::EV_GAMEPAD_CONNECT, 7, 2 << 8 | 1),
                (b, control::EV_GAMEPAD_AXIS, 0x0001_0030, -5),
            ]
        );
        // b never saw that press: its release is not sent.
        seat.handle(Message::PadButton { id: 7, ts_ns: 0, usage: 0x0009_0001, down: false }, &tx, &comp);
        assert_eq!(ev(&rx), vec![]);
        seat.handle(Message::Gone { id: 7 }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(b, control::EV_GAMEPAD_DISCONNECT, 0, 0)]);
    }

    #[test]
    fn pinch_and_swipe_routes() {
        let comp = Arc::new(Mutex::new(Compositor::new()));
        comp.lock().unwrap().cursor = (40.0, 30.0);
        let (tx, rx) = channel();
        let mut seat = Seat::default();
        // A pinch: latched where it began, END repeats the last scale.
        seat.handle(gesture(Pinch, 2, Phase::Begin, 0.0, 0.0, 1.2), &tx, &comp);
        comp.lock().unwrap().cursor = (90.0, 90.0);
        seat.handle(gesture(Pinch, 2, Phase::Update, 0.0, 0.0, 1.5), &tx, &comp);
        seat.handle(gesture(Pinch, 2, Phase::End, 0.0, 0.0, 1.0), &tx, &comp);
        let got: Vec<_> = rx.try_iter().collect();
        let m: Vec<(f32, f32, f32, u8)> = got
            .iter()
            .map(|e| match *e {
                DisplayEvent::InputPointerMagnify { x, y, scale, phase, .. } => (x, y, scale, phase),
                ref e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(m, vec![(40.0, 30.0, 1.2, 1), (40.0, 30.0, 1.5, 2), (40.0, 30.0, 1.5, 3)]);
        // A three-finger swipe: totals, to window 0, END repeats the total.
        seat.handle(gesture(Swipe, 3, Phase::Begin, 10.0, 1.0, 1.0), &tx, &comp);
        seat.handle(gesture(Swipe, 3, Phase::Update, 15.0, -1.0, 1.0), &tx, &comp);
        seat.handle(gesture(Swipe, 3, Phase::End, 0.0, 0.0, 1.0), &tx, &comp);
        let s: Vec<_> = rx
            .try_iter()
            .map(|e| match e {
                DisplayEvent::InputSwipe { window_id: 0, fingers: 3, dx, dy, phase } => (dx, dy, phase),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(s, vec![(10.0, 1.0, 1), (25.0, 0.0, 2), (25.0, 0.0, 3)]);
        // tactusd going away mid-pinch ends it.
        seat.handle(gesture(Pinch, 2, Phase::Begin, 0.0, 0.0, 0.8), &tx, &comp);
        let _ = rx.try_iter().count();
        seat.release(&tx, &comp);
        assert!(matches!(rx.try_iter().last(), Some(DisplayEvent::InputPointerMagnify { scale, phase: 3, .. }) if scale == 0.8));
    }
}
