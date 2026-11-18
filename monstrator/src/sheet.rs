//! Monstrator's sheet (docs/spec/opening-links.md §5): "Open this link
//! in…", the link's scheme and host, the apps that take it, "Always use
//! this app", and Cancel. The state only; `view` draws it and monstratord
//! runs it. A pick counts only once frescod has counted its press
//! (`PROMPT`, self-spent: the rows are the window's guard).

use portcullis_ipc::{LinkChoice, PersonaChoice};

/// What the person did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `persona`: another persona to open it in (personas.md §6), by
    /// name; `None`, the asking app's own.
    Chose { app: String, always: bool, persona: Option<String> },
    /// Copy address (personas.md §6): the address goes on the clipboard
    /// in the asking app's persona, spending `token`, the press.
    Copy { token: [u8; 16] },
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
    /// Which persona it opens in (cycles); only with named personas.
    Persona,
    /// Copy the address instead, in the asker's persona; only with named
    /// personas, for when the link may not go where the person wants.
    Copy,
    Always,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct Sheet {
    /// What the sheet shows of the link (`atrium_links::Link::shown`).
    pub shown: String,
    pub scheme: String,
    pub apps: Vec<LinkChoice>,
    /// The person's personas, the asker's first (empty: none named).
    pub personas: Vec<PersonaChoice>,
    /// The one chosen, an index into `personas`.
    pub into: usize,
    pub always: bool,
    pub focus: usize,
    /// Said under the text: a press frescod didn't count, or a persona
    /// the link may not go to.
    pub notice: Option<String>,
}

impl Sheet {
    pub fn new(shown: &str, scheme: &str, apps: Vec<LinkChoice>) -> Sheet {
        Sheet::with_personas(shown, scheme, apps, vec![])
    }

    /// The sheet with the person's personas: the apps that only another
    /// persona may get (the asker's own) are rows too, open only there.
    pub fn with_personas(shown: &str, scheme: &str, mut apps: Vec<LinkChoice>, personas: Vec<PersonaChoice>) -> Sheet {
        for p in &personas {
            for a in &p.also {
                if !apps.iter().any(|x| x.id == a.id) {
                    apps.push(a.clone());
                }
            }
        }
        let personas = if personas.len() > 1 { personas } else { vec![] };
        Sheet { shown: shown.into(), scheme: scheme.into(), apps, personas, into: 0, always: false, focus: 0, notice: None }
    }

    pub fn rows(&self) -> Vec<Row> {
        let p = (!self.personas.is_empty()).then_some([Row::Persona, Row::Copy]);
        (0..self.apps.len()).map(Row::App).chain(p.into_iter().flatten()).chain([Row::Always, Row::Cancel]).collect()
    }

    fn persona_name(p: &PersonaChoice) -> String {
        let mut c = p.name.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }

    /// Whether app `i` may get the link in the chosen persona: in the
    /// asker's own, not the asker itself; elsewhere, only with the switch.
    fn opens(&self, i: usize) -> Result<(), String> {
        let Some(p) = self.personas.get(self.into) else { return Ok(()) };
        let here = &self.personas[0];
        if !p.allowed {
            return Err(format!("Links from {} to {} are off. Settings, Apps, Between personas has the switch.", Self::persona_name(here), Self::persona_name(p)));
        }
        let only_elsewhere = self.personas.iter().any(|q| q.also.iter().any(|a| a.id == self.apps[i].id));
        if p.here && only_elsewhere {
            return Err(format!("{} asked: choose another persona for it.", self.apps[i].name));
        }
        Ok(())
    }

