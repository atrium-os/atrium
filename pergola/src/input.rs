//! Translates `fresco_client::Event`s into `pergola::Event`s.
//!
//! Lives in the toolkit so apps don't repeat the translation. The
//! translation is deliberately small — it covers what TextField +
//! Button need (pointer down/up + printable characters + Backspace +
//! Tab + Enter + Escape) and forwards everything else as `None`.
//! Larger key coverage (function keys, full unicode IME) layers in
//! when those features are added.

use crate::event::{Event, Key, KeyEventKind, Modifiers};
use crate::geom::Point;

/// Map a fresco-client event to Pergola's input model. Returns `None`
/// when the input is irrelevant to the toolkit (e.g. window-resize
/// events, which the App handles separately).
pub fn translate(ev: &fresco_client::Event) -> Option<Event> {
    match ev {
        fresco_client::Event::PointerButton { x, y, pressed, .. } => {
            let at = Point::new(*x, *y);
            Some(if *pressed {
                Event::PointerDown { at }
            } else {
                Event::PointerUp { at }
            })
        }
        fresco_client::Event::PointerMotion { x, y, .. } => {
            Some(Event::PointerMove { at: Point::new(*x, *y) })
        }
        fresco_client::Event::Key { hid_usage, pressed, modifiers, .. } => {
            let mods = decode_modifiers(*modifiers);
            let kind = if *pressed { KeyEventKind::Down } else { KeyEventKind::Up };
            let (key, chars) = decode_key(*hid_usage, &mods);
            Some(Event::Key { kind, key, modifiers: mods, chars })
        }
        // Window lifecycle events not yet routed through Pergola's
        // input system — the embedder handles them at the loop level.
        fresco_client::Event::Resized { .. }
        | fresco_client::Event::FocusChanged { .. }
        | fresco_client::Event::CloseRequested { .. }
        | fresco_client::Event::DpiChanged { .. }
        | fresco_client::Event::PointerScroll { .. }
        | fresco_client::Event::Unknown { .. } => None,
    }
}

/// fresco-client packs modifiers into a single byte. The bit layout
/// follows USB HID modifier byte: shift = bits 1|5, ctrl = bits 0|4,
/// alt = bits 2|6, meta/gui = bits 3|7.
fn decode_modifiers(byte: u8) -> Modifiers {
    Modifiers {
        ctrl:  byte & 0b0001_0001 != 0,
        shift: byte & 0b0010_0010 != 0,
        alt:   byte & 0b0100_0100 != 0,
        meta:  byte & 0b1000_1000 != 0,
    }
}

/// Resolve an HID usage code (USB HID page 0x07) to a logical Key
/// + the printable character it produces (if any), respecting
/// shift state for casing.
fn decode_key(hid: u16, mods: &Modifiers) -> (Key, String) {
    // a–z: 0x04..=0x1D
    if (0x04..=0x1D).contains(&hid) {
        let lower = (b'a' + (hid - 0x04) as u8) as char;
        let c = if mods.shift { lower.to_ascii_uppercase() } else { lower };
        return (Key::Char, c.to_string());
    }
    // 1234567890: 0x1E..=0x27
    if (0x1E..=0x27).contains(&hid) {
        // HID weirdness: 0x1E is '1', 0x27 is '0'.
        let table_unshifted = ['1','2','3','4','5','6','7','8','9','0'];
        let table_shifted   = ['!','@','#','$','%','^','&','*','(',')'];
        let i = (hid - 0x1E) as usize;
        let c = if mods.shift { table_shifted[i] } else { table_unshifted[i] };
        return (Key::Char, c.to_string());
    }

    match hid {
        0x28 => (Key::Enter,     String::new()),
        0x29 => (Key::Escape,    String::new()),
        0x2A => (Key::Backspace, String::new()),
        0x2B => (Key::Tab,       String::new()),
        0x2C => (Key::Char,      " ".into()),                    // spacebar
        0x2D => (Key::Char,      if mods.shift { "_".into() } else { "-".into() }),  // -/_
        0x2E => (Key::Char,      if mods.shift { "+".into() } else { "=".into() }),  // =/+
        0x2F => (Key::Char,      if mods.shift { "{".into() } else { "[".into() }),
        0x30 => (Key::Char,      if mods.shift { "}".into() } else { "]".into() }),
        0x31 => (Key::Char,      if mods.shift { "|".into() } else { "\\".into() }),
        0x33 => (Key::Char,      if mods.shift { ":".into() } else { ";".into() }),
        0x34 => (Key::Char,      if mods.shift { "\"".into() } else { "'".into() }),
        0x36 => (Key::Char,      if mods.shift { "<".into() } else { ",".into() }),
        0x37 => (Key::Char,      if mods.shift { ">".into() } else { ".".into() }),
        0x38 => (Key::Char,      if mods.shift { "?".into() } else { "/".into() }),
        0x4C => (Key::Delete,    String::new()),
        0x4F => (Key::Right,     String::new()),
        0x50 => (Key::Left,      String::new()),
        0x51 => (Key::Down,      String::new()),
        0x52 => (Key::Up,        String::new()),
        0x4A => (Key::Home,      String::new()),
        0x4D => (Key::End,       String::new()),
        // Anything else: use Char with empty content — the Down event
        // still reaches handlers that want to react to non-printable
        // keys, but TextField won't append anything.
        _ => (Key::Char, String::new()),
    }
}
