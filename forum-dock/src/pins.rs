//! The apps kept in the dock (docs/spec/forum.md §6.1): a list of app ids,
//! one per line, at `$FORUM_DOCK_PINS` or `$HOME/.config/forum-dock/pinned`.

use std::path::{Path, PathBuf};

use crate::AppEntry;

/// Pinned when there is no pin file yet: the first this many catalog apps.
pub const DEFAULT_PINS: usize = 5;

/// Where the pins live, if anywhere.
pub fn path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FORUM_DOCK_PINS") {
        return Some(PathBuf::from(p));
    }
    // A session component's own state folder (portcullisd mounts it).
    if let Some(d) = std::env::var_os("ATRIUM_STATE_DIR") {
        return Some(PathBuf::from(d).join("pinned"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/forum-dock/pinned"))
}

/// The pinned ids, in dock order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    pub ids: Vec<String>,
    /// Read from a file (so an empty list is the person's choice).
    pub from_file: bool,
}

impl Pins {
    /// Parse a pin file: ids one per line, `#` comments and blanks skipped,
    /// duplicates dropped.
    pub fn parse(text: &str) -> Pins {
        let mut ids: Vec<String> = Vec::new();
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if !ids.iter().any(|i| i == l) {
                ids.push(l.to_string());
            }
        }
        Pins { ids, from_file: true }
    }

    /// Load from `p`; a missing or unreadable file is "no pins chosen yet".
    pub fn load(p: Option<&Path>) -> Pins {
        p.and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Pins::parse(&t)).unwrap_or_default()
    }

    /// What the dock shows: the chosen pins that are still installed, or,
    /// with no pin file, the first [`DEFAULT_PINS`] apps.
    pub fn resolve<'a>(&self, apps: &'a [AppEntry]) -> Vec<&'a AppEntry> {
        if self.from_file {
            self.ids.iter().filter_map(|id| apps.iter().find(|a| &a.id == id)).collect()
        } else {
            apps.iter().take(DEFAULT_PINS).collect()
        }
    }

    pub fn contains(&self, apps: &[AppEntry], id: &str) -> bool {
        self.resolve(apps).iter().any(|a| a.id == id)
    }

    /// Pin `id` at the end, or unpin it. The first change turns the default
    /// set into an explicit list, so what the person sees is what is kept.
    pub fn toggle(&mut self, apps: &[AppEntry], id: &str) {
        if !self.from_file {
            self.ids = self.resolve(apps).iter().map(|a| a.id.clone()).collect();
            self.from_file = true;
        }
        if let Some(i) = self.ids.iter().position(|p| p == id) {
            self.ids.remove(i);
        } else {
            self.ids.push(id.to_string());
        }
    }

    /// Write to `p` (creating its directory). An error means the pins hold
    /// only until logout (a session jail with no writable home).
    pub fn save(&self, p: &Path) -> std::io::Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut text = String::from("# Apps kept in the Forum dock, in order (forum.md §6.1).\n");
        for id in &self.ids {
            text.push_str(id);
            text.push('\n');
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> AppEntry {
        AppEntry { id: id.into(), name: id.into(), description: None, icon: None, uid: None, vector: None, console: false }
    }

    #[test]
    fn no_file_pins_the_first_apps_and_a_toggle_makes_it_explicit() {
        let apps: Vec<AppEntry> = (0..8).map(|i| app(&format!("a{i}"))).collect();
        let mut p = Pins::default();
        assert_eq!(p.resolve(&apps).len(), DEFAULT_PINS);
        p.toggle(&apps, "a1");
        assert!(p.from_file);
        assert_eq!(p.ids, ["a0", "a2", "a3", "a4"]);
        p.toggle(&apps, "a7");
        assert_eq!(p.ids.last().map(String::as_str), Some("a7"));
    }

    #[test]
    fn a_file_keeps_order_skips_comments_and_uninstalled_apps() {
        let apps = vec![app("x"), app("y")];
        let p = Pins::parse("# c\ny\n\nx\ny\ngone\n");
        assert_eq!(p.ids, ["y", "x", "gone"]);
        let shown: Vec<&str> = p.resolve(&apps).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(shown, ["y", "x"]);
        // An empty file is the person's choice: nothing pinned.
        assert!(Pins::parse("").resolve(&apps).is_empty());
    }

    #[test]
    fn saved_pins_load_back() {
        let dir = std::env::temp_dir().join(format!("forum-dock-pins-{}", std::process::id()));
        let f = dir.join("sub/pinned");
        let p = Pins { ids: vec!["b".into(), "a".into()], from_file: true };
        p.save(&f).unwrap();
        assert_eq!(Pins::load(Some(&f)), p);
        let _ = std::fs::remove_dir_all(dir);
    }
}
