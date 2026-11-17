//! USB HID usage code + modifiers → the bytes an xterm sends (US ANSI
//! layout), so programs that read keys as xterm-256color (vi, Helix,
//! Neovim, `less`) get what they expect: cursor keys by DECCKM, the
//! editing and function keys, modifier-encoded variants (`ESC [1;5C`),
//! Alt as an ESC prefix, the Ctrl control codes, and the keypad by
//! DECKPAM.
//!
//! The Fresco wire protocol carries USB HID Keyboard/Keypad usage
//! codes (Usage Page 0x07) — *not* Linux evdev codes — and the HID
//! modifier byte (bit 0 left Ctrl, 1 left Shift, 2 left Alt, 3 left GUI,
//! 4–7 the right ones).
//!
//! Reference: USB HID Usage Tables, Section 10
//! (https://usb.org/document-library/hid-usage-tables-15); xterm's
//! ctlseqs, "PC-Style Function Keys".

use stoa_term::mode;

const HID_A:           u16 = 0x04;
const HID_Z:           u16 = 0x1d;
const HID_1:           u16 = 0x1e;
const HID_2:           u16 = 0x1f;
const HID_6:           u16 = 0x23;
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
const HID_F1:          u16 = 0x3a;
const HID_F12:         u16 = 0x45;
const HID_INSERT:      u16 = 0x49;
const HID_HOME:        u16 = 0x4a;
const HID_PAGE_UP:     u16 = 0x4b;
const HID_DELETE:      u16 = 0x4c;
const HID_END:         u16 = 0x4d;
const HID_PAGE_DOWN:   u16 = 0x4e;
const HID_RIGHT:       u16 = 0x4f;
const HID_LEFT:        u16 = 0x50;
const HID_DOWN:        u16 = 0x51;
const HID_UP:          u16 = 0x52;
const HID_KP_SLASH:    u16 = 0x54;
const HID_KP_STAR:     u16 = 0x55;
const HID_KP_MINUS:    u16 = 0x56;
const HID_KP_PLUS:     u16 = 0x57;
const HID_KP_ENTER:    u16 = 0x58;
const HID_KP_1:        u16 = 0x59;
const HID_KP_0:        u16 = 0x62;
const HID_KP_PERIOD:   u16 = 0x63;
const HID_F13:         u16 = 0x68;
const HID_F24:         u16 = 0x73;

/// The HID modifier byte's halves.
const MOD_CTRL:  u8 = 0x01 | 0x10;
const MOD_SHIFT: u8 = 0x02 | 0x20;
const MOD_ALT:   u8 = 0x04 | 0x40;

/// Is the press `mods` a Ctrl+Shift chord (the Terminal's own keys)?
pub fn ctrl_shift(mods: u8) -> bool {
    mods & MOD_CTRL != 0 && mods & MOD_SHIFT != 0
}

