use winit::event::{WindowEvent, ElementState, MouseButton};
use winit::keyboard::{KeyCode, PhysicalKey};

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value_a: i32,
    pub value_b: i32,
    pub _pad0: [u32; 13],
}

pub const INPUT_KEY: u16 = 1;
pub const INPUT_MOUSE_MOVE: u16 = 2;
pub const INPUT_MOUSE_BUTTON: u16 = 3;
pub const INPUT_SCROLL: u16 = 4;
pub const INPUT_RESIZE: u16 = 5;

pub struct InputCapture {
    events: Vec<InputEvent>,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub scale: f64,
}

impl InputCapture {
    pub fn new() -> Self {
        Self { events: Vec::new(), cursor_x: 0.0, cursor_y: 0.0, scale: 1.0 }
    }

    pub fn drain(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn handle_winit_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    let pressed = match event.state {
                        ElementState::Pressed => 1,
                        ElementState::Released => 0,
                    };
                    self.events.push(InputEvent {
                        event_type: INPUT_KEY,
                        code: winit_key_to_code(key),
                        value_a: pressed,
                        value_b: 0,
                        _pad0: [0; 13],
                    });
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_x = position.x as f32;
                self.cursor_y = position.y as f32;
                // Send logical coordinates to guest (divide by scale)
                let lx = (position.x / self.scale) as i32;
                let ly = (position.y / self.scale) as i32;
                self.events.push(InputEvent {
                    event_type: INPUT_MOUSE_MOVE,
                    code: 0,
                    value_a: lx,
                    value_b: ly,
                    _pad0: [0; 13],
                });
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let btn = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    _ => 3,
                };
                let pressed = match state {
                    ElementState::Pressed => 1,
                    ElementState::Released => 0,
                };
                self.events.push(InputEvent {
                    event_type: INPUT_MOUSE_BUTTON,
                    code: btn,
                    value_a: pressed,
                    value_b: 0,
                    _pad0: [0; 13],
                });
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (*x as i32, *y as i32),
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                self.events.push(InputEvent {
                    event_type: INPUT_SCROLL,
                    code: 0,
                    value_a: dx,
                    value_b: dy,
                    _pad0: [0; 13],
                });
            }

            _ => {}
        }
    }
}

/// Map winit physical key codes → USB HID usage codes (Page 0x07,
/// Keyboard/Keypad, from the USB-IF HID Usage Tables).
///
/// HID is the cross-platform standard for keyboard scancodes — used
/// by USB keyboards on the wire, FreeBSD's usbhid/hkbd drivers,
/// macOS IOKit HID events, and the Web's KeyboardEvent.code spec.
/// We deliberately do NOT use Linux evdev codes here — the Fresco
/// protocol is meant to be Linux-baggage-free.
fn winit_key_to_code(key: KeyCode) -> u16 {
    match key {
        // Letters: 0x04 = A ... 0x1d = Z
        KeyCode::KeyA => 0x04, KeyCode::KeyB => 0x05, KeyCode::KeyC => 0x06,
        KeyCode::KeyD => 0x07, KeyCode::KeyE => 0x08, KeyCode::KeyF => 0x09,
        KeyCode::KeyG => 0x0a, KeyCode::KeyH => 0x0b, KeyCode::KeyI => 0x0c,
        KeyCode::KeyJ => 0x0d, KeyCode::KeyK => 0x0e, KeyCode::KeyL => 0x0f,
        KeyCode::KeyM => 0x10, KeyCode::KeyN => 0x11, KeyCode::KeyO => 0x12,
        KeyCode::KeyP => 0x13, KeyCode::KeyQ => 0x14, KeyCode::KeyR => 0x15,
        KeyCode::KeyS => 0x16, KeyCode::KeyT => 0x17, KeyCode::KeyU => 0x18,
        KeyCode::KeyV => 0x19, KeyCode::KeyW => 0x1a, KeyCode::KeyX => 0x1b,
        KeyCode::KeyY => 0x1c, KeyCode::KeyZ => 0x1d,
        // Digits: 0x1e = 1 ... 0x26 = 9, 0x27 = 0
        KeyCode::Digit1 => 0x1e, KeyCode::Digit2 => 0x1f,
        KeyCode::Digit3 => 0x20, KeyCode::Digit4 => 0x21,
        KeyCode::Digit5 => 0x22, KeyCode::Digit6 => 0x23,
        KeyCode::Digit7 => 0x24, KeyCode::Digit8 => 0x25,
        KeyCode::Digit9 => 0x26, KeyCode::Digit0 => 0x27,
        // Editing
        KeyCode::Enter        => 0x28,
        KeyCode::Escape       => 0x29,
        KeyCode::Backspace    => 0x2a,
        KeyCode::Tab          => 0x2b,
        KeyCode::Space        => 0x2c,
        // Symbols
        KeyCode::Minus        => 0x2d,
        KeyCode::Equal        => 0x2e,
        KeyCode::BracketLeft  => 0x2f,
        KeyCode::BracketRight => 0x30,
        KeyCode::Backslash    => 0x31,
        KeyCode::Semicolon    => 0x33,
        KeyCode::Quote        => 0x34,
        KeyCode::Backquote    => 0x35,
        KeyCode::Comma        => 0x36,
        KeyCode::Period       => 0x37,
        KeyCode::Slash        => 0x38,
        KeyCode::CapsLock     => 0x39,
        // F1..F12
        KeyCode::F1 => 0x3a, KeyCode::F2 => 0x3b, KeyCode::F3 => 0x3c,
        KeyCode::F4 => 0x3d, KeyCode::F5 => 0x3e, KeyCode::F6 => 0x3f,
        KeyCode::F7 => 0x40, KeyCode::F8 => 0x41, KeyCode::F9 => 0x42,
        KeyCode::F10 => 0x43, KeyCode::F11 => 0x44, KeyCode::F12 => 0x45,
        // Arrows
        KeyCode::ArrowRight => 0x4f,
        KeyCode::ArrowLeft  => 0x50,
        KeyCode::ArrowDown  => 0x51,
        KeyCode::ArrowUp    => 0x52,
        // Modifiers — 0xe0..0xe7
        KeyCode::ControlLeft  => 0xe0,
        KeyCode::ShiftLeft    => 0xe1,
        KeyCode::AltLeft      => 0xe2,
        KeyCode::SuperLeft    => 0xe3,
        KeyCode::ControlRight => 0xe4,
        KeyCode::ShiftRight   => 0xe5,
        KeyCode::AltRight     => 0xe6,
        KeyCode::SuperRight   => 0xe7,
        _ => 0,
    }
}
