//! Default apps per link scheme (opening-links.md §4.4): the person's own
//! choices over the system's, in `/var/db/atrium/<user>/links.toml` next
//! to `opens.toml`. Installing an app changes nothing here; only
//! Monstrator's sheet and Curia change it, each on a press, through
//! portcullisd.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The system's first choices.
pub fn system_default(scheme: &str) -> Option<&'static str> {
    atrium_links::WEB.contains(&scheme).then_some(crate::opens::NAVIGATOR)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkDefaults {
    /// Scheme -> app id: what the person chose.
    #[serde(default)]
    pub chosen: BTreeMap<String, String>,
}

impl LinkDefaults {
    pub fn user_path(user: &str) -> PathBuf {
        PathBuf::from("/var/db/atrium").join(user).join("links.toml")
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

    /// The app that takes `scheme`: the person's choice, else the system's,
    /// as long as it is installed and still declares it (`declares`).
    /// `None`: ask.
    pub fn app_for(&self, scheme: &str, declares: impl Fn(&str) -> bool) -> Option<String> {
        self.chosen
            .get(scheme)
            .map(String::as_str)
            .into_iter()
            .chain(system_default(scheme))
            .find(|a| declares(a))
            .map(str::to_string)
    }

    /// Choose `app` for `scheme`; the caller checks the app declares it.
    pub fn choose(&mut self, scheme: &str, app: &str) -> Result<(), String> {
        atrium_links::declarable(scheme)?;
        self.chosen.insert(scheme.to_string(), app.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opens::NAVIGATOR;

    #[test]
    fn the_persons_choice_then_the_systems_then_ask() {
        let mut d = LinkDefaults::default();
        assert_eq!(d.app_for("https", |a| a == NAVIGATOR).as_deref(), Some(NAVIGATOR));
        assert_eq!(d.app_for("mailto", |_| true), None, "no system mail app: ask");
        d.choose("mailto", "org.example.mail").unwrap();
        assert_eq!(d.app_for("mailto", |a| a == "org.example.mail").as_deref(), Some("org.example.mail"));
        assert_eq!(d.app_for("mailto", |_| false), None, "removed: ask again");
        assert!(d.choose("javascript", "org.example.mail").is_err());
        assert!(d.choose("atrium-app", "org.example.mail").is_err());
        let p = std::env::temp_dir().join(format!("links-{}/links.toml", std::process::id()));
        d.save(&p).unwrap();
        assert_eq!(LinkDefaults::load(&p).unwrap(), d);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }
}
