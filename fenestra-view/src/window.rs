//! The windowed viewer (fenestra.md §14): one local window showing the
//! host's whole desktop (§3.4), drawn with softbuffer from the windows'
//! scene streams, and the person's keyboard, pointer and wheel sent to the
//! host as `Control::Input` in the host screen's pixels.
//!
//! Drawing here is the CPU path: each window is rastered by the same code
//! as headless mode when its version moves, composed by the layout, and
//! scaled to the local window. The GPU path comes later.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use fenestra_wire::scene::Input;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::{desktop, Views};

struct App {
    views: Arc<Views>,
    input: tokio::sync::mpsc::Sender<Input>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    /// The host screen's size as last drawn, for the pointer's scale.
    screen: (u32, u32),
    modifiers: u8,
    title: String,
}

impl App {
    fn send(&self, i: Input) {
        // A full queue (the host far behind) drops input rather than block
        // the window.
        let _ = self.input.try_send(i);
    }

    fn draw(&mut self) {
        let (Some(w), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else { return };
        let size = w.inner_size();
        let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surface.resize(ww, wh).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        buf.fill(0x0020_2020);
        if let Some(((sw, sh, px), _)) = desktop(&self.views) {
            if self.screen != (sw, sh) {
                // The first layout, or the host's screen changed: size the
                // window to it (the window manager may not agree).
                self.screen = (sw, sh);
                let _ = w.request_inner_size(PhysicalSize::new(sw, sh));
            }
            // Nearest pixel, the host screen scaled into the window with
            // its aspect kept.
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
        let _ = buf.present();
    }

    /// Window pixels to the host screen's; None outside the drawn screen.
    fn to_host(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let w = self.window.as_ref()?;
        let size = w.inner_size();
        if size.width == 0 || size.height == 0 || self.screen.0 == 0 {
            return None;
        }
        let (ox, oy, dw, dh) = fit(self.screen, (size.width, size.height));
        let (hx, hy) = ((x - f64::from(ox)) * f64::from(self.screen.0) / f64::from(dw), (y - f64::from(oy)) * f64::from(self.screen.1) / f64::from(dh));
        (hx >= 0.0 && hy >= 0.0 && hx < f64::from(self.screen.0) && hy < f64::from(self.screen.1)).then_some((hx as f32, hy as f32))
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(self.title.clone()).with_inner_size(PhysicalSize::new(1280u32, 720u32));
        let w = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("fenestra-view: no window: {e}");
                el.exit();
                return;
            }
        };
        let surface = softbuffer::Context::new(w.clone()).and_then(|c| softbuffer::Surface::new(&c, w.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("fenestra-view: no surface: {e}");
                el.exit();
                return;
            }
        }
        self.window = Some(w);
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _: ()) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((x, y)) = self.to_host(position.x, position.y) {
                    self.send(Input::Motion { x, y });
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

/// Run the window on this thread (the main one: macOS needs it) until it
/// is closed. `views` fills from the session running elsewhere.
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
                let mut seen = (u64::MAX, 0usize);
                loop {
                    std::thread::sleep(Duration::from_millis(8));
                    let now = (views.frames.load(Ordering::Relaxed), views.layout.lock().map(|l| l.as_ref().map_or(0, |l| l.1.len() + 1)).unwrap_or(0));
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
    let mut app = App { views, input, window: None, surface: None, screen: (0, 0), modifiers: 0, title };
    el.run_app(&mut app).map_err(|e| e.to_string())
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
    use super::fit;

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
