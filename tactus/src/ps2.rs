//! PS/2 sources (docs/spec/atrium-input.md §3.2): the i8042 keyboard's
//! scancodes and the psm pointer's packets, turned into the same
//! [`Input`] events as a HID report, by HID usage.
//!
//! - [`Set1`]: scancode set 1, which is what `atkbd` gives in `K_RAW`
//!   with the controller translating (the PC default). Translated to HID
//!   with the scancode table Microsoft publishes for its keyboard stack
//!   ("Keyboard Scan Code Specification"), not via Linux keycodes. The
//!   keyboard's own typematic repeats are dropped: Tactus repeats keys
//!   itself (§5.4).
//! - [`SysMouse`]: psm at `MOUSE_SETLEVEL 1`, the 8-byte sysmouse packet
//!   psm makes from whatever the mouse speaks (PS/2, IntelliMouse,
//!   Explorer, and a touchpad as a plain pointer).

use crate::decode::{Axis, Input};
use crate::hid::{usage, Usage};

/// What turns a source's bytes into [`Input`]: a HID [`crate::decode::Decoder`]
/// or one of the PS/2 decoders here.
pub trait Source {
    /// One read's worth of bytes (one HID report; any number of PS/2
    /// bytes or packets).
    fn report(&mut self, bytes: &[u8], out: &mut Vec<Input>);
    /// Release everything held (the device went away or lost input).
    fn release_all(&mut self, out: &mut Vec<Input>);
}

impl Source for crate::decode::Decoder {
    fn report(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        crate::decode::Decoder::report(self, bytes, out)
    }
    fn release_all(&mut self, out: &mut Vec<Input>) {
        crate::decode::Decoder::release_all(self, out)
    }
}

const fn kb(id: u16) -> Usage {
    usage(0x07, id)
}
const fn cons(id: u16) -> Usage {
    usage(0x0c, id)
}
const fn sys(id: u16) -> Usage {
    usage(0x01, id)
}

/// Set 1 make code (no prefix) to HID usage; 0 when unused.
fn base(code: u8) -> Usage {
    const T: [u16; 0x80] = {
        let mut t = [0u16; 0x80];
        t[0x01] = 0x29; // Esc
        let mut i = 0;
        while i < 10 {
            t[0x02 + i] = 0x1e + i as u16; // 1..9, 0
            i += 1;
        }
        t[0x0c] = 0x2d; // -
        t[0x0d] = 0x2e; // =
        t[0x0e] = 0x2a; // Backspace
        t[0x0f] = 0x2b; // Tab
        let q = [0x14, 0x1a, 0x08, 0x15, 0x17, 0x1c, 0x18, 0x0c, 0x12, 0x13]; // QWERTYUIOP
        let mut i = 0;
        while i < 10 {
            t[0x10 + i] = q[i];
            i += 1;
        }
        t[0x1a] = 0x2f; // [
        t[0x1b] = 0x30; // ]
        t[0x1c] = 0x28; // Enter
        t[0x1d] = 0xe0; // LCtrl
        let a = [0x04, 0x16, 0x07, 0x09, 0x0a, 0x0b, 0x0d, 0x0e, 0x0f]; // ASDFGHJKL
        let mut i = 0;
        while i < 9 {
            t[0x1e + i] = a[i];
            i += 1;
        }
        t[0x27] = 0x33; // ;
        t[0x28] = 0x34; // '
        t[0x29] = 0x35; // `
        t[0x2a] = 0xe1; // LShift
        t[0x2b] = 0x31; // \ (US) / # (ISO)
        let z = [0x1d, 0x1b, 0x06, 0x19, 0x05, 0x11, 0x10]; // ZXCVBNM
        let mut i = 0;
        while i < 7 {
            t[0x2c + i] = z[i];
            i += 1;
        }
        t[0x33] = 0x36; // ,
        t[0x34] = 0x37; // .
        t[0x35] = 0x38; // /
        t[0x36] = 0xe5; // RShift
        t[0x37] = 0x55; // KP *
        t[0x38] = 0xe2; // LAlt
        t[0x39] = 0x2c; // Space
        t[0x3a] = 0x39; // Caps Lock
        let mut i = 0;
        while i < 10 {
            t[0x3b + i] = 0x3a + i as u16; // F1..F10
            i += 1;
        }
        t[0x45] = 0x53; // Num Lock
        t[0x46] = 0x47; // Scroll Lock
        t[0x47] = 0x5f; // KP 7
        t[0x48] = 0x60; // KP 8
        t[0x49] = 0x61; // KP 9
        t[0x4a] = 0x56; // KP -
        t[0x4b] = 0x5c; // KP 4
        t[0x4c] = 0x5d; // KP 5
        t[0x4d] = 0x5e; // KP 6
        t[0x4e] = 0x57; // KP +
        t[0x4f] = 0x59; // KP 1
        t[0x50] = 0x5a; // KP 2
        t[0x51] = 0x5b; // KP 3
        t[0x52] = 0x62; // KP 0
        t[0x53] = 0x63; // KP .
        t[0x54] = 0x46; // SysRq (Alt+Print Screen)
        t[0x56] = 0x64; // ISO \ |
        t[0x57] = 0x44; // F11
        t[0x58] = 0x45; // F12
        t[0x59] = 0x67; // KP =
        let mut i = 0;
        while i < 11 {
            t[0x64 + i] = 0x68 + i as u16; // F13..F23
            i += 1;
        }
        t[0x70] = 0x88; // Katakana/Hiragana (International 2)
        t[0x73] = 0x87; // Ro (International 1)
        t[0x76] = 0x73; // F24
        t[0x79] = 0x8a; // Henkan (International 4)
        t[0x7b] = 0x8b; // Muhenkan (International 5)
        t[0x7d] = 0x89; // Yen (International 3)
        t[0x7e] = 0x85; // KP , (Brazil)
        t
    };
    match T[usize::from(code & 0x7f)] {
        0 => 0,
        u => kb(u),
    }
}