/// The bytes a key press sends, given the HID modifier byte and the
/// terminal's [`mode`] bits; `None` for keys that send nothing (modifiers
/// themselves, unknown usages).
pub fn encode(code: u16, mods: u8, modes: u32) -> Option<Vec<u8>> {
    let shift = mods & MOD_SHIFT != 0;
    let alt = mods & MOD_ALT != 0;
    let ctrl = mods & MOD_CTRL != 0;
    // xterm's modifier parameter: 1 + Shift 1 + Alt 2 + Ctrl 4.
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi_letter = |c: u8| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[1;{m}{}", c as char).into_bytes()
        } else if modes & mode::CURSOR_KEYS != 0 {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if m > 1 { format!("\x1b[{n};{m}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    let alt_prefix = |mut b: Vec<u8>| -> Vec<u8> {
        if alt {
            b.insert(0, 0x1b);
        }
        b
    };
    Some(match code {
        HID_UP => csi_letter(b'A'),
        HID_DOWN => csi_letter(b'B'),
        HID_RIGHT => csi_letter(b'C'),
        HID_LEFT => csi_letter(b'D'),
        HID_HOME => csi_letter(b'H'),
        HID_END => csi_letter(b'F'),
        HID_INSERT => tilde(2),
        HID_DELETE => tilde(3),
        HID_PAGE_UP => tilde(5),
        HID_PAGE_DOWN => tilde(6),
        HID_F1..=HID_F12 => {
            let i = (code - HID_F1) as usize;
            if i < 4 {
                // F1–F4: SS3 P..S, or CSI 1;m P..S with modifiers.
                let c = b"PQRS"[i];
                if m > 1 { format!("\x1b[1;{m}{}", c as char).into_bytes() } else { vec![0x1b, b'O', c] }
            } else {
                tilde([15, 17, 18, 19, 20, 21, 23, 24][i - 4])
            }
        }
        HID_F13..=HID_F24 => {
            // As xterm: F13–F24 are F1–F12 with Shift.
            let i = (code - HID_F13) as usize;
            let m = m.max(2);
            if i < 4 {
                format!("\x1b[1;{m}{}", b"PQRS"[i] as char).into_bytes()
            } else {
                format!("\x1b[{};{m}~", [15, 17, 18, 19, 20, 21, 23, 24][i - 4]).into_bytes()
            }
        }
        HID_ENTER => alt_prefix(vec![b'\r']),
        HID_TAB if shift => b"\x1b[Z".to_vec(),
        HID_TAB => alt_prefix(vec![b'\t']),
        HID_BACKSPACE => alt_prefix(vec![if ctrl { 0x08 } else { 0x7f }]),
        HID_ESCAPE => alt_prefix(vec![0x1b]),
        HID_SPACE if ctrl => alt_prefix(vec![0x00]),
        HID_KP_SLASH..=HID_KP_PERIOD => return keypad(code, modes),
        _ => {
            let b = printable(code, shift)?;
            alt_prefix(vec![if ctrl { control(b, code)? } else { b }])
        }
    })
}

/// Ctrl with a printable key: the C0 code xterm sends.
fn control(b: u8, code: u16) -> Option<u8> {
    Some(match b {
        b'a'..=b'z' => b - b'a' + 1,
        b'A'..=b'Z' => b - b'A' + 1,
        b'@' | b' ' => 0x00,
        b'[' | b'{' => 0x1b,
        b'\\' | b'|' => 0x1c,
        b']' | b'}' => 0x1d,
        b'^' | b'~' => 0x1e,
        b'_' | b'/' | b'?' => 0x1f,
        b'-' => 0x1f,
        _ if code == HID_2 => 0x00,
        _ if code == HID_6 => 0x1e,
        b'3' => 0x1b,
        b'4' => 0x1c,
        b'5' => 0x1d,
        b'7' => 0x1f,
        b'8' => 0x7f,
        _ => b, // Ctrl has no code for it: the key as typed
    })
}

/// The keypad: digits and operators, or DECKPAM's application sequences.
fn keypad(code: u16, modes: u32) -> Option<Vec<u8>> {
    let app = modes & mode::APP_KEYPAD != 0;
    let (plain, appc) = match code {
        HID_KP_SLASH => (b'/', b'o'),
        HID_KP_STAR => (b'*', b'j'),
        HID_KP_MINUS => (b'-', b'm'),
        HID_KP_PLUS => (b'+', b'k'),
        HID_KP_ENTER => (b'\r', b'M'),
        HID_KP_PERIOD => (b'.', b'n'),
        HID_KP_0 => (b'0', b'p'),
        c if (HID_KP_1..HID_KP_0).contains(&c) => {
            let d = (c - HID_KP_1) as u8;
            (b'1' + d, b'q' + d)
        }
        _ => return None,
    };
    Some(if app { vec![0x1b, b'O', appc] } else { vec![plain] })
}

fn printable(code: u16, shift: bool) -> Option<u8> {
    // Letters
    if (HID_A..=HID_Z).contains(&code) {
        let base = (code - HID_A) as u8;
        return Some(if shift { b'A' + base } else { b'a' + base });
    }
    // Digit row
    if (HID_1..=HID_0).contains(&code) {
        // HID order: 1, 2, 3, 4, 5, 6, 7, 8, 9, 0
        let lo = b"1234567890";
        let hi = b"!@#$%^&*()";
        let i = (code - HID_1) as usize;
        return Some(if shift { hi[i] } else { lo[i] });
    }
    let (lo, hi) = match code {
        HID_SPACE     => (b' ',  b' '),
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
    Some(if shift { hi } else { lo })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u8 = 0x01;
    const SHIFT: u8 = 0x02;
    const ALT: u8 = 0x04;

    fn k(code: u16, mods: u8) -> Vec<u8> { encode(code, mods, mode::DEFAULT).unwrap() }

    #[test]
    fn letters_shift_ctrl_alt() {
        assert_eq!(k(HID_A, 0), b"a");
        assert_eq!(k(HID_A, SHIFT), b"A");
        assert_eq!(k(HID_A + 2, CTRL), [3]); // Ctrl+C
        assert_eq!(k(HID_A, ALT), b"\x1ba");
        assert_eq!(k(HID_A + 23, CTRL | ALT), [0x1b, 24]); // Alt+Ctrl+X
        assert_eq!(k(HID_A + 1, CTRL | 0x10), [2], "right Ctrl too");
    }

    #[test]
    fn ctrl_punctuation() {
        assert_eq!(k(HID_LBRACKET, CTRL), [0x1b]);
        assert_eq!(k(HID_BACKSLASH, CTRL), [0x1c]);
        assert_eq!(k(HID_RBRACKET, CTRL), [0x1d]);
        assert_eq!(k(HID_SPACE, CTRL), [0x00]);
        assert_eq!(k(HID_2, CTRL), [0x00]);
        assert_eq!(k(HID_6, CTRL | SHIFT), [0x1e]);
        assert_eq!(k(HID_SLASH, CTRL), [0x1f]);
    }

    #[test]
    fn cursor_keys_by_decckm() {
        assert_eq!(k(HID_UP, 0), b"\x1b[A");
        assert_eq!(encode(HID_UP, 0, mode::CURSOR_KEYS).unwrap(), b"\x1bOA");
        assert_eq!(k(HID_RIGHT, CTRL), b"\x1b[1;5C");
        assert_eq!(encode(HID_LEFT, SHIFT, mode::CURSOR_KEYS).unwrap(), b"\x1b[1;2D", "modifiers override DECCKM");
        assert_eq!(k(HID_HOME, 0), b"\x1b[H");
        assert_eq!(k(HID_END, ALT), b"\x1b[1;3F");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(k(HID_DELETE, 0), b"\x1b[3~");
        assert_eq!(k(HID_PAGE_UP, SHIFT), b"\x1b[5;2~");
        assert_eq!(k(HID_F1, 0), b"\x1bOP");
        assert_eq!(k(HID_F1 + 4, 0), b"\x1b[15~"); // F5
        assert_eq!(k(HID_F12, CTRL), b"\x1b[24;5~");
        assert_eq!(k(HID_F1 + 1, SHIFT), b"\x1b[1;2Q");
    }

    #[test]
    fn enter_tab_backspace_escape() {
        assert_eq!(k(HID_ENTER, 0), b"\r");
        assert_eq!(k(HID_TAB, 0), b"\t");
        assert_eq!(k(HID_TAB, SHIFT), b"\x1b[Z");
        assert_eq!(k(HID_BACKSPACE, 0), [0x7f]);
        assert_eq!(k(HID_BACKSPACE, CTRL), [0x08]);
        assert_eq!(k(HID_BACKSPACE, ALT), [0x1b, 0x7f]);
        assert_eq!(k(HID_ESCAPE, 0), [0x1b]);
    }

    #[test]
    fn keypad_modes() {
        assert_eq!(k(HID_KP_1 + 4, 0), b"5");
        assert_eq!(encode(HID_KP_1 + 4, 0, mode::APP_KEYPAD).unwrap(), b"\x1bOu");
        assert_eq!(encode(HID_KP_ENTER, 0, mode::APP_KEYPAD).unwrap(), b"\x1bOM");
        assert_eq!(k(HID_KP_0, 0), b"0");
    }

    #[test]
    fn modifiers_alone_send_nothing() {
        for code in 0xe0..=0xe7 {
            assert_eq!(encode(code, 0, 0), None);
        }
    }
}
