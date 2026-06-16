//! forum-dock — the app catalog (`docs/spec/forum.md` §6).
//!
//! The dock lists the installed apps and, on the human's activate, asks the TCB to
//! launch one (the launch *client* lives in the binary; this is the catalog logic).
//! The dock holds no special capability — listing reads manifests; launching is a
//! request to portcullisd, authorized by the user's grants. A bug here can't launch
//! anything the user didn't approve, and can't touch another app's windows (it has
//! no `window-management` / `forum-control`).

use std::path::Path;

/// The conventional install root: one subdirectory per app, each with its
/// `atrium.toml` manifest (mirrors portcullisd's launch tree).
pub const APPS_DIR: &str = "/var/lib/atrium/apps";

/// One launchable app, as shown in the dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Icon name/path the app declared in its manifest (`[app] icon = ...`).
    pub icon: Option<String>,
}

/// Parse one manifest's text into a catalog entry. `None` if it doesn't parse —
/// an unreadable/invalid manifest is simply not offered (fail safe, not loud).
pub fn entry_from_manifest(text: &str) -> Option<AppEntry> {
    let m = portcullis_toml::Manifest::from_str(text).ok()?;
    Some(AppEntry {
        id: m.app.id,
        name: m.app.name,
        description: m.app.description,
        icon: m.app.icon,
    })
}

/// Scan an apps directory → the installed-app catalog, sorted by id for a stable
/// dock order. Missing dir → empty (no apps installed is not an error).
pub fn catalog(apps_dir: &Path) -> Vec<AppEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(apps_dir) else { return out };
    for e in entries.flatten() {
        let manifest = e.path().join("atrium.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            if let Some(entry) = entry_from_manifest(&text) {
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[app]
id = "org.atrium.edit"
name = "Atrium Edit"
version = "1.0.0"
entry = "/usr/local/bin/atrium-edit"
description = "A text editor"

[capabilities]
graphics = "fresco"
"#;

    #[test]
    fn parses_a_manifest_into_a_catalog_entry() {
        let e = entry_from_manifest(SAMPLE).expect("parse");
        assert_eq!(e.id, "org.atrium.edit");
        assert_eq!(e.name, "Atrium Edit");
        assert_eq!(e.description.as_deref(), Some("A text editor"));
    }

    #[test]
    fn invalid_manifest_is_skipped_not_fatal() {
        assert!(entry_from_manifest("this is not toml = = =").is_none());
        assert!(entry_from_manifest("").is_none());
    }

    #[test]
    fn missing_apps_dir_yields_empty_catalog() {
        assert!(catalog(Path::new("/no/such/atrium/apps")).is_empty());
    }
}
