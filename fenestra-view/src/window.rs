//! The windowed viewer (fenestra.md §14), in the mode the host's layout
//! says (§3.4):
//!
//! - **whole desktop** (a WM runs on the host): one local window showing
//!   the host's desktop, scaled into it; input goes in the host screen's
//!   pixels.
//! - **per-app** (no WM on the host): one local window per host window,
//!   its size, under a host strip this viewer draws (§11.5); the pointer
//!   goes as a position in that window (`Input::MotionIn`), so host
//!   windows stacked over each other still each get their own clicks.
//!
//! Drawing here is the CPU path: each window is rastered by the same code
//! as headless mode when its version moves. The GPU path comes later.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use fenestra_wire::scene::Input;
use winit::application::ApplicationHandler;
use fenestra_wire::scene::placed_kind;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::{desktop, Views};

/// The host strip over each per-app window, points.
const HOST_STRIP_PT: f64 = 6.0;

struct Win {
    w: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    /// The host window it shows; None for the whole desktop.
    host: Option<u32>,
    /// Per-app: the host window's size as last asked of this one.
    size: (u32, u32),
    /// Per-app pop-ups: the host window it belongs to. A pop-up has no
    /// strip and no frame, and sits against that window's.
    popup_of: Option<u32>,
    /// Per-app: a size the person gave it here and the host was asked
    /// for, and when; layouts with another size do not resize this window
    /// back until the host answers or [`RESIZE_WAIT`] passes (§6.4).
    asked: Option<((u32, u32), std::time::Instant)>,
}

/// How far from its parent's window a pop-up may sit, pixels.
const POPUP_REACH: i64 = 16;

/// A pop-up's size here: the host's, each side at most twice its
/// parent's (a menu may run past its window's bottom, never cover a
/// screen).
fn popup_size((w, h): (u32, u32), (pw, ph): (u32, u32)) -> (u32, u32) {
    (w.min(pw.saturating_mul(2)).max(1), h.min(ph.saturating_mul(2)).max(1))
}

/// A pop-up's offset from its parent's top left: the host's, clamped so
/// the pop-up (`size`) overlaps the parent (`(x, y, w, h)` on the host's
/// screen) or sits within [`POPUP_REACH`] of it.
fn popup_offset((x, y): (i32, i32), (w, h): (u32, u32), (px, py, pw, ph): (i32, i32, u32, u32)) -> (i64, i64) {
    let rx = i64::from(x) - i64::from(px);
    let ry = i64::from(y) - i64::from(py);
    (rx.clamp(-i64::from(w) - POPUP_REACH, i64::from(pw) + POPUP_REACH), ry.clamp(-i64::from(h) - POPUP_REACH, i64::from(ph) + POPUP_REACH))
}

/// How long a local resize waits for the host's answer before the host's
/// size wins.
const RESIZE_WAIT: Duration = Duration::from_secs(2);

struct App {
    views: Arc<Views>,
    input: tokio::sync::mpsc::Sender<Input>,
    wins: HashMap<WindowId, Win>,
    /// The host screen's size as last drawn, for the pointer's scale.
    screen: (u32, u32),
    modifiers: u8,
    title: String,
    /// Per-app windows the person closed here, until the host's layout
    /// drops them.
    dismissed: Vec<u32>,
    /// The mode the windows are in now.
    mode: Option<bool>,
    strip_colour: u32,
}

impl App {
    fn send(&self, i: Input) {
        // A full queue (the host far behind) drops input rather than block
        // the window.
        let _ = self.input.try_send(i);
    }

