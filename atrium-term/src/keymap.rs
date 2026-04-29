//! USB HID Usage Code → ASCII byte translation (US ANSI layout).
//!
//! The Fresco wire protocol carries USB HID Keyboard/Keypad usage
//! codes (Usage Page 0x07) — *not* Linux evdev codes. HID is the
//! cross-platform standard: USB keyboards send these on the wire,
//! FreeBSD's usbhid driver speaks them natively, and macOS IOKit
//! HID events use them too.
//!
//! Reference: USB HID Usage Tables, Section 10
//! (https://usb.org/document-library/hid-usage-tables-15)

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
const HID_LSHIFT:      u16 = 0xe1;
const HID_RSHIFT:      u16 = 0xe5;
const HID_LCTRL:       u16 = 0xe0;
const HID_RCTRL:       u16 = 0xe4;

pub struct Keymap {
    shift: bool,
    ctrl:  bool,
}

impl Keymap {
    pub fn new() -> Self { Self { shift: false, ctrl: false } }

    /// Process a key event. `pressed` true = key-down, false = key-up.
    /// Returns Some(bytes) to write to the pty.
    pub fn handle(&mut self, code: u16, pressed: bool) -> Option<Vec<u8>> {
        match code {
            HID_LSHIFT | HID_RSHIFT => { self.shift = pressed; None }
            HID_LCTRL  | HID_RCTRL  => { self.ctrl  = pressed; None }
            _ if !pressed => None,
            HID_ENTER     => Some(vec![b'\r']),
            HID_BACKSPACE => Some(vec![0x7f]),
            HID_TAB       => Some(vec![b'\t']),
            HID_SPACE     => Some(vec![b' ']),
            HID_ESCAPE    => Some(vec![0x1b]),
            _ => self.printable(code).map(|b| vec![b]),
        }
    }

    fn printable(&self, code: u16) -> Option<u8> {
        // Letters
        if (HID_A..=HID_Z).contains(&code) {
            let base = (code - HID_A) as u8;
            if self.ctrl {
                // Ctrl+letter → 0x01..0x1a
                return Some(base + 1);
            }
            return Some(if self.shift { b'A' + base } else { b'a' + base });
        }
        // Digit row
        if (HID_1..=HID_0).contains(&code) {
            // HID order: 1, 2, 3, 4, 5, 6, 7, 8, 9, 0
            let lo = b"1234567890";
            let hi = b"!@#$%^&*()";
            let i = (code - HID_1) as usize;
            return Some(if self.shift { hi[i] } else { lo[i] });
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
        Some(if self.shift { hi } else { lo })
    }
}
