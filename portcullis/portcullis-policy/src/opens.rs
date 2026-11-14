//! Default apps per file type (opening-files.md §5.2): the user's own
//! choices over the system's, in `/var/db/atrium/<user>/opens.toml` next
//! to their policy. Installing an app changes nothing here; only
//! Scrinium's Open with sheet and Curia change it, each with a press
//! token (D7), through portcullisd.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use atrium_types::Family;
use serde::{Deserialize, Serialize};

/// The system's first choices, by type, then by family.
pub const PLAYER: &str = "org.atrium.player";
pub const WRITE: &str = "org.atrium.write";
pub const SHEETS: &str = "org.atrium.sheets";
pub const SLIDES: &str = "org.atrium.slides";
/// The Navigator shows documents through its document worker (limen
/// `doc-viewer`); picture viewing goes there too until there is an image
/// viewer (D5).
pub const NAVIGATOR: &str = "org.atrium.navigator";

/// The system's default for a type, before the user chooses.
pub fn system_default(mime: &str) -> Option<&'static str> {
    let t = atrium_types::by_mime(mime)?;
    match (t.mime, t.family) {
        ("application/pdf" | "image/svg+xml", _) => Some(NAVIGATOR),
        ("text/plain" | "text/markdown" | "application/json" | "application/xml", _) => Some(WRITE),
        (_, Family::Audio | Family::Video) => Some(PLAYER),
        (_, Family::Document | Family::Text) => Some(WRITE),
        (_, Family::Spreadsheet) => Some(SHEETS),
        (_, Family::Presentation) => Some(SLIDES),
        (_, Family::WebPage | Family::Image) => Some(NAVIGATOR),
        (_, Family::Archive | Family::Program | Family::Link | Family::Bundle) => None,
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    /// Media type -> app id: what the user chose.
    #[serde(default)]
    pub chosen: BTreeMap<String, String>,
}

impl Defaults {
    pub fn user_path(user: &str) -> PathBuf {
        PathBuf::from("/var/db/atrium").join(user).join("opens.toml")
    }

    /// Missing file: no choices yet.
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(self).map_err(io::Error::other)?)?;
        fs::rename(&tmp, path)
    }

    /// The app that opens `mime`: the user's choice, else the system's,
    /// as long as that app is installed and still declares the type
    /// (`declares`). `None`: ask, in the Open with sheet.
    pub fn app_for(&self, mime: &str, declares: impl Fn(&str) -> bool) -> Option<String> {
        self.chosen
            .get(mime)
            .map(String::as_str)
            .into_iter()
            .chain(system_default(mime))
            .find(|a| declares(a))
            .map(str::to_string)
    }

    /// Choose `app` for `mime`. Refused for a type the table doesn't know
    /// or never opens in an app; the caller checks the app declares it.
    pub fn choose(&mut self, mime: &str, app: &str) -> Result<(), String> {
        match atrium_types::by_mime(mime) {
            Some(t) if t.family.declarable() => {
                self.chosen.insert(mime.to_string(), app.to_string());
                Ok(())
            }
            Some(_) => Err(format!("{mime} files are never opened in an app")),
            None => Err(format!("{mime} is not a type Atrium knows")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_defaults() {
        assert_eq!(system_default("audio/flac"), Some(PLAYER));
        assert_eq!(system_default("video/mp4"), Some(PLAYER));
        assert_eq!(system_default("application/vnd.oasis.opendocument.text"), Some(WRITE));
        assert_eq!(system_default("application/vnd.oasis.opendocument.spreadsheet"), Some(SHEETS));
        assert_eq!(system_default("text/csv"), Some(SHEETS));
        assert_eq!(system_default("application/pdf"), Some(NAVIGATOR));
        assert_eq!(system_default("image/png"), Some(NAVIGATOR));
        assert_eq!(system_default("application/x-executable"), None);
        assert_eq!(system_default("application/zip"), None);
        assert_eq!(system_default("nope/nope"), None);
    }

    #[test]
    fn the_users_choice_then_the_systems_then_ask() {
        let mut d = Defaults::default();
        let installed = |a: &str| a == PLAYER || a == "org.example.music";
        assert_eq!(d.app_for("audio/flac", installed).as_deref(), Some(PLAYER));
        d.choose("audio/flac", "org.example.music").unwrap();
        assert_eq!(d.app_for("audio/flac", installed).as_deref(), Some("org.example.music"));
        // The chosen app gone (or no longer declaring it): the system's.
        assert_eq!(d.app_for("audio/flac", |a| a == PLAYER).as_deref(), Some(PLAYER));
        // Nothing installed that opens it: ask.
        assert_eq!(d.app_for("audio/flac", |_| false), None);
        assert!(d.choose("application/x-shellscript", "org.example.music").is_err());
        assert!(d.choose("audio/x-mine", "org.example.music").is_err());
    }

    #[test]
    fn saved_and_loaded() {
        let p = std::env::temp_dir().join(format!("opens-{}/opens.toml", std::process::id()));
        assert_eq!(Defaults::load(&p).unwrap(), Defaults::default());
        let mut d = Defaults::default();
        d.choose("audio/flac", "org.example.music").unwrap();
        d.save(&p).unwrap();
        assert_eq!(Defaults::load(&p).unwrap(), d);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }
}
