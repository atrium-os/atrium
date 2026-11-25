//! Updates (opifex.md §11, build step C5): what to do with a newer catalog
//! release of an installed app.
//!
//! - another publisher: refused (shown, never applied);
//! - more capabilities: the person is asked, on the sheet, about the
//!   additions only;
//! - otherwise by timing class (insula §14.3): the publisher's `[app]
//!   updates` is the least care it allows (`auto`, `notify`, `required`), and
//!   a person may choose more care for an app (`auto` → `notify`), never
//!   less. `auto` and `required` are installed in the background (the app
//!   picks the update up at its next start); `notify` waits for the person.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Installed without asking.
    Background,
    /// Waits for the person to pick when (Curia's Updates, or the sheet).
    Notify,
    /// Asks for new capabilities, on the sheet.
    Ask,
    /// Never applied: another publisher, or a side-loaded app.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub name: String,
    pub from: String,
    pub to: String,
    pub action: Action,
    /// Capabilities the new release asks for that the installed one doesn't.
    pub added: Vec<String>,
    /// For `Refused`, why.
    #[serde(default)]
    pub why: String,
}

/// The capabilities a manifest asks for, by key: every app has a window, and
/// `false` and `network = "none"` grant nothing.
pub fn caps(manifest: &str) -> Vec<String> {
    let v: toml::Value = manifest.parse().unwrap_or(toml::Value::Table(Default::default()));
    let mut out = granted(&v);
    // An Artifex plugin that asks to read workspaces (dev-tools.md §3.8,
    // D28): the person still allows it per workspace in Artifex, but the
    // sheet names it and an update that starts asking is asked about.
    let ext = v.get("roles").and_then(|r| r.get("editor-extension"));
    if ext.and_then(|e| e.get("workspace-read")).and_then(|w| w.as_bool()) == Some(true) {
        out.push("workspace-read".into());
    }
    out
}

fn granted(v: &toml::Value) -> Vec<String> {
    v.get("capabilities").and_then(|c| c.as_table()).map(|t| {
        t.iter().filter(|(k, v)| k.as_str() != "graphics" && v.as_bool() != Some(false) && v.as_str() != Some("none"))
            .flat_map(|(k, v)| match (k.as_str(), v) {
                // A changed network mode or grant list is a change worth asking about.
                ("network", toml::Value::String(s)) => vec![format!("network={s}")],
                // One line per role it embeds, so an update that adds a role
                // asks about that role (portcullis.md, embeds).
                ("embeds", toml::Value::Array(a)) => a.iter().filter_map(|r| r.as_str())
                    .map(|r| format!("embeds:{r}")).collect(),
                _ => vec![k.clone()],
            })
            .collect()
    }).unwrap_or_default()
}

/// The app's display name, if the manifest gives one.
pub fn name(manifest: &str) -> Option<String> {
    let v: toml::Value = manifest.parse().ok()?;
    let sec = ["app", "font", "driver", "firmware"].iter().find_map(|k| v.get(*k))?;
    sec.get("name")?.as_str().map(str::to_string)
}

/// The publisher's timing class (`auto` when it names none or an unknown one).
pub fn class(manifest: &str) -> String {
    let v: toml::Value = manifest.parse().unwrap_or(toml::Value::Table(Default::default()));
    v.get("app").and_then(|a| a.get("updates")).and_then(|u| u.as_str())
        .filter(|u| portcullis_toml::schema::UPDATE_CLASSES.contains(u)).unwrap_or("auto").to_string()
}

/// What to do with `new` over `installed`, for a person who chose `pref`.
#[cfg(test)]
mod embeds_tests {
    #[test]
    fn a_plugin_that_reads_workspaces_says_so_and_an_update_that_starts_asking_asks() {
        let m = |read: &str| format!("[app]\nid = \"org.a.b\"\nname = \"B\"\nversion = \"1\"\nentry = \"x\"\n[capabilities]\nnetwork = \"none\"\n[roles]\nimplements = [\"editor-extension\"]\n[roles.editor-extension]\nlanguages = [\"c\"]\n{read}");
        assert_eq!(super::caps(&m("workspace-read = true\n")), ["workspace-read"]);
        assert!(super::caps(&m("workspace-read = false\n")).is_empty());
        assert!(super::caps(&m("")).is_empty());
        let (_, added) = super::decide(&m(""), &m("workspace-read = true\n"), true, None);
        assert_eq!(added, ["workspace-read"]);
    }

    #[test]
    fn each_embedded_role_is_its_own_line_and_an_added_one_is_asked() {
        let m = |e: &str| format!("[app]\nid = \"org.a.b\"\nname = \"B\"\nversion = \"1\"\nentry = \"x\"\n[capabilities]\nembeds = [{e}]\n");
        assert_eq!(super::caps(&m("\"picker\", \"doc-viewer\"")), ["embeds:picker", "embeds:doc-viewer"]);
        let (_, added) = super::decide(&m("\"picker\""), &m("\"picker\", \"doc-viewer\""), true, None);
        assert_eq!(added, ["embeds:doc-viewer"]);
        let (_, added) = super::decide(&m("\"picker\", \"doc-viewer\""), &m("\"picker\""), true, None);
        assert!(added.is_empty(), "dropping a role asks nothing");
    }
}