    pub fn label(&self, r: &Row) -> String {
        match r {
            Row::App(i) => {
                let here = self.personas.get(self.into).is_some_and(|p| p.here);
                let only_elsewhere = self.personas.iter().any(|q| q.also.iter().any(|a| a.id == self.apps[*i].id));
                if here && only_elsewhere {
                    format!("{} (in another persona only)", self.apps[*i].name)
                } else {
                    self.apps[*i].name.clone()
                }
            }
            Row::Persona => {
                let p = &self.personas[self.into];
                let how = if p.here { " (where it was opened)" } else if p.allowed { "" } else { " (off in Settings)" };
                format!("In: {}{how}", Self::persona_name(p))
            }
            Row::Copy => format!("Copy address (in {})", Self::persona_name(&self.personas[0])),
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
            Some(Row::App(a)) => match self.opens(*a) {
                Ok(()) => Do::Spend { row: i, token },
                Err(why) => {
                    self.notice = Some(why);
                    self.focus = i;
                    Do::Redraw
                }
            },
            Some(Row::Persona) => {
                self.into = (self.into + 1) % self.personas.len();
                self.notice = None;
                self.focus = i;
                Do::Redraw
            }
            Some(Row::Always) => {
                self.always = !self.always;
                self.focus = i;
                Do::Redraw
            }
            Some(Row::Copy) => Do::Done(Outcome::Copy { token }),
            Some(Row::Cancel) => Do::Done(Outcome::Cancelled),
            None => Do::Nothing,
        }
    }

    /// What frescod said of the press for row `row`.
    pub fn spent(&mut self, row: usize, counted: bool) -> Do {
        match (self.rows().get(row), counted) {
            (Some(Row::App(i)), true) => {
                let persona = self.personas.get(self.into).filter(|p| !p.here).map(|p| p.name.clone());
                Do::Done(Outcome::Chose { app: self.apps[*i].id.clone(), always: self.always, persona })
            }
            _ => {
                self.notice = Some("That press didn't count: the sheet was covered or had just moved. Press again.".into());
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
            Key::Space if matches!(self.rows()[self.focus], Row::Always | Row::Persona) => self.press(self.focus, token),
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
        assert_eq!(s.spent(1, true), Do::Done(Outcome::Chose { app: "org.b.mail".into(), always: true, persona: None }));
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

    fn p(name: &str, here: bool, allowed: bool, also: &[&str]) -> PersonaChoice {
        PersonaChoice { name: name.into(), colour: None, here, allowed,
                        also: also.iter().map(|a| LinkChoice { id: (*a).into(), name: "Navigator".into(), default: false }).collect() }
    }

    #[test]
    fn a_link_crosses_only_where_the_switch_is_on() {
        let nav = LinkChoice { id: "org.x.nav2".into(), name: "Other".into(), default: false };
        // From the Work Navigator: the Personal Navigator is a choice only
        // in Personal; School's switch is off.
        let mut s = Sheet::with_personas("https://a.example/", "https", vec![nav],
            vec![p("work", true, true, &[]), p("personal", false, true, &["org.atrium.navigator"]), p("school", false, false, &["org.atrium.navigator"])]);
        assert_eq!(s.rows(), vec![Row::App(0), Row::App(1), Row::Persona, Row::Copy, Row::Always, Row::Cancel]);
        assert!(s.label(&Row::Persona).starts_with("In: Work"));
        assert_eq!(s.press(1, [1; 16]), Do::Redraw, "the asker itself, in its own persona: no");
        assert!(s.notice.as_deref().unwrap().contains("another persona"));
        assert_eq!(s.press(2, [0; 16]), Do::Redraw);
        assert_eq!(s.label(&Row::Persona), "In: Personal");
        assert_eq!(s.press(1, [1; 16]), Do::Spend { row: 1, token: [1; 16] });
        assert_eq!(s.spent(1, true), Do::Done(Outcome::Chose { app: "org.atrium.navigator".into(), always: false, persona: Some("personal".into()) }));
        s.press(2, [0; 16]);
        assert!(s.label(&Row::Persona).contains("off in Settings"));
        assert_eq!(s.press(0, [1; 16]), Do::Redraw, "School's switch is off");
        assert!(s.notice.as_deref().unwrap().contains("Links from Work to School are off"));
        // Copy address instead: in Work, whatever persona is chosen.
        assert_eq!(s.label(&Row::Copy), "Copy address (in Work)");
        assert_eq!(s.press(3, [4; 16]), Do::Done(Outcome::Copy { token: [4; 16] }));
        // No named persona: no persona row.
        assert!(!Sheet::with_personas("x", "https", vec![], vec![p("personal", true, true, &[])]).rows().contains(&Row::Persona));
    }
}
