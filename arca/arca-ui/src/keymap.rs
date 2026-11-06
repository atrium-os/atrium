//! HID usage page 0x07 to what the field does with it. US layout, ASCII
//! only: the field takes printable characters one byte at a time into
//! locked memory, so nothing typed passes through a `String`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(u8),
    Backspace,
    Enter,
    Escape,
    Tab { back: bool },
    Up,
    Down,
    Other,
}

const SHIFT: u8 = 0b0010_0010;
const CTRL_ALT_META: u8 = 0b1101_1101;

pub fn decode(hid: u16, mods: u8) -> Key {
    let shift = mods & SHIFT != 0;
    let c = |lo: u8, hi: u8| Key::Char(if shift { hi } else { lo });
    let k = match hid {
        0x04..=0x1D => {
            let lo = b'a' + (hid - 0x04) as u8;
            c(lo, lo.to_ascii_uppercase())
        }
        0x1E..=0x27 => {
            let i = (hid - 0x1E) as usize;
            c(b"1234567890"[i], b"!@#$%^&*()"[i])
        }
        0x28 | 0x58 => Key::Enter,
        0x29 => Key::Escape,
        0x2A => Key::Backspace,
        0x2B => Key::Tab { back: shift },
        0x2C => Key::Char(b' '),
        0x2D => c(b'-', b'_'),
        0x2E => c(b'=', b'+'),
        0x2F => c(b'[', b'{'),
        0x30 => c(b']', b'}'),
        0x31 => c(b'\\', b'|'),
        0x33 => c(b';', b':'),
        0x34 => c(b'\'', b'"'),
        0x35 => c(b'`', b'~'),
        0x36 => c(b',', b'<'),
        0x37 => c(b'.', b'>'),
        0x38 => c(b'/', b'?'),
        0x51 => Key::Down,
        0x52 => Key::Up,
        _ => Key::Other,
    };
    // Shortcuts are not characters: ctrl-a must not type an 'a'.
    match k {
        Key::Char(_) if mods & CTRL_ALT_META != 0 => Key::Other,
        k => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(decode(0x04, 0), Key::Char(b'a'));
        assert_eq!(decode(0x04, 0x02), Key::Char(b'A'));
        assert_eq!(decode(0x04, 0x20), Key::Char(b'A'));
        assert_eq!(decode(0x04, 0x01), Key::Other, "ctrl-a");
        assert_eq!(decode(0x27, 0), Key::Char(b'0'));
        assert_eq!(decode(0x1E, 0x02), Key::Char(b'!'));
        assert_eq!(decode(0x2B, 0x02), Key::Tab { back: true });
        assert_eq!(decode(0x58, 0), Key::Enter);
        assert_eq!(decode(0x3A, 0), Key::Other, "F1");
    }
}
