//! USB-HID → file-browser action.

const HID_A:           u16 = 0x04;
const HID_Z:           u16 = 0x1d;
const HID_ENTER:       u16 = 0x28;
const HID_ESCAPE:      u16 = 0x29;
const HID_BACKSPACE:   u16 = 0x2a;
const HID_ARROW_R:     u16 = 0x4f;
const HID_ARROW_L:     u16 = 0x50;
const HID_ARROW_D:     u16 = 0x51;
const HID_ARROW_U:     u16 = 0x52;
const HID_LSHIFT:      u16 = 0xe1;
const HID_RSHIFT:      u16 = 0xe5;
const HID_LCTRL:       u16 = 0xe0;
const HID_RCTRL:       u16 = 0xe4;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Up, Down,
    Enter,           // descend if dir
    ParentDir,       // ../ — also Backspace
    PageUp, PageDown,
    Quit,
}

pub struct Keymap { ctrl: bool, _shift: bool }
impl Keymap {
    pub fn new() -> Self { Self { ctrl: false, _shift: false } }
    pub fn handle(&mut self, code: u16, pressed: bool) -> Option<Action> {
        match code {
            HID_LSHIFT | HID_RSHIFT => { self._shift = pressed; None }
            HID_LCTRL  | HID_RCTRL  => { self.ctrl   = pressed; None }
            _ if !pressed => None,
            HID_ARROW_U   => Some(Action::Up),
            HID_ARROW_D   => Some(Action::Down),
            HID_ARROW_L   => Some(Action::ParentDir),
            HID_ARROW_R   => Some(Action::Enter),
            HID_ENTER     => Some(Action::Enter),
            HID_BACKSPACE => Some(Action::ParentDir),
            HID_ESCAPE    => Some(Action::Quit),
            // Ctrl+Q quit, j/k vi-style nav as bonus
            c if (HID_A..=HID_Z).contains(&c) => {
                let letter = b'a' + (c - HID_A) as u8;
                if self.ctrl && letter == b'q' { return Some(Action::Quit); }
                match letter {
                    b'k' => Some(Action::Up),
                    b'j' => Some(Action::Down),
                    b'h' => Some(Action::ParentDir),
                    b'l' => Some(Action::Enter),
                    _    => None,
                }
            }
            _ => None,
        }
    }
}
