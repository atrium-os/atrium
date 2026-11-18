//! The install sheet's state (opifex.md §7.1): the app's name, what it is,
//! where it comes from, what it may do in plain words (the ones that reach
//! beyond its window marked), its size, and *Install* · *Not now*. `view`
//! draws it and opifex-sheetd runs it. *Install* counts only once frescod
//! has counted its press (`PROMPT`, self-spent: the rows are the guard).

use opifex::proto::Described;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Install,
    NotNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Do {
    Nothing,
    Redraw,
    /// Spend `token` with frescod; if it counts, Install.
    Spend { token: [u8; 16] },
    Done(Outcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Install,
    NotNow,
}

pub const ROWS: [Row; 2] = [Row::Install, Row::NotNow];

/// How a line of text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Body,
    Quiet,
    Marked,
}

#[derive(Debug, Clone)]
pub struct Sheet {
    pub app: Described,
    pub focus: usize,
    pub notice: Option<&'static str>,
}

impl Sheet {
    pub fn new(app: Described) -> Sheet {
        // Not now is the default: Enter alone never installs.
        Sheet { app, focus: 1, notice: None }
    }

    pub fn update(&self) -> bool {
        self.app.installed.is_some()
    }

    pub fn title(&self) -> String {
        let name = if self.app.name.is_empty() { &self.app.id } else { &self.app.name };
        if self.update() {
            format!("Update {name} to {}?", self.app.latest)
        } else {
            format!("Install {name}?")
        }
    }

    /// The lines under the title.
    pub fn lines(&self) -> Vec<(String, Tone)> {
        let a = &self.app;
        let mut v = Vec::new();
        if !a.description.is_empty() {
            v.push((a.description.clone(), Tone::Body));
        }
        let key: String = a.publisher.chars().take(12).collect();
        v.push((format!("{} {} from the {} catalog, signed by key {key}", a.id, a.latest, a.catalog), Tone::Quiet));
        if let Some(old) = &a.installed {
            v.push((format!("You have {old}."), Tone::Quiet));
        }
        // An update shows only what it adds: the rest was agreed to already.
        let (head, list) = if self.update() { ("It now also asks to:", &a.added) } else { ("It may:", &a.capabilities) };
        if a.kind == "font" {
            v.push(("Fonts run nothing and see nothing; every app can draw with them.".into(), Tone::Body));
        } else if self.update() && list.is_empty() {
            v.push(("It asks for nothing new.".into(), Tone::Body));
        } else if list.is_empty() {
            v.push(("It may use nothing beyond its own window.".into(), Tone::Body));
        } else {
            v.push((head.into(), Tone::Body));
            for c in list {
                let (w, risky) = crate::words(c);
                v.push((format!("  {w}"), if risky { Tone::Marked } else { Tone::Body }));
            }
        }
        v.push((format!("{} to download.", crate::size(a.archive_size)), Tone::Quiet));
        if let Some(n) = self.notice {
            v.push((n.into(), Tone::Marked));
        }
        v
    }

    pub fn label(&self, r: Row) -> &'static str {
        match (r, self.update()) {
            (Row::Install, false) => "Install",
            (Row::Install, true) => "Update",
            (Row::NotNow, _) => "Not now",
        }
    }

    pub fn press(&mut self, i: usize, token: [u8; 16]) -> Do {
        match ROWS.get(i) {
            Some(Row::Install) => Do::Spend { token },
            Some(Row::NotNow) => Do::Done(Outcome::NotNow),
            None => Do::Nothing,
        }
    }

    /// What frescod said of the press on Install.
    pub fn spent(&mut self, counted: bool) -> Do {
        if counted {
            Do::Done(Outcome::Install)
        } else {
            self.notice = Some("That press didn't count: the sheet was covered or had just moved. Press again.");
            Do::Redraw
        }
    }

    pub fn key(&mut self, k: Key, token: [u8; 16]) -> Do {
        match k {
            Key::Up | Key::Down | Key::Tab => {
                self.focus = 1 - self.focus;
                Do::Redraw
            }
            // frescod counts a key for PROMPT only if it is Enter.
            Key::Enter => self.press(self.focus, token),
            Key::Space if self.focus == 1 => Do::Done(Outcome::NotNow),
            Key::Escape => Do::Done(Outcome::NotNow),
            Key::Space | Key::Other => Do::Nothing,
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
pub(crate) fn described(installed: Option<&str>) -> Described {
    Described {
        id: "org.atrium.hello".into(), kind: "app".into(), name: "Hello".into(), description: "Says hello".into(),
        latest: "1.1".into(), publisher: "0123456789abcdef0123".into(), catalog: "Atrium".into(),
        capabilities: vec!["clipboard".into(), "network".into()], archive_size: 2_500_000,
        installed: installed.map(Into::into), manifest_sha256: "ab".repeat(32), added: vec![],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_counts_only_with_its_press() {
        let mut s = Sheet::new(described(None));
        assert_eq!(s.focus, 1, "Not now has the focus");
        assert_eq!(s.key(Key::Enter, [1; 16]), Do::Done(Outcome::NotNow));
        assert_eq!(s.press(0, [7; 16]), Do::Spend { token: [7; 16] });
        assert_eq!(s.spent(false), Do::Redraw);
        assert!(s.lines().iter().any(|(l, t)| l.contains("didn't count") && *t == Tone::Marked));
        assert_eq!(s.spent(true), Do::Done(Outcome::Install));
        assert_eq!(s.key(Key::Up, [0; 16]), Do::Redraw);
        assert_eq!(s.key(Key::Space, [3; 16]), Do::Nothing, "frescod wouldn't count it");
        assert_eq!(s.key(Key::Enter, [3; 16]), Do::Spend { token: [3; 16] });
        assert_eq!(s.key(Key::Escape, [0; 16]), Do::Done(Outcome::NotNow));
        assert_eq!(s.press(5, [0; 16]), Do::Nothing);
    }

    #[test]
    fn says_what_it_is_and_may_do() {
        let s = Sheet::new(described(None));
        assert_eq!(s.title(), "Install Hello?");
        let l = s.lines();
        assert!(l.iter().any(|(x, _)| x.contains("from the Atrium catalog, signed by key 0123456789ab")));
        assert!(l.contains(&("  use the network".into(), Tone::Marked)));
        assert!(l.contains(&("  read and write the clipboard".into(), Tone::Body)));
        assert!(l.iter().any(|(x, _)| x == "2.5 MB to download."));
        let u = Sheet::new(described(Some("1.0")));
        assert_eq!(u.title(), "Update Hello to 1.1?");
        assert_eq!(u.label(Row::Install), "Update");
        assert!(u.lines().iter().any(|(x, _)| x == "You have 1.0."));
        assert!(u.lines().iter().any(|(x, _)| x == "It asks for nothing new."));
        assert!(!u.lines().iter().any(|(x, _)| x.contains("clipboard")));
        let mut d = described(Some("1.0"));
        d.added = vec!["network".into()];
        let l = Sheet::new(d).lines();
        assert!(l.iter().any(|(x, _)| x == "It now also asks to:"));
        assert!(l.contains(&("  use the network".into(), Tone::Marked)));
        assert!(!l.iter().any(|(x, _)| x.contains("clipboard")));
        let f = Sheet::new(Described { kind: "font".into(), capabilities: vec![], ..described(None) });
        assert_eq!(f.title(), "Install Hello?");
        assert!(f.lines().iter().any(|(x, _)| x.starts_with("Fonts run nothing")));
    }
}
