//! Monstrator's sheet (docs/spec/opening-links.md §5): "Open this link
//! in…", the link's scheme and host, the apps that take it, "Always use
//! this app", and Cancel. The state only; `view` draws it and monstratord
//! runs it. A pick counts only once frescod has counted its press
//! (`PROMPT`, self-spent: the rows are the window's guard).

use portcullis_ipc::LinkChoice;

/// What the person did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Chose { app: String, always: bool },
    Cancelled,
}

/// What the runner does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Do {
    Nothing,
    Redraw,
    /// Spend `token` with frescod; if it counts, the pick of row `row`.
    Spend { row: usize, token: [u8; 16] },
    Done(Outcome),
}

/// A row of the sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    App(usize),
    Always,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct Sheet {
    /// What the sheet shows of the link (`atrium_links::Link::shown`).
    pub shown: String,
    pub scheme: String,
    pub apps: Vec<LinkChoice>,
    pub always: bool,
    pub focus: usize,
    /// Said under the text after a press frescod didn't count.
    pub notice: Option<&'static str>,
}

impl Sheet {
    pub fn new(shown: &str, scheme: &str, apps: Vec<LinkChoice>) -> Sheet {
        Sheet { shown: shown.into(), scheme: scheme.into(), apps, always: false, focus: 0, notice: None }
    }

    pub fn rows(&self) -> Vec<Row> {
        (0..self.apps.len()).map(Row::App).chain([Row::Always, Row::Cancel]).collect()
    }

    pub fn label(&self, r: &Row) -> String {
        match r {
            Row::App(i) => self.apps[*i].name.clone(),
            Row::Always => format!("{} Always use this app for {} links", if self.always { "[x]" } else { "[ ]" }, self.scheme),
            Row::Cancel => "Cancel".into(),
        }
    }

    /// The title and the line under it.
    pub fn text(&self) -> (String, String) {
        ("Open this link in\u{2026}".into(), self.shown.clone())
    }

    /// A click (or Enter) on row `i` with its press `token`.
    pub fn press(&mut self, i: usize, token: [u8; 16]) -> Do {
        match self.rows().get(i) {
            Some(Row::App(_)) => Do::Spend { row: i, token },
            Some(Row::Always) => {
                self.always = !self.always;
                self.focus = i;
                Do::Redraw
            }
            Some(Row::Cancel) => Do::Done(Outcome::Cancelled),
            None => Do::Nothing,
        }
    }

    /// What frescod said of the press for row `row`.
    pub fn spent(&mut self, row: usize, counted: bool) -> Do {
        match (self.rows().get(row), counted) {
            (Some(Row::App(i)), true) => Do::Done(Outcome::Chose { app: self.apps[*i].id.clone(), always: self.always }),
            _ => {
                self.notice = Some("That press didn't count: the sheet was covered or had just moved. Press again.");
                Do::Redraw
            }
        }
    }

    /// Keys: up and down move, Enter presses, Space toggles, Escape cancels.
    pub fn key(&mut self, k: Key, token: [u8; 16]) -> Do {
        let n = self.rows().len();
        match k {
            Key::Up => {
                self.focus = (self.focus + n - 1) % n;
                Do::Redraw
            }
            Key::Down | Key::Tab => {
                self.focus = (self.focus + 1) % n;
                Do::Redraw
            }
            Key::Enter => self.press(self.focus, token),
            Key::Space if self.rows()[self.focus] == Row::Always => self.press(self.focus, token),
            Key::Escape => Do::Done(Outcome::Cancelled),
            _ => Do::Nothing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Tab,
    Enter,
    Space,
    Escape,
    Other,
}

impl Key {
    /// From a HID usage (page 7).
    pub fn from_hid(hid: u16) -> Key {
        match hid {
            0x52 => Key::Up,
            0x51 => Key::Down,
            0x2B => Key::Tab,
            0x28 | 0x58 => Key::Enter,
            0x2C => Key::Space,
            0x29 => Key::Escape,
            _ => Key::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet() -> Sheet {
        let c = |id: &str| LinkChoice { id: id.into(), name: id.to_uppercase(), default: false };
        Sheet::new("mailto:ana@example.org", "mailto", vec![c("org.a.mail"), c("org.b.mail")])
    }

    #[test]
    fn a_pick_counts_only_with_its_press() {
        let mut s = sheet();
        assert_eq!(s.press(1, [7; 16]), Do::Spend { row: 1, token: [7; 16] });
        assert_eq!(s.spent(1, false), Do::Redraw, "not counted: ask again");
        assert!(s.notice.is_some());
        assert_eq!(s.press(2, [0; 16]), Do::Redraw, "Always toggles");
        assert!(s.always);
        assert_eq!(s.spent(1, true), Do::Done(Outcome::Chose { app: "org.b.mail".into(), always: true }));
        assert_eq!(s.press(3, [0; 16]), Do::Done(Outcome::Cancelled));
        assert_eq!(s.press(9, [0; 16]), Do::Nothing);
    }

    #[test]
    fn keys_move_press_and_cancel() {
        let mut s = sheet();
        assert_eq!(s.key(Key::Up, [0; 16]), Do::Redraw);
        assert_eq!(s.focus, 3, "wraps to Cancel");
        assert_eq!(s.key(Key::Up, [0; 16]), Do::Redraw);
        assert_eq!(s.key(Key::Space, [0; 16]), Do::Redraw);
        assert!(s.always);
        s.key(Key::Down, [0; 16]);
        s.key(Key::Down, [0; 16]);
        assert_eq!(s.key(Key::Enter, [5; 16]), Do::Spend { row: 0, token: [5; 16] });
        assert_eq!(s.key(Key::Escape, [0; 16]), Do::Done(Outcome::Cancelled));
        assert_eq!(Key::from_hid(0x28), Key::Enter);
        assert!(s.label(&Row::Always).contains("mailto links"));
    }
}
