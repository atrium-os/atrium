//! atrium-keyboard — type a string into the FreeBSD-native Atrium
//! editor by injecting HID-coded keystrokes over the Unix socket.
//!
//! Usage:
//!   atrium-keyboard "Hello, world!"
//!
//! Each character becomes a (down, up) pair of `CMD_INJECT_KEY` with
//! the appropriate HID Usage code + shift modifier. The compositor
//! broadcasts these as `COMP_INPUT_KEY` to every connected client; the
//! editor (atrium-edit-socket) receives them and applies via its
//! existing keymap.
//!
//! Replaced by a real /dev/usbhid (or virtio-input) reader inside
//! frescod in step 2(c.14).

use fresco_socket::Connection;

use std::time::Duration;

const HID_A:         u16 = 0x04;
const HID_1:         u16 = 0x1e;
const HID_0:         u16 = 0x27;
const HID_ENTER:     u16 = 0x28;
const HID_BACKSPACE: u16 = 0x2a;
const HID_TAB:       u16 = 0x2b;
const HID_SPACE:     u16 = 0x2c;
const HID_MINUS:     u16 = 0x2d;
const HID_EQUAL:     u16 = 0x2e;
const HID_LBRACKET:  u16 = 0x2f;
const HID_RBRACKET:  u16 = 0x30;
const HID_BACKSLASH: u16 = 0x31;
const HID_SEMICOLON: u16 = 0x33;
const HID_QUOTE:     u16 = 0x34;
const HID_BACKTICK:  u16 = 0x35;
const HID_COMMA:     u16 = 0x36;
const HID_PERIOD:    u16 = 0x37;
const HID_SLASH:     u16 = 0x38;

const MOD_SHIFT: u8 = 0x01;

/// Translate one ASCII char to (HID Usage, modifier-bitmap).
/// Returns None for chars we can't represent (rare-ASCII, control).
fn char_to_hid(c: char) -> Option<(u16, u8)> {
    if c.is_ascii_lowercase() {
        return Some((HID_A + (c as u16 - 'a' as u16), 0));
    }
    if c.is_ascii_uppercase() {
        return Some((HID_A + (c as u16 - 'A' as u16), MOD_SHIFT));
    }
    if c == '0' {
        return Some((HID_0, 0));
    }
    if c.is_ascii_digit() {
        return Some((HID_1 + (c as u16 - '1' as u16), 0));
    }
    match c {
        ' '  => Some((HID_SPACE,     0)),
        '\n' => Some((HID_ENTER,     0)),
        '\t' => Some((HID_TAB,       0)),
        '-'  => Some((HID_MINUS,     0)),
        '_'  => Some((HID_MINUS,     MOD_SHIFT)),
        '='  => Some((HID_EQUAL,     0)),
        '+'  => Some((HID_EQUAL,     MOD_SHIFT)),
        '['  => Some((HID_LBRACKET,  0)),
        '{'  => Some((HID_LBRACKET,  MOD_SHIFT)),
        ']'  => Some((HID_RBRACKET,  0)),
        '}'  => Some((HID_RBRACKET,  MOD_SHIFT)),
        '\\' => Some((HID_BACKSLASH, 0)),
        '|'  => Some((HID_BACKSLASH, MOD_SHIFT)),
        ';'  => Some((HID_SEMICOLON, 0)),
        ':'  => Some((HID_SEMICOLON, MOD_SHIFT)),
        '\'' => Some((HID_QUOTE,     0)),
        '"'  => Some((HID_QUOTE,     MOD_SHIFT)),
        '`'  => Some((HID_BACKTICK,  0)),
        '~'  => Some((HID_BACKTICK,  MOD_SHIFT)),
        ','  => Some((HID_COMMA,     0)),
        '<'  => Some((HID_COMMA,     MOD_SHIFT)),
        '.'  => Some((HID_PERIOD,    0)),
        '>'  => Some((HID_PERIOD,    MOD_SHIFT)),
        '/'  => Some((HID_SLASH,     0)),
        '?'  => Some((HID_SLASH,     MOD_SHIFT)),
        '!'  => Some((HID_1,         MOD_SHIFT)),
        '@'  => Some((HID_1 + 1,     MOD_SHIFT)),
        '#'  => Some((HID_1 + 2,     MOD_SHIFT)),
        '$'  => Some((HID_1 + 3,     MOD_SHIFT)),
        '%'  => Some((HID_1 + 4,     MOD_SHIFT)),
        '^'  => Some((HID_1 + 5,     MOD_SHIFT)),
        '&'  => Some((HID_1 + 6,     MOD_SHIFT)),
        '*'  => Some((HID_1 + 7,     MOD_SHIFT)),
        '('  => Some((HID_1 + 8,     MOD_SHIFT)),
        ')'  => Some((HID_0,         MOD_SHIFT)),
        _    => None,
    }
}

fn main() -> std::io::Result<()> {
    let s = std::env::args().nth(1)
        .unwrap_or_else(|| "Hello, atrium!".to_string());
    let sock = std::env::args().nth(2)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("connected to {sock}; injecting {:?}", s);

    for c in s.chars() {
        let Some((hid, mods)) = char_to_hid(c) else {
            eprintln!("skipping unsupported char {:?}", c);
            continue;
        };
        // Down + up. Modifier-as-shift is encoded in `mods` per event.
        conn.inject_key(hid, /*pressed=*/true, mods, /*target=*/0)?;
        conn.inject_key(hid, /*pressed=*/false, mods, /*target=*/0)?;
        // Slow enough that the editor can comfortably re-render
        // between presses. Faster than a human types but slower than
        // the protocol's saturation point.
        std::thread::sleep(Duration::from_millis(40));
    }
    eprintln!("done");
    Ok(())
}
