//! The person's share sheet (share-sheet.md §4.4): apps it never lists and
//! the order of the rest, in `/var/db/atrium/<user>/share.toml` next to
//! `links.toml`. Only Settings changes it, on a press, through portcullisd;
//! the share sheet reads it. Installing an app changes nothing here: a new
//! share-target is listed, after the ones the person ordered.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Ids kept, in each list: far above what a person installs.
pub const MAX_IDS: usize = 256;
/// The file is never read past this.
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePrefs {
    /// The order the person chose, first first.
    #[serde(default)]
    pub order: Vec<String>,
    /// Apps the sheet doesn't list.
    #[serde(default)]
    pub hidden: BTreeSet<String>,
}

impl SharePrefs {
    pub fn user_path(user: &str) -> PathBuf {
        PathBuf::from("/var/db/atrium").join(user).join("share.toml")
    }

    /// Missing file: nothing chosen.
    pub fn load(path: &Path) -> io::Result<Self> {
        use std::io::Read;
        let f = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut s = String::new();
        f.take(MAX_BYTES).read_to_string(&mut s)?;
        toml::from_str(&s).map_err(io::Error::other)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(self).map_err(io::Error::other)?)?;
        fs::rename(&tmp, path)
    }

    /// What Settings asked for, keeping only apps in `known` (the
    /// share-targets installed for the person), each once.
    pub fn chosen(order: &[String], hidden: &[String], known: &[String]) -> Self {
        let mut o: Vec<String> = Vec::new();
        for id in order.iter().filter(|id| known.contains(id)) {
            if !o.contains(id) && o.len() < MAX_IDS {
                o.push(id.clone());
            }
        }
        let h = hidden.iter().filter(|id| known.contains(id)).take(MAX_IDS).cloned().collect();
        SharePrefs { order: o, hidden: h }
    }

    /// `items` as the person ordered them: the ones they ordered first, in
    /// their order, then the rest as given. Hidden ones are kept (Settings
    /// shows them); [`SharePrefs::shown`] drops them.
    pub fn arrange<T>(&self, items: Vec<T>, id: impl Fn(&T) -> &str) -> Vec<T> {
        let rank = |t: &T| self.order.iter().position(|o| o == id(t)).unwrap_or(usize::MAX);
        let mut v: Vec<(usize, usize, T)> = items.into_iter().enumerate().map(|(i, t)| (rank(&t), i, t)).collect();
        v.sort_by_key(|(r, i, _)| (*r, *i));
        v.into_iter().map(|(_, _, t)| t).collect()
    }

    /// What the share sheet lists: arranged, without the hidden ones.
    pub fn shown<T>(&self, items: Vec<T>, id: impl Fn(&T) -> &str) -> Vec<T> {
        let v = self.arrange(items, &id);
        v.into_iter().filter(|t| !self.hidden.contains(id(t))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

    #[test]
    fn order_hide_and_only_known_apps() {
        let known = s(&["a", "b", "c", "d"]);
        let p = SharePrefs::chosen(&s(&["c", "x", "a", "c"]), &s(&["b", "y"]), &known);
        assert_eq!(p.order, s(&["c", "a"]), "unknown and repeated ids go");
        assert_eq!(p.hidden.iter().cloned().collect::<Vec<_>>(), s(&["b"]));
        // Ordered ones first, the rest (a new app: e) as given.
        assert_eq!(p.arrange(s(&["a", "b", "c", "d", "e"]), |x| x), s(&["c", "a", "b", "d", "e"]));
        assert_eq!(p.shown(s(&["a", "b", "c", "d", "e"]), |x| x), s(&["c", "a", "d", "e"]));
    }

    #[test]
    fn saved_and_read_back() {
        let d = std::env::temp_dir().join(format!("share-prefs-{}", std::process::id()));
        let path = d.join("alice/share.toml");
        assert_eq!(SharePrefs::load(&path).unwrap(), SharePrefs::default());
        let p = SharePrefs::chosen(&s(&["b", "a"]), &s(&["a"]), &s(&["a", "b"]));
        p.save(&path).unwrap();
        assert_eq!(SharePrefs::load(&path).unwrap(), p);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