/// Set 1 make code after an E0 prefix to HID usage; 0 when unused
/// (including the fake shifts some keyboards wrap around keys).
fn extended(code: u8) -> Usage {
    match code & 0x7f {
        0x1c => kb(0x58), // KP Enter
        0x1d => kb(0xe4), // RCtrl
        0x35 => kb(0x54), // KP /
        0x37 => kb(0x46), // Print Screen
        0x38 => kb(0xe6), // RAlt / AltGr
        0x46 => kb(0x48), // Ctrl+Break: Pause
        0x47 => kb(0x4a), // Home
        0x48 => kb(0x52), // Up
        0x49 => kb(0x4b), // Page Up
        0x4b => kb(0x50), // Left
        0x4d => kb(0x4f), // Right
        0x4f => kb(0x4d), // End
        0x50 => kb(0x51), // Down
        0x51 => kb(0x4e), // Page Down
        0x52 => kb(0x49), // Insert
        0x53 => kb(0x4c), // Delete
        0x5b => kb(0xe3), // LGui
        0x5c => kb(0xe7), // RGui
        0x5d => kb(0x65), // Application (menu)
        0x5e => sys(0x81), // System Power Down
        0x5f => sys(0x82), // System Sleep
        0x63 => sys(0x83), // System Wake Up
        0x10 => cons(0xb6), // Scan Previous Track
        0x19 => cons(0xb5), // Scan Next Track
        0x20 => cons(0xe2), // Mute
        0x21 => cons(0x192), // AL Calculator
        0x22 => cons(0xcd), // Play/Pause
        0x24 => cons(0xb7), // Stop
        0x2e => cons(0xea), // Volume Down
        0x30 => cons(0xe9), // Volume Up
        0x32 => cons(0x223), // AC Home
        0x65 => cons(0x221), // AC Search
        0x66 => cons(0x22a), // AC Bookmarks
        0x67 => cons(0x227), // AC Refresh
        0x68 => cons(0x226), // AC Stop
        0x69 => cons(0x225), // AC Forward
        0x6a => cons(0x224), // AC Back
        0x6b => cons(0x194), // AL Local Machine Browser
        0x6c => cons(0x18a), // AL Email Reader
        0x6d => cons(0x183), // AL Consumer Control Configuration
        _ => 0,
    }
}

/// Scancode set 1 from `atkbd` in `K_RAW`.
#[derive(Debug, Default)]
pub struct Set1 {
    /// E0 seen: the next byte is an extended key.
    e0: bool,
    /// Bytes still to skip of an E1 (Pause) sequence.
    e1: u8,
    held: Vec<Usage>,
}

impl Set1 {
    pub fn new() -> Set1 {
        Set1::default()
    }

