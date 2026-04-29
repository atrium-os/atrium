//! USB-HID-driven keymap → editor action.
//!
//! Returns a high-level `Action` rather than raw bytes (as atrium-term
//! does) — the editor consumes structured commands, not a TTY stream.

const HID_A:           u16 = 0x04;
const HID_Z:           u16 = 0x1d;
const HID_1:           u16 = 0x1e;
const HID_0:           u16 = 0x27;
const HID_ENTER:       u16 = 0x28;
const HID_ESCAPE:      u16 = 0x29;
const HID_BACKSPACE:   u16 = 0x2a;
const HID_TAB:         u16 = 0x2b;
const HID_SPACE:       u16 = 0x2c;
const HID_MINUS:       u16 = 0x2d;
const HID_EQUAL:       u16 = 0x2e;
const HID_LBRACKET:    u16 = 0x2f;
const HID_RBRACKET:    u16 = 0x30;
const HID_BACKSLASH:   u16 = 0x31;
const HID_SEMICOLON:   u16 = 0x33;
const HID_QUOTE:       u16 = 0x34;
const HID_BACKTICK:    u16 = 0x35;
const HID_COMMA:       u16 = 0x36;
const HID_PERIOD:      u16 = 0x37;
const HID_SLASH:       u16 = 0x38;
const HID_ARROW_R:     u16 = 0x4f;
const HID_ARROW_L:     u16 = 0x50;
const HID_ARROW_D:     u16 = 0x51;
const HID_ARROW_U:     u16 = 0x52;
const HID_LSHIFT:      u16 = 0xe1;
const HID_RSHIFT:      u16 = 0xe5;
const HID_LCTRL:       u16 = 0xe0;
const HID_RCTRL:       u16 = 0xe4;

#[derive(Debug, Clone)]
pub enum Action {
    Insert(char),
    Newline,
    Backspace,
    Tab,
    Left, Right, Up, Down,
    Save,
    Quit,
}

pub struct Keymap {
    pub shift: bool,
    pub ctrl:  bool,
}

impl Keymap {
    pub fn new() -> Self { Self { shift: false, ctrl: false } }

    pub fn handle(&mut self, code: u16, pressed: bool) -> Option<Action> {
        match code {
            HID_LSHIFT | HID_RSHIFT => { self.shift = pressed; None }
            HID_LCTRL  | HID_RCTRL  => { self.ctrl  = pressed; None }
            _ if !pressed => None,
            HID_ENTER     => Some(Action::Newline),
            HID_BACKSPACE => Some(Action::Backspace),
            HID_TAB       => Some(Action::Tab),
            HID_SPACE     => Some(Action::Insert(' ')),
            HID_ESCAPE    => Some(Action::Quit),
            HID_ARROW_L   => Some(Action::Left),
            HID_ARROW_R   => Some(Action::Right),
            HID_ARROW_U   => Some(Action::Up),
            HID_ARROW_D   => Some(Action::Down),
            _ => self.printable_or_ctrl(code),
        }
    }

    fn printable_or_ctrl(&self, code: u16) -> Option<Action> {
        if (HID_A..=HID_Z).contains(&code) {
            let base = (code - HID_A) as u8;
            if self.ctrl {
                // Ctrl combos that the editor cares about.
                return match (b'a' + base) as char {
                    's' => Some(Action::Save),
                    'q' => Some(Action::Quit),
                    _   => None,
                };
            }
            let c = if self.shift { b'A' + base } else { b'a' + base } as char;
            return Some(Action::Insert(c));
        }
        if (HID_1..=HID_0).contains(&code) {
            let lo = b"1234567890";
            let hi = b"!@#$%^&*()";
            let i = (code - HID_1) as usize;
            return Some(Action::Insert((if self.shift { hi[i] } else { lo[i] }) as char));
        }
        let (lo, hi) = match code {
            HID_MINUS     => (b'-',  b'_'),
            HID_EQUAL     => (b'=',  b'+'),
            HID_LBRACKET  => (b'[',  b'{'),
            HID_RBRACKET  => (b']',  b'}'),
            HID_BACKSLASH => (b'\\', b'|'),
            HID_SEMICOLON => (b';',  b':'),
            HID_QUOTE     => (b'\'', b'"'),
            HID_BACKTICK  => (b'`',  b'~'),
            HID_COMMA     => (b',',  b'<'),
            HID_PERIOD    => (b'.',  b'>'),
            HID_SLASH     => (b'/',  b'?'),
            _ => return None,
        };
        Some(Action::Insert((if self.shift { hi } else { lo }) as char))
    }
}