    fn open(&self, el: &ActiveEventLoop, title: String, size: PhysicalSize<u32>, host: Option<u32>, host_size: (u32, u32), popup_at: Option<PhysicalPosition<i32>>) -> Option<Win> {
        let mut attrs = Window::default_attributes().with_title(title).with_inner_size(size);
        if let Some(at) = popup_at {
            // A pop-up: no frame, at its place, and it never takes the
            // focus from its parent (which would close it on the host).
            attrs = attrs.with_decorations(false).with_active(false).with_position(at);
        }
        let w = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("fenestra-view: no window: {e}");
                return None;
            }
        };
        match softbuffer::Context::new(w.clone()).and_then(|c| softbuffer::Surface::new(&c, w.clone())) {
            Ok(surface) => Some(Win { w, surface, host, size: host_size, popup_of: None, asked: None }),
            Err(e) => {
                eprintln!("fenestra-view: no surface: {e}");
                None
            }
        }
    }

    fn strip(w: &Window) -> u32 {
        (HOST_STRIP_PT * w.scale_factor()).round() as u32
    }

    /// The strip over `win`'s content: a top-level app window's; a
    /// pop-up has none.
    fn strip_of(win: &Win) -> u32 {
        if win.popup_of.is_some() {
            0
        } else {
            Self::strip(&win.w)
        }
    }

    /// Where a pop-up at `(x, y)` on the host's screen, `size` as
    /// [`popup_size`] gives it, goes on this one: its parent's window
    /// here, at the same offset from the parent as on the host, under the
    /// parent's strip. The offset is clamped so the pop-up overlaps the
    /// parent or lies within [`POPUP_REACH`] of it: a host can't put an
    /// undecorated window anywhere else on this screen (§11.5).
    fn popup_position(&self, parent: u32, (x, y): (i32, i32), size: (u32, u32), placed: &[fenestra_wire::scene::Placed]) -> Option<PhysicalPosition<i32>> {
        let p = placed.iter().find(|p| p.0 == parent)?;
        let win = self.wins.values().find(|w| w.host == Some(parent))?;
        let at = win.w.inner_position().ok()?;
        let (rx, ry) = popup_offset((x, y), size, (p.1, p.2, p.3, p.4));
        let to = |base: i32, v: i64| i32::try_from(i64::from(base) + v).ok();
        Some(PhysicalPosition::new(to(at.x, rx)?, to(at.y, i64::from(Self::strip(&win.w)) + ry)?))
    }

    /// The local windows the host's layout calls for: the desktop's, or
    /// one per host window.
    fn reconcile(&mut self, el: &ActiveEventLoop) {
        let mode = self.views.mode();
        let placed: Vec<fenestra_wire::scene::Placed> = self.views.layout.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|l| l.1.clone()).unwrap_or_default();
        match mode {
            None => {}
            Some(true) => {
                self.wins.retain(|_, w| w.host.is_none());
                if self.wins.is_empty() {
                    if let Some(win) = self.open(el, self.title.clone(), PhysicalSize::new(1280, 720), None, (0, 0), None) {
                        let size = win.w.inner_size();
                        // The host's output follows this window (§4.1).
                        self.views.set_output(size.width, size.height, (win.w.scale_factor() * 1000.0).round() as u32);
                        self.wins.insert(win.w.id(), win);
                    }
                }
            }
            Some(false) => {
                if self.mode != Some(false) {
                    // The host's output is this viewer's screen: its windows
                    // may go anywhere on it.
                    if let Some(m) = el.primary_monitor().or_else(|| el.available_monitors().next()) {
                        let s = m.size();
                        self.views.set_output(s.width, s.height, (m.scale_factor() * 1000.0).round() as u32);
                    }
                }
                self.dismissed.retain(|id| placed.iter().any(|p| p.0 == *id));
                // Slots are drawn inside their parent's window, not as one.
                let shown = |id: u32| placed.iter().any(|p| p.0 == id && p.6 != placed_kind::SLOT);
                self.wins.retain(|_, w| w.host.is_some_and(shown));
                for &(id, x, y, pw, ph, parent, kind) in &placed {
                    if self.dismissed.contains(&id) || kind == placed_kind::SLOT {
                        continue;
                    }
                    if kind == placed_kind::POPUP {
                        let Some(of) = placed.iter().find(|p| p.0 == parent) else { continue };
                        let (pw, ph) = popup_size((pw, ph), (of.3, of.4));
                        let at = self.popup_position(parent, (x, y), (pw, ph), &placed);
                        match self.wins.values_mut().find(|w| w.host == Some(id)) {
                            Some(win) => {
                                if win.size != (pw, ph) {
                                    win.size = (pw, ph);
                                    let _ = win.w.request_inner_size(PhysicalSize::new(pw, ph));
                                }
                                if let Some(at) = at {
                                    win.w.set_outer_position(at);
                                }
                            }
                            None => {
                                let Some(at) = at else { continue };
                                if let Some(mut win) = self.open(el, format!("{}: pop-up {id}", self.title), PhysicalSize::new(pw, ph), Some(id), (pw, ph), Some(at)) {
                                    win.popup_of = Some(parent);
                                    self.wins.insert(win.w.id(), win);
                                }
                            }
                        }
                        continue;
                    }
                    match self.wins.values_mut().find(|w| w.host == Some(id)) {
                        Some(win) => {
                            if let Some((want, at)) = win.asked {
                                if want == (pw, ph) || at.elapsed() > RESIZE_WAIT {
                                    win.asked = None;
                                }
                            }
                            if win.asked.is_none() && win.size != (pw, ph) {
                                win.size = (pw, ph);
                                let _ = win.w.request_inner_size(PhysicalSize::new(pw, ph + Self::strip(&win.w)));
                            }
                        }
                        None => {
                            let title = format!("{}: window {id}", self.title);
                            // The strip at this scale is known once the window
                            // is: open it at the window's size, then add it.
                            if let Some(win) = self.open(el, title, PhysicalSize::new(pw, ph), Some(id), (pw, ph), None) {
                                let _ = win.w.request_inner_size(PhysicalSize::new(pw, ph + Self::strip(&win.w)));
                                self.wins.insert(win.w.id(), win);
                            }
                        }
                    }
                }
            }
        }
        self.mode = mode;
    }

    fn draw(&mut self, id: WindowId) {
        let views = self.views.clone();
        let strip_colour = self.strip_colour;
        let Some(win) = self.wins.get_mut(&id) else { return };
        let size = win.w.inner_size();
        let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if win.surface.resize(ww, wh).is_err() {
            return;
        }
        let strip = Self::strip_of(win);
        let Ok(mut buf) = win.surface.buffer_mut() else { return };
        buf.fill(0x0020_2020);
        match win.host {
            None => {
                if let Some(((sw, sh, px), _)) = desktop(&views) {
                    if self.screen != (sw, sh) {
                        // The first layout, or the host's screen changed: size
                        // the window to it (the window manager may not agree).
                        self.screen = (sw, sh);
                        let _ = win.w.request_inner_size(PhysicalSize::new(sw, sh));
                    }
                    // Nearest pixel, the host screen scaled into the window
                    // with its aspect kept.
                    let (ox, oy, dw, dh) = fit((sw, sh), (size.width, size.height));
                    for y in 0..dh {
                        let sy = (u64::from(y) * u64::from(sh) / u64::from(dh)) as usize;
                        let row = ((oy + y) * size.width + ox) as usize;
                        for x in 0..dw {
                            let sx = (u64::from(x) * u64::from(sw) / u64::from(dw)) as usize;
                            buf[row + x as usize] = px[sy * sw as usize + sx] & 0x00ff_ffff;
                        }
                    }
                }
            }
            Some(host) => {
                // The host strip, then the window 1:1 under it.
                for p in buf.iter_mut().take((strip.min(size.height) * size.width) as usize) {
                    *p = strip_colour;
                }
                // The window, then its slots over it where the host has
                // them, each source over.
                let placed: Vec<fenestra_wire::scene::Placed> = views.layout.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|l| l.1.clone()).unwrap_or_default();
                let me = placed.iter().find(|p| p.0 == host).copied();
                let mut layers = vec![(host, 0i64, 0i64)];
                if let Some(me) = me {
                    layers.extend(placed.iter().filter(|p| p.6 == placed_kind::SLOT && p.5 == host).map(|p| (p.0, i64::from(p.1) - i64::from(me.1), i64::from(p.2) - i64::from(me.2))));
                }
                let below = (size.width, size.height.saturating_sub(strip));
                let body = buf.get_mut((strip.min(size.height) * size.width) as usize..).unwrap_or_default();
                for (i, (id, ox, oy)) in layers.into_iter().enumerate() {
                    let v = views.windows.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
                    let Some((w, h, px, _)) = v.and_then(|v| v.lock().unwrap_or_else(|e| e.into_inner()).raster_cached()) else { continue };
                    // The window itself as it is; a slot over it.
                    if i == 0 {
                        crate::blit(body, below, &px, (w, h), (ox, oy), |s, _| s & 0x00ff_ffff);
                    } else {
                        crate::blit(body, below, &px, (w, h), (ox, oy), |s, d| crate::over(s, d | 0xff00_0000) & 0x00ff_ffff);
                    }
                }
                if win.popup_of.is_some() {
                    // A pop-up's edge in the host's colour, so it never
                    // passes for one of this screen's own (§11.5).
                    let e = (win.w.scale_factor().round() as u32).max(1);
                    let (ww, wh) = (size.width, size.height);
                    for y in 0..wh {
                        for x in 0..ww {
                            if x < e || y < e || x + e >= ww || y + e >= wh {
                                buf[(y * ww + x) as usize] = strip_colour;
                            }
                        }
                    }
                }
            }
        }
        let _ = buf.present();
    }

    /// A pointer position in local window `id` as the host takes it.
    fn pointer(&self, id: WindowId, x: f64, y: f64) -> Option<Input> {
        let win = self.wins.get(&id)?;
        let size = win.w.inner_size();
        match win.host {
            Some(host) => {
                let y = y - f64::from(Self::strip_of(win));
                (x >= 0.0 && y >= 0.0 && x < f64::from(win.size.0) && y < f64::from(win.size.1)).then_some(Input::MotionIn { window: host, x: x as f32, y: y as f32 })
            }
            None => {
                if size.width == 0 || size.height == 0 || self.screen.0 == 0 {
                    return None;
                }
                let (ox, oy, dw, dh) = fit(self.screen, (size.width, size.height));
                let (hx, hy) = ((x - f64::from(ox)) * f64::from(self.screen.0) / f64::from(dw), (y - f64::from(oy)) * f64::from(self.screen.1) / f64::from(dh));
                (hx >= 0.0 && hy >= 0.0 && hx < f64::from(self.screen.0) && hy < f64::from(self.screen.1)).then_some(Input::Motion { x: hx as f32, y: hy as f32 })
            }
        }
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.reconcile(el);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        self.reconcile(el);
        for w in self.wins.values() {
            w.w.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => match self.wins.get(&id).and_then(|w| w.host) {
                // Closing an app's window here hides it until the host
                // closes it; the last one ends the viewer.
                Some(host) => {
                    self.dismissed.push(host);
                    self.wins.remove(&id);
                    if self.wins.is_empty() {
                        el.exit();
                    }
                }
                None => el.exit(),
            },
            WindowEvent::RedrawRequested => self.draw(id),
            WindowEvent::Resized(size) => {
                let mut ask = None;
                if let Some(win) = self.wins.get_mut(&id) {
                    match win.host {
                        // The host's output follows the desktop's window (§4.1).
                        None => self.views.set_output(size.width, size.height, (win.w.scale_factor() * 1000.0).round() as u32),
                        // The person resized an app's window: the app is
                        // asked for that size; until it answers, its last
                        // frame stays at the top left, clipped or padded.
                        Some(_) if win.popup_of.is_some() => {}
                        Some(host) => {
                            let want = (size.width, size.height.saturating_sub(Self::strip(&win.w)));
                            if want != win.size && want.0 > 0 && want.1 > 0 {
                                win.size = want;
                                win.asked = Some((want, std::time::Instant::now()));
                                ask = Some(Input::Resize { window: host, width: want.0, height: want.1 });
                            }
                        }
                    }
                    win.w.request_redraw();
                }
                if let Some(i) = ask {
                    self.send(i);
                }
            }
            WindowEvent::Focused(true) => {
                // Keys go to the app whose window the person is in.
                if let Some(host) = self.wins.get(&id).and_then(|w| w.host) {
                    self.send(Input::Focus { window: host });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(i) = self.pointer(id, position.x, position.y) {
                    self.send(i);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 3,
                    MouseButton::Back => 4,
                    MouseButton::Forward => 5,
                    MouseButton::Other(_) => return,
                };
                self.send(Input::Button { button, pressed: state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 40.0, y * 40.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                self.send(Input::Scroll { dx, dy });
            }
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                self.modifiers = (s.contains(ModifiersState::CONTROL) as u8)
                    | (s.contains(ModifiersState::SHIFT) as u8) << 1
                    | (s.contains(ModifiersState::ALT) as u8) << 2
                    | (s.contains(ModifiersState::SUPER) as u8) << 3;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // The host repeats held keys itself.
                if event.repeat {
                    return;
                }
                if let PhysicalKey::Code(c) = event.physical_key {
                    if let Some(usage) = hid_usage(c) {
                        self.send(Input::Key { usage, pressed: event.state == ElementState::Pressed, modifiers: self.modifiers });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Run the windows on this thread (the main one: macOS needs it) until
/// the person closes them. `views` fills from the session running
/// elsewhere.
pub fn run(views: Arc<Views>, input: tokio::sync::mpsc::Sender<Input>, title: String) -> Result<(), String> {
    let el = EventLoop::<()>::with_user_event().build().map_err(|e| e.to_string())?;
    el.set_control_flow(ControlFlow::Wait);
    let proxy = el.create_proxy();
    {
        // Redraw when a frame or the layout arrives.
        let views = views.clone();
        std::thread::Builder::new()
            .name("fenestra-view-redraw".into())
            .spawn(move || {
                let mut seen = (u64::MAX, 0u64);
                loop {
                    std::thread::sleep(Duration::from_millis(8));
                    let now = (views.frames.load(Ordering::Relaxed), views.layout_gen.load(Ordering::Relaxed));
                    if now != seen {
                        seen = now;
                        if proxy.send_event(()).is_err() {
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
    }
    let strip_colour = host_colour(&title);
    let mut app = App { views, input, wins: HashMap::new(), screen: (0, 0), modifiers: 0, title, dismissed: Vec::new(), mode: None, strip_colour };
    el.run_app(&mut app).map_err(|e| e.to_string())
}

/// The host strip's colour (0RGB): from the host's name until the person
/// can choose one (§11.5), dark enough for light text.
fn host_colour(name: &str) -> u32 {
    let h = name.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
    let c = |s: u32| 0x30 + ((h >> s) & 0x3f);
    c(0) << 16 | c(8) << 8 | c(16)
}

/// The HID usage (keyboard page) for a key, by its position.
fn hid_usage(c: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match c {
        KeyA => 0x04, KeyB => 0x05, KeyC => 0x06, KeyD => 0x07, KeyE => 0x08, KeyF => 0x09, KeyG => 0x0a,
        KeyH => 0x0b, KeyI => 0x0c, KeyJ => 0x0d, KeyK => 0x0e, KeyL => 0x0f, KeyM => 0x10, KeyN => 0x11,
        KeyO => 0x12, KeyP => 0x13, KeyQ => 0x14, KeyR => 0x15, KeyS => 0x16, KeyT => 0x17, KeyU => 0x18,
        KeyV => 0x19, KeyW => 0x1a, KeyX => 0x1b, KeyY => 0x1c, KeyZ => 0x1d,
        Digit1 => 0x1e, Digit2 => 0x1f, Digit3 => 0x20, Digit4 => 0x21, Digit5 => 0x22,
        Digit6 => 0x23, Digit7 => 0x24, Digit8 => 0x25, Digit9 => 0x26, Digit0 => 0x27,
        Enter => 0x28, Escape => 0x29, Backspace => 0x2a, Tab => 0x2b, Space => 0x2c,
        Minus => 0x2d, Equal => 0x2e, BracketLeft => 0x2f, BracketRight => 0x30, Backslash => 0x31,
        Semicolon => 0x33, Quote => 0x34, Backquote => 0x35, Comma => 0x36, Period => 0x37, Slash => 0x38,
        CapsLock => 0x39,
        F1 => 0x3a, F2 => 0x3b, F3 => 0x3c, F4 => 0x3d, F5 => 0x3e, F6 => 0x3f,
        F7 => 0x40, F8 => 0x41, F9 => 0x42, F10 => 0x43, F11 => 0x44, F12 => 0x45,
        Insert => 0x49, Home => 0x4a, PageUp => 0x4b, Delete => 0x4c, End => 0x4d, PageDown => 0x4e,
        ArrowRight => 0x4f, ArrowLeft => 0x50, ArrowDown => 0x51, ArrowUp => 0x52,
        ControlLeft => 0xe0, ShiftLeft => 0xe1, AltLeft => 0xe2, SuperLeft => 0xe3,
        ControlRight => 0xe4, ShiftRight => 0xe5, AltRight => 0xe6, SuperRight => 0xe7,
        _ => return None,
    })
}

/// Where a screen of `s` is drawn in a window of `w`: offset and size,
/// its aspect kept.
fn fit(s: (u32, u32), w: (u32, u32)) -> (u32, u32, u32, u32) {
    let (sw, sh, ww, wh) = (u64::from(s.0.max(1)), u64::from(s.1.max(1)), u64::from(w.0), u64::from(w.1));
    let (dw, dh) = if ww * sh <= wh * sw { (ww, ww * sh / sw) } else { (wh * sw / sh, wh) };
    let (dw, dh) = (dw.max(1).min(ww), dh.max(1).min(wh));
    (((ww - dw) / 2) as u32, ((wh - dh) / 2) as u32, dw as u32, dh as u32)
}

#[cfg(test)]
mod tests {
    use super::{fit, popup_offset, popup_size, POPUP_REACH};

    #[test]
    fn a_pop_up_stays_by_its_parent_and_no_larger_than_twice_it() {
        let parent = (100, 50, 900, 600);
        // The host's offset where it is near the parent.
        assert_eq!(popup_offset((656, 50), (383, 696), parent), (556, 0));
        assert_eq!(popup_offset((90, 40), (20, 20), parent), (-10, -10));
        // Far off, or at the ends of an i32: pulled back to touching
        // distance, with no overflow.
        assert_eq!(popup_offset((i32::MAX, i32::MIN), (383, 696), parent), (900 + POPUP_REACH, -696 - POPUP_REACH));
        assert_eq!(popup_offset((i32::MIN, i32::MAX), (10, 10), (i32::MAX, i32::MIN, 1, 1)), (-10 - POPUP_REACH, 1 + POPUP_REACH));
        // A menu past its window's bottom keeps its size; a screen-sized
        // one is cut to twice its parent.
        assert_eq!(popup_size((383, 696), (900, 600)), (383, 696));
        assert_eq!(popup_size((8192, 8192), (300, 200)), (600, 400));
    }

    #[test]
    fn fit_keeps_the_aspect_and_centres() {
        assert_eq!(fit((1280, 720), (1280, 720)), (0, 0, 1280, 720));
        // A wider window: bars left and right.
        assert_eq!(fit((1280, 720), (1600, 720)), (160, 0, 1280, 720));
        // A taller one: bars above and below.
        assert_eq!(fit((1280, 720), (640, 720)), (0, 180, 640, 360));
        // Degenerate sizes stay inside the window.
        let (ox, oy, dw, dh) = fit((0, 0), (10, 10));
        assert!(ox + dw <= 10 && oy + dh <= 10 && dw >= 1 && dh >= 1);
    }
}