    pub fn byte(&mut self, b: u8, out: &mut Vec<Input>) {
        if self.e1 > 0 {
            // Pause: E1 1D 45 E1 9D C5, make and break together and no
            // repeat; sent as a press and a release.
            self.e1 -= 1;
            if self.e1 == 0 {
                out.push(Input::Key { usage: kb(0x48), down: true });
                out.push(Input::Key { usage: kb(0x48), down: false });
            }
            return;
        }
        match b {
            0xe0 => {
                self.e0 = true;
                return;
            }
            0xe1 => {
                self.e1 = 5;
                self.e0 = false;
                return;
            }
            // Korean Hanja / Hangul: one byte, no break code.
            0xf1 | 0xf2 => {
                let u = kb(if b == 0xf1 { 0x91 } else { 0x90 });
                out.push(Input::Key { usage: u, down: true });
                out.push(Input::Key { usage: u, down: false });
                self.e0 = false;
                return;
            }
            // Overrun, and the keyboard's replies (ACK, resend, errors).
            // 0xAA is also its self-test pass, but that is LShift's
            // break, which goes through as one.
            0x00 if !self.e0 => return,
            0xfa | 0xfc | 0xfd | 0xfe | 0xff => {
                self.e0 = false;
                return;
            }
            _ => {}
        }
        let u = if std::mem::take(&mut self.e0) { extended(b) } else { base(b) };
        if u == 0 {
            return;
        }
        let down = b & 0x80 == 0;
        let held = self.held.iter().position(|&h| h == u);
        match (down, held) {
            (true, None) => self.held.push(u),
            // The keyboard's own typematic repeat.
            (true, Some(_)) => return,
            (false, Some(i)) => {
                self.held.swap_remove(i);
            }
            (false, None) => return,
        }
        out.push(Input::Key { usage: u, down });
    }
}

impl Source for Set1 {
    fn report(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        for &b in bytes {
            self.byte(b, out);
        }
    }
    fn release_all(&mut self, out: &mut Vec<Input>) {
        self.e0 = false;
        self.e1 = 0;
        for u in self.held.drain(..) {
            out.push(Input::Key { usage: u, down: false });
        }
    }
}

/// psm's sysmouse packets (`MOUSE_SETLEVEL 1`, 8 bytes): byte 0 is
/// `1000 0LMR` with the buttons inverted (0 is pressed), 1+3 and 2+4 the
/// x and y movement in two halves (y up), 5+6 the wheel as two 7-bit
/// halves (positive towards the user), 7 buttons 4..10, inverted.
#[derive(Debug, Default)]
pub struct SysMouse {
    buf: Vec<u8>,
    /// Buttons held, bit n = HID button n + 1.
    buttons: u16,
}

pub const SYSMOUSE_PACKET: usize = 8;

impl SysMouse {
    pub fn new() -> SysMouse {
        SysMouse::default()
    }

    fn packet(&mut self, p: &[u8; SYSMOUSE_PACKET], out: &mut Vec<Input>) {
        let i8_ = |b: u8| i32::from(b as i8);
        let dx = i8_(p[1]) + i8_(p[3]);
        let dy = -(i8_(p[2]) + i8_(p[4]));
        let dz = (i8_(p[5] << 1) + i8_(p[6] << 1)) >> 1;
        let std = !p[0] & 0x07;
        let mut now = 0u16;
        // Left, right, middle are HID buttons 1, 2, 3.
        if std & 0x04 != 0 {
            now |= 1;
        }
        if std & 0x01 != 0 {
            now |= 2;
        }
        if std & 0x02 != 0 {
            now |= 4;
        }
        now |= u16::from(!p[7] & 0x7f) << 3;
        if dx != 0 || dy != 0 {
            out.push(Input::Motion { dx, dy });
        }
        let changed = now ^ self.buttons;
        for n in 0..10u16 {
            if changed & (1 << n) != 0 {
                out.push(Input::Button { usage: usage(0x09, n + 1), down: now & (1 << n) != 0 });
            }
        }
        self.buttons = now;
        if dz != 0 {
            out.push(Input::Scroll { axis: Axis::Vertical, detents: -dz });
        }
    }
}

