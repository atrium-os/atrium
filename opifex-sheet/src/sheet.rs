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

/// A catalog someone asks to add (opifex.md §6.5): its name, where it is
/// served, and its root.toml's hash (what the press approves).
#[derive(Debug, Clone, PartialEq)]
pub struct Adding {
    pub name: String,
    pub url: String,
    pub root_sha256: String,
    pub warning: String,
}

#[derive(Debug, Clone)]
pub struct Sheet {
    pub app: Described,
    /// Set when the sheet asks about adding a catalog, not an app.
    pub adding: Option<Adding>,
    pub focus: usize,
    pub notice: Option<&'static str>,
}

impl Sheet {
    pub fn new(app: Described) -> Sheet {
        // Not now is the default: Enter alone never installs.
        Sheet { app, adding: None, focus: 1, notice: None }
    }

    pub fn catalog(a: Adding) -> Sheet {
        Sheet { app: Described::default(), adding: Some(a), focus: 1, notice: None }
    }

    /// The window's title.
    pub fn window_title(&self) -> &'static str {
        if self.adding.is_some() { "Add a catalog" } else { "Install" }
    }

    pub fn update(&self) -> bool {
        self.app.installed.is_some()
    }

    pub fn title(&self) -> String {
        if let Some(a) = &self.adding {
            return format!("Add the catalog {}?", a.name);
        }
        let name = if self.app.name.is_empty() { &self.app.id } else { &self.app.name };
        if self.update() {
            format!("Update {name} to {}?", self.app.latest)
        } else {
            format!("Install {name}?")
        }
    }

    /// The lines under the title.
    pub fn lines(&self) -> Vec<(String, Tone)> {
        let mut v = Vec::new();
        if let Some(c) = &self.adding {
            v.push((format!("From {}", c.url), Tone::Quiet));
            v.push((c.warning.clone(), Tone::Marked));
            v.push(("Its apps will be offered to you only. Nothing is installed until you ask for an app.".into(), Tone::Body));
            let short: String = c.root_sha256.chars().take(16).collect();
            v.push((format!("Its keys: {short}"), Tone::Quiet));
            if let Some(n) = self.notice {
                v.push((n.into(), Tone::Marked));
            }
            return v;
        }
        let a = &self.app;
        if !a.description.is_empty() {
            v.push((a.description.clone(), Tone::Body));
        }
        if a.third_party {
            let who = if a.publisher_domains.is_empty() { a.publisher_name.clone() }
                      else { format!("{} ({})", a.publisher_name, a.publisher_domains.join(", ")) };
            v.push((format!("{} {} from {who}, in the {} catalog you added", a.id, a.latest, a.catalog), Tone::Quiet));
            v.push(("Atrium has not reviewed this catalog's apps.".into(), Tone::Marked));
        } else {
            let key: String = a.publisher.chars().take(12).collect();
            v.push((format!("{} {} from the {} catalog, signed by key {key}", a.id, a.latest, a.catalog), Tone::Quiet));
        }
        if a.recovered {
            v.push(("Its publisher lost its key, and the catalog's owners replaced it. Check this update is one you expect.".into(), Tone::Marked));
        }
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
            (Row::Install, _) if self.adding.is_some() => "Add",
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
        let mut t = described(None);
        (t.third_party, t.publisher_name, t.publisher_domains, t.catalog, t.recovered) =
            (true, "Example Ltd".into(), vec!["example.com".into()], "Example Apps".into(), true);
        let l = Sheet::new(t).lines();
        assert!(l.iter().any(|(x, _)| x.contains("from Example Ltd (example.com), in the Example Apps catalog you added")));
        assert!(l.contains(&("Atrium has not reviewed this catalog's apps.".into(), Tone::Marked)));
        assert!(l.iter().any(|(x, t)| x.starts_with("Its publisher lost its key") && *t == Tone::Marked));
        let c = Sheet::catalog(Adding { name: "Example Apps".into(), url: "https://apps.example.com/cat/".into(),
                                        root_sha256: "ab".repeat(32), warning: "Apps from this catalog are checked by nobody.".into() });
        assert_eq!((c.title().as_str(), c.label(Row::Install), c.window_title()), ("Add the catalog Example Apps?", "Add", "Add a catalog"));
        assert!(c.lines().contains(&("Apps from this catalog are checked by nobody.".into(), Tone::Marked)));
        assert_eq!(c.focus, 1, "Not now has the focus");
        assert_eq!(f.title(), "Install Hello?");
        assert!(f.lines().iter().any(|(x, _)| x.starts_with("Fonts run nothing")));
    }
}
