//! The Navigator's keyboard map (docs/spec/navigator-ui.md §9). System
//! shortcuts go to Forum first; these are what reaches the window.

use pergola::{Event, Key, KeyEventKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    FocusWhere,
    NewTrail,
    NewUnrecorded,
    PutAway,
    BringBack,
    Back,
    Forward,
    NextTrail,
    PrevTrail,
    /// Ctrl-1…9, from 1.
    Nth(usize),
    ToggleRail,
    Keep,
    Mute,
    ToggleStatus,
}

/// The action a key press asks for, if it is one of §9's. Not yet:
/// Ctrl-H (history view) and Ctrl-F (find), step 7; F6 (Pergola has no
/// function keys yet); zoom belongs to the page's ScrollView.
pub fn action(ev: &Event) -> Option<Action> {
    let Event::Key { kind: KeyEventKind::Down, key, modifiers: m, chars } = ev else { return None };
    let c = chars.to_ascii_lowercase();
    if m.alt && !m.ctrl {
        return match key {
            Key::Left => Some(Action::Back),
            Key::Right => Some(Action::Forward),
            _ => None,
        };
    }
    if !m.ctrl || m.alt || m.meta {
        return None;
    }
    match key {
        Key::Tab => return Some(if m.shift { Action::PrevTrail } else { Action::NextTrail }),
        Key::Char => {}
        _ => return None,
    }
    Some(match (c.as_str(), m.shift) {
        ("l", false) => Action::FocusWhere,
        ("t", false) => Action::NewTrail,
        ("t", true) => Action::BringBack,
        ("n", true) => Action::NewUnrecorded,
        ("w", false) => Action::PutAway,
        ("b", false) => Action::ToggleRail,
        ("d", false) => Action::Keep,
        ("m", false) => Action::Mute,
        ("/", _) | ("?", true) => Action::ToggleStatus,
        (d, false) if matches!(d.as_bytes(), [b'1'..=b'9']) => Action::Nth(d.parse().ok()?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::Modifiers;

    fn k(key: Key, chars: &str, ctrl: bool, shift: bool, alt: bool) -> Event {
        Event::Key { kind: KeyEventKind::Down, key, modifiers: Modifiers { ctrl, shift, alt, meta: false }, chars: chars.into() }
    }

    #[test]
    fn map() {
        assert_eq!(action(&k(Key::Char, "l", true, false, false)), Some(Action::FocusWhere));
        assert_eq!(action(&k(Key::Char, "T", true, true, false)), Some(Action::BringBack));
        assert_eq!(action(&k(Key::Char, "N", true, true, false)), Some(Action::NewUnrecorded));
        assert_eq!(action(&k(Key::Tab, "", true, true, false)), Some(Action::PrevTrail));
        assert_eq!(action(&k(Key::Left, "", false, false, true)), Some(Action::Back));
        assert_eq!(action(&k(Key::Char, "3", true, false, false)), Some(Action::Nth(3)));
        assert_eq!(action(&k(Key::Char, "9", true, false, false)), Some(Action::Nth(9)));
        assert_eq!(action(&k(Key::Char, "0", true, false, false)), None, "Ctrl-0 is zoom");
        assert_eq!(action(&k(Key::Char, "l", false, false, false)), None);
        let up = Event::Key { kind: KeyEventKind::Up, key: Key::Char, modifiers: Modifiers { ctrl: true, ..Default::default() }, chars: "l".into() };
        assert_eq!(action(&up), None);
    }
}