impl Source for SysMouse {
    fn report(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        for &b in bytes {
            // Resynchronise on the first byte's fixed bits.
            if self.buf.is_empty() && b & 0xf8 != 0x80 {
                continue;
            }
            self.buf.push(b);
            if self.buf.len() == SYSMOUSE_PACKET {
                let p: [u8; SYSMOUSE_PACKET] = self.buf[..].try_into().expect("8");
                self.buf.clear();
                self.packet(&p, out);
            }
        }
    }
    fn release_all(&mut self, out: &mut Vec<Input>) {
        self.buf.clear();
        for n in 0..10u16 {
            if self.buttons & (1 << n) != 0 {
                out.push(Input::Button { usage: usage(0x09, n + 1), down: false });
            }
        }
        self.buttons = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<(Usage, bool)> {
        let mut k = Set1::new();
        let mut out = Vec::new();
        k.report(bytes, &mut out);
        out.into_iter()
            .map(|e| match e {
                Input::Key { usage, down } => (usage, down),
                e => panic!("{e:?}"),
            })
            .collect()
    }

    #[test]
    fn set1_keys() {
        // Shift+a, typematic repeat of a dropped.
        assert_eq!(
            keys(&[0x2a, 0x1e, 0x1e, 0x1e, 0x9e, 0xaa]),
            vec![(kb(0xe1), true), (kb(0x04), true), (kb(0x04), false), (kb(0xe1), false)]
        );
        // Letters, digits, F keys, keypad.
        assert_eq!(keys(&[0x10, 0x90])[0], (kb(0x14), true)); // Q
        assert_eq!(keys(&[0x32])[0], (kb(0x10), true)); // M
        assert_eq!(keys(&[0x0b])[0], (kb(0x27), true)); // 0
        assert_eq!(keys(&[0x44])[0], (kb(0x43), true)); // F10
        assert_eq!(keys(&[0x58])[0], (kb(0x45), true)); // F12
        assert_eq!(keys(&[0x52])[0], (kb(0x62), true)); // KP 0
        // Extended: arrows, right-hand modifiers, Gui, KP Enter, Delete.
        assert_eq!(keys(&[0xe0, 0x48, 0xe0, 0xc8]), vec![(kb(0x52), true), (kb(0x52), false)]);
        assert_eq!(keys(&[0xe0, 0x1d])[0], (kb(0xe4), true));
        assert_eq!(keys(&[0xe0, 0x38])[0], (kb(0xe6), true));
        assert_eq!(keys(&[0xe0, 0x5b])[0], (kb(0xe3), true));
        assert_eq!(keys(&[0xe0, 0x1c])[0], (kb(0x58), true));
        assert_eq!(keys(&[0xe0, 0x53])[0], (kb(0x4c), true));
        // Print Screen with its fake shift: only the key.
        assert_eq!(keys(&[0xe0, 0x2a, 0xe0, 0x37, 0xe0, 0xb7, 0xe0, 0xaa]), vec![(kb(0x46), true), (kb(0x46), false)]);
        // Pause.
        assert_eq!(keys(&[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]), vec![(kb(0x48), true), (kb(0x48), false)]);
        // Media keys go to the consumer page, power to system control.
        assert_eq!(keys(&[0xe0, 0x30, 0xe0, 0xb0]), vec![(cons(0xe9), true), (cons(0xe9), false)]);
        assert_eq!(keys(&[0xe0, 0x22])[0], (cons(0xcd), true));
        assert_eq!(keys(&[0xe0, 0x5e])[0], (sys(0x81), true));
        // ACKs and a break with no make are ignored.
        assert_eq!(keys(&[0xfa, 0x9e]), vec![]);
    }

    #[test]
    fn set1_release_all() {
        let mut k = Set1::new();
        let mut out = Vec::new();
        k.report(&[0x1d, 0xe0, 0x4b], &mut out);
        out.clear();
        k.release_all(&mut out);
        assert_eq!(out, vec![Input::Key { usage: kb(0xe0), down: false }, Input::Key { usage: kb(0x50), down: false }]);
    }

    fn pkt(buttons_up: u8, dx: i8, dy: i8, dz: i8, ext_up: u8) -> [u8; 8] {
        let (x1, y1) = (dx >> 1, dy >> 1);
        let z1 = dz >> 1;
        [
            0x80 | buttons_up,
            x1 as u8,
            y1 as u8,
            (dx - x1) as u8,
            (dy - y1) as u8,
            (z1 as u8) & 0x7f,
            ((dz - z1) as u8) & 0x7f,
            ext_up,
        ]
    }

    #[test]
    fn sysmouse() {
        let mut m = SysMouse::new();
        let mut out = Vec::new();
        // Garbage first, then: right of 5, up 3 (HID dy -3), wheel one
        // detent towards the user (HID -1), left button down.
        let mut bytes = vec![0x00, 0x13];
        bytes.extend(pkt(0x03, 5, 3, 1, 0x7f));
        m.report(&bytes, &mut out);
        assert_eq!(
            out,
            vec![
                Input::Motion { dx: 5, dy: -3 },
                Input::Button { usage: usage(9, 1), down: true },
                Input::Scroll { axis: Axis::Vertical, detents: -1 },
            ]
        );
        out.clear();
        // Split across reads: left up, right and button 4 down, wheel away.
        let p = pkt(0x06, 0, 0, -2, 0x7e);
        m.report(&p[..3], &mut out);
        assert!(out.is_empty());
        m.report(&p[3..], &mut out);
        assert_eq!(
            out,
            vec![
                Input::Button { usage: usage(9, 1), down: false },
                Input::Button { usage: usage(9, 2), down: true },
                Input::Button { usage: usage(9, 4), down: true },
                Input::Scroll { axis: Axis::Vertical, detents: 2 },
            ]
        );
        out.clear();
        m.release_all(&mut out);
        assert_eq!(out, vec![Input::Button { usage: usage(9, 2), down: false }, Input::Button { usage: usage(9, 4), down: false }]);
        // Middle alone.
        out.clear();
        m.report(&pkt(0x05, 0, 0, 0, 0x7f), &mut out);
        assert_eq!(out, vec![Input::Button { usage: usage(9, 3), down: true }]);
    }
}