pub fn decide(installed: &str, new: &str, same_publisher: bool, pref: Option<&str>) -> (Action, Vec<String>) {
    let have = caps(installed);
    let added: Vec<String> = caps(new).into_iter().filter(|c| !have.contains(c)).collect();
    if !same_publisher {
        return (Action::Refused, added);
    }
    if !added.is_empty() {
        return (Action::Ask, added);
    }
    let a = match (class(new).as_str(), pref) {
        ("required", _) => Action::Background,
        ("notify", _) | (_, Some("notify")) => Action::Notify,
        _ => Action::Background,
    };
    (a, added)
}

/// A person's choices and what has been announced, `<state>/updates.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Prefs {
    /// id → `"notify"` (more care than the publisher's `auto`).
    #[serde(default)]
    pub care: BTreeMap<String, String>,
    /// `id version` pairs a notice has already named.
    #[serde(default)]
    pub announced: Vec<String>,
    /// When the last sync ran (Unix seconds), and its outcome.
    #[serde(default)]
    pub last_sync: u64,
    #[serde(default)]
    pub last_error: String,
}

pub struct PrefStore { pub path: PathBuf }

impl PrefStore {
    pub fn new(state: &Path) -> PrefStore { PrefStore { path: state.join("updates.toml") } }

    pub fn load(&self) -> Prefs {
        fs::read_to_string(&self.path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, p: &Prefs) -> Result<(), String> {
        if let Some(d) = self.path.parent() {
            fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, toml::to_string(p).map_err(|e| e.to_string())?).and_then(|_| fs::rename(&tmp, &self.path))
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

/// The notice for updates waiting on the person: a title and a body, plain.
pub fn notice(waiting: &[Pending]) -> (String, String) {
    let title = if waiting.len() == 1 { format!("{} has an update", waiting[0].name) } else { format!("{} updates are waiting", waiting.len()) };
    let lines: Vec<String> = waiting.iter().take(5).map(|p| match p.action {
        Action::Ask => format!("{} {}: asks to {}", p.name, p.to,
                               p.added.iter().map(|c| c.split('=').next().unwrap_or(c).replace('-', " ")).collect::<Vec<_>>().join(", ")),
        _ => format!("{} {}", p.name, p.to),
    }).collect();
    let more = if waiting.len() > 5 { format!("\nand {} more", waiting.len() - 5) } else { String::new() };
    (title, format!("{}{more}\nOpen Settings, Updates.", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(class: &str, caps: &str) -> String {
        format!("[app]\nid = \"org.a.b\"\nname = \"B\"\nversion = \"1\"\nentry = \"x\"\n{class}\n[capabilities]\ngraphics = \"fresco\"\n{caps}")
    }

    #[test]
    fn what_to_do() {
        let old = m("", "clipboard = true\nnetwork = \"none\"\n");
        assert_eq!(decide(&old, &m("", "clipboard = true\n"), true, None), (Action::Background, vec![]));
        assert_eq!(decide(&old, &m("", "clipboard = true\n"), true, Some("notify")), (Action::Notify, vec![]), "the person's care");
        assert_eq!(decide(&old, &m("updates = \"notify\"", "clipboard = true\n"), true, None), (Action::Notify, vec![]));
        assert_eq!(decide(&old, &m("updates = \"required\"", "clipboard = true\n"), true, Some("notify")), (Action::Background, vec![]),
                   "required can't be held back");
        assert_eq!(decide(&old, &m("", "clipboard = true\nnetwork = \"full\"\ncamera = true\n"), true, None),
                   (Action::Ask, vec!["camera".into(), "network=full".into()]));
        assert_eq!(decide(&old, &m("", "clipboard = true\n"), false, None).0, Action::Refused);
        assert_eq!(class(&m("updates = \"sometime\"", "")), "auto");
    }

    #[test]
    fn notices_are_plain_and_short() {
        let p = |n: &str, a, added: &[&str]| Pending { id: n.into(), name: n.into(), from: "1".into(), to: "2".into(), action: a,
                                                       added: added.iter().map(|s| s.to_string()).collect(), why: String::new() };
        let (t, b) = notice(&[p("Mail", Action::Ask, &["camera", "network=full"])]);
        assert_eq!(t, "Mail has an update");
        assert!(b.starts_with("Mail 2: asks to camera, network"), "{b}");
        let many: Vec<Pending> = (0..7).map(|i| p(&format!("A{i}"), Action::Notify, &[])).collect();
        let (t, b) = notice(&many);
        assert_eq!(t, "7 updates are waiting");
        assert!(b.contains("and 2 more"));
    }
}
