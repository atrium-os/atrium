//! Mouse reports for programs that ask for them (DEC private modes 9,
//! 1000, 1002, 1003; SGR encoding 1006), and the wheel on the alternate
//! screen as cursor keys (1007) for programs that don't.
//!
//! The Terminal sends a report only when the screen's modes ask: a click
//! with no mouse mode set stays the Terminal's (the strip, the switcher).

use stoa_term::mode;

/// A pointer event, in cells (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Press(Button),
    Release(Button),
    /// Motion; `held` is the button down, if any.
    Motion { held: Option<Button> },
    /// One wheel step: `up` away from the user.
    Wheel { up: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    /// From Fresco's button number (HID: 1 left, 2 right, 3 middle).
    pub fn from_fresco(b: u8) -> Option<Button> {
        match b {
            1 => Some(Button::Left),
            2 => Some(Button::Right),
            3 => Some(Button::Middle),
            _ => None,
        }
    }

    fn code(self) -> u8 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
        }
    }
}

/// The report for `action` at cell (`row`, `col`) with HID modifiers
/// `mods`, if the `modes` ask for one.
pub fn report(action: Action, row: u16, col: u16, mods: u8, modes: u32) -> Option<Vec<u8>> {
    if modes & mode::MOUSE_ANY == 0 {
        return None;
    }
    let x10 = modes & mode::MOUSE_X10 != 0;
    let (mut code, release) = match action {
        Action::Press(b) => (b.code(), false),
        Action::Release(b) => {
            if x10 {
                return None; // X10: presses only
            }
            (b.code(), true)
        }
        Action::Motion { held } => {
            let wanted = modes & mode::MOUSE_MOTION != 0
                || (modes & mode::MOUSE_DRAG != 0 && held.is_some());
            if !wanted {
                return None;
            }
            (32 + held.map_or(3, |b| b.code()), false)
        }
        Action::Wheel { up } => {
            if x10 {
                return None;
            }
            (if up { 64 } else { 65 }, false)
        }
    };
    if !x10 {
        // Shift 4, Alt (Meta) 8, Ctrl 16.
        if mods & 0x22 != 0 { code |= 4 }
        if mods & 0x44 != 0 { code |= 8 }
        if mods & 0x11 != 0 { code |= 16 }
    }
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if modes & mode::MOUSE_SGR != 0 {
        return Some(format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes());
    }
    // The classic encoding: a release is button 3, positions past 223
    // can't be said.
    if x > 223 || y > 223 {
        return None;
    }
    let b = if release { 3 | (code & !3) } else { code };
    Some(vec![0x1b, b'[', b'M', 32 + b, 32 + x as u8, 32 + y as u8])
}

/// The wheel on the alternate screen with no mouse mode: cursor keys
/// (three per step), by DECCKM.
pub fn alt_scroll(up: bool, modes: u32) -> Option<Vec<u8>> {
    if modes & mode::MOUSE_ANY != 0 || modes & mode::ALT_SCREEN == 0 || modes & mode::ALT_SCROLL == 0 {
        return None;
    }
    let one: &[u8] = match (up, modes & mode::CURSOR_KEYS != 0) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    Some(one.repeat(3))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SGR: u32 = mode::MOUSE_CLICK | mode::MOUSE_SGR;

    #[test]
    fn nothing_without_a_mode() {
        assert_eq!(report(Action::Press(Button::Left), 0, 0, 0, mode::DEFAULT), None);
    }

    #[test]
    fn sgr_press_release() {
        assert_eq!(report(Action::Press(Button::Left), 4, 9, 0, SGR).unwrap(), b"\x1b[<0;10;5M");
        assert_eq!(report(Action::Release(Button::Left), 4, 9, 0, SGR).unwrap(), b"\x1b[<0;10;5m");
        assert_eq!(report(Action::Press(Button::Right), 0, 0, 0x01, SGR).unwrap(), b"\x1b[<18;1;1M");
    }

    #[test]
    fn wheel() {
        assert_eq!(report(Action::Wheel { up: true }, 0, 0, 0, SGR).unwrap(), b"\x1b[<64;1;1M");
        assert_eq!(report(Action::Wheel { up: false }, 0, 0, 0, SGR).unwrap(), b"\x1b[<65;1;1M");
    }

    #[test]
    fn motion_by_mode() {
        let drag = mode::MOUSE_DRAG | mode::MOUSE_SGR;
        assert_eq!(report(Action::Motion { held: None }, 0, 0, 0, drag), None);
        assert_eq!(report(Action::Motion { held: Some(Button::Left) }, 1, 2, 0, drag).unwrap(), b"\x1b[<32;3;2M");
        assert_eq!(report(Action::Motion { held: None }, 0, 0, 0, SGR), None, "1000 has no motion");
        let any = mode::MOUSE_MOTION | mode::MOUSE_SGR;
        assert_eq!(report(Action::Motion { held: None }, 0, 0, 0, any).unwrap(), b"\x1b[<35;1;1M");
    }

    #[test]
    fn classic_encoding() {
        let m = mode::MOUSE_CLICK;
        assert_eq!(report(Action::Press(Button::Left), 0, 0, 0, m).unwrap(), b"\x1b[M !!");
        assert_eq!(report(Action::Release(Button::Left), 0, 0, 0, m).unwrap(), b"\x1b[M#!!");
        assert_eq!(report(Action::Press(Button::Left), 0, 300, 0, m), None);
        assert_eq!(report(Action::Release(Button::Left), 0, 0, 0, mode::MOUSE_X10), None);
    }

    #[test]
    fn alt_scroll_keys() {
        let alt = mode::DEFAULT | mode::ALT_SCREEN;
        assert_eq!(alt_scroll(true, alt).unwrap(), b"\x1b[A\x1b[A\x1b[A");
        assert_eq!(alt_scroll(false, alt | mode::CURSOR_KEYS).unwrap(), b"\x1bOB\x1bOB\x1bOB");
        assert_eq!(alt_scroll(true, mode::DEFAULT), None, "main screen");
        assert_eq!(alt_scroll(true, alt | mode::MOUSE_CLICK), None, "the program takes the wheel");
    }
}
