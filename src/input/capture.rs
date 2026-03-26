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
}

impl InputCapture {
    pub fn new() -> Self {
        Self { events: Vec::new() }
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
                self.events.push(InputEvent {
                    event_type: INPUT_MOUSE_MOVE,
                    code: 0,
                    value_a: position.x as i32,
                    value_b: position.y as i32,
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

fn winit_key_to_code(key: KeyCode) -> u16 {
    // Map winit key codes to Linux evdev codes (same as KarythraOS input service uses)
    match key {
        KeyCode::Escape => 1,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Minus => 12,
        KeyCode::Equal => 13,
        KeyCode::Backspace => 14,
        KeyCode::Tab => 15,
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::BracketLeft => 26,
        KeyCode::BracketRight => 27,
        KeyCode::Enter => 28,
        KeyCode::ControlLeft => 29,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::Semicolon => 39,
        KeyCode::Quote => 40,
        KeyCode::Backquote => 41,
        KeyCode::ShiftLeft => 42,
        KeyCode::Backslash => 43,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Comma => 51,
        KeyCode::Period => 52,
        KeyCode::Slash => 53,
        KeyCode::ShiftRight => 54,
        KeyCode::AltLeft => 56,
        KeyCode::Space => 57,
        KeyCode::ArrowUp => 103,
        KeyCode::ArrowLeft => 105,
        KeyCode::ArrowRight => 106,
        KeyCode::ArrowDown => 108,
        _ => 0,
    }
}
