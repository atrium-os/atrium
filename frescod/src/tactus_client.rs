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
    // A grant taken away moves the pads at once (fresco-capabilities.md §6).
    crate::caps::watch(crate::socket_server::live_clients, caps_changed);
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
                        *ACTIVE.lock().unwrap() = Some((seat.pads.clone(), sink.clone(), comp.clone()));
                        let e = seat.run(s, &sink, &comp);
                        *ACTIVE.lock().unwrap() = None;
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
    /// Gamepads and joysticks, shared with the clients' claims.
    pads: Arc<Mutex<Pads>>,
}

/// The seat's gamepads and joysticks, by tactus device id, and who holds
/// which exclusively (fresco-capabilities.md §4).
#[derive(Default)]
struct Pads {
    map: std::collections::BTreeMap<u32, Pad>,
    /// Pad -> the window that claimed it: it goes there whatever has focus.
    claims: std::collections::BTreeMap<u32, u32>,
    /// The pad that sent input last; a claim of device 0 takes it.
    recent: Option<u32>,
    /// Claimed pads whose Guide button is down, with the press's number:
    /// held [`GUIDE_HOLD`], it takes the pad back (§4).
    guide_down: std::collections::BTreeMap<u32, u64>,
    presses: u64,
}

/// How long the Guide button is held to take a claimed pad back
/// (`FRESCOD_GUIDE_HOLD_MS`, default 1000).
fn guide_hold() -> Duration {
    let ms = std::env::var("FRESCOD_GUIDE_HOLD_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(1000u64);
    Duration::from_millis(ms.clamp(200, 10_000))
}

/// The usages that are a pad's Guide (Xbox, PS, Home) button: the one its
/// standard mapping names (pergola-gamepad, as apps see it), and the HID
/// System Main Menu and AC Home usages some pads send for it.
fn guide_usages(vendor: u16, product: u16) -> Vec<u32> {
    let mut v = pergola_gamepad::Mapping::for_device(vendor, product).usages_of(pergola_gamepad::Button::Guide);
    v.extend([0x0001_0085, 0x000c_0223]);
    v
}

/// Every pad and who holds it, for the WM (`OP_WM_PADS`).
pub fn wm_pads() -> Vec<fresco_protocol::WmPad> {
    let active = ACTIVE.lock().unwrap().clone();
    let Some((pads, _, _)) = active else { return Vec::new() };
    let pads = pads.lock().unwrap();
    pads.map
        .iter()
        .map(|(&id, p)| fresco_protocol::WmPad {
            device_id: id,
            name: p.name.clone(),
            vendor: p.vendor,
            product: p.product,
            window_id: p.owner.unwrap_or(0),
            held_by: pads.claims.get(&id).copied().unwrap_or(0),
        })
        .collect()
}

/// The user takes pad `device` (0: every claimed pad) back from the window
/// holding it (`OP_WM_PAD_RELEASE`, from Forum's controller menu).
pub fn take_back(device: u32) {
    let active = ACTIVE.lock().unwrap().clone();
    if let Some((pads, sink, comp)) = active {
        pads.lock().unwrap().take_back(&sink, &comp, device, "the controller menu");
    }
}

/// Guide press number `press` on pad `device` began [`guide_hold`] ago: if
/// it is still down, the pad goes back to following focus.
fn guide_held(device: u32, press: u64) {
    let active = ACTIVE.lock().unwrap().clone();
    if let Some((pads, sink, comp)) = active {
        pads.lock().unwrap().guide_expired(&sink, &comp, device, press);
    }
}

/// The connected seat's pads and where its events go, for claims.
type Active = (Arc<Mutex<Pads>>, Sender<DisplayEvent>, Arc<Mutex<Compositor>>);
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// `OP_PAD_CLAIM` from client `client` (fresco-capabilities.md §4): answered
/// with `EV_GAMEPAD_CLAIM` to `window` unless the window is not the client's.
pub fn claim(client: u8, window: u32, device: u32, on: bool) {
    let active = ACTIVE.lock().unwrap().clone();
    match active {
        Some((pads, sink, comp)) => {
            pads.lock().unwrap().claim(&sink, &comp, client, crate::caps::of(client), window, device, on);
        }
        // No tactusd, so no pads to hold.
        None => log::info!("frescod: client {client}: pad claim with no input service"),
    }
}

/// Client `client`'s grants changed: claims it may no longer hold end,
/// and every connected pad goes where it now should.
pub fn caps_changed(client: u8) {
    let active = ACTIVE.lock().unwrap().clone();
    if let Some((pads, sink, comp)) = active {
        pads.lock().unwrap().regrant(&sink, &comp, client, crate::caps::of(client));
    }
}

/// The window pad `id` goes to now: the one holding it, else the focused
/// window if its app may have gamepads (`input.gamepad`), else none.
fn pad_target(claims: &std::collections::BTreeMap<u32, u32>, id: u32, comp: &Mutex<Compositor>) -> u32 {
    let g = comp.lock().unwrap();
    if let Some(&w) = claims.get(&id) {
        // A window off screen (the lock hid it) keeps its claim but gets nothing.
        if let Some(win) = u16::try_from(w).ok().and_then(|w| g.windows.get(&w)) {
            return if win.hidden { 0 } else { w };
        }
    }
    let w = g.keyboard_window();
    let owner = u16::try_from(w).ok().and_then(|i| g.windows.get(&i)).map(|x| x.owner);
    match owner {
        Some(o) if crate::caps::of(o as u8).has(crate::caps::Caps::GAMEPAD) => w,
        _ => 0,
    }
}

impl Pads {
    /// See [`take_back`]: the holder gets releases and `DISCONNECT` (if
    /// focus is elsewhere), then `EV_GAMEPAD_CLAIM` 0.
    fn take_back(&mut self, sink: &Sender<DisplayEvent>, comp: &Mutex<Compositor>, device: u32, how: &str) {
        let ended: Vec<(u32, u32)> = self.claims.iter().filter(|&(&d, _)| device == 0 || d == device).map(|(&d, &w)| (d, w)).collect();
        for &(dev, window) in &ended {
            self.claims.remove(&dev);
            eprintln!("frescod: pad {dev} taken back from window {window} by {how}");
        }
        for &(dev, _) in &ended {
            let target = pad_target(&self.claims, dev, comp);
            if let Some(p) = self.map.get_mut(&dev) {
                if p.owner.is_some() {
                    pad_follow(sink, dev, p, target);
                }
            }
        }
        for (dev, window) in ended {
            let _ = sink.send(DisplayEvent::Gamepad { window_id: window, kind: control::EV_GAMEPAD_CLAIM, device_id: dev, usage: 0, value: 0 });
        }
    }

    /// See [`guide_held`].
    fn guide_expired(&mut self, sink: &Sender<DisplayEvent>, comp: &Mutex<Compositor>, device: u32, press: u64) {
        if self.guide_down.get(&device) == Some(&press) {
            self.guide_down.remove(&device);
            self.take_back(sink, comp, device, "a long press on Guide");
        }
    }

    /// See [`caps_changed`].
    fn regrant(&mut self, sink: &Sender<DisplayEvent>, comp: &Mutex<Compositor>, client: u8, caps: crate::caps::Caps) {
        let mut ended = Vec::new();
        if !caps.has(crate::caps::Caps::GAMEPAD_EXCLUSIVE) {
            let g = comp.lock().unwrap();
            let mine = |w: u32| u16::try_from(w).ok().and_then(|w| g.windows.get(&w)).is_some_and(|w| w.owner as u8 == client);
            ended = self.claims.iter().filter(|&(_, &w)| mine(w)).map(|(&d, &w)| (d, w)).collect();
        }
        for &(dev, _) in &ended {
            self.claims.remove(&dev);
        }
        for (&id, p) in self.map.iter_mut() {
            if p.owner.is_some() {
                pad_follow(sink, id, p, pad_target(&self.claims, id, comp));
            }
        }
        for (dev, window) in ended {
            let _ = sink.send(DisplayEvent::Gamepad { window_id: window, kind: control::EV_GAMEPAD_CLAIM, device_id: dev, usage: 0, value: 0 });
        }
    }

    /// Hold or give back a pad; see [`claim`].
    #[allow(clippy::too_many_arguments)]
    fn claim(&mut self, sink: &Sender<DisplayEvent>, comp: &Mutex<Compositor>, client: u8, caps: crate::caps::Caps, window: u32, device: u32, on: bool) {
        let (owned, focused) = {
            let g = comp.lock().unwrap();
            let owned = u16::try_from(window).ok().and_then(|w| g.windows.get(&w)).is_some_and(|w| w.owner as u8 == client && window != 0);
            (owned, g.keyboard_window() == window)
        };
        if !owned {
            log::info!("frescod: client {client}: pad claim for window {window}, not its own");
            return;
        }
        let dev = if device == 0 { self.recent.unwrap_or(0) } else { device };
        let tell = |held: bool| {
            let _ = sink.send(DisplayEvent::Gamepad { window_id: window, kind: control::EV_GAMEPAD_CLAIM, device_id: dev, usage: 0, value: i32::from(held) });
        };
        if !on {
            if self.claims.get(&dev) == Some(&window) {
                self.claims.remove(&dev);
                let target = pad_target(&self.claims, dev, comp);
                if let Some(p) = self.map.get_mut(&dev) {
                    if p.owner.is_some() {
                        pad_follow(sink, dev, p, target);
                    }
                }
            }
            tell(false);
            return;
        }
        let why = if !caps.has(crate::caps::Caps::GAMEPAD_EXCLUSIVE) {
            Some("no input.gamepad_exclusive grant")
        } else if !focused {
            Some("window not focused")
        } else if !self.map.contains_key(&dev) {
            Some("no such pad")
        } else if self.claims.get(&dev).is_some_and(|&w| w != window && pad_target(&self.claims, dev, comp) == w) {
            Some("held by another window")
        } else {
            None
        };
        if let Some(why) = why {
            log::info!("frescod: client {client}: claim of pad {dev} for window {window} refused: {why}");
            tell(false);
            return;
        }
        self.claims.insert(dev, window);
        let p = self.map.get_mut(&dev).expect("checked");
        pad_follow(sink, dev, p, window);
        tell(true);
    }
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
    /// Its Guide button's usages ([`guide_usages`]).
    guide: Vec<u32>,
}

fn wire_phase(p: Phase) -> u8 {
    match p {
        Phase::Begin => scroll_phase::BEGIN,
        Phase::Update => scroll_phase::UPDATE,
        Phase::End => scroll_phase::END,
    }
}

/// The wire's modifier byte is the HID one, as the keyboard sends it
/// (fresco-protocol `InputKeyEvent::modifiers`): left Ctrl 0x01, Shift
/// 0x02, Alt 0x04, Gui 0x08, the right-hand keys the same bits << 4.
/// Every client decodes it that way (Pergola, forum-wm, Arca's and
/// Sigillum's sheets, Scrinium); it once went out as shift 1, ctrl 2,
/// alt 4, which turned a real Ctrl into Shift and dropped Gui.
fn wire_mods(hid: u8) -> u8 {
    hid
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
                    let guide = guide_usages(vendor, product);
                    self.pads.lock().unwrap().map.insert(id, Pad { name, vendor, product, guide, ..Pad::default() });
                }
            }
            Message::PadLayout { id, buttons, axes } => {
                let mut pads = self.pads.lock().unwrap();
                let target = pad_target(&pads.claims, id, comp);
                if let Some(p) = pads.map.get_mut(&id) {
                    p.buttons = u16::try_from(buttons.len()).unwrap_or(u16::MAX);
                    p.axes = u16::try_from(axes.len()).unwrap_or(u16::MAX);
                    // Plugged in: the focused window has it from now.
                    pad_follow(sink, id, p, target);
                }
            }
            Message::PadButton { id, usage, down, .. } => {
                let mut pads = self.pads.lock().unwrap();
                let target = pad_target(&pads.claims, id, comp);
                pads.recent = Some(id);
                // The user's override: Guide held on a claimed pad (§4).
                if pads.map.get(&id).is_some_and(|p| p.guide.contains(&usage)) {
                    if down && pads.claims.contains_key(&id) {
                        pads.presses += 1;
                        let press = pads.presses;
                        pads.guide_down.insert(id, press);
                        let hold = guide_hold();
                        let _ = std::thread::Builder::new().name("frescod-guide".into()).spawn(move || {
                            std::thread::sleep(hold);
                            guide_held(id, press);
                        });
                    } else if !down {
                        pads.guide_down.remove(&id);
                    }
                }
                let Some(p) = pads.map.get_mut(&id) else { return };
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
                let mut pads = self.pads.lock().unwrap();
                let target = pad_target(&pads.claims, id, comp);
                pads.recent = Some(id);
                let Some(p) = pads.map.get_mut(&id) else { return };
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
                // Motion is in points: as far across a 2.0 monitor as a
                // 1.0 one for the same hand movement.
                let k = g.scale_at(x, y) as f32 / 1000.0;
                self.moved(sink, &mut g, (x + dx * k).clamp(0.0, w - 1.0), (y + dy * k).clamp(0.0, h - 1.0));
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
                // dy scrolls content down. 8 pt per detent, at any scale.
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
                let mut pads = self.pads.lock().unwrap();
                pads.claims.remove(&id);
                pads.guide_down.remove(&id);
                if pads.recent == Some(id) {
                    pads.recent = None;
                }
                if let Some(mut p) = pads.map.remove(&id) {
                    pad_follow(sink, id, &mut p, 0);
                }
            }
            Message::Hello { .. } => {}
        }
    }

    /// Hand every gamepad to the focused window, if focus moved (a click
    /// or a key usually moves it, so this runs after each), unless a
    /// window holds it.
    fn pads_follow(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) {
        let mut pads = self.pads.lock().unwrap();
        let Pads { map, claims, .. } = &mut *pads;
        for (&id, p) in map.iter_mut() {
            if p.owner.is_some() {
                pad_follow(sink, id, p, pad_target(claims, id, comp));
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
            let mut g = comp.lock().unwrap();
            let (cx, cy) = g.cursor;
            let hit = g.pointer_target(cx, cy);
            // A press in a slot gives its child the keys (limen.md §6).
            if down && button == 1 {
                g.press_focus(hit.0);
            }
            hit
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
        let mut pads = self.pads.lock().unwrap();
        pads.claims.clear();
        for (id, mut p) in std::mem::take(&mut pads.map) {
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
        crate::caps::set(1, crate::caps::Caps::GAMEPAD);
        crate::caps::set(2, crate::caps::Caps::GAMEPAD);
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

    /// fresco-capabilities.md §3-§4: a pad reaches only apps granted
    /// `input.gamepad`; a claim holds it across focus, needs the exclusive
    /// grant and focus, cannot take a held pad, and gives it back to focus.
    #[test]
    fn gamepad_grants_and_exclusive_claims() {
        use crate::caps::{self, Caps};
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (a, b, c) = {
            let mut g = comp.lock().unwrap();
            let a = g.create(3, (100, 100));
            let b = g.create(4, (100, 100));
            let c = g.create(5, (100, 100));
            g.set_focus_no_raise(Some(a));
            (u32::from(a), u32::from(b), u32::from(c))
        };
        caps::set(3, Caps::GAMEPAD.union(Caps::GAMEPAD_EXCLUSIVE));
        caps::set(4, Caps::GAMEPAD);
        caps::set(5, Caps::NONE);
        let (tx, rx) = channel();
        let mut seat = Seat::default();
        let ev = |rx: &std::sync::mpsc::Receiver<DisplayEvent>| -> Vec<(u32, u16, i32)> {
            rx.try_iter()
                .map(|e| match e {
                    DisplayEvent::GamepadConnect { window_id, .. } => (window_id, control::EV_GAMEPAD_CONNECT, 0),
                    DisplayEvent::Gamepad { window_id, kind, value, .. } => (window_id, kind, value),
                    e => panic!("{e:?}"),
                })
                .collect()
        };
        let focus = |w: u32| comp.lock().unwrap().set_focus_no_raise(Some(w as u16));
        let dev = Message::Device { id: 7, bus: tactus::wire::Bus::Usb, classes: vec![tactus::wire::Class::Gamepad], vendor: 1, product: 2, name: "pad".into() };
        seat.handle(dev, &tx, &comp);
        seat.handle(Message::PadLayout { id: 7, buttons: vec![0x0009_0001], axes: vec![] }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_CONNECT, 0)]);
        // c has no input.gamepad: the pad leaves a and goes to no one.
        focus(c);
        seat.handle(Message::PadButton { id: 7, ts_ns: 0, usage: 0x0009_0001, down: true }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_DISCONNECT, 0)]);
        seat.handle(Message::PadButton { id: 7, ts_ns: 0, usage: 0x0009_0001, down: false }, &tx, &comp);
        // a claims the pad it used last (device 0), unfocused: refused.
        let pads = seat.pads.clone();
        let claim = |client: u8, w: u32, on: bool| pads.lock().unwrap().claim(&tx, &comp, client, caps::of(client), w, 0, on);
        claim(3, a, true);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_CLAIM, 0)]);
        // Focused, it holds the pad; it stays when focus moves to b.
        focus(a);
        claim(3, a, true);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_CONNECT, 0), (a, control::EV_GAMEPAD_CLAIM, 1)]);
        focus(b);
        seat.pads_follow(&tx, &comp);
        seat.handle(Message::PadButton { id: 7, ts_ns: 0, usage: 0x0009_0001, down: true }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_BUTTON, 1)]);
        // b has no exclusive grant; and a claim for another app's window
        // gets no answer at all.
        claim(4, b, true);
        claim(4, a, true);
        assert_eq!(ev(&rx), vec![(b, control::EV_GAMEPAD_CLAIM, 0)]);
        // a gives it back: the pad follows focus to b.
        claim(3, a, false);
        assert_eq!(
            ev(&rx),
            vec![
                (a, control::EV_GAMEPAD_BUTTON, 0),
                (a, control::EV_GAMEPAD_DISCONNECT, 0),
                (b, control::EV_GAMEPAD_CONNECT, 0),
                (a, control::EV_GAMEPAD_CLAIM, 0),
            ]
        );
        // A held pad cannot be taken: a holds it, then (as if granted) b,
        // focused, tries.
        focus(a);
        claim(3, a, true);
        let _ = ev(&rx);
        focus(b);
        caps::set(4, Caps::GAMEPAD.union(Caps::GAMEPAD_EXCLUSIVE));
        claim(4, b, true);
        assert_eq!(ev(&rx), vec![(b, control::EV_GAMEPAD_CLAIM, 0)]);
        // The exclusive grant is taken away: the claim ends and the pad
        // follows focus (b); then input.gamepad goes too: b loses it.
        caps::set(3, Caps::GAMEPAD);
        pads.lock().unwrap().regrant(&tx, &comp, 3, Caps::GAMEPAD);
        assert_eq!(
            ev(&rx),
            vec![(a, control::EV_GAMEPAD_DISCONNECT, 0), (b, control::EV_GAMEPAD_CONNECT, 0), (a, control::EV_GAMEPAD_CLAIM, 0)]
        );
        caps::set(4, Caps::NONE);
        pads.lock().unwrap().regrant(&tx, &comp, 4, Caps::NONE);
        assert_eq!(ev(&rx), vec![(b, control::EV_GAMEPAD_DISCONNECT, 0)]);
        // Unplugged: the claim goes with it (a holds it again first).
        caps::set(3, Caps::GAMEPAD.union(Caps::GAMEPAD_EXCLUSIVE));
        focus(a);
        claim(3, a, true);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_CONNECT, 0), (a, control::EV_GAMEPAD_CLAIM, 1)]);
        seat.handle(Message::Gone { id: 7 }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_DISCONNECT, 0)]);
        assert!(seat.pads.lock().unwrap().claims.is_empty());
    }

    /// fresco-capabilities.md §4: only the user takes a claimed pad back,
    /// from Forum's controller menu (`OP_WM_PAD_RELEASE`) or by holding
    /// Guide; either way the holder is told and the pad follows focus.
    #[test]
    fn the_user_takes_a_claimed_pad_back() {
        use crate::caps::{self, Caps};
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (a, b) = {
            let mut g = comp.lock().unwrap();
            let a = g.create(13, (100, 100));
            let b = g.create(14, (100, 100));
            g.set_focus_no_raise(Some(a));
            (u32::from(a), u32::from(b))
        };
        caps::set(13, Caps::GAMEPAD.union(Caps::GAMEPAD_EXCLUSIVE));
        caps::set(14, Caps::GAMEPAD);
        let (tx, rx) = channel();
        let mut seat = Seat::default();
        let ev = |rx: &std::sync::mpsc::Receiver<DisplayEvent>| -> Vec<(u32, u16, i32)> {
            rx.try_iter()
                .map(|e| match e {
                    DisplayEvent::GamepadConnect { window_id, .. } => (window_id, control::EV_GAMEPAD_CONNECT, 0),
                    DisplayEvent::Gamepad { window_id, kind, value, .. } => (window_id, kind, value),
                    e => panic!("{e:?}"),
                })
                .collect()
        };
        let focus = |w: u32| comp.lock().unwrap().set_focus_no_raise(Some(w as u16));
        // A generic pad: Guide is button 17.
        const GUIDE: u32 = 0x0009_0011;
        let dev = Message::Device { id: 9, bus: tactus::wire::Bus::Usb, classes: vec![tactus::wire::Class::Gamepad], vendor: 1, product: 2, name: "pad".into() };
        seat.handle(dev, &tx, &comp);
        seat.handle(Message::PadLayout { id: 9, buttons: vec![0x0009_0001, GUIDE], axes: vec![] }, &tx, &comp);
        let pads = seat.pads.clone();
        let claim = || pads.lock().unwrap().claim(&tx, &comp, 13, caps::of(13), a, 9, true);
        claim();
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_CONNECT, 0), (a, control::EV_GAMEPAD_CLAIM, 1)]);
        focus(b);
        seat.pads_follow(&tx, &comp);
        // Nothing claimed elsewhere: taking back pad 3 does nothing.
        pads.lock().unwrap().take_back(&tx, &comp, 3, "test");
        assert!(ev(&rx).is_empty());
        // The menu takes it back (0: every claimed pad) while a holds button 1.
        seat.handle(Message::PadButton { id: 9, ts_ns: 0, usage: 0x0009_0001, down: true }, &tx, &comp);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_BUTTON, 1)]);
        pads.lock().unwrap().take_back(&tx, &comp, 0, "test");
        assert_eq!(
            ev(&rx),
            vec![
                (a, control::EV_GAMEPAD_BUTTON, 0),
                (a, control::EV_GAMEPAD_DISCONNECT, 0),
                (b, control::EV_GAMEPAD_CONNECT, 0),
                (a, control::EV_GAMEPAD_CLAIM, 0),
            ]
        );
        seat.handle(Message::PadButton { id: 9, ts_ns: 0, usage: 0x0009_0001, down: false }, &tx, &comp);
        assert!(ev(&rx).is_empty(), "b never saw the press, so gets no release");
        // Claimed again, and listed for the WM as held by a.
        focus(a);
        seat.pads_follow(&tx, &comp);
        claim();
        let _ = ev(&rx);
        focus(b);
        seat.pads_follow(&tx, &comp);
        assert_eq!(pads.lock().unwrap().claims.get(&9), Some(&a));
        // A short Guide press does not take it back; the holder gets it.
        let mut press = |down| seat.handle(Message::PadButton { id: 9, ts_ns: 0, usage: GUIDE, down }, &tx, &comp);
        press(true);
        let first = *pads.lock().unwrap().guide_down.get(&9).expect("guide timed");
        press(false);
        pads.lock().unwrap().guide_expired(&tx, &comp, 9, first);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_BUTTON, 1), (a, control::EV_GAMEPAD_BUTTON, 0)]);
        // Held past the time: taken back.
        press(true);
        let second = *pads.lock().unwrap().guide_down.get(&9).expect("guide timed");
        assert_ne!(first, second);
        pads.lock().unwrap().guide_expired(&tx, &comp, 9, first);
        assert_eq!(ev(&rx), vec![(a, control::EV_GAMEPAD_BUTTON, 1)], "a stale timer does nothing");
        pads.lock().unwrap().guide_expired(&tx, &comp, 9, second);
        assert_eq!(
            ev(&rx),
            vec![
                (a, control::EV_GAMEPAD_BUTTON, 0),
                (a, control::EV_GAMEPAD_DISCONNECT, 0),
                (b, control::EV_GAMEPAD_CONNECT, 0),
                (a, control::EV_GAMEPAD_CLAIM, 0),
            ]
        );
        press(false);
        assert!(ev(&rx).is_empty());
        assert!(pads.lock().unwrap().claims.is_empty());
        // Guide on a pad nobody holds is just a button.
        press(true);
        assert!(pads.lock().unwrap().guide_down.is_empty());
        assert_eq!(ev(&rx), vec![(b, control::EV_GAMEPAD_BUTTON, 1)]);
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
